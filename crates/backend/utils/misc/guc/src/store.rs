use std::cell::{Cell, RefCell};

use guc_tables::{all_settings, GucDefaultValue, GucSetting};
use types_core::Oid;
use types_error::{ErrorLevel, PgResult, FATAL};
use types_guc::{
    config_type, GucContext, GucSource, PGC_POSTMASTER, PGC_S_DYNAMIC_DEFAULT, PGC_S_ENV_VAR,
    PGC_S_OVERRIDE,
};

use crate::model::{
    config_bool, config_enum, config_generic, config_int, config_real, config_string,
};
use crate::registry::{DeferredAssignHook, GucRegistry, GucVariable};

thread_local! {
    // guc_hashtab + the runtime records; one backend = one thread, so this
    // thread_local IS the C file-static store. RefCell: re-entrant reads free,
    // re-entrant writes fail fast (assign hooks are deferred past the borrow).
    static GUC_STORE: RefCell<Option<GucRegistry>> = const { RefCell::new(None) };
    // TimestampTz PgReloadTime (guc.c).
    static PG_RELOAD_TIME: Cell<i64> = const { Cell::new(0) };
    // Fast flags for the per-statement no-op paths (C reads a bare list head;
    // reaching ours costs a RefCell borrow + Option). Contract: a flag is
    // NEVER false while the TLS store's list is non-empty; stale TRUE is fine
    // (the slow path re-checks the real list).
    static HAS_STACKED_HINT: Cell<bool> = const { Cell::new(false) };
    static REPORT_PENDING_HINT: Cell<bool> = const { Cell::new(false) };
    // §3.4 binding guard: true while this thread's store holds a leader's
    // session bind. A pool thread must never claim a task with a live bind
    // (cross-session GUC leak, the session-clobber P1 class).
    static SESSION_BOUND: Cell<bool> = const { Cell::new(false) };
    // Monotonic count of mutable store borrows: the layered-snapshot cache
    // key (guc::layers). Conservative — any with_store_mut invalidates —
    // and off the per-query hot path (a clean statement takes no mutable
    // borrow: AtEOXact_GUC/report fast paths gate on the hint Cells above).
    static STORE_MUTATIONS: Cell<u64> = const { Cell::new(0) };
    // Count of registries installed on this thread (initialize_guc_options):
    // a variable's index is stable for its registry's life (a removed
    // placeholder stays behind as a tombstone, a definition replaces its
    // placeholder in place), so an index remembered under one generation is
    // good until the next install (`get_config_option_at`).
    static STORE_GEN: Cell<u64> = const { Cell::new(0) };
}

/// This thread's store mutation counter (see STORE_MUTATIONS).
pub fn store_mutation_count() -> u64 {
    STORE_MUTATIONS.get()
}

/// This thread's registry generation (see STORE_GEN).
pub(crate) fn store_generation() -> u64 {
    STORE_GEN.get()
}

#[inline]
pub(crate) fn has_stacked_hint() -> bool {
    HAS_STACKED_HINT.get()
}

#[inline]
pub(crate) fn set_has_stacked_hint(v: bool) {
    HAS_STACKED_HINT.set(v);
}

#[inline]
pub(crate) fn report_pending_hint() -> bool {
    REPORT_PENDING_HINT.get()
}

#[inline]
pub(crate) fn set_report_pending_hint(v: bool) {
    REPORT_PENDING_HINT.set(v);
}

/// The postmaster's PgReloadTime, which a fresh child inherits (fork in C;
/// launch_backend.c:1034 restore_backend_variables under EXEC_BACKEND).
static POSTMASTER_RELOAD_TIME: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

pub fn set_pg_reload_time(t: i64) {
    PG_RELOAD_TIME.set(t);
    if init_small::globals::MyProcPid() == init_small::globals::PostmasterPid() {
        POSTMASTER_RELOAD_TIME.store(t, std::sync::atomic::Ordering::Relaxed);
    }
}

pub(crate) fn inherit_postmaster_reload_time() {
    PG_RELOAD_TIME.set(POSTMASTER_RELOAD_TIME.load(std::sync::atomic::Ordering::Relaxed));
}

pub fn pg_reload_time() -> i64 {
    PG_RELOAD_TIME.get()
}

// Also help_config.rs (build_guc_variables' table without hooks).
pub(crate) fn build_variable(setting: GucSetting) -> Option<GucVariable> {
    let name = setting.name();
    let gen = |vartype: config_type| {
        config_generic::boot(
            name,
            setting.context(),
            setting.group(),
            setting.short_desc(),
            setting.long_desc(),
            setting.flags(),
            vartype,
        )
    };
    Some(match (setting, setting.default_value()) {
        (GucSetting::Bool(s), GucDefaultValue::Bool(b)) => GucVariable::Bool(config_bool {
            gen: gen(types_guc::PGC_BOOL),
            variable: s.variable,
            value: Some(b),
            boot_val: b,
            check_hook: s.check_hook,
            assign_hook: s.assign_hook,
            show_hook: s.show_hook,
            reset_val: b,
            reset_extra: None,
        }),
        (GucSetting::Int(s), GucDefaultValue::Int(i)) => GucVariable::Int(config_int {
            gen: gen(types_guc::PGC_INT),
            variable: s.variable,
            value: Some(i),
            boot_val: i,
            min: s.min,
            max: s.max,
            check_hook: s.check_hook,
            assign_hook: s.assign_hook,
            show_hook: s.show_hook,
            reset_val: i,
            reset_extra: None,
        }),
        (GucSetting::Real(s), GucDefaultValue::Real(r)) => GucVariable::Real(config_real {
            gen: gen(types_guc::PGC_REAL),
            variable: s.variable,
            value: Some(r),
            boot_val: r,
            min: s.min,
            max: s.max,
            check_hook: s.check_hook,
            assign_hook: s.assign_hook,
            show_hook: s.show_hook,
            reset_val: r,
            reset_extra: None,
        }),
        (GucSetting::String(s), GucDefaultValue::String(v)) => {
            let v: Option<String> = v.map(|s| s.to_string());
            GucVariable::String(config_string {
                gen: gen(types_guc::PGC_STRING),
                variable: s.variable,
                value: Some(v.clone()),
                boot_val: v.clone(),
                check_hook: s.check_hook,
                assign_hook: s.assign_hook,
                show_hook: s.show_hook,
                reset_val: v,
                reset_extra: None,
            })
        }
        (GucSetting::Enum(s), GucDefaultValue::Enum(e)) => GucVariable::Enum(config_enum {
            gen: gen(types_guc::PGC_ENUM),
            variable: s.variable,
            value: Some(e),
            boot_val: e,
            options: s.options,
            check_hook: s.check_hook,
            assign_hook: s.assign_hook,
            show_hook: s.show_hook,
            reset_val: e,
            reset_extra: None,
        }),
        _ => return None,
    })
}

// InitializeGUCOptions (guc.c:1530).
pub fn initialize_guc_options() -> PgResult<()> {
    initialize_guc_options_impl(|_| true)
}

// Child-thread bring-up (SubPostmasterMain shape). C's re-init writes only the
// child's own address space; our backing vars are process-shared, so boot
// values must not be published for variables the snapshot restore is about to
// overwrite — other threads (the postmaster's maybe_adjust_io_workers) read
// them concurrently.
pub fn initialize_guc_options_for_child(snapshot: &[NondefaultGuc]) -> PgResult<()> {
    initialize_guc_options_impl(|name| !snapshot.iter().any(|v| v.name == name))?;
    inherit_postmaster_reload_time();
    crate::redefine_inherited_custom_variables()
}

// Same suppression contract, keyed on a shared base snapshot (guc::layers):
// boot values must not be published over process-shared backings for
// variables the subsequent bind_base is about to overwrite.
pub fn initialize_guc_options_for_child_base(
    base: &crate::layers::GucBaseSnapshot,
) -> PgResult<()> {
    initialize_guc_options_impl(|name| !base.contains(name))?;
    inherit_postmaster_reload_time();
    crate::redefine_inherited_custom_variables()
}

// D4682 step 2 (a), the GUC registry template: while on (launch_backend sets it when backend_thread_reuse_keep is on), a
// registry shares the process's index of the built-in variables' names, published by the first registry built while on,
// and builds only the values (every variable, its hooks, the boot and base values: nothing of a previous session).
static SHARE_BUILTIN_INDEX: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static BUILTIN_INDEX: pgsync::OnceLock<std::sync::Arc<crate::registry::BuiltinIndex>> = pgsync::OnceLock::new();
// The one publish: a registry that finds the index unpublished takes this and looks again, so two first sessions racing
// build it once (a lock and a second look rather than get_or_init: the determinism lint ratchets raw once sites).
static PUBLISH_BUILTIN_INDEX: pgsync::Mutex<()> = pgsync::Mutex::new(());

pub fn set_share_builtin_index(on: bool) {
    SHARE_BUILTIN_INDEX.store(on, std::sync::atomic::Ordering::Relaxed);
}

fn initialize_guc_options_impl(publish: impl Fn(&str) -> bool) -> PgResult<()> {
    // Before log_line_prefix-style GUCs can demand elog timestamps.
    pgtz::pg_timezone_initialize()?;

    let share = SHARE_BUILTIN_INDEX.load(std::sync::atomic::Ordering::Relaxed);
    let mut reg = GucRegistry::new();
    if let Some(builtins) = BUILTIN_INDEX.get().filter(|_| share) {
        reg.adopt_builtin_index(std::sync::Arc::clone(builtins));
    }
    for setting in all_settings() {
        let Some(mut var) = build_variable(setting) else {
            continue;
        };
        // A check-hook failure on a boot value is C's elog(FATAL, "failed to
        // initialize %s to ...").
        crate::registry::initialize_one_guc_option_hooks(&mut var, publish(setting.name()))
            .map_err(|e| {
                Box::new(
                    elog::ereport(FATAL)
                        .errmsg(format!(
                            "failed to initialize {} to {:?}: {}",
                            var.name(),
                            setting.default_value(),
                            e.message()
                        ))
                        .into_error(),
                )
            })?;
        reg.define_builtin(var)?;
    }
    reg.settle_builtin_index()?;
    if share && BUILTIN_INDEX.get().is_none() {
        // Process lifetime, on UNBOUND by construction (9d, 10-16): built once, under the publish lock, never freed,
        // never shed, so none of it is the first session's (its account would end with it as residue).
        let _publish = PUBLISH_BUILTIN_INDEX.lock().unwrap_or_else(|e| e.into_inner());
        if BUILTIN_INDEX.get().is_none() {
            let _home = memledger::Home::of(memledger::Acct::UNBOUND).enter();
            let _ = BUILTIN_INDEX.set(std::sync::Arc::new(reg.own_index()));
        }
    }
    STORE_MUTATIONS.set(STORE_MUTATIONS.get().wrapping_add(1));
    STORE_GEN.set(STORE_GEN.get().wrapping_add(1));
    GUC_STORE.with(|c| *c.borrow_mut() = Some(reg));

    crate::report::set_reporting_enabled(false);

    // Prevent any attempt to override the transaction modes from
    // non-interactive sources.
    crate::SetConfigOption(
        "transaction_isolation",
        Some("read committed"),
        PGC_POSTMASTER,
        PGC_S_OVERRIDE,
    )?;
    crate::SetConfigOption(
        "transaction_read_only",
        Some("no"),
        PGC_POSTMASTER,
        PGC_S_OVERRIDE,
    )?;
    crate::SetConfigOption(
        "transaction_deferrable",
        Some("no"),
        PGC_POSTMASTER,
        PGC_S_OVERRIDE,
    )?;

    initialize_guc_options_from_environment()?;
    Ok(())
}

// InitializeGUCOptionsFromEnvironment (guc.c:1589). The stack-rlimit branch
// needs get_stack_depth_rlimit (ported in stack_depth, which depends on guc);
// deferred for layering.
pub fn initialize_guc_options_from_environment() -> PgResult<()> {
    if let Ok(env) = std::env::var("PGPORT") {
        crate::SetConfigOption("port", Some(&env), PGC_POSTMASTER, PGC_S_ENV_VAR)?;
    }
    if let Ok(env) = std::env::var("PGDATESTYLE") {
        crate::SetConfigOption("datestyle", Some(&env), PGC_POSTMASTER, PGC_S_ENV_VAR)?;
    }
    if let Ok(env) = std::env::var("PGCLIENTENCODING") {
        crate::SetConfigOption("client_encoding", Some(&env), PGC_POSTMASTER, PGC_S_ENV_VAR)?;
    }
    // pgrust-only, P7-2 D-8 tombstone: PGRUST_LANE_V2 no longer seeds
    // pgrust.lane_executor (itself a tombstoned no-effect GUC — the lane-v2
    // executor is removed). The env var is accepted and ignored for one
    // release per the tombstone law (docs/design/sqe/p72-deletion-plan.md
    // §1.3), then this arm goes too.
    if std::env::var("PGRUST_LANE_V2").is_ok() {
        eprintln!(
            "pgrust: PGRUST_LANE_V2 is a lanev2 tombstone and has no effect \
             (the lane-v2 executor was removed, P7-2); accepted and ignored"
        );
    }
    // pgrust-only: pgrust.condition_cache's startup default, same contract
    // as PGRUST_LANE_V2 above (the harness arm switch; the boot default
    // stays full without the env var); a mode name seeds that mode.
    if let Ok(env) = std::env::var("PGRUST_CONDITION_CACHE") {
        let v = match env.as_str() {
            "0" | "off" => "off",
            "granule" | "rows" | "full" => env.as_str(),
            _ => "full",
        };
        crate::SetConfigOption("pgrust.condition_cache", Some(v), PGC_POSTMASTER, PGC_S_ENV_VAR)?;
    }
    // pgrust-only (env-to-guc train): pgrust.runtime's startup default (the M0
    // runtime-pool master switch). Only the exact value "0" kills, matching the
    // legacy runtime::runtime_enabled contract (any other value / unset = on),
    // so this seed is byte-neutral vs the prior env-only behavior. The pool
    // spawn (launch_backend::rtpool) and the M5 planner probe both key off the
    // resulting GUC value.
    if let Ok(env) = std::env::var("PGRUST_RUNTIME") {
        let v = if env == "0" { "off" } else { "on" };
        crate::SetConfigOption("pgrust.runtime", Some(v), PGC_POSTMASTER, PGC_S_ENV_VAR)?;
    }
    // pgrust-only (GL-M41-3 flip): pgrust.runtime_vacuum_pool's startup
    // default (parallel VACUUM on the morsel-pool workers). Flipped-kill
    // spelling per t35 convention: "0" or "off" disables; any other value /
    // unset = on (the flipped default).
    if let Ok(env) = std::env::var("PGRUST_RUNTIME_VACUUM_POOL") {
        let v = if matches!(env.trim(), "0" | "off") { "off" } else { "on" };
        crate::SetConfigOption("pgrust.runtime_vacuum_pool", Some(v), PGC_POSTMASTER, PGC_S_ENV_VAR)?;
    }
    // pgrust-only (env-to-guc train): pgrust.mem_autotune's startup default
    // (the boot-time machine-scaled memory/parallel auto-tune gate). Accepts
    // the same truthy set the autotune reader recognized (1/on/true/yes); the
    // default stays OFF without the env var.
    // pgrust-only: pgrust.gravity_dump_dir's startup default (the Gravity IR / machine-code dump directory).
    if let Ok(env) = std::env::var("PGRUST_GRAVITY_DUMP_IR") {
        crate::SetConfigOption("pgrust.gravity_dump_dir", Some(env.as_str()), PGC_POSTMASTER, PGC_S_ENV_VAR)?;
    }
    // pgrust-only: pgrust.gravity_jit_perfmap's startup default (off | func | tags: the JIT perf map, gravity_backend perfmap).
    if let Ok(env) = std::env::var("PGRUST_GRAVITY_JIT_PERFMAP") {
        let v = match env.trim() {
            "1" | "on" | "true" | "yes" => "func",
            "" | "0" => "off",
            v => v,
        };
        crate::SetConfigOption("pgrust.gravity_jit_perfmap", Some(v), PGC_POSTMASTER, PGC_S_ENV_VAR)?;
    }
    if let Ok(env) = std::env::var("PGRUST_MEM_AUTOTUNE") {
        let v = if matches!(env.as_str(), "1" | "on" | "true" | "yes") { "on" } else { "off" };
        crate::SetConfigOption("pgrust.mem_autotune", Some(v), PGC_POSTMASTER, PGC_S_ENV_VAR)?;
    }
    adjust_max_stack_depth_from_rlimit()
}

// guc.c:1618-1628: 2MB cap is DYNAMIC_DEFAULT; below that, ENV_VAR.
pub fn boot_limit_and_source(new_limit: isize) -> (i32, GucSource) {
    if new_limit < 2048 {
        (new_limit as i32, PGC_S_ENV_VAR)
    } else {
        (2048, PGC_S_DYNAMIC_DEFAULT)
    }
}

// guc.c:1613: part of InitializeGUCOptionsFromEnvironment, so a reload that
// drops the file setting lands back on the rlimit default, not 100kB.
pub fn adjust_max_stack_depth_from_rlimit() -> PgResult<()> {
    // D3103: on wasm32 the host-declared engine stack stands in for the rlimit; natively it IS get_stack_depth_rlimit.
    let stack_rlimit = stack_depth_core::stack_depth_default_source();
    if stack_rlimit > 0 {
        let new_limit = stack_rlimit.saturating_sub(stack_depth_core::STACK_DEPTH_SLOP) / 1024;
        if new_limit > 100 {
            let (new_limit, source) = boot_limit_and_source(new_limit);
            crate::SetConfigOption(
                "max_stack_depth",
                Some(&new_limit.to_string()),
                PGC_POSTMASTER,
                source,
            )?;
        }
    }
    Ok(())
}

pub fn is_initialized() -> bool {
    GUC_STORE.with(|c| c.borrow().is_some())
}

pub struct NondefaultGuc {
    pub name: String,
    pub value: Option<String>,
    pub scontext: GucContext,
    pub source: GucSource,
    pub srole: Oid,
}

// write_nondefault_variables (guc.c): the EXEC_BACKEND parameter file,
// rendered as an in-memory snapshot taken on the spawning (postmaster)
// thread; backend threads have no fork to inherit the TLS store through.
pub fn capture_nondefault_variables() -> Vec<NondefaultGuc> {
    with_store(|reg| {
        reg.iter()
            .filter(|v| v.gen().source != types_guc::GucSource::PGC_S_DEFAULT)
            .map(|v| NondefaultGuc {
                name: v.name().to_string(),
                value: Some(crate::registry::show_guc_option(v, false)),
                scontext: v.gen().scontext,
                source: v.gen().source,
                srole: v.gen().srole,
            })
            .collect()
    })
    .unwrap_or_default()
}

// read_nondefault_variables (guc.c): InitializeGUCOptions must already have
// run on this thread (SubPostmasterMain order).
pub fn restore_nondefault_variables(vars: &[NondefaultGuc]) -> PgResult<()> {
    // Per-variable timing, PGRUST_GATHER_TRACE-gated (§2 fixed-cost hunt).
    let trace = std::env::var_os("PGRUST_GATHER_TRACE").is_some();
    let t0 = trace.then(std::time::Instant::now);
    for v in vars {
        let tv = trace.then(std::time::Instant::now);
        crate::set_config_option_ext(
            &v.name,
            v.value.as_deref(),
            v.scontext,
            v.source,
            v.srole,
            crate::GUC_ACTION_SET,
            true,
            ErrorLevel(0),
            true,
        )?;
        if let Some(tv) = tv {
            let us = tv.elapsed().as_micros();
            if us >= 100 {
                eprintln!("GTRACE guc.var name={} dt_us={us}", v.name);
            }
        }
    }
    if let Some(t0) = t0 {
        eprintln!("GTRACE guc.restore n={} total_us={}", vars.len(), t0.elapsed().as_micros());
    }
    Ok(())
}

// write_nondefault_variables / read_nondefault_variables (guc.c) as the
// crash supervisor's carrier: C's postmaster keeps its settings across a
// crash restart, so the next server generation restores this image instead
// of rereading the configuration files. Layout per variable: name\0
// value\0 sourcefile\0 then sourceline, source, scontext, srole (i32 LE)
// and a GUC_PENDING_RESTART byte; the image starts with PgReloadTime.
pub fn write_postmaster_gucs(out: &mut Vec<u8>) {
    out.extend_from_slice(&PG_RELOAD_TIME.get().to_le_bytes());
    with_store(|reg| {
        for v in reg.iter() {
            let g = v.gen();
            let pending = g.status & crate::model::GUC_PENDING_RESTART != 0;
            if g.source == types_guc::GucSource::PGC_S_DEFAULT && !pending {
                continue;
            }
            out.extend_from_slice(v.name().as_bytes());
            out.push(0);
            out.extend_from_slice(crate::registry::show_guc_option(v, false).as_bytes());
            out.push(0);
            out.extend_from_slice(g.sourcefile.as_deref().unwrap_or("").as_bytes());
            out.push(0);
            for n in [g.sourceline, g.source as i32, g.scontext as i32, g.srole as i32] {
                out.extend_from_slice(&n.to_le_bytes());
            }
            out.push(pending as u8);
        }
    });
}

/// Ok(false) when `image` is malformed (nothing applied past the bad entry).
pub fn read_postmaster_gucs(image: &[u8]) -> PgResult<bool> {
    let Some((reload, mut rest)) = image.split_first_chunk::<8>() else { return Ok(false) };
    let cstr = |b: &mut &[u8]| -> Option<String> {
        let at = b.iter().position(|&c| c == 0)?;
        let s = std::str::from_utf8(&b[..at]).ok()?.to_owned();
        *b = &b[at + 1..];
        Some(s)
    };
    let int = |b: &mut &[u8]| -> Option<i32> {
        let (n, tail) = b.split_first_chunk::<4>()?;
        *b = tail;
        Some(i32::from_le_bytes(*n))
    };
    while !rest.is_empty() {
        let parsed = (|| {
            let name = cstr(&mut rest)?;
            let value = cstr(&mut rest)?;
            let file = cstr(&mut rest)?;
            let line = int(&mut rest)?;
            let source = guc_source_from(int(&mut rest)?)?;
            let scontext = guc_context_from(int(&mut rest)?)?;
            let srole = int(&mut rest)? as Oid;
            let (&pending, tail) = rest.split_first()?;
            rest = tail;
            Some((name, value, file, line, source, scontext, srole, pending != 0))
        })();
        let Some((name, value, file, line, source, scontext, srole, pending)) = parsed else {
            return Ok(false);
        };
        if source != types_guc::GucSource::PGC_S_DEFAULT {
            crate::set_config_option_ext(
                &name,
                Some(&value),
                scontext,
                source,
                srole,
                crate::GUC_ACTION_SET,
                true,
                ErrorLevel(0),
                true,
            )?;
        }
        if !file.is_empty() {
            crate::process_config::set_config_sourcefile(&name, &file, line);
        }
        if pending {
            with_store_mut(|reg| {
                if let Some(record) = reg.find_option_mut(&name) {
                    record.gen_mut().status |= crate::model::GUC_PENDING_RESTART;
                }
            });
        }
    }
    set_pg_reload_time(i64::from_le_bytes(*reload));
    Ok(true)
}

fn guc_source_from(n: i32) -> Option<GucSource> {
    use types_guc::GucSource::*;
    [
        PGC_S_DEFAULT, PGC_S_DYNAMIC_DEFAULT, PGC_S_ENV_VAR, PGC_S_FILE, PGC_S_ARGV,
        PGC_S_GLOBAL, PGC_S_DATABASE, PGC_S_USER, PGC_S_DATABASE_USER, PGC_S_CLIENT,
        PGC_S_OVERRIDE, PGC_S_INTERACTIVE, PGC_S_TEST, PGC_S_SESSION,
    ]
    .into_iter()
    .find(|s| *s as i32 == n)
}

fn guc_context_from(n: i32) -> Option<GucContext> {
    use types_guc::GucContext::*;
    [PGC_INTERNAL, PGC_POSTMASTER, PGC_SIGHUP, PGC_SU_BACKEND, PGC_BACKEND, PGC_SUSET, PGC_USERSET]
        .into_iter()
        .find(|c| *c as i32 == n)
}

pub use crate::registry::CapturedGuc;

// RestoreGUCState's two passes (guc.c:6236): every restorable variable back to
// its boot value, then `apply` the leader's state over it.
//
// D3649: in C both passes touch only the worker's own copy, but SIGHUP
// backings are process-wide here (guc_tables backing.rs). A variable `held`
// names (one `apply` sets again) keeps its boot publish parked through the
// reset, so no other session sees the boot value in between: autovacuum = off
// read as on by an index build's AutoVacuumingActive, which then wrote the
// heap's reltuples / relpages.
fn reset_then_apply(
    held: impl Fn(&str) -> bool,
    apply: impl FnOnce() -> PgResult<()>,
) -> PgResult<()> {
    let mut deferred_hooks: Vec<DeferredAssignHook> = Vec::new();
    let mut parked = Vec::new();
    with_store_mut(|reg| {
        crate::registry::reset_restorable_to_boot(reg, &mut deferred_hooks, held, &mut parked)
    })
    .unwrap_or_else(|| store_uninitialized("RestoreGUCState"));
    for hook in deferred_hooks {
        hook();
    }
    let applied = apply();
    // A parked variable `apply` did not set keeps its boot value: publish it
    // now, on an error too, so its backing matches the store.
    if !parked.is_empty() {
        let late = with_store(|reg| crate::registry::unapplied_publishes(reg, parked))
            .unwrap_or_else(|| store_uninitialized("RestoreGUCState"));
        for hook in late {
            hook();
        }
    }
    applied
}

/// RestoreGUCState from the string capture (the PGRUST_NO_GUC_BIND path of a
/// parallel worker or parked query task): reset_then_apply over
/// restore_nondefault_variables.
pub fn restore_guc_state(vars: &[NondefaultGuc]) -> PgResult<()> {
    reset_then_apply(
        |name| vars.iter().any(|v| crate::name::guc_name_eq(&v.name, name)),
        || restore_nondefault_variables(vars),
    )
}

#[derive(Clone)]
pub struct ExactGucState {
    registry: GucRegistry,
    pg_reload_time: i64,
    has_stacked_hint: bool,
    report_pending_hint: bool,
}

pub fn capture_exact_guc_state() -> ExactGucState {
    ExactGucState {
        registry: with_store(crate::registry::clone_current_state)
            .unwrap_or_else(|| store_uninitialized("capture_exact_guc_state")),
        pg_reload_time: PG_RELOAD_TIME.get(),
        has_stacked_hint: HAS_STACKED_HINT.get(),
        report_pending_hint: REPORT_PENDING_HINT.get(),
    }
}

pub fn replace_exact_guc_state(state: &ExactGucState) -> ExactGucState {
    let saved = capture_exact_guc_state();
    let mut deferred_hooks = Vec::new();
    with_store_mut(|reg| {
        *reg = state.registry.clone();
        crate::registry::activate_current_values(reg, &mut deferred_hooks);
    })
    .unwrap_or_else(|| store_uninitialized("replace_exact_guc_state"));
    PG_RELOAD_TIME.set(state.pg_reload_time);
    HAS_STACKED_HINT.set(state.has_stacked_hint);
    REPORT_PENDING_HINT.set(state.report_pending_hint);
    for hook in deferred_hooks {
        hook();
    }
    saved
}

// The donor lineage also poisoned a warm-rebind fingerprint (LAST_BIND_FP)
// here; the lane-executor-v2 store has no fingerprint machinery, so envelope
// replacement is identical to the plain exact replacement.
pub fn replace_exact_guc_state_for_envelope(state: &ExactGucState) -> ExactGucState {
    replace_exact_guc_state(state)
}

// PGRUST_NO_GUC_BIND reverts parallel workers to the string
// serialize/restore path (check hooks rerun per launch) for A/B.
pub fn session_guc_bind_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("PGRUST_NO_GUC_BIND").is_none())
}

/// Leader side of §3.4 P-guc: typed capture of every nondefault variable with
/// its validated hook extra.
pub fn capture_session_gucs() -> Vec<CapturedGuc> {
    with_store(crate::registry::capture_session_gucs).unwrap_or_default()
}

/// Claim-side leak check (§5): a standby claiming a task must be bind-free.
pub fn session_bound() -> bool {
    SESSION_BOUND.get()
}

/// Worker-held proof of a live session bind; Drop clears the guard so a
/// retained pool thread can be asserted clean on its next claim.
pub struct SessionGucBinding {
    _not_send: std::marker::PhantomData<*const ()>,
}

impl Drop for SessionGucBinding {
    fn drop(&mut self) {
        SESSION_BOUND.set(false);
    }
}

/// Worker side of §3.4 P-guc: applies the leader's captured session onto this
/// thread's store. Assign hooks fire per variable in capture order, exactly as
/// the restore path fires them; check hooks do not rerun.
pub fn bind_session_gucs(caps: &[CapturedGuc]) -> PgResult<SessionGucBinding> {
    assert!(
        !SESSION_BOUND.get(),
        "bind_session_gucs: thread already carries a session bind"
    );
    SESSION_BOUND.set(true);
    let binding = SessionGucBinding { _not_send: std::marker::PhantomData };
    reset_then_apply(
        |name| crate::registry::recaptured(caps, name),
        || apply_captured_session_gucs_impl(caps, false),
    )?;
    Ok(binding)
}

/// Applies a captured session GUC set onto this thread's store without
/// touching the session-bind marker; the session-envelope restore path
/// reuses it (harvested from 0a71b80a9 + a1c4e48cc, ported onto the
/// lane-executor-v2 store which has no bind fingerprint/stamp machinery).
///
/// Envelope restore is an exact replacement: source priority from the inner
/// binding must not suppress the outer session's captured value.
pub fn apply_captured_session_gucs(caps: &[CapturedGuc]) -> PgResult<()> {
    apply_captured_session_gucs_impl(caps, true)
}

fn apply_captured_session_gucs_impl(caps: &[CapturedGuc], exact: bool) -> PgResult<()> {
    let trace = std::env::var_os("PGRUST_GATHER_TRACE").is_some();
    let t0 = trace.then(std::time::Instant::now);
    for cap in caps {
        let mut deferred_hooks: Vec<DeferredAssignHook> = Vec::new();
        with_store_mut(|reg| crate::registry::bind_captured_guc(reg, cap, &mut deferred_hooks, exact))
            .unwrap_or_else(|| store_uninitialized("apply_captured_session_gucs"))?;
        for hook in deferred_hooks {
            hook();
        }
    }
    if let Some(t0) = t0 {
        eprintln!("GTRACE guc.bind n={} total_us={}", caps.len(), t0.elapsed().as_micros());
    }
    Ok(())
}

pub fn with_store<R>(f: impl FnOnce(&GucRegistry) -> R) -> Option<R> {
    GUC_STORE.with(|c| {
        let guard = c.borrow();
        Some(f(guard.as_ref()?))
    })
}

pub fn with_store_mut<R>(f: impl FnOnce(&mut GucRegistry) -> R) -> Option<R> {
    STORE_MUTATIONS.set(STORE_MUTATIONS.get().wrapping_add(1));
    GUC_STORE.with(|c| {
        let mut guard = c.borrow_mut();
        Some(f(guard.as_mut()?))
    })
}

#[cold]
fn store_uninitialized(name: &str) -> ! {
    panic!("GUC store access for {name:?} before InitializeGUCOptions built it")
}

fn lookup_var<R>(name: &str, pick: impl FnOnce(&GucVariable) -> Option<R>) -> Option<R> {
    with_store(|reg| reg.find_option(name).and_then(pick))
        .unwrap_or_else(|| store_uninitialized(name))
}

pub fn get_bool(name: &str) -> Option<bool> {
    lookup_var(name, |v| match v {
        GucVariable::Bool(c) => c.value,
        _ => None,
    })
}

pub fn get_int(name: &str) -> Option<i32> {
    lookup_var(name, |v| match v {
        GucVariable::Int(c) => c.value,
        _ => None,
    })
}

pub fn get_real(name: &str) -> Option<f64> {
    lookup_var(name, |v| match v {
        GucVariable::Real(c) => c.value,
        _ => None,
    })
}

pub fn get_enum(name: &str) -> Option<i32> {
    lookup_var(name, |v| match v {
        GucVariable::Enum(c) => c.value,
        _ => None,
    })
}

pub fn get_string(name: &str) -> Option<Option<String>> {
    lookup_var(name, |v| match v {
        GucVariable::String(c) => c.value.clone(),
        _ => None,
    })
}

// set_config_option over the global store: check hook under a shared borrow
// (check_datestyle reads the store), assign hooks after release (they re-enter).
#[allow(clippy::too_many_arguments)]
pub fn set_config_option_global(
    name: &str,
    value: Option<&str>,
    context: GucContext,
    source: GucSource,
    srole: Oid,
    action: crate::registry::GucAction,
    change_val: bool,
    elevel: ErrorLevel,
    is_reload: bool,
) -> PgResult<i32> {
    use crate::registry::{
        set_config_option_apply, set_config_option_prepare, set_config_option_value,
        SetConfigPrepared,
    };

    let prep = match with_store_mut(|reg| {
        set_config_option_prepare(
            reg, name, value, context, source, srole, action, change_val, elevel, is_reload,
        )
    })
    .unwrap_or_else(|| store_uninitialized(name))
    {
        SetConfigPrepared::Done(result) => return result,
        SetConfigPrepared::Pending(prep) => prep,
    };

    crate::reset_guc_check_error();
    let (newval, newextra) = match with_store(|reg| set_config_option_value(reg, &prep, value, source))
        .unwrap_or_else(|| store_uninitialized(name))
    {
        Ok(parsed) => parsed,
        Err(e) if crate::take_guc_check_thrown() => return Err(e),
        Err(e) => return crate::registry::reject(prep.elevel, *e),
    };

    let mut deferred_hooks: Vec<DeferredAssignHook> = Vec::new();
    let result = with_store_mut(|reg| {
        set_config_option_apply(
            reg,
            prep,
            name,
            value,
            newval,
            newextra,
            context,
            source,
            srole,
            action,
            is_reload,
            &mut deferred_hooks,
        )
    })
    .unwrap_or_else(|| store_uninitialized(name));

    for hook in deferred_hooks {
        hook();
    }

    result
}

pub fn reset_all_options() {
    with_store_mut(crate::registry::reset_all_options)
        .unwrap_or_else(|| store_uninitialized("ResetAllOptions"));
}

/// Scrub a REUSED pooled/retained worker thread's GUC store back to the
/// session-neutral fresh-backend baseline (boot defaults overlaid with the
/// postmaster's config), ready for the new leader's GUC pin.
///
/// `ResetAllOptions` alone is unsafe on a reused thread: by C parity it
/// preserves every value sourced `<= PGC_S_OVERRIDE` (client startup options
/// via PGOPTIONS, `ALTER ROLE`/`ALTER DATABASE ... SET`) and the `reset_val`
/// stamps `make_default` bookkeeping wrote for them, so a PRIOR session's
/// `search_path`/`TimeZone`/etc. -- and the worker-side name resolution and
/// privilege context they drive -- would stay live under the NEXT (different
/// user's) session's parallel-worker task. A C parallel worker is a fresh
/// process and cannot leak this way; this reproduces that guarantee for the
/// thread-pooled engine by rebuilding the fresh-backend store before every
/// cross-session rebind.
pub fn reset_store_to_process_base() {
    // 1. Return every session-settable variable to its boot default, discarding
    //    the prior session's low-source values AND their reset_val stamps
    //    (the class ResetAllOptions cannot reach).
    let mut deferred_hooks: Vec<DeferredAssignHook> = Vec::new();
    with_store_mut(|reg| crate::registry::reset_session_options_to_boot(reg, &mut deferred_hooks))
        .unwrap_or_else(|| store_uninitialized("reset_store_to_process_base"));
    for hook in deferred_hooks {
        hook();
    }
    with_store_mut(crate::registry::GucRegistry::remove_all_placeholders)
        .unwrap_or_else(|| store_uninitialized("reset_store_to_process_base"));
    // 2. Re-overlay the postmaster config base (postgresql.conf/env/argv) that a
    //    freshly initialized backend has over boot defaults, so legitimate
    //    config-file settings are preserved. The base is session-neutral and
    //    was validated by the postmaster; a bind failure here must not abort the
    //    claim (the store is already at a safe boot baseline).
    if crate::layers::base_share_enabled() {
        let base = crate::layers::current_base();
        let _ = crate::layers::bind_base(&base);
    }
}

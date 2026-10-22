//! Gravity backends: a v1 IR program as `gravity_program::Code` — mapped through TPDE, or interpreted as tier 0 (docs/gravity/backend.md).
#![deny(unsafe_code)]

use gravity_abi::Ctx;
use gravity_ir::{Func, Program, Type};
use gravity_program::Code;
use mcx::{MemoryContext, SendRoot};
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};

pub use gravity_interp;
pub use gravity_ir;
pub use gravity_tpde;

pub mod cache;
pub mod dump;
pub mod perfmap;
pub mod promote;
mod raw;
#[cfg(target_family = "wasm")]
pub mod wasm;
pub use cache::{Lookup, Outcome};
pub use raw::{mapped, with_row_estimate};
use raw::{interp_run, rows_read};

/// The entry signature every `Code` speaks (abi-v2 §2): one `Ptr` (the ctx) in, one `I64` (the status) out.
pub fn check_entry(program: &Program, entry: Func) -> Result<(), String> {
    let f = program.funcs.get(entry.index()).ok_or_else(|| format!("gravity_backend: entry f{} of {} functions", entry.0, program.funcs.len()))?;
    if f.params.as_slice() != [Type::Ptr] || f.returns.as_slice() != [Type::I64] {
        return Err(format!("gravity_backend: entry {} is {:?} -> {:?}, not (ptr) -> (i64)", f.name, f.params, f.returns));
    }
    Ok(())
}

struct Mapped {
    compiled: gravity_tpde::Compiled<'static>,
    context: SendRoot,
}

fn check_entries(program: &Program, entries: &[Func]) -> PgResult<()> {
    for &e in entries {
        if let Err(m) = check_entry(program, e) {
            return Err(PgError::error(m).with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE).into());
        }
    }
    Ok(())
}

/// A `Code` per entry of `program` through the process-shared code cache (`cache`, docs/gravity/code-cache.md): the module
/// compiled once per content per process — every function in one compile, so a program of n pipelines costs one compile, not
/// n (D230) — and a hit for every later caller; the `Outcome` says which.
///
/// Tier 0 (H16 (d)): with `pgrust.gravity_tier0_rows` non-zero, a miss the interpreter can take hands back
/// tier-0 codes instead (`Code::tiered`, nothing compiled, nothing cached): the driver interprets the program's first
/// execution when its pipelines are serial and predict at most that many rows, and compiles through this cache at the
/// start of any other execution. 0 = every miss compiles here, as before.
pub fn mapped_cached_all(program: &Program, entries: &[Func]) -> PgResult<(Vec<Code>, Outcome)> {
    mapped_cached_keyed(program, entries, None, &[])
}

/// `mapped_cached_all` under the program's code key when the caller holds it (`key`: `cache::key_of(program)`, gravity_lower
/// `Lowered::key`, taken once by the lowerer); `None`: taken here. D3886: the key is taken once per program, where a miss
/// hashed the whole IR in the lookup, again in `tiered_codes` or the compile, and once more in the pool's compile.
/// D4005: `sites` = lines of the program that run on fewer rows than the statement reads, each with those rows (exec
/// knobs/tier0.rs `site_rows`: a projection's entries and its node's planned rows); the derived cap weighs them (`sited_cap`).
pub fn mapped_cached_keyed(program: &Program, entries: &[Func], key: Option<cache::CodeKey>, sites: &[(u32, u64)]) -> PgResult<(Vec<Code>, Outcome)> {
    check_entries(program, entries)?;
    debug_assert!(key.map_or(true, |k| k == cache::key_of(program)), "gravity backend: a code key that is not the program's");
    let key = key.unwrap_or_else(|| cache::key_of(program));
    let (cap, derived) = tier0_rows(program);
    let (rows, bound) = if cap != 0 { rows_read() } else { (u64::MAX, u64::MAX) };
    let cap = if derived { sited_cap(program, rows, sites).unwrap_or(cap) } else { cap };
    if cap != 0 && rows <= cap {
        if let Some(h) = cache::lookup_keyed(key, entries) {
            return Ok(h);
        }
        match tier0_ok(program) {
            Ok(()) => {
                fires::fire("tier0.interpreted"); // shapecheck fire signal (REGISTRY EX-18): a miss runs its first execution interpreted
                return tiered_codes(program, entries, cap, bound, key);
            }
            Err(_) => {
                TIER0_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    return compiled_keyed(key, program, entries);
    #[cfg(target_os = "macos")]
    return compiled_or_interpreted(key, program, entries);
}

/// macOS safety net: a compile the platform refused to map executable (`gravity_tpde::MAPPING_REFUSED`, e.g. a build
/// whose runtime forbids writable-then-executable memory) runs the program interpreted (tier 0's interpreter, the
/// reference semantics, uncached: the next miss tries the compile again) instead of failing the statement. Every other
/// error, and a program the interpreter refuses, stays the compile's error.
#[cfg(target_os = "macos")]
fn compiled_or_interpreted(key: cache::CodeKey, program: &Program, entries: &[Func]) -> PgResult<(Vec<Code>, Outcome)> {
    match compiled_keyed(key, program, entries) {
        Err(e) if e.message() == gravity_tpde::MAPPING_REFUSED && tier0_ok(program).is_ok() => {
            MAPPING_REFUSED_INTERPRETED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let shared = std::sync::Arc::new(program.clone_owned(Some(cache::PROGRAM_CACHES))); // alloc: Program
            let mut out = Vec::with_capacity(entries.len()); // alloc: Program
            for &f in entries {
                out.push(interp_shared(shared.clone(), f, key.0).map_err(|m| Box::new(PgError::error(m)))?);
            }
            Ok((out, Outcome { lookup: Lookup::Miss, key, bytes: 0, timing: Default::default(), ns: 0 }))
        }
        r => r,
    }
}

/// Misses whose compile the platform refused to map, run interpreted instead (`compiled_or_interpreted`, macOS only).
#[cfg(target_os = "macos")]
pub static MAPPING_REFUSED_INTERPRETED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A TPDE compile's scratch context, owned at birth as the code's root is (`cache::birth_owner`: the compiling
/// statement's on a backend, the program caches' on the compile pool): `gravity_tpde::compile_owned`'s `own`.
fn own_scratch(c: &MemoryContext) {
    c.set_owner(cache::birth_owner());
}

#[cfg(not(target_family = "wasm"))]
fn compiled_all(program: &Program, entries: &[Func]) -> PgResult<(Vec<Code>, Outcome)> {
    cache::get_or_compile_all(program, entries, |ctx| gravity_tpde::compile_owned(ctx.mcx(), program, own_scratch))
}

/// The compile pool's compile of a module (promote.rs `run`): `compiled_all`, with the key held `hold_ms` first inside its
/// claim (D3811's fault injection, `TIER0_HOLD_ENV`), so a promotion of the same module waits on that compile as it waits
/// on a long one (cache `Flight::wait`).
#[cfg(not(target_family = "wasm"))]
pub(crate) fn pool_compiled_all(program: &Program, entries: &[Func], hold_ms: u64, key: cache::CodeKey) -> PgResult<(Vec<Code>, Outcome)> {
    cache::get_or_compile_keyed(key, program, entries, |ctx| {
        if hold_ms != 0 {
            pgsync::thread::sleep(std::time::Duration::from_millis(hold_ms));
        }
        perfmap::with_tags(key.0, |tags| gravity_tpde::compile_tagged(ctx.mcx(), program, own_scratch, tags))
    })
}

#[cfg(target_family = "wasm")]
pub(crate) fn pool_compiled_all(program: &Program, entries: &[Func], _hold_ms: u64, _key: cache::CodeKey) -> PgResult<(Vec<Code>, Outcome)> {
    compiled_all(program, entries)
}

/// `compiled_all` under the program's key, taken once by the caller (`tiered_codes`).
#[cfg(not(target_family = "wasm"))]
fn compiled_keyed(key: cache::CodeKey, program: &Program, entries: &[Func]) -> PgResult<(Vec<Code>, Outcome)> {
    cache::get_or_compile_keyed(key, program, entries, |ctx| perfmap::with_tags(key.0, |tags| gravity_tpde::compile_tagged(ctx.mcx(), program, own_scratch, tags)))
}

/// wasm32: one gravity_wasm module installed into the server's function table (`wasm`, lane wasmjit stage 2); TPDE is
/// native-only.
#[cfg(target_family = "wasm")]
fn compiled_all(program: &Program, entries: &[Func]) -> PgResult<(Vec<Code>, Outcome)> {
    wasm::compiled_all(program, entries)
}

#[cfg(target_family = "wasm")]
fn compiled_keyed(_key: cache::CodeKey, program: &Program, entries: &[Func]) -> PgResult<(Vec<Code>, Outcome)> {
    wasm::compiled_all(program, entries)
}

/// `pgrust.gravity_tier0_rows` (docs/gravity/breakers.md "Knobs", exec knobs/tier0.rs): the rows at most every pipeline of a
/// tier-0 program predicts; 0 = tier 0 off; unset = the cost home's break-even for the program's IR (`tier0_cap`). Read at
/// each miss through the GUC seam (a session's SET counts).
pub const TIER0_GUC: &str = "pgrust.gravity_tier0_rows";
/// `PGRUST_GRAVITY_TIER0`: `force` = every miss the interpreter can take is tier 0 whatever its rows (the agreement
/// run); `off` = none, whatever the option says; `fail` = `force`, and every promotion's compile fails (the fault
/// injection: the code stays interpreted, rows = PG's) (read once).
pub const TIER0_ENV: &str = "PGRUST_GRAVITY_TIER0";
/// `PGRUST_GRAVITY_TIER0_HOLD_MS` (exec knobs/tier0.rs; D3811's fault injection, server start only, as
/// `PGRUST_GRAVITY_TIER0=fail`): the compile pool's compile of a module a tier-0 miss hands it holds the module's key this
/// many ms before it compiles (at most the cost home's `TIER0_HOLD_MAX_MS`), so the program's next execution waits for it
/// (cache `Flight::wait`) as for a long compile. Unset, unparsable or 0 = no hold (read once).
pub const TIER0_HOLD_ENV: &str = "PGRUST_GRAVITY_TIER0_HOLD_MS";

fn tier0_hold_ms() -> u64 {
    static HOLD: pgsync::OnceLock<u64> = pgsync::OnceLock::new();
    *HOLD.get_or_init(|| gravity_driver::env::num::<u64>(TIER0_HOLD_ENV).unwrap_or(0).min(gravity_program::cost::runtime::TIER0_HOLD_MAX_MS))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tier0Env {
    Unset,
    Force,
    Off,
    Fail,
}

fn tier0_env() -> Tier0Env {
    static FORCED: pgsync::OnceLock<Tier0Env> = pgsync::OnceLock::new();
    *FORCED.get_or_init(|| match gravity_driver::env::text(TIER0_ENV).as_deref().map(str::trim) {
        Some("force") => Tier0Env::Force,
        Some("off") => Tier0Env::Off,
        Some("fail") => Tier0Env::Fail,
        _ => Tier0Env::Unset,
    })
}

/// The row cap for this miss: the env override, else the option, else the break-even for `program` (`tier0_cap`); 0
/// without the GUC seam. And whether it is the break-even (`sited_cap` may weigh it).
fn tier0_rows(program: &Program) -> (u64, bool) {
    match tier0_env() {
        Tier0Env::Force | Tier0Env::Fail => (u64::MAX, false),
        Tier0Env::Off => (0, false),
        Tier0Env::Unset if gravity_driver::env::guc_installed() => gravity_driver::env::guc_num::<u64>(TIER0_GUC).map_or_else(|| (tier0_cap(program), true), |n| (n, false)),
        Tier0Env::Unset => (0, false),
    }
}

/// The cost home's tier-0 cap for `program` (gap-optoverfit EX-18, `gravity_program::cost::compile`): the rows whose
/// interpretation costs half the compile of its IR lines (every function's instructions and block terminators).
pub fn tier0_cap(program: &Program) -> u64 {
    gravity_program::cost::compile::tier0_rows(ir_lines(program))
}

/// Every function's instructions and block terminators.
fn ir_lines(program: &Program) -> u64 {
    program.funcs.iter().flat_map(|f| f.blocks.iter()).map(|b| b.insts.len() as u64 + 1).sum()
}

/// D4005: the break-even when some of the program's lines run on fewer rows than the statement reads (`rows`): each site's
/// lines count the share of a statement row they run on (its rows over `rows`, at most 1), the rest a whole row, so the
/// interpreter's extra per row is that many lines' (`cost::compile::tier0_rows_running`); the compile is every line's.
/// `None` (the plain cap) without a site or a bound on the rows. The tagged lines are the lowering's, before dead blocks
/// were emptied, so they are held to the program's.
fn sited_cap(program: &Program, rows: u64, sites: &[(u32, u64)]) -> Option<u64> {
    sited_rows(ir_lines(program), rows, sites)
}

/// `sited_cap` over a program of `lines` lines.
fn sited_rows(lines: u64, rows: u64, sites: &[(u32, u64)]) -> Option<u64> {
    if sites.is_empty() || rows == 0 || rows == u64::MAX {
        return None;
    }
    let saved: f64 = sites.iter().map(|&(l, r)| l as f64 * (1.0 - r.min(rows) as f64 / rows as f64)).sum();
    Some(gravity_program::cost::compile::tier0_rows_running(lines, lines as f64 - saved.min(lines as f64)))
}

#[cfg(test)]
mod sited {
    use super::sited_rows;
    use gravity_program::cost::compile::tier0_rows;

    #[test]
    fn a_projection_over_few_rows_raises_the_cap() {
        // 50,000 of 55,000 lines project the 2 rows a filter keeps of 1,000 read: the cap clears the 1,000
        let cap = sited_rows(55_000, 1_000, &[(50_000, 2)]).unwrap();
        assert!(cap > 1_000 && tier0_rows(55_000) < 1_000, "{cap} against the plain {}", tier0_rows(55_000));
        // a site on every row the statement reads (or more, or unknown) saves nothing
        assert_eq!(sited_rows(55_000, 1_000, &[(50_000, 1_000)]), Some(tier0_rows(55_000)));
        assert_eq!(sited_rows(55_000, 1_000, &[(50_000, u64::MAX)]), Some(tier0_rows(55_000)));
        // no site, or no bound on the statement's rows: the plain cap
        assert_eq!(sited_rows(55_000, 1_000, &[]), None);
        assert_eq!(sited_rows(55_000, u64::MAX, &[(50_000, 2)]), None);
        // tagged lines past the program's (dead blocks emptied after the lowering) are held to its lines
        assert_eq!(sited_rows(1_000, 1_000, &[(5_000, 0)]), sited_rows(1_000, 1_000, &[(1_000, 0)]));
    }
}

/// Misses tier 0 could not take (a host the interpreter refuses), compiled at once.
pub static TIER0_REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// What the interpreter refuses at run time (`Trap::UnsupportedHostSignature`), checked before tier 0 is chosen: a host
/// with more than 8 arguments or a float argument or result.
fn tier0_ok(program: &Program) -> Result<(), String> {
    for h in program.host_fns.iter() {
        if h.params.len() > 8 || h.params.iter().any(|t| t.is_float()) || h.ret.is_some_and(|t| t.is_float()) {
            return Err(format!("host {} {:?} -> {:?}", h.name, h.params, h.ret)); // alloc: Cold
        }
    }
    Ok(())
}

/// One tier-0 code per entry: the interpreter over a copy of the IR now, the whole module compiled through the cache at
/// the promotion (every entry's `up` compiles it once; the later ones hit). Each code's identity is the IR's content key
/// and its entry (D303): the program key tells two tier-0 programs of one descriptor shape apart.
/// The module's compile is handed to the compile pool now (`promote`), so the promotion finds it filed, or waits for its
/// running compile; not under fault injection (`PGRUST_GRAVITY_TIER0=fail`: the promotion must fail) or with the cache off.
fn tiered_codes(program: &Program, entries: &[Func], cap: u64, bound: u64, key: cache::CodeKey) -> PgResult<(Vec<Code>, Outcome)> {
    let id = key.0;
    // one copy of the IR for every entry's interpreter and the promotion's compile, on the program caches (it outlives the
    // statement: the codes and the compile pool hold it)
    let shared = std::sync::Arc::new(program.clone_owned(Some(cache::PROGRAM_CACHES))); // alloc: Program
    let all: std::sync::Arc<[Func]> = entries.into(); // alloc: Program
    if tier0_env() != Tier0Env::Fail && cache::budget_bytes() != 0 {
        promote::submit(shared.clone(), all.clone(), tier0_hold_ms(), key);
    }
    let mut out = Vec::with_capacity(entries.len()); // alloc: Program
    for (i, &f) in entries.iter().enumerate() {
        let t0 = interp_shared(shared.clone(), f, id).map_err(|e| Box::new(PgError::error(e)))?;
        let (p, es) = (shared.clone(), all.clone());
        let up = move || promoted(&p, &es, i, key);
        out.push(Code::tiered(t0, cap, Box::new(up)).tier0_bound(bound).tier0_module(id).with_identity(id ^ f.0 as u128)); // alloc: Program
    }
    Ok((out, Outcome { lookup: Lookup::Miss, key, bytes: 0, timing: Default::default(), ns: 0 }))
}

/// A tier-0 code's promotion: entry `i` of the whole module compiled through the cache. The code generator's refusal or
/// caught panic comes back as the reason (the code stays interpreted, `Tiered::promote`); a resource error PG would raise
/// too (class 53, insufficient resources: out of memory; class 57, operator intervention: a cancel) is thrown as an ereport
/// is, the statement's error as PG's.
///
/// D3811: a compile stopped by a pending interrupt (D2909's stop points), or a wait for the compile pool's compile of the
/// module ended by one (cache `Flight::wait`), takes the interrupt through the host's check (`cache::take_interrupt`):
/// PostgreSQL's own error (a cancel's 57014, a terminate's FATAL) thrown as an ereport is, and the code is not failed (a
/// failure is never retried: a cancel would leave the program interpreted for good). An interrupt that raised nothing (a
/// catch-up) promotes again. With no check installed (a test without the host) it is the reason, as before.
fn promoted(p: &Program, es: &[Func], i: usize, key: cache::CodeKey) -> Result<Code, String> {
    promoted_out(p, es, i, key).map(|c| c.0)
}

/// `promoted`, with the code cache's outcome for it (`Outcome::lookup`: whether this call compiled; `ns`: the compile, or
/// the wait for another's).
fn promoted_out(p: &Program, es: &[Func], i: usize, key: cache::CodeKey) -> Result<(Code, Outcome), String> {
    if tier0_env() == Tier0Env::Fail {
        return Err("gravity backend: PGRUST_GRAVITY_TIER0=fail (fault injection)".to_string()); // alloc: Cold
    }
    loop {
        match compiled_keyed(key, p, es) {
            Ok((mut c, out)) => return Ok((c.swap_remove(i), out)),
            Err(e) if e.message() == cache::INTERRUPTED => match cache::take_interrupt() {
                Err(raised) => std::panic::panic_any(raised),
                Ok(true) => continue,
                Ok(false) => return Err(e.message().to_string()), // alloc: Cold
            },
            Err(e) => {
                let class = types_error::error::errcode_to_category(e.sqlstate());
                if class == types_error::error::errcode_to_category(types_error::error::ERRCODE_OUT_OF_MEMORY) || class == types_error::error::errcode_to_category(types_error::error::ERRCODE_QUERY_CANCELED) {
                    std::panic::panic_any(e);
                }
                return Err(e.message().to_string()); // alloc: Cold
            }
        }
    }
}

/// D3168 S0: a pipeline's staged function (gravity_program `Staged`) from `side`, that function alone
/// (`gravity_ir::Program::leaf`), copied onto the program caches as tier 0's IR is: compiled through the code cache the
/// first time its plain function hands a call on, once per module in the process; interpreted under `interp` (the plain
/// code's own form: a rig's interpreter) and where the code generator refuses it (as a failed promotion is), so the rows
/// never depend on the compile. An interrupt or a resource error the compile meets is thrown as `promoted` throws it.
/// `tag`: its shape's memo entry (gravity_program `stage_memo`), marked chosen at the make; 0: none.
/// The IR is the code cache's (`cache::file_side`): in its bytes, freed by its trim. A side whose IR the trim took makes
/// no code: its plain function runs with the stage off, and its shape, chosen, lowers the staged function again.
pub fn staged(side: &Program, interp: bool, tag: u64) -> Box<dyn gravity_program::code::Staged> {
    let own = memledger::charge::Charge::open(cache::PROGRAM_CACHES, memledger::charge::release_of(cache::PROGRAM_CACHES), memledger::Kind::Region, memledger::charge::Door::Heap, core::mem::size_of::<Side>() as u64);
    let ir = cache::file_side(side.clone_owned(Some(cache::PROGRAM_CACHES))); // alloc: Program charged PROGRAM_CACHES (the IR's root is the caches' from its birth; SideIr::own its Arc block and root header)
    Box::new(Side { _own: own, ir, interp, tag, trimmed: core::sync::atomic::AtomicBool::new(false), code: pgsync::OnceLock::new() }) // alloc: Program charged PROGRAM_CACHES (Side::_own: the box; the IR's root is the caches' from its birth)
}

/// `staged`'s function: its box's bytes on the program caches (`_own`), its IR in the code cache, the code once made.
struct Side {
    _own: memledger::charge::Charge,
    ir: cache::SideRef,
    interp: bool,
    tag: u64,
    trimmed: core::sync::atomic::AtomicBool,
    code: pgsync::OnceLock<Code>,
}

impl Drop for Side {
    fn drop(&mut self) {
        self.ir.forget();
    }
}

impl gravity_program::code::Staged for Side {
    fn code(&self) -> Option<&Code> {
        use core::sync::atomic::Ordering::Relaxed;
        if let Some(c) = self.code.get() {
            return Some(c);
        }
        if self.trimmed.load(Relaxed) {
            return None;
        }
        if self.tag != 0 {
            gravity_program::code::stage_memo::chose(self.tag);
        }
        let Some(ir) = self.ir.get() else {
            self.trimmed.store(true, Relaxed);
            return None;
        };
        let _ = self.code.set(staged_made(ir.ir(), self.interp));
        drop(ir);
        // its module is the code cache's (an interpreted one has its own copy): the IR is done
        self.ir.forget();
        self.code.get()
    }
}

fn staged_made(p: &Program, interpret: bool) -> Code {
    use std::sync::atomic::Ordering::Relaxed;
    let (key, f) = (cache::key_of(p), Func(0));
    let made = if interpret { Err(String::new()) } else { promoted_out(p, &[f], 0, key) };
    let code = match made {
        Ok((code, out)) => {
            if out.lookup != Lookup::Hit {
                gravity_program::code::STAGED_COMPILED.fetch_add(1, Relaxed);
                gravity_program::code::STAGED_NS.fetch_add(out.ns, Relaxed);
            }
            code
        }
        Err(_) => interp(p.clone_owned(Some(cache::PROGRAM_CACHES)), f).expect("a staged function is a pipeline entry"),
    };
    gravity_program::code::STAGED_MADE.fetch_add(1, Relaxed);
    fires::fire_shared("join.probe.staged.made"); // shapecheck fire signal (D3168 S0): a staged function made at run time
    code
}

/// `mapped_cached_all` for one entry.
pub fn mapped_cached(program: &Program, entry: Func) -> PgResult<(Code, Outcome)> {
    let (mut codes, o) = mapped_cached_all(program, &[entry])?;
    Ok((codes.pop().expect("one entry, one code"), o))
}

/// Never tier 0: the driver never promotes a kernel's code.
pub fn compiled(program: &Program, entry: Func) -> PgResult<(Code, Outcome)> {
    check_entries(program, &[entry])?;
    let (mut codes, o) = compiled_all(program, &[entry])?;
    Ok((codes.pop().expect("one entry, one code"), o))
}

/// Server start (the catalog pre-warm worker): one compile through the cache, so the process's
/// first miss pays its own compile alone — the cache's parent context (a thread of its own) and
/// the compiler's first-use paths cost a fresh server's first statement ~0.5 ms
/// (docs/gravity/cold-floor.md §2). The entry stays cached under its content key (a few bytes).
pub fn warm() -> PgResult<()> {
    let mut pb = gravity_ir::builder::ProgramBuilder::new();
    let f = pb.declare_func("warm", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let k = b.iconst(Type::I64, 0);
    b.ret(&[k]);
    drop(b);
    compiled_all(&pb.finish(), &[f]).map(|_| ())
}

struct InterpState {
    program: std::sync::Arc<Program>,
    entry: Func,
}

/// `Code::interp` over `program`'s `entry`: tier 0, the reference semantics; every call walks the IR. Its identity is the
/// IR's content key and the entry (D303: every interpreted entry is one function, so the program key must see the IR).
pub fn interp(program: Program, entry: Func) -> Result<Code, String> {
    let id = cache::key_of(&program).0;
    interp_shared(std::sync::Arc::new(program), entry, id) // alloc: Program
}

/// `interp` over a program shared with its other entries (tier 0's codes of one module: one copy of the IR, one content
/// key `key` taken by the caller).
fn interp_shared(program: std::sync::Arc<Program>, entry: Func, key: u128) -> Result<Code, String> {
    check_entry(&program, entry)?;
    Ok(Code::interp(interp_run, Box::new(InterpState { program, entry })).with_identity(key ^ entry.0 as u128)) // alloc: Program
}

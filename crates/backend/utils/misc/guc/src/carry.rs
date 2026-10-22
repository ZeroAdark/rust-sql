//! D3919: a claim on another thread runs with its leader's session GUCs, as PostgreSQL's parallel worker does through
//! SerializeGUCState / RestoreGUCState. The capture is the leader's query pin (`layers::current_query_pin`); `arm_claim`
//! applies every pinned variable `can_skip_gucvar` would not skip (and no PGC_SIGHUP one but the `THREAD_SIGHUP` ones,
//! no `NOT_CARRIED` one, no placeholder) where the claim thread's backing differs: assign hook with the leader's extra
//! (no check hook reruns), then the backing, guc.c's order. The guard puts back what it changed, innermost first; the
//! claim thread's store records are not touched. The plan is built once per pin per thread, and an outermost claim under
//! an unmoved store compares only the variables that differed at its last claim of that pin.
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use guc_tables::{GucBoolAssignFn, GucBoolVar, GucEnumAssignFn, GucEnumVar, GucIntAssignFn, GucIntVar, GucRealAssignFn, GucRealVar, GucStringAssignFn, GucStringVar};
use types_error::{PgError, PgResult};
use types_guc::{GucContext, GUC_CUSTOM_PLACEHOLDER, PGC_S_DEFAULT};

use crate::layers::GucQuerySnapshot;
use crate::model::{config_var_val, SharedExtra};
use crate::registry::{installed_hook, GucRegistry, GucVariable};

/// Set by other means in PostgreSQL's ParallelWorkerMain: the transaction modes (its transaction state), client_encoding
/// (the database's), and the identity pair, whose assign hooks would move the pool thread's own identity (a claim runs
/// no privilege check).
const NOT_CARRIED: [&str; 6] = ["transaction_isolation", "transaction_read_only", "transaction_deferrable", "client_encoding", "role", "session_authorization"];

/// D4244: the PGC_SIGHUP variables whose backing is a thread_local! one, so a claim thread holds the value it started with
/// (no pool thread runs ProcessConfigFile): the server-log settings a log line a claim writes reads on its own thread
/// (elog report.rs log_status_format, send_message_to_server_log, syslog.rs), and D4240's wal_sync_method and fsync, which a
/// claim's own WAL write reads (XLogWrite's fsync after a dirty-buffer eviction's XLogFlush). They are carried as PostgreSQL's
/// RestoreGUCState carries every PGC_SIGHUP variable to its worker. Every other PGC_SIGHUP backing is process-wide: the
/// reload itself writes it, and a claim's undo would put an older value back over a reload, so none is carried.
const THREAD_SIGHUP: [&str; 6] = ["log_line_prefix", "log_destination", "syslog_sequence_numbers", "syslog_split_messages", "wal_sync_method", "fsync"];

#[derive(Clone, Copy)]
enum Slot {
    Bool(&'static GucBoolVar, Option<GucBoolAssignFn>),
    Int(&'static GucIntVar, Option<GucIntAssignFn>),
    Real(&'static GucRealVar, Option<GucRealAssignFn>),
    Str(&'static GucStringVar, Option<GucStringAssignFn>),
    Enum(&'static GucEnumVar, Option<GucEnumAssignFn>),
}

impl Slot {
    fn of(record: &GucVariable) -> Option<Slot> {
        // a backing no owning unit installed holds no value a claim could read
        match record {
            GucVariable::Bool(c) => c.variable.installed().then(|| Slot::Bool(c.variable, installed_hook(c.assign_hook))),
            GucVariable::Int(c) => c.variable.installed().then(|| Slot::Int(c.variable, installed_hook(c.assign_hook))),
            GucVariable::Real(c) => c.variable.installed().then(|| Slot::Real(c.variable, installed_hook(c.assign_hook))),
            GucVariable::String(c) => c.variable.installed().then(|| Slot::Str(c.variable, installed_hook(c.assign_hook))),
            GucVariable::Enum(c) => c.variable.installed().then(|| Slot::Enum(c.variable, installed_hook(c.assign_hook))),
        }
    }

    fn read(self) -> config_var_val {
        match self {
            Slot::Bool(v, _) => config_var_val::Boolval(v.read()),
            Slot::Int(v, _) => config_var_val::Intval(v.read()),
            Slot::Real(v, _) => config_var_val::Realval(v.read()),
            Slot::Str(v, _) => config_var_val::Stringval(v.read()),
            Slot::Enum(v, _) => config_var_val::Enumval(v.read()),
        }
    }

    /// The assign hook with `extra`, then the backing write (guc.c's order: the hook still sees the old value in its
    /// variable). A value of another type than the slot's writes nothing.
    fn set(self, val: config_var_val, extra: Option<&SharedExtra>) {
        let extra = extra.map(|e| &**e);
        match (self, val) {
            (Slot::Bool(v, hook), config_var_val::Boolval(x)) => {
                if let Some(f) = hook {
                    f(x, extra);
                }
                v.write(x);
            }
            (Slot::Int(v, hook), config_var_val::Intval(x)) => {
                if let Some(f) = hook {
                    f(x, extra);
                }
                v.write(x);
            }
            (Slot::Real(v, hook), config_var_val::Realval(x)) => {
                if let Some(f) = hook {
                    f(x, extra);
                }
                v.write(x);
            }
            (Slot::Str(v, hook), config_var_val::Stringval(x)) => {
                if let Some(f) = hook {
                    f(x.as_deref(), extra);
                }
                v.write(x);
            }
            (Slot::Enum(v, hook), config_var_val::Enumval(x)) => {
                if let Some(f) = hook {
                    f(x, extra);
                }
                v.write(x);
            }
            _ => {}
        }
    }
}

/// `idx`: the claim thread's registry index, stable for its registry's life (`store_generation`).
struct Entry {
    idx: usize,
    slot: Slot,
    want: config_var_val,
    extra: Option<SharedExtra>,
}

struct Plan {
    pin: Arc<GucQuerySnapshot>,
    gen: u64,
    entries: Vec<Entry>,
    /// The entries that differed at the thread's last outermost claim of this pin, under the store mutation count then
    /// (`store_mutation_count`): while the count holds, the others still equal the leader's.
    differed: RefCell<Option<(u64, Vec<u32>)>>,
}

thread_local! {
    static PLAN: RefCell<Option<Rc<Plan>>> = const { RefCell::new(None) };
    /// The values the claims armed on this thread applied (registry index, the value's extra), innermost last: the
    /// extra a nested claim's undo puts back with an outer claim's value.
    static APPLIED: RefCell<Vec<(usize, Option<SharedExtra>)>> = const { RefCell::new(Vec::new()) };
}

#[track_caller]
#[cold]
fn oom() -> Box<PgError> {
    elog::ereport(types_error::ERROR).errcode(types_error::ERRCODE_OUT_OF_MEMORY).errmsg("out of memory").into_error().into()
}

fn carried(reg: &GucRegistry, name: &str) -> Option<(usize, Slot)> {
    if NOT_CARRIED.iter().any(|n| crate::name::guc_name_eq(n, name)) {
        return None;
    }
    let idx = reg.find_index(name)?;
    let record = reg.live_at(idx)?;
    let gen = record.gen();
    let shared = match gen.context {
        GucContext::PGC_POSTMASTER | GucContext::PGC_INTERNAL => true,
        GucContext::PGC_SIGHUP => !THREAD_SIGHUP.iter().any(|n| crate::name::guc_name_eq(n, name)),
        _ => false,
    };
    if shared || gen.flags & GUC_CUSTOM_PLACEHOLDER != 0 {
        return None;
    }
    Some((idx, Slot::of(record)?))
}

#[cfg(test)]
pub(crate) fn is_carried(name: &str) -> bool {
    crate::store::with_store(|reg| carried(reg, name).is_some()).unwrap_or(false)
}

fn build(pin: &Arc<GucQuerySnapshot>) -> PgResult<Plan> {
    let vars = pin.session_vars();
    let mut entries = Vec::new();
    entries.try_reserve_exact(vars.len()).map_err(|_| oom())?;
    crate::store::with_store(|reg| {
        for cap in vars.iter().filter(|c| c.source() != PGC_S_DEFAULT) {
            if let Some((idx, slot)) = carried(reg, cap.name()) {
                entries.push(Entry { idx, slot, want: cap.value().clone(), extra: cap.extra().cloned() });
            }
        }
    });
    Ok(Plan { pin: pin.clone(), gen: crate::store::store_generation(), entries, differed: RefCell::new(None) })
}

fn plan_of(pin: &Arc<GucQuerySnapshot>) -> PgResult<Rc<Plan>> {
    let gen = crate::store::store_generation();
    if let Some(plan) = PLAN.with(|p| p.borrow().as_ref().filter(|p| Arc::ptr_eq(&p.pin, pin) && p.gen == gen).cloned()) {
        return Ok(plan);
    }
    let plan = Rc::new(build(pin)?);
    PLAN.with(|p| *p.borrow_mut() = Some(plan.clone()));
    Ok(plan)
}

/// The extra of this thread's value of variable `idx`: an armed claim's, else its store's.
fn extra_now(idx: usize) -> Option<SharedExtra> {
    let applied = APPLIED.with(|a| a.borrow().iter().rev().find(|(i, _)| *i == idx).map(|(_, x)| x.clone()));
    match applied {
        Some(extra) => extra,
        None => crate::store::with_store(|reg| reg.live_at(idx).and_then(|r| r.gen().extra.clone())).flatten(),
    }
}

struct Undo {
    slot: Slot,
    val: config_var_val,
    extra: Option<SharedExtra>,
}

/// A claim's carried GUCs (`arm_claim`): dropped at the claim's end, an unwind too, it puts back each value the claim
/// changed (assign hook, then the backing), innermost first.
#[must_use = "the leader's GUCs stay carried only while the guard lives"]
pub struct ClaimGucs {
    undo: Vec<Undo>,
    mark: usize,
}

impl Drop for ClaimGucs {
    fn drop(&mut self) {
        while let Some(u) = self.undo.pop() {
            u.slot.set(u.val, u.extra.as_ref());
        }
        let _ = APPLIED.try_with(|a| {
            if let Ok(mut a) = a.try_borrow_mut() {
                a.truncate(self.mark);
            }
        });
    }
}

fn apply(i: usize, e: &Entry, guard: &mut ClaimGucs, differed: Option<&mut Vec<u32>>) {
    let have = e.slot.read();
    if have == e.want {
        return;
    }
    let had = extra_now(e.idx);
    e.slot.set(e.want.clone(), e.extra.as_ref());
    // within the capacity arm_claim reserved for every entry
    guard.undo.push(Undo { slot: e.slot, val: have, extra: had });
    APPLIED.with(|a| a.borrow_mut().push((e.idx, e.extra.clone())));
    if let Some(d) = differed {
        d.push(i as u32);
    }
}

/// The leader's session GUCs on this thread until the guard drops (module doc); on the leader's own thread a no-op.
pub fn arm_claim(pin: &Arc<GucQuerySnapshot>) -> PgResult<ClaimGucs> {
    let plan = plan_of(pin)?;
    let mark = APPLIED.with(|a| a.borrow().len());
    let mut undo = Vec::new();
    undo.try_reserve_exact(plan.entries.len()).map_err(|_| oom())?;
    APPLIED.with(|a| a.borrow_mut().try_reserve(plan.entries.len())).map_err(|_| oom())?;
    let mut guard = ClaimGucs { undo, mark };
    // an outermost claim under an unmoved store compares only what differed at the last one
    let count = crate::store::store_mutation_count();
    if mark == 0 {
        let known = plan.differed.borrow();
        if let Some((_, ix)) = known.as_ref().filter(|d| d.0 == count) {
            for &i in ix {
                apply(i as usize, &plan.entries[i as usize], &mut guard, None);
            }
            return Ok(guard);
        }
    }
    if mark != 0 {
        for (i, e) in plan.entries.iter().enumerate() {
            apply(i, e, &mut guard, None);
        }
        return Ok(guard);
    }
    let mut differed = Vec::new();
    differed.try_reserve_exact(plan.entries.len()).map_err(|_| oom())?;
    for (i, e) in plan.entries.iter().enumerate() {
        apply(i, e, &mut guard, Some(&mut differed));
    }
    *plan.differed.borrow_mut() = Some((count, differed));
    Ok(guard)
}

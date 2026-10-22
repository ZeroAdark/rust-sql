//! D3881: the thread's pool of kept executions. A deep statement level is freed at the end of its caller's run
//! (`reentry`), and its filed admissions, with the Nodes parked under their keys and the admission context they live in
//! (`Admitted::keep`), come here, keyed by plan key (never by depth): the next call of that plan at any level whose memo
//! misses the key takes one (`refill`), as every level of a warm recursion does. Bounded by mempolicy
//! (`keep::nested_kept_keeps`, oldest given back first) and charged to its account (`memledger::NESTED_KEPT`); dropped at a
//! transaction's abort, a plan-cache reset, its plansource's drop, a flush and the session's end.

use core::cell::{Cell, RefCell};
use core::mem::ManuallyDrop;

use std::sync::Arc;

use gravity_driver::Node;
use gravity_hosts::ops::scan::index_source::SiteRecipe;
use gravity_hosts::ops::HostRegistry;
use gravity_program::{Handle, PlanKey, Program};
use memledger::NESTED_KEPT;

use super::{ArmRun, InitRun};
use crate::{Admitted, Backend};

/// One kept execution: the Nodes (each with the key it was parked under: the plan key, or the content park key
/// `runner::park_key` gives) drop before the admission they were built over (D2190), its context last.
struct Kept {
    key: PlanKey,
    nodes: Vec<(PlanKey, Box<Node>)>,
    a: Box<Admitted>,
    bytes: u64,
}

thread_local! {
    // ManuallyDrop (KEPT, SHELLS): never dropped by the thread's TLS destructor, after the contexts they reach (a session
    // without its teardown leaves them as it leaves its levels: reentry `LEVELS`)
    static KEPT: RefCell<ManuallyDrop<Vec<Kept>>> = const { RefCell::new(ManuallyDrop::new(Vec::new())) }; // alloc: Exec
}

leader_tls::leader_tls! {
    thread_local! {
        /// Freed deep levels' emptied state with its bytes (`shelve`, `shell`).
        static SHELLS: RefCell<ManuallyDrop<Vec<(Box<Backend>, u64)>>> = const { RefCell::new(ManuallyDrop::new(Vec::new())) }; // alloc: Exec
        static HELD: Cell<u64> = const { Cell::new(0) };
        static XACT: Cell<bool> = const { Cell::new(false) };
        /// An abort since: the levels its statements left are freed without handing anything in (`reentry` linger).
        static ABORTED: Cell<bool> = const { Cell::new(false) };
        /// The session's most bytes held, executions kept and taken (its end's report line).
        static PEAK: Cell<u64> = const { Cell::new(0) };
        /// The flush generation (`reentry::flush_gen`) every kept execution was kept under: a flush since drops them all.
        static GEN: Cell<u64> = const { Cell::new(0) };
        static PUTS: Cell<u64> = const { Cell::new(0) };
        static TAKES: Cell<u64> = const { Cell::new(0) };
    }
}

/// Kept executions leaving the pool unused: their bytes off the account, then dropped.
fn give_back(gone: Vec<Kept>) {
    for x in gone.iter() {
        discharge(x.bytes);
    }
    drop(gone);
}

/// The pool after a flush (`reentry::flush_later`): every kept execution was lowered before it, so all go.
#[inline]
fn follow_flush() {
    let now = super::reentry::flush_gen();
    if GEN.replace(now) != now {
        clear();
    }
}

/// A freed deep level's filed admissions and their parked Nodes into the pool (the level is idle: none is lent).
pub(crate) fn keep_level(b: &mut Backend) {
    if ABORTED.get() {
        return;
    }
    let n = b.plans.iter().filter(|s| s.is_some()).count();
    if n == 0 {
        return;
    }
    follow_flush();
    let root = b.admit_root();
    let share = root.used() as u64 / n as u64;
    let mut gone = Vec::new(); // alloc: Exec
    for i in 0..b.plans.len() {
        let Some(s) = b.plans[i].take() else { continue };
        let (key, mut a) = (s.key, s.served);
        let nodes = parked(b, key, &a);
        // an admission the pool gave this level keeps the context it was placed in
        let own = a.keep.is_none();
        if own {
            a.keep = Some(root.clone());
        }
        let bytes = if own { share } else { 0 } + own_bytes(&a, &nodes);
        let x = Kept { key, nodes, a, bytes };
        if room(bytes, &mut gone) {
            charge(bytes);
            PUTS.set(PUTS.get() + 1);
            KEPT.with(|k| k.borrow_mut().push(x)); // alloc: Exec
        } else {
            gone.push(Kept { bytes: 0, ..x }); // alloc: Exec
        }
    }
    drop(gone);
    if !XACT.replace(true) {
        xact::RegisterXactCallback(AT_ABORT, Default::default());
    }
}

/// Room in the pool for `bytes` more (mempolicy's answer), the oldest kept executions given back into `gone` first, then
/// the newest shells.
fn room(bytes: u64, gone: &mut Vec<Kept>) -> bool {
    while !mempolicy::keep::nested_kept_keeps(HELD.get(), bytes) {
        if let Some(old) = KEPT.with(|k| { let mut k = k.borrow_mut(); (!k.is_empty()).then(|| k.remove(0)) }) {
            discharge(old.bytes);
            gone.push(old); // alloc: Exec
        } else if let Some((shell, b)) = SHELLS.with(|s| s.borrow_mut().pop()) {
            discharge(b);
            drop(shell);
        } else {
            return false;
        }
    }
    true
}

fn charge(bytes: u64) {
    NESTED_KEPT.charge(bytes);
    HELD.set(HELD.get() + bytes);
    PEAK.set(PEAK.get().max(HELD.get()));
}

fn discharge(bytes: u64) {
    NESTED_KEPT.discharge(bytes);
    HELD.set(HELD.get().saturating_sub(bytes));
}

/// A freed deep level's state, emptied (`Backend::empty_deep`), for the next deep level built (`shell`), when the pool
/// has room for it; it holds nothing of a transaction (none after an abort, until its levels are freed).
pub(crate) fn shelve(mut b: Box<Backend>) {
    if ABORTED.get() {
        return;
    }
    b.empty_deep();
    let bytes = b.shell_bytes();
    let mut gone = Vec::new(); // alloc: Exec
    if mempolicy::keep::nested_kept_keeps(HELD.get(), bytes) || room(bytes, &mut gone) {
        charge(bytes);
        SHELLS.with(|s| s.borrow_mut().push((b, bytes))); // alloc: Exec
    }
    drop(gone);
}

/// An emptied deep level's state for the deep level built now, the newest shelved.
#[inline]
pub(crate) fn shell() -> Option<Box<Backend>> {
    let (b, bytes) = SHELLS.with(|s| s.borrow_mut().pop())?;
    discharge(bytes);
    Some(b)
}

/// A kept execution's own bytes, an estimate: its admission, its bound words and host recipes (each a site's size), its
/// InitPlan and Append arm runs, each program no level's filings or cache holds beside it, its parked Nodes. The regions it
/// holds (`GraphWords::sets`) are charged to the grant pool.
fn own_bytes(a: &Admitted, nodes: &[(PlanKey, Box<Node>)]) -> u64 {
    use core::mem::{size_of, size_of_val};
    fn program(p: &Arc<Program>) -> usize {
        if Arc::strong_count(p) == 1 { p.code_bytes() } else { 0 }
    }
    fn recipes(r: &HostRegistry) -> usize {
        r.len() * size_of::<SiteRecipe>()
    }
    let mut n = size_of::<Admitted>() + program(&a.program) + recipes(&a.registry);
    if let Some(g) = &a.graph {
        // the merge keys and guards are the template's (its root, on its owner), not the admission's
        n += size_of_val(&*g.words) + size_of_val(&*g.folded);
        n += g.inits.iter().map(|i| size_of::<InitRun>() + program(&i.program) + recipes(&i.registry)).sum::<usize>();
        n += g.arms.iter().map(|x| size_of::<ArmRun>() + program(&x.program) + recipes(&x.registry)).sum::<usize>();
    }
    (n + nodes.iter().map(|(_, x)| x.bytes() + size_of::<Node>()).sum::<usize>()) as u64
}

/// The Nodes parked at the level for admission `a` of plan key `key`: under the plan key, and under its content park key.
fn parked(b: &mut Backend, key: PlanKey, a: &Admitted) -> Vec<(PlanKey, Box<Node>)> {
    let park = b.entry.park();
    let mut out = Vec::new(); // alloc: Exec
    park.take_parked_into(key, &mut out);
    if let Some(c) = crate::runner::park_key_if(a, |_| false).filter(|c| *c != key) {
        park.take_parked_into(c, &mut out);
    }
    out
}

/// The level's memo misses `key`: the pool's most recent execution of it, its Nodes parked at the level and its admission
/// filed there, so the memo's take serves it. False when the pool holds none.
#[inline]
pub(crate) fn refill(b: &mut Backend, key: PlanKey) -> bool {
    if KEPT.with(|k| k.borrow().is_empty()) {
        return false;
    }
    refill_from(b, key)
}

#[inline(never)]
fn refill_from(b: &mut Backend, key: PlanKey) -> bool {
    follow_flush();
    let hit = KEPT.with(|k| {
        let mut k = k.borrow_mut();
        let at = k.iter().rposition(|x| x.key == key)?;
        Some(k.remove(at))
    });
    let Some(x) = hit else { return false };
    TAKES.set(TAKES.get() + 1);
    discharge(x.bytes);
    let Kept { nodes, a, .. } = x;
    // parked first: an admission the level does not file (`remember_served`: a support change since) drops the Nodes
    // parked under its key before itself
    for (k, n) in nodes {
        b.entry.park().put_parked(k, n);
    }
    b.remember_served(key, a).is_some()
}

/// Every kept execution of plansource `h` dropped (its plan is gone).
pub(crate) fn drop_for(h: Handle) {
    if KEPT.with(|k| k.borrow().is_empty()) {
        return;
    }
    let gone = KEPT.with(|k| {
        let mut k = k.borrow_mut();
        let (gone, live): (Vec<Kept>, Vec<Kept>) = core::mem::take(&mut **k).into_iter().partition(|x| x.key.handle == h); // alloc: Exec
        **k = live;
        gone
    });
    give_back(gone);
}

/// Every kept execution dropped (an abort, a plan-cache reset, the session's end).
pub(crate) fn clear() {
    let gone: Vec<Kept> = KEPT.try_with(|k| k.try_borrow_mut().map(|mut k| core::mem::take(&mut **k)).unwrap_or_default()).unwrap_or_default();
    give_back(gone);
}

/// The session's end: every kept execution and shell dropped, and (under the report knob) the session's use of the pool.
pub(crate) fn session_end() {
    let (peak, puts, takes) = (PEAK.replace(0), PUTS.replace(0), TAKES.replace(0));
    if puts > 0 && crate::inputs::knobs::seam::report_on() {
        let _ = elog::ereport(types_error::LOG).errmsg(format!("gravity: kept executions at session end: peak {peak} bytes, kept {puts}, taken {takes}")).errhidestmt(true).errhidecontext(true).finish(types_error::ErrorLocation::new("gravity", 0, "kept")); // alloc: Cold
    }
    clear();
    let shells: Vec<(Box<Backend>, u64)> = SHELLS.try_with(|s| s.try_borrow_mut().map(|mut s| core::mem::take(&mut **s)).unwrap_or_default()).unwrap_or_default();
    shells.iter().for_each(|(_, b)| discharge(*b));
    drop(shells);
}

/// The levels an abort left are freed (`reentry` linger): the pool takes again.
pub(crate) fn abort_settled() {
    ABORTED.set(false);
}

/// The bytes the pool holds now.
pub fn held() -> u64 {
    HELD.get()
}

/// The executions the pool holds now.
pub fn len() -> usize {
    KEPT.with(|k| k.borrow().len())
}

const AT_ABORT: xact::XactCallback = |event, _| {
    use types_core::xact::XactEvent::*;
    if matches!(event, XACT_EVENT_ABORT | XACT_EVENT_PARALLEL_ABORT) {
        clear();
        ABORTED.set(true);
        super::reentry::linger();
    }
    Ok(())
};

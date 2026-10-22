//! W9 PERF D1559 S2: a nested statement's served shell kept between its executions (a SQL function body's generic plan,
//! run once per call). S1 keys the body's QueryDesc by its CachedPlan, so each call takes the memo's filed program; S2 also
//! keeps the `Serve` built around it (its context, result descriptor, adapter and key words), shed at the free as a parked
//! portal's serve is (`Serve::shed`), so the next call skips `Serve::new`, the context's creation and the Serve's drop.
//!
//! The shelf never outlives the memo's verdict: a shelved serve holds the admission the memo lent (its slot stays taken, as
//! while any execution runs it), and it is revived only when the memo at the same re-entrancy level still names that slot
//! for the key and the slot is still empty. An invalidation (CREATE OR REPLACE, a replan, ALTER, a cache reset) drops or
//! re-files the key's verdict and a re-admission fills a slot, so the stale serve is dropped at the next look. The per-call
//! checks of a memo take run on every revival: the permission check (a REVOKE between calls), the grant, the read-only
//! guard, the relations held under this call's params; the keys and InitPlans are evaluated again at the open. A body's
//! recursion finds the shelf empty while the outer call runs (its serve is out) and admits its own, as a memo take does.
//! D1369: a statement started while another one's ExecutorRun / ExecutorFinish runs (a SQL comparator body the row
//! executor's sort calls per comparison) is in the shelf's scope too (entry `in_statement`), not only an SPI or served nesting.
//! A run that failed, rewound or finished with an error is never shelved; one its caller cut short (D3865 L2: fmgr_sql's
//! one row, SELECT INTO, a lazy SQL function's early end) is ended at ExecutorEnd and shelved as a finished one. A write,
//! a row-locking read (an RI check's FOR KEY SHARE) and a crosscheck run are shelved too: the revival reads the output
//! command id again (`Serve::recid`), and the crosscheck snapshot is the run's. D3865 L5: so is a CURRENT-OF-eligible
//! cursor's identity serve (a PL/pgSQL FOR-IN's or OPEN's portal), under the plan's identity key: its admission holds the
//! plan copy it runs (`Admitted::ident`), and the shed leaves it no last row (`Serve::shed`).

use core::cell::{Cell, RefCell};
use core::mem::ManuallyDrop;

use crate::{Backend, Verdict};
use gravity_program::PlanKey;
use types_error::PgResult;
use types_nodes::plannodes::PlannedStmt;
use types_portal::CachedPlanHandle;

use crate::admit::entry::run::Serve;

/// Shelved serves per session: a nested body cycling more keys than this re-builds the oldest (bounded memory: each is a
/// shed serve, its context empty).
const SHELF_MAX: usize = gravity_program::cost::entry::SHELVED_SERVES_MAX;

struct Shelved {
    key: PlanKey,
    /// The memo slot the admission was lent from, at the level `home`.
    slot: u16,
    home: usize,
    /// The grant pool's depot trims at the shelving (as a parked portal's, lib.rs `parked_trims`): a trim since (memory
    /// pressure) makes the revival decline, and the serve goes back to the memo as any execution's.
    trims: u64,
    serve: Box<Serve>,
}

// ManuallyDrop (as DESCS): a serve's drop reaches contexts; the session's Portals cleanup phase drains the shelf.
leader_tls::leader_tls! {
    thread_local! {
        static SHELF: RefCell<ManuallyDrop<Vec<Shelved>>> = const { RefCell::new(ManuallyDrop::new(Vec::new())) }; // alloc: Cold
        static DRAIN: Cell<bool> = const { Cell::new(false) };
        /// The transaction-end drain's callback is registered (once per backend thread, as exec `after_query`'s D832 one).
        static XACT: Cell<bool> = const { Cell::new(false) };
    }
}

/// The memo at this level still lends `slot` for `key`: its verdict names the slot and nothing re-filled it.
fn lent(b: &mut Backend, key: PlanKey, slot: u16) -> bool {
    b.lookup(key) == Some(Verdict::Served(slot)) && b.lent(slot as usize, key)
}

/// The revival at ExecutorStart (inside `reentry::enter` at `depth`): the shelved serve of this plan's key when the memo
/// still lends its slot and the permission check passes; a stale one comes back in `stale`, dropped by the caller outside
/// the level's borrow.
pub(crate) struct Taken {
    pub hit: Option<Box<Serve>>,
    pub stale: Option<Box<Serve>>,
}

/// `ident` (D3865 L5): the identity serve, shelved under the plan's identity key (`pstmt`: the desc's own plan).
pub(crate) fn take(b: &mut Backend, depth: usize, cplan: CachedPlanHandle, pstmt: &PlannedStmt<'_>, params: types_portal::ParamListHandle, ident: bool) -> PgResult<Taken> {
    let none = Taken { hit: None, stale: None };
    let Some(key) = crate::runner::entry_key_as(cplan, pstmt, ident) else { return Ok(none) };
    let got = SHELF.with(|s| {
        let mut s = s.borrow_mut();
        let i = s.iter().position(|x| x.key == key && x.home == depth)?;
        Some(s.remove(i))
    });
    let Some(x) = got else { return Ok(none) };
    // D2663: a support change since the admission drops the level's verdicts, and a re-admission may fill and lend the
    // same slot again: the serve's own admission decides, not the slot
    if !lent(b, key, x.slot) || x.serve.key != Some(key) || Backend::serve_stale(&x.serve.a) || !crate::admit::folded_hold(&x.serve.a, params) {
        return Ok(Taken { hit: None, stale: Some(stale(b, key, x.serve)) });
    }
    if x.trims != crate::admit::entry::pool_trims() {
        // the grant pool trimmed since the shelving (memory pressure): the shell goes, as a parked portal's (its
        // admission with it: the key's next execution admits afresh and is shelved or given back at its free)
        return Ok(Taken { hit: None, stale: Some(stale(b, key, x.serve)) });
    }
    b.note_kept_serve();
    Ok(Taken { hit: Some(x.serve), stale: None })
}

/// D3346: a stale shelved serve leaves its level (`depth` == its home): the Nodes parked under its key here (and under the
/// serve's own key, when another) go first, since one may be built over its admission (D2190); the caller drops the serve.
fn stale(b: &mut Backend, key: PlanKey, serve: Box<Serve>) -> Box<Serve> {
    b.entry.park().drop_for(key.handle);
    if let Some(k) = serve.key.filter(|k| *k != key) {
        b.entry.park().drop_for(k.handle);
    }
    serve
}

/// D3346: shelved serves dropped (the statement's, the transaction's or the session's end): each shell goes, then its
/// admission retires at its home level after the Nodes parked under its key there (`reentry::retire`, D2190's rule).
fn retire_all(all: Vec<Shelved>) {
    for x in all {
        let (key, home, serve) = (x.key, x.home, x.serve);
        let own = serve.key.filter(|k| *k != key);
        let a = serve.a;
        if let Some(k) = own {
            crate::admit::reentry::purge(home, k.handle);
        }
        crate::admit::reentry::retire(home, key, a);
    }
}

/// The shelving at FreeQueryDesc of a shed, retainable nested serve: `None` once shelved; else the serve back to the
/// caller, which releases its admission to the memo as before (not at its level, the memo re-filed or displaced the key,
/// or the shelf already holds the key: a recursion's second serve). An evicted serve is dropped outside the borrow.
/// D3865 L5: an identity serve under the plan's identity key (`pstmt`: the desc's own plan).
pub(crate) fn put(cplan: CachedPlanHandle, pstmt: &PlannedStmt<'_>, serve: Box<Serve>) -> Option<Box<Serve>> {
    let home = serve.home;
    let ident = serve.key.is_some_and(|k| k.is_identity());
    let mut back = Some(serve);
    let mut evicted = None;
    let _ = crate::admit::reentry::enter(None, None, false, |b, depth| {
        let Some(key) = crate::runner::entry_key_as(cplan, pstmt, ident) else { return };
        if depth != home || back.as_ref().is_none_or(|s| s.key != Some(key)) {
            return;
        }
        let Some(Verdict::Served(slot)) = b.lookup(key) else { return };
        if !lent(b, key, slot) || SHELF.with(|s| s.borrow().iter().any(|x| x.key == key && x.home == home)) {
            return;
        }
        let serve = back.take().expect("checked");
        SHELF.with(|s| {
            let mut s = s.borrow_mut();
            if s.len() >= SHELF_MAX {
                evicted = Some(s.remove(0));
            }
            s.push(Shelved { key, slot, home, trims: crate::admit::entry::pool_trims(), serve }); // alloc: Exec (at most SHELF_MAX)
        });
    });
    if back.is_none() && !DRAIN.replace(true) {
        mcx::register_session_cleanup_phase(mcx::SessionCleanupPhase::Portals, Box::new(drain)); // alloc: Cold
    }
    if back.is_none() && !XACT.replace(true) {
        xact::RegisterXactCallback(AT_XACT_END, Default::default());
    }
    // the oldest shelved serve's admission back to its memo slot when the slot is still lent at its level (else the
    // memo re-filed or dropped the key and the admission goes too), so the key's next execution takes the memo's program
    // instead of admitting afresh; the shed shell is dropped outside every borrow
    if let Some(x) = evicted {
        let (key, slot, home, serve) = (x.key, x.slot, x.home, x.serve);
        let mut a = Some(serve.a);
        let _ = crate::admit::reentry::enter(None, None, false, |b, depth| {
            if depth == home && lent(b, key, slot) {
                b.file_back(key, a.take().expect("once"));
            }
        });
        // D3346: not filed back: retired at its home level after the Nodes parked under its key
        if let Some(a) = a {
            crate::admit::reentry::retire(home, key, a);
        }
    }
    back
}

/// The end of a top-level statement the entry frees (lib.rs `free_query_desc` / `release_query_desc` outside any
/// nesting) and every transaction's end (`AT_XACT_END`: a top-level statement exec's portal runner serves never reaches
/// the entry's free): every shelved serve is dropped, so a statement leaves no entry context behind (m5_spi_memory); a
/// kept serve lives only while the statement whose nested calls reuse it runs, or its transaction block. The dropped
/// admissions' memo slots stay lent to their keys: each key's next execution admits once and gives its plan back into its
/// own slot (exec `Backend::file_back`).
pub(crate) fn end_of_statement() {
    if SHELF.with(|s| s.borrow().is_empty()) {
        return;
    }
    let all: Vec<Shelved> = SHELF.with(|s| core::mem::take(&mut **s.borrow_mut()));
    retire_all(all);
}

/// D3881: the serves shelved at a level `depth` or deeper leave the shelf: those levels are freed at the end of their
/// caller's run (reentry `Above`), so each admission goes back into the memo slot it was lent from (the level hands it to
/// the thread's pool of kept executions), or retires there when the slot is no longer its.
pub(crate) fn retire_from(depth: usize) {
    let gone: Vec<Shelved> = SHELF.with(|s| {
        let mut s = s.borrow_mut();
        if !s.iter().any(|x| x.home >= depth) {
            return Vec::new(); // alloc: Exec
        }
        let (gone, kept): (Vec<Shelved>, Vec<Shelved>) = core::mem::take(&mut **s).into_iter().partition(|x| x.home >= depth); // alloc: Exec
        **s = kept;
        gone
    });
    for x in gone {
        let (key, slot, home, serve) = (x.key, x.slot, x.home, x.serve);
        if let Some(k) = serve.key.filter(|k| *k != key) {
            crate::admit::reentry::purge(home, k.handle);
        }
        let mut a = Some(serve.a);
        let _ = crate::admit::reentry::at_idle(home, |b| {
            if lent(b, key, slot) {
                b.file_back(key, a.take().expect("once"));
            }
        });
        if let Some(a) = a {
            crate::admit::reentry::retire(home, key, a);
        }
    }
}

/// The transaction's end (COMMIT / ABORT / PREPARE, in their callbacks while every context lives): `end_of_statement`.
const AT_XACT_END: xact::XactCallback = |event, _| {
    use types_core::xact::XactEvent::*;
    if matches!(event, XACT_EVENT_COMMIT | XACT_EVENT_PARALLEL_COMMIT | XACT_EVENT_ABORT | XACT_EVENT_PARALLEL_ABORT | XACT_EVENT_PREPARE) {
        end_of_statement();
    }
    Ok(())
};

/// The session's end: the shelved serves dropped while every context is alive (their admissions go with them; the
/// memo's own table is dropped after).
fn drain() {
    DRAIN.set(false);
    let all: Vec<Shelved> = SHELF.with(|s| core::mem::take(&mut **s.borrow_mut()));
    retire_all(all);
}

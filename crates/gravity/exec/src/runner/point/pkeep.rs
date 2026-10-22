//! pgrust.point_portal_keep: an S1x portal's bound probe kept across the portals of one generic plan. S1x (portal.rs
//! `build_probe`) built at every Bind what ExecInitIndexScan builds (the table and the index opened, S1's tests of
//! them, the scan keys with their functions, index_beginscan and the table's slot), the out columns and the result
//! descriptor, in a context of the portal's, and ended and freed it all at PortalDrop: about 4.4 % of an RO
//! transaction's server cycles (RO July t32 on the Hearth base: the runner's point bind 3.90, PortalDrop's end 1.06).
//! Kept here per plan (`glue::plan_key`: the plan source and its generation, so a replan keys anew) and the plan's root
//! (the address of the IndexScan the plan cache holds), under what the build read (`gravity_deps::Held`): the parked
//! scan (index_parkscan: no pin, relation or snapshot held), the parked relation handles, the tests' verdict (the keys'
//! strategies and right-hand types), the out columns and the descriptor. The portal that takes them gives them back at
//! its PortalDrop (`put`); one portal at a time holds them (the live point portal is the unnamed one).
//! Per Bind, as ExecutorStart does for every portal, nothing kept (`Probe::arm`): the read-only guard and the permission
//! checks, the snapshot registered on the portal's owner (portal.rs), the index lock where the transaction holds none, the
//! relations re-opened (through the parked handles while their relcache entries are valid, `rel::repin`), the parameters'
//! types against the plan's (`probe::params_fit`), the keys' EXECUTE gates, the scan re-armed with this portal's
//! relations, snapshot and key values, and the AM's rescan.
//! Dropped: a kept probe whose reads moved (at its next take), one whose plan was replanned or dropped (its key is never
//! asked again: least recently used out past `ENTRIES`), at a failed Execute (an Execute that raised; the portal's drop
//! ends the scan), at a drop outside a live transaction (AtAbort_Portals / AtSubAbort_Portals end it), at the idle
//! release, at a memory-short epoch move (REACH) and at the session's teardown (a reused thread included).
//! Memory: about 4 KB a kept probe (its scan's btree state and slot, its keys, the out columns, its contexts' first
//! blocks: 4,096 bytes for sysbench's and pgbench -S's point SELECTs), at most `ENTRIES` a backend, charged to the point cache holder (`skip::MEMO_OWNER`) and counted in the point
//! memo's byte cap (`mempolicy::cache::POINT_MEMO_BYTES`, skip.rs `evict`).
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use gravity_hosts::ops::shared::rel::{park, repin};
use gravity_hosts::NodeScratch;
use gravity_program::{Cmd, OutCol, PlanKey};
use mcx::{McxOwned, PgBox, PgVec};
use types_core::Oid;
use types_error::PgResult;
use types_nodes::list::NodeList;
use types_nodes::plannodes::PlannedStmt;
use types_portal::params::ParamExternData;
use types_rel::{NoLock, Relation, RelationData};
use types_snapshot::SnapshotData;
use types_tuple::TupleDescData;

use super::out::out_col;
use super::select::{self, Shape};
use super::{probe, report, reporting, skip};

/// The kept probes a backend holds at most: an application's prepared point statements (sysbench's oltp_read_only has
/// one, pgbench -S one, an ORM's tens); past it the least recently used one goes.
pub(super) const ENTRIES: usize = 32;

leader_tls::leader_tls! {
    thread_local! {
        /// The session's kept probes, by plan: made at the first portal of a plan whose probe the drop keeps, dropped at
        /// the idle release, a memory-short epoch move and the session's teardown.
        static KEEP: RefCell<Keep> = const { RefCell::new(Keep::EMPTY) };
        /// The kept probes' bytes (`Slot::bytes` summed), for the point memo's byte cap (skip.rs `evict`).
        static BYTES: Cell<usize> = const { Cell::new(0) };
    }
}

/// The slots, least recently used first out past `ENTRIES`.
struct Keep {
    slots: [Option<Slot>; ENTRIES],
    tick: u64,
    /// REACH: `mcx::memory_short_epoch` when the first slot was filled; every slot goes at the first take that reads another.
    short: u64,
    /// `l2cache::committed_moves` at the last `sweep`, and the earliest next one (`pg_clock::mono_ns`).
    swept: u64,
    sweep_at: u64,
}

/// At most one `sweep` a second.
const SWEEP_EVERY_NS: u64 = 1_000_000_000;

impl Keep {
    const EMPTY: Keep = Keep { slots: [const { None }; ENTRIES], tick: 0, short: 0, swept: 0, sweep_at: 0 };

    /// skip.rs `sweep`'s for the kept probes (Memory Accounting's DDL check, D4550): when a committed catalog generation moved
    /// since the last sweep and at most once a second, the slots whose reads can never stand again in this database go (a
    /// dropped or rewritten table's, a changed catalog row's): their plans would be replanned anyway, and a dropped table's
    /// probe is never taken again. Returns the slots and bytes dropped.
    fn sweep(&mut self) -> (usize, usize) {
        let moves = l2cache::committed_moves();
        if self.swept == moves {
            return (0, 0);
        }
        let now = pg_clock::mono_ns();
        if now < self.sweep_at {
            return (0, 0);
        }
        self.swept = moves;
        self.sweep_at = now.saturating_add(SWEEP_EVERY_NS);
        let db = init_small::globals::MyDatabaseId();
        let (mut n, mut b) = (0, 0);
        for s in self.slots.iter_mut() {
            if s.as_ref().is_some_and(|slot| slot.held.deps().superseded_in(db)) {
                let slot = s.take().expect("checked above");
                BYTES.with(|x| x.set(x.get().saturating_sub(slot.bytes)));
                n += 1;
                b += slot.bytes;
            }
        }
        (n, b)
    }

    fn clear(&mut self) -> (usize, usize) {
        let mut n = 0;
        for s in self.slots.iter_mut() {
            if s.take().is_some() {
                n += 1;
            }
        }
        (n, BYTES.with(|b| b.replace(0)))
    }
}

/// One plan's kept probe: its key, what its build read, and the pieces.
struct Slot {
    key: PlanKey,
    root: usize,
    tick: u64,
    held: gravity_deps::Held,
    kept: KeptBox,
    bytes: usize,
}

mcx::bind!(KeptTy => KeptIn<'k>);

/// A kept probe in its own small context, charged to the point cache holder: the slot holds two pointers, a portal takes
/// and returns them without copying the probe.
struct KeptIn<'k> {
    probe: PgBox<'k, Probe>,
}

type KeptBox = McxOwned<KeptTy>;

fn boxed(p: Probe) -> PgResult<KeptBox> {
    let ctx = mcx::MemoryContext::new_start_small("GravityPointPortalKept");
    ctx.set_owner(skip::MEMO_OWNER);
    McxOwned::<KeptTy>::try_new(ctx, move |m| {
        let probe = PgBox::try_new_in(p, m).map_err(|_| m.oom_boxed(core::mem::size_of::<Probe>()))?;
        Ok(KeptIn { probe })
    })
}

/// A kept probe's bytes: its own context's footprint and its scratch roots' (keep.rs `Kept::bytes`' measure).
fn bytes_of(kb: &KeptBox) -> usize {
    kb.context().mem_allocated() + kb.with(|k| k.probe.scratch.used_parts().iter().sum::<usize>())
}

/// The kept probe of S1x's shape: its table and index, the keys' strategies and right-hand types (`probe::index_refusal`'s
/// verdict), the parked relation handles, the out columns, the result descriptor (shared with each portal that takes the
/// probe, as `set_portal_desc` shares a served plan's) and the scan, parked between portals. The relation tests passed
/// when it was made.
pub(super) struct Probe {
    relid: Oid,
    indexid: Oid,
    ops: probe::KeyOps,
    heap: Option<Weak<RelationData<'static>>>,
    index: Option<Weak<RelationData<'static>>>,
    pub(super) cols: PgVec<'static, OutCol>,
    pub(super) desc: Rc<TupleDescData<'static>>,
    pub(super) scan: Option<probe::Scan<'static>>,
    /// Declared last: everything above lives in its contexts (node: the build and the scan; exec: the portal's fetches:
    /// portal.rs reads them through its `exec_mcx`, reset at the drop), and drops first.
    pub(super) scratch: NodeScratch,
}

/// A kept probe while its portal lives (portal.rs `State::Kept`): the pieces, the relations the Bind opened (closed or
/// parked at the drop) and what to file them under at the drop.
pub(super) struct Lent {
    key: PlanKey,
    root: usize,
    held: gravity_deps::Held,
    kept: KeptBox,
    heap: Option<Relation<'static>>,
    index: Option<Relation<'static>>,
    /// An Execute raised: the drop ends the scan and keeps nothing.
    pub(super) failed: bool,
}

/// pgrust.point_portal_keep, the session's value.
pub(super) fn on() -> bool {
    crate::inputs::knobs::seam::point_portal_keep()
}

/// The root of `pstmt`'s plan as the key's second half: the IndexScan the plan cache holds for the plan.
pub(super) fn root_of(pstmt: &PlannedStmt<'_>) -> usize {
    pstmt.planTree.map_or(0, |n| n.as_raw().as_ptr() as usize)
}

/// The Bind of an S1x portal over plan `key` / `root` under the active snapshot (portal.rs `open_kept` pushed it, registered
/// on the portal's owner): ExecutorStart's checks, then the plan's kept probe re-armed (`Probe::arm`), else one made
/// (`Probe::make`, what it read held). `Ok(Err(why))`: S1's tests or the parameters decline the portal; nothing is served
/// (a kept probe goes back to its slot). An error drops a taken probe (it may be half re-armed).
pub(super) fn bind(pstmt: &'static PlannedStmt<'static>, s: &Shape<'static>, key: PlanKey, root: usize, params: &[ParamExternData], mcx: mcx::Mcx<'_>) -> PgResult<Result<Lent, &'static str>> {
    super::super::start_checks(pstmt, Cmd::Select, mcx)?;
    let snap = snapmgr::GetActiveSnapshot();
    if let Some((held, mut kept, tick)) = take(key, root) {
        // the checks' errors and declines leave the probe kept (nothing of it was touched); a re-arm's error drops it
        match kept.with(|k| k.probe.check(s, params)) {
            Ok(None) => {}
            Ok(Some(why)) => {
                put_slot(key, root, held, kept, Some(tick));
                return Ok(Err(why));
            }
            Err(e) => {
                put_slot(key, root, held, kept, Some(tick));
                return Err(e);
            }
        }
        let (heap, index) = kept.with_mut(|k| k.probe.rearm(s, params, snap))?;
        if reporting() {
            report("portal keep used");
        }
        return Ok(Ok(Lent { key, root, held, kept, heap: Some(heap), index: Some(index), failed: false }));
    }
    let (made, held) = gravity_deps::Held::fill(|| Probe::make(pstmt, s, params, snap));
    let (probe, heap, index) = match made? {
        Ok(m) => m,
        Err(why) => return Ok(Err(why)),
    };
    if reporting() {
        report("portal keep made");
    }
    let kept = boxed(probe)?;
    Ok(Ok(Lent { key, root, held, kept, heap: Some(heap), index: Some(index), failed: false }))
}

impl Probe {
    /// S1x's build for a kept probe (portal.rs `build_probe`'s order after the checks): the table opened (the plan cache's
    /// AcquireExecutorLocks holds its lock) and the index (`select::index_lockmode`), S1's tests under the portal's
    /// parameters (`Err` with the reason: nothing kept), the keys' EXECUTE gates, the out columns and the descriptor, then
    /// the scan begun under the portal's snapshot with its keys given to the AM. The pieces live in their own contexts,
    /// charged to the point cache holder; the relations stay open for the portal (`Lent`).
    #[allow(clippy::type_complexity)]
    fn make(pstmt: &'static PlannedStmt<'static>, s: &Shape<'static>, params: &[ParamExternData], snap: Rc<SnapshotData<'static>>) -> PgResult<Result<(Probe, Relation<'static>, Relation<'static>), &'static str>> {
        let mut scratch = {
            let _small = gravity_hosts::scratch::small_roots();
            NodeScratch::new("GravityPointPortalProbe")
        };
        scratch.home(skip::MEMO_OWNER);
        let m = scratch.mcx();
        let heap = table::table_open(m, s.relid, NoLock)?;
        let index = match indexam::index_open(m, s.scan.indexid, select::index_lockmode(s)) {
            Ok(i) => i,
            Err(e) => {
                table::table_close(heap, NoLock)?;
                return Err(e);
            }
        };
        let mut ops: probe::KeyOps = [(0, 0); types_core::fmgr::INDEX_MAX_KEYS as usize];
        let built = (|| -> PgResult<Result<(PgVec<'static, OutCol>, Rc<TupleDescData<'static>>, probe::Scan<'static>), &'static str>> {
            if let Some(why) = select::relation_refusal(&heap, &index, s, params, &mut ops)? {
                return Ok(Err(why));
            }
            probe::key_gates(&s.scan.indexqual)?;
            let tlist: &NodeList<'_> = &s.scan.scan.plan.targetlist;
            let n = tlist.len();
            let mut cols: PgVec<'static, OutCol> = PgVec::new_in(m);
            cols.try_reserve_exact(n).map_err(|_| m.oom(n * core::mem::size_of::<OutCol>()))?;
            for t in tlist.iter() {
                cols.push(out_col(t.as_target_entry().ok_or_else(|| super::keep::internal("an out entry that is no target entry"))?)?);
            }
            let desc = execmain_seams::exec_clean_type_from_tl::call(pstmt)?;
            let mut scan = probe::Scan::begin(m, &heap, &index, &s.scan.indexqual, params, &ops, snap)?;
            if let Err(e) = scan.rescan() {
                let _ = scan.end(m);
                return Err(e);
            }
            Ok(Ok((cols, desc, scan)))
        })();
        let (cols, desc, scan) = match built {
            Ok(Ok(b)) => b,
            other => {
                let closed = indexam::index_close(index, NoLock).and_then(|()| table::table_close(heap, NoLock));
                let r = other?;
                closed?;
                return Ok(Err(r.err().expect("the Ok arm returned above")));
            }
        };
        let probe = Probe { relid: s.relid, indexid: s.scan.indexid, ops, heap: None, index: None, cols, desc, scan: Some(scan), scratch };
        Ok(Ok((probe, heap, index)))
    }

    /// A taken probe's per-portal checks, its build's order (nothing of the probe touched): the index lock where the
    /// transaction holds none (`select::index_lockmode`, as `build_probe`'s index_open takes it), the portal's parameters
    /// against the plan's types (`Some("parameter")`: `index_refusal`'s), the keys' EXECUTE gates.
    fn check(&self, s: &Shape<'static>, params: &[ParamExternData]) -> PgResult<Option<&'static str>> {
        if self.relid != s.relid || self.indexid != s.scan.indexid {
            return Err(super::keep::internal("a kept portal probe of another table or index"));
        }
        let mode = select::index_lockmode(s);
        if mode != NoLock {
            lmgr_seams::lock_relation_oid::call(self.indexid, mode)?;
        }
        if !probe::params_fit(&s.scan.indexqual, params) {
            return Ok(Some("parameter"));
        }
        probe::key_gates(&s.scan.indexqual)?;
        Ok(None)
    }

    /// After `check`: the relations re-opened through the parked handles (or opened: an entry no longer valid, or gone),
    /// then the scan re-armed with the portal's relations, snapshot and keys (made again when the table's row type is
    /// another descriptor) and the AM's rescan. Returns the relations, open for the portal. On an error the scan is ended
    /// and the relations closed; the caller drops the probe.
    fn rearm(&mut self, s: &Shape<'static>, params: &[ParamExternData], snap: Rc<SnapshotData<'static>>) -> PgResult<(Relation<'static>, Relation<'static>)> {
        let (m, em) = (self.scratch.mcx(), self.scratch.exec_mcx());
        let heap = match repin(&self.heap) {
            Some(h) => h,
            None => table::table_open(em, self.relid, NoLock)?,
        };
        let index = match repin(&self.index) {
            Some(i) => i,
            None => match indexam::index_open(em, self.indexid, NoLock) {
                Ok(i) => i,
                Err(e) => {
                    table::table_close(heap, NoLock)?;
                    return Err(e);
                }
            },
        };
        self.heap = None;
        self.index = None;
        let armed = (|| -> PgResult<()> {
            match self.scan.as_mut().filter(|scan| scan.fits(&heap)) {
                Some(scan) => scan.rearm(&heap, &index, &s.scan.indexqual, params, snap)?,
                None => {
                    if let Some(old) = self.scan.take() {
                        old.end(em)?;
                    }
                    self.scan = Some(probe::Scan::begin(m, &heap, &index, &s.scan.indexqual, params, &self.ops, snap)?);
                }
            }
            self.scan.as_mut().expect("set above").rescan()
        })();
        match armed {
            Ok(()) => Ok((heap, index)),
            Err(e) => {
                // the scan first (it holds the relations' aliases), then the relations; the caller drops the probe
                if let Some(scan) = self.scan.take() {
                    let _ = scan.end(em);
                }
                let _ = indexam::index_close(index, NoLock).and_then(|()| table::table_close(heap, NoLock));
                Err(e)
            }
        }
    }
}

impl Lent {
    pub(super) fn with_probe<T>(&mut self, f: impl FnOnce(&mut Probe) -> T) -> T {
        self.kept.with_mut(|k| f(&mut k.probe))
    }

    pub(super) fn desc(&self) -> Rc<TupleDescData<'static>> {
        self.kept.with(|k| k.probe.desc.clone())
    }
}

/// The portal's drop (portal.rs `drop_portal`): inside a live transaction, after Executes that returned, the scan parks
/// (the slot cleared, no pin left), the relations park, the probe's fetch context is reset and its bytes go back to the
/// point cache holder, and the probe goes back under its plan's key; else (a failed Execute, an abort's drop) the scan
/// ends, the relations close and nothing is kept. ExecEndIndexScan's errors have no caller to reach (C4 runs from
/// PortalDrop's cleanup): they drop the probe.
pub(super) fn put(lent: Lent) {
    let Lent { key, root, held, mut kept, heap, index, failed } = lent;
    let keep = !failed && xact::IsTransactionState() && on();
    let parked = kept.with_mut(|k| -> PgResult<()> {
        let p = &mut *k.probe;
        let em = p.scratch.exec_mcx();
        let r = if keep {
            p.scan.as_mut().map_or(Ok(()), |scan| scan.park(em))
        } else {
            p.scan.take().map_or(Ok(()), |scan| scan.end(em))
        };
        let index = park(index).map(|w| p.index = w);
        let heap = park(heap).map(|w| p.heap = w);
        p.scratch.reset_exec();
        p.scratch.home(skip::MEMO_OWNER);
        r.and(index).and(heap)
    });
    if !keep || parked.is_err() {
        if reporting() {
            report(if failed { "portal keep dropped (failed execute)" } else { "portal keep dropped (drop)" });
        }
        return;
    }
    put_slot(key, root, held, kept, None);
}

/// `kept` filed under `key` / `root` (`tick`: the slot's own, a probe taken and given back unused; else a new tick), the
/// least recently used slot out when every slot is full; then the point memo trims to its byte cap with the new bytes.
fn put_slot(key: PlanKey, root: usize, held: gravity_deps::Held, kept: KeptBox, tick: Option<u64>) {
    let bytes = bytes_of(&kept);
    let _ = KEEP.try_with(|k| {
        let Ok(mut g) = k.try_borrow_mut() else {
            return;
        };
        if g.slots.iter().all(Option::is_none) {
            g.short = mcx::memory_short_epoch();
        }
        let tick = tick.unwrap_or_else(|| {
            g.tick += 1;
            g.tick
        });
        let at = match g.slots.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                let i = victim(g.slots.iter().map(|s| s.as_ref().map_or(0, |s| s.tick)));
                drop_slot(&mut g.slots[i]);
                i
            }
        };
        g.slots[at] = Some(Slot { key, root, tick, held, kept, bytes });
        BYTES.with(|b| b.set(b.get() + bytes));
    });
    skip::trim();
}

/// The least recently used of `ticks` (a full keep's slots).
fn victim(ticks: impl Iterator<Item = u64>) -> usize {
    ticks.enumerate().min_by_key(|&(_, t)| t).map_or(0, |(i, _)| i)
}

fn drop_slot(s: &mut Option<Slot>) {
    if let Some(slot) = s.take() {
        BYTES.with(|b| b.set(b.get().saturating_sub(slot.bytes)));
    }
}

/// Plan `key` / `root`'s kept probe, out of its slot for a portal: with its reads, and the slot's tick. `None`: none kept,
/// or its reads moved (the slot goes), or the memory-short epoch moved since the first slot was filled (every slot goes).
fn take(key: PlanKey, root: usize) -> Option<(gravity_deps::Held, KeptBox, u64)> {
    let (got, stale) = KEEP.try_with(|k| {
        let Ok(mut g) = k.try_borrow_mut() else {
            return (None, None);
        };
        if g.slots.iter().all(Option::is_none) {
            return (None, None);
        }
        if g.short != mcx::memory_short_epoch() {
            return (None, Some(g.clear()));
        }
        let swept = g.sweep();
        if swept.0 > 0 && reporting() {
            report(&format!("portal keep sweep {} slots ({} bytes)", swept.0, swept.1)); // alloc: Cold
        }
        let Some(i) = g.slots.iter().position(|s| s.as_ref().is_some_and(|s| s.key == key && s.root == root)) else {
            return (None, None);
        };
        let slot = g.slots[i].take().expect("found above");
        BYTES.with(|b| b.set(b.get().saturating_sub(slot.bytes)));
        if !slot.held.stands() {
            return (None, Some((1, slot.bytes)));
        }
        g.tick += 1;
        let tick = g.tick;
        (Some((slot.held, slot.kept, tick)), None)
    })
    .unwrap_or((None, None));
    if let Some((n, b)) = stale.filter(|_| reporting()) {
        report(&format!("portal keep stale {n} slots ({b} bytes)")); // alloc: Cold
    }
    got
}

/// The kept probes' bytes (skip.rs `evict` counts them in the point memo's byte cap).
pub(super) fn bytes() -> usize {
    BYTES.try_with(Cell::get).unwrap_or(0)
}

/// Every kept probe out (`None`: none kept): the idle release and a memory-short sweep (skip.rs); their count and bytes.
pub(super) fn drop_all() -> Option<(usize, usize)> {
    KEEP.try_with(|k| k.try_borrow_mut().ok().map(|mut g| g.clear())).ok().flatten().filter(|&(n, _)| n > 0)
}

/// The session's teardown (`point::drop_state_at_teardown`): every kept probe goes with the session (a reused thread starts
/// the next session with none).
pub(super) fn drop_at_teardown() {
    let _ = drop_all();
}

#[cfg(test)]
mod tests {
    use super::victim;

    #[test]
    fn the_victim_is_the_least_recently_used_slot() {
        assert_eq!(victim([5u64, 2, 9].into_iter()), 1);
        assert_eq!(victim([7u64].into_iter()), 0);
    }
}

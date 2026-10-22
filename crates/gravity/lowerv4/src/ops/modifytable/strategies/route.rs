//! Road `write.route` (H16b / H27; R12-L15; cards/M4.md L3 `write/route.rs`): a write through a partitioned root or over
//! several result relations. Owner M4-ROUTE.
//!
//! The face its readers call (hookup (a)): a routed INSERT's `write/insert.rs` admit asks
//! `registry::need_strategy::<Route>(&node, WRITE_ROUTE)` and hands over the write's frozen plan; `Route::plan` returns
//! it with the route set (`WritePlan::with_route`: decompose never sets it) or refuses by name.
//!
//! R1 (this landing) serves the routed INSERT, PostgreSQL's ExecInsert through a partitioned root, the ladder's form
//! (`lower/src/write_route.rs` at ladder-final): the kernel writes each row in the root's layout into the root's write
//! site, and the host routes it (`hosts/src/write_route.rs` `RouteHost`, picked by `hosts/src/registry.rs` for a routed
//! INSERT plan): `ExecFindPartition` per row (PostgreSQL's error when no partition takes the row: "no partition of relation
//! ... found for row", at that row), the leaf opened under RowExclusiveLock at its first row and re-checked there against
//! the leaves frozen here (a leaf attached, altered or indexed since is an ERROR by name, never a fallback: rows may already
//! be in other leaves), the root's statement triggers, and each leaf's own constraints and indexes. A routed row skips its
//! partition constraint unless the leaf has a BEFORE ROW INSERT trigger (`hosts/src/write.rs`, ExecInsert's condition).
//!
//! Refused by name here: a leaf the route host cannot write (`route_leaves::unservable`: not heap (a routed INSERT's
//! pgrcolumnar leaf excepted: the host raises the pgrc trickle gate's 0A000 at its first row, D3622), rules, a generated
//! column, an index other than a plain btree, INSTEAD OF or unknown trigger facts), a leaf whose triggers fire on INSERT
//! (the leaf host fires them; the kind is write.transition's, M4-TRIG's hand-off: NotBuilt until it registers the
//! kind), a root with rules, or over the host's bound (a tree without partitions serves: ExecFindPartition's error at
//! its first row). V4R-498: a MERGE whose only writes are NOT MATCHED INSERTs routes them the same way (the host binds
//! the root's statement triggers for MERGE's actions); a MERGE's UPDATE / DELETE takes R2's form (merge/route.rs); a cross-partition UPDATE
//! (`Member::CrossPartition`, H-e (c)) takes its destinations from `Route::moves`. The IR bound search on key lanes (card:
//! int / date range and list keys) is R4, measure first.
//!
//! R2 serves a DELETE of several result relations (`Member::Targets`: an inheritance tree, a partitioned table not pruned
//! to one leaf), ModifyTable's `ExecLookupResultRelByOid` per row: decompose streams the write into its Append's arms (one
//! arm per target, in child order, as ModifyTable pulls them), write/delete.rs asks `Route::target` for the arm's own
//! target, and every arm stores its row's position (its relation, block, offset) into the one write site write/rowop.rs
//! shares across the arms; the host (`hosts/src/write_route.rs`, H-e(b)) deletes it from the target the position names,
//! each target opened under RowExclusiveLock at its first row and re-checked against the targets frozen here. An UPDATE of
//! several result relations (write/update.rs) is the same road: the write site carries the SET cells alone, in the plan's
//! `updateColnos` order (the root's attnos ascending), and the host forms each target's new row as ExecUpdate does over
//! the old version it fetches by the position (hosts `MultiTargetHost`).
use gravity_decompose::ops::modifytable::route_leaves::unservable;
use gravity_decompose::ops::modifytable::trig_facts::{TrigExt, TRIG_INSERT};
use gravity_decompose::ops::shared::modifytable::{Member, WriteDesc};
use gravity_decompose::catalog::LeafFact;
use gravity_decompose::Source;
use gravity_program::{limits, ArenaRef, Cmd, RouteLeaf, WritePlan};
use mcx::{GrowIn, PgVec};

use crate::face::end::TargetsRoad;
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;

crate::strategies! {
    /// Routing on key lanes with bound leaves.
    WRITE_ROUTE = 120, "write.route", Insert;
}

pub(crate) const ROWS: Table = Table { strategies: &[(WRITE_ROUTE, &Route), (WRITE_ROUTE, &TargetsRoad(target))], ..crate::ops::NONE };

/// Road `write.route`'s face (`RowOpRoad::Route`).
pub(crate) struct Route;

impl Route {
    /// Write `w`'s frozen plan `plan` (copied into `a`) as the host writes it, routed through its partitioned root; a
    /// refusal by name at `node` otherwise.
    pub(crate) fn plan(&self, gx: &Gx<'_, '_>, node: i32, w: &WriteDesc<'_>, plan: WritePlan, a: ArenaRef) -> WritePlan {
        let Some(r) = w.route.as_ref() else { crate::refuse!(node, Reason::Bug { payload: "write.route over a write without a routing root" }) };
        // R2 (several result relations) and R3 (the cross-partition move) are this road's other forms; V4R-498: a MERGE whose
        // writes are its NOT MATCHED INSERTs routes them as an INSERT (write/merge_admit.rs admits only that MERGE)
        if !matches!(w.cmd, Cmd::Insert | Cmd::Merge) || !w.members.has(Member::Route) || w.members.has(Member::Targets) || w.members.has(Member::CrossPartition) || r.cross_partition {
            crate::refuse!(node, Reason::Bug { payload: "write.route's INSERT plan over a write that is not a routed INSERT" });
        }
        let leaves = admit(gx, node, w, r.root_relid, plan.target_rel);
        plan.with_route(a, r.root_relid, &leaves[..])
    }
}

/// R2 (`TargetsRoad`, write/delete.rs's and write/update.rs's): pipeline `k` is one arm of a DELETE or an UPDATE of several
/// result relations. The arm's own target `(rti, relid)` (the one its source scans; `targets[0]` when none does, which the
/// kinds' own-source checks refuse) and the plan every arm shares, routed through the root (the named table). D2489: the
/// plan's leaves (every target in `targets` order, `leaves`) are added once per write, by the first arm's claim
/// (write/rowop.rs), never once per arm: N arms over N targets would read N x N relations and copy N x N leaves.
/// The cross-partition UPDATE (R3) stays NotBuilt by this road's name.
fn target(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>, a: ArenaRef) -> (u32, u32, WritePlan) {
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    if !matches!(w.cmd, Cmd::Delete | Cmd::Update | Cmd::Merge) {
        crate::refuse!(node, Reason::Bug { payload: "write.route's several targets over a write that is not an UPDATE / DELETE / MERGE" });
    }
    if w.targets.len() > limits::ROUTE_LEAVES {
        shape(node, "a write over more result relations than the route host's bound");
    }
    let arm = match &p.source {
        Source::Range { scanrelid, .. } | Source::Index { scanrelid, .. } | Source::Bitmap { scanrelid, .. } | Source::Tid { scanrelid, .. } => Some(u32::from(*scanrelid)),
        _ => None,
    };
    let t = w.targets.iter().find(|t| Some(t.rti) == arm).unwrap_or(&w.targets[0]);
    let root = w.route.map_or(w.targets[0].relid, |r| r.root_relid);
    (t.rti, t.relid, w.plan.copy_in(a).with_route(a, root, &[]))
}

/// R2's leaves: every result relation of write `w` in `targets` order, as the multi-target host re-checks them at their
/// first row (write/rowop.rs `claim` adds them to the plan the arms share, once per write: D2489).
pub(crate) fn leaves<'g>(gx: &Gx<'g, '_>, w: &WriteDesc<'_>) -> PgVec<'g, RouteLeaf> {
    let mut out = mcx::reserved_in(w.targets.len(), gx.mcx);
    for t in w.targets.iter() {
        // decompose refuses a target that is not a heap by contract; write/delete.rs checks each arm's
        let natts = gx.facts.rel(t.relid).map_or(0, |r| r.natts.max(0) as u16);
        out.push_in(RouteLeaf { relid: t.relid, natts, nindexes: t.plan.indexes.len() as u16, before_row: false });
    }
    out
}

impl Route {
    /// R3 (H-e (c)): a cross-partition UPDATE's destinations = every partition of its root (ExecCrossPartitionUpdate's
    /// ExecInsert through the root may take any), each written by the move host (hosts `write_move.rs`); a source leaf's DELETE
    /// and a destination's INSERT row triggers fire there (write.transition's, as R1's leaves). A refusal by name otherwise.
    pub(crate) fn moves<'g>(&self, gx: &Gx<'g, '_>, node: i32, w: &WriteDesc<'_>) -> PgVec<'g, RouteLeaf> {
        // MOVE: or a MERGE's UPDATE action through the root (merge/route.rs `plan`)
        let Some(r) = w.route.as_ref().filter(|_| matches!(w.cmd, Cmd::Update | Cmd::Merge) && w.members.has(Member::Route)) else {
            crate::refuse!(node, Reason::Bug { payload: "write.route's move over a write that is not an UPDATE or MERGE through a partitioned root" })
        };
        // V4R-329: check options (WITH CHECK OPTION / RLS) are the move host's: each destination takes the first result
        // relation's list mapped onto it (ExecInitPartitionInfo), checked with the UPDATE's kind (hosts write_move.rs)
        // V4R-323: an UPDATE over a join moves as any other (the marks are write/rowop.rs's): a moved row's delete that
        // meets a newer version answers Recheck, the leader's EvalPlanQual rebuilds the join from the marks and re-projects
        // the SET, and the write runs again (it may move or not: ExecCrossPartitionUpdate's EvalPlanQual, then lreplace)
        if gx.facts.rel_ext(r.root_relid).has_rules {
            shape(node, "a cross-partition UPDATE whose partitioned root has rules");
        }
        if !w.leaves_known {
            shape(node, "a cross-partition UPDATE whose partition leaves the facts source does not know (the corpus fixtures carry none)");
        }
        if w.leaves.is_empty() || w.leaves.len() > limits::ROUTE_LEAVES {
            shape(node, "a cross-partition UPDATE over no partitions or more than the route host's bound");
        }
        let mut out = mcx::reserved_in(w.leaves.len(), gx.mcx);
        for l in w.leaves.iter() {
            // the source leaf deletes (ExecDelete with changingPart): its DELETE row triggers fire as a DELETE's
            if let Some(why) = crate::ops::modifytable::triggers::refusal(l.trig, Cmd::Delete, false, false).filter(|_| l.has_triggers) {
                shape(node, why);
            }
            out.push_in(leaf(node, l, false, true));
        }
        out
    }
}

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// The ladder's `write_route::admit` (and `write::unwritable` / `check_write` for a routed root): every leaf the facts
/// source read at admission, each writable by the route host, as the host re-checks them at their first row.
fn admit<'g>(gx: &Gx<'g, '_>, node: i32, w: &WriteDesc<'_>, root: u32, target: u32) -> PgVec<'g, RouteLeaf> {
    // V4R-498: a MERGE's result relations are the leaves the planner kept (its INSERTs route through `root`)
    if w.cmd != Cmd::Merge && (w.targets.len() != 1 || w.targets[0].relid != target || root != target) {
        shape(node, "a routed INSERT whose result relation is not its routing root");
    }
    // D636: a check option (WITH CHECK OPTION / RLS) through a partitioned root is the leaf host's, mapped to the leaf's
    // layout as ExecInitPartitionInfo maps it (hosts/src/write_route.rs `leaf_checks`); write.checks refuses what it cannot
    if gx.facts.rel_ext(root).has_rules {
        shape(node, "a routed INSERT whose partitioned root has rules");
    }
    if !w.leaves_known {
        shape(node, "a routed INSERT whose partition leaves the facts source does not know (the corpus fixtures carry none)");
    }
    // a tree without partitions serves: the host's ExecFindPartition raises PG's "no partition of relation ... found for
    // row" (23514) at the first row, a sub-partitioned leaf's name when the tree below it has none; no row, no error
    if w.leaves.len() > limits::ROUTE_LEAVES {
        shape(node, "a routed INSERT over more partitions than the route host's bound");
    }
    let conflict_update = w.on_conflict.as_ref().is_some_and(|c| c.update);
    let mut out = mcx::reserved_in(w.leaves.len(), gx.mcx);
    for l in w.leaves.iter() {
        out.push_in(leaf(node, l, conflict_update, w.cmd == Cmd::Insert));
    }
    out
}

/// The instrumented lowering's refusal of a routed write whose leaf triggers fire (D3381): EXPLAIN ANALYZE prints a line per
/// routed leaf's trigger; exec's entry delegates on it rather than run the plain program.
pub const ROUTED_TRIGGER_LINES: &str = "EXPLAIN ANALYZE of a routed write whose leaf triggers fire (the routed relations' trigger lines)";

/// One leaf as the route host writes it, or a refusal by name.
/// PGRCSERVE M1: a routed INSERT (`closed`) writes a pgrcolumnar leaf through its table AM (M3: a move too); a MERGE refuses it.
fn leaf(node: i32, l: &LeafFact, conflict_update: bool, closed: bool) -> RouteLeaf {
    if let Some(what) = unservable(&LeafFact { heap: l.heap || closed && l.pgrc, ..*l }) {
        shape(node, what);
    }
    let before_row = l.has_triggers && l.trig.fires_before(Cmd::Insert);
    // D661-b: ON CONFLICT DO UPDATE's leaf host fires the leaf's UPDATE row triggers too (ExecOnConflictUpdate's ExecUpdate
    // on the leaf); EXCLUDED is the row its BEFORE ROW INSERT triggers returned (write.onconflict reads it back)
    let update = conflict_update && (l.trig.fires(Cmd::Update) || l.trig.fires_before(Cmd::Update));
    if l.has_triggers && (leaf_fires(l.trig) || update) {
        // the leaf host fires the leaf's row triggers (hosts write_route.rs, the root's statement half). EXPLAIN ANALYZE's
        // per-trigger lines name the routed leaves in PostgreSQL's routing order, which the host does not keep: exec's entry
        // delegates on this refusal, never the plain program (whose run prints no trigger line)
        if crate::frame::instr::on() {
            shape(node, ROUTED_TRIGGER_LINES);
        }
    }
    RouteLeaf { relid: l.relid, natts: l.natts.max(0) as u16, nindexes: l.nindexes, before_row }
}

/// A leaf trigger a routed INSERT fires: its BEFORE / AFTER / INSTEAD OF row triggers (a referencing partition's RI checks
/// are AFTER ROW), or facts the source could not answer. A leaf's statement triggers and transition tables never fire
/// for a row routed through the root (ExecInsert fires the root's: `hosts/src/write_route.rs` `root_stmt`).
fn leaf_fires(t: TrigExt) -> bool {
    !t.known || (t.before_row | t.after_row | t.instead_row) & TRIG_INSERT != 0
}

#[cfg(test)]
mod tests {
    use super::{leaf, leaf_fires};
    use gravity_decompose::ops::modifytable::trig_facts::{TrigExt, TRIG_DELETE, TRIG_INSERT, TRIG_UPDATE};
    use gravity_decompose::catalog::LeafFact;

    fn plain(relid: u32) -> LeafFact {
        LeafFact { relid, natts: 3, heap: true, foreign: false, pgrc: false, same_layout: true, has_triggers: false, trig: TrigExt::NONE, has_rules: false, has_generated: false, nindexes: 1, indexes_plain: true }
    }

    /// A plain heap leaf routes as admitted: its relid, width and index count frozen, no BEFORE ROW turn.
    #[test]
    fn a_plain_leaf_is_frozen_as_read() {
        let l = leaf(1, &plain(77), false, false);
        assert_eq!((l.relid, l.natts, l.nindexes, l.before_row), (77, 3, 1, false));
    }

    /// PGRCSERVE M1: a routed INSERT (M3: a move) freezes a pgrcolumnar leaf (written through its table AM); a MERGE's
    /// leaves, and another table AM's, keep the refusal.
    #[test]
    fn a_pgrc_leaf_routes_only_for_an_insert() {
        let c = LeafFact { heap: false, pgrc: true, ..plain(78) };
        assert_eq!(leaf(1, &c, false, true).relid, 78);
        let refused = |l: LeafFact, closed: bool| crate::catch(1, || leaf(1, &l, false, closed)).is_err();
        assert!(refused(c, false));
        assert!(refused(LeafFact { heap: false, ..plain(79) }, true));
    }

    /// Row triggers on INSERT fire for a routed row (RI checks included); UPDATE / DELETE-only row triggers and the
    /// leaf's statement / transition triggers do not; unknown facts do.
    #[test]
    fn leaf_triggers_fire_by_command() {
        assert!(!leaf_fires(TrigExt::NONE));
        assert!(leaf_fires(TrigExt::UNKNOWN));
        let fire: [fn(&mut TrigExt); 3] = [|t| t.before_row = TRIG_INSERT, |t| t.after_row = TRIG_INSERT | TRIG_UPDATE, |t| t.instead_row = TRIG_INSERT];
        for set in fire {
            let mut t = TrigExt::NONE;
            set(&mut t);
            assert!(leaf_fires(t));
        }
        let quiet: [fn(&mut TrigExt); 3] = [|t| t.after_row = TRIG_DELETE | TRIG_UPDATE, |t| t.statement = TRIG_INSERT, |t| t.transition = TRIG_INSERT];
        for set in quiet {
            let mut t = TrigExt::NONE;
            set(&mut t);
            assert!(!leaf_fires(t), "{t:?}");
        }
    }
}

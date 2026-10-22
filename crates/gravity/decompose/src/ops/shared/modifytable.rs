//! The write and row-lock descriptors other families read; their methods stay with ops/modifytable.
use gravity_program::{ArenaVec, Cmd, LockKind, SiteDesc, WaitPolicy, WritePlan};
use types_nodes::list::{IntList, NodeList, OidList};
use types_nodes::parsenodes::WCOKind;
use types_nodes::plannodes::{ModifyTable, RowMarkType};
use types_nodes::primnodes::MergeMatchKind;
use types_nodes::Node;

use crate::catalog::{IndexAm, InsteadOf, LeafFact};

/// The write family's members (decompose.md §2.12): one per option a `ModifyTable`, `LockRows` or `PlanRowMark` can
/// carry (run-time pruning, MULTIEXPR, `WHERE CURRENT OF` and the `tableoid` lane are mapped unconditionally). The
/// decomposer records each member a desc carries (`MemberSet`); the lowering family admits or refuses it by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Member {
    /// INSERT
    Insert,
    /// UPDATE
    Update,
    /// DELETE
    Delete,
    /// MERGE
    Merge,
    /// several result relations (an inheritance tree's UPDATE / DELETE)
    Targets,
    /// a write routed through a partitioned parent
    Route,
    /// an UPDATE of partition key columns (cross-partition: DELETE + routed INSERT)
    CrossPartition,
    /// WITH CHECK OPTION (a view's check)
    CheckView,
    /// a row-level security check option
    CheckRls,
    /// ON CONFLICT DO NOTHING
    ConflictNothing,
    /// ON CONFLICT DO UPDATE (the arbiter, EXCLUDED)
    ConflictUpdate,
    /// RETURNING
    Returning,
    /// RETURNING OLD
    ReturningOld,
    /// RETURNING NEW
    ReturningNew,
    /// merge_action() in RETURNING
    MergeActionFn,
    /// a SubPlan in a write's own expressions (a MERGE action, ON CONFLICT, a check, RETURNING)
    WriteSubPlan,
    /// row marks on a write's source relations (EPQ)
    WriteMarks,
    /// a row-share lock on a write's source relation
    LockingWriteMark,
    /// a row mark on a partitioned or inheritance parent (its children carry the lock)
    ParentMark,
    /// row locks on several relations
    MultiLock,
    /// a generated column on the written relation
    Generated,
    /// a virtual generated column on the written relation (no storage: the write stores the planner's NULL)
    GeneratedVirtual,
    /// a whole-row reference to a MERGE target (the carried columns form the row image)
    WholeRow,
    /// a unique btree index
    IndexUnique,
    /// a non-unique btree index
    IndexPlain,
    /// a deferrable unique or primary key index
    IndexDeferrable,
    /// an exclusion constraint
    IndexExclusion,
    /// a partial index
    IndexPartial,
    /// an index with an expression key
    IndexExpression,
    /// an index with INCLUDE columns
    IndexInclude,
    /// a non-btree index (hash / gin / gist / spgist / brin / bloom)
    IndexNonBtree,
    /// a view target (its INSTEAD OF row triggers take the rows)
    TargetView,
    /// a foreign-table target
    TargetForeign,
    /// a target whose access method is not heap and that this server cannot write through its table AM
    TargetOtherAm,
    /// a direct foreign modify
    DirectForeign,
    /// PGRCSERVE G2: a target of a writable non-heap access method (`TargetKind::Am`), written through its table AM
    TargetAm,
}

/// A set of members: what a desc carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct MemberSet(pub u64);

#[derive(Clone, Copy)]
pub struct OnConflict<'p> {
    pub update: bool,
    pub arbiter: &'p OidList<'p>,
    pub set: &'p NodeList<'p>,
    /// DO UPDATE's target attnos, one per `set` entry (`ModifyTable.onConflictCols`: the SET list's resnos are 1..n).
    pub cols: &'p IntList<'p>,
    pub filter: Option<Node<'p>>,
    pub excl_rti: u32,
}

/// The write op (oltp-kernels.md's shapes, ir-probe §18): the pipeline's rows are the source (an
/// INSERT's values, an UPDATE's SET values + ctid, a DELETE's ctid); `site` is the write site the
/// host writes through, `plan` the index list it maintains, `set` an UPDATE's target attnos;
/// RETURNING is the projection after the op over the written row left in the site's cells.
/// `targets[0]` is `rti` / `site` / `plan`; `members` names every family member the write carries.
pub struct WriteDesc<'p> {
    pub cmd: Cmd,
    pub rti: u32,
    pub site: SiteDesc,
    pub plan: WritePlan,
    pub set: Option<&'p IntList<'p>>,
    pub returning: Option<&'p NodeList<'p>>,
    pub on_conflict: Option<OnConflict<'p>>,
    pub epq: i32,
    pub marks: &'p NodeList<'p>,
    pub merge: Option<MergeDesc<'p>>,
    /// Leaked into the arena once built, as are `checks`, `leaves`, `mark_descs` and `old_cols`: an Append's arms each
    /// carry a copy of the write (`append.rs` `dup`) and share these lists (D2483: a per-arm copy of every target was N
    /// arms x N targets; a per-arm copy of every leaf ran the decompose arena out past ~2000 partitions).
    pub targets: &'p [Target<'p>],
    /// The child's `tableoid` junk column picking the row's target (0 = one target).
    pub tableoid_col: i16,
    pub checks: &'p [CheckOpt<'p>],
    pub route: Option<RouteDesc>,
    /// A routed INSERT's leaves as admitted (`route_leaves.rs`); `leaves_known` = the facts source answered.
    pub leaves: &'p [LeafFact],
    pub leaves_known: bool,
    pub old_new: OldNew<'p>,
    pub mark_descs: &'p [RowMark],
    /// UNION-COPY: `(parent, member)` for each COPY mark whose relation is a flattened UNION ALL (a subquery
    /// RTE the planner made an appendrel parent: no scan carries its rti) and every relation under it, to the leaves
    /// (`union_members`). PostgreSQL's recheck hands that stored row to no scan and scans the members, which no mark names.
    pub copy_arms: &'p [(u32, u32)],
    pub members: MemberSet,
    /// An UPDATE's old row (R12-L05): `(attno, resno)` for each target attribute the SET list leaves, carried from the
    /// target's scan in the child's projection (carry.rs; `update_carry`). Empty for other commands, and for an UPDATE whose
    /// child is a Result with no input (a constant-false filter: no row comes).
    pub old_cols: &'p [(i16, i16)],
}

/// One result relation: its write site, the frozen `WritePlan` (the btree indexes `IndexPlan`
/// expresses), every index classified (`indexes` ⊇ `plan.indexes()`; the consumer binds the rest
/// or refuses by name), the child's `ctid` junk column (0 for an INSERT), the generated columns, and
/// its own per-target lists (`updateColnosLists[i]`, `returningLists[i]`, `mergeActionLists[i]` +
/// `mergeJoinConditions[i]`: a partition's Vars are its own attnos). `targets[0]`'s are the desc's.
/// A view target (`TargetKind::View`, member `TargetView`) has no heap, no ctid and no indexes: its
/// INSTEAD OF row trigger (`instead`, the relation's fact) takes each formed row — an UPDATE's /
/// DELETE's old row is the child's `wholerow` junk column (`wholerow_col`, the view's row image the
/// executor hands the trigger as OLD), the trigger's returned row is what RETURNING and the view's
/// WITH CHECK OPTION read, a NULL return skips the row (`ExecIR{Insert,Update,Delete}Triggers`).
pub struct Target<'p> {
    pub rti: u32,
    pub relid: u32,
    pub kind: TargetKind,
    pub site: SiteDesc,
    pub plan: WritePlan,
    pub indexes: ArenaVec<IndexDesc>,
    pub junk_col: i16,
    pub wholerow_col: i16,
    pub instead: InsteadOf,
    pub generated: ArenaVec<(i16, u8)>,
    pub set: Option<&'p IntList<'p>>,
    pub returning: Option<&'p NodeList<'p>>,
    pub merge: Option<MergeActions<'p>>,
    pub to_root: ArenaVec<i16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    Heap,
    Partitioned,
    View,
    Foreign,
    /// PGRCSERVE G2: another access method the server writes through its table AM (`writable_am`)
    Am,
    OtherAm,
}

/// One index of a written relation, classified (`IndexFact` + `IndexExt`): `plain` = the frozen
/// `IndexPlan` form (btree, no predicate, no expression, no INCLUDE; a deferrable unique one too, which
/// the host inserts with UNIQUE_CHECK_PARTIAL, H-a2, and a deferrable exclusion one, probed CEOUC_NOWAIT, V4R-297); `known` = the
/// constraint facts were available (the corpus header / a relcache facts source that carries them).
#[derive(Clone, Copy, Debug)]
pub struct IndexDesc {
    pub indexid: u32,
    pub am: IndexAm,
    /// The AM's insert is one the host serves (`Facts::index_insertable`).
    pub insertable: bool,
    pub nkeys: u8,
    pub unique: bool,
    pub primary: bool,
    pub deferrable: bool,
    pub exclusion: bool,
    pub partial: bool,
    pub expression: bool,
    pub include: bool,
    pub known: bool,
}

/// One WITH CHECK OPTION / RLS check (`ExecWithCheckOptions`) over the **new** row of `target`:
/// a false or NULL result is the 44000 view violation naming `relname` or the 42501 policy
/// violation naming `polname`; `cascaded` only words the error (the plan expanded the set).
#[derive(Clone, Copy)]
pub struct CheckOpt<'p> {
    pub target: u16,
    pub kind: WCOKind,
    pub relname: Option<&'p str>,
    pub polname: Option<&'p str>,
    pub qual: Node<'p>,
    pub cascaded: bool,
}

/// A write through a partitioned parent: each row is routed to its leaf (`ExecFindPartition`), the
/// leaf's write site opened on first use; `cross_partition` = `partColsUpdated`.
#[derive(Clone, Copy, Debug)]
pub struct RouteDesc {
    pub root_rti: u32,
    pub root_relid: u32,
    pub root_natts: u16,
    pub cross_partition: bool,
}

/// RETURNING OLD / NEW: the aliases and which images the list reads.
#[derive(Clone, Copy, Debug, Default)]
pub struct OldNew<'p> {
    pub old_alias: Option<&'p str>,
    pub new_alias: Option<&'p str>,
    pub old: bool,
    pub new: bool,
}

/// One WHEN clause: its match kind, its command (`None` = DO NOTHING), its quals (a list, ANDed),
/// the row it forms (an INSERT's columns in attno order; an UPDATE's SET values over `set`).
#[derive(Clone, Copy)]
pub struct MergeAction<'p> {
    pub kind: MergeMatchKind,
    pub cmd: Option<Cmd>,
    pub quals: Option<&'p NodeList<'p>>,
    pub tlist: &'p NodeList<'p>,
    pub set: Option<&'p IntList<'p>>,
}

/// A target's WHEN clauses: `join_cond` (only with NOT MATCHED BY SOURCE actions) tells MATCHED from
/// NOT MATCHED BY SOURCE among the rows with a target row.
pub struct MergeActions<'p> {
    pub join_cond: Option<&'p NodeList<'p>>,
    pub actions: ArenaVec<MergeAction<'p>>,
}

/// MERGE (decompose.md §2.12): the child is the source ⟕ target join; per joined row the first
/// action of the row's kind whose quals hold runs through the write host. `join_cond` / `actions`
/// are `targets[0]`'s (`Target.merge` has every target's). The target's old row travels in the
/// child's projection (carry.rs): `rowid_col` its ctid (NULL = not matched by target), `target_cols`
/// its attributes as (attno, resno) — the routing root's attnos when the write is routed.
/// `returning_action`: RETURNING reads `merge_action()`, the action's command tag per row.
pub struct MergeDesc<'p> {
    pub join_cond: Option<&'p NodeList<'p>>,
    pub actions: ArenaVec<MergeAction<'p>>,
    pub rowid_col: i16,
    pub target_cols: ArenaVec<(i16, i16)>,
    pub returning_action: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct RowMark {
    pub rti: u32,
    pub relid: u32,
    pub mark: RowMarkType,
    pub lock: LockKind,
    pub wait: WaitPolicy,
    /// `isParent`: the mark of a partitioned / inheritance parent, whose children carry the lock.
    pub parent: bool,
    /// `prti`: the top parent's rti for a partitioned / inherited child (and an intermediate parent), else `rti`.
    pub prti: u32,
    /// D655: the child projection's `ctid{rowmarkId}` junk column (preprocess_targetlist's, the row's ctid that
    /// EvalPlanQualFetchRowMark refetches a REFERENCE-marked relation's row by), or 0; D732: a COPY mark's
    /// `wholerow{rowmarkId}` (the row itself, which EvalPlanQual substitutes whole).
    pub ctid_col: i16,
    /// REFPARENT: a parent's or child's mark, the child projection's `tableoid{rowmarkId}` junk column
    /// (preprocess_targetlist adds one per parent mark; its children share the parent's rowmarkId), whose value names the
    /// child the row came from (EvalPlanQualFetchRowMark's tableoid test), or 0.
    pub toid_col: i16,
}

pub struct LockDesc<'p> {
    pub marks: ArenaVec<RowMark>,
    pub plan: WritePlan,
    pub epq: i32,
    pub tlist: &'p NodeList<'p>,
    pub members: MemberSet,
}

/// The write's own expression lists past the child's (subplan.rs scans them for per-row SubPlans;
/// `place` puts one before the write op, or before RETURNING's projection).
pub fn expr_lists<'p>(mt: &'p ModifyTable<'p>, f: &mut dyn FnMut(&'p NodeList<'p>)) {
    f(&mt.onConflictSet);
    // IN-CONFLICT: ON CONFLICT DO UPDATE's WHERE (an implicit-AND list after the planner), the conflict arm's read
    if let Some(q) = mt.onConflictWhere.and_then(|q| q.as_list()) {
        f(q);
    }
    // D1112: each check option's qual (its implicit-AND list), not the WithCheckOption nodes `expr::walk` does not enter
    for l in mt.withCheckOptionLists.iter() {
        for w in l.as_list().map_or(NodeList::nil().iter(), |l| l.iter()) {
            if let Some(q) = w.as_with_check_option().and_then(|w| w.qual).and_then(|q| q.as_list()) {
                f(q);
            }
        }
    }
    for l in mt.returningLists.iter() {
        if let Some(l) = l.as_list() {
            f(l);
        }
    }
    for l in mt.mergeActionLists.iter() {
        for a in l.as_list().map_or(NodeList::nil().iter(), |l| l.iter()) {
            if let Some(a) = a.as_merge_action() {
                f(&a.targetList);
                if let Some(q) = a.qual.and_then(|q| q.as_list()) {
                    f(q);
                }
            }
        }
    }
}

/// Whether one of the write's pre-write expressions reads SubPlan `plan_id` (subplan.rs `place`).
pub fn refers(d: &WriteDesc<'_>, plan_id: i32) -> bool {
    let mut lists = d.checks.iter().map(|c| c.qual).any(|q| crate::ops::shared::subplan::refers(q, plan_id));
    if let Some(oc) = &d.on_conflict {
        lists |= crate::ops::shared::subplan::refers_list(oc.set, plan_id) || oc.filter.is_some_and(|f| crate::ops::shared::subplan::refers(f, plan_id));
    }
    if let Some(m) = &d.merge {
        for a in m.actions.iter() {
            lists |= crate::ops::shared::subplan::refers_list(a.tlist, plan_id) || a.quals.is_some_and(|q| crate::ops::shared::subplan::refers_list(q, plan_id));
        }
    }
    lists
}

/// An Append arm's copy of the write (`append.rs` `dup`): the fields that own a list copy it (the site, the plan, MERGE's
/// actions); the rest are shared (`&'p` lists, `Copy` descriptors), as the arms share `targets`, `checks` and `leaves`.
impl Clone for WriteDesc<'_> {
    fn clone(&self) -> Self {
        WriteDesc {
            cmd: self.cmd,
            rti: self.rti,
            site: self.site.clone(),
            plan: self.plan.clone(),
            set: self.set,
            returning: self.returning,
            on_conflict: self.on_conflict,
            epq: self.epq,
            marks: self.marks,
            merge: self.merge.clone(),
            targets: self.targets,
            tableoid_col: self.tableoid_col,
            checks: self.checks,
            route: self.route,
            leaves: self.leaves,
            leaves_known: self.leaves_known,
            old_new: self.old_new,
            mark_descs: self.mark_descs,
            copy_arms: self.copy_arms,
            members: self.members,
            old_cols: self.old_cols,
        }
    }
}

impl Clone for MergeDesc<'_> {
    fn clone(&self) -> Self {
        MergeDesc { join_cond: self.join_cond, actions: self.actions.clone(), rowid_col: self.rowid_col, target_cols: self.target_cols.clone(), returning_action: self.returning_action }
    }
}

impl Clone for Target<'_> {
    fn clone(&self) -> Self {
        Target {
            rti: self.rti,
            relid: self.relid,
            kind: self.kind,
            site: self.site.clone(),
            plan: self.plan.clone(),
            indexes: self.indexes.clone(),
            junk_col: self.junk_col,
            wholerow_col: self.wholerow_col,
            instead: self.instead,
            generated: self.generated.clone(),
            set: self.set,
            returning: self.returning,
            merge: self.merge.clone(),
            to_root: self.to_root.clone(),
        }
    }
}

impl Clone for MergeActions<'_> {
    fn clone(&self) -> Self {
        MergeActions { join_cond: self.join_cond, actions: self.actions.clone() }
    }
}

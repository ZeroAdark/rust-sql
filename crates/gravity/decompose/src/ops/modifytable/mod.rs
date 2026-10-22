use core::fmt::{self, Write};

use gravity_program::{limits, ArenaVec, Cmd, IndexPlan, LockKind, Parallel, Pipeline, Reason, SiteDesc, WaitPolicy, WritePlan};
use mcx::{GrowIn, PgVec};
use types_nodes::list::{IntList, NodeList};
use types_nodes::nodes_enums::{CmdType, LockWaitPolicy};
use types_nodes::parsenodes::WCOKind;
use types_nodes::plannodes::{ModifyTable, Plan, RowMarkType};
use types_nodes::primnodes::{MergeMatchKind, VarReturningType};
use types_nodes::{Node, NodeTag};

use crate::catalog::{Am, IndexAm, IndexFact, InsteadOf, RelFact};
use crate::graph::{Op, PipelineGraph};
use crate::ledger::Kind;
use crate::ops;
use crate::admit::Refusal;
use crate::walk::{Builder, Open, STMT_NODE};
use crate::ops::shared::modifytable::{refers, CheckOpt, IndexDesc, LockDesc, Member, MemberSet, MergeAction, MergeActions, MergeDesc, OldNew, OnConflict, RouteDesc, RowMark, Target, TargetKind, WriteDesc};
pub mod carry;
pub mod lockrows;
pub mod merge;
pub mod partwrite;
pub mod route_leaves;
pub mod trig_facts;

impl Member {
    /// Every member, in declaration order.
    pub const LIST: [Member; 36] = [Member::Insert, Member::Update, Member::Delete, Member::Merge, Member::Targets, Member::Route, Member::CrossPartition, Member::CheckView, Member::CheckRls, Member::ConflictNothing, Member::ConflictUpdate, Member::Returning, Member::ReturningOld, Member::ReturningNew, Member::MergeActionFn, Member::WriteSubPlan, Member::WriteMarks, Member::LockingWriteMark, Member::ParentMark, Member::MultiLock, Member::Generated, Member::GeneratedVirtual, Member::WholeRow, Member::IndexUnique, Member::IndexPlain, Member::IndexDeferrable, Member::IndexExclusion, Member::IndexPartial, Member::IndexExpression, Member::IndexInclude, Member::IndexNonBtree, Member::TargetView, Member::TargetForeign, Member::TargetOtherAm, Member::DirectForeign, Member::TargetAm];
}

impl MemberSet {
    pub const EMPTY: MemberSet = MemberSet(0);

    pub const fn with(self, m: Member) -> MemberSet {
        MemberSet(self.0 | 1 << m as u8)
    }

    pub const fn has(self, m: Member) -> bool {
        self.0 & (1 << m as u8) != 0
    }

    pub fn iter(self) -> impl Iterator<Item = Member> {
        Member::LIST.into_iter().filter(move |m| self.has(*m))
    }
}

impl IndexDesc {
    /// Decision 18: an index the plain form leaves out only for a predicate, an expression key or INCLUDE columns (any
    /// access method `plain` takes): frozen as an `IndexPlan` the host maintains through the executor's IndexInfo (hosts
    /// index_maint.rs `general`: the predicate, then FormIndexDatum). V4R-297: a deferrable exclusion constraint too (the
    /// host's CEOUC_NOWAIT probe and recheck list).
    pub const fn general(&self) -> bool {
        (self.partial || self.expression || self.include) && Self::am_plain(self)
    }

    const fn am_plain(&self) -> bool {
        // a non-btree unique is a temporal key (WITHOUT OVERLAPS): its exclusion operators are what index_maint checks
        matches!(self.am, IndexAm::Btree) || (self.insertable && (!self.unique || self.exclusion))
    }

    pub const fn plain(&self) -> bool {
        Self::am_plain(self) && !self.partial && !self.expression && !self.include
    }
}

impl LockDesc<'_> {
    /// L2 (V4R-248): the top parent's rti when the lock is one partitioned / inheritance family's and nothing else's: a top
    /// parent's mark (`isParent`, prti = rti), every other mark its child or intermediate parent (prti = its rti) with the
    /// plan's strength and wait policy, one of them a child. ExecLockRows skips the parent marks and locks each row in its
    /// child's relation. A family beside another relation's mark (a join: REFERENCE / COPY, D655) is None.
    pub fn family(&self) -> Option<u32> {
        let top = self.marks.iter().find(|m| m.parent && m.prti == m.rti)?;
        let mut kin = self.marks.iter().filter(|m| m.rti != top.rti);
        let one = kin.clone().all(|m| m.prti == top.rti && m.lock == self.plan.lock && m.wait == self.plan.wait);
        (one && kin.any(|m| !m.parent)).then_some(top.rti)
    }

    /// FAMJOIN: `family`, or one family's marks (one top parent, its children and intermediate parents with the
    /// plan's strength and wait policy, one of them a child) beside other relations' REFERENCE marks, which lock nothing (a
    /// join under FOR UPDATE OF the family: EvalPlanQualFetchRowMark refetches their rows at the versions read).
    pub fn family_top(&self) -> Option<u32> {
        if let Some(t) = self.family() {
            return Some(t);
        }
        let mut tops = self.marks.iter().filter(|m| m.parent && m.prti == m.rti);
        let (Some(top), None) = (tops.next(), tops.next()) else { return None };
        let kin = |m: &RowMark| m.prti == top.rti;
        let other = |m: &RowMark| m.lock == LockKind::None && !m.parent && m.mark == RowMarkType::ROW_MARK_REFERENCE;
        let ok = self.marks.iter().all(|m| if kin(m) { m.lock == self.plan.lock && m.wait == self.plan.wait } else { other(m) });
        (ok && self.marks.iter().any(|m| kin(m) && !m.parent)).then_some(top.rti)
    }
}

/// The members a desc carries, recorded as they are found.
pub(crate) struct Carried {
    pub(crate) set: MemberSet,
}

impl Carried {
    pub(crate) fn add(&mut self, m: Member) {
        self.set = self.set.with(m);
    }
}

fn index_desc(b: &Builder<'_, '_>, i: &IndexFact) -> IndexDesc {
    let e = b.facts.index_ext(i.indexid);
    let expression = i.nkeys == 0 || i.keys().iter().any(|&a| a <= 0);
    let nkeyatts = if e.known && e.nkeyatts > 0 { e.nkeyatts } else { i.nkeys };
    IndexDesc { indexid: i.indexid, am: b.facts.index_am(i.indexid), insertable: b.facts.index_insertable(i.indexid), nkeys: nkeyatts, unique: i.unique, primary: e.primary, deferrable: e.deferrable, exclusion: e.exclusion, partial: i.partial, expression, include: nkeyatts < i.nkeys, known: e.known }
}

const fn index_member(d: &IndexDesc) -> Member {
    if !matches!(d.am, IndexAm::Btree) {
        Member::IndexNonBtree
    } else if d.exclusion {
        Member::IndexExclusion
    } else if d.deferrable {
        Member::IndexDeferrable
    } else if d.partial {
        Member::IndexPartial
    } else if d.expression {
        Member::IndexExpression
    } else if d.include {
        Member::IndexInclude
    } else if d.unique {
        Member::IndexUnique
    } else {
        Member::IndexPlain
    }
}

/// Every index of the relation classified, the plain btree ones also as the frozen `IndexPlan`s.
fn index_set(b: &Builder<'_, '_>, rel: RelFact, c: &mut Carried, k: Kind, id: i32) -> Result<(ArenaVec<IndexDesc>, ArenaVec<IndexPlan>), Refusal> {
    let mut descs = ArenaVec::new_in(b.arena);
    let mut plans = ArenaVec::new_in(b.arena);
    let mut err = None;
    let all = b.facts.indexes_all(rel.relid, &mut |i: IndexFact| {
        if err.is_some() {
            return;
        }
        if descs.len() == limits::INDEXES {
            err = Some(Refusal::shape(k, id, "more than 65535 indexes on the written relation (the u16 index count)"));
            return;
        }
        let d = index_desc(b, &i);
        c.add(index_member(&d));
        // Decision 18: a partial, expression or INCLUDE btree index is frozen too (its keys indkey as is: 0 = an expression
        // column, INCLUDE columns after the keys); the host evaluates the predicate and the expressions as
        // ExecInsertIndexTuples does
        if d.plain() || d.general() {
            plans.push(IndexPlan::new(b.arena, i.indexid, i.keys(), i.unique));
        }
        descs.push(d);
    });
    if !all && err.is_none() {
        err = Some(Refusal::shape(k, id, "an index of the written relation another session holds locked now (its facts unread; D582)"));
    }
    match err {
        Some(e) => Err(e),
        None => Ok((descs, plans)),
    }
}

pub(crate) fn serial_after(open: &mut Open<'_>, reason: Reason) {
    let site = if open.desc.driving_site >= 0 { open.desc.driving_site + 1 } else { 0 };
    open.desc = Pipeline { parallel: Parallel::Ineligible { site, reason }, finish: false, ..open.desc };
}

/// The child projection's junk column of that name (`ExecFindJunkAttributeInTlist`), or 0.
fn junk_col(plan: &Plan<'_>, name: &str) -> i16 {
    let Some(child) = plan.lefttree.and_then(|c| c.as_plan()) else { return 0 };
    child.targetlist.iter().filter_map(|e| e.as_target_entry()).find(|te| te.resjunk && te.resname == Some(name)).map_or(0, |te| te.resno)
}

/// D655: the child projection's junk column `<prefix><rowmarkId>` (preprocess_targetlist's `ctid%u`), or 0.
fn junk_mark_col(plan: &Plan<'_>, prefix: &str, id: u32) -> i16 {
    let Some(child) = plan.lefttree.and_then(|c| c.as_plan()) else { return 0 };
    let named = |n: &str| n.strip_prefix(prefix).is_some_and(|d| d.parse::<u32>() == Ok(id));
    child.targetlist.iter().filter_map(|e| e.as_target_entry()).find(|te| te.resjunk && te.resname.is_some_and(named)).map_or(0, |te| te.resno)
}

/// A target's kind: a partitioned table, a view or a foreign table by its relkind; any other relation (a table, a
/// materialized view, a TOAST table) by its access method (CAND-03): heap, a `writable_am`, or one it cannot write.
const fn target_kind(rel: RelFact) -> TargetKind {
    match rel.relkind {
        b'p' => TargetKind::Partitioned,
        b'v' => TargetKind::View,
        b'f' => TargetKind::Foreign,
        _ if matches!(rel.am, Am::Heap) => TargetKind::Heap,
        _ if writable_am(&rel) => TargetKind::Am,
        _ => TargetKind::OtherAm,
    }
}

/// PGRCSERVE G2: a non-heap relation the server writes through its table AM: an AM the facts name, or one on the rows road.
pub const fn writable_am(rel: &RelFact) -> bool {
    !matches!(rel.am, Am::Heap) && (rel.rows || !matches!(rel.am, Am::Other))
}

/// One `Target` per entry of `resultRelations` (decompose.md §3 decision 75): a partitioned parent
/// carries no index plans of its own — routing opens the leaf's.
fn targets_of<'p>(b: &mut Builder<'p, '_>, mt: &'p ModifyTable<'p>, plan: &'p Plan<'p>, cmd: Cmd, c: &mut Carried, k: Kind, id: i32) -> Result<ArenaVec<Target<'p>>, Refusal> {
    let ctid = if cmd == Cmd::Insert { 0 } else { junk_col(plan, "ctid") };
    let mut targets = ArenaVec::with_capacity_in(mt.resultRelations.len(), b.arena);
    for r in mt.resultRelations.iter() {
        let rti = r as u32;
        let rel = b.relation(rti, k, id)?;
        let kind = target_kind(rel);
        let i = targets.len();
        match kind {
            TargetKind::View => c.add(Member::TargetView),
            TargetKind::Foreign => c.add(Member::TargetForeign),
            TargetKind::OtherAm => c.add(Member::TargetOtherAm),
            TargetKind::Am => c.add(Member::TargetAm),
            TargetKind::Partitioned | TargetKind::Heap => {}
        }
        let (instead, wholerow) = if kind == TargetKind::View { view_target(b, rel, mt, cmd, plan, i, k, id)? } else { (InsteadOf::UNKNOWN, 0) };
        // a DELETE maintains no index (heap_delete marks the tuple; the entries wait for vacuum): none is classified or frozen
        let (indexes, plans) = if matches!(kind, TargetKind::Partitioned | TargetKind::View) || cmd == Cmd::Delete { (ArenaVec::new_in(b.arena), ArenaVec::new_in(b.arena)) } else { index_set(b, rel, c, k, id)? };
        let mut generated = ArenaVec::new_in(b.arena);
        for a in 1..=rel.natts.max(0) {
            let g = b.facts.att_generated(rel.relid, a);
            if g != 0 {
                c.add(if g == b'v' { Member::GeneratedVirtual } else { Member::Generated });
                generated.push((a, g));
            }
        }
        if targets.len() == limits::RESULT_RELATIONS {
            return Err(Refusal::shape(k, id, "more than 65535 result relations (the u16 target index)"));
        }
        let set = match cmd {
            // `SET b = DEFAULT` on a generated column leaves no column to update: the empty list (an UPDATE's `onConflictCols` is the plan's own nil list of that lifetime)
            Cmd::Update if mt.updateColnosLists.len() <= i || mt.updateColnosLists.nth(i).as_int_list().is_none_or(|l| l.is_nil()) => Some(&mt.onConflictCols),
            Cmd::Update => Some(mt.updateColnosLists.nth(i).as_int_list().ok_or(Refusal::shape(k, id, "an UPDATE without its column list"))?),
            _ => None,
        };
        let returning = if mt.returningLists.len() > i { mt.returningLists.nth(i).as_list() } else { None };
        let merge = if cmd == Cmd::Merge { Some(merge::merge_actions(b, mt, i, k, id)?) } else { None };
        targets.push(Target { rti, relid: rel.relid, kind, site: SiteDesc::write(rel.relid, rel.natts.max(0) as u16), plan: WritePlan::write(b.arena, cmd, rel.relid, &plans), indexes, junk_col: if kind == TargetKind::View { 0 } else { ctid }, wholerow_col: wholerow, instead, generated, set, returning, merge, to_root: ArenaVec::new_in(b.arena) });
    }
    if targets.is_empty() {
        return Err(Refusal::shape(k, id, "a write without a result relation"));
    }
    Ok(targets)
}

/// A view target's facts (decompose.md §2.12 "a view target"): the INSTEAD OF row triggers the relation has
/// and the child's `wholerow` junk column (an UPDATE / DELETE / MERGE's old row image, `ExecInitModifyTable`'s
/// junk filter for a view). Refused by name: a command whose INSTEAD OF trigger the view lacks (the executor's
/// `CheckValidResultRel` raises 55000 `cannot insert into view`: the row executor's error), a plan without the
/// wholerow column. A MERGE's old row is that wholerow image too (`merge_carried`: no carry.rs row, no ctid).
fn view_target<'p>(b: &Builder<'p, '_>, rel: RelFact, mt: &'p ModifyTable<'p>, cmd: Cmd, plan: &'p Plan<'p>, i: usize, k: Kind, id: i32) -> Result<(InsteadOf, i16), Refusal> {
    let t = b.facts.instead_of(rel.relid);
    if t.known {
        let lacks = |c: Cmd| match c {
            Cmd::Insert => !t.insert,
            Cmd::Update => !t.update,
            Cmd::Delete => !t.delete,
            Cmd::Merge | Cmd::Select => false,
        };
        let missing = match cmd {
            Cmd::Merge => {
                let list = if mt.mergeActionLists.len() > i { mt.mergeActionLists.nth(i).as_list() } else { None };
                list.is_some_and(|l| {
                    l.iter().filter_map(|a| a.as_merge_action()).any(|a| match a.commandType {
                        CmdType::CMD_INSERT => lacks(Cmd::Insert),
                        CmdType::CMD_UPDATE => lacks(Cmd::Update),
                        CmdType::CMD_DELETE => lacks(Cmd::Delete),
                        _ => false,
                    })
                })
            }
            c => lacks(c),
        };
        if missing {
            return Err(Refusal::shape(k, id, "a view target without the INSTEAD OF row trigger its command needs (the executor's 55000 'cannot insert/update/delete from view', CheckValidResultRel)"));
        }
    }
    let wholerow = if cmd == Cmd::Insert { 0 } else { junk_col(plan, "wholerow") };
    if cmd != Cmd::Insert && wholerow == 0 {
        return Err(Refusal::shape(k, id, "a view target without its wholerow junk column (the old row image the INSTEAD OF trigger takes)"));
    }
    Ok((t, wholerow))
}

/// The routing face (decompose.md §3 decision 75): a `rootRelation` (an inheritance or partition tree's
/// UPDATE / DELETE) or a partitioned result relation (an INSERT through the parent).
fn route_of(b: &Builder<'_, '_>, mt: &ModifyTable<'_>, targets: &[Target<'_>], c: &mut Carried, k: Kind, id: i32) -> Result<Option<RouteDesc>, Refusal> {
    let root_rti = if mt.rootRelation != 0 {
        mt.rootRelation
    } else if let Some(t) = targets.iter().find(|t| t.kind == TargetKind::Partitioned) {
        t.rti
    } else {
        if mt.partColsUpdated {
            return Err(Refusal::shape(k, id, "an UPDATE of partition key columns without a partitioned root"));
        }
        if targets.len() > 1 {
            c.add(Member::Targets);
        }
        return Ok(None);
    };
    let root = b.relation(root_rti, k, id)?;
    if root.relkind == b'p' {
        c.add(Member::Route);
    } else {
        c.add(Member::Targets);
    }
    if mt.partColsUpdated {
        c.add(Member::CrossPartition);
    }
    Ok(Some(RouteDesc { root_rti, root_relid: root.relid, root_natts: root.natts.max(0) as u16, cross_partition: mt.partColsUpdated }))
}

/// `withCheckOptionLists` in the targets' order (`ExecWithCheckOptions`).
fn checks_of<'p>(b: &Builder<'p, '_>, mt: &'p ModifyTable<'p>, targets: &[Target<'_>], c: &mut Carried, k: Kind, id: i32) -> Result<ArenaVec<CheckOpt<'p>>, Refusal> {
    let mut checks = ArenaVec::new_in(b.arena);
    if mt.withCheckOptionLists.is_nil() {
        return Ok(checks);
    }
    if mt.withCheckOptionLists.len() != targets.len() {
        return Err(Refusal::shape(k, id, "a WITH CHECK OPTION list that is not one per result relation"));
    }
    for (t, l) in mt.withCheckOptionLists.iter().enumerate() {
        let l = l.as_list().ok_or(Refusal::shape(k, id, "a withCheckOptionLists entry that is not a list"))?;
        for w in l.iter() {
            let w = w.as_with_check_option().ok_or(Refusal::shape(k, id, "a check option that is not a WithCheckOption"))?;
            let qual = w.qual.ok_or(Refusal::shape(k, id, "a check option without a qual"))?;
            c.add(if w.kind == WCOKind::WCO_VIEW_CHECK { Member::CheckView } else { Member::CheckRls });
            checks.push(CheckOpt { target: t as u16, kind: w.kind, relname: w.relname, polname: w.polname, qual, cascaded: w.cascaded });
        }
    }
    Ok(checks)
}

/// What the RETURNING list reads past the written row: the OLD / NEW images (a `Var`'s `varreturningtype`, or a
/// view's `ReturningExpr`), `merge_action()`.
fn returning_of<'p>(mt: &'p ModifyTable<'p>, returning: Option<&'p NodeList<'p>>, c: &mut Carried) -> Result<(OldNew<'p>, bool), Refusal> {
    let mut on = OldNew { old_alias: mt.returningOldAlias, new_alias: mt.returningNewAlias, old: false, new: false };
    let mut action = false;
    let Some(r) = returning else { return Ok((on, false)) };
    c.add(Member::Returning);
    for n in r.iter() {
        crate::expr::walk(n, &mut |x| {
            match x.node_tag() {
                NodeTag::T_ReturningExpr => {
                    if x.as_returning_expr().is_some_and(|e| e.retold) {
                        on.old = true;
                    } else {
                        on.new = true;
                    }
                }
                NodeTag::T_MergeSupportFunc => action = true,
                NodeTag::T_Var => match x.as_var().map(|v| v.varreturningtype) {
                    Some(VarReturningType::VAR_RETURNING_OLD) => on.old = true,
                    Some(VarReturningType::VAR_RETURNING_NEW) => on.new = true,
                    _ => {}
                },
                _ => {}
            }
            true
        });
    }
    if on.old {
        c.add(Member::ReturningOld);
    }
    if on.new {
        c.add(Member::ReturningNew);
    }
    if action {
        c.add(Member::MergeActionFn);
    }
    Ok((on, action))
}

/// IN-CONFLICT: whether `n` (an implicit-AND list's clauses too) reads SubPlan `plan_id`; a node the walk does not open
/// counts.
fn reads_sub(n: Node<'_>, plan_id: i32) -> bool {
    sub_read(n, plan_id, true)
}

/// Whether `n` reads SubPlan `plan_id`, a node the walk does not open counting as `unknown` (D1112: an action's read is
/// found, never assumed).
fn sub_read(n: Node<'_>, plan_id: i32, unknown: bool) -> bool {
    crate::walk::stack_check(); // D3748
    if let Some(l) = n.as_list() {
        return l.iter().any(|x| sub_read(x, plan_id, unknown));
    }
    let mut found = false;
    let known = crate::expr::walk(n, &mut |x| {
        found |= x.as_sub_plan().is_some_and(|s| s.plan_id == plan_id);
        !found
    });
    found || !known && unknown
}

/// UNION-COPY (`WriteDesc::copy_arms`): each COPY mark over a subquery RTE with appendrel members (the
/// statement's `AppendRelInfo`s, a pulled-up UNION ALL leaf already renumbered to its relation by the planner), with
/// every member under it to the leaves.
fn union_members<'p>(b: &Builder<'p, '_>, marks: &[RowMark]) -> ArenaVec<(u32, u32)> {
    let mut out = ArenaVec::new_in(b.arena);
    let infos = || b.stmt.appendRelations.iter().filter_map(|n| n.as_append_rel_info());
    let subquery = |rti: u32| b.rte(rti).is_some_and(|r| r.rtekind == types_nodes::parsenodes::RTEKind::RTE_SUBQUERY);
    for m in marks.iter().filter(|m| m.mark == RowMarkType::ROW_MARK_COPY && subquery(m.rti)) {
        // breadth first over `out` itself: the parent's members, then each member's
        let mut i = out.len();
        let mut p = m.rti;
        loop {
            for a in infos() {
                if a.parent_relid == p && a.child_relid != p && !out.iter().any(|e: &(u32, u32)| e.1 == a.child_relid) {
                    out.push((m.rti, a.child_relid));
                }
            }
            let Some(e) = out.get(i) else { break };
            p = e.1;
            i += 1;
        }
    }
    out
}

/// D3477 (UNION-COPY, settled): for each COPY mark with appendrel members (`union_members`), the plan node of every Append
/// whose `apprelids` hold the mark's relation (graph `copy_unions`). D3497: a mark no Append holds (its quals left one live
/// member, whose one-child Append setrefs dropped) names that member's own node instead (graph `copy_one`): the one
/// SubqueryScan or heap scan of a member's range table index.
fn copy_unions<'p>(b: &mut Builder<'p, '_>, marks: &[RowMark], arms: &[(u32, u32)]) {
    let mcx = b.mcx;
    let mut found: PgVec<'_, (u32, i32)> = PgVec::new_in(mcx);
    let mut one: PgVec<'_, (u32, i32)> = PgVec::new_in(mcx);
    let scanrelid = |n: Node<'p>| match n.node_tag() {
        NodeTag::T_SubqueryScan => n.as_subquery_scan().map(|s| s.scan.scanrelid),
        NodeTag::T_SeqScan => n.as_seq_scan().map(|s| s.scan.scanrelid),
        NodeTag::T_IndexScan => n.as_index_scan().map(|s| s.scan.scanrelid),
        NodeTag::T_BitmapHeapScan => n.as_bitmap_heap_scan().map(|s| s.scan.scanrelid),
        _ => None,
    };
    for m in marks.iter().filter(|m| m.mark == RowMarkType::ROW_MARK_COPY && arms.iter().any(|e| e.0 == m.rti)) {
        let (mut held, mut lone): (bool, PgVec<'_, i32>) = (false, PgVec::new_in(mcx));
        crate::rerun::each_plan_node(b.stmt, &mut |n| {
            if let Some(a) = n.as_append().filter(|a| a.apprelids.is_member(m.rti as i32)) {
                found.push_in((m.rti, a.plan.plan_node_id));
            }
            held |= n.as_append().is_some_and(|a| a.apprelids.is_member(m.rti as i32)) || n.as_merge_append().is_some_and(|a| a.apprelids.is_member(m.rti as i32));
            if scanrelid(n).is_some_and(|r| arms.iter().any(|e| e.0 == m.rti && e.1 == r)) {
                lone.push_in(n.as_plan().map_or(-1, |p| p.plan_node_id));
            }
        });
        if let ([node], false) = (&lone[..], held) {
            one.push_in((m.rti, *node));
        }
    }
    for f in found {
        b.graph.copy_unions.push(f);
    }
    for f in one {
        b.graph.copy_one.push(f);
    }
}

/// D3477 (settled MERGE): for each COPY mark with appendrel members (`union_members`), the child columns that are the
/// subquery's own (graph `copy_union_cols`): PostgreSQL's ExecMergeMatched acts with the ORIGINAL source row after its
/// EvalPlanQual re-ran the members, and the mark's stored record holds that row's columns by number. A child column that
/// reads the subquery but is no plain column of it (a PlaceHolderVar, an expression) leaves the one entry resno -1.
/// D3496: an UPDATE's or DELETE's RETURNING (`whole` false) reads the ORIGINAL row too (ExecProcessReturning's planSlot)
/// through the plain columns alone; its SET expressions read the re-joined row, so they leave no -1 entry.
fn copy_union_cols<'p>(b: &mut Builder<'p, '_>, plan: &'p Plan<'p>, marks: &[RowMark], arms: &[(u32, u32)], whole: bool) {
    let Some(child) = plan.lefttree.and_then(|c| c.as_plan()) else { return };
    let mcx = b.mcx;
    let mut found: PgVec<'_, (u32, i16, i16, u32)> = PgVec::new_in(mcx);
    for m in marks.iter().filter(|m| m.mark == RowMarkType::ROW_MARK_COPY && arms.iter().any(|e| e.0 == m.rti)) {
        let mut odd = false;
        let at = found.len();
        for te in child.targetlist.iter().filter_map(|n| n.as_target_entry()) {
            match te.expr.as_var().filter(|v| v.varlevelsup == 0 && v.varnosyn == m.rti) {
                Some(v) if v.varattnosyn > 0 => found.push_in((m.rti, te.resno, v.varattnosyn, v.vartype)),
                // the mark's own whole row (`wholerow{rowmarkId}`)
                Some(_) => {}
                None => {
                    let mut hit = false;
                    let known = crate::expr::walk(te.expr, &mut |n| {
                        hit |= n.as_var().is_some_and(|v| v.varnosyn == m.rti) || n.node_tag() == NodeTag::T_PlaceHolderVar;
                        !hit
                    });
                    odd |= hit || !known;
                }
            }
        }
        if odd && whole {
            found.truncate(at);
            found.push_in((m.rti, -1, 0, 0));
        }
    }
    for f in found {
        b.graph.copy_union_cols.push(f);
    }
}

/// D1685: a MERGE with a join condition (NOT MATCHED BY SOURCE actions; PostgreSQL 18.6's ExecMergeMatched then decides a
/// rechecked row by that condition over the newest target version and the ORIGINAL source row, its stored COPY row) whose
/// COPY mark is a flattened UNION ALL (`union_members`) hashed (its Append the child of a Hash node, so the target probes
/// and the target's row is the recheck's first): the union's record row (`union_record_row`), so the recheck hands the
/// hash build that stored row (lowerv4 rerun/epq/copy_row.rs). D3466: whatever its members (constant rows, an arm gated
/// off, a scanned relation under a qual, a nested union): the mark is the union's own and no member carries one, so
/// PostgreSQL's actions and join condition read the stored row whichever member made it (a bare relation member, which
/// the planner pulls into the union itself, still refuses: D3468). Any other form keeps no record row and refuses by
/// COPY_MARK.
fn union_copy_rows<'p>(b: &mut Builder<'p, '_>, marks: &[RowMark], arms: &[(u32, u32)]) {
    let mcx = b.mcx;
    let mut rows: PgVec<'_, (u32, &'p Plan<'p>)> = PgVec::new_in(mcx);
    for m in marks.iter().filter(|m| m.mark == RowMarkType::ROW_MARK_COPY) {
        if !arms.iter().any(|e| e.0 == m.rti) {
            continue;
        }
        let mut hashed = None;
        crate::rerun::each_plan_node(b.stmt, &mut |n| {
            let under = n.as_hash().and_then(|h| h.plan.lefttree).and_then(|c| c.as_append());
            if let Some(a) = under.filter(|a| a.apprelids.is_member(m.rti as i32)) {
                hashed = Some(&a.plan);
            }
        });
        rows.extend_in(hashed.map(|a| (m.rti, a)));
    }
    for (rti, a) in rows {
        crate::ops::shared::record_row::union_record_row(b, rti, a);
    }
}

pub(crate) fn marks_of<'p>(b: &Builder<'p, '_>, plan: &Plan<'p>, list: &'p NodeList<'p>, k: Kind, id: i32, c: &mut Carried, lock_plan: Option<&mut Option<WritePlan>>) -> Result<ArenaVec<RowMark>, Refusal> {
    let mut marks = ArenaVec::with_capacity_in(list.len(), b.arena);
    let mut lock_plan = lock_plan;
    let mut parent_lock = None;
    // L2 (V4R-248): the top parent of the mark that set the lock plan. Its later children (the same top parent, strength and
    // wait policy) are the one lock ExecLockRows takes in each row's own leaf, not row locks on several relations
    let mut family: Option<u32> = None;
    let mut empty = false;
    for m in list.iter() {
        let m = m.as_plan_row_mark().ok_or(Refusal::shape(k, id, "a rowMarks entry that is not a PlanRowMark"))?;
        let relid = b.rte(m.rti).map_or(0, |r| r.relid);
        let (lock, wait) = (lock_of(m.markType), wait_of(m.waitPolicy));
        // returning-019: a write's REFERENCE parent mark over a one-member family (an inheritance parent without children,
        // itself its one child) adds nothing: its child's mark carries the row (EvalPlanQualFetchRowMark by its ctid)
        let lone = lock_plan.is_none() && lock == LockKind::None && list.iter().filter_map(|x| x.as_plan_row_mark()).filter(|x| !x.isParent && x.prti == m.rti).count() == 1;
        if m.isParent && !lone {
            c.add(Member::ParentMark);
        }
        if m.isParent {
            if lock != LockKind::None && parent_lock.is_none() {
                parent_lock = Some(WritePlan::locking(relid, lock, wait));
            }
        } else if lock != LockKind::None {
            match lock_plan.as_deref_mut() {
                None => c.add(Member::LockingWriteMark),
                Some(slot) => {
                    let rel = b.relation(m.rti, k, id)?;
                    // D3379: a partitioned table planned without its children (none, or ONLY) is a dummy rel
                    // (set_rel_size), and a locked relation is never nullable: no row reaches LockRows
                    if rel.relkind == b'p' {
                        empty = true;
                    } else if rel.am != Am::Heap && !rel.rows {
                        // PGRCSERVE M3: a relation on the rows road locks by its AM's TIDs (the lock host's tableam lock)
                        return Err(Refusal::shape(k, id, "a row lock through a relation that is not a heap table"));
                    }
                    let sibling = m.prti != m.rti && family == Some(m.prti) && slot.as_ref().is_some_and(|p| p.lock == lock && p.wait == wait);
                    if slot.is_none() {
                        *slot = Some(WritePlan::locking(relid, lock, wait));
                        family = (m.prti != m.rti).then_some(m.prti);
                    } else if !sibling {
                        c.add(Member::MultiLock);
                    }
                }
            }
        }
        // D732: a COPY mark's row travels whole (`wholerow{rowmarkId}`), a REFERENCE mark's as its ctid
        let ctid_col = junk_mark_col(plan, if m.markType == RowMarkType::ROW_MARK_COPY { "wholerow" } else { "ctid" }, m.rowmarkId);
        let toid_col = if m.isParent || m.prti != m.rti { junk_mark_col(plan, "tableoid", m.rowmarkId) } else { 0 };
        marks.push(RowMark { rti: m.rti, relid, mark: m.markType, lock, wait, parent: m.isParent, prti: m.prti, ctid_col, toid_col });
    }
    // a parent with no child marks (no partitions yet): the parent's own lock is the plan's
    if let Some(slot) = lock_plan {
        if empty {
            *slot = None;
        } else if slot.is_none() {
            *slot = parent_lock;
        }
    }
    Ok(marks)
}

fn map_modify_table<'p>(b: &mut Builder<'p, '_>, mt: &'p ModifyTable<'p>) -> Result<Open<'p>, Refusal> {
    let (plan, k) = (&mt.plan, Kind::ModifyTable);
    let id = plan.plan_node_id;
    let mut c = Carried { set: MemberSet::EMPTY };
    let (cmd, m) = match mt.operation {
        CmdType::CMD_INSERT => (Cmd::Insert, Member::Insert),
        CmdType::CMD_UPDATE => (Cmd::Update, Member::Update),
        CmdType::CMD_DELETE => (Cmd::Delete, Member::Delete),
        CmdType::CMD_MERGE => (Cmd::Merge, Member::Merge),
        _ => return Err(Refusal::shape(k, id, "an unknown write operation")),
    };
    c.add(m);
    if !mt.fdwDirectModifyPlans.is_empty() {
        c.add(Member::DirectForeign);
    }
    if !plan.qual.is_nil() {
        return Err(Refusal::shape(k, id, "a qual on ModifyTable"));
    }
    let mut targets = targets_of(b, mt, plan, cmd, &mut c, k, id)?;
    let rti = targets[0].rti;
    let route = route_of(b, mt, &targets, &mut c, k, id)?;
    crate::ops::modifytable::partwrite::faces(b, &mut targets, route, k, id)?;
    let (leaves, leaves_known) = crate::ops::modifytable::route_leaves::leaves(b, route.as_ref(), cmd);
    let checks = checks_of(b, mt, &targets, &mut c, k, id)?;
    let tableoid_col = if targets.len() > 1 { junk_col(plan, "tableoid") } else { 0 };
    if targets.len() > 1 && tableoid_col == 0 {
        return Err(Refusal::shape(k, id, "a write over several result relations without the row's tableoid"));
    }
    let (set, returning) = (targets[0].set, targets[0].returning);
    let (old_new, returning_action) = returning_of(mt, returning, &mut c)?;
    let merge_in = if cmd == Cmd::Merge { Some(merge::merge_desc(b, mt, &targets, route, returning_action, &mut c, k, id)?) } else { None };
    if cmd == Cmd::Update {
        update_carry(b, &targets, set);
    }
    // D3681: twin result relations' scans read as one relation set, each row written to its own leaf (`set_write`)
    let at = set_write(b, mt, &targets, route.as_ref(), &c, cmd);
    let prev = core::mem::replace(&mut b.write_set, at);
    let open = b.child(plan, k);
    b.write_set = prev;
    let mut open = open?;
    let old_cols = update_carried(b, plan, k, id)?;
    let view_row = if targets[0].kind == TargetKind::View { targets[0].wholerow_col } else { 0 };
    let merge = match merge_in {
        Some(m) => Some(merge::merge_carried(b, plan, m, view_row, k, id)?),
        None => None,
    };
    let on_conflict = match mt.onConflictAction {
        0 => None,
        1 | 2 => {
            c.add(if mt.onConflictAction == 2 { Member::ConflictUpdate } else { Member::ConflictNothing });
            Some(OnConflict { update: mt.onConflictAction == 2, arbiter: &mt.arbiterIndexes, set: &mt.onConflictSet, cols: &mt.onConflictCols, filter: mt.onConflictWhere, excl_rti: mt.exclRelRTI })
        }
        _ => return Err(Refusal::shape(k, id, "an unknown ON CONFLICT action")),
    };
    if !mt.rowMarks.is_nil() {
        c.add(Member::WriteMarks);
    }
    let mark_descs = marks_of(b, &mt.plan, &mt.rowMarks, k, id, &mut c, None)?;
    let copy_arms = union_members(b, &mark_descs);
    copy_unions(b, &mark_descs, &copy_arms);
    if merge.is_some() || returning.is_some() {
        copy_union_cols(b, plan, &mark_descs, &copy_arms, merge.is_some());
    }
    if merge.as_ref().is_some_and(|m| m.join_cond.is_some()) {
        union_copy_rows(b, &mark_descs, &copy_arms);
    }
    serial_after(&mut open, Reason::Write);
    let (site, mut wplan) = (targets[0].site.clone(), targets[0].plan.clone());
    wplan.defer = cmd == Cmd::Insert && insert_defer(b, mt);
    open.push(Op::Write(WriteDesc { cmd, rti, site, plan: wplan, set, returning, on_conflict, epq: mt.epqParam, marks: &mt.rowMarks, merge, targets: targets.leak(), tableoid_col, checks: checks.leak(), route, leaves: leaves.leak(), leaves_known, old_new, mark_descs: mark_descs.leak(), copy_arms: copy_arms.leak(), members: c.set, old_cols: old_cols.leak() }));
    if let Some(r) = returning {
        ops::project(&mut open, r);
    }
    if !mt.rowMarks.is_nil() {
        b.graph.note(Kind::PlanRowMark);
    }
    b.graph.note(k);
    Ok(open)
}

/// PGRCSERVE M1 (`WritePlan::defer`, PGRCSERVE.md §1.4): not for a one-row source, RETURNING of a system column but tableoid,
/// or a per-row call that acts past itself (source, RETURNING, check options, the statement's other subplans).
fn insert_defer<'p>(b: &Builder<'p, '_>, mt: &'p ModifyTable<'p>) -> bool {
    let Some(src) = mt.plan.lefttree else { return false };
    if src.node_tag() == NodeTag::T_Result && src.as_plan().is_some_and(|p| p.lefttree.is_none()) {
        return false;
    }
    let sys = |n: Node<'p>| n.as_var().is_some_and(|v| v.varattno < 0 && v.varattno != -6 /* tableoid */);
    let entries = |ls: &'p NodeList<'p>| ls.iter().filter_map(|l| l.as_list()).flat_map(|l| l.iter());
    let returning: Vec<Node<'p>> = entries(&mt.returningLists).map(|e| e.as_target_entry().map_or(e, |te| te.expr)).collect(); // alloc: Cold
    if returning.iter().any(|&e| !crate::expr::walk(e, &mut |n| !sys(n))) {
        return false;
    }
    let mut exprs = returning;
    exprs.extend(entries(&mt.withCheckOptionLists).map(|w| w.as_with_check_option().map_or(Some(w), |c| c.qual)).flatten()); // alloc: Cold
    let acts = |f: crate::effects::found::Found| f.acts_past_call;
    if acts(crate::rerun::of_plan(b.stmt, b.facts, b.arena, src)) || exprs.iter().any(|&e| acts(crate::rerun::of_expr(b.stmt, b.facts, b.arena, e))) {
        return false;
    }
    let me = mt as *const ModifyTable<'_> as *const u8;
    !b.stmt.subplans.iter().flatten().any(|r| r.as_modify_table().is_none_or(|m| !core::ptr::eq(m as *const ModifyTable<'_> as *const u8, me)) && acts(crate::rerun::of_plan(b.stmt, b.facts, b.arena, r)))
}

/// D3681: the plan node id of the Append under top-level UPDATE / DELETE `mt` whose twin scans may be read as one relation
/// set (ops/append.rs `relsets`, which checks the scans: every child a twin of the first, no run-time pruning); -1 when the
/// write keeps one arm per result relation. The write: two or more result relations, every leaf of a partitioned root a
/// heap relation written alike (`Facts::write_twin` against the first: layout, CHECK constraints, defaults, indexes; no
/// triggers, rules or row security), one column map into the root (identical AppendRelInfo maps: `to_root`) and one SET
/// list; no RETURNING, check option, row mark, ON CONFLICT, direct foreign modify, InitPlan at the write or partition key
/// update (every row stays in its leaf). Each row's leaf is then its member's, as PostgreSQL's ExecModifyTable looks up
/// the result relation by the row's tableoid.
fn set_write(b: &Builder<'_, '_>, mt: &ModifyTable<'_>, targets: &[Target<'_>], route: Option<&RouteDesc>, c: &Carried, cmd: Cmd) -> i32 {
    let top = b.stmt.planTree.and_then(|n| n.as_modify_table()).is_some_and(|m| core::ptr::eq(m as *const ModifyTable<'_> as *const u8, mt as *const ModifyTable<'_> as *const u8));
    let Some(a) = mt.plan.lefttree.and_then(|n| n.as_append()) else { return -1 };
    let plain = matches!(cmd, Cmd::Update | Cmd::Delete) && top && targets.len() > 1 && a.appendplans.len() == targets.len() && route.is_some_and(|r| !r.cross_partition) && c.set.has(Member::Route) && !mt.partColsUpdated;
    let bare = mt.returningLists.is_nil() && mt.withCheckOptionLists.is_nil() && mt.rowMarks.is_nil() && mt.onConflictAction == 0 && mt.fdwDirectModifyPlans.is_empty() && mt.plan.initPlan.is_nil();
    if !plain || !bare || b.carry.is_some() {
        return -1;
    }
    let first = &targets[0];
    let same_set = |t: &Target<'_>| match (t.set, first.set) {
        (None, None) => true,
        (Some(x), Some(y)) => x.iter().eq(y.iter()),
        _ => false,
    };
    let alike = |t: &Target<'_>| t.kind == TargetKind::Heap && t.to_root.iter().eq(first.to_root.iter()) && same_set(t);
    if !alike(first) || !targets[1..].iter().all(|t| alike(t) && b.facts.write_twin(first.relid, t.relid)) {
        return -1;
    }
    a.plan.plan_node_id
}

/// An UPDATE's old row, noted for the child walk (R12-L05, the ladder's `widen`): every attribute of the one heap target
/// (PGRCSERVE M3: or table AM target, whose scan is on the rows road) the SET list does not assign (dropped attributes have
/// no facts and stay out). Nothing is carried for a view (its old row is the wholerow junk column), for several result
/// relations (write.route's), or without a node allocator; the consumer then finds `old_cols` short and refuses by name.
fn update_carry<'p>(b: &mut Builder<'p, '_>, targets: &[Target<'p>], set: Option<&IntList<'_>>) {
    let [t] = targets else { return };
    if !matches!(t.kind, TargetKind::Heap | TargetKind::Am) || b.alloc.is_none() {
        return;
    }
    let natts = b.facts.rel(t.relid).map_or(0, |r| r.natts.max(0));
    let mut attnos = ArenaVec::with_capacity_in(natts as usize, b.arena);
    for a in 1..=natts {
        if b.facts.att(t.relid, a).is_some() && !set.is_some_and(|s| s.iter().any(|x| x == a as i32)) {
            attnos.push(a);
        }
    }
    b.carry = Some(crate::ops::shared::carry::Carry { update: true, rti: t.rti, relid: t.relid, attnos, by_node: ArenaVec::new_in(b.arena), rowid_from: -1 });
}

/// After an UPDATE's child: the carried attributes' resnos in the child's projection. A child Result with no input (the
/// planner's dummy rel under a constant-false filter) carries nothing: its row never comes.
fn update_carried<'p>(b: &mut Builder<'p, '_>, plan: &'p Plan<'p>, k: Kind, id: i32) -> Result<ArenaVec<(i16, i16)>, Refusal> {
    let mut out = ArenaVec::new_in(b.arena);
    let Some(carry) = b.carry.take_if(|c| c.update) else { return Ok(out) };
    let child = plan.lefttree.and_then(|c| c.as_plan()).ok_or(Refusal::shape(k, id, "UPDATE without a child"))?;
    match carry.by_node.iter().find(|(n, _)| *n == child.plan_node_id).map(|(_, r)| r) {
        Some(r) if r.len() == carry.attnos.len() => out.extend(carry.attnos.iter().copied().zip(r.iter().copied())),
        _ if carry.attnos.is_empty() || child.lefttree.is_none() && plan.lefttree.is_some_and(|c| c.as_result().is_some()) => {}
        _ => return Err(Refusal::shape(k, id, "UPDATE: the target's old columns did not reach the write op")),
    }
    Ok(out)
}

const fn lock_of(m: RowMarkType) -> LockKind {
    match m {
        RowMarkType::ROW_MARK_EXCLUSIVE => LockKind::Exclusive,
        RowMarkType::ROW_MARK_NOKEYEXCLUSIVE => LockKind::NoKeyExclusive,
        RowMarkType::ROW_MARK_SHARE => LockKind::Share,
        RowMarkType::ROW_MARK_KEYSHARE => LockKind::KeyShare,
        RowMarkType::ROW_MARK_REFERENCE | RowMarkType::ROW_MARK_COPY => LockKind::None,
    }
}

const fn wait_of(w: LockWaitPolicy) -> WaitPolicy {
    match w {
        LockWaitPolicy::LockWaitBlock => WaitPolicy::Block,
        LockWaitPolicy::LockWaitSkip => WaitPolicy::Skip,
        LockWaitPolicy::LockWaitError => WaitPolicy::Error,
    }
}

/// The statement's marks when no LockRows / ModifyTable consumes them: each is a PlanRowMark.
pub fn map_marks(b: &mut Builder<'_, '_>) -> Result<(), Refusal> {
    if b.stmt.rowMarks.iter().any(|m| m.as_plan_row_mark().is_none()) {
        return Err(Refusal::shape(Kind::PlanRowMark, STMT_NODE, "a rowMarks entry that is not a PlanRowMark"));
    }
    Ok(())
}

pub fn map<'p>(b: &mut Builder<'p, '_>, node: Node<'p>, k: Kind) -> Result<Open<'p>, Refusal> {
    match k {
        Kind::ModifyTable => map_modify_table(b, node.as_modify_table().unwrap()),
        Kind::LockRows => lockrows::map_lock_rows(b, node.as_lock_rows().unwrap()),
        _ => Err(Refusal::shape(k, STMT_NODE, "a PlanRowMark is attached to a LockRows / ModifyTable, not a plan node")),
    }
}

/// Every family member a graph carries, across its descs (the write and lock ops' own sets, into a
/// nested inner): the matrix's row hits.
pub fn members_of(g: &PipelineGraph<'_>) -> MemberSet {
    let mut s = MemberSet::EMPTY;
    for p in g.pipelines.iter() {
        s = s.with_node(p);
    }
    s
}

/// The written relations' indexes whose constraint facts the facts source did not carry (`known` =
/// false): zero over the corpus (`tests/write_matrix.rs`) — an unclassified index fails the test.
pub fn unclassified(g: &PipelineGraph<'_>) -> usize {
    let writes = g.pipelines.iter().flat_map(|p| p.ops.iter()).filter_map(|o| if let Op::Write(d) = o { Some(d) } else { None });
    writes.flat_map(|d| d.targets.iter()).flat_map(|t| t.indexes.iter()).filter(|i| !i.known).count()
}

impl MemberSet {
    fn with_node(mut self, p: &crate::graph::PipelineNode<'_>) -> MemberSet {
        crate::walk::stack_check(); // D3748
        for op in p.ops.iter() {
            self = match op {
                Op::Write(d) => MemberSet(self.0 | d.members.0),
                Op::Lock(d) => MemberSet(self.0 | d.members.0),
                Op::SubPlan(d) => match &d.form {
                    crate::ops::shared::subplan::SubForm::Nested { inner } => self.with_node(inner),
                    _ => self,
                },
                Op::Join(crate::join::JoinDesc::Nest { inner, .. }) => self.with_node(inner),
                _ => self,
            };
        }
        self
    }
}

impl WriteDesc<'_> {
    /// IN-CONFLICT (modifytable): whether SubPlan `plan_id` is read by this INSERT's ON CONFLICT DO UPDATE (a SET entry or
    /// the WHERE) and by none of the write's other lists (its checks, RETURNING, a MERGE action): ExecOnConflictUpdate
    /// evaluates it over the existing row, where its ExecQual / ExecProject reaches it.
    pub fn conflict_only(&self, plan_id: i32) -> bool {
        let Some(oc) = self.on_conflict.as_ref().filter(|oc| oc.update && self.cmd == Cmd::Insert) else { return false };
        let read = oc.set.iter().any(|n| reads_sub(n, plan_id)) || oc.filter.is_some_and(|f| reads_sub(f, plan_id));
        let other = self.checks.iter().any(|c| reads_sub(c.qual, plan_id)) || self.returning.is_some_and(|r| r.iter().any(|n| reads_sub(n, plan_id))) || self.merge.is_some();
        read && !other
    }

    /// D1112 (writesubplan, nest.subplan B4's MERGE part): whether SubPlan `plan_id` is read by this MERGE's actions (a WHEN
    /// qual or a target list) and by none of the write's other lists (its checks, RETURNING): ExecMergeMatched /
    /// ExecMergeNotMatched evaluate it over the action's row, where its ExecQual / ExecProject reaches it.
    pub fn action_only(&self, plan_id: i32) -> bool {
        let Some(m) = self.merge.as_ref() else { return false };
        let (list, found) = (|l: &NodeList<'_>| l.iter().any(|n| reads_sub(n, plan_id)), |l: &NodeList<'_>| l.iter().any(|n| sub_read(n, plan_id, false)));
        let read = m.actions.iter().any(|a| found(a.tlist) || a.quals.is_some_and(found));
        let other = self.checks.iter().any(|c| reads_sub(c.qual, plan_id)) || self.returning.is_some_and(list);
        read && !other
    }

    /// Whether one of the write's own lists reads SubPlan `plan_id`: a pre-write one (`refers`) or the RETURNING list.
    pub fn reads_own(&self, plan_id: i32) -> bool {
        refers(self, plan_id) || self.returning.is_some_and(|r| r.iter().any(|n| reads_sub(n, plan_id)))
    }

    pub fn dump(&self, out: &mut dyn Write) -> fmt::Result {
        write!(out, "  write {} rel={} rti={} natts={} indexes=(", self.cmd.tag(), self.plan.target_rel, self.rti, self.site.natts())?;
        dump_indexes(&self.targets[0], out)?;
        write!(out, ")")?;
        if let Some(s) = self.set {
            write!(out, " set=(")?;
            for (i, a) in s.iter().enumerate() {
                write!(out, "{}{a}", if i > 0 { "," } else { "" })?;
            }
            write!(out, ")")?;
        }
        if let Some(c) = &self.on_conflict {
            write!(out, " on_conflict={} arbiter=(", if c.update { "update" } else { "nothing" })?;
            for (i, o) in c.arbiter.iter().enumerate() {
                write!(out, "{}{o}", if i > 0 { "," } else { "" })?;
            }
            write!(out, ")")?;
            if !c.set.is_nil() {
                write!(out, " excl_rti={} cols=(", c.excl_rti)?;
                for (i, a) in c.cols.iter().enumerate() {
                    write!(out, "{}{a}", if i > 0 { "," } else { "" })?;
                }
                write!(out, ") set=")?;
                crate::expr::args(out, c.set)?;
            }
            if let Some(f) = c.filter {
                write!(out, " where=")?;
                crate::expr::print(f, out)?;
            }
        }
        if self.returning.is_some() {
            write!(out, " returning")?;
            if self.old_new.old {
                write!(out, " old={}", self.old_new.old_alias.unwrap_or("old"))?;
            }
            if self.old_new.new {
                write!(out, " new={}", self.old_new.new_alias.unwrap_or("new"))?;
            }
        }
        if self.epq != 0 {
            write!(out, " epq=$exec{}", self.epq)?;
        }
        if !self.marks.is_nil() {
            write!(out, " marks={}", self.marks.len())?;
            dump_marks(&self.mark_descs, out)?;
        }
        if self.targets.len() > 1 {
            write!(out, " tableoid=outer.{}", self.tableoid_col)?;
        }
        if let Some(r) = &self.route {
            write!(out, " route=rti{}:{} natts={}{}", r.root_rti, r.root_relid, r.root_natts, if r.cross_partition { " cross-partition" } else { "" })?;
        }
        for (i, t) in self.targets.iter().enumerate() {
            if i > 0 {
                write!(out, "\n    target rti={} rel={} natts={} indexes=(", t.rti, t.relid, t.site.natts())?;
                dump_indexes(t, out)?;
                write!(out, ")")?;
                if let Some(s) = t.set {
                    write!(out, " set=(")?;
                    for (j, a) in s.iter().enumerate() {
                        write!(out, "{}{a}", if j > 0 { "," } else { "" })?;
                    }
                    write!(out, ")")?;
                }
                if t.returning.is_some() {
                    write!(out, " returning")?;
                }
            }
            if t.kind == TargetKind::View {
                write!(out, " view instead={}", t.instead)?;
                if t.wholerow_col != 0 {
                    write!(out, " wholerow=outer.{}", t.wholerow_col)?;
                }
            }
            if !t.to_root.is_empty() && !crate::ops::modifytable::partwrite::identity(t) {
                write!(out, " map=(")?;
                for (j, a) in t.to_root.iter().enumerate() {
                    write!(out, "{}{a}", if j > 0 { "," } else { "" })?;
                }
                write!(out, ")")?;
            }
            if !t.generated.is_empty() {
                write!(out, " generated=(")?;
                for (j, (a, g)) in t.generated.iter().enumerate() {
                    write!(out, "{}{a}{}", if j > 0 { "," } else { "" }, if *g == b'v' { "v" } else { "s" })?;
                }
                write!(out, ")")?;
            }
            if i > 0 {
                if let Some(m) = &t.merge {
                    if let Some(j) = m.join_cond {
                        write!(out, " join_cond=")?;
                        crate::expr::args(out, j)?;
                    }
                    dump_actions(&m.actions, "      ", out)?;
                }
            }
        }
        for c in self.checks.iter() {
            let kind = match c.kind {
                WCOKind::WCO_VIEW_CHECK => "view",
                WCOKind::WCO_RLS_INSERT_CHECK => "rls insert",
                WCOKind::WCO_RLS_UPDATE_CHECK => "rls update",
                WCOKind::WCO_RLS_CONFLICT_CHECK => "rls conflict",
                WCOKind::WCO_RLS_MERGE_UPDATE_CHECK => "rls merge update",
                WCOKind::WCO_RLS_MERGE_DELETE_CHECK => "rls merge delete",
            };
            write!(out, "\n    check {kind}")?;
            if self.targets.len() > 1 {
                write!(out, " target={}", c.target)?;
            }
            if let Some(n) = c.relname {
                write!(out, " rel={n}")?;
            }
            if let Some(n) = c.polname {
                write!(out, " policy={n}")?;
            }
            if c.cascaded {
                write!(out, " cascaded")?;
            }
            write!(out, " ")?;
            match c.qual.as_list() {
                Some(l) => crate::expr::args(out, l)?,
                None => crate::expr::print(c.qual, out)?,
            }
        }
        if let Some(m) = &self.merge {
            write!(out, "\n    merge rowid=outer.{} target_cols=(", m.rowid_col)?;
            for (i, (a, r)) in m.target_cols.iter().enumerate() {
                write!(out, "{}{a}@{r}", if i > 0 { "," } else { "" })?;
            }
            write!(out, ")")?;
            if m.returning_action {
                write!(out, " returning merge_action()")?;
            }
            if let Some(j) = m.join_cond {
                write!(out, " join_cond=")?;
                crate::expr::args(out, j)?;
            }
            dump_actions(&m.actions, "    ", out)?;
        }
        Ok(())
    }
}

fn dump_actions(actions: &[MergeAction<'_>], indent: &str, out: &mut dyn Write) -> fmt::Result {
    for a in actions.iter() {
        let kind = match a.kind {
            MergeMatchKind::MERGE_WHEN_MATCHED => "matched",
            MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_SOURCE => "not matched by source",
            MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_TARGET => "not matched",
        };
        write!(out, "\n{indent}when {kind}")?;
        if let Some(q) = a.quals {
            write!(out, " and ")?;
            crate::expr::args(out, q)?;
        }
        match a.cmd {
            None => write!(out, " then do nothing")?,
            Some(Cmd::Delete) => write!(out, " then delete")?,
            Some(Cmd::Insert) => {
                write!(out, " then insert ")?;
                crate::expr::args(out, a.tlist)?;
            }
            _ => {
                write!(out, " then update set=(")?;
                for (i, c) in a.set.iter().flat_map(|s| s.iter()).enumerate() {
                    write!(out, "{}{c}", if i > 0 { "," } else { "" })?;
                }
                write!(out, ") ")?;
                crate::expr::args(out, a.tlist)?;
            }
        }
    }
    Ok(())
}

/// `oid[keys]` + flags: `u` unique, `p` primary, `d` deferrable, `x` exclusion, `w` partial (where),
/// `e` expression key, `i` INCLUDE, `?` constraint facts unknown; a non-btree index names its AM.
pub fn dump_indexes(t: &Target<'_>, out: &mut dyn Write) -> fmt::Result {
    for (i, d) in t.indexes.iter().enumerate() {
        write!(out, "{}{}", if i > 0 { ", " } else { "" }, d.indexid)?;
        if let Some(ip) = t.plan.indexes().iter().find(|p| p.index_oid == d.indexid) {
            write!(out, "{:?}", ip.keys())?;
        } else {
            write!(out, "[{}]", d.nkeys)?;
        }
        for (flag, on) in [("u", d.unique), ("p", d.primary), ("d", d.deferrable), ("x", d.exclusion), ("w", d.partial), ("e", d.expression), ("i", d.include), ("?", !d.known)] {
            if on {
                write!(out, "{flag}")?;
            }
        }
        if d.am != IndexAm::Btree {
            write!(out, ":{}", d.am.name())?;
        }
    }
    Ok(())
}

fn dump_marks(marks: &[RowMark], out: &mut dyn Write) -> fmt::Result {
    write!(out, " (")?;
    for (i, m) in marks.iter().enumerate() {
        write!(out, "{}rti{}:{:?}{}", if i > 0 { ", " } else { "" }, m.rti, m.mark, if m.parent { " parent" } else { "" })?;
    }
    write!(out, ")")
}

impl LockDesc<'_> {
    pub fn dump(&self, out: &mut dyn Write) -> fmt::Result {
        write!(out, "  lock rel={} {:?} {:?} marks=(", self.plan.target_rel, self.plan.lock, self.plan.wait)?;
        for (i, m) in self.marks.iter().enumerate() {
            write!(out, "{}rti{}:{:?}{}", if i > 0 { ", " } else { "" }, m.rti, m.mark, if m.parent { " parent" } else { "" })?;
        }
        write!(out, ")")?;
        if self.epq != 0 {
            write!(out, " epq=$exec{}", self.epq)?;
        }
        Ok(())
    }
}

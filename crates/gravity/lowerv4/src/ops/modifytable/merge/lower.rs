//! Kind `write.merge` (L6, cards/M4.md; the ladder's `actions_of` / `lower_merge`). Owner M4-MERGE. PostgreSQL 18's
//! ExecMergeMatched for WHEN MATCHED actions: per row, each action's quals in order over the carried target row
//! (`MergeDesc::target_cols`), a later action's only when every earlier one failed; the first that holds runs. DO NOTHING
//! leaves the row; UPDATE forms the FULL new image (SET entries at their attnos, the carried old cells elsewhere) for ONE
//! `WRITE_UPDATE`; DELETE is ONE `WRITE_DELETE`. The host does the rest (hosts/src/write.rs, H-f V4R-239): the MERGE
//! USING checks on the target's current version, the statement triggers of every listed action, CMD_MERGE capture,
//! 21000 / 27000. NOT MATCHED [BY TARGET] (W6) and BY SOURCE (V4R-495) actions: write/merge_nm.rs.
//! Concurrency: Recheck reruns this program over the newest version (the actions restart from the first); a version that
//! no longer joins leaves the recheck empty (or, over a LEFT join, a BY SOURCE row), TM_Deleted the NOT MATCHED ones.
//! R2 (D655's form, marks.rs): a source joined by inner probes onto the target's own scan; its REFERENCE mark's ctid at
//! its producer's site lets the recheck refetch the row the join read. D776 (marks.rs RowOf, M4-LOCK V4R-275): the target
//! hashed on the build side, the marked source the emit's own scan: the write's position is the target's junk ctid, the
//! recheck names the target producer's scan site (rerun at the host's newest version) and the source's own scan takes
//! the row from its RowOf site. D789 (M4-RET): with RETURNING the write is a step (MergeStep) and each action's row goes
//! on through write.returning (`lower_merge`: the written row, OLD / NEW, `merge_action()`), before its sink; DO NOTHING
//! returns none. Refused by name (write/merge_admit.rs): a locking or parent mark (write.lock); a FULL, RIGHT or nested-loop
//! join (each by its kind); routed or several targets (write.route); an action SubPlan (nest.subplan); an INSTEAD OF
//! trigger or unknown trigger facts (write.transition's plan). A BEFORE ROW trigger fires in the host's action prologue.
use gravity_abi::ctx::{site_off, POS_ROW, POS_VALID};
use gravity_abi::program::{WRITE_DELETE, WRITE_UPDATE};
use gravity_abi::site::site;
use gravity_abi::status::{recheck, unsupported, UNSUPPORTED_SHAPE};
use gravity_decompose::catalog::typefacts::{self, TypeFacts};
use gravity_decompose::Op;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use gravity_program::{Cmd, ExprLife, ExprOut, Rep};
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};
use types_nodes::primnodes::{MergeMatchKind, SubLinkType};
use types_nodes::{Node, NodeTag};

use crate::cell::{Cell, Nullable, TIDOID};
use crate::face::end::{Arm, EndMech, LocFrom, RowOpDecl, RowOpMaker, RowOpMech, RowOpRow, SinkKind, SYS_CELLS};
use crate::face::kinds::KindId;
use crate::face::step::{ExprFace, StepMech};
use crate::frame::env::Env;
use crate::frame::instr::{self, Ctr};
use crate::frame::frame::{EndMaker, Next, StepMaker};
use crate::frame::iface::{expr_face, kind_impl};
use crate::ops::modifytable::face::{RetMaker, RetMech};
use crate::frame::px::{ExId, Px};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::lx::CellRef;
use crate::admit::refusal::Reason;
use crate::ops::Table;
use crate::rep;

use crate::ops::modifytable::merge::admit::{admit, shape};
use crate::ops::modifytable::merge::view::ViewRow;

/// The oid type (a junk tableoid's: mergecmd G2(c)).
const OIDOID: u32 = 26;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::WriteMerge, &EndMaker(make_end)), (KindId::WriteMerge, &StepMaker(make_step))], ..crate::ops::NONE };

/// The write op in the chain: the end does the write (without RETURNING, the only form served here).
struct WriteStep;

impl StepMech for WriteStep {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        next(b, env, row)
    }
}

fn make_step<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    // D789: with RETURNING the step writes and hands each action's row to write.returning (the end is its sink)
    match &px.gx.pipelines()[k].ops[j] {
        Op::Write(w) if w.returning.is_some() => mcx::pg_box_dyn!(px.mcx(), MergeStep(merge_of(px, k)) => dyn StepMech + 'g),
        _ => mcx::pg_box_dyn!(px.mcx(), WriteStep => dyn StepMech + 'g),
    }
}

/// D789: MERGE .. RETURNING's write, as a step before write.returning's sink.
struct MergeStep<'g>(MergeEnd<'g>);

impl StepMech for MergeStep<'_> {
    fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _x: &Exits) {
        self.0.op.open(b, env);
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        self.0.run(b, env, row, Some(next))
    }

    fn wants_locator(&self) -> bool {
        true
    }
}

/// A target attribute: the child's resno carrying it, its type facts and its cell form.
pub(super) struct Att {
    pub(super) resno: i16,
    tf: TypeFacts,
    out: ExprOut,
}

enum Act<'g> {
    Nothing,
    /// Each SET entry: its attno's index, its compiled expression, the form `emit` gives it in; then the action's MULTIEXPR
    /// SubPlans (merge#340), emitted before the entries.
    Update(PgVec<'g, (usize, ExId, ExprOut)>, PgVec<'g, ExId>),
    Delete,
}

/// One WHEN MATCHED action: its quals, ANDed in order, and what it does.
pub(super) struct Action<'g> {
    quals: PgVec<'g, ExId>,
    act: Act<'g>,
}

pub(super) struct MergeEnd<'g> {
    pub(super) op: PgBox<'g, dyn RowOpMech + 'g>,
    /// Per attno (index `a - 1`): the carried attribute, `None` for a dropped or uncarried one.
    pub(super) atts: PgVec<'g, Option<Att>>,
    pub(super) actions: PgVec<'g, Action<'g>>,
    pub(super) ex: &'static ExprFace,
    pub(super) node: i32,
    /// The ModifyTable node (the write op's), whose EXPLAIN ANALYZE counts the rows each kind of action wrote.
    pub(super) mt: i32,
    /// D776: the target's ctid junk column and the tid type's facts when the target is hashed (`LocFrom::JunkCtid`).
    tid: Option<(i16, TypeFacts)>,
    /// mergecmd RIGHT_OWN: the same column read only as the NOT MATCHED test, the position the driving scan's
    /// (`LocFrom::Driving`): a RIGHT merge join whose outer is the target's own scan, its inner fill's rows NULL there.
    test: Option<(i16, TypeFacts)>,
    /// D789: write.returning's continuation when the MERGE has RETURNING (the step writes).
    pub(super) ret: Option<PgBox<'g, dyn RetMech + 'g>>,
    /// V4R-353: a view target's wholerow old row (write/merge_view.rs); its cells end with the wholerow.
    view: Option<ViewRow<'g>>,
    /// W6: the NOT MATCHED [BY TARGET] actions (a NULL target ctid: write/merge_nm.rs).
    pub(super) nm: PgVec<'g, crate::ops::modifytable::merge::nm::Nm<'g>>,
    /// V4R-495: the NOT MATCHED BY SOURCE actions and the join condition splitting them from MATCHED (merge_nm.rs `split`).
    pub(super) bs: PgVec<'g, Action<'g>>,
    pub(super) cond: PgVec<'g, ExId>,
    /// W6-MERGE-ROUTE-MATCHED (b): an UPDATE's cells, the statement's updated columns by attribute index (merge_route.rs).
    pick: Option<PgVec<'g, usize>>,
    /// G2(c): several result relations whose rows come from one settled union of their feeds (route.rs
    /// `union_targets`): the junk tableoid column, the oid type's facts, and each target's relation and feed site.
    union: Option<(i16, TypeFacts, PgVec<'g, (u32, usize)>)>,
    /// G2(c), RETURNING under the union form: per result relation (its relid), each attribute of the first one's
    /// (the RETURNING list's: decompose's write carries returningLists[0]) as that relation's cell index, by root column
    /// (None: dropped there). A row's written cells are its own relation's layout (hosts MultiTargetHost `written`).
    ret_map: Option<PgVec<'g, (u32, PgVec<'g, Option<usize>>)>>,
    /// SORT-UNION: the site's cell count beside the routed INSERT (the root's row, route.rs `plan`), an UPDATE's
    /// SET cells first and NULL after; 0 otherwise.
    pad: usize,
    /// writesubplan (gen mg-0178): the written row's system columns the host hands back after the site's cells (D774,
    /// `WritePlan.sys`): SYS_CELLS under RETURNING of a MERGE through a root's union form that reads them, else 0.
    sys: usize,
    /// writesubplan (gen mg-0179): the attribute count of the result relation RETURNING projects (`write.returning`'s
    /// target: the first one's, an arm's own under the (b) form), the written row's first cells it reads.
    ret_n: usize,
    /// D655 Row C: the redo in the IR when the write rechecks there (merge/epq.rs).
    redo: Option<crate::ops::modifytable::merge::epq::Redo<'g>>,
    /// D3477: a settled union's source, which the recheck's first joined row acts with as stored (merge/settled.rs).
    settled: Option<crate::ops::modifytable::merge::settled::Settled<'g>>,
}

fn make_end<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let p = &px.gx.pipelines()[k];
    if p.ops.iter().any(|o| matches!(o, Op::Write(w) if w.returning.is_some())) {
        return (kind_impl::<EndMaker>(&p.node_id, KindId::WriteReturning).0)(px, k);
    }
    mcx::pg_box_dyn!(px.mcx(), merge_of(px, k) => dyn EndMech + 'g)
}

/// mergecmd insert-only nest / D2247: pipeline `k`'s insert-only MERGE `w` whose write reads no target position: over a
/// nested loop (nest_write.rs `positionless`), or in a spooled drain (`a2a_merge`: the probe's joined rows spooled, then
/// the drained records, the source rows the join left unmatched). mergecmd RIGHT-MERGE: or over a RIGHT merge join (the
/// target's own scan or Sort its outer: the source rows the inner fill hands on are NULL in the target's junk ctid).
/// D3367: or any MERGE whose target's side is the planner's dummy relation under a LEFT nest (nest_write.rs `dummy_side`:
/// every row NOT MATCHED, its MATCHED actions never run).
pub(crate) fn positionless(gx: &crate::graph::Gx<'_, '_>, k: usize, w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>, m: &gravity_decompose::ops::shared::modifytable::MergeDesc<'_>) -> bool {
    use gravity_decompose::join::JoinDesc;
    let joins = |o: &Op<'_>| matches!(o, Op::Join(JoinDesc::Nest { .. }) | Op::Join(JoinDesc::Merge { jointype: types_nodes::jointype::JoinType::JOIN_RIGHT, .. }));
    let nest = gx.pipelines()[k].ops.iter().take_while(|o| !matches!(o, Op::Write(_))).any(joins);
    crate::ops::modifytable::nest_write::positionless(w, m) && (nest || gx.a2a_write(k).is_some()) || crate::ops::modifytable::nest_write::dummy_side(gx, k)
}

impl crate::graph::Gx<'_, '_> {
    /// D2247: whether a drain pipeline's (`ops`) MERGE write `w` is row A2a's kind of write (hashjoin/drain/lower.rs
    /// `a2a_write` asks here): an insert-only MERGE into one heap table (nest_write.rs `positionless`: a MATCHED row has no
    /// action and a NOT MATCHED one inserts a new row, so no position is read and nothing rechecks), with no locking mark or
    /// row lock. The spooled form takes it as it takes a plain INSERT: PostgreSQL's MERGE inserts each not-matched source row
    /// as ExecInsert does, in the join's order, which the spooled form changes only for the drained rows (DECISIONS 6b).
    /// ROUTED-DRAIN (tw-part-023, gen mg-0181's form): or V4R-498's insert-only MERGE routed through a partitioned
    /// root (`positionless`'s routed form): each not-matched row, probed or drained, goes through the root's tuple routing
    /// (ExecInsert on rootRelInfo). A probe over several leaves is a union's arm: spool_admit.rs refuses it (D2132). Only
    /// in a drain (`ops` holding the Drain op): elsewhere the routed form keeps its own admission (nm.rs `routed_target`).
    pub(crate) fn a2a_merge(&self, ops: &[Op<'_>], w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>) -> bool {
        let drain = ops.iter().any(|o| matches!(o, Op::Join(gravity_decompose::join::JoinDesc::Drain { .. })));
        let heap = (w.route.is_none() || drain) && w.targets.first().is_some_and(|t| t.kind == gravity_decompose::ops::shared::modifytable::TargetKind::Heap);
        let plain = w.on_conflict.is_none() && w.mark_descs.iter().all(|m| m.lock == gravity_program::LockKind::None) && !ops.iter().any(|o| matches!(o, Op::Lock(_)));
        heap && plain && w.merge.as_ref().is_some_and(|m| crate::ops::modifytable::nest_write::positionless(w, m))
    }
}

/// The MERGE's actions and row op over pipeline `k` (the end's, or the step's under RETURNING, D789).
fn merge_of<'g>(px: &mut Px<'g, '_>, k: usize) -> MergeEnd<'g> {
    let mcx = px.mcx();
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Some((j, w)) = p.ops.iter().enumerate().find_map(|(j, o)| if let Op::Write(w) = o { Some((j, w)) } else { None }) else {
        crate::refuse!(node, Reason::Bug { payload: "a row-op end without its write op" })
    };
    let Some(m) = w.merge.as_ref().filter(|_| w.cmd == Cmd::Merge) else {
        crate::refuse!(node, Reason::Bug { payload: "write.merge's end over a write that is not a MERGE" })
    };
    let mt = p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(node, |n| n.node_id);
    instr::merge_split(mt); // D3500: the rows a recheck hands the actions beyond the one it rechecks (driver `fold_recheck`)
    let (relid, built) = admit(gx, k, w, m);
    crate::ops::modifytable::merge::route::transition(px, k, j, relid, w); // write.transition's plan (the target's triggers; routed, every target's)
    let natts = w.site.natts();
    let view = crate::ops::modifytable::merge::view::of(gx, k, j, w, node);
    let atts: PgVec<'g, Option<Att>> = (1..=natts as i16)
        .map(|a| {
            let f = gx.facts.att(relid, a)?;
            // V4R-353: a view's old columns are its wholerow's fields (resno 0); V4R-498: leaf attno `a` carried at its root's
            let resno = if view.is_some() { 0 } else { m.target_cols.iter().find(|c| c.0 == gravity_decompose::ops::modifytable::partwrite::root_attno(&w.targets[0], a))?.1 };
            let Some(tf) = typefacts::facts_of(gx.facts, f.typid) else { shape(node, "MERGE: a target column type the catalog does not know") };
            Some(Att { resno, tf, out: ExprOut { rep: Rep::of_storage(f.typlen, f.byval), typid: f.typid, life: ExprLife::Row } })
        })
        .collect_in(mcx);
    let carried: &[Option<Att>] = if view.is_some() { &[] } else { &atts };
    let full = (1..=natts as i16).all(|a| gx.facts.att(relid, a).is_none() || atts[a as usize - 1].is_some()); // D883: every live column carried
    // D3367: the planner's dummy relation is the write's whole input or the target's side under a LEFT nest (its join
    // condition folds to false or NULL; nest_write.rs `dummy_side`): no row is MATCHED, so no MATCHED / NOT MATCHED BY
    // SOURCE action ever runs, and the child carries nothing its quals and SET values read: they are not compiled, and an
    // UPDATE or DELETE lowers as the arm that writes no row and reads no position (its statement triggers are the write's,
    // from the plan's action list, not this arm's)
    let never = crate::ops::shared::dummy::dummy(gx, k, j) || crate::ops::modifytable::nest_write::dummy_side(gx, k);
    // mergecmd merge#192: one heap target's tableoid in a MATCHED action is its OID, a plan-time constant (the expression
    // compiles it as one while `StepExt::tableoid` is set: D774's RETURNING form, ret_sys.rs); D3467: the result relation's
    // own (a partitioned root's one leaf left, not the root). Over several heap result relations (a partitioned or
    // inherited target) a MATCHED or NOT MATCHED BY SOURCE row's target is the leaf its junk tableoid names, so the action
    // reads that column of the child's row (`INNER_VAR`, where the actions read the source): PostgreSQL's
    // ri_oldTupleSlot is fetched from that leaf and carries its OID
    let heap = w.targets.iter().all(|t| t.kind == gravity_decompose::ops::shared::modifytable::TargetKind::Heap);
    // a partitioned root holds no row of its own: its leaves' rows carry the junk tableoid
    let one = match w.targets {
        [t] if gx.facts.rel(t.relid).is_some_and(|f| f.relkind != b'p') => Some(t.relid),
        _ => None,
    };
    let tableoid = if view.is_some() || !heap {
        None
    } else if let Some(r) = one {
        Some((w.rti as i32, gravity_expr::TableOid::Rel(r)))
    } else if w.tableoid_col > 0 {
        Some((w.rti as i32, gravity_expr::TableOid::Col(gravity_expr::Source::Inner, w.tableoid_col)))
    } else {
        None
    };
    let sys = |n: Node<'_>| tableoid.is_some() && n.node_tag() == NodeTag::T_Var && n.as_var().is_some_and(|v| v.varlevelsup == 0 && v.varno == w.rti as i32 && v.varattno == -6);
    let check = |x: Node<'_>| {
        let mut why = None;
        let known = gravity_decompose::expr::walk(x, &mut |n| {
            why = why.or_else(|| if sys(n) { None } else { crate::ops::modifytable::merge::admit::reads(n, w.rti as i32, carried, full && view.is_none()) });
            why.is_none()
        });
        if let Some(why) = why {
            shape(node, why);
        }
        // D2046-ID: an identity column's NextValueExpr (an INSERT action's) is a leaf the expression compiles
        if !known && !crate::ops::modifytable::nest_write::seq_only(x) {
            shape(node, "MERGE: an action expression the walker does not open");
        }
    };
    let ex: &'static ExprFace = expr_face(&node);
    let (mut actions, mut bs) = (mcx::reserved_in(m.actions.len(), mcx), PgVec::new_in(mcx));
    crate::step::expr::ext(px).tableoid = tableoid;
    for a in m.actions.iter().filter(|a| matches!(a.kind, MergeMatchKind::MERGE_WHEN_MATCHED | MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_SOURCE)) {
        let mut quals = PgVec::new_in(mcx);
        for q in a.quals.iter().flat_map(|l| l.iter()).filter(|_| !never) {
            check(q);
            quals.push_in(ex.0.compile(px, q, None, node));
        }
        let act = match a.cmd {
            None => Act::Nothing,
            Some(Cmd::Update | Cmd::Delete) if never => Act::Nothing,
            Some(Cmd::Delete) => Act::Delete,
            Some(Cmd::Update) => {
                // merge#340 (writesubplan D1112): a multiple-column SET `(b, c) = (SELECT ..)` keeps its MULTIEXPR sublink as a
                // junk entry after the assignable ones (PG's updateColnos names the non-junk entries only). Correlated, it is a
                // SubPlan ExecBuildUpdateProjection runs in its setup steps, before any entry, its output Params feeding the
                // entries: lent with no row (nest.subplan's in_action read, graph.rs `lazy_nested_sub`), read here over the
                // action's row ahead of the SET entries, as IN-CONFLICT reads D1531's. Uncorrelated, it is an initplan and
                // the entry subselect.c's NULL record constant, a no-op: the entries read the initplan's Params.
                let mut multi = PgVec::new_in(mcx);
                let mut own = mcx::reserved_in(a.tlist.len(), mcx);
                for e in a.tlist.iter() {
                    let Some(te) = e.as_target_entry() else { crate::refuse!(node, Reason::Bug { payload: "a MERGE SET entry that is not a TargetEntry" }) };
                    if !te.resjunk {
                        own.push_in(te);
                        continue;
                    }
                    match te.expr.node_tag() {
                        NodeTag::T_SubPlan if te.expr.as_sub_plan().is_some_and(|s| s.subLinkType == SubLinkType::MULTIEXPR_SUBLINK) => {
                            check(te.expr);
                            multi.push_in(ex.0.compile(px, te.expr, None, node));
                        }
                        NodeTag::T_Const if te.expr.as_const().is_some_and(|c| c.constisnull) => {}
                        _ => crate::refuse!(node, Reason::Bug { payload: "a junk MERGE SET entry that is no MULTIEXPR sublink" }),
                    }
                }
                let Some(set) = a.set.filter(|s| s.len() == own.len()) else { crate::refuse!(node, Reason::Bug { payload: "a MERGE UPDATE whose SET values do not match its column list" }) };
                let mut sets = mcx::reserved_in(set.len(), mcx);
                for (te, attno) in own.into_iter().zip(set.iter()) {
                    let Some(Some(att)) = usize::try_from(attno - 1).ok().and_then(|i| atts.get(i)) else { shape(node, "MERGE: a SET column the child does not carry") };
                    check(te.expr);
                    let out = gx.out_of(te.expr).unwrap_or(att.out);
                    sets.push_in(((attno - 1) as usize, ex.0.compile(px, te.expr, None, node), out));
                }
                Act::Update(sets, multi)
            }
            Some(_) => crate::refuse!(node, Reason::Bug { payload: "a WHEN MATCHED action that is not UPDATE / DELETE / DO NOTHING" }),
        };
        if a.kind == MergeMatchKind::MERGE_WHEN_MATCHED { &mut actions } else { &mut bs }.push_in(Action { quals, act });
    }
    crate::step::expr::ext(px).tableoid = None;
    let cond = crate::ops::modifytable::merge::nm::cond(px, m, ex, node, &check);
    let routed = crate::ops::modifytable::merge::nm::routed(w); // V4R-498: INSERTs into the root, routed
    // SORT-UNION: or a NOT MATCHED INSERT beside UPDATE / DELETE actions over the result relations' sorted arms,
    // into the root (route.rs `insert_beside`)
    let beside = crate::ops::modifytable::merge::route::insert_beside(gx, k, w);
    // writesubplan (gen mg-0178): under RETURNING over the union form the INSERT's image is as wide as the site (route.rs
    // `ret_width`: the widest result relation's row; the route leg reads the root's cells, the first)
    let beside = beside.map(|(r, n)| (r, crate::ops::modifytable::merge::route::ret_width(gx, k, w).map_or(n, |x| x.max(n))));
    // writesubplan (gen mg-0178): RETURNING's system columns of the written row, the host's (ret_sys.rs `sys_site`)
    let sys_plan = crate::ops::modifytable::ret_sys::sys_plan_at(gx, k, w);
    let sys_cells = if sys_plan.is_some() { SYS_CELLS as usize } else { 0 };
    // mergecmd view NM: a view target's NOT MATCHED INSERT is its INSTEAD OF INSERT trigger's row (hosts view_write), over
    // any join: a NULL wholerow is the not-matched row (no ctid, no recheck). The host's view buffer holds the wholerow cell
    // after the view's columns only beside an UPDATE or DELETE action (write.rs bind: `view_rows` / `view_del`), so the
    // declaration and an INSERT's image (that cell NULL, unread) carry it then alone
    let whole_cell = view.is_some() && actions.iter().chain(bs.iter()).any(|a| matches!(a.act, Act::Update(..) | Act::Delete));
    // INHERIT-ROOT: a routed MERGE's RETURNING reads the root's written row at target 0's attnos (`to_root`);
    // writesubplan (gen mg-0178): so does a NOT MATCHED INSERT's beside UPDATE / DELETE actions (the route leg's row is the
    // root's: ExecInitMerge maps the first result relation's returningList onto rootRelInfo's attnos)
    let ret_at = if (routed.is_some() || beside.is_some()) && w.returning.is_some() { &w.targets[0].to_root[..] } else { &[][..] };
    // mergecmd insert-only nest (`positionless`): the source rows the join left unmatched are NULL in the target's junk
    // ctid, as a hashed target's are
    let free = built.is_none() && view.is_none() && positionless(gx, k, w, m);
    // mergecmd RIGHT_OWN (held-out subq-0184; merge/admit.rs): no built target under a RIGHT merge join is the target's own
    // scan its outer: a matched row's position is the driving scan's (the join hands it on at the outer's current row),
    // and the target's junk ctid, NULL in the inner fill's rows (ExecMergeMatched's "ctid is NULL" test), tells them
    let right = |o: &Op<'_>| matches!(o, Op::Join(gravity_decompose::join::JoinDesc::Merge { jointype: types_nodes::jointype::JoinType::JOIN_RIGHT, .. }));
    let own_right = built.is_none() && view.is_none() && !free && gx.pipelines()[k].ops.iter().take_while(|o| !matches!(o, Op::Write(_))).any(right);
    let nm = crate::ops::modifytable::merge::nm::actions(px, m, (routed.or(beside).unwrap_or((relid, natts + whole_cell as usize)), node, built.is_some() || view.is_some() || free || own_right), ex, &check, (ret_at, sys_cells));
    if !full && !never && actions.iter().chain(bs.iter()).any(|a| matches!(a.act, Act::Update(..))) {
        shape(node, "MERGE: a target column the child does not carry (no old row to form the new tuple from)");
    }
    // D776: the target hashed on the build side: the write's position is the target's junk ctid, and a recheck names the
    // target producer's scan site (the driver reruns that producer at the host's newest version; the marked source's own
    // scan takes its row from marks.rs's RowOf site)
    let test = if own_right { crate::ops::modifytable::merge::nm::junk_tid(gx, w, node) } else { None };
    let (loc, tid) = match built {
        None if view.is_some() => (LocFrom::None, None),
        // mergecmd insert-only nest: no position (a MATCHED row has no action), the junk ctid telling the matched rows
        None if free => (LocFrom::None, crate::ops::modifytable::merge::nm::junk_tid(gx, w, node)),
        None => (LocFrom::Driving, None),
        Some(_) if routed.is_some() => (LocFrom::None, crate::ops::modifytable::merge::nm::junk_tid(gx, w, node)),
        Some(q) => {
            let (Some(site), col) = (px.pipeline_site(q), w.targets[0].junk_col) else { crate::refuse!(node, Reason::Bug { payload: "a hashed MERGE target's producer without its scan site" }) };
            let Some(tf) = typefacts::facts_of(gx.facts, TIDOID).filter(|_| col > 0) else { crate::refuse!(node, Reason::Bug { payload: "a hashed MERGE target without its ctid junk column" }) };
            (LocFrom::JunkCtid { site }, Some((col, tf)))
        }
    };
    // G2(c): per row the relation is the row's junk tableoid and a recheck names its feed's scan site
    let union = match crate::ops::modifytable::merge::route::union_targets(gx, k, w) {
        Some(feeds) => {
            let sites = crate::ops::modifytable::update_built::feed_sites(px, &feeds[..], w);
            let (Some(sites), Some(of)) = (sites, typefacts::facts_of(gx.facts, OIDOID)) else { crate::refuse!(node, Reason::Bug { payload: "a union-fed MERGE target without its feed's scan site or the oid type" }) };
            Some((w.tableoid_col, of, sites))
        }
        None => None,
    };
    let ret_map = union.as_ref().filter(|_| w.returning.is_some()).map(|_| ret_map(mcx, w));
    // D789: RETURNING OLD reads the version each UPDATE action replaced, which the host fetches (R4a's old site)
    let plan = w.plan.copy_in(px.arena);
    let plan = if w.returning.is_some() && w.old_new.old { plan.with_old() } else { plan };
    let plan = match sys_plan {
        Some(header) => plan.with_sys(header),
        None => plan,
    };
    let (relid, n, plan, pick) = crate::ops::modifytable::merge::route::plan(px, k, (w, m), (relid, routed.map_or(natts, |r| r.1)), plan); // V4R-498, W6-MERGE-ROUTE-MATCHED
    let decl = RowOpDecl { op: WRITE_UPDATE, relid, natts: n as u16 + whole_cell as u16, plan, loc, conflict: false };
    // writesubplan (gen mg-0178): under RETURNING over the union form the site is as wide as the widest result relation
    // (route.rs `plan`): an UPDATE's SET cells first, the rest NULL, as beside the routed INSERT
    // (gen mg-0179: so is an arm's of the (b) form, route.rs `b_arm`)
    let pad = if beside.is_some() || crate::ops::modifytable::merge::route::ret_width(gx, k, w).is_some() { n } else { 0 };
    // writesubplan (gen mg-0179): RETURNING's row is its projection's result relation's (an arm's of the (b) form its own)
    let ret_n = crate::ops::modifytable::returning::target_at(p, j, w).map_or(natts, |t| t.site.natts());
    let op = (kind_impl::<RowOpMaker>(&node, KindId::WriteDelete).0)(px, k, decl);
    let ret = w.returning.is_some().then(|| (kind_impl::<RetMaker>(&node, KindId::WriteReturning).0)(px, k, j));
    let redo = op.epq_in_ir().then(|| crate::ops::modifytable::merge::epq::of(px, (k, j), w, (node, ex))); // D655 Row C
    // D3477: a settled union's source (after the row op, whose marks made the mark's copy site); its recheck is the runner's
    let settled = crate::ops::modifytable::merge::settled::of(px, k, w, node);
    if settled.is_some() && (redo.is_some() || !nm.is_empty() || !bs.is_empty() || ret.is_some()) {
        crate::refuse!(node, Reason::Bug { payload: "a settled MERGE with an IR redo, a NOT MATCHED action or RETURNING" });
    }
    MergeEnd { op, atts, actions, ex, node, mt, tid, test, ret, view, nm, bs, cond, pick, union, ret_map, pad, sys: sys_cells, ret_n, redo, settled }
}

impl EndMech for MergeEnd<'_> {
    fn open_words(&self, w: &mut crate::frame::open::OpenWords) {
        if let Some(s) = &self.settled {
            s.open_words(w);
        }
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        self.op.open(b, env);
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        self.run(b, env, row, None)
    }

    fn sink(&self) -> SinkKind {
        SinkKind::RowOp
    }
}

impl MergeEnd<'_> {
    /// The actions over `row`; under RETURNING (`next`, D789) each written row goes on through write.returning.
    fn run<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, mut next: Option<Next<'_, 'e>>) -> Carry<'e> {
        // D3500: EXPLAIN ANALYZE's count of the rows the actions take (a recheck node's is read: driver `fold_recheck`)
        instr::count_one(b, env.ctx, self.mt, Ctr::MergeTaken);
        // the row reached the write (the scan node's EXPLAIN ANALYZE rows; under RETURNING the sink counts)
        let out = if next.is_some() { row.carry.out } else { b.int_bin(IntBinOp::Add, row.carry.out, env.one) };
        let at = RowVals { row: row.row.clone(), vals: row.vals.clone(), cols: row.cols.clone(), carry: Carry { out, accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) }, next: row.next, at: row.at };
        // the actions' row: target attribute `a` at its carried resno, the child's columns the inner map (INNER_VAR)
        let cols = self.atts.iter().map(|a| a.as_ref().filter(|a| a.resno > 0).and_then(|a| at.cols.get(a.resno as usize - 1).copied().flatten())).collect_in(env.mcx);
        let arm = RowVals { row: at.row.clone(), vals: at.vals.clone(), cols, carry: at.carry.copy_in(env.mcx), next: at.next, at: at.at };
        let outer_inner = env.inner.replace(mcx::to_vec_in(&at.cols[..], env.mcx));
        let types: PgVec<'_, Type> = at.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let l_join = b.create_block(&types[..]);
        // D3477: a settled union's source: the recheck's first joined row acts with the stored source row (merge/settled.rs)
        if let Some(s) = &self.settled {
            s.lower(self, b, env, (&at, &arm, l_join), &mut next);
            b.switch_to(l_join);
            env.inner = outer_inner;
            env.memo.clear();
            return Carry::from_params(b.block_params(l_join), env.mcx);
        }
        // V4R-353: a view's NULL wholerow is ExecMerge's not-matched path: no WHEN MATCHED action runs (its NOT MATCHED
        // actions do, below: mergecmd view NM)
        let whole = self.view.as_ref().map(|v| v.whole(b, env, &at, self.node));
        if let Some((_, valid)) = whole.filter(|_| self.nm.is_empty()) {
            let l_on = b.create_block(&[]);
            b.cond_br(valid, (l_on, &[]), (l_join, &at.carry.params()));
            b.switch_to(l_on);
        }
        // W6: a NULL target ctid is a source row no target row joined: the NOT MATCHED actions (write/merge_nm.rs); a
        // view's NULL wholerow stands for it
        // mergecmd RIGHT_OWN: or the junk ctid read as the test alone (`test`), the position the driving scan's
        let test = || self.test.as_ref();
        let tid = if self.nm.is_empty() { None } else if whole.is_some() { whole } else { self.tid(b, env, &at).or_else(|| test().and_then(|t| crate::ops::modifytable::merge::nm::junk(b, env, &at, t, self.node))) };
        crate::ops::modifytable::merge::nm::lower(b, env, (&self.nm, self.ex, self.node, self.atts.len(), self.mt), (&at, &arm, tid, l_join), &*self.op, self.ret.as_deref(), &mut next);
        // V4R-495: the join condition true runs the MATCHED actions, else (`l_bs`) the BY SOURCE ones (ExecMergeMatched)
        let l_bs = crate::ops::modifytable::merge::nm::split(b, env, (&self.cond, self.ex, self.node), &arm);
        // D655 Row C: every MATCHED action's RECHECK enters the redo loop (merge/epq.rs), a BY SOURCE one its own
        let l_redo = self.redo.as_ref().map(|_| crate::ops::modifytable::merge::epq::header(b));
        let l_redo_bs = l_bs.and(l_redo).map(|_| crate::ops::modifytable::merge::epq::header(b));
        self.acts(b, env, &self.actions, (&at, &arm, whole, l_join), &mut next, l_redo);
        if let Some(l_bs) = l_bs {
            b.switch_to(l_bs);
            self.acts(b, env, &self.bs, (&at, &arm, whole, l_join), &mut next, l_redo_bs);
        }
        if let (Some(r), Some(l)) = (&self.redo, l_redo) {
            r.lower(self, b, env, (&at, &arm, l, l_redo_bs, l_join), &mut next);
        }
        b.switch_to(l_join);
        env.inner = outer_inner;
        env.memo.clear();
        Carry::from_params(b.block_params(l_join), env.mcx)
    }

    /// One action list from the current block: the first whose quals hold runs, then (as when none does) `l_join`; a
    /// RECHECK enters `redo` when the write rechecks in the IR (mergecmd D655 Row C).
    pub(super) fn acts<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, list: &[Action<'_>], (at, arm, whole, l_join): (&RowVals, &RowVals, Option<(Value, Value)>, gravity_ir::Block), next: &mut Option<Next<'_, 'e>>, redo: Option<gravity_ir::Block>) {
        // RETURNING's continuation (D789), shared by an action's written row and its concurrent fall-through's INSERT
        // (14a): the row op runs one of the two per call, so each takes it in turn
        let next = core::cell::RefCell::new(next);
        for a in list.iter() {
            // an action's quals run only after every earlier action failed: nothing memoised under one dominates the next
            env.memo.clear();
            let l_next = b.create_block(&[]);
            for &q in a.quals.iter() {
                let (pass, l_on) = (self.ex.0.pred(b, env, self.node, arm, q), b.create_block(&[]));
                b.cond_br(pass, (l_on, &[]), (l_next, &[]));
                b.switch_to(l_on);
            }
            let (op, ret) = (&*self.op, self.ret.as_deref());
            let delete = matches!(a.act, Act::Delete);
            let mt = self.mt;
            // D789: the action's written row (an UPDATE's new row, a DELETE's deleted version), its OLD and its text;
            // EXPLAIN ANALYZE's count of the rows the kind of action wrote
            let mut done = |b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, r: &RowVals| {
                instr::count_one(b, env.ctx, mt, if delete { Ctr::MergeDeleted } else { Ctr::MergeUpdated });
                match (ret, next.borrow_mut().as_deref_mut()) {
                    (Some(ret), Some(n)) => {
                        let mut written = op.cells(b, env, false);
                        // writesubplan (gen mg-0178): the written row's system columns after the site's cells (`sys`)
                        let cut = written.len().saturating_sub(self.sys);
                        let tail = mcx::to_vec_in(&written[cut..], env.mcx);
                        written.truncate(cut);
                        let mut old = if delete { PgVec::new_in(env.mcx) } else { op.old_cells(b, env) };
                        // G2(c): under the union form, the row's own relation's cells in the first one's layout (the
                        // site as wide as the widest result relation); D888: a view's site carries the wholerow cell after its
                        // natts cells: RETURNING reads the natts
                        let (mut written, old) = match self.ret_map.as_deref() {
                            Some(m) => {
                                let old = if old.is_empty() { old } else { self.per_leaf(b, env, at, m, &old) };
                                (self.per_leaf(b, env, at, m, &written), old)
                            }
                            None => {
                                written.truncate(self.ret_n);
                                old.truncate(self.ret_n);
                                (written, old)
                            }
                        };
                        written.extend_in(tail);
                        ret.lower_merge(b, env, r, &written, &old, !delete, crate::ops::modifytable::merge::ret::merge_action(delete), n)
                    }
                    _ => r.carry.copy_in(env.mcx),
                }
            };
            // W6: a target row concurrently deleted falls through to the NOT MATCHED actions (merge/nm.rs `chain`); under
            // RETURNING (14a) the INSERT it runs returns its row as any NOT MATCHED INSERT does (ExecMergeNotMatched)
            let (nm, fx) = (&self.nm[..], (&self.nm[..], self.ex, self.node, self.atts.len(), self.mt));
            let mut fall = |b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, r: &RowVals| crate::ops::modifytable::merge::nm::chain(b, env, fx, (r, arm), op, ret, &mut **next.borrow_mut());
            let mut call = |b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, w: u32, r: RowOpRow<'_>, done: Next<'_, 'e>| {
                let fall = (!nm.is_empty()).then_some(&mut fall as Next<'_, 'e>);
                op.call_op(b, env, at, w, r, done, match redo { Some(l) => Arm::MergeEpq(fall, l), None => Arm::Merge(fall) })
            };
            let c = match &a.act {
                Act::Nothing => at.carry.copy_in(env.mcx),
                Act::Delete => {
                    let tid = self.tid(b, env, at);
                    let rel = self.rel(b, env, at);
                    // a view's DELETE: NULL cells, then the wholerow OLD (the INSTEAD OF DELETE trigger's row)
                    let cells: PgVec<'_, (Value, Value)> = match whole {
                        Some(w) => {
                            let f = b.bconst(false);
                            self.atts.iter().map(|_| (env.zero, f)).chain([w]).collect_in(env.mcx)
                        }
                        None => PgVec::new_in(env.mcx), // (empty: allocates nothing)
                    };
                    call(b, env, WRITE_DELETE, RowOpRow { cells: &cells[..], tid, rel }, &mut done)
                }
                Act::Update(sets, multi) => {
                    // merge#340: each MULTIEXPR SubPlan first, its setParams written for the entries (the value is PG's dummy NULL)
                    for &m in multi.iter() {
                        let _ = self.ex.0.emit(b, env, self.node, arm, m);
                    }
                    let cells = self.image(b, env, at, arm, sets, whole);
                    let tid = self.tid(b, env, at);
                    let rel = self.rel(b, env, at);
                    call(b, env, WRITE_UPDATE, RowOpRow { cells: &cells, tid, rel }, &mut done)
                }
            };
            b.br(l_join, &c.params());
            b.switch_to(l_next);
        }
        b.br(l_join, &at.carry.params());
    }

    /// D776: the target's junk ctid `(datum, valid)` (a NULL ctid: the row op's unsupported), none over the target's scan.
    fn tid(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, at: &RowVals) -> Option<(Value, Value)> {
        crate::ops::modifytable::merge::nm::junk(b, env, at, self.tid.as_ref()?, self.node)
    }

    /// G2(c): the row's relation (its junk tableoid) and the recheck code naming that relation's feed site; every
    /// feed site's position stored empty first, so a recheck, which reruns every feed, finds no row in the other result
    /// relations (EvalPlanQualStart's relsubs_blocked) and the row's own relation's newest version in its (the substituted
    /// site). None but under the union form. Empty is offset 0 (stored with the flags); the block word stays, as
    /// write/update_built.rs `union_rel`'s (writesubplan D2015 NEST-ARMS: a nest inner's arms are still scanning there).
    fn rel(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, at: &RowVals) -> Option<(Value, Value)> {
        let (col, of, sites) = self.union.as_ref()?;
        let Some(cr) = at.col(*col) else { crate::refuse!(self.node, Reason::Bug { payload: "a union-fed MERGE's tableoid past the child's projection" }) };
        let mut cx = env.lx(b, self.node);
        let (rel, _) = rep::datum(&mut cx, cr, of);
        env.memo = cx.memo;
        let mut code = b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE));
        for &(relid, s) in sites.iter().rev() {
            let off = site_off(s) as i32;
            let (flags, r) = (b.iconst(Type::I32, (POS_VALID | POS_ROW) as i64), b.iconst(Type::I32, relid as i32 as i64));
            b.store(env.ctx, off + site::POS_FLAGS as i32, flags);
            b.store(env.ctx, off + site::POS_REL as i32, r);
            let (id, c) = (b.iconst(Type::I64, i64::from(relid)), b.iconst(Type::I64, recheck(s)));
            let mine = b.icmp(IntCond::Eq, rel, id);
            code = b.select(mine, c, code);
        }
        Some((rel, code))
    }

    /// G2(c): `cells` (`(datum, isnull)` in the row's own result relation's layout) as the first result relation's
    /// attributes: per attribute a select over the relations by the row's junk tableoid, NULL where that relation dropped
    /// the column.
    fn per_leaf<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, at: &RowVals, map: &[(u32, PgVec<'_, Option<usize>>)], cells: &[(Value, Value)]) -> PgVec<'e, (Value, Value)> {
        let Some((col, of, _)) = self.union.as_ref() else { crate::refuse!(self.node, Reason::Bug { payload: "a union-form RETURNING map without the union" }) };
        let Some(cr) = at.col(*col) else { crate::refuse!(self.node, Reason::Bug { payload: "a union-fed MERGE's tableoid past the child's projection" }) };
        let mut cx = env.lx(b, self.node);
        let (rel, _) = rep::datum(&mut cx, cr, of);
        env.memo = cx.memo;
        let mines: PgVec<'_, Value> = map.iter().map(|(relid, _)| {
            let id = b.iconst(Type::I64, i64::from(*relid));
            b.icmp(IntCond::Eq, rel, id)
        }).collect_in(env.mcx);
        // `(datum, isnull)` pairs (the row op's cells): NULL where no relation matches or the column is dropped
        let (zero, null) = (env.zero, b.bconst(true));
        (0..self.atts.len()).map(|i| {
            let mut acc = (zero, null);
            for ((_, m), &mine) in map.iter().zip(mines.iter()) {
                let Some(c) = m.get(i).copied().flatten().and_then(|j| cells.get(j).copied()) else { continue };
                acc = (b.select(mine, c.0, acc.0), b.select(mine, c.1, acc.1));
            }
            acc
        }).collect_in(env.mcx)
    }

    /// An UPDATE action's new tuple, `(datum, valid)` per attribute: SET entries, else the carried old cell, NULL if dropped.
    /// A view's (`whole`, V4R-353): the old cells are the wholerow's fields, and the wholerow follows the view's cells.
    fn image<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, at: &RowVals, arm: &RowVals, sets: &[(usize, ExId, ExprOut)], whole: Option<(Value, Value)>) -> PgVec<'e, (Value, Value)> {
        let mcx = env.mcx;
        let t = b.bconst(true);
        let mut new: PgVec<'_, Option<(Value, Value)>> = mcx::pg_vec![in mcx; None; self.atts.len()];
        for &(a, ex, out) in sets.iter() {
            let (v, valid) = self.ex.0.emit(b, env, self.node, arm, ex);
            let Some(att) = &self.atts[a] else { crate::refuse!(self.node, Reason::Bug { payload: "a MERGE SET column that is not carried" }) };
            let isnull = b.int_bin(IntBinOp::Xor, valid, t);
            let cell = Cell::of_expr(out, 0, 0, Nullable::Maybe);
            let mut cx = env.lx(b, self.node);
            let (d, null) = rep::datum(&mut cx, CellRef { cell: &cell, val: v, null: Some(isnull) }, &att.tf);
            env.memo = cx.memo;
            new[a] = Some((d, b.int_bin(IntBinOp::Xor, null, t)));
        }
        if let (Some(v), Some((img, _))) = (self.view.as_ref(), whole) {
            let olds: PgVec<'_, i16> = (1..=self.atts.len() as i16).filter(|&a| new[a as usize - 1].is_none() && v.live(a)).collect_in(env.mcx);
            for (a, c) in olds.iter().zip(v.fields(b, env, img, &olds[..])) {
                new[*a as usize - 1] = Some(c);
            }
        }
        let (zero, f) = (env.zero, b.bconst(false));
        let mut cx = env.lx(b, self.node);
        let cells = self.atts.iter().zip(new).map(|(att, set)| match (att, set) {
            (_, Some(c)) => c,
            (None, None) => (zero, f),
            (Some(att), None) => {
                let Some(cr) = at.col(att.resno) else { crate::refuse!(self.node, Reason::Bug { payload: "a MERGE cell past the child's projection" }) };
                let (d, null) = rep::datum(&mut cx, cr, &att.tf);
                (d, cx.b.int_bin(IntBinOp::Xor, null, t))
            }
        });
        let mut cells: PgVec<'e, (Value, Value)> = cells.collect_in(mcx);
        env.memo = cx.memo;
        cells.extend_in(whole);
        let mut cells = crate::ops::modifytable::merge::route::picked(cells, self.pick.as_deref(), mcx);
        // SORT-UNION: beside the routed INSERT the site is the root's row; the cells past the SET ones are NULL
        if cells.len() < self.pad {
            let f = b.bconst(false);
            cells.resize_in(self.pad, (env.zero, f));
        }
        cells
    }
}

/// G2(c): `MergeEnd::ret_map` (every result relation's cell index of each first-relation attribute, by root
/// column: partwrite `root_attno`, the planner's map_partition_varattnos over returningLists).
fn ret_map<'g>(mcx: Mcx<'g>, w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>) -> PgVec<'g, (u32, PgVec<'g, Option<usize>>)> {
    use gravity_decompose::ops::modifytable::partwrite::root_attno;
    let t0 = &w.targets[0];
    let roots: PgVec<'_, i16> = (1..=t0.site.natts() as i16).map(|a| root_attno(t0, a)).collect_in(mcx);
    w.targets.iter().map(|t| (t.relid, roots.iter().map(|&r| (1..=t.site.natts() as i16).position(|a| r > 0 && root_attno(t, a) == r)).collect_in(mcx))).collect_in(mcx)
}

//! W5 multilock Row A (V4R-524; the approved plan's section 3, main 09-29 04:31Z): write.lock over several locking marks,
//! or over one on a relation another pipeline scans (a hash build side the rechecking pipeline probes), rechecked by the runner
//! over every locked site (executor R). ExecLockRows locks each row's marks in PlanRowMark order (preprocess_rowmarks':
//! the locking clauses in range table order), each with its own strength and wait policy: `Program.locks`, one lock host
//! per mark, the mark's index in `lock_row`'s argument (`LockSite::arg`). Per mark:
//! - WRITE_SKIPPED (SKIP LOCKED, deleted, TM_SelfModified): the next row; the marks locked before stay locked, the later
//!   ones are not tried;
//! - WRITE_RECHECK (READ COMMITTED, a newer version locked): locking goes on with the next mark; after the last one the
//!   row stops for the runner's EvalPlanQual (`recheck_exit`), once, whichever marks traversed;
//! - below zero: the host's error (NOWAIT's 55P03 naming the mark's relation, a serialization failure under a transaction
//!   snapshot, a deadlock), with the rows finished before it.
//! Each mark's row is its junk ctid, stored as the position of the site that scans it (marks.rs: a producer's scan site,
//! or the rechecking pipeline's own scan's marked-row site, D776); the host locks there and leaves a traversed newest
//! version there. So the stopped node's settle holds every locked relation's EPQ test tuple (the newest version, or the
//! one read) beside the REFERENCE / COPY rows, and the stop names the last traversed mark's site, whose `recheck_pos` is
//! its newest version: exec epq.rs substitutes them all and reruns the producers, then the plan below the lock (L3).
//! CNM-38 (lockrows 15): the same form serves a lock whose rows reach it through a breaker (LockRows over a Sort: an
//! ORDER BY below FOR UPDATE, a view's or a subquery's), one locking mark or several (`over_breaker`): each row's marks
//! are its junk ctids (preprocess_targetlist carries them through the Sort), stored at the producers' scan sites, and
//! the recheck reruns those producers over the substituted scans before the drain, as PostgreSQL's EvalPlanQual reruns
//! the subplan below LockRows (the Sort over the test tuple alone) and LockRows returns that row in its sorted place.
//! D3269 (a): so does a lock over an INNER merge join (the locked relation reaching it through a Sort, a sorted inner run
//! or the pipeline's own scan; the other side a REFERENCE relation or a COPY subquery): the recheck reruns both sorted
//! inputs' producers over the substituted rows, then the merge over one row a side, as PostgreSQL's EvalPlanQual reruns
//! the Merge Join over its test tuples.
//! lockrows NEST: a mark (locking or REFERENCE) on a relation an INNER nested loop's inner scans (rerun/epq/marks.rs
//! `nest_joins`: one heap scan under filters and projections; the nest in this pipeline or in a producer the recheck
//! reruns) is stored in a RowOf site of the inner's scan site, the host locks there, and a stop after it names the inner's
//! site: the recheck substitutes the inner with that row (a locked one's newest version) and opens it once, the outer side
//! substituted too, as PostgreSQL's EvalPlanQual rescans the inner over its test tuple. Over a VALUES outer of its own
//! (`nest_outer`, lk-0104) the VALUES' COPY row is that one outer row (driver `Entry::recheck` hands it back, mergecmd 9).
//! lockrows D2604: a family lock over a breaker (`family_breaker`: a cursor's ORDER BY .. FOR UPDATE over a tree) takes this
//! form too, each child's mark active only on its leaf's rows (`Marks::gate`), so WHERE CURRENT OF answers that leaf alone.
//! lockrows FAMJOIN: so does one beside a join's REFERENCE marks (an ORDER BY over the family's arms probing a build), the
//! REFERENCE marks never gated; a family's union kept in its Materialize form is rerun over its feeds (`sites_in`).
//! lockrows REFFAM: so does one beside other parents' whole REFERENCE families (`lock_family`: FOR SHARE OF one alias of a
//! self-joined inheritance tree), each family's children gated on that family's own tableoid junk column.
//! Refused by name: a family beside another relation's COPY or locking mark (FAMILY_JOIN); a locked relation a nested loop's inner
//! scans in another form (NEST_INNER); a pipeline over neither a heap scan of its own nor a breaker, or a locked relation no heap scan
//! reads (NOT_OWN: a subquery, a TID or non-heap source); any op above but the projection (LIMIT_ABOVE: the in-IR recheck is
//! one-site; OP_ABOVE); a pipeline that produces for another (marks.rs EMIT_ONLY) or whose rows do not go to the client
//! (NOT_SINK); write/lock_rerun.rs: a producer the recheck reruns first that PG may not run and that may raise or call
//! a function (RERUN_FIRST, D2033), an outer side PG's later rechecks may skip (BUILD_FIRST, D2050), a producer holding
//! an outer join (NULL_SIDE, D2048). Owner M8-MEMO-CTE.
use std::cell::Cell;
use std::rc::Rc;

use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};

use gravity_abi::ctx::{hdr, out};
use gravity_abi::status::{recheck, unsupported, UNSUPPORTED_SHAPE, WRITE_RECHECK, WRITE_SKIPPED};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::ops::shared::modifytable::{LockDesc, Member, RowMark};
use gravity_decompose::{Am, Op, PipelineNode, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_program::program::LockSite;
use gravity_program::LockKind;
use types_nodes::jointype::JoinType;
use types_nodes::plannodes::RowMarkType;

use crate::face::end::{EndMech, SinkKind};
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::iface::limit_bounds;
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::admit::refusal::{Reason, ShapeWhy};

use crate::ops::lockrows::lower::{FAMILY_JOIN, LIMIT_ABOVE, NOT_OWN, NOT_SINK, OP_ABOVE};

/// A lock on a nested loop's inner relation: the runner substitutes the inner once, where PG's rescan of the inner per
/// outer row (nodeNestloop.c ExecNestLoop) resets it; a batched window's position is the window's last row, not the row's.
const NEST_INNER: ShapeWhy = ShapeWhy("a row lock on a relation a nested loop's inner scans (EPQ over several sites: the inner's rescan)");

fn shape(node: i32, why: ShapeWhy) -> ! {
    crate::refuse!(node, Reason::Shape { why })
}

/// Whether lock `d` of pipeline `p` takes this form: locks on several relations (decompose's MultiLock: a family's
/// siblings are one lock, write/lockrows.rs's L2), or its one mark on a relation other than the one the pipeline's own
/// scan reads (a pipeline over a sort or a subquery stays lockrows.rs's).
pub(crate) fn several(p: &PipelineNode<'_>, d: &LockDesc<'_>) -> bool {
    if d.members.iter().any(|m| m == Member::MultiLock) {
        return true;
    }
    let mut locking = d.marks.iter().filter(|m| m.lock != LockKind::None);
    // D3278 (SAMPLE): or a TABLESAMPLE scan of another relation (REFERENCE-marked, its row substituted: `make`'s `sample`)
    // D3292: or a table function (its COPY row the recheck's copy entry hands the scan: `make`'s `tablefunc`)
    let sample = match p.source {
        Source::Sample { am: Am::Heap, scanrelid, .. } | Source::TableFunc { scanrelid, .. } => Some(u32::from(scanrelid)),
        _ => None,
    };
    match (locking.next(), locking.next()) {
        (Some(m), None) => !m.parent && own(&p.source).or(sample).is_some_and(|r| r != m.rti),
        _ => false,
    }
}

/// CNM-38 (module doc): whether lock `d` of pipeline `p` (not several's) takes this form over a breaker: its one locking
/// mark, no parent's, read back from a breaker the producers fill (a Sort below LockRows), not from a scan of its own.
pub(crate) fn over_breaker(p: &PipelineNode<'_>, d: &LockDesc<'_>) -> bool {
    let mut locking = d.marks.iter().filter(|m| m.lock != LockKind::None);
    matches!(p.source, Source::Breaker { .. }) && matches!((locking.next(), locking.next()), (Some(m), None) if !m.parent)
}

/// lockrows D2604: whether lock `d` of pipeline `p` is one partitioned / inheritance family's (L2's `LockDesc::family`: a top
/// parent's mark and its children's, no other relation's) read back from a breaker (LockRows over a Sort or Material over
/// the family's Append, a cursor's ORDER BY .. FOR UPDATE over an inheritance tree): this form, each child's mark positioned
/// at its Append arm's scan site, active only where the row's tableoid junk column names its leaf (`Marks::gate`).
/// lockrows FAMJOIN: or beside a join's REFERENCE marks (`LockDesc::family_top`: an ORDER BY over the family's Append
/// probing another relation's build, the arms streamed into the Sort), those marks never gated.
/// lockrows REFFAM: or beside whole REFERENCE families (`lock_family`).
pub(crate) fn family_breaker(p: &PipelineNode<'_>, d: &LockDesc<'_>) -> bool {
    matches!(p.source, Source::Breaker { .. }) && lock_family(d).is_some()
}

/// lockrows REFFAM: `LockDesc::family_top`, or the one locked family's top parent beside other parents' whole REFERENCE
/// families (FOR SHARE OF one alias of a self-joined inheritance tree: its other alias's isParent REFERENCE mark and its
/// children's). Each family's children share their top parent's `tableoid{rowmarkId}` junk column (`RowMark::toid_col`),
/// and EvalPlanQualFetchRowMark / ExecLockRows act on a child's mark only where the row's tableoid names it, so each child
/// is gated on its own family's column (`Marks::gate`); a plain relation's REFERENCE mark is never gated.
pub(crate) fn lock_family(d: &LockDesc<'_>) -> Option<u32> {
    if let Some(t) = d.family_top() {
        return Some(t);
    }
    // lockrows MULTIFAM (portals#244: FOR UPDATE of both aliases of a self-joined inheritance tree): one or more locked
    // families, each child locking as its own top parent's mark asks (ExecLockRows locks each family's active child)
    let tops = || d.marks.iter().filter(|m| m.parent && m.prti == m.rti && m.lock != LockKind::None);
    let first = tops().next()?;
    let top_of = |m: &RowMark| tops().find(|t| m.prti == t.rti);
    let refs = |m: &RowMark| m.lock == LockKind::None && m.mark == RowMarkType::ROW_MARK_REFERENCE && (m.prti == m.rti && !m.parent || m.toid_col > 0);
    let ok = d.marks.iter().all(|m| match top_of(m) {
        Some(t) => m.toid_col > 0 && (m.rti == t.rti || m.lock == t.lock && m.wait == t.wait),
        None => refs(m),
    });
    let whole = tops().all(|t| d.marks.iter().any(|m| !m.parent && m.prti == t.rti));
    (ok && whole).then_some(first.rti)
}

/// lockrows REFFAM L1: every parent's mark and every child's is a REFERENCE mark (no lock) with its family's
/// `tableoid{rowmarkId}` junk column, and there is a parent: the lock's relations are plain, beside whole REFERENCE families.
pub(crate) fn refs_beside(d: &LockDesc<'_>) -> bool {
    let fam = |m: &RowMark| m.parent || m.prti != m.rti;
    d.marks.iter().any(|m| m.parent) && d.marks.iter().filter(|m| fam(m)).all(|m| m.lock == LockKind::None && m.mark == RowMarkType::ROW_MARK_REFERENCE && m.toid_col > 0)
}

/// lockrows D2604: the family's `tableoid{rowmarkId}` junk column (preprocess_targetlist adds one per top parent's mark; the
/// children share their parent's rowmarkId), LockRows' targetlist being its child's.
pub(crate) fn tableoid_col(d: &LockDesc<'_>) -> Option<i16> {
    let named = |n: &str| n.strip_prefix("tableoid").is_some_and(|id| !id.is_empty() && id.bytes().all(|c| c.is_ascii_digit()));
    let mut cols = d.tlist.iter().filter_map(|e| e.as_target_entry()).filter(|te| te.resjunk && te.resname.is_some_and(named)).map(|te| te.resno);
    match (cols.next(), cols.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

pub(crate) fn own(s: &Source<'_>) -> Option<u32> {
    match s {
        Source::Range { scanrelid, .. } | Source::Index { scanrelid, .. } | Source::Bitmap { scanrelid, .. } | Source::Tid { scanrelid, .. } => Some(u32::from(*scanrelid)),
        _ => None,
    }
}

/// lockrows NEST: whether lock `d` of pipeline `k` (not several's) takes this form over a VALUES outer: its one locking mark,
/// no parent's, on a relation a nest inner of `k` scans (`nest_scan`) under the pipeline's VALUES source, whose COPY row
/// the recheck hands back whole (one outer row).
pub(crate) fn nest_outer(gx: &crate::graph::Gx<'_, '_>, k: usize, d: &LockDesc<'_>) -> bool {
    let mut locking = d.marks.iter().filter(|m| m.lock != LockKind::None);
    let one = matches!((locking.next(), locking.next()), (Some(m), None) if !m.parent && crate::rerun::epq::marks::nest_scan(gx, k, m).is_some());
    one && matches!(gx.pipelines()[k].source, Source::Values { .. })
}

/// A heap scan's (scanrelid, relid), marks.rs's form: range, index (not index-only), bitmap.
fn heap_scan(s: &Source<'_>) -> Option<(u32, u32)> {
    match s {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } => Some((u32::from(*scanrelid), *relid)),
        _ => None,
    }
}

/// Whether nest op `o`'s inner (at any depth) scans range table index `rti`.
fn inner_scans_deep(o: &Op<'_>, rti: u32) -> bool {
    matches!(o, Op::Join(JoinDesc::Nest { inner, .. }) if own(&inner.source) == Some(rti) || inner_scans(inner, rti))
}

/// Whether a nested loop's inner in `p` (at any depth) scans range table index `rti`.
fn inner_scans(p: &PipelineNode<'_>, rti: u32) -> bool {
    crate::stack_check(); // D3832
    p.ops.iter().any(|o| matches!(o, Op::Join(JoinDesc::Nest { inner, .. }) if own(&inner.source) == Some(rti) || inner_scans(inner, rti)))
}

/// lockrows J182: op `o` of pipeline `k` is an INNER nest whose inner reads a pulled plan Material (nest.spool S1:
/// rerun/spool/form.rs `spool_pull`, its child one scan of filters and projections made again inside the inner over the
/// fill pipeline's site, where a mark on the scanned relation positions) under filters and projections. The recheck
/// re-opens the inner over an empty spool of its own (exec rerun/epq.rs `fresh_spools`) and its child pulls the row at that
/// site, as EvalPlanQual's fresh Material pulls its child's test tuple.
fn spool_nest(gx: &crate::graph::Gx<'_, '_>, k: usize, o: &Op<'_>) -> bool {
    matches!(o, Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, .. })) && spool_inner(gx, k, o)
}

/// D3287 (b): op `o` of pipeline `k` is a LEFT nest whose inner reads `spool_nest`'s pulled plan Material (m7_epq_joins:
/// Nested Loop Left Join over Materialize(Seq Scan)). FOR UPDATE never reaches a nullable side, so its child scans a
/// REFERENCE relation: the recheck re-opens the inner over an empty spool of its own and the child pulls the row the
/// statement read at that site, none where the join null-extended (its ctid NULL), so the recheck null-extends again.
pub(crate) fn spool_left_nest(gx: &crate::graph::Gx<'_, '_>, k: usize, o: &Op<'_>) -> bool {
    matches!(o, Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_LEFT, .. })) && spool_inner(gx, k, o)
}

/// `spool_nest`'s inner: a pulled plan Material's reader under filters and projections.
fn spool_inner(gx: &crate::graph::Gx<'_, '_>, k: usize, o: &Op<'_>) -> bool {
    let Op::Join(JoinDesc::Nest { inner, .. }) = o else { return false };
    let Source::Breaker { slot } = inner.source else { return false };
    let fill = gx.life_of(k, slot).map(|l| l.0 as usize);
    inner.ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) && fill.is_some_and(|f| crate::pipes::spool_pull(gx, f).is_ok())
}

/// Every refusal by name of the module doc, then the marks' positions (marks.rs, no target) and `Program.locks`.
pub(crate) fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize, node: i32, d: &LockDesc<'_>, fam: bool) -> PgBox<'g, dyn StepMech + 'g> {
    let gx = px.gx;
    let ps = gx.pipelines();
    let p = &ps[k];
    // D2604: a family over a breaker (`fam`, plan.rs `Plan::form`); any other parent's mark is a family beside
    // another relation's (A2)
    // lockrows REFFAM L1: a lock with no family of its own beside other parents' whole REFERENCE families (`refs_beside`),
    // each REFERENCE child gated on its family's tableoid column by rerun/epq/marks.rs `sites_in`
    let refs = !fam && refs_beside(d);
    if !fam && !refs && d.marks.iter().any(|m| m.parent) {
        shape(node, FAMILY_JOIN);
    }
    if d.plan.lock == LockKind::None || d.members.iter().any(|m| m != Member::MultiLock && !((fam || refs) && m == Member::ParentMark)) {
        crate::refuse!(node, Reason::Bug { payload: "a LockRows over several marks without a lock plan, or carrying a member write.lock does not bind" });
    }
    let gate = match fam {
        true => {
            let Some(of) = gravity_decompose::catalog::typefacts::facts_of(gx.facts, types_core::catalog::OIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the oid type the catalog does not know" }) };
            // lockrows REFFAM: a settled union's spool (its feeds' record) is the recheck's own and empty when the
            // recheck reruns its feeds (exec rerun/epq.rs `fresh_spools`, mergecmd G2(c)), as EvalPlanQualStart's fresh
            // tuplestores; rerun/epq/marks.rs `sites_in` reruns its reader only over every feed (else D652)
            // FAMJOIN: only a family's children are gated (a join's REFERENCE mark beside them is always active); REFFAM:
            // each on its own family's tableoid column (the locked family's, or a REFERENCE family's beside it)
            let col = |m: &RowMark| if m.prti != m.rti { m.toid_col } else { 0 };
            if d.marks.iter().any(|m| !m.parent && m.prti != m.rti && m.toid_col <= 0) {
                crate::refuse!(node, Reason::Bug { payload: "a family child's mark without its tableoid junk column" })
            }
            let kin = d.marks.iter().filter(|m| !m.parent).map(col).collect_in(px.mcx());
            Some((of, kin))
        }
        false => None,
    };
    // a parent's mark locks nothing (ExecLockRows skips it); its children lock each row in its leaf
    let locking: PgVec<'_, &RowMark> = d.marks.iter().filter(|m| m.lock != LockKind::None && !m.parent).collect_in(px.mcx());
    // lockrows J182: a locked relation scanned only by a pulled plan Material's child (its fill, rerun/spool/form.rs `spool_pull`)
    let pulled_lock = |m: &RowMark| {
        let mut scans = (0..ps.len()).filter(|&q| own(&ps[q].source) == Some(m.rti)).peekable();
        scans.peek().is_some() && scans.all(|q| crate::pipes::spool_pull(gx, q).is_ok())
    };
    // D3269 (a): the rechecking pipeline's own copy entry (a COPY subquery this pipeline runs, epq-subq-window's merge
    // outer): the recheck enters at its SubqueryScan with the stored row, and the subquery's own ops below it never run
    // (rerun/epq/copy_row.rs; rerun/epq/marks.rs `copy_pos` W5-C)
    let rec = |m: &RowMark| gx.graph.record_rows.iter().find(|r| r.scanrelid == m.rti);
    let entry = d.marks.iter().filter(|m| m.mark == RowMarkType::ROW_MARK_COPY).filter_map(rec).filter_map(|r| crate::rerun::epq::copy_row::boundary(gx, k, r)).max().unwrap_or(0).min(j);
    for o in p.ops[entry..j].iter() {
        match o {
            Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::SubPlan(_) => {}
            o if crate::rerun::epq::marks::joins(o) => {}
            // lk-0100 (D2131): the spooled RIGHT SEMI (D3379 RDRAIN: or RIGHT) drain the lock sits on (hashjoin/drain/spool_admit.rs `free_lock`)
            Op::Join(JoinDesc::Drain { .. }) if gx.free_lock(k, k).is_some() => {}
            o if fam && crate::rerun::union_arms::union_op(o) => {}
            // D3287 (b): a family's lock over a MergeAppend of its leaves (no run-time pruning; lockrows-d2604-family-sort's
            // ORDER BY .. FOR UPDATE and its cursor): each arm feeds one sorted run; the recheck reruns the arms over the
            // substituted rows (only the leaf the row's tableoid names has an active mark, the others no row) and merges them,
            // as PostgreSQL's EvalPlanQual reruns the Merge Append over that leaf's test tuple (FAMMERGE's form without a LIMIT)
            Op::Append(AppendDesc::Merge { prune: None, .. }) if fam => {}
            // D3269 (a): an INNER merge join (J182's beside a pulled Material's nest, or over a locked relation's scan):
            // its sorted inputs are producers the recheck reruns over the substituted rows (the locked relation's newest
            // version, a REFERENCE row, a COPY row through its copy entry: rerun/epq/marks.rs `sites_in`), then the merge
            // runs once over one row a side, as PostgreSQL's EvalPlanQual reruns the Merge Join over its test tuples; an
            // inner PG may not run must be quiet (rerun.rs RERUN_FIRST), and a stop at the recheck's push re-enters the
            // merge's pair (D3289)
            Op::Join(JoinDesc::Merge { jointype: JoinType::JOIN_INNER, .. }) => {}
            // D3287 PULLNEST (a FOR UPDATE cursor's fast-start plan: LockRows -> Nested Loop, the inner a Materialize over
            // the other relation's scan): or each locked relation the lock pipeline's own scan or, over a family's settled
            // union (`fam`), one of its children's feeds, the recheck substituting its newest version there (gated on the
            // family's tableoid) while the inner's child pulls its row at its own site (locked or REFERENCE), as
            // EvalPlanQual reruns the Nested Loop over the test tuples with a fresh Material
            o if spool_nest(gx, k, o) && locking.iter().all(|m| pulled_lock(m) || own(&p.source) == Some(m.rti) || fam && !m.parent && m.prti != m.rti) => {}
            o if crate::rerun::epq::marks::nest_joins(o) && locking.iter().all(|m| !inner_scans_deep(o, m.rti) || crate::rerun::epq::marks::nest_scan(gx, k, m).is_some()) => {}
            // D3291: a nest whose inner holds one Memoize, every mark its inner scans a `nest_scan` position (fresh memo)
            o if crate::rerun::epq::marks::nest_memo_joins(o) && d.marks.iter().filter(|m| !m.parent && inner_scans_deep(o, m.rti)).all(|m| crate::rerun::epq::marks::nest_scan(gx, k, m).is_some()) => {}
            // D3287 (b): a LEFT join whose inner no lock reads (its REFERENCE row null_empty: lower.rs L1's `join`)
            o if crate::rerun::epq::marks::left_joins(o) && locking.iter().all(|m| !inner_scans_deep(o, m.rti)) => {}
            o if spool_left_nest(gx, k, o) && locking.iter().all(|m| !pulled_lock(m)) => {}
            Op::Join(JoinDesc::Nest { inner, .. }) if locking.iter().any(|m| own(&inner.source) == Some(m.rti) || inner_scans(inner, m.rti)) => shape(node, NEST_INNER),
            _ => shape(node, NOT_OWN),
        }
    }
    let scanned = |m: &RowMark| ps.iter().any(|q| heap_scan(&q.source) == Some((m.rti, m.relid))) || crate::rerun::epq::marks::nest_scan(gx, k, m).is_some();
    // CNM-38: a breaker source's rows come from the producers marks.rs `sites` makes the recheck rerun (each locked
    // relation's scan among them, else D652 there)
    // lockrows NEST: a VALUES source whose every locked relation a nest inner scans (its COPY row the one outer row)
    let values = matches!(p.source, Source::Values { .. }) && locking.iter().all(|m| crate::rerun::epq::marks::nest_scan(gx, k, m).is_some());
    // D3278 (SAMPLE): or a TABLESAMPLE scan of a REFERENCE-marked relation (rerun/epq/marks.rs `sample_of`: its row substituted)
    let sample = matches!(p.source, Source::Sample { am: Am::Heap, .. });
    // D3292: or a table function a COPY mark carries, its copy entry on this pipeline (rerun/epq/copy_row.rs: the recheck
    // enters with the stored row, the function never called again, as EvalPlanQual hands the Table Function Scan its row)
    let tablefunc = matches!(p.source, Source::TableFunc { scanrelid, .. } if d.marks.iter().any(|m| m.mark == RowMarkType::ROW_MARK_COPY && m.rti == u32::from(scanrelid) && rec(m).is_some_and(|r| crate::rerun::epq::copy_row::boundary(gx, k, r).is_some())));
    if heap_scan(&p.source).is_none() && !matches!(p.source, Source::Breaker { .. }) && !values && !sample && !tablefunc || !locking.iter().all(|m| scanned(m)) {
        shape(node, NOT_OWN);
    }
    for o in p.ops[j + 1..].iter() {
        if limit_bounds(o).is_some() {
            shape(node, LIMIT_ABOVE);
        }
        if !matches!(o, Op::Project(_)) {
            shape(node, OP_ABOVE);
        }
    }
    // lockrows D2604: a family's InitPlan refusals first (lock_init.rs, as L2 asks them), then a pair's second write plan
    if fam {
        crate::ops::lockrows::lock_init::admit(px, k, node);
    }
    // D4154 INITMULTI: the M4 pair's B (plan.rs `Plan::role`, `runner_off`): its marks are Program.locks entries
    // after A's (entry 1), and A's write plan is the pair's, as lower.rs's B replaces it
    let off = crate::ops::lockrows::plan::runner_off(px, node);
    // one write plan per program (`Px::write`): a lock beside a data-modifying CTE is a second one (L0-d)
    if off == 0 {
        px.second_write(node);
    }
    let mut marks = crate::rerun::epq::marks::sites_in(px, k, node, &d.marks, &|_| false, true);
    if gate.is_some() {
        marks.gate = gate;
    }
    if marks.pos.len() != d.marks.iter().filter(|m| !m.parent).count() {
        crate::refuse!(node, Reason::Bug { payload: "a LockRows over several marks whose marks are not each a position" });
    }
    // D2033 step 2: the epq probe's lazy group and the first probe's build the recheck keeps in the statement run's order
    // (rerun.rs `first_held`), as lower.rs's L1 form
    let mut lazy = crate::ops::lockrows::rerun::epq_lazy(px, k);
    let held = crate::ops::lockrows::rerun::first_held(px, k, &d.marks, &lazy);
    lazy.extend_in(held.iter().copied());
    // D2033 step 3: and the epq probe's own held build (`epq_held`)
    let epq_held = crate::ops::lockrows::rerun::epq_held(px, k, &d.marks, &lazy);
    lazy.extend_in(epq_held);
    // D4154 INITMULTI: the pair's B reruns none of the InitPlans' members (`sites_in`: lock_init.rs `members`), its
    // recheck reading the InitPlan A's run filled, as EvalPlanQualStart copies its value
    let init = if off == 1 { crate::rerun::epq::marks::init_members(px) } else { PgVec::new_in(px.mcx()) };
    crate::ops::lockrows::rerun::admit_lazy(px.gx, k, node, &d.marks, (true, &init), &lazy);
    px.recheck_hold.extend(held); // alloc: Program
    px.recheck_hold.extend(epq_held); // alloc: Program
    px.recheck_hold_epq.extend(epq_held); // alloc: Program
    let mut locks = Vec::new(); // alloc: Program
    let mut sites = PgVec::new_in(px.mcx());
    for (m, at) in d.marks.iter().filter(|m| !m.parent).zip(marks.pos.iter()).filter(|(m, _)| m.lock != LockKind::None) {
        if at.copy.is_some() {
            crate::refuse!(node, Reason::Bug { payload: "a locking mark positioned as a COPY row" });
        }
        locks.push(LockSite { rti: m.rti, relid: m.relid, lock: m.lock, wait: m.wait }); // alloc: Program
        sites.push_in((at.site.clone(), at.own, at.of));
    }
    px.keep_write(d.plan.copy_in(px.arena));
    crate::ops::lockrows::plan::Plan(px).runner(off, locks);
    let again = again(px, k, j);
    mcx::pg_box_dyn!(px.mcx(), Locks { mcx: px.mcx(), node, marks, sites, again, off } => dyn StepMech + 'g)
}

struct Locks<'g> {
    mcx: Mcx<'g>,
    node: i32,
    marks: crate::rerun::epq::marks::Marks<'g>,
    /// Each locking mark's site, in lock order, whether it is the rechecking pipeline's own scan's (the stop then names
    /// that scan's site), and the nest inner's scan site it is a RowOf site of (lockrows NEST: the stop names that one).
    sites: PgVec<'g, (Rc<Cell<usize>>, bool, Option<usize>)>,
    /// D3289: the re-entry word (`again`), over a join below the lock.
    again: Option<i32>,
    /// D4154 INITMULTI: the Program.locks entries before the first mark's (the M4 pair's A), 0 alone.
    off: usize,
}

/// D3289: a lock's re-entry word (`StepMech::reentry`) when a join op sits below it in pipeline `p` (an expanding step
/// that would otherwise continue past the row a receiver's stop at the recheck's push leaves the cursor at).
pub(crate) fn again(px: &mut Px<'_, '_>, k: usize, j: usize) -> Option<i32> {
    let below = px.gx.pipelines()[k].ops[..j].iter().any(|o| matches!(o, Op::Join(_)));
    below.then(|| crate::frame::stop::resume_word(px, crate::frame::stop::ResumeWord::StepOffset { k, j: crate::frame::px::key_j(j) }))
}

impl<'g> StepMech for Locks<'g> {
    /// The runner's recheck sends the rechecked row to the client (module doc): the end is the serial out-block sink.
    fn end_is(&mut self, end: &dyn EndMech) {
        if end.sink() != SinkKind::Serial {
            shape(self.node, NOT_SINK);
        }
    }

    fn reentry(&self) -> Option<i32> {
        self.again
    }

    /// D3289: the re-entry word starts clear at every open, as an expanding step's offset does.
    fn open_words(&self, w: &mut OpenWords) {
        if let Some(o) = self.again {
            w.declare(OpenWord::open("write.lock re-entry", Base::Words, o, Type::I64, 0));
        }
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        // the pipeline's own scan's site, read by an `own` mark's stop (a breaker source has none, and no `own` mark)
        let src = env.site;
        // D3289: the row is here: a re-entry it waited for is over
        if let Some(w) = self.again {
            env.store_word(b, w, env.zero);
        }
        if src.is_none() && self.sites.iter().any(|s| s.1) {
            crate::refuse!(self.node, Reason::Bug { payload: "a row lock over several marks whose source has no site" });
        }
        crate::rerun::epq::marks::store(b, env, row, self.node, &self.marks);
        let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
        let lock_row = gravity_kernels::ops::lockrows::entries::lock_row as *const () as usize;
        let h = b.declare_host_once("lock_row", &[Type::Ptr, Type::I64], Some(Type::I64), lock_row, mem);
        let types: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(self.mcx);
        let l_join = b.create_block(&types[..]);
        let l_bad = b.create_block(&[Type::I64]);
        b.mark_cold(l_bad);
        let (rc, sk) = (b.iconst(Type::I64, WRITE_RECHECK), b.iconst(Type::I64, WRITE_SKIPPED));
        // the stop's status: the last traversed mark's recheck(site), 0 while none traversed (straight-line, dominated)
        let mut rs = env.zero;
        for (i, (site, own, of)) in self.sites.iter().enumerate() {
            let arg = b.iconst(Type::I64, LockSite::arg(site.get(), self.off + i) as i64);
            let st = b.call_host(h, &[env.ctx, arg], Some(Type::I64)).unwrap_or(env.zero);
            // below zero: the host's error; above RECHECK: no lock answers it (CONFLICT is ON CONFLICT's)
            let odd = b.icmp(IntCond::Sgt, st, rc);
            let neg = b.icmp(IntCond::Slt, st, env.zero);
            let bad = b.int_bin(IntBinOp::Or, neg, odd);
            let (l_ok, l_on) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(bad, (l_bad, &[st]), (l_ok, &[]));
            b.switch_to(l_ok);
            let is_sk = b.icmp(IntCond::Eq, st, sk);
            b.cond_br(is_sk, (l_join, &row.carry.params()), (l_on, &[]));
            b.switch_to(l_on);
            let is_rc = b.icmp(IntCond::Eq, st, rc);
            let at = b.iconst(Type::I64, recheck(match (*own, src, *of) { (true, Some(src), _) => src, (_, _, Some(of)) => of, _ => site.get() }));
            rs = b.select(is_rc, at, rs);
        }
        let stop = b.icmp(IntCond::Ne, rs, env.zero);
        let (l_rc, l_done) = (b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_rc);
        b.cond_br(stop, (l_rc, &[]), (l_done, &[]));
        b.switch_to(l_rc);
        recheck_exit(b, env, row, self.again, &|_| rs);
        b.switch_to(l_done);
        let c = next(b, env, row);
        b.br(l_join, &c.params());
        b.switch_to(l_bad);
        let st = b.block_params(l_bad)[0];
        let neg = b.icmp(IntCond::Slt, st, env.zero);
        let uns = b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE));
        let err = b.select(neg, st, uns);
        b.br(env.x.l_stop_in, &[row.carry.out, row.at, err]);
        b.switch_to(l_join);
        Carry::from_params(b.block_params(l_join), env.mcx)
    }
}

/// RECHECK (write/lockrows.rs, and each form here): the rows the out block holds are pushed first (the client sees them
/// before the rechecked row, as PG's row order), then the call stops with the status `rs` makes and the cursor past the
/// row. A receiver's stop at that push leaves with its status and the cursor AT the row: the resumed call locks it again
/// (each lock follows the update chain to the version this transaction already holds and rechecks once more), so no row
/// is skipped. D3289: over a join, that stop sets the lock's re-entry word `again` (`StepMech::reentry`), so the
/// expanding steps before the lock hand the same row on again rather than continue past it.
pub(crate) fn recheck_exit(b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, again: Option<i32>, rs: &dyn Fn(&mut FunctionBuilder<'_>) -> Value) {
    let o = b.load(Type::Ptr, env.ctx, hdr::OUT as i32);
    let count = b.load(Type::I64, o, out::COUNT as i32);
    let held = b.icmp(IntCond::Sgt, count, env.zero);
    let (l_push, l_leave) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(held, (l_push, &[]), (l_leave, &[]));
    b.switch_to(l_push);
    let fl = b.call_host(env.core.push, &[env.ctx], Some(Type::I64)).unwrap_or(env.zero);
    let cont = b.icmp(IntCond::Eq, fl, env.zero);
    let l_pushed = b.create_block(&[]);
    match again {
        // D3289: the stop AT the row tells the expanding steps below to hand the same row on again (`Locks::again`)
        Some(w) => {
            let l_held = b.create_block(&[]);
            b.cond_br(cont, (l_pushed, &[]), (l_held, &[]));
            b.switch_to(l_held);
            let one = b.iconst(Type::I64, 1);
            env.store_word(b, w, one);
            b.br(env.x.l_stop_in, &[row.carry.out, row.at, fl]);
        }
        None => b.cond_br(cont, (l_pushed, &[]), (env.x.l_stop_in, &[row.carry.out, row.at, fl])),
    }
    b.switch_to(l_pushed);
    if env.hir.hosted() {
        env.hir.rewind(b);
    }
    b.br(l_leave, &[]);
    b.switch_to(l_leave);
    let rs = rs(b);
    b.br(env.x.l_stop_in, &[row.carry.out, row.next, rs]);
}

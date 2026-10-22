//! D655 (V4R-256; owner M4-WRITE): EvalPlanQual over the relations a write or a lock joins. PostgreSQL marks every
//! non-target relation of an UPDATE / DELETE (and every relation a SELECT ... FOR UPDATE OF does not lock) with
//! ROW_MARK_REFERENCE, and preprocess_targetlist carries its row's ctid as the junk column `ctid{rowmarkId}`. When the
//! write or lock meets a concurrently updated target row, EvalPlanQual reruns the plan with the target's newest version
//! and each marked relation's row refetched by that ctid under SnapshotAny, the exact version the join read
//! (EvalPlanQualFetchRowMark), never its newest one.
//!
//! Here: per row, before the host call, each marked relation's junk ctid is stored as the position of the site that scans
//! it (`store`). The stopped node's positions then hold them; the leader's recheck (exec epq.rs, driver `Entry::recheck`)
//! substitutes every site (the target's newest version, the others at these positions), reruns the producer pipelines
//! (the builds, now of that one row each) and then the stopped pipeline.
//!
//! Admitted (`sites`) when the rechecking pipeline is the program's emit (a producer's recheck would run on no plan) and
//! every other pipeline is a producer whose source is one marked relation's own heap scan (range, index, bitmap), made
//! as its graph pipeline (a join.buildside swap moves sources between pipelines: the site's relation must be the mark's).
//! D732 (V4R-268): a ROW_MARK_COPY mark's row travels whole (`wholerow{rowmarkId}`, a VALUES scan's blessed record):
//! `store` keeps its datum in the scan's copy site, the recheck hands it to that site (`POS_COPY`) and the VALUES source
//! emits that one row from it (EvalPlanQualFetchRowMark's copy leg: never re-evaluated). D732-rest (V4R-290): a subquery's
//! row the same way through its SubqueryScan's copy entry (`copy_row.rs`), the producers only it reads never rerun.
//! D776 (V4R-275): a marked relation the rechecking pipeline scans itself (the target hashed on the build side) keeps its
//! row in a late `SiteKind::RowOf` site (the scan site's `pos` is its window's base); the settle hands it to the scan
//! site's recheck position, and the producer scanning the target reruns at the host's newest version.
//! lockrows NEST (`sites_in` with `inners`, a row lock's): a marked relation an INNER nested loop's inner scans (`nest_joins`:
//! one heap scan under filters and projections, the nest in the rechecking pipeline or in a producer it reruns) keeps its
//! row in a late `SiteKind::RowOf` site of the inner's scan site, as D776's own scan does (the inner's `pos` is its window's
//! base and its resume point); the settle hands it to the inner site's recheck position. At the recheck every relation
//! the nest's outer side reads is substituted (at most one row), so the inner opens once over its substituted source,
//! which hands that row through its keys and recheck quals (hosts `next_substituted`): PostgreSQL's EvalPlanQual rescans
//! the inner over its test tuple (ExecScanReScan re-arms relsubs_done) and its keys recheck it (IndexRecheck,
//! BitmapHeapRecheck).
//! D3278 (DRAINED): a data-modifying CTE nothing reads (its RETURNING rows stored, never read) runs after the statement's
//! emit, and EvalPlanQual never runs it: its pipelines are no product the recheck reads (`drained`).
//! Refused by name: a COPY mark on a function / table function row or a ProjectSet subquery (NotBuilt M5, its kind), a
//! product of any other kind (an InitPlan, a SubPlan, a relation the marks do not name: D652's rest), a marked relation
//! scanned by neither a producer of its own nor the rechecking pipeline (nor, with `inners`, such a nest inner: another
//! nest keeps the refusal it had).
use gravity_abi::ctx::{site_off, POS_ROW, POS_VALID};
use gravity_abi::site::site;
use gravity_decompose::catalog::typefacts::{self, TypeFacts};
use gravity_decompose::ops::shared::modifytable::RowMark;
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::srf::SrfPlace;
use gravity_decompose::{Am, Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, Type};
use gravity_program::{LockKind, SiteDesc};
use std::cell::Cell;
use std::rc::Rc;
use mcx::{CollectIn, GrowIn, Mcx, PgVec};

use crate::face::kinds::{Face, KindId};
use types_nodes::jointype::JoinType;
use types_nodes::plannodes::RowMarkType;

use crate::cell::TIDOID;

const RECORDOID: u32 = 2249;
use crate::frame::env::Env;
use crate::frame::px::{Px, WordKey};
use crate::graph::Gx;
use crate::frame::row::RowVals;
use crate::admit::refusal::{Reason, ShapeWhy};

/// The named refusals (module doc).
pub const COPY_MARK: ShapeWhy = ShapeWhy("a write or lock whose ROW_MARK_COPY relation is no VALUES, subquery, function or table function scan");
pub const EMIT_ONLY: ShapeWhy = ShapeWhy("a rechecking write or lock in a pipeline that produces for another (D655: the recheck reruns the producers, then the emit)");
pub const SWAPPED: ShapeWhy = ShapeWhy("a marked relation whose scan a join.buildside swap moved to another pipeline (D655)");
pub const REJOINED: ShapeWhy = ShapeWhy("a MERGE with other than WHEN MATCHED actions, or a RETURNING list reading a column of another relation than the union's subquery, over a flattened UNION ALL whose members the recheck scans again (PostgreSQL reads the ORIGINAL plan row: D3641)");

/// A marked relation's scan site, the relation, and the child projection's `ctid{rowmarkId}` column; D732: a COPY mark's
/// copy site, its `wholerow{rowmarkId}` column and that column's type facts (`copy`).
#[derive(Clone, Debug)]
pub struct MarkPos {
    pub site: Rc<Cell<usize>>,
    pub relid: u32,
    pub col: i16,
    pub copy: Option<TypeFacts>,
    /// D776: the relation is the rechecking pipeline's own scan, `site` its marked-row site (`SiteKind::RowOf`: the scan
    /// site's `pos` is the window's base, its resume point).
    pub own: bool,
    /// V4R-495, D2048: a REFERENCE mark (its row may be null-extended by an outer join in any pipeline; D1933B: an ANTI
    /// probe's inner, whose ctid is always NULL): `store`.
    pub null_empty: bool,
    /// lockrows NEST: the nest inner's scan site `site` (a RowOf site) is of; a lock's stop names it (its newest version).
    pub of: Option<usize>,
}

/// A write's or lock's marked relations: their positions, and the `tid` type's facts their junk columns are read by.
#[derive(Debug)]
pub struct Marks<'g> {
    pub pos: PgVec<'g, MarkPos>,
    pub tid: Option<TypeFacts>,
    /// lockrows D2604: a family lock over a breaker's `tableoid{rowmarkId}` junk column: a mark whose relation is not the
    /// row's tableoid stores the empty position (an inactive child: ExecLockRows' ermActive false, curCtid invalid; its
    /// scan gives EvalPlanQual no test tuple).
    /// lockrows FAMJOIN: which positions are the family's children (a join's REFERENCE mark beside them is never gated).
    /// lockrows REFFAM: per position, the `tableoid{rowmarkId}` column of the family the mark's relation is a child of (0:
    /// never gated), so a lock's family and a REFERENCE family beside it each gate on their own column.
    pub gate: Option<(TypeFacts, PgVec<'g, i16>)>,
    /// producerchain UNION-COPY: the scan sites of a flattened UNION ALL's members, whose position `store` clears so the
    /// recheck scans them as the statement did (a settled site's `pos` is its scan's own, which the driver would substitute);
    /// producerchain B2: or of a de-duplicated producer's nest inners.
    pub rescan: PgVec<'g, Rc<Cell<usize>>>,
}

impl<'g> Marks<'g> {
    pub fn none(mcx: Mcx<'g>) -> Marks<'g> {
        Marks { pos: PgVec::new_in(mcx), tid: None, gate: None, rescan: PgVec::new_in(mcx) }
    }
}

/// The marks' positions for pipeline `k`'s write or lock (`target`: a result relation's / the locked relation's range
/// table index; marks `marks`), made after the producers: none when pipeline `k` reads no product; else every refusal
/// by name of the module doc. W5 multilock Row A: a lock over several marks names no target, so every locked relation's
/// row is a position too (write/lock_marks.rs locks at it).
pub fn sites<'g>(px: &mut Px<'g, '_>, k: usize, node: i32, marks: &[RowMark], target: &dyn Fn(u32) -> bool) -> Marks<'g> {
    sites_in(px, k, node, marks, target, false)
}

/// `sites`; `inners` (lockrows NEST, module doc): a marked relation a nest inner scans is positioned at that inner's site.
pub fn sites_in<'g>(px: &mut Px<'g, '_>, k: usize, node: i32, marks: &[RowMark], target: &dyn Fn(u32) -> bool, inners: bool) -> Marks<'g> {
    let gx = px.gx;
    let mcx = px.mcx();
    crate::rerun::epq::sub_lock::runner_check(gx, k, node); // lockrows SUBLOCK: the recheck node never locks again (D604)
    // no product: trunk's recheck substitutes the pipeline's own sites (an INSTEAD OF view write's base relation is
    // marked, and scanned here or not at all); lockrows NEST: a nest inner's marked row is stored at its RowOf site
    // mergecmd 9 (COPYROW): or a MERGE's own COPY row (over a nested loop whose outer is a VALUES scan, or a subquery or
    // function row's copy entry): its stored row is the one row the recheck enters with (`copy_pos`)
    let others: PgVec<'_, &RowMark> = marks.iter().filter(|m| !m.parent && !target(m.rti)).collect_in(mcx);
    let write = gx.pipelines()[k].ops.iter().any(|o| matches!(o, Op::Write(_)));
    let merge = gx.pipelines()[k].ops.iter().any(|o| matches!(o, Op::Write(w) if w.cmd == gravity_program::Cmd::Merge));
    // writesubplan A2: or a write's COPY row entering in such a nest's inner (`nest_copy`: a view's OFFSET 0 subquery a
    // rule's OLD reads), handed to the inner's copy entry (`copy_pos`)
    // D3287 (b): or a row lock's COPY row held by such a nest's VALUES inner (`lock_values`)
    let nested = inners && others.iter().any(|m| if m.mark == RowMarkType::ROW_MARK_COPY { write && nest_copy(gx, k, m).is_some() || lock_values(gx, k, m).is_some() } else { nest_scan(gx, k, m).is_some() });
    // D1933B: or an UPDATE / DELETE nest_write.rs `dml` admits over a nested loop (the same one-row recheck): its outer's
    // own COPY row, or its own scan of a REFERENCE-marked relation (D1933 (a): `own_pos` below)
    let nest = gx.pipelines()[k].ops.iter().any(|o| matches!(o, Op::Write(w) if crate::ops::modifytable::nest_write::dml_form(gx, k, w).is_some()));
    // mergecmd 13a: or a MERGE over a nested loop whose outer is a REFERENCE-marked relation's own heap scan (`nest_ref`):
    // its row is refetched at its junk ctid (D776's `own_pos`), the one row the recheck's outer yields
    let own_ref = merge && others.iter().any(|m| nest_ref(gx, k, m));
    // D2772 POST (modifytable, triggers#500): a write beside the data-modifying CTE nothing reads reads none of its
    // pipelines, which run after the statement's own (ops/modifytable/post.rs `reads_product`); D3379 LOCKPOST: a row lock too
    let reads = crate::ops::modifytable::post::reads_product(gx, k);
    if !reads && !nested && !own_ref && !(merge && others.iter().any(|m| m.mark == RowMarkType::ROW_MARK_COPY && own_copy(gx, k, m.rti)) || nest && !others.is_empty()) {
        return Marks::none(mcx);
    }
    let Some(tid) = typefacts::facts_of(gx.facts, TIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the tid type the catalog does not know" }) };
    if gx.pipelines()[k].desc.produces().is_some() {
        shape(node, EMIT_ONLY);
    }
    let mut out: PgVec<'_, MarkPos> = mcx::reserved_in(others.len(), mcx);
    let mut made: PgVec<'_, usize> = mcx::reserved_in(others.len(), mcx);
    let mut skip: PgVec<'_, usize> = PgVec::new_in(mcx);
    // lockrows REFPARENT (`parent_mark`): a write joined to one parent's children; `kin`: which positions are gated
    // D3400 REFPAR2: or several parents' children, each family gated on its own tableoid{rowmarkId} column
    let families = if write { write_families(marks).unwrap_or_default() } else { Vec::new() }; // alloc: Program
    let child = |m: &RowMark| families.iter().any(|&(top, _)| !m.parent && m.prti == top);
    // lockrows REFFAM: any REFERENCE child of an isParent mark, a write's or a lock's (EvalPlanQualFetchRowMark's tableoid
    // test), is gated on its family's tableoid{rowmarkId} column (0: no column, never gated)
    let kin_col = |m: &RowMark| if m.prti != m.rti && !m.parent && m.lock == LockKind::None && m.mark == RowMarkType::ROW_MARK_REFERENCE { m.toid_col } else { 0 };
    let arm_write = |q: usize| gx.pipelines()[q].ops.iter().any(|o| matches!(o, Op::Append(AppendDesc::Arm { .. }))) && gx.pipelines()[q].ops.iter().any(|o| matches!(o, Op::Write(_)));
    let mut kin: PgVec<'_, bool> = mcx::reserved_in(others.len(), mcx);
    let mut entry_own = false;
    let wd = gx.pipelines()[k].ops.iter().find_map(|o| if let Op::Write(w) = o { Some(w) } else { None });
    let members = wd.map_or(&[][..], |w| w.copy_arms);
    let settled = gx.pipelines()[k].ops.iter().find_map(|o| if let Op::Write(w) = o { crate::ops::modifytable::update_built::copied_settled(gx, k, w) } else { None });
    let one = gx.pipelines()[k].ops.iter().any(|o| matches!(o, Op::Write(w) if crate::ops::modifytable::update_built::copied_one(gx, k, w)));
    let mut lone = false;
    let mut rescan: PgVec<'_, Rc<Cell<usize>>> = PgVec::new_in(mcx);
    // D3278 (CTE TWICE): every COPY mark's copy entry, each read below its boundary no rerun (`copy_row::read_elsewhere`)
    let mut copies = crate::rerun::epq::copy_row::entries(gx, others.iter().filter(|m| m.mark == RowMarkType::ROW_MARK_COPY).map(|m| m.rti));
    // D3507 (graphile-worker's get_job: the claim CTE j read by the writing CTE q and the statement's UPDATE): or a
    // copy entry of another write's COPY mark, a data-modifying CTE's UPDATE / DELETE rechecking in its own IR beside
    // this one (lockrows plan.rs `Plan::write_pair`): its pipelines produce nothing this recheck reruns, and its own
    // recheck reads the row its mark stored
    if write {
        copies.extend_in(beside_ir_entries(gx, k));
    }
    for m in others {
        // producerchain UNION-COPY: a flattened UNION ALL's COPY mark (decompose `copy_arms`) stores no row: PostgreSQL's
        // recheck hands it to no scan and scans the members, which no mark names, as the statement did; their producers
        // rerun over their whole scans (`Marks::rescan`). A union whose members no producer scans (constant arms only, as
        // m4_d732's `SELECT 1 UNION ALL SELECT 2`) keeps the COPY row's own form below (its refusal, COPY_MARK)
        let member = |r: u32| members.iter().any(|e| e.0 == m.rti && e.1 == r);
        // D3379 (UNION-COPY, a subquery member): or another arm of the same Append whose member is a subquery the planner
        // keeps (a WHERE over an arm that projects: a Subquery Scan over its relation's scan): no mark names that scan
        // either, so EvalPlanQual scans it again whole, as the relation members
        // D3379 (UNION-DRAINED): or a scan feeding an arm kept as a Subquery Scan over an Append of its own (update_built.rs
        // `nested_arm`): EvalPlanQual runs that subquery again, whole; the recheck's fixpoint below reruns the arm's emit
        // D3497: or a producer scanning the one live member a Subquery Scan keeps (decompose graph `copy_one`, `lone_member`)
        let union_arm = |q: usize| union_arm_of(gx, q, &member) || nested_arm_feed(gx, q, &member) || lone_member(gx, q, m.rti);
        let arms = (0..gx.pipelines().len()).filter(|&q| q != k && (scan_of(&gx.pipelines()[q].source).is_some_and(|(r, _)| member(r)) || union_arm(q)));
        // D3497: or that union's one live member, the rechecking pipeline's own scan (update_built.rs `copied_one`): it
        // scans again whole, its site left unsubstituted (`place_rescans`)
        if one && m.mark == RowMarkType::ROW_MARK_COPY && gx.graph.copy_one.iter().any(|e| e.0 == m.rti) {
            let own = &mut px.ext_mut::<CopySites>(Face::End).2;
            if !own.contains(&k) {
                own.push_in(k);
            }
            lone = true;
            continue;
        }
        // D3477 (settled): or every feed of the union the write's own pipeline reads, whatever its members (a subquery's
        // scan, a relation, a constant row: ops/modifytable/update_built.rs `copied_settled`): each reruns whole, a scan's
        // position cleared, a Result made again
        if let Some(feeds) = settled.as_ref().filter(|_| m.mark == RowMarkType::ROW_MARK_COPY && gx.graph.copy_unions.iter().any(|e| e.0 == m.rti)) {
            for &q in feeds.iter() {
                if scan_of(&gx.pipelines()[q].source).is_some() {
                    let Some(s) = px.pipeline_site(q) else { crate::refuse!(node, Reason::Bug { payload: "a settled union's feed made after the rechecking pipeline" }) };
                    rescan.push_in(Rc::new(Cell::new(s))); // alloc: Program
                }
                made.push_in(q);
            }
            // a MERGE's actions read the ORIGINAL source row on the recheck (ExecMergeMatched's planSlot): the mark's
            // `wholerow{rowmarkId}` record is stored in a copy site of its own, held there on the recheck node (`POS_COPY`),
            // and read back by the end (merge/lower.rs `Settled`); D3496: so does a RETURNING list (returning.rs `Stored`)
            if merge || wd.is_some_and(|w| crate::ops::modifytable::update_built::ret_record(gx, w).is_some()) {
                out.push_in(stored_row(px, node, m));
                kin.push_in(false);
            }
            continue;
        }
        // D3468: a MERGE with NOT MATCHED BY SOURCE actions rechecks its join condition over the newest version and the
        // ORIGINAL source row (ExecMergeMatched's ecxt_innertuple stays context->planSlot), and its target is the outer
        // side, so the members' re-joined rows decide nothing: the union's record row re-joins the stored row (`copy_pos`)
        let original = merge && wd.is_some_and(|w| w.merge.as_ref().is_some_and(|mm| mm.join_cond.is_some())) && gx.graph.record_rows.iter().any(|r| r.scanrelid == m.rti);
        if m.mark == RowMarkType::ROW_MARK_COPY && arms.clone().next().is_some() && !original {
            // D3641: PostgreSQL's MERGE actions and a write's RETURNING read the ORIGINAL plan row after EvalPlanQual
            // (ExecMergeMatched's and ExecProcessReturning's context->planSlot), never the members' re-joined row: a MERGE
            // of WHEN MATCHED actions alone reads the mark's stored record as the settled leg's (merge/settled.rs), and so
            // does a RETURNING list reading the union's own columns (D3496, returning.rs `Stored`); any other MERGE or
            // RETURNING list refuses
            if write && (merge || wd.is_some_and(|w| w.returning.is_some_and(|l| !crate::ops::modifytable::ret_cols::outer(l).is_empty()))) {
                let served = wd.is_some_and(|w| match w.merge.as_ref() {
                    Some(mm) => crate::ops::modifytable::update_built::settled_merge(gx, w, mm),
                    None => crate::ops::modifytable::update_built::ret_record(gx, w).is_some(),
                });
                if !served {
                    shape(node, REJOINED);
                }
                out.push_in(stored_row(px, node, m));
                kin.push_in(false);
                px.ext_mut::<CopySites>(Face::End).3.push_in(m.rti);
            }
            let arms: PgVec<'_, usize> = arms.collect_in(mcx);
            for &q in arms.iter() {
                let Some(s) = px.pipeline_site(q) else { crate::refuse!(node, Reason::Bug { payload: "a union member's producer made after the rechecking pipeline" }) };
                made.push_in(q);
                rescan.push_in(Rc::new(Cell::new(s))); // alloc: Program
            }
            // D3658: and the union's constant rows, made again
            made.extend_in(const_feeds(gx, k, &arms[..]));
            continue;
        }
        if m.mark == RowMarkType::ROW_MARK_COPY {
            let (q, pos, up) = copy_pos(px, k, node, m, write, inners, &copies);
            entry_own |= q == k;
            made.push_in(q);
            made.extend_in(up.iter().copied()); // D732-rest: never rerun (recheck_skip)
            skip.extend_in(up);
            out.push_in(pos);
            kin.push_in(false);
            continue;
        }
        if m.lock == LockKind::None && m.mark != RowMarkType::ROW_MARK_REFERENCE || m.ctid_col <= 0 {
            crate::refuse!(node, Reason::Bug { payload: "a non-target mark that neither locks nor is REFERENCE / COPY, or no ctid junk column" });
        }
        // D3278 (SAMPLE, `sample_of`): or its TABLESAMPLE scan, the substituted row handed on without the sample filter
        let scans = |q: &usize| q != &k && (scan_of(&gx.pipelines()[*q].source).is_some_and(|(r, relid)| r == m.rti && relid == m.relid) || sample_of(&gx.pipelines()[*q].source, m));
        let Some(q) = (0..gx.pipelines().len()).find(scans) else {
            // producerchain B2 (nest_ref.rs `deduped_inner`): the relation a nest inner of the producer of the group a
            // write's target nest reads scans stores no row: the recheck reruns that producer with the inner scanning
            // again under the statement's snapshot (its site's position cleared before the host call, `Marks::rescan`)
            if let Some((pq, ik)) = crate::ops::modifytable::nest_ref::deduped_inner(gx, k, m).filter(|_| write) {
                let Some(s) = px.pipeline_site(ik) else { crate::refuse!(node, Reason::Bug { payload: "a de-duplicated producer's nest inner whose scan site is not numbered" }) };
                if !made.contains(&pq) {
                    made.push_in(pq);
                }
                rescan.push_in(Rc::new(Cell::new(s))); // alloc: Program
                continue;
            }
            match nest_scan(gx, k, m).filter(|_| inners) {
                Some(ik) => out.push_in(inner_pos(px, node, m, ik)),
                None => out.push_in(own_pos(px, k, node, m)),
            }
            kin.push_in(child(m) || kin_col(m) > 0);
            continue;
        };
        // lockrows REFPARENT: a child another write arm scans (the parent's Append on the emit side) holds no row of this
        // arm's: PG's fetch of that child finds another tableoid and gives none, and the recheck never reruns that arm
        if child(m) && arm_write(q) && arm_write(k) {
            continue;
        }
        // modifytable FAMNEST (write/nest_ref.rs `family_inner`): a child a nest inner's union arm scans keeps its row in a
        // RowOf site of the arm's scan site, as `inner_pos` (the arm scans on while the inner reads: its position is its
        // resume point), gated on the family's tableoid; the recheck reopens the inner over that row or none
        if inners && write && child(m) && crate::ops::modifytable::nest_ref::family_feed(gx, k, q) {
            out.push_in(inner_pos(px, node, m, q));
            made.push_in(q);
            kin.push_in(true);
            continue;
        }
        let pq = &gx.pipelines()[q];
        if pq.desc.produces().is_none() || pq.ops.iter().any(|o| matches!(o, Op::Write(_) | Op::Lock(_))) {
            shape(node, crate::admit::refusal::READS_PRODUCT);
        }
        if sample_of(&pq.source, m) {
            fires::fire("epq.sample_ref"); // shapecheck fire signal (D3278 SAMPLE): a recheck substitutes a mark's row in a TABLESAMPLE scan
        }
        let Some(s) = px.pipeline_site(q) else { crate::refuse!(node, Reason::Bug { payload: "a marked relation's producer made after the rechecking pipeline" }) };
        if px.sites.get(s).is_none_or(|d| d.relation != m.relid) {
            shape(node, SWAPPED);
        }
        made.push_in(q);
        // D2048: a NULL ctid gives no row whichever pipeline's outer join null-extended it (EvalPlanQualFetchRowMark's
        // "non-locked rels could be on the inside of outer joins"): a producer's LEFT join too, not only the stopped one's
        out.push_in(MarkPos { site: Rc::new(Cell::new(s)), relid: m.relid, col: m.ctid_col, copy: None, own: false, null_empty: true, of: None }); // alloc: Program
        kin.push_in(child(m) || kin_col(m) > 0);
    }
    // D776: with a mark on the pipeline's own scan, the target is scanned by a producer (the build side of the join the
    // pipeline probes): the recheck reruns it at the host's newest version (the end names that producer's site)
    // D1249: or the emit drains a merge join's sorted outer (a breaker source): the target's sorted producer reruns so,
    // and a producer draining one the recheck reruns (merge.join's inner Materialize over its Sort) reruns after it
    // producerchain cluster A: or a write's COPY row entering on the pipeline itself (W5-C's own entry over a heap scan of
    // the copied row's subquery, or its FROM function): the target's producer reruns so, the emit from the stored row
    // D3497: or the emit's own scan of a union's lone member, which the recheck scans again whole
    if out.iter().any(|p: &MarkPos| p.own) || write && (entry_own || lone) || matches!(gx.pipelines()[k].source, Source::Breaker { .. }) && gx.pipelines()[k].ops.iter().any(|o| matches!(o, Op::Write(_))) {
        made.extend_in((0..gx.pipelines().len()).filter(|q| *q != k && scan_of(&gx.pipelines()[*q].source).is_some_and(|(r, _)| target(r))));
    }
    // every other producer is one marked relation's (an InitPlan, a SubPlan, another product: D652's rest); another emit
    // only a sibling Append arm (a write of several result relations, a lock's family), which the recheck steps over
    let arm = |q: usize| gx.pipelines()[q].ops.iter().any(|o| matches!(o, Op::Append(AppendDesc::Arm { .. })));
    let product = |q: usize| gx.pipelines()[q].desc.produces().is_some();
    // M8-WRITE W5a: an InitPlan's members run once per execution (nest.initplan's Init cell); the recheck reads its value
    // as PostgreSQL's EvalPlanQualStart copies an evaluated InitPlan's param (and evaluates a pending one), never reruns them;
    // V4R-527: a lock's when lock_init.rs admits each (attached at the lock or above, forced by every row reaching it)
    let init = crate::ops::lockrows::lock_init::members(px, k, node, write, init_members);
    // D3278 (SUBPLAN, `sub_producer`): a SubPlan's materialized rows rerun at the recheck as PG's EPQ runs the subplan again
    let subs: PgVec<'_, usize> = (0..gx.pipelines().len()).filter(|&q| q != k && !made.contains(&q) && !init.contains(&q) && sub_producer(gx, k, q, marks, target)).collect_in(mcx);
    if !subs.is_empty() {
        fires::fire("epq.subplan_rerun"); // shapecheck fire signal (D3278 SUBPLAN): a SubPlan's producer in the recheck's rerun set
    }
    made.extend_in(subs);
    // D3418 CONST-BUILD (merge-update's merge2a: `USING (SELECT 1 AS key, ..) s` under WHEN NOT MATCHED BY SOURCE, a Hash
    // Full Join that hashes the flattened source's Result): a producer over a Result with no child reruns at a write's
    // recheck, as EvalPlanQual evaluates that Result again in its own plan tree; only one a rerun pipeline (or `k`) reads
    if write {
        let reads = |r: usize, q: usize, s: u8| gx.pipelines()[r].desc.consumes.contains(&s) && gx.life_of(r, s).is_some_and(|l| l.0 as usize == q);
        let consts: PgVec<'_, usize> = (0..gx.pipelines().len()).filter(|&q| q != k && !made.contains(&q) && !init.contains(&q) && const_producer(gx, q) && gx.pipelines()[q].desc.produces().is_some_and(|s| made.iter().copied().chain([k]).any(|r| reads(r, q, s)))).collect_in(mcx);
        if !consts.is_empty() {
            fires::fire("epq.const_build"); // shapecheck fire signal (D3418 CONST-BUILD): a recheck reruns a Result's producer
        }
        made.extend_in(consts);
    }
    // D1249 / W5-J (D652-J): a write's recheck reruns, to a fixpoint, every producer whose every slot a rerun producer
    // makes (a merge join's inner Materialize over its Sort, an Agg / Unique over a marked scan, a view's join chain)
    // (a recheck_skip producer's slot opens empty on the recheck: never a rerun producer's input)
    let made_slot = |made: &[usize], slot: u8| made.iter().any(|&m| !skip.contains(&m) && gx.pipelines()[m].desc.produces() == Some(slot));
    let reruns = |made: &[usize], q: usize| {
        let p = &gx.pipelines()[q];
        let own = |o: &Op<'_>| matches!(o, Op::Write(_) | Op::Lock(_) | Op::Join(JoinDesc::Nest { .. }));
        product(q) && matches!(p.source, Source::Breaker { .. }) && !p.ops.iter().any(own) && p.desc.consumes.iter().all(|&slot| made_slot(made, slot))
    };
    // lockrows REFFAM: a lock's recheck reruns a settled union's reader (decompose append.rs `settle_arms`: a family's
    // union under a build) once it reruns every feed of the union's spool: the recheck's spool is its own and empty (exec
    // rerun/epq.rs `fresh_spools`), as EvalPlanQualStart's fresh tuplestores; a feed the recheck skips leaves the reader
    // out of the set (D652 below)
    let lock = !write && gx.pipelines()[k].ops.iter().any(|o| matches!(o, Op::Lock(_)));
    // the pipelines filling each slot `q` reads: its union's feeds (rerun/union_join_arms.rs `feeds`: after the slot's previous
    // reader, a slot being reused after its life), else the slot's last producer before `q`
    let fills = |q: usize, slot: u8| match crate::rerun::union_arms::union_of(&gx.pipelines()[q]) {
        Some((s, _)) if s == slot => crate::rerun::union_join_arms::feeds(gx, q, slot),
        _ => (0..q).rev().find(|&m| gx.pipelines()[m].desc.produces() == Some(slot)).into_iter().collect(),
    };
    let feeds_made = |made: &[usize], q: usize| gx.pipelines()[q].desc.consumes.iter().all(|&slot| { let f = fills(q, slot); !f.is_empty() && f.iter().all(|m| made.contains(m) && !skip.contains(m)) });
    let union_of = |q: usize| crate::rerun::union_arms::union_reader(&gx.pipelines()[q]);
    let union_reader = |made: &[usize], q: usize| union_of(q) && feeds_made(made, q);
    while let Some(q) = (0..gx.pipelines().len()).find(|&q| (write || lock && union_reader(&made[..], q)) && q != k && !made.contains(&q) && !init.contains(&q) && reruns(&made[..], q)) {
        made.push_in(q);
    }
    // V4R-498: the planner's dummy relation (a partition-less root's target) never emits a row: nothing to rerun
    let dummy = |q: usize| crate::ops::shared::dummy::dummy(gx, q, gx.pipelines()[q].ops.len());
    // writesubplan (D1112, a MERGE action's rerun set): the members of a rerun set whose SubPlan only the MERGE's actions
    // read, lent at the action's read (merge/admit.rs `action_sub`), are stepped past by the recheck (exec rerun_producer,
    // Entry::skip_rerun) and rerun by the action at its read over the recheck node, with the newest version's arguments, as
    // EvalPlanQual re-executes the SubPlan (its scans unsubstituted: D1478's Rescan protos)
    let pk = &gx.pipelines()[k];
    let wk = pk.ops.iter().find_map(|o| if let Op::Write(w) = o { Some(w) } else { None });
    // writesubplan (merge#297): or one only the write's RETURNING list reads, a step after the write (returning.rs
    // `ret_only_sub`): the recheck steps past its members as any rerun set's, the step reruns it per returned row
    let ret = |i: usize, o: &Op<'_>, w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>| match o {
        Op::SubPlan(d) if pk.ops[..i].iter().any(|o| matches!(o, Op::Write(_))) && crate::ops::modifytable::returning::ret_only_sub(w, d.plan_id) => Some(d.plan_id),
        _ => None,
    };
    // writesubplan (merge#340): or a SubPlan step a MERGE chain's join qual reads (`join_qual_sub`): the recheck's rerun of
    // the chain's probe reads it over the new pair, rerunning its set as EvalPlanQual re-evaluates the join filter's SubPlan
    let qual = |o: &Op<'_>, w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>| match o {
        Op::SubPlan(d) if w.merge.is_some() && join_qual_sub(o) => Some(d.plan_id),
        _ => None,
    };
    let acts: PgVec<'_, i32> = wk.map_or_else(|| PgVec::new_in(mcx), |w| pk.ops.iter().enumerate().filter_map(|(i, o)| if crate::ops::modifytable::merge::admit::action_sub(o, w) { crate::graph::lazy_nested_sub(o) } else { ret(i, o, w).or_else(|| qual(o, w)) }).collect_in(mcx));
    let act_rerun = |q: usize| gx.graph.reruns.iter().any(|r| acts.contains(&r.plan_id) && r.pipelines.iter().any(|&m| usize::from(m) == q));
    // mergecmd ACTION-CTE (regress with#239 / #241): or a pulled CTE only those action SubPlans read (`act_ctes`)
    let act_cte = act_ctes(gx, k, wk, &act_rerun);
    let act_set = |q: usize| act_rerun(q) || act_cte.contains(&q);
    // subplan (W6-D1112 row B): the set of a check option's hashed SubPlan only the write's host-fed probe reads (`wco_sets`)
    let wco = wco_sets(gx, k);
    // lockrows REFFAM: the lock's own pipeline over a settled union (ops/lockrows/marks.rs `make`) reads every feed's rows
    let lock_union = lock && union_of(k) && !feeds_made(&made[..], k);
    // D3278 (DRAINED, `drained`): an unread data-modifying CTE's pipelines run after the statement's emit; no recheck reads them
    let drained = drained(gx);
    if !drained.is_empty() {
        fires::fire("epq.drained_cte"); // shapecheck fire signal (D3278 DRAINED): a recheck beside an unread writing CTE's pipelines
    }
    // D3379 SUBSET (`sub_sets`): a lock's correlated SubPlan's rerun set whose step a producer the recheck reruns holds;
    // D3702 MOVESET (`move_sets`): or a moving UPDATE's
    let moving = write && wd.is_some_and(moves);
    // D3826: or the SubPlan steps before a write's nest (ops/modifytable/nest_write.rs `dml_form`), the outer's filter
    let nested = write && !moving && wd.is_some_and(|w| crate::ops::modifytable::nest_write::dml_form(gx, k, w).is_some());
    let subset = if lock { sub_sets(gx, k, &made[..], &skip[..]) } else if moving || nested { move_sets(gx, k, &made[..], &skip[..]) } else { PgVec::new_in(mcx) };
    // D3568 HASHSET (`hashed_sets`): or the set a hashed SubPlan step there probes, rebuilt or probed as built
    // D3778: or a write's, its sets all kept ones (`kept_sets`)
    let hashed = if lock { hashed_sets(gx, k, &made[..], &skip[..]) } else if write { kept_sets(gx, k, &made[..], &skip[..]) } else { PgVec::new_in(mcx) };
    if subset.iter().any(|q| !made.contains(q) && !init.contains(q)) {
        fires::fire("epq.sub_set"); // shapecheck fire signal (D3379 SUBSET): a lock's recheck reruns a producer's SubPlan step over its set
    }
    // D3379 LOCKPOST: and the post pipelines (`after_emit`), which run after the emit
    let after = gx.after_emit();
    if lock_union || (0..gx.pipelines().len()).any(|q| q != k && !init.contains(&q) && !dummy(q) && !act_set(q) && !wco.contains(&q) && !after.contains(&q) && !subset.contains(&q) && !hashed.contains(&q) && if product(q) { !made.contains(&q) } else { !arm(q) }) {
        shape(node, crate::admit::refusal::READS_PRODUCT);
    }
    // D2050: a write's or lock's outer-first INNER / SEMI probe over a build the recheck reruns builds first from the
    // recheck after one whose outer reached it (PostgreSQL keeps hj_OuterNotEmpty): `epq_first` names its build to the driver
    epq_first(px, k, &made[..], &skip[..]);
    // lockrows REFPARENT / REFFAM: each child's position is active only where the row's tableoid names it (`store`'s gate)
    // REFFAM: a gated position's column is its own mark's family tableoid{rowmarkId} junk column (the mark at the position's
    // relation and ctid column: a self-joined tree's two families differ in that column), else the write family's
    let toid = |i: usize| out.get(i).and_then(|p| marks.iter().find(|m| !m.parent && m.relid == p.relid && m.ctid_col == p.col)).map_or(0, |m| m.toid_col.max(0));
    let cols: PgVec<'_, i16> = kin.iter().enumerate().map(|(i, &g)| if !g { 0 } else if toid(i) > 0 { toid(i) } else if families.len() == 1 { families[0].1 } else { 0 }).collect_in(mcx);
    let gate = match cols.iter().any(|&c| c > 0) {
        true => {
            let Some(of) = typefacts::facts_of(gx.facts, types_core::catalog::OIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the oid type the catalog does not know" }) };
            Some((of, cols))
        }
        false => None,
    };
    Marks { pos: out, tid: Some(tid), gate, rescan }
}

/// A union COPY mark's `wholerow{rowmarkId}` record stored in a copy site of its own (made once per mark), held there on
/// the recheck node (`POS_COPY`) for the readers of the ORIGINAL row (merge/settled.rs).
fn stored_row(px: &mut Px<'_, '_>, node: i32, m: &RowMark) -> MarkPos {
    let made_site = px.ext::<CopySites>(Face::End).and_then(|c| c.0.iter().find(|e| e.0 == m.rti)).map(|e| e.1.clone());
    let site = made_site.unwrap_or_else(|| {
        let site = px.add_late_site(SiteDesc::copy(px.arena, m.rti));
        px.ext_mut::<CopySites>(Face::End).0.push_in((m.rti, site.clone()));
        site
    });
    let Some(tf) = typefacts::facts_of(px.gx.facts, RECORDOID) else { crate::refuse!(node, Reason::Bug { payload: "the record type the catalog does not know" }) };
    if m.ctid_col <= 0 {
        crate::refuse!(node, Reason::Bug { payload: "a settled union's COPY mark without its wholerow junk column" });
    }
    MarkPos { site, relid: 0, col: m.ctid_col, copy: Some(tf), own: false, null_empty: false, of: None }
}

/// D3379 (UNION-COPY, a subquery member): pipeline `q` is an arm of the same Append (its arm op's plan node) as a pipeline
/// scanning a member relation of the mark (`member`), both `arm_node`'s shape.
fn union_arm_of(gx: &Gx<'_, '_>, q: usize, member: &dyn Fn(u32) -> bool) -> bool {
    let ps = gx.pipelines();
    let Some(n) = arm_node(&ps[q]) else { return false };
    (0..ps.len()).any(|x| x != q && scan_of(&ps[x].source).is_some_and(|(r, _)| member(r)) && arm_node(&ps[x]) == Some(n))
}

/// D3658: the constant feeds (a Result with no child, under filters and projections) of the unions member pipelines `arms`
/// feed: EvalPlanQual scans every member of the union again, a constant arm's row made again.
fn const_feeds<'g>(gx: &Gx<'g, '_>, k: usize, arms: &[usize]) -> PgVec<'g, usize> {
    use crate::rerun::union_arms::feed_of;
    let ps = gx.pipelines();
    let slots: PgVec<'_, u8> = arms.iter().filter_map(|&q| feed_of(&ps[q])).collect_in(gx.mcx);
    let simple = |q: usize| ps[q].ops.split_last().is_some_and(|(last, body)| !matches!(last, Op::Filter(_) | Op::Project(_)) && body.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))));
    let fed = |q: usize| feed_of(&ps[q]).is_some_and(|s| slots.contains(&s));
    (0..ps.len()).filter(|&q| q != k && !arms.contains(&q) && matches!(ps[q].source, Source::Const { .. }) && ps[q].desc.consumes.is_empty() && fed(q) && simple(q)).collect_in(gx.mcx)
}

/// D3497: pipeline `q` scans the one live member of the flattened UNION ALL of COPY mark `rti` (decompose graph `copy_one`:
/// no Append carries the mark), a heap range, index or bitmap scan under filters and projections up to that member's node
/// (its Subquery Scan): EvalPlanQual scans it again whole.
fn lone_member(gx: &Gx<'_, '_>, q: usize, rti: u32) -> bool {
    let p = &gx.pipelines()[q];
    let scan = matches!(p.source, Source::Range { am: Am::Heap, .. } | Source::Index { am: Am::Heap, only: false, .. } | Source::Bitmap { am: Am::Heap, .. });
    let at = |node: i32| p.node_ops.iter().find(|n| n.node_id == node).map(|n| usize::from(n.first_op));
    let quiet = |j: usize| p.ops.iter().take(j).all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_)));
    scan && gx.graph.copy_one.iter().any(|e| e.0 == rti && at(e.1).is_some_and(quiet))
}

/// D3379 (UNION-DRAINED): pipeline `q` feeds the settled union of an arm PostgreSQL keeps as a Subquery Scan over an Append
/// of its own (update_built.rs `nested_arm`), that arm a feed of the union a member relation's scan of the mark feeds too.
fn nested_arm_feed(gx: &Gx<'_, '_>, q: usize, member: &dyn Fn(u32) -> bool) -> bool {
    use crate::rerun::union_arms::{feed_of, union_of};
    let ps = gx.pipelines();
    let Some(v) = feed_of(&ps[q]) else { return false };
    let Some(e) = (q + 1..ps.len()).find(|&x| union_of(&ps[x]).is_some_and(|u| u.0 == v)) else { return false };
    let Some(u) = feed_of(&ps[e]) else { return false };
    let scans_member = |x: usize| x != q && scan_of(&ps[x].source).is_some_and(|(r, _)| member(r)) && feed_of(&ps[x]) == Some(u);
    crate::ops::modifytable::update_built::nested_arm(gx, e, u).is_some_and(|f| f.contains(&q)) && (0..ps.len()).any(scans_member)
}

/// The plan node of the Append whose arm pipeline `p` is: a heap range or index scan under filters and projections, its one
/// arm op, then the op that hands its rows on (the union's Sort or Feed), reading no other product; else none.
fn arm_node(p: &gravity_decompose::PipelineNode<'_>) -> Option<i32> {
    let scan = matches!(p.source, Source::Range { am: Am::Heap, .. } | Source::Index { am: Am::Heap, only: false, .. });
    let (last, body) = p.ops.split_last()?;
    let mut arm = body.iter().enumerate().filter(|(_, o)| crate::graph::is_arm(o));
    let (Some((j, _)), None) = (arm.next(), arm.next()) else { return None };
    let quiet = body.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_)) || crate::graph::is_arm(o));
    let hands = matches!(last, Op::Sort(_) | Op::Append(AppendDesc::Feed { .. }));
    (scan && quiet && hands && p.desc.consumes.is_empty()).then(|| crate::rerun::inner::op_node(p, j))
}

/// D3418: a producer of a Result with no child (Source::Const) under filters, gates and projections, ending in the
/// breaker its last op makes: it reads no relation and no other product, so its rerun makes the rows the run made.
pub(crate) fn const_producer(gx: &Gx<'_, '_>, q: usize) -> bool {
    let p = &gx.pipelines()[q];
    let quiet = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    matches!(p.source, Source::Const { .. }) && p.desc.consumes.is_empty() && p.ops.split_last().is_some_and(|(l, before)| matches!(l, Op::Join(JoinDesc::Build { .. })) && before.iter().all(quiet))
}

/// D2050: the BuildSide proto of the build that pipeline `k`'s marked probe (`epq_probe`) reads, when the recheck reruns it
/// (`made`, never a `skip` producer: its slot opens empty), gets program `BUILDSIDE_EPQ_FIRST`. PostgreSQL's write keeps its
/// EvalPlanQual tree for the statement, and ExecReScanHashJoin keeps `hj_OuterNotEmpty` across the rechecks' rebuilds. So
/// once a recheck's outer side reached the join, later rechecks build first and, over an empty build, never run the outer
/// side (its VOLATILE calls, its errors). D2674: the proto names the probe's frame word (`EPQ_WORD`), where its call records
/// PG's points that set the flag; the driver's `Entry::rerun` reads it after each called recheck and skips the stopped call
/// over such an empty build while it is set.
fn epq_first(px: &mut Px<'_, '_>, k: usize, made: &[usize], skip: &[usize]) {
    let gx = px.gx;
    let Some(j) = epq_probe(gx, k) else { return };
    let Some(Op::Join(JoinDesc::Probe { slot, .. })) = gx.pipelines()[k].ops.get(j) else { return };
    let Some(b) = gx.life_of(k, *slot).map(|l| l.0 as usize).filter(|b| made.contains(b) && !skip.contains(b)) else { return };
    let word = u64::from(px.frame_word(WordKey::Open { face: Face::Join, k, j: crate::frame::px::key_j(j), i: EPQ_WORD })) + 1;
    let id = px.ident(b) as u16;
    let ours = |p: &gravity_program::protos::Proto| p.kind == gravity_program::protos::ProtoKind::BuildSide && p.pipeline == id && p.peer as usize == k && p.read[0].is_some();
    let arg = gravity_program::protos::BUILDSIDE_EPQ_FIRST | word << gravity_program::protos::BUILDSIDE_EPQ_WORD_SHIFT;
    px.protos.iter_mut().filter(|p| ours(p)).for_each(|p| p.arg |= arg);
}

/// D2674: the hashjoin probe's open-word index (`WordKey::Open`, `Face::Join`) of its `hj_OuterNotEmpty` record.
pub(crate) const EPQ_WORD: u8 = 6;

/// D2033 step 2 (lockrows rerun.rs `first_held`): build `b` of probe `j` of pipeline `k` is op 14's held build (hashjoin
/// build_class.rs `defers`) and takes no lazy road over a recursive union (`endless`): its merge holds its error for the
/// probe's first row.
pub(crate) fn held_build(gx: &Gx<'_, '_>, b: usize, k: usize, j: usize) -> bool {
    !crate::ops::hashjoin::build_class::endless(gx, b, k) && crate::ops::hashjoin::build_class::defers(gx, b, k, j)
}

/// D2050 / D2674: the probe of graph pipeline `k` a write's or lock's recheck reads PG's `hj_OuterNotEmpty` at: `k` writes
/// or locks (ExecLockRows keeps its EvalPlanQual tree for the statement too), and its last probe is an outer-first INNER or
/// SEMI hash join (not psql's catalog form). Only the topmost join of the pipeline:
/// a skip over its empty build is PG's (its build-first call returns before anything below it runs, its flag kept), and no
/// join above it resets a flag PG would read. The probe (hashjoin probe.rs) records PG's points in `EPQ_WORD`.
pub(crate) fn epq_probe(gx: &Gx<'_, '_>, k: usize) -> Option<usize> {
    if k >= gx.pipelines().len() {
        return None;
    }
    let p = &gx.pipelines()[k];
    if !p.ops.iter().any(|o| matches!(o, Op::Write(_) | Op::Lock(_))) {
        return None;
    }
    let j = p.ops.iter().rposition(|o| matches!(o, Op::Join(JoinDesc::Probe { .. })))?;
    matches!(&p.ops[j], Op::Join(JoinDesc::Probe { outer_first: true, jointype: JoinType::JOIN_INNER | JoinType::JOIN_SEMI, catalog: false, .. })).then_some(j)
}

/// mergecmd ACTION-CTE (regress with#239 / #241): the pipeline of each pulled CTE (`SubLink::Cte`, a plan of one pipeline:
/// no producer of its own) that only a MERGE action's lazily lent SubPlan reads (merge/admit.rs `action_sub`: its nested
/// inners under pipeline `k`, never `k`'s own source; or a member of its rerun set, `act`). PostgreSQL's EvalPlanQualStart
/// copies the parent's param values, so the recheck's CteScan finds the statement's leader and reads (and pulls on) the
/// statement's one tuplestore (nodeCtescan.c), never the CTE run again: the recheck steps past that plan as the statement
/// does (exec runner `rerun_producer`, `Entry::skip_pulled`), and the action's re-lend over the recheck node reads the
/// statement's store through the CTE's object (its words the statement's).
fn act_ctes<'g>(gx: &Gx<'g, '_>, k: usize, w: Option<&gravity_decompose::ops::shared::modifytable::WriteDesc<'_>>, act: &dyn Fn(usize) -> bool) -> PgVec<'g, usize> {
    let Some(w) = w.filter(|w| w.cmd == gravity_program::Cmd::Merge) else { return PgVec::new_in(gx.mcx) };
    let ps = gx.pipelines();
    let n = ps.len();
    let inners = ps.inners();
    // the op of pipeline `k` embedded inner `i` lies under (through its nesting), if `k` is its top owner
    let top = |mut i: usize| loop {
        let (o, j) = inners[i].0;
        if o < n {
            return (o == k).then_some(j);
        }
        i = o - n;
    };
    let read = |x: &gravity_decompose::PipelineNode<'_>, slot: u8| matches!(x.source, Source::Breaker { slot: s } if s == slot);
    let lent = |slot: u8| !read(&ps[k], slot) && inners.iter().enumerate().filter(|(_, x)| read(x.1, slot)).all(|(i, _)| top(i).is_some_and(|j| crate::ops::modifytable::merge::admit::action_sub(&ps[k].ops[j], w)));
    let descs: PgVec<'_, gravity_program::Pipeline> = ps.iter().map(|p| p.desc.clone()).collect_in(gx.mcx);
    let d = &descs[..];
    let readers = |c: usize| (0..n).filter(|&q| q != c && gravity_program::lives::reads(d, q, gravity_program::LifeId(c as u16))).collect_in(gx.mcx);
    let ctes = gx.graph.init_plans.iter().filter(|i| matches!(i.link, gravity_decompose::ops::shared::subplan::SubLink::Cte)).map(|i| i.pipeline as usize);
    ctes.filter(|&c| c < n && c != k && ps[c].desc.consumes.is_empty() && ps[c].desc.produces().is_some_and(lent)).filter(|&c| {
        let r = readers(c);
        !r.is_empty() && r.iter().all(|&q| q == k || act(q))
    })
    .collect_in(gx.mcx)
}

/// D3278 (DRAINED): the pipelines of every data-modifying CTE no pipeline reads, its RETURNING rows going to a store
/// nothing reads (ctescan's DRAIN pulls it to its end after the statement's emit, PG's ExecPostprocessPlan): the CTE's
/// pipeline and, to a fixpoint, every producer only those read. PostgreSQL's EvalPlanQual tree holds no CteScan of such a
/// CTE, so its recheck never runs it: a recheck neither reruns nor reads these pipelines (`Pipeline::recheck_skip`, the
/// driver steps past them unopened, frame/program.rs). A slot another life reuses counts as read (fail closed). Only a
/// CTE whose every write has a RETURNING list: one without stores nothing, so nothing drains it; post.rs runs it after
/// the statement's own write (D2772 POST, D3379 POSTSAME over the same relations), else it runs in D1687's order, before
/// that write, where PG runs it after.
pub fn drained(gx: &Gx<'_, '_>) -> Vec<usize> {
    let ps = gx.pipelines();
    let n = ps.len();
    let readers = |q: usize| ps[q].desc.produces().map_or_else(|| PgVec::new_in(gx.mcx), |s| (0..n).filter(|&m| m != q && ps[m].desc.consumes.contains(&s)).collect_in(gx.mcx));
    let writes = |c: usize| ps[c].ops.iter().any(|o| matches!(o, Op::Write(w) if w.cmd != gravity_program::Cmd::Select));
    let stored = |c: usize| ps[c].ops.iter().all(|o| !matches!(o, Op::Write(w) if w.cmd != gravity_program::Cmd::Select && w.returning.is_none()));
    let mut set: Vec<usize> = (0..n).filter(|&c| gx.cte_write(c) && writes(c) && stored(c) && ps[c].desc.produces().is_some() && readers(c).is_empty()).collect(); // alloc: Program
    if set.is_empty() {
        return set;
    }
    while let Some(q) = (0..n).find(|&q| !set.contains(&q) && { let r = readers(q); !r.is_empty() && r.iter().all(|x| set.contains(x)) }) {
        set.push(q); // alloc: Program
    }
    set
}

/// D3379 SUBSET: the members of every correlated SubPlan's rerun set whose step lies in the stopped pipeline `k` or in a
/// producer the recheck reruns (`made`, not `skip`), to a fixpoint through the members' own steps. The recheck steps past
/// the members (exec runner `rerun_producer`, `Entry::skip_rerun`) and the step reruns its set on the recheck node over
/// the newest version's arguments, its scans unsubstituted (D1478's Rescan protos), as EvalPlanQual re-evaluates the
/// SubPlan in its plan copy: a lock over a Sort whose scan carries the SubPlan (the ORDER BY forms).
fn sub_sets<'g>(gx: &Gx<'g, '_>, k: usize, made: &[usize], skip: &[usize]) -> PgVec<'g, usize> {
    let ps = gx.pipelines();
    let mut runs: PgVec<'_, usize> = made.iter().copied().filter(|q| !skip.contains(q)).chain([k]).collect_in(gx.mcx);
    let mut out: PgVec<'_, usize> = PgVec::new_in(gx.mcx);
    loop {
        let plans: PgVec<'_, i32> = runs.iter().flat_map(|&q| ps[q].ops.iter()).filter_map(|o| if let Op::SubPlan(d) = o { Some(d.plan_id) } else { None }).collect_in(gx.mcx);
        let mut more: PgVec<'_, usize> = gx.graph.reruns.iter().filter(|r| r.plan_id != 0 && plans.contains(&r.plan_id)).flat_map(|r| r.pipelines.iter().map(|&m| usize::from(m))).filter(|m| *m != k && !out.contains(m)).collect_in(gx.mcx);
        more.sort_unstable();
        more.dedup();
        if more.is_empty() {
            return out;
        }
        out.extend_in(more.iter().copied());
        runs = more;
    }
}

/// D3702 MOVESET: whether write `w` is an UPDATE that may move a row to another partition (a SET of a partition key: its
/// EvalPlanQual is ExecCrossPartitionUpdate's, so rowop_epq.rs `subs_form` keeps the runner's recheck).
fn moves(w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>) -> bool {
    use gravity_decompose::ops::shared::modifytable::Member;
    w.cmd == gravity_program::Cmd::Update && w.merge.is_none() && (w.members.has(Member::CrossPartition) || w.route.is_some_and(|r| r.cross_partition))
}

/// D3702 MOVESET: `sub_sets` for a moving UPDATE's pipeline `k` (`moves`), the steps of its sibling arms (the other result
/// relations' arms of its write) counted too. The recheck steps past every member (exec runner `rerun_producer`,
/// `Entry::skip_rerun`): `k`'s steps rerun their sets on the recheck node over the newest version, a qual's as EvalPlanQual
/// re-runs the arm's quals and a check option's as the WCO PostgreSQL runs over the row the recheck made (ExecUpdate, or
/// ExecInsert for a moved row); a sibling arm's are never run there, as EvalPlanQual runs no other result relation's plan.
/// None when a member is one a set keeps from its first run (`graph.kept`): the recheck node never made that first run.
fn move_sets<'g>(gx: &Gx<'g, '_>, k: usize, made: &[usize], skip: &[usize]) -> PgVec<'g, usize> {
    let ps = gx.pipelines();
    let sibling = |q: usize| q != k && ps[q].ops.iter().any(|o| matches!(o, Op::Append(AppendDesc::Arm { .. }))) && ps[q].ops.iter().any(|o| matches!(o, Op::Write(_)));
    let runs: PgVec<'_, usize> = made.iter().copied().chain((0..ps.len()).filter(|&q| sibling(q))).collect_in(gx.mcx);
    let sets = sub_sets(gx, k, &runs[..], skip);
    if sets.iter().any(|&q| gx.graph.kept.iter().any(|&m| usize::from(m) == q)) {
        return PgVec::new_in(gx.mcx);
    }
    sets
}

/// D3568 HASHSET (Odoo's cron claim: `SELECT * FROM ir_cron WHERE active AND (nextcall <= now() OR EXISTS (SELECT
/// cron_id FROM ir_cron_trigger WHERE call_at <= now() AND cron_id = ir_cron.id)) AND id = %s ORDER BY priority FOR NO
/// KEY UPDATE SKIP LOCKED`, the EXISTS a hashed SubPlan in the scan's filter): the pipelines that build the sets the
/// hashed SubPlan steps of the stopped pipeline `k` or of a producer the recheck reruns (`made`, not `skip`) probe:
/// each set's producer and, to a fixpoint, a product only those read; none when one of them writes or locks. A
/// SubPlan's relations carry no row mark: PostgreSQL's EvalPlanQual initializes the SubPlan again in its recheck state
/// and builds its hash table at its first probe, scanning them under the statement's snapshot, so over the rows the
/// statement's set holds. The recheck node steps past these builds (`hashed_skipped`: a rebuild over the statement's
/// sealed set would insert into it), and the probe reads the set as built, or builds it there at its first probe (a
/// LAZY set the run never probed, D3285): those rows either way.
fn hashed_sets<'g>(gx: &Gx<'g, '_>, k: usize, made: &[usize], skip: &[usize]) -> PgVec<'g, usize> {
    use gravity_decompose::ops::shared::subplan::SubForm;
    let ps = gx.pipelines();
    let runs = made.iter().copied().filter(|q| !skip.contains(q)).chain([k]);
    // each probe's set: the nearest producer of its slot before the probing pipeline
    let mut out: PgVec<'_, usize> = PgVec::new_in(gx.mcx);
    for r in runs {
        for o in ps[r].ops.iter() {
            let Op::SubPlan(d) = o else { continue };
            let SubForm::Hashed { slot } = d.form else { continue };
            match (0..r).rev().find(|&q| ps[q].desc.produces() == Some(slot)) {
                Some(q) if !out.contains(&q) => out.push_in(q),
                Some(_) => {}
                None => return PgVec::new_in(gx.mcx),
            }
        }
    }
    // a product only those builds read (a join inside the set's subquery)
    let only = |out: &[usize], q: usize| ps[q].desc.produces().is_some_and(|s| {
        let mut readers = (0..ps.len()).filter(|&r| ps[r].desc.consumes.contains(&s)).peekable();
        readers.peek().is_some() && readers.all(|r| out.contains(&r))
    });
    while let Some(q) = (0..ps.len()).find(|&q| q != k && !out.contains(&q) && only(&out[..], q)) {
        out.push_in(q);
    }
    let acts = |q: &usize| ps[*q].ops.iter().any(|o| matches!(o, Op::Write(_) | Op::Lock(_)));
    if out.iter().any(acts) {
        return PgVec::new_in(gx.mcx);
    }
    out
}

/// D3778 (TypeORM's closure-table DELETE: `DELETE FROM c WHERE des IN (SELECT des FROM c s WHERE s.anc = $1) AND anc
/// NOT IN (SELECT des FROM c a WHERE a.anc = $1)`, PostgreSQL 18.6's Hash Join whose probe, the target's scan, filters
/// by a hashed NOT IN): `hashed_sets` for the write of pipeline `k` the runner rechecks (`runner_write`), when no set's
/// plan acts (subplan hashed.rs `lazy_build`: a VOLATILE call or a lock runs again in PostgreSQL's rebuild, D3678's
/// twins), else none. EvalPlanQual builds each set again in its fresh SubPlan state from the same plan under the
/// statement's snapshot, so over the rows the statement's set holds: the probe reads that set, as a lock's does.
fn kept_sets<'g>(gx: &Gx<'g, '_>, k: usize, made: &[usize], skip: &[usize]) -> PgVec<'g, usize> {
    use gravity_decompose::ops::shared::subplan::SubForm;
    let ps = gx.pipelines();
    let lazy = |q: &usize| matches!(ps[*q].ops.last(), Some(Op::SubPlan(d)) if matches!(d.form, SubForm::Build) && crate::ops::subplan::hashed::lazy_build(d));
    let sets = hashed_sets(gx, k, made, skip);
    if !runner_write(gx, k) || sets.iter().any(lazy) {
        return PgVec::new_in(gx.mcx);
    }
    sets
}

/// D3778: pipeline `k` holds the statement's UPDATE / DELETE, rechecked by the runner (not in the IR:
/// ops/modifytable/rowop_epq.rs `in_ir`).
fn runner_write(gx: &Gx<'_, '_>, k: usize) -> bool {
    let p = &gx.pipelines()[k];
    p.desc.produces().is_none() && p.ops.iter().any(|o| matches!(o, Op::Write(w) if matches!(w.cmd, gravity_program::Cmd::Update | gravity_program::Cmd::Delete))) && !crate::ops::modifytable::rowop_epq::in_ir(gx, k)
}

/// D3568 HASHSET: whether graph pipeline `q` builds a set the recheck of a row lock (a lock pipeline `k` that writes
/// nothing) probes, among the `hashed_sets` of the pipelines before `k`: a recheck never reruns it
/// (`Pipeline::recheck_skip`), its probe reading the statement's set object. D3778: or of a write's (`kept_sets`).
pub fn hashed_skipped(gx: &Gx<'_, '_>, q: usize) -> bool {
    let ps = gx.pipelines();
    let lock = |k: usize| ps[k].ops.iter().any(|o| matches!(o, Op::Lock(_))) && !ps[k].ops.iter().any(|o| matches!(o, Op::Write(_))) || runner_write(gx, k);
    // D3585: in the graph's lowering, a `q` after the last locking pipeline answers at once (memo.rs): no walk of every
    // later pipeline for each `q` (quadratic in an Append's arms)
    let key = crate::memo::key_of(&gx.graph.pipelines);
    let last = crate::memo::read(key, |m| m.last_lock).map(|x| {
        x.unwrap_or_else(|| {
            let last = (0..ps.len()).rev().find(|&k| lock(k)).map(|k| k as u32);
            crate::memo::write(key, |m| m.last_lock = Some(last));
            last
        })
    });
    if last.is_some_and(|l| l.is_none_or(|l| l as usize <= q)) {
        return false;
    }
    let sets = |k: usize| if runner_write(gx, k) { kept_sets(gx, k, &(0..k).collect_in(gx.mcx)[..], &[]) } else { hashed_sets(gx, k, &(0..k).collect_in(gx.mcx)[..], &[]) };
    (q + 1..ps.len()).any(|k| lock(k) && sets(k).contains(&q))
}

/// D3278 (SUBPLAN): pipeline `q` materializes the rows of a SubPlan only the rechecking pipeline `k` reads, through a nested
/// SubPlan op of its own (a Materialize under an uncorrelated `ALL` / `ANY` / `EXISTS` / scalar subquery: no slot of a
/// graph pipeline or of any other inner reads them), from a source no row mark names (a subquery's relations are never
/// marked), reading no other product. PostgreSQL's EvalPlanQual initializes every SubPlan again in its recheck EState
/// without EXEC_FLAG_REWIND, so the Materialize hands its child's rows through and each evaluation of the SubPlan runs the
/// subplan again: one run per recheck of a row that reaches the op once. The recheck reruns `q` (the driver reruns every
/// producer before the stopped pipeline on the recheck node, whose store is its own). When the subplan's rows can differ or
/// it calls a VOLATILE function (`invariant` false: a nextval counts the runs), the op is reached by every rechecked row,
/// as PG's is: unconditional (`conditional` false), only projections and other SubPlan ops before it in `k`, and `k`'s
/// source a marked relation's or the write target's own scan (the recheck's one test row).
fn sub_producer(gx: &Gx<'_, '_>, k: usize, q: usize, marks: &[RowMark], target: &dyn Fn(u32) -> bool) -> bool {
    use gravity_decompose::ops::shared::subplan::{SubForm, SubPlanDesc};
    let ps = gx.pipelines();
    let p = &ps[q];
    let Some(s) = p.desc.produces() else { return false };
    let marked = |rti: u32| marks.iter().any(|m| m.rti == rti);
    let source = match &p.source {
        Source::Range { scanrelid, .. } | Source::Index { scanrelid, .. } | Source::Bitmap { scanrelid, .. } => !marked(u32::from(*scanrelid)),
        Source::Const | Source::Values { .. } => true,
        _ => false,
    };
    // `k`'s descriptor lists the slots its inners read too: it may read `s` only through them (no source, probe, merge or
    // hashed SubPlan of its own over `s`)
    let at_k = matches!(ps[k].source, Source::Breaker { slot } if slot == s)
        || ps[k].ops.iter().any(|o| matches!(o, Op::Join(JoinDesc::Probe { slot, .. } | JoinDesc::Merge { slot, .. }) | Op::SubPlan(SubPlanDesc { form: SubForm::Hashed { slot }, .. }) if *slot == s));
    if !source || at_k || !p.desc.consumes.is_empty() || p.ops.iter().any(|o| matches!(o, Op::Write(_) | Op::Lock(_))) || (0..ps.len()).any(|m| m != q && m != k && ps[m].desc.consumes.contains(&s)) {
        return false;
    }
    let own = scan_of(&ps[k].source).is_some_and(|(r, _)| marked(r) || target(r));
    let ops = &ps[k].ops;
    // D3498: a pulled fill (rerun/spool/form.rs `spool_pull`) runs nothing on the rerun; the reader pulls the fresh record's
    // child only when the test row reaches the op, so a filter or gate before it may drop the row first, as PG's qual does
    let pulled = crate::pipes::spool_pull(gx, q).is_ok();
    let before = |o: &Op<'_>| matches!(o, Op::Project(_) | Op::SubPlan(_)) || pulled && matches!(o, Op::Filter(_) | Op::Gate(_));
    let reached = |j: usize, d: &SubPlanDesc<'_>| d.invariant || own && !d.conditional && ops[..j].iter().all(before);
    let direct = |x: &(usize, usize)| x.0 == k && matches!(&ops[x.1], Op::SubPlan(d @ SubPlanDesc { form: SubForm::Nested { .. }, .. }) if reached(x.1, d));
    let readers: PgVec<'_, (usize, usize)> = ps.inners().into_iter().filter(|x| x.1.desc.consumes.contains(&s)).map(|x| x.0).collect_in(gx.mcx);
    !readers.is_empty() && readers.iter().all(direct)
}

/// W5a: the pipelines of every InitPlan's on-demand run (its pipeline and exclusive producers: `members_in`, the
/// runtime's closure), which the recheck never reruns (and the write road's admission, write_road.rs, never asks).
pub(crate) fn init_members<'g>(px: &Px<'g, '_>) -> PgVec<'g, usize> {
    let gx = px.gx;
    let descs: PgVec<'_, gravity_program::Pipeline> = gx.pipelines().iter().map(|p| p.desc.clone()).collect_in(gx.mcx);
    let inits = gx.graph.init_plans.iter().filter(|i| !matches!(i.link, gravity_decompose::ops::shared::subplan::SubLink::Cte));
    let mut set: PgVec<'g, usize> = inits.flat_map(|i| gravity_program::protos::members_in(&descs[..], &px.protos, i.pipeline as usize).members).map(usize::from).collect_in(gx.mcx);
    // mergecmd landing 7: a pulled CTE (its readers pull it, so no Init's closure holds it: protos::producers_in) that
    // only those members read is the Init's too: the recheck reads the Init cell, never the CTE (EvalPlanQualStart's
    // copy); to a fixpoint (a CTE read only by another such CTE)
    let ctes = || gx.graph.init_plans.iter().filter(|i| matches!(i.link, gravity_decompose::ops::shared::subplan::SubLink::Cte)).map(|i| i.pipeline as usize);
    let d = &descs[..];
    let readers = move |c: usize| (0..d.len()).filter(move |&q| q != c && gravity_program::lives::reads(d, q, gravity_program::LifeId(c as u16)));
    while let Some(c) = ctes().find(|&c| !set.contains(&c) && readers(c).next().is_some() && readers(c).all(|q| set.contains(&q))) {
        let more: PgVec<'_, usize> = gravity_program::protos::members_in(&descs[..], &px.protos, c).members.into_iter().map(usize::from).filter(|q| !set.contains(q)).collect_in(gx.mcx);
        set.extend_in(more);
    }
    set
}

/// D732: a COPY mark's producer (a VALUES scan of its range table index, made as its graph pipeline, writing and locking
/// nothing) and its copy site (`source/values.rs` adds it when `copied` names the scan). D732-rest: a subquery's, the
/// producer running its SubqueryScan (its copy entry, `copy_row.rs`), with the producers only the part below its entry
/// reads (never rerun). D890: a function row's, the producer running its FROM step (entered past it). A CTE row, and a
/// function row without a record row (a composite member), refuse by name; a subquery the rechecking pipeline runs
/// itself (on the join's probe side, its target on the build side) D652. writesubplan A2 (a write's, with `inners`): last,
/// the nest inner whose SubqueryScan it is (`nest_copy`): the inner's copy entry (rerun/inner.rs) hands its re-open that row.
fn copy_pos<'g>(px: &mut Px<'g, '_>, k: usize, node: i32, m: &RowMark, write: bool, inners: bool, beside: &[(usize, usize)]) -> (usize, MarkPos, PgVec<'g, usize>) {
    let gx = px.gx;
    let values = |q: &usize| q != &k && own_values(gx, *q, m.rti);
    let rec = gx.graph.record_rows.iter().find(|r| r.scanrelid == m.rti);
    let subquery = |q: &usize| rec.is_some_and(|r| crate::rerun::epq::copy_row::boundary(gx, *q, r).is_some());
    // mergecmd 9 (COPYROW): last, the rechecking pipeline's own VALUES scan (W5-C's own entry, a VALUES one)
    let mine = own_values(gx, k, m.rti);
    // D3592 (Odoo 18's claim, the runner's recheck): or a lock's COPY row whose Subquery Scan a producer's nest inner
    // runs
    let nested = || if inners && write { nest_copy(gx, k, m) } else if inners { lock_values(gx, k, m).or_else(|| producer_copy(gx, k, m)) } else { None };
    let Some(q) = (0..gx.pipelines().len()).find(values).or_else(|| (0..gx.pipelines().len()).find(subquery)).or_else(nested).or(mine.then_some(k)) else { copy_kind(gx, node, m.rti) };
    let at = rec.filter(|_| !values(&q)).and_then(|r| crate::rerun::epq::copy_row::boundary(gx, q, r)).unwrap_or(0);
    let up = crate::rerun::epq::copy_row::upstream(gx, q, at);
    let pq = &gx.pipelines()[q];
    // W5-C (I44): the copy entry on the rechecking pipeline itself (its target on a join's build side): the recheck reruns
    // k from its entry over the stored row, never the part below it; a producer's must make its product
    // lockrows JOBQ (IN form): a lock below the entry (the subquery's own LockRows) never runs on the recheck, which enters
    // at the SubqueryScan with the stored row; plan.rs `Plan::write_pair` serves that lock beside the write
    let own = q == k && gx.pipelines()[k].ops.iter().take(at).all(|o| !matches!(o, Op::Write(_) | Op::Lock(_)));
    let acts = pq.ops.iter().any(|o| matches!(o, Op::Write(_))) || pq.ops.iter().skip(at).any(|o| matches!(o, Op::Lock(_)));
    let inner = q >= gx.pipelines().len();
    if !own && !inner && (q == k || pq.desc.produces().is_none() || acts) || m.ctid_col <= 0 {
        shape(node, crate::admit::refusal::READS_PRODUCT);
    }
    // mergecmd 7c: a write's InitPlan members (W5a) read the never-rerun part too (a MATERIALIZED CTE the COPY row's
    // subquery and an action's InitPlan both scan): the recheck never reruns them either, and an on-demand run reads
    // the execution's products (PostgreSQL forces a pending one in the parent's estate), so they are no reader here
    let quiet: PgVec<'_, usize> = if write { up.iter().copied().chain(init_members(px)).collect_in(gx.mcx) } else { mcx::to_vec_in(&up[..], gx.mcx) };
    // COPY-SCAN: a nest inner's slots are its owner's consumes too (decompose breakers.rs `embed`), so `k` reading them
    // only through the inner `q` is no reader past the entry (`only_through`)
    // D3592: the inner's owner, a producer the recheck reruns (`producer_copy`), or `k`
    let owner = if inner { gx.pipelines().inners()[q - gx.pipelines().len()].0 .0 } else { k };
    let through = inner && only_through(gx, owner, q, &quiet[..]);
    if up.contains(&k) || crate::rerun::epq::copy_row::read_elsewhere(gx, q, at, &quiet[..], beside) && !through {
        shape(node, crate::admit::refusal::READS_PRODUCT);
    }
    if crate::rerun::epq::copy_row::read_elsewhere(gx, q, at, &quiet[..], &[]) && !through {
        fires::fire("epq.copy_twice"); // shapecheck fire signal (D3278 CTE TWICE): another copy entry reads this mark's producers below its boundary
    }
    // W5-C: the rechecking pipeline's own entry is made here, before its source (copy_row::entry reuses it there); its
    // own VALUES scan's copy site too (mergecmd 9: values/lower.rs reuses it)
    if own && px.ext::<CopySites>(Face::End).is_none_or(|c| c.0.iter().all(|e| e.0 != m.rti)) {
        if mine {
            let site = px.add_late_site(SiteDesc::copy(px.arena, m.rti));
            px.ext_mut::<CopySites>(Face::End).0.push_in((m.rti, site));
        } else if crate::rerun::epq::copy_row::entry(px, k).is_none() {
            shape(node, crate::admit::refusal::READS_PRODUCT);
        }
    }
    // mergecmd VALUES-INNER: a nest inner's VALUES scan (`nest_copy`) may be made after the end; its copy site is made
    // here then (values/lower.rs reuses it); a `nest_copy_scan` inner's too (copy_row.rs `entry` reuses it)
    if inner && px.ext::<CopySites>(Face::End).is_none_or(|c| c.0.iter().all(|e| e.0 != m.rti)) {
        let site = px.add_late_site(SiteDesc::copy(px.arena, m.rti));
        px.ext_mut::<CopySites>(Face::End).0.push_in((m.rti, site));
    }
    let copy = px.ext::<CopySites>(Face::End).and_then(|c| c.0.iter().find(|e| e.0 == m.rti)).map(|e| e.1.clone());
    let Some(site) = copy else { crate::refuse!(node, Reason::Bug { payload: "a COPY mark's scan made without its copy site" }) };
    let Some(tf) = typefacts::facts_of(gx.facts, RECORDOID) else { crate::refuse!(node, Reason::Bug { payload: "the record type the catalog does not know" }) };
    (q, MarkPos { site, relid: 0, col: m.ctid_col, copy: Some(tf), own: false, null_empty: false, of: None }, up)
}

/// D3507: the copy entries (copy_row.rs `entries`) of the COPY marks of every data-modifying CTE's UPDATE / DELETE
/// other than pipeline `k`'s that rechecks in its own IR (modifytable rowop_epq.rs `cte_in_ir`).
fn beside_ir_entries<'g>(gx: &Gx<'g, '_>, k: usize) -> PgVec<'g, (usize, usize)> {
    let ps = gx.pipelines();
    let write = |c: usize| ps[c].ops.iter().find_map(|o| if let Op::Write(w) = o { Some(w) } else { None });
    let ctes = (0..ps.len()).filter(|&c| c != k && crate::ops::modifytable::rowop_epq::cte_in_ir(gx, c));
    let rtis = ctes.filter_map(write).flat_map(|w| w.mark_descs.iter().filter(|m| m.mark == RowMarkType::ROW_MARK_COPY).map(|m| m.rti));
    let mut out = crate::rerun::epq::copy_row::entries(gx, rtis.into_iter());
    // D3650 NEST-COPY: or a CTE write's COPY row entering its nest's inner (rowop_epq.rs `copy_outer`): the pipeline
    // reads the store through that nest op
    out.extend_in((0..ps.len()).filter(|&c| c != k).filter_map(|c| crate::ops::modifytable::rowop_epq::copy_outer_entry(gx, c)));
    out
}

/// A COPY row a later milestone builds, refused by its scan's own kind (M5, main 09-26): a JSON_TABLE / XMLTABLE row
/// srf.tablefunc, a function RTE's without a record row (D890: a composite member) srf.function, a subquery over a
/// ProjectSet (its SubqueryScan elided) srf.projectset.
fn copy_kind(gx: &Gx<'_, '_>, node: i32, rti: u32) -> ! {
    let ps = gx.pipelines();
    let srf = |f: &dyn Fn(&SrfPlace) -> bool| ps.iter().flat_map(|p| p.ops.iter()).any(|o| matches!(o, Op::Srf(d) if f(&d.place)));
    let k = if ps.iter().any(|p| matches!(p.source, Source::TableFunc { scanrelid, .. } if u32::from(scanrelid) == rti)) {
        KindId::SrfTablefunc
    } else if srf(&|p| matches!(p, SrfPlace::From { scanrelid, .. } if u32::from(*scanrelid) == rti)) {
        KindId::SrfFunction
    } else if srf(&|p| matches!(p, SrfPlace::TargetList)) {
        KindId::SrfProjectset
    } else {
        shape(node, COPY_MARK)
    };
    crate::not_built!(node, crate::admit::refusal::Variant::Kind(k))
}

/// D776: a marked relation the rechecking pipeline scans itself (its source: a heap scan of that relation): the row is
/// stored in a late marked-row site of that scan site, which the settle hands the recheck as the scan's position; else
/// D652. The end is made before the frame numbers pipeline `k`'s own source site, so the site's `of` is a draft
/// (`DRAFT_OF`, pipeline `k` in `index`) that `place_rows` fills when the late sites are numbered.
fn own_pos(px: &mut Px<'_, '_>, k: usize, node: i32, m: &RowMark) -> MarkPos {
    // D3278 (SAMPLE, `sample_of`): or the pipeline's own TABLESAMPLE scan of it (the probe side of the join)
    let own = |s: &Source<'_>| scan_of(s).is_some_and(|(r, relid)| r == m.rti && relid == m.relid) || sample_of(s, m);
    if !own(&px.gx.pipelines()[k].source) {
        shape(node, crate::admit::refusal::READS_PRODUCT);
    }
    if sample_of(&px.gx.pipelines()[k].source, m) {
        fires::fire("epq.sample_ref"); // shapecheck fire signal (D3278 SAMPLE): a recheck substitutes a mark's row in a TABLESAMPLE scan
    }
    let mut d = SiteDesc::row_of(px.arena, DRAFT_OF, m.relid);
    d.index = k as u32;
    px.ext_mut::<CopySites>(Face::End).1.push_in((k, node));
    let site = px.add_late_site(d);
    MarkPos { site, relid: m.relid, col: m.ctid_col, copy: None, own: true, null_empty: false, of: None }
}

/// lockrows NEST (module doc): the embedded inner (its `Pipes` index) whose source is mark `m`'s relation's heap scan, when
/// its nest is `nest_joins`' in a graph pipeline (`k`, or a producer the recheck reruns) and every op before the nest hands
/// on at most one row per substituted row (`one_row`); else none (the mark keeps the refusal it had).
pub fn nest_scan(gx: &Gx<'_, '_>, k: usize, m: &RowMark) -> Option<usize> {
    let ps = gx.pipelines();
    let inners = ps.inners();
    let i = inners.iter().position(|x| scan_of(&x.1.source).is_some_and(|(r, relid)| r == m.rti && relid == m.relid))?;
    let (q, j) = inners[i].0;
    // producerchain G2: or, for an UPDATE / DELETE nest_write.rs `dml_form` admits, a LEFT nest in its own pipeline
    // (`nest_left_joins`): a NULL row (the nest null-extended) gives `inner_pos`'s null_empty site no row, so the recheck
    // null-extends again (EvalPlanQualFetchRowMark's no-row for a relation inside an outer join); every lock path unchanged
    // mergecmd BY-SOURCE-NEST: or a MERGE nest_ref.rs `chain` admits (its LEFT nest over one heap range scan: `left_range`)
    let merge_left = |w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>| w.cmd == gravity_program::Cmd::Merge && crate::ops::modifytable::nest_ref::chain(gx, k, w);
    let write = || ps[k].ops.iter().any(|o| matches!(o, Op::Write(w) if crate::ops::modifytable::nest_write::dml_form(gx, k, w).is_some() || merge_left(w)));
    // D3287 (b): or a row lock's LEFT nest in its own pipeline (lockrows L1 / marks.rs `left_joins`), the same no-row
    let lock = || ps[k].ops.iter().any(|o| matches!(o, Op::Lock(_)));
    let left = |o: &Op<'_>| q == k && nest_left_joins(o) && (write() || lock());
    // modifytable ANTINEST: or an ANTI nest in the own pipeline of an UPDATE / DELETE nest_ref.rs `chain` admits
    // (`nest_anti_joins`): its rows carry the inner's ctid NULL, so `inner_pos`'s null_empty site holds no row and the
    // recheck's re-open of the inner yields none: the target's newest version passes the anti join, as PostgreSQL's
    // EvalPlanQualFetchRowMark gives no test tuple for the NULL ctid
    // D3851: or one after the target's nest of an UPDATE / DELETE `dml_form` admits (its `ref_after`)
    let anti = |o: &Op<'_>| q == k && nest_anti_joins(o) && (ps[k].ops.iter().any(|o| matches!(o, Op::Write(w) if crate::ops::modifytable::nest_ref::chain(gx, k, w))) || write());
    // modifytable SEMINEST: or a SEMI nest in the own pipeline of an UPDATE / DELETE nest_ref.rs `chain` admits
    // (`nest_semi_joins`): its row carries the first matching inner row's ctid, so `inner_pos`'s site holds that row and the
    // recheck's re-open of the inner yields it alone, as PostgreSQL's EvalPlanQualFetchRowMark hands the semi join's inner
    // the refetched row (REFRESH MATERIALIZED VIEW CONCURRENTLY's DELETE: Nested Loop Semi Join of the matview and diff)
    let chain = || ps[k].ops.iter().any(|o| matches!(o, Op::Write(w) if crate::ops::modifytable::nest_ref::chain(gx, k, w)));
    let semi = |o: &Op<'_>| q == k && (nest_semi_joins(o) || nest_semi_deep(o)) && chain();
    // D3739: or a `nest_joins` nest inside the inner of such a `nest_semi_deep` nest of `k` (its outer the inner `q`)
    let deep = |o: &Op<'_>| q >= ps.len() && nest_joins(o) && inners.get(q - ps.len()).is_some_and(|x| x.0 .0 == k && ps[k].ops.get(x.0 .1).is_some_and(nest_semi_deep)) && chain();
    // D3287 (b): under a row lock, an INNER nest over a constant Result before it hands on one row too (`nest_const`)
    // D3611 (Hibernate JOINED inheritance's lock, a LEFT join per subclass table): or a lock's LEFT nest over a marked
    // relation's scan (`left`): the recheck re-opens that inner over its refetched row or none, one row either way
    // D3851 (Rails' update_all through an inner join plus LEFT joins: Nested Loop Left Join (Nested Loop (Nested Loop (Hash
    // Left Join (comments, Hash of the parent), the target's index scan), posts' index scan), the grandparent's index
    // scan)): or, for an UPDATE / DELETE `dml_form` admits, a LEFT or ANTI hash probe (nest_write.rs G6: its build reruns
    // over its relation's refetched row, or none) or a LEFT nest (G2: its inner re-opened over the refetched row, or
    // none) in its own pipeline: each hands on one row per substituted row
    let left_write = |o: &Op<'_>| q == k && (left_joins(o) || matches!(o, Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_ANTI, .. }))) && write();
    let before = |o: &Op<'_>| one_row(o) || q == k && (nest_const(o) || nest_left_joins(o)) && lock() || left_write(o);
    // D3291: or a row lock's nest whose inner holds one Memoize (`nest_memo_joins`), in its own pipeline; D3688: or an
    // UPDATE's / DELETE's nest_ref.rs `chain` admits
    let memo = |o: &Op<'_>| q == k && nest_memo_joins(o) && (lock() || ps[k].ops.iter().any(|o| matches!(o, Op::Write(w) if crate::ops::modifytable::nest_ref::chain(gx, k, w))));
    // D3871 (Hibernate ORM 7.4.12's MutationQueriesAndNotFoundActionTest#testUpdate: `update comments set text=$1 from
    // comments c1_0 join users u1_0 on u1_0.id=c1_0.user_id where u1_0.id=$2 and comments.ctid=c1_0.ctid`, PostgreSQL
    // 18.6's Nested Loop (Nested Loop (Seq Scan c1_0, Tid Scan comments), Index Scan users)): or, before the nest, the
    // write's own target nest in its own pipeline (ops/modifytable/nest_write.rs `dml_form`: its inner the target's range,
    // index or TID scan): the recheck opens it once over the target's newest version, one row
    let target = |i: usize| {
        q == k && ps.inner_of(k, i).is_some_and(|ik| ps[k].ops.iter().any(|o| matches!(o, Op::Write(w) if crate::ops::modifytable::nest_write::dml_form(gx, k, w) == Some(ik))))
    };
    let ok = q < ps.len() && (nest_joins(&ps[q].ops[j]) || memo(&ps[q].ops[j]) || left(&ps[q].ops[j]) || anti(&ps[q].ops[j]) || semi(&ps[q].ops[j])) && ps[q].ops[..j].iter().enumerate().all(|(i, o)| before(o) || target(i)) && (q == k || ps[q].desc.produces().is_some())
        || deep(&ps[q].ops[j]) && ps[q].ops[..j].iter().all(one_row);
    ok.then_some(ps.len() + i)
}

/// writesubplan A2: the embedded inner (its `Pipes` index) running COPY mark `m`'s SubqueryScan, when its nest is a
/// `nest_joins` nest of pipeline `k` itself and every op before the nest hands on at most one row per substituted row
/// (`one_row`). PostgreSQL's EvalPlanQual hands that SubqueryScan the stored row (EvalPlanQualFetchRowMark's copy leg),
/// its quals rechecked over it, the subquery below never run again.
/// mergecmd VALUES-INNER (updatable_views#822 / #826): or the inner whose source is the mark's VALUES scan (`nest_values`):
/// the recheck's re-open of it yields the stored row alone (values/lower.rs under `POS_COPY`), as PostgreSQL's
/// EvalPlanQual hands the Values Scan (under the Materialize decompose drops) its stored row. Or a `nest_copy_scan` inner
/// over the mark's Function Scan or CTE Scan (its copy entry: rerun/inner.rs `copy_walk`).
pub fn nest_copy(gx: &Gx<'_, '_>, k: usize, m: &RowMark) -> Option<usize> {
    let ps = gx.pipelines();
    // a function row's record row names no plan node (its FROM step places it): only a `nest_copy_scan` inner takes it
    let r = gx.graph.record_rows.iter().find(|r| r.scanrelid == m.rti);
    let inners = ps.inners();
    let values = |i: usize| matches!(inners[i].1.source, Source::Values { scanrelid, .. } if u32::from(scanrelid) == m.rti);
    let i = (0..inners.len()).find(|&i| r.is_some_and(|r| crate::rerun::epq::copy_row::boundary(gx, ps.len() + i, r).is_some()) || values(i))?;
    let (q, j) = inners[i].0;
    let planned = r.is_some_and(|r| r.node_id >= 0);
    let nest = if values(i) { nest_values(&ps[q].ops[j]) } else { planned && nest_joins(&ps[q].ops[j]) || nest_copy_scan(&ps[q].ops[j]) };
    // D3705: or, for an UPDATE / DELETE, the ops below the pipeline's own COPY row entry (merge/admit.rs `own_entry`), which
    // the recheck enters past
    let entry = ps[k].ops.iter().find_map(|o| match o { Op::Write(w) if matches!(w.cmd, gravity_program::Cmd::Update | gravity_program::Cmd::Delete) => Some(crate::ops::modifytable::merge::admit::own_entry(gx, k, w)), _ => None }).filter(|&e| e <= j).unwrap_or(0);
    let ok = m.mark == RowMarkType::ROW_MARK_COPY && q == k && nest && ps[q].ops[..j].iter().enumerate().all(|(i, o)| one_row(o) || i < entry && crate::ops::modifytable::merge::admit::subquery_own(o));
    ok.then_some(ps.len() + i)
}

/// D3592 (Odoo 18's cron claim: `WITH last_cron_progress AS (.. LIMIT 1) SELECT * FROM ir_cron LEFT JOIN
/// last_cron_progress lcp ON .. WHERE .. AND (nextcall <= .. OR EXISTS (SELECT .. FROM ir_cron_trigger ..)) AND id = %s
/// ORDER BY priority FOR NO KEY UPDATE OF ir_cron SKIP LOCKED`, its filter's hashed SubPlan keeping the IR's REFCOPY
/// off, ops/lockrows/lower.rs `ref_copy`): the embedded inner (its `Pipes` index) running row lock `k`'s COPY mark
/// `m`'s Subquery Scan, when its nest is a producer's that the recheck reruns (one before `k` making a product, writing
/// and locking nothing), INNER or LEFT with no parameters, entering the stored row as rerun/epq/ref_copy.rs `inner`
/// takes it, past ops that hand on at most one row per substituted row (`one_row`). The rerun nest re-opens the inner
/// over the stored row (rerun/inner.rs `copy_walk`; none for a null-extended row's, D1685), as PostgreSQL's
/// EvalPlanQual hands the Subquery Scan the row's stored record.
fn producer_copy(gx: &Gx<'_, '_>, k: usize, m: &RowMark) -> Option<usize> {
    let ps = gx.pipelines();
    let inners = ps.inners();
    (0..inners.len()).find_map(|i| {
        let (q, j) = inners[i].0;
        let quiet = q < k && ps[q].desc.produces().is_some() && ps[q].ops[..j].iter().all(one_row) && !ps[q].ops.iter().any(|o| matches!(o, Op::Write(_) | Op::Lock(_)));
        let at = crate::ops::lockrows::plan::Census(gx).inner_index(i);
        (quiet && crate::rerun::epq::ref_copy::inner(gx, q, j, m).is_some_and(|(x, _, _)| x == at)).then_some(at)
    })
}

/// D3287 (b): `nest_copy`'s VALUES inner under a row lock (ops/lockrows/lower.rs `admit`'s `join`: FOR UPDATE OF one
/// relation joined to a VALUES list, an INNER nest whose inner is the COPY mark's Values Scan): the runner's recheck
/// re-opens that inner over the stored row alone (values/lower.rs under `POS_COPY`), as PostgreSQL's EvalPlanQual hands the
/// Values Scan its test tuple (EvalPlanQualFetchRowMark's copy leg). A subquery's copy entry under a lock keeps its refusal.
fn lock_values(gx: &Gx<'_, '_>, k: usize, m: &RowMark) -> Option<usize> {
    let ps = gx.pipelines();
    if !ps[k].ops.iter().any(|o| matches!(o, Op::Lock(_))) {
        return None;
    }
    let i = nest_copy(gx, k, m)?;
    let inners = ps.inners();
    matches!(inners.get(i - ps.len()).map(|x| &x.1.source), Some(Source::Values { scanrelid, .. }) if u32::from(*scanrelid) == m.rti).then_some(i)
}

/// D3287 (b): whether op `o` of pipeline `k` is a `nest_values` nest whose VALUES scan's COPY mark (of `marks`, a row
/// lock's) `lock_values` positions at that inner: ops/lockrows/lower.rs `admit` takes only such a nest, so a VALUES inner the recheck would
/// re-open whole (no stored row) keeps its refusal (NOT_OWN).
pub(crate) fn lock_values_nest(gx: &Gx<'_, '_>, k: usize, o: &Op<'_>, marks: &[RowMark]) -> bool {
    let Op::Join(JoinDesc::Nest { inner, .. }) = o else { return false };
    let Source::Values { scanrelid, .. } = inner.source else { return false };
    nest_values(o) && marks.iter().any(|m| m.rti == u32::from(scanrelid) && m.mark == RowMarkType::ROW_MARK_COPY && lock_values(gx, k, m).is_some())
}

/// D3656: op `o` of pipeline `k` is an INNER nest without parameters whose inner reads, under filters and projections, the
/// spool of nest.spool's quiet child (effects/visible/material.rs `quiet`) over the VALUES scan of a COPY mark of `marks`.
/// The runner's recheck reruns that fill first (ops/lockrows/rerun.rs RERUN_FIRST's `quiet_fill`), its Values Scan emitting
/// the mark's stored row (`admit_lazy`'s copied entry), and the nest reads the refilled spool, as EvalPlanQual's fresh
/// Material over the Values Scan hands back that row alone.
pub(crate) fn lock_values_spool(gx: &Gx<'_, '_>, k: usize, o: &Op<'_>, marks: &[RowMark]) -> bool {
    let Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, inner, params, .. }) = o else { return false };
    let Source::Breaker { slot } = inner.source else { return false };
    let Some(fill) = gx.life_of(k, slot).map(|l| l.0 as usize) else { return false };
    let copy = matches!(gx.pipelines()[fill].source, Source::Values { scanrelid, .. } if marks.iter().any(|m| m.mark == RowMarkType::ROW_MARK_COPY && m.rti == u32::from(scanrelid)));
    params.is_nil() && inner.ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) && copy && crate::effects::visible::material::quiet(gx, fill)
}

/// D3287 (b): an INNER nested loop whose inner is a constant Result (no scan, no mark: PostgreSQL drops an RTE_RESULT's row
/// mark, remove_useless_result_rtes) under filters, gates and projections: eval-plan-qual selectresultforupdate's pulled-up
/// `(select 1 as x) ss1 join (select 7 as y) ss2 on true` beside the locked relation's scan. It hands on at most its one
/// row per outer row; the recheck re-opens it over the substituted outer row, as PostgreSQL's EvalPlanQual reruns the
/// Result (ops/lockrows/lower.rs `admit`'s `join`; `nest_scan`'s ops before a marked nest).
pub fn nest_const(o: &Op<'_>) -> bool {
    matches!(o, Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, inner, .. }) if matches!(inner.source, Source::Const) && inner.ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_))))
}

/// mergecmd VALUES-INNER: an INNER nested loop whose inner is a VALUES scan under filters and projections (PostgreSQL's
/// Materialize over the Values Scan, which decompose drops), its row a COPY mark's (`nest_copy`).
pub fn nest_values(o: &Op<'_>) -> bool {
    matches!(o, Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, inner, .. }) if matches!(inner.source, Source::Values { .. }) && inner.ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))))
}

/// An INNER nested loop whose inner is a Function Scan (its FROM step over the constant source) or a CTE Scan (a breaker's
/// emit) under filters and projections: PostgreSQL 18.6 plans `UPDATE t .. FROM generate_series(..) g` / `.. FROM cte`
/// with the target's scan the outer and no Materialize. A `nest_copy` inner when its row is a write's COPY mark: the
/// recheck's re-open yields the stored row alone (rerun/inner.rs `copy_walk`), its quals checked again over it, as
/// EvalPlanQual hands the scan its test tuple (EvalPlanQualFetchRowMark's copy leg) and never runs the function or the CTE.
pub fn nest_copy_scan(o: &Op<'_>) -> bool {
    let Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, inner, .. }) = o else { return false };
    let simple = |ops: &[Op<'_>]| ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_)));
    match (&inner.source, inner.ops.split_first()) {
        (Source::Const, Some((Op::Srf(d), rest))) => matches!(d.place, SrfPlace::From { .. }) && simple(rest),
        (Source::Breaker { .. }, _) => simple(&inner.ops),
        _ => false,
    }
}

/// COPY-SCAN (`copy_pos`): whether the products in `set` that `copy_row::read_elsewhere` finds read past nest inner
/// `q`'s entry are read only by its owner `k` through `q`: `q` reads no slot but its own source's, no other pipeline
/// outside `set` reads them, and `k` reads none by its source, a join's or a set operation's slot of its own, or another
/// inner (decompose breakers.rs `embed`: an owner consumes every slot its inners read).
pub(crate) fn only_through(gx: &Gx<'_, '_>, k: usize, q: usize, set: &[usize]) -> bool {
    let ps = gx.pipelines();
    let src = |p: usize, s: u8| matches!(ps[p].source, Source::Breaker { slot } if slot == s);
    let product = |c: usize, s: u8| (0..c).rev().find(|&p| ps[p].desc.produces() == Some(s)).is_some_and(|p| set.contains(&p));
    let op = |s: u8| {
        ps[k].ops.iter().any(|o| {
            matches!(o, Op::Join(JoinDesc::Probe { slot, .. } | JoinDesc::Merge { slot, .. }) if *slot == s)
                || matches!(o, Op::SetOp(gravity_decompose::ops::shared::setop::SetDesc::Set { right, .. }) if *right == s)
        })
    };
    let inners = ps.inners();
    let sibling = |s: u8| inners.iter().enumerate().any(|(i, x)| ps.len() + i != q && x.0 .0 == k && x.1.desc.consumes.contains(&s));
    let others = (0..ps.len()).filter(|&c| c != q && c != k && !set.contains(&c)).any(|c| ps[c].desc.consumes.iter().any(|&s| product(c, s)));
    let mine = ps[q].desc.consumes.iter().all(|&s| src(q, s));
    mine && !others && ps[k].desc.consumes.iter().all(|&s| !product(k, s) || !src(k, s) && !op(s) && !sibling(s))
}

/// lockrows NEST (module doc): mark `m`'s row stored in a late RowOf site of nest inner `ik`'s scan site (`nest_scan`'s),
/// numbered by now: its nest step is made before a later step of its pipeline, a producer before its readers.
fn inner_pos(px: &mut Px<'_, '_>, node: i32, m: &RowMark, ik: usize) -> MarkPos {
    let Some(s) = px.pipeline_site(ik) else { crate::refuse!(node, Reason::Bug { payload: "a marked nest inner whose scan site is not numbered before the lock" }) };
    if px.sites.get(s).is_none_or(|d| d.relation != m.relid) {
        shape(node, SWAPPED);
    }
    let site = px.add_late_site(SiteDesc::row_of(px.arena, s as u16, m.relid));
    MarkPos { site, relid: m.relid, col: m.ctid_col, copy: None, own: false, null_empty: true, of: Some(s) }
}

/// D776: the draft `of` of a marked-row site made before its pipeline's source site was numbered.
const DRAFT_OF: u16 = u16::MAX;

/// D776: a late site `d` about to be numbered (`Px::place_late_sites`): a draft marked-row site takes its pipeline's source
/// site (numbered by now); a pipeline without one, or whose source site reads another relation (a join.buildside swap),
/// refuses (a Bug / SWAPPED).
pub fn place_rows(px: &Px<'_, '_>, d: &mut SiteDesc) {
    if !matches!(d.kind, gravity_program::SiteKind::RowOf { of: DRAFT_OF }) {
        return;
    }
    let k = d.index as usize;
    let node = px.ext::<CopySites>(Face::End).and_then(|o| o.1.iter().find(|e| e.0 == k)).map_or(-1, |e| e.1);
    let Some(s) = px.pipeline_site(k) else { crate::refuse!(node, Reason::Bug { payload: "a marked relation's own scan without its site" }) };
    if px.sites.get(s).is_none_or(|x| x.relation != d.relation) {
        shape(node, SWAPPED);
    }
    d.kind = gravity_program::SiteKind::RowOf { of: s as u16 };
    d.index = 0;
}

/// D732: the copy sites the VALUES scans made (range table index, the site numbered late: after every source site, as
/// exec's positional host registry needs); D776 / D884: in the same Face::End slot (one type per face), the pipelines
/// whose marked-row sites wait for their source site, with the node a refusal names; D3497: the pipelines whose own
/// source the recheck scans again whole (`place_rescans`).
pub struct CopySites<'g>(pub PgVec<'g, (u32, Rc<Cell<usize>>)>, PgVec<'g, (usize, i32)>, PgVec<'g, usize>, pub PgVec<'g, u32>);
crate::frame::px::ext_g!(CopySites<'g>, |m| CopySites(PgVec::new_in(m), PgVec::new_in(m), PgVec::new_in(m), PgVec::new_in(m)));

/// D3497: each pipeline whose own scan a recheck runs again whole (update_built.rs `copied_one`: the one live member of a
/// flattened UNION ALL its COPY mark names, which PostgreSQL's EvalPlanQual scans again, no row substituted), named to
/// exec epq.rs `rescans` by its source site once numbered (`Px::place_late_sites`), as a SubPlan's own scans are.
pub fn place_rescans(px: &mut Px<'_, '_>) {
    let mcx = px.mcx();
    let own: PgVec<'_, usize> = px.ext::<CopySites>(Face::End).map_or_else(|| PgVec::new_in(mcx), |c| mcx::to_vec_in(&c.2[..], mcx));
    for k in own {
        let Some(s) = px.pipeline_site(k) else { crate::refuse!(px.gx.pipelines()[k].node_id, Reason::Bug { payload: "a rescanned member's own scan without its site" }) };
        px.declare_proto(gravity_program::protos::Proto::new(gravity_program::protos::ProtoKind::Rescan, k as u16).site(s as i16));
    }
}

/// mergecmd 9 (COPYROW): whether graph pipeline `q`'s source is the VALUES scan of range table index `rti`.
fn own_values(gx: &Gx<'_, '_>, q: usize, rti: u32) -> bool {
    matches!(gx.pipelines()[q].source, Source::Values { scanrelid, .. } if u32::from(scanrelid) == rti)
}

/// mergecmd 9 (COPYROW): the op where pipeline `q` takes range table index `rti`'s COPY row: 0 for its own VALUES scan,
/// else the copy entry of a subquery or function row it runs (`copy_row::boundary`).
pub fn own_copy_at(gx: &Gx<'_, '_>, q: usize, rti: u32) -> Option<usize> {
    if own_values(gx, q, rti) {
        return Some(0);
    }
    gx.graph.record_rows.iter().find(|r| r.scanrelid == rti).and_then(|r| crate::rerun::epq::copy_row::boundary(gx, q, r))
}

fn own_copy(gx: &Gx<'_, '_>, q: usize, rti: u32) -> bool {
    own_copy_at(gx, q, rti).is_some()
}

/// mergecmd 13a: whether `m` is a REFERENCE mark (no parent's, with its junk ctid) on pipeline `k`'s own heap range or
/// index scan, and the pipeline runs a nested loop before its write: the nest's outer (a MERGE's plain source table, the
/// target the nest's inner). EvalPlanQualFetchRowMark refetches that row at its ctid under SnapshotAny, the version the
/// join read; `own_pos` stores the ctid in the source's marked-row site, so the recheck's outer yields that row alone.
/// mergecmd D1933 BITMAP-REF (held-out orm-0358): or its bitmap heap scan (`scan_of`, as producerchain G5's UPDATE outer):
/// PostgreSQL 18.6's recheck hands the refetched row through BitmapHeapRecheck (the recheck quals) and the scan's filter,
/// as the substituted bitmap source does.
pub(crate) fn nest_ref(gx: &Gx<'_, '_>, k: usize, m: &RowMark) -> bool {
    let p = &gx.pipelines()[k];
    let scan = scan_of(&p.source) == Some((m.rti, m.relid));
    let nest = p.ops.iter().take_while(|o| !matches!(o, Op::Write(_))).any(|o| matches!(o, Op::Join(JoinDesc::Nest { .. })));
    m.mark == RowMarkType::ROW_MARK_REFERENCE && !m.parent && m.ctid_col > 0 && m.lock == LockKind::None && scan && nest
}

/// D732: whether a write or lock of the program marks range table index `rti` ROW_MARK_COPY (its scan keeps a copy site).
pub fn copied(gx: &Gx<'_, '_>, rti: u32) -> bool {
    let copy = |m: &RowMark| m.rti == rti && m.mark == RowMarkType::ROW_MARK_COPY;
    gx.pipelines().iter().flat_map(|p| p.ops.iter()).any(|o| match o {
        Op::Write(w) => w.mark_descs.iter().any(copy),
        Op::Lock(d) => d.marks.iter().any(copy),
        _ => false,
    })
}

/// A heap scan source's (scanrelid, relid): range, index (not index-only), bitmap.
/// D3278 (SAMPLE): a TABLESAMPLE scan of mark `m`'s heap relation. Its host is the heap range source under a sample filter
/// (hosts heap_source.rs `SampleFilter`), whose EvalPlanQual substitution hands the version at the position alone, the
/// filter not asked (`window_substituted`): PostgreSQL's SampleScan returns the test tuple and SampleRecheck is true, so
/// the recheck reruns the scan's producer over the mark's row, as a plain scan's (`sites_in`).
fn sample_of(s: &Source<'_>, m: &RowMark) -> bool {
    matches!(s, Source::Sample { am: Am::Heap, scanrelid, relid, .. } if u32::from(*scanrelid) == m.rti && *relid == m.relid)
}

fn scan_of(s: &Source<'_>) -> Option<(u32, u32)> {
    match s {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } => Some((u32::from(*scanrelid), *relid)),
        _ => None,
    }
}

/// D1000 (main 09-27 08:42Z (ii), M7's write.*): a write's REFERENCE mark over an inheritance or partitioned parent
/// (PLANROWMARK isParent): the children carry the row, so the recheck is EvalPlanQual over the parent's arms.
/// REFPARENT: served for one parent's children (`write_families`), each child's mark positioned at the arm that
/// scans it and active only where the row's `tableoid{rowmarkId}` junk column names that child (`sites_in`): PostgreSQL's
/// EvalPlanQualFetchRowMark fetches a child's row only when the child is the row's tableoid, the other children give none.
/// D3400 REFPAR2 (lockrows_refparent: `UPDATE t .. FROM p, q WHERE ..`, p and q both parents): or several parents' children,
/// each family's marks sharing their own `tableoid{rowmarkId}` column, so each child position is gated on its family's.
/// Any other form (a locking mark, no tableoid column, two families sharing one) keeps the refusal by name.
pub fn parent_mark(node: i32, marks: &[RowMark]) {
    if write_families(marks).is_none() {
        crate::refuse!(node, crate::admit::refusal::Reason::Shape { why: crate::admit::refusal::ShapeWhy("a write's REFERENCE mark over an inheritance or partitioned parent (the children carry the row: EPQ over the parent's arms)") })
    }
}

/// REFPARENT: a write's marks' parent families, each `(top parent's rti, its tableoid junk column)`: every parent's
/// or child's mark REFERENCE (none locks), each family's marks sharing one `tableoid{rowmarkId}` column and no two families
/// one (D3400 REFPAR2: several parents). `None` without a parent, or when the form is another (a locking mark, no tableoid
/// column, two families' marks on one column).
fn write_families(marks: &[RowMark]) -> Option<Vec<(u32, i16)>> {
    let mut fams: Vec<(u32, i16)> = Vec::new(); // alloc: Program
    for m in marks.iter().filter(|m| m.parent || m.prti != m.rti) {
        let top = if m.parent && m.prti == m.rti { m.rti } else { m.prti };
        if m.mark != RowMarkType::ROW_MARK_REFERENCE || m.lock != LockKind::None || m.toid_col <= 0 {
            return None;
        }
        match fams.iter().find(|f| f.0 == top) {
            Some(f) if f.1 != m.toid_col => return None,
            Some(_) => {}
            None if fams.iter().any(|f| f.1 == m.toid_col) => return None,
            None => fams.push((top, m.toid_col)), // alloc: Program
        }
    }
    (!fams.is_empty()).then_some(fams)
}

/// Whether op `o` may sit before a write or lock whose marks recheck: a hash or nested-loop join's inner probe.
pub fn joins(o: &Op<'_>) -> bool {
    matches!(o, Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_INNER, .. }))
}

/// lockrows NEST (module doc): an INNER nested loop whose inner is one heap scan (range, index, bitmap) under filters and
/// projections, which a recheck opens once over its substituted source.
pub fn nest_joins(o: &Op<'_>) -> bool {
    matches!(o, Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, inner, .. }) if scan_of(&inner.source).is_some() && inner.ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))))
}

/// D3291 (lockrows MEMO-NEST): a `nest_joins` nest whose inner holds one Memoize over its scan. A replayed row carries
/// its junk ctid (`store`); the recheck's memo is a fresh, empty one (exec rerun/epq.rs `recheck_on`, as EvalPlanQualStart's
/// fresh Memoize), so its lookup misses and the substituted scan runs once, as nest_write.rs's tw-core-197 form.
pub fn nest_memo_joins(o: &Op<'_>) -> bool {
    let memo = |o: &Op<'_>| matches!(o, Op::Join(JoinDesc::Memo { .. }));
    matches!(o, Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, inner, .. }) if scan_of(&inner.source).is_some() && inner.ops.iter().filter(|o| memo(o)).count() == 1 && inner.ops.iter().all(|o| memo(o) || matches!(o, Op::Filter(_) | Op::Project(_))))
}

/// producerchain G2: a LEFT nested loop whose inner is `nest_joins`' one heap scan under filters and projections, before a
/// write's target nest (nest_write.rs `dml_form`): the recheck opens its inner once over the substituted row, or none.
pub fn nest_left_joins(o: &Op<'_>) -> bool {
    matches!(o, Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_LEFT, inner, .. }) if scan_of(&inner.source).is_some() && inner.ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))))
}

/// modifytable ANTINEST: an ANTI nested loop whose inner is `nest_joins`' one heap scan under filters and projections
/// (`DELETE FROM c WHERE NOT EXISTS (SELECT 1 FROM o WHERE o.cid = c.id) ..`: Nested Loop Anti Join, the inner an index scan
/// parameterized by the target's row): a write's recheck opens its inner over an empty position, so no row (nest_ref.rs).
pub fn nest_anti_joins(o: &Op<'_>) -> bool {
    matches!(o, Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_ANTI, inner, .. }) if scan_of(&inner.source).is_some() && inner.ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))))
}

/// modifytable SEMINEST: a SEMI nested loop whose inner is `nest_joins`' one heap scan under filters and projections
/// (`DELETE FROM mv WHERE ctid = ANY (SELECT diff.tid FROM diff ..)`, REFRESH MATERIALIZED VIEW CONCURRENTLY's, planned as
/// Nested Loop Semi Join of the target's scan and diff's): the row carries the first matching inner row's junk ctid, so a
/// write's recheck opens its inner over that stored row alone (nest_ref.rs), as an INNER nest's.
pub fn nest_semi_joins(o: &Op<'_>) -> bool {
    matches!(o, Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_SEMI, inner, .. }) if scan_of(&inner.source).is_some() && inner.ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))))
}

/// D3739 (Django's delete() across a relation with a pk filter: `DELETE FROM login WHERE id IN (SELECT U0.id FROM login
/// U0 JOIN orgunit U1 .. WHERE U0.id = 1 ..)`, PostgreSQL 18.6's Nested Loop Semi Join (the target's index scan, Nested
/// Loop (U0's index scan, U1's))): a SEMI nested loop whose inner is one heap scan under filters, projections and
/// `nest_joins` nests. The row carries the first match's junk ctids, so a write's recheck re-opens the inner over those
/// stored rows alone (`nest_scan`: each nested inner's RowOf site too), as EvalPlanQual hands each scan its refetched
/// row.
pub fn nest_semi_deep(o: &Op<'_>) -> bool {
    matches!(o, Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_SEMI, inner, .. }) if scan_of(&inner.source).is_some() && inner.ops.iter().any(nest_joins) && inner.ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_)) || nest_joins(o)))
}

/// D3287 (b): a row lock's LEFT join in its rechecking pipeline: a LEFT hash probe, or a `nest_left_joins` nest (its inner
/// one heap scan under filters and projections). FOR UPDATE never applies to the nullable side, so the inner relation's
/// mark is a REFERENCE one, positioned `null_empty` (a null-extended row's NULL ctid gives the recheck no inner row).
pub fn left_joins(o: &Op<'_>) -> bool {
    matches!(o, Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_LEFT, .. })) || nest_left_joins(o)
}

/// lockrows NEST: an op that hands on at most one row per row it reads (a recheck's substituted rows stay one row): a
/// filter, gate, projection or SubPlan step, an INNER hash probe (`joins`: its build reruns over substituted rows) or
/// a `nest_joins` nest.
pub fn one_row(o: &Op<'_>) -> bool {
    matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::SubPlan(_)) || joins(o) || nest_joins(o)
}

/// D1249 (M8-WRITE W4): a write's chain joins, `joins` or an INNER merge join (its sorted inputs are producers the recheck
/// reruns, one row each); a lock keeps `joins` (FOR UPDATE over a merge join stays refused by name). D1933B: or an ANTI
/// probe, whose rows carry the inner relation's mark ctid NULL (PG's null inner slot), so `sites` marks it `null_empty`
/// and the recheck reruns its build empty: the target's newest version passes the anti join, as EvalPlanQualFetchRowMark's
/// no-row makes PostgreSQL's recheck do.
pub fn write_joins(o: &Op<'_>) -> bool {
    joins(o) || matches!(o, Op::Join(JoinDesc::Merge { jointype: JoinType::JOIN_INNER, .. }) | Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_SEMI | JoinType::JOIN_ANTI, .. }))
}

/// merge#340 (writesubplan D1112): op `o` is a SubPlan step its join's qual reads (V4R-499's join-qual site: lent to the
/// join's reader, evaluated over each pair where PostgreSQL's join filter reaches it). merge/admit.rs's chain takes it; a
/// recheck reruns the chain's probe, which reads it again over the new version (`sites_in`'s `acts`).
pub fn join_qual_sub(o: &Op<'_>) -> bool {
    matches!(o, Op::SubPlan(d) if d.site == gravity_decompose::ops::shared::subplan::SubSite::JoinQual)
}

fn shape(node: i32, why: ShapeWhy) -> ! {
    crate::refuse!(node, Reason::Shape { why })
}

/// Each mark's junk ctid (an ItemPointerData image: bi_hi, bi_lo, ip_posid) stored as its site's position, before the
/// host call. V4R-495, D2048: a `null_empty` mark's NULL ctid (an outer join's null-extended row in the stopped pipeline or
/// in a producer it reruns, MERGE's NOT MATCHED BY SOURCE) stores the empty row position (offset 0: a substituted heap or
/// index source's recheck finds no row), so the recheck null-extends again as EvalPlanQualFetchRowMark's no-row does.
/// D1685: a COPY mark's NULL wholerow (its relation null-extended: MERGE's NOT MATCHED BY SOURCE) stores the empty copy
/// (datum 0, `copy_row::none`): the recheck's copy leg gives no row, as EvalPlanQualFetchRowMark's does for a NULL one.
pub fn store(b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, node: i32, marks: &Marks) {
    let Some(tid) = marks.tid.as_ref() else { return };
    for (i, m) in marks.pos.iter().enumerate() {
        let Some(cr) = row.col(m.col) else { crate::refuse!(node, Reason::Bug { payload: "a mark's ctid column past the child's projection" }) };
        let gate = marks.gate.as_ref().and_then(|(of, cols)| cols.get(i).copied().filter(|&c| c > 0).map(|c| (c, of)));
        let mut cx = env.lx(b, node);
        let (d, null) = crate::rep::datum(&mut cx, cr, m.copy.as_ref().unwrap_or(tid));
        env.memo = cx.memo;
        // lockrows D2604: another leaf's row leaves this child's mark inactive (its position empty)
        let null = match gate {
            Some((col, of)) => {
                let Some(oc) = row.col(col) else { crate::refuse!(node, Reason::Bug { payload: "a family's tableoid column past the child's projection" }) };
                let mut cx = env.lx(b, node);
                let (o, onull) = crate::rep::datum(&mut cx, oc, of);
                env.memo = cx.memo;
                let rel = b.iconst(Type::I64, i64::from(m.relid));
                let other = b.icmp(gravity_ir::IntCond::Ne, o, rel);
                let off = b.int_bin(IntBinOp::Or, other, onull);
                b.int_bin(IntBinOp::Or, null, off)
            }
            None => null,
        };
        let (l_on, l_join) = (b.create_block(&[]), b.create_block(&[]));
        let s = site_off(m.site.get()) as i32;
        if !m.null_empty && m.copy.is_none() && gate.is_none() {
            b.cond_br(null, (l_join, &[]), (l_on, &[]));
        } else {
            let l_none = b.create_block(&[]);
            b.cond_br(null, (l_none, &[]), (l_on, &[]));
            b.switch_to(l_none);
            if m.copy.is_some() {
                // D1685: a recheck node's held row (`POS_COPY`) is kept, as below
                let (l_empty, held) = (b.create_block(&[]), crate::rerun::epq::copy_row::copying(b, env, m.site.get()));
                b.cond_br(held, (l_join, &[]), (l_empty, &[]));
                b.switch_to(l_empty);
            }
            let (flags, rel, zero) = (b.iconst(Type::I32, (POS_VALID | POS_ROW) as i64), b.iconst(Type::I32, m.relid as i32 as i64), b.iconst(Type::I32, 0));
            b.store(env.ctx, s + site::POS_FLAGS as i32, flags);
            b.store(env.ctx, s + site::POS_REL as i32, rel);
            b.store(env.ctx, s + site::POS_BLOCK as i32, zero);
            b.br(l_join, &[]);
        }
        b.switch_to(l_on);
        if m.copy.is_some() {
            // mergecmd 9 (COPYROW): on a recheck node the site already holds the stopped row (`POS_COPY`, driver
            // `set_copies`): kept, as a store would clear the flag and the rechecking pipeline's own VALUES scan would go
            // on past that one row
            let (l_store, held) = (b.create_block(&[]), crate::rerun::epq::copy_row::copying(b, env, m.site.get()));
            b.cond_br(held, (l_join, &[]), (l_store, &[]));
            b.switch_to(l_store);
            // D732: the row's datum (its record's image lives in the build the stopped row was probed against, which the
            // stopped node keeps through the host call and so through the recheck)
            let flags = b.iconst(Type::I32, (POS_VALID | POS_ROW) as i64);
            b.store(env.ctx, s + site::POS_FLAGS as i32, flags);
            b.store_unaligned(env.ctx, s + site::POS_REL as i32, d);
            b.br(l_join, &[]);
            b.switch_to(l_join);
            continue;
        }
        let p = b.conv(ConvOp::IntToPtr, Type::Ptr, d);
        let (hi, lo, off) = (b.load_zext(Type::I16, p, 0), b.load_zext(Type::I16, p, 2), b.load_zext(Type::I16, p, 4));
        let sixteen = b.iconst(Type::I64, 16);
        let hi = b.int_bin(IntBinOp::Shl, hi, sixteen);
        let block = b.int_bin(IntBinOp::Or, hi, lo);
        let off = b.conv(ConvOp::Trunc, Type::I32, off);
        let sixteen = b.iconst(Type::I32, 16);
        let off = b.int_bin(IntBinOp::Shl, off, sixteen);
        let flags = b.iconst(Type::I32, (POS_VALID | POS_ROW) as i64);
        let w0 = b.int_bin(IntBinOp::Or, off, flags);
        b.store(env.ctx, s + site::POS_FLAGS as i32, w0);
        let rel = b.iconst(Type::I32, m.relid as i32 as i64);
        b.store(env.ctx, s + site::POS_REL as i32, rel);
        let blk = b.conv(ConvOp::Trunc, Type::I32, block);
        b.store(env.ctx, s + site::POS_BLOCK as i32, blk);
        b.br(l_join, &[]);
        b.switch_to(l_join);
    }
    // producerchain UNION-COPY: no position (the flags word with the offset beside it, one 32-bit store): the recheck's
    // rerun scans the member whole. Its scan ended before the write's pipeline ran (a breaker's producer)
    for s in marks.rescan.iter() {
        let zero = b.iconst(Type::I32, 0);
        b.store(env.ctx, site_off(s.get()) as i32 + site::POS_FLAGS as i32, zero);
    }
}

/// subplan (W6-D1112 row B, a check option's hashed SubPlan, host-fed): the pipelines that build the sets only such
/// probes of pipeline `k`'s write read: each set's producer and, to a fixpoint, a product only those read. PostgreSQL
/// evaluates a check option after EvalPlanQual, in the main executor state, with the hash table it built at the option's
/// first probe (ExecWithCheckOptions; the recheck never rebuilds it), and so does the program: the recheck steps past
/// these builds, and the host's check read probes the set as built (lazily, at its first probe, as before a recheck).
/// A set another op of the pipeline reads (a qual, the projection, a join) is not one of these.
fn wco_sets<'g>(gx: &Gx<'g, '_>, k: usize) -> PgVec<'g, usize> {
    use gravity_decompose::ops::shared::subplan::{refers, refers_list};
    use gravity_decompose::ops::shared::subplan::SubForm;
    let ps = gx.pipelines();
    let pk = &ps[k];
    let Some(w) = pk.ops.iter().find_map(|o| if let Op::Write(w) = o { Some(w) } else { None }) else { return PgVec::new_in(gx.mcx) };
    let other = |id: i32| pk.ops.iter().any(|o| match o {
        Op::Filter(n) | Op::Gate(n) => refers(*n, id),
        Op::Project(l) => refers_list(l, id),
        Op::Join(j) => j.qual().is_some_and(|l| refers_list(l, id)) || j.keys().is_some_and(|l| refers_list(l, id)),
        Op::Window(wd) => wd.functions.iter().any(|f| refers_list(&f.func.args, id)),
        Op::Srf(s) => refers_list(s.tlist, id) || s.functions.iter().any(|f| refers(f.node, id)),
        // an upsert's or MERGE's own lists: not a check option's alone
        Op::Write(d) => d.on_conflict.is_some() || d.merge.is_some(),
        _ => false,
    });
    let slots: PgVec<'_, u8> = pk.ops.iter().filter_map(|o| match o {
        Op::SubPlan(d) if w.checks.iter().any(|c| refers(c.qual, d.plan_id)) && !other(d.plan_id) => match d.form {
            SubForm::Hashed { slot } => Some(slot),
            _ => None,
        },
        _ => None,
    }).collect_in(gx.mcx);
    if slots.is_empty() {
        return PgVec::new_in(gx.mcx);
    }
    let mut out: PgVec<'_, usize> = (0..ps.len()).filter(|&q| q != k && ps[q].desc.produces().is_some_and(|s| slots.contains(&s))).collect_in(gx.mcx);
    // a product only those builds read (a join inside the set's subquery)
    let only = |out: &[usize], q: usize| ps[q].desc.produces().is_some_and(|s| {
        let mut readers = (0..ps.len()).filter(|&r| ps[r].desc.consumes.iter().any(|&c| c == s)).peekable();
        readers.peek().is_some() && readers.all(|r| out.contains(&r))
    });
    while let Some(q) = (0..ps.len()).find(|&q| q != k && !out.contains(&q) && only(&out[..], q)) {
        out.push_in(q);
    }
    out
}

/// subplan (W6-D1112 row B): whether graph pipeline `q` is one of a write's `wco_sets`: a recheck never reruns it
/// (`Pipeline::recheck_skip`; the driver opens its slot empty). The set is the statement's, as PostgreSQL's check option
/// hash table is the main executor state's: EvalPlanQual's fresh plan tree never holds it, so the recheck neither rebuilds
/// it nor resets it, and the write's check after the recheck probes it as built (or builds it, LAZY, at its first probe).
pub fn wco_skipped(gx: &Gx<'_, '_>, q: usize) -> bool {
    // D3585: in the graph's lowering every pipeline's sets are found once (memo.rs), not again for each `q`
    let key = crate::memo::key_of(&gx.graph.pipelines);
    let sets = crate::memo::read(key, |m| m.wco.clone()).map(|x| {
        x.unwrap_or_else(|| {
            let all: std::rc::Rc<[(u32, Box<[u32]>)]> = (0..gx.pipelines().len()) // alloc: Program
                .filter_map(|k| {
                    let s = wco_sets(gx, k);
                    (!s.is_empty()).then(|| (k as u32, s.into_iter().map(|q| q as u32).collect())) // alloc: Program
                })
                .collect();
            crate::memo::write(key, |m| m.wco = Some(all.clone()));
            all
        })
    });
    match sets {
        Some(sets) => sets.iter().any(|(k, s)| *k as usize != q && s.contains(&(q as u32))),
        None => (0..gx.pipelines().len()).any(|k| k != q && wco_sets(gx, k).contains(&q)),
    }
}

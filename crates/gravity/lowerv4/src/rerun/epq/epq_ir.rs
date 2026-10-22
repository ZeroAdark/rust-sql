//! EvalPlanQual in the IR over a site's cells (V4R-316, L0-h; the V4R-224 design, main 09-26), shared by the steps that
//! recheck a row where it stands instead of in the runner (exec epq.rs): write.lock's (write/lock_epq.rs) and the write
//! row-op's. The runner's recheck sends the rechecked row to the client itself, past any step above; here the newer
//! version goes on through the pipeline's own steps above the step. Over a TID source (D1672) PG drops its TID quals from
//! the scan's qual and rechecks the newer version's ctid against the TID list or range (TidRecheck / TidRangeRecheck):
//! here each TID clause is tested over the newest version's ctid against its value, the scan's own key word (a by-reference
//! literal is one word, the key's: compiling the clause again would file it twice), any one of a list's (its list is their
//! union), every one of a range's (its bounds are their intersection), none for `WHERE CURRENT OF` (it always takes the
//! newest version); an `= ANY (array)` clause compiles (its elements are words of their own). A value computed from
//! executor parameters keeps the refusal by name (D1672).
//! Owner M8-MEMO-CTE (moved from write/lock_epq.rs, V4R-512, multilock plan 4.1).
use std::rc::Rc;

use gravity_abi::ctx::{site_off, POS_ROW, POS_VALID};
use gravity_abi::site::site;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::scan::TidForm;
use gravity_decompose::catalog::typefacts::{self, TypeFacts};
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_program::program::LockSite;
use gravity_program::{ExprLife, ExprOut, OutCol, Rep, SiteDesc};
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};
use types_nodes::jointype::JoinType;
use types_nodes::Node;

use crate::cell::{At, Cell, Nullable, Row, TIDOID};
use crate::face::step::ExprFace;
use crate::frame::env::{Env, Filled};
use crate::frame::frame::Next;
use crate::frame::iface;
use crate::frame::px::{ExId, Px};
use crate::frame::row::{Carry, RowVals};
use crate::frame::site::CTID_AT;
use crate::admit::refusal::{Reason, ShapeWhy};

/// D1672: a TID clause's value the in-IR recheck cannot test (computed from executor parameters, or no comparison).
const TID: ShapeWhy = ShapeWhy("a recheck in the IR over a TID scan (PG's TidRecheck / TidRangeRecheck, D1672)");
/// D655 Row C (V4R-534): `epq_of`'s `sys` for the write row-op's recheck in a data-modifying CTE (write/rowop_epq.rs).
pub(crate) const ROWC_SYS: ShapeWhy = ShapeWhy("a rechecking UPDATE / DELETE in a data-modifying CTE over a system column or an unrecorded projection (D655 Row C)");

/// EvalPlanQual in the IR for a lock with a LIMIT / OFFSET above (ExecLockRows' EvalPlanQualNext). The newer version
/// RL1b's host put in the lock site's cells (no second lock, D604) goes through the source's recheck quals
/// (indexqualorig, bitmapqualorig: IndexRecheck, BitmapHeapRecheck; a range scan has none; a tid scan's is not built) and
/// the ops below the lock again (filters, projections), compiled for any row: no window road (a text.frame verdict is
/// the old version's). A row that fails is skipped, its version's lock kept (PG's); one that passes goes on through
/// `next`, so the steps above count it, and a LIMIT reached ends the window before another row is locked.
pub(crate) struct Epq<'g> {
    /// The relation's attributes by attno (a dropped one `None`): the site's cells.
    atts: PgVec<'g, Option<ExprOut>>,
    redo: PgVec<'g, Redo<'g>>,
    ex: &'static ExprFace,
    /// modifytable ROWC-UNION (`epq_of_union`): under a write of several result relations fed by one settled union, the
    /// row's relation (its junk tableoid column and the oid type's facts) and each relation's own recheck by its relid;
    /// `atts` / `redo` then unused.
    union: Option<((i16, TypeFacts), PgVec<'g, (u32, Epq<'g>)>)>,
    /// D3271: a lock's pending InitPlans PostgreSQL's EvalPlanQualStart forces before the recheck (ops/lockrows/lock_init.rs
    /// `forced`: each one's first parameter and handle word), read at the recheck's start, a pending one run there.
    force: PgVec<'g, (i32, u16)>,
    /// D3272: the InitPlans inside a SubPlan step's plan PostgreSQL's recheck estate re-arms (ops/lockrows/lock_init.rs
    /// `armed`: each one's handle word, first parameter and parameter count), armed for the redo and put back after it.
    fresh: PgVec<'g, (u16, i32, u32)>,
    /// D3274: the EvalPlanQual state `fresh` is armed in (`m8_init_epq`'s key: the lock's or write's plan node + 1), so each
    /// state runs an InitPlan again once until it ends (`epq_end`); 0: D3272's one state per execution.
    key: u32,
    /// D3506 SETLOCK (rerun/epq/set_lock.rs): the redo's hashed probes read the sets of its own (`Env::epq_twin`).
    twin: bool,
    /// D3506 SETLOCK: the hashed probes of a claim's Sort producer made again for the redo (`set_lock::redo_probes`),
    /// by SubPlan id: the redo's `Redo::Sub` lends each over the row so far to the readers after it.
    own: PgVec<'g, (i32, PgBox<'g, dyn crate::face::step::StepMech + 'g>)>,
}

enum Redo<'g> {
    Qual(ExId),
    /// D1672: TidRecheck / TidRangeRecheck over the row's ctid: any clause of a list (`false`: TidListEval's union, each
    /// evaluated), every clause of a range (`true`: TidRangeEval's bounds).
    Tid(PgVec<'g, TidCheck>, bool),
    /// Each target entry's column (resno - 1), expression and form.
    Project(PgVec<'g, (usize, ExId, ExprOut)>),
    /// D652-S1 (modifytable): an eager per-row SubPlan step, evaluated again over the row so far (subplan site.rs `redo`),
    /// as EvalPlanQual re-runs the SubPlan with the newest version's arguments. D3451 LAZYSUB: a lazy one (`Lend::Row`: a
    /// conditional SubPlan, a hashed probe under OR / CASE) is lent again over the row so far to the redo's readers after it
    /// (subplan site.rs `relend`), each evaluating it where it reaches it, as EvalPlanQual's ExecQual over the newest
    /// version does; `true` when no op at or past the write / lock reads it (its readers all in the redo), else LAZY_SUB.
    Sub(i32, bool),
    /// D1933 (c) (admit): an INNER nest's join quals over the row so far joined to the inner row the nest handed on
    /// (its node; nest.loop's `Env::redo_nests`), as EvalPlanQual joins the newest version to the other relation's row
    /// refetched at its junk ctid: the version the join read, which that inner row is. producerchain C: a LEFT nest's
    /// (`left`) too: the NULL-extended row it handed on stays NULL-extended (no inner row matched: the relation's junk
    /// ctid is NULL, so EvalPlanQualFetchRowMark gives no test tuple), and a matched inner row the join quals fail over
    /// the newest version is NULL-extended (ExecNestLoop's unmatched outer row), never skipped.
    Nest { node: i32, quals: PgVec<'g, ExId>, left: bool },
    /// producerchain D (nest_outer.rs): an INNER hash probe's match over the build record it handed on (its node; the
    /// record's image and payload row, hashjoin walk.rs / probe.rs), as EvalPlanQual joins the newest version to the build
    /// relation's row refetched at its junk ctid: the version the build read, whose key the record holds (`keys`: the
    /// probe's outer keys, their image over the newest version against the record's), then the join qual (`quals`).
    /// modifytable rowc-copy: a SEMI probe's too (its first match's record, PG's single_match row), and a build of a
    /// COPY-marked relation's rows (a CTE's): EvalPlanQual's test tuple is the row the join read, which the record holds.
    /// D3747: a LEFT probe's too (`left`): its NULL-extended record stays NULL-extended (no build row matched, so no test
    /// tuple), and a matched record whose key or join qual fails over the newest version is NULL-extended (`run_on`), as
    /// ExecHashJoin's unmatched outer row over EvalPlanQual's test tuples, never skipped.
    Probe { node: i32, keys: PgVec<'g, crate::ops::hashjoin::keys::KeySrc>, quals: PgVec<'g, ExId>, left: bool },
    /// lockrows REFFETCH: an INNER hash probe whose build is a REFERENCE-marked relation's own scan (`Fetch`).
    Ref(PgBox<'g, Fetch<'g>>),
    /// lockrows REFBUILD: the same with the sides swapped: the row so far is the build side's (the locked relation's), the
    /// fetched REFERENCE row the probe side's, its pipeline's ops before the probe redone over it (`Fetch::build`).
    Outer(PgBox<'g, Fetch<'g>>),
    /// modifytable D2739 NEST-INNER (subq-0323): the row so far is an INNER nest's inner row formed over the target's newest
    /// version (its pipeline's recheck quals, filters and projection), joined to the outer row the nest held (its node;
    /// nest.loop's `Env::redo_outers`), then the join quals: EvalPlanQual refetches the outer relation's row at its junk
    /// ctid (the version the join read, which that outer row is) and reruns the inner scan over the newest version.
    InnerOf { node: i32, quals: PgVec<'g, ExId> },
    /// D3561 / D3562 REFANTI (rerun/epq/ref_anti.rs): an ANTI join whose inner reads a REFERENCE-marked relation's own scan.
    /// That relation's junk ctid is NULL above the join (the null inner slot), so EvalPlanQualFetchRowMark gives it no test
    /// tuple, its scan is empty in the recheck and the newest version passes the join: the row so far joined to the inner's
    /// columns NULL (`cols`: each one's column and form, ref_anti.rs `cols`), as the join hands an unmatched row on.
    Anti(PgVec<'g, (usize, ExprOut)>),
    /// D3565 REFDRAIN (`epq_of_drain`): a Hash Right Anti Join's drain over the row so far, the build's (the locked
    /// relation's): its probe side's junk ctid is NULL in a drained row, so EvalPlanQualFetchRowMark gives that relation no
    /// test tuple, the outer scan is empty and the join drains the newest version: the row so far as the join's inner, the
    /// outer columns NULL (`cols`: each one's column and form, the OUTER_VAR columns the ops after the Drain read).
    Drain(PgVec<'g, (usize, ExprOut)>),
    /// D3802 SORTCHAIN (rerun/epq/sort_chain.rs, `epq_of_sort_chain`): the redo's first step, its row the producer's source
    /// relation's, a REFERENCE row refetched at its junk ctid (the version the join read), not the lock site's: the locked
    /// relation is a nest's inner further on (`HELD`).
    Source(PgBox<'g, Fetch<'g>>),
}

/// D3802 SORTCHAIN: a fetch entry's column for the locked relation's own inner (`redo_of`'s nest arm): no refetch, its row
/// the lock site's newest version (`Fetch::held`, the site `hold` gives it).
pub(crate) const HELD: i16 = -1;

/// lockrows REFFETCH (main 10-04, option (a)): an INNER hash probe whose build side is a REFERENCE-marked relation's own
/// heap scan. PostgreSQL's EvalPlanQual refetches that relation's row at the junk ctid of the row LockRows locked
/// (EvalPlanQualFetchRowMark under SnapshotAny: the version the join read), runs the build side's scan over it alone (its
/// recheck quals, filters and projection: the Hash's row) and joins the newest version to it (ExecScanHashBucket's hash
/// clauses, then the join qual); no hash table is kept for it. Here the lock host fetches that version into `site`
/// (`LockSite::FETCH`, hosts lock.rs `fetch`), the build side's redo runs over its cells, and the row so far, joined to it
/// (`Env::inner` maps INNER_VAR to its columns), goes through the probe's clauses and qual at the probe's node.
pub(crate) struct Fetch<'g> {
    /// A write site as wide as the relation (its cells and position, which the host fills), numbered late.
    site: Rc<core::cell::Cell<usize>>,
    relid: u32,
    /// The REFERENCE mark's junk ctid column in the step's row, and the tid type's facts.
    col: i16,
    tid: TypeFacts,
    atts: PgVec<'g, Option<ExprOut>>,
    build: PgVec<'g, Redo<'g>>,
    node: i32,
    quals: PgVec<'g, ExId>,
    /// lockrows REFNEST (rerun/epq/ref_nest.rs): a nest's parameters over the row so far, set while `build` runs.
    params: PgVec<'g, (i32, ExId)>,
    /// D3379 LOCKPAIR (`epq_of_pair`): `site` is the row's own lock site, whose cells hold its test tuple already (the
    /// lock's newest version, or the version it locked, fetched): no fetch at `col`.
    held: bool,
    /// D3379 INITSINK (D3436, `fetching_by`): the `Program.locks` entry whose lock host fetches the row (`LockSite::arg`'s
    /// mark: the M4 pair's B, 2), else 0 (the program's one lock).
    mark: usize,
    /// D3560 REFNEST-LEFT: a LEFT nest's: no fetched row (a NULL junk ctid), a failing inner redo or join qual null-extends
    /// the row (`run_on` branches), as ExecNestLoop's unmatched outer row over EvalPlanQual's test tuples.
    left: bool,
    /// D3569 REFCOPY (rerun/epq/ref_copy.rs): a COPY mark's stored row in place of a refetched one (`site`, `relid`,
    /// `tid` and `atts` unused): the record at `col`, the mark's junk whole-row column, a NULL one no row.
    copy: Option<PgBox<'g, CopyRow<'g>>>,
}

/// D3569 REFCOPY: a COPY mark's stored row as a nest inner's (`Fetch::copy`): EvalPlanQualFetchRowMark's copy leg hands
/// the Subquery Scan the record the row carries, its fields the scan's input columns, and its quals and projection run
/// again over it (`Fetch::build`, from the scan's entry), the subquery below never.
pub(crate) struct CopyRow<'g> {
    /// The record type's facts (the junk column's datum).
    rec: TypeFacts,
    /// The record's fields as the Subquery Scan's input columns (copy_row.rs `cols_of`).
    fields: PgVec<'g, (i16, ExprOut)>,
}

/// D3569 REFCOPY: the `record` pseudo-type (a COPY mark's whole row, `CopyRow::rec`).
const RECORDOID: u32 = 2249;

/// lockrows REFFETCH: a fetched REFERENCE row's tid column past the step's row (the form's check says it is there).
const REF_COL: &str = "a REFERENCE mark's junk ctid column past the rechecked row";
/// lockrows REFBUILD: an op of the union's emit or above the probe that the form does not take (its check says none is).
const BUILD_OP: &str = "a REFBUILD redo over an op other than a filter, gate, projection or the union's marker";

/// D1933 (c): a write's redo past a nest whose inner row is out of its reach (never: nest.loop pushes it for the steps
/// after it, the write's among them).
const NEST_ROW: ShapeWhy = ShapeWhy("a rechecking write past a nest whose inner row its EvalPlanQual redo cannot reach (D1933 (c))");

/// producerchain D: a write's redo past a hash probe whose matched record is out of its reach (never: the probe pushes its
/// image and payload row for the steps after it, the write's among them; join.mult's weighted row pushes none).
const PROBE_ROW: ShapeWhy = ShapeWhy("a rechecking write past a hash probe whose matched record its EvalPlanQual redo cannot reach (producerchain D)");
/// producerchain D: a probe whose hash clauses run again on each candidate (hashjoin keys.rs `confirms`, D1899): the redo
/// matches by the key images alone.
const CONFIRMS: ShapeWhy = ShapeWhy("a rechecking write past a hash probe whose clauses run on each candidate (D1899; producerchain D)");

/// D652-S1: a write's redo reaching a SubPlan its reader evaluates where it reaches it (a lazy step: its lent row is the
/// version the recheck replaced), when an op at or past the write reads it too (D3451 LAZYSUB serves the rest).
const LAZY_SUB: ShapeWhy = ShapeWhy("a rechecking write whose EvalPlanQual redo reads a SubPlan where its reader reaches it (D652-S1: a lazy step)");

/// The plan node of pipeline `p`'s scan: its first node starting at op 0 that is not a node of a bitmap source's tree. A
/// Bitmap Heap Scan's tree nodes (BitmapAnd / BitmapOr / Bitmap Index Scan) run inside its source and are listed before
/// it (decompose closure.rs `bitmap_nodes`), but the scan's lanes (`scan_lanes`: tableoid, the system columns) and its
/// target list are the heap scan's own, so expressions over its row compile at the heap scan's node (step/expr.rs reads
/// the tableoid lane by that node: a write's tableoid junk column over a bitmap leaf, main 21:03Z).
pub(crate) fn scan_node(p: &gravity_decompose::PipelineNode<'_>) -> i32 {
    fn in_tree(t: &gravity_decompose::ops::shared::scan::BitmapPlan<'_>, id: i32) -> bool {
        crate::stack_check();
        use gravity_decompose::ops::shared::scan::BitmapPlan as B;
        match t {
            B::Index { node_id, .. } => *node_id == id,
            B::And { node_id, plans } | B::Or { node_id, plans } => *node_id == id || plans.iter().any(|x| in_tree(x, id)),
        }
    }
    let tree = |id: i32| matches!(&p.source, Source::Bitmap { plan, .. } if in_tree(plan, id));
    p.node_ops.iter().find(|n| n.first_op == 0 && !tree(n.node_id)).map_or(p.node_id, |n| n.node_id)
}

/// `sys`: the caller's refusal by name for a scan reading a system column past ctid and tableoid (the site's cells hold
/// the attributes and the position only; tableoid is the scanned leaf's constant), or a projection below the step whose
/// form decompose did not record.
pub(crate) fn epq_of<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, j: usize, node: i32, relid: u32, sys: ShapeWhy) -> Epq<'g> {
    epq_with(px, k, j, node, relid, sys, false, &[])
}

/// D3563 (ops/lockrows/lower.rs `sort_tail`): `epq_of` over the Sort's producer `q` up to its sort build `jq`, then the
/// lock pipeline `k`'s ops before the lock `j` (`ops_at`: the Sort emit's projection, the Result's SubPlan steps, each
/// evaluated again over the row so far, and its projections), as EvalPlanQual reruns Result -> Sort -> scan. D3777
/// MERGETAIL (ops/lockrows/lower.rs `merge_tail`): or `k` a merge over the Sort's emit whose inner is a REFERENCE
/// relation's scan (`fetch`): `k`'s ops before the lock through `redo_of`, the merge's REFMERGE arm refetching its row.
#[allow(clippy::too_many_arguments)]
pub(crate) fn epq_of_tail<'g, 'p>(px: &mut Px<'g, 'p>, (q, jq): (usize, usize), (k, j): (usize, usize), node: i32, relid: u32, sys: ShapeWhy, fetch: &[(usize, i16, u32)]) -> Epq<'g> {
    let mut e = epq_with(px, q, jq, node, relid, sys, false, &[]);
    let tail = match fetch.is_empty() {
        true => ops_at(px, k, 0..j, node, sys, e.ex),
        false => redo_of(px, k, j, node, sys, (false, fetch), e.ex),
    };
    e.redo.extend_in(tail);
    e
}

/// D652-S1 (modifytable, write/rowop_epq.rs): `epq_of` for a write whose ops below it run per-row SubPlan steps, each
/// evaluated again in the redo, in its place among the filters and projections.
pub(crate) fn epq_of_subs<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, j: usize, node: i32, relid: u32, sys: ShapeWhy) -> Epq<'g> {
    // D3678: a write's redo probes the twins of the sets whose plans act (ops/modifytable/rowop_epq.rs `write_twins`)
    let twin = crate::ops::modifytable::rowop_epq::write_twins(px.gx, k);
    let e = epq_with(px, k, j, node, relid, sys, true, &[]).twinning(twin);
    // D3274: a write over a SubPlan step whose inner locks forces and arms what the ModifyTable's EvalPlanQualStart does
    // D3489 CTECLAIM-INIT: a data-modifying CTE's write arms the InitPlans at its ModifyTable its first recheck runs again
    let inits = match crate::rerun::epq::sub_lock::write_recheck(px, k) {
        Some(w) => Some(w),
        None => crate::rerun::epq::inits::cte_write_recheck(px, k),
    };
    match inits {
        Some((force, fresh, key)) => e.forcing(force).arming(fresh).keyed(key),
        None => e,
    }
}

/// D3838 ROWC-INIT (ops/modifytable/rowop_epq.rs `in_ir`): recheck `e` of the data-modifying CTE's write over pipeline
/// `k` arming the InitPlans at its ModifyTable its first recheck runs again (inits.rs `cte_write_recheck`), as
/// `epq_of_subs` does for the own scan's: the nest's, the built and the drained COPY forms.
pub(crate) fn cte_armed<'g>(px: &mut Px<'g, '_>, k: usize, e: Epq<'g>) -> Epq<'g> {
    match crate::rerun::epq::inits::cte_write_recheck(px, k) {
        Some((force, fresh, key)) => e.forcing(force).arming(fresh).keyed(key),
        None => e,
    }
}

/// lockrows REFFETCH (write/lockrows.rs `ref_limit`): `epq_of` whose ops below the step run one INNER hash probe of a
/// REFERENCE-marked relation's own scan, `fetch` = (that build's pipeline, the mark's junk ctid column in the step's row,
/// its relation): the probe's redo is `Redo::Ref`. D3567 REFCHAIN: or several such joins (hash probes, REFNEST's nests), one
/// fetch each in the order of their ops.
pub(crate) fn epq_of_ref<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, j: usize, node: i32, relid: u32, sys: ShapeWhy, fetch: &[(usize, i16, u32)]) -> Epq<'g> {
    epq_with(px, k, j, node, relid, sys, false, fetch)
}

/// D3802 SORTCHAIN (rerun/epq/sort_chain.rs): the recheck of a lock over a Sort whose producer `q` (up to its sort build
/// `jq`) reads a REFERENCE relation's scan and joins the locked relation as a nest's inner further on: the source row
/// refetched at its junk ctid, `from` = (that column, its relation) (`Redo::Source`), then `q`'s redo, each join's fetch in
/// `fetch` in the order `redo_of` takes them (a nest inner's own joins' fetches right after the nest's), the locked
/// inner's entry `HELD`: its row the newest version in the lock site `site` (made first by the lock, ops/lockrows/lower.rs
/// `sort_chain_lock`). PostgreSQL's EvalPlanQual reruns Sort -> the joins over every REFERENCE row refetched and the
/// newest version, and the Sort hands that one row on.
#[allow(clippy::too_many_arguments)]
pub(crate) fn epq_of_sort_chain<'g, 'p>(px: &mut Px<'g, 'p>, (q, jq): (usize, usize), node: i32, relid: u32, sys: ShapeWhy, (col, frel): (i16, u32), fetch: &[(usize, i16, u32)], site: &Rc<core::cell::Cell<usize>>) -> Epq<'g> {
    let ex = iface::expr_face(&node);
    let mcx = px.mcx();
    let gx = px.gx;
    let Some(tid) = typefacts::facts_of(gx.facts, TIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the tid type the catalog does not know" }) };
    let natts = gx.facts.rel(frel).map_or(0, |r| r.natts.max(0) as u16);
    let fsite = px.add_late_site(SiteDesc::write(frel, natts));
    let n = scan_node(&gx.pipelines()[q]);
    let mut redo = mcx::pg_vec![in mcx; Redo::Source(mcx::box_in(Fetch { site: fsite, relid: frel, col, tid, atts: atts_of(gx, frel), build: PgVec::new_in(mcx), node: n, quals: PgVec::new_in(mcx), params: PgVec::new_in(mcx), held: false, mark: 0, left: false, copy: None }, mcx))];
    let (rest, used) = redo_n(px, q, jq, node, sys, (false, fetch), ex);
    redo.extend_in(rest);
    if used != fetch.len() || hold(&mut redo, site) != 1 {
        crate::refuse!(node, Reason::Bug { payload: "a SORTCHAIN redo whose joins do not take its fetches, the locked inner's once" });
    }
    Epq { atts: atts_of(gx, relid), redo, ex, union: None, force: PgVec::new_in(mcx), fresh: PgVec::new_in(mcx), key: 0, twin: false, own: PgVec::new_in(mcx) }
}

/// D3802 SORTCHAIN: the `HELD` fetches of `redo` (its builds' too) read lock site `site`; their count.
fn hold(redo: &mut [Redo<'_>], site: &Rc<core::cell::Cell<usize>>) -> usize {
    crate::stack_check();
    let mut n = 0;
    for r in redo.iter_mut() {
        if let Redo::Ref(f) | Redo::Outer(f) | Redo::Source(f) = r {
            if f.held && f.col == HELD {
                f.site = site.clone();
                n += 1;
            }
            n += hold(&mut f.build, site);
        }
    }
    n
}

/// lockrows REFBUILD (write/lockrows.rs `ref_build`): the recheck of a lock in probe pipeline `k` (its INNER hash probe op
/// `jp`, the lock op `j`) whose locked relation is on the build side: the arm `arm` = (pipeline, its ops before its feed or
/// build) whose scan reads the locked leaf `relid`, then the union's emit `union` = (pipeline, its ops before the build)
/// when the build side is a family's union, as the build row (the Hash's); the probe side's REFERENCE row refetched at its
/// junk ctid, `fetch` = (that column in the step's row, its relation), its pipeline's recheck quals and ops before the probe
/// redone over it (`Redo::Outer`); the hash clauses and the join qual over the pair; then `k`'s ops between the probe and
/// the lock. PostgreSQL's EvalPlanQual reruns the join over the test tuple (the other leaves' marks inactive) and the row it
/// refetches. D3777 LOCKMERGE (ops/lockrows/lower.rs `ref_merge`): or op `jp` a merge join (its clauses and join qual), `so`
/// the merge outer's Sort producer and its sort build op when that side is sorted: the refetched row's redo that
/// producer's part, then `k`'s ops before the merge (its emit).
#[allow(clippy::too_many_arguments)]
pub(crate) fn epq_of_build<'g, 'p>(px: &mut Px<'g, 'p>, arm: (usize, usize), union: Option<(usize, usize)>, (k, jp, j): (usize, usize, usize), (col, frel): (i16, u32), node: i32, relid: u32, sys: ShapeWhy, so: Option<(usize, usize)>) -> Epq<'g> {
    let ex = iface::expr_face(&node);
    let mcx = px.mcx();
    let gx = px.gx;
    let atts = atts_of(gx, relid);
    let mut redo = redo_of(px, arm.0, arm.1, node, sys, (false, &[]), ex);
    if let Some((u, ju)) = union {
        redo.extend_in(ops_at(px, u, 0..ju, node, sys, ex));
    }
    let pk = &gx.pipelines()[k];
    let Op::Join(JoinDesc::Probe { clauses, qual, .. } | JoinDesc::Merge { clauses, qual, .. }) = &pk.ops[jp] else { crate::refuse!(node, Reason::Bug { payload: "a REFBUILD lock without its probe" }) };
    let n = crate::rerun::inner::op_node(pk, jp);
    let outer = match so {
        Some((q, jq)) => {
            let mut r = redo_of(px, q, jq, node, sys, (false, &[]), ex);
            r.extend_in(ops_at(px, k, 0..jp, node, sys, ex));
            r
        }
        None => redo_of(px, k, jp, node, sys, (false, &[]), ex),
    };
    let Some(tid) = typefacts::facts_of(gx.facts, TIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the tid type the catalog does not know" }) };
    let qs = clauses.iter().chain(qual.iter()).map(|q| ex.0.compile(px, q, None, n)).collect_in(mcx);
    let natts = gx.facts.rel(frel).map_or(0, |r| r.natts.max(0) as u16);
    let site = px.add_late_site(SiteDesc::write(frel, natts));
    redo.push_in(Redo::Outer(mcx::box_in(Fetch { site, relid: frel, col, tid, atts: atts_of(gx, frel), build: outer, node: n, quals: qs, params: PgVec::new_in(mcx), held: false, mark: 0, left: false, copy: None }, mcx)));
    redo.extend_in(ops_at(px, k, jp + 1..j, node, sys, ex));
    Epq { atts, redo, ex, union: None, force: PgVec::new_in(mcx), fresh: PgVec::new_in(mcx), key: 0, twin: false, own: PgVec::new_in(mcx) }
}

/// D3565 REFDRAIN (ops/lockrows/plan.rs `ref_drain`): the recheck of a lock over the Sort of a Hash Right Anti Join's drain,
/// pipeline `q` (its Drain op `jd`, its sort build `jq`), whose build is the locked relation's own scan: the build's
/// pipeline `bq` up to its build op `jb` over the newest version (the Hash's one row), the probe side's columns NULL
/// (`Redo::Drain`), then `q`'s ops between the Drain and the sort build. PostgreSQL's EvalPlanQual reruns the join over
/// the test tuple alone (the REFERENCE relation's scan empty) and the Sort hands the drained row to LockRows; the NOT
/// EXISTS is not tested again.
pub(crate) fn epq_of_drain<'g, 'p>(px: &mut Px<'g, 'p>, (bq, jb): (usize, usize), (q, jd, jq): (usize, usize, usize), node: i32, relid: u32, sys: ShapeWhy) -> Epq<'g> {
    let ex = iface::expr_face(&node);
    let mcx = px.mcx();
    let gx = px.gx;
    let atts = atts_of(gx, relid);
    let mut redo = redo_of(px, bq, jb, node, sys, (false, &[]), ex);
    let Some(vars) = crate::ops::hashjoin::drain::source::outer_vars(&gx.pipelines()[q], px.mcx()) else { crate::refuse!(node, Reason::Shape { why: sys }) };
    let mut cols = mcx::reserved_in(vars.len(), mcx);
    for (a, typid) in vars {
        let Some(tf) = typefacts::facts_of(gx.facts, typid) else { crate::refuse!(node, Reason::Shape { why: sys }) };
        cols.push_in(((a - 1) as usize, ExprOut { rep: Rep::of_storage(tf.typlen, tf.byval), typid, life: ExprLife::Row }));
    }
    redo.push_in(Redo::Drain(cols));
    redo.extend_in(ops_at(px, q, jd + 1..jq, node, sys, ex));
    Epq { atts, redo, ex, union: None, force: PgVec::new_in(mcx), fresh: PgVec::new_in(mcx), key: 0, twin: false, own: PgVec::new_in(mcx) }
}

/// D3647 BUILT-COPY (modifytable rowop_epq.rs `built_copy`): the recheck of write op `j` of probe pipeline `k` whose target
/// is the build side of its INNER hash probe op `jp`: the build's pipeline `bq` up to its build op `jb` over the newest
/// version (the Hash's one row), then the probe side's row from the write's COPY mark `(col, rti)`: its stored record's
/// fields as `k`'s input columns at the mark's entry op `at` (D3569's copy leg, `CopyRow`), `k`'s ops from there up to the
/// probe redone over them, the hash clauses and the join qual over the pair (`Redo::Outer`, REFBUILD's shape), then `k`'s
/// ops between the probe and the write. PostgreSQL's EvalPlanQual hands the CTE Scan its stored row and reruns the Hash
/// Join over it and the test tuple; the CTE is never run again.
#[allow(clippy::too_many_arguments)]
pub(crate) fn epq_of_build_copy<'g, 'p>(px: &mut Px<'g, 'p>, (bq, jb): (usize, usize), (k, at, jp, j): (usize, usize, usize, usize), (col, rti): (i16, u32), node: i32, relid: u32, sys: ShapeWhy) -> Epq<'g> {
    built_outer_copy(px, (bq, jb), (k, at, jp), (k, jp + 1, j), (col, rti), node, relid, sys)
}

/// D3870 BUILT-HELD (write/rowop_epq.rs `built_held`): the recheck of write op `j` of pipeline `k` whose target is hashed
/// on the build side of its INNER probe op `jp`: the build's redo over the newest version (`redo_of` up to its build op
/// `jb`: the Hash's one row), joined to the probe row the probe held while it handed the match on (hashjoin walk.rs
/// `Env::redo_outers`), the probe's hash clauses and join qual over the pair (`Redo::InnerOf`, the probe's node), then
/// `k`'s ops after the probe. PostgreSQL's EvalPlanQual reruns the probe side over the COPY mark's stored row, which
/// makes that same row again (rowop_epq.rs `built_held`'s quiet entry), and the Hash Join over the pair.
pub(crate) fn epq_of_built_held<'g, 'p>(px: &mut Px<'g, 'p>, (bq, jb): (usize, usize), (k, jp, j): (usize, usize, usize), node: i32, relid: u32, sys: ShapeWhy) -> Epq<'g> {
    let ex = iface::expr_face(&node);
    let mcx = px.mcx();
    let gx = px.gx;
    let atts = atts_of(gx, relid);
    let mut redo = redo_of(px, bq, jb, node, sys, (false, &[]), ex);
    let pk = &gx.pipelines()[k];
    let Op::Join(JoinDesc::Probe { clauses, qual, .. }) = &pk.ops[jp] else { crate::refuse!(node, Reason::Bug { payload: "a BUILT-HELD write without its probe" }) };
    let n = crate::rerun::inner::op_node(pk, jp);
    let qs = clauses.iter().chain(qual.iter()).map(|q| ex.0.compile(px, q, None, n)).collect_in(mcx);
    redo.push_in(Redo::InnerOf { node: n, quals: qs });
    redo.extend_in(ops_at(px, k, jp + 1..j, node, sys, ex));
    Epq { atts, redo, ex, union: None, force: PgVec::new_in(mcx), fresh: PgVec::new_in(mcx), key: 0, twin: false, own: PgVec::new_in(mcx) }
}

/// D3824 DRAIN-COPY (modifytable rowop_epq.rs `drain_copy`): the recheck of write op `j` of drain pipeline `k` whose rows
/// are its RIGHT SEMI drain's (Drain op `jd`) build rows, the target's own scan: BUILT-COPY's redo (`epq_of_build_copy`)
/// with the probe in its own pipeline `pr` (the COPY mark's stored row entering at its op `at`, its probe op `jp`) and
/// the ops after the join the drain pipeline's between the Drain op and the write. PostgreSQL's EvalPlanQual hands the
/// CTE Scan its stored row and reruns the Hash Right Semi Join over it and the test tuple (the Hash's one row), which
/// hands the newest version on when the hash clauses and the join qual pass; the CTE is never run again.
#[allow(clippy::too_many_arguments)]
pub(crate) fn epq_of_drain_copy<'g, 'p>(px: &mut Px<'g, 'p>, (bq, jb): (usize, usize), (pr, at, jp): (usize, usize, usize), (k, jd, j): (usize, usize, usize), (col, rti): (i16, u32), node: i32, relid: u32, sys: ShapeWhy) -> Epq<'g> {
    built_outer_copy(px, (bq, jb), (pr, at, jp), (k, jd + 1, j), (col, rti), node, relid, sys)
}

/// D3647 BUILT-COPY / D3824 DRAIN-COPY: the build's pipeline `bq` up to its build op `jb` over the newest version, the
/// probe side's row from the COPY mark `(col, rti)`'s stored record entering probe pipeline `pk` at op `at`, `pk`'s ops
/// from there up to its probe op `jp`, the hash clauses and join qual over the pair (`Redo::Outer`), then pipeline `k`'s
/// ops `from..j` over the joined row.
#[allow(clippy::too_many_arguments)]
fn built_outer_copy<'g, 'p>(px: &mut Px<'g, 'p>, (bq, jb): (usize, usize), (pk, at, jp): (usize, usize, usize), (k, from, j): (usize, usize, usize), (col, rti): (i16, u32), node: i32, relid: u32, sys: ShapeWhy) -> Epq<'g> {
    let ex = iface::expr_face(&node);
    let mcx = px.mcx();
    let gx = px.gx;
    let atts = atts_of(gx, relid);
    let mut redo = redo_of(px, bq, jb, node, sys, (false, &[]), ex);
    let pp = &gx.pipelines()[pk];
    let Op::Join(JoinDesc::Probe { clauses, qual, .. }) = &pp.ops[jp] else { crate::refuse!(node, Reason::Bug { payload: "a BUILT-COPY write without its probe" }) };
    let n = crate::rerun::inner::op_node(pp, jp);
    let Some(r) = gx.graph.record_rows.iter().find(|r| r.scanrelid == rti) else { crate::refuse!(node, Reason::Shape { why: sys }) };
    let (Some(rec), Some(tid)) = (typefacts::facts_of(gx.facts, RECORDOID), typefacts::facts_of(gx.facts, TIDOID)) else { crate::refuse!(node, Reason::Bug { payload: "the record or tid type the catalog does not know" }) };
    let fields = crate::rerun::epq::copy_row::cols_of(gx, r, node);
    let build = ops_at(px, pk, at..jp, node, sys, ex);
    let qs = clauses.iter().chain(qual.iter()).map(|q| ex.0.compile(px, q, None, n)).collect_in(mcx);
    let copy = Some(mcx::box_in(CopyRow { rec, fields }, mcx));
    let site = Rc::new(core::cell::Cell::new(0)); // alloc: Program
    redo.push_in(Redo::Outer(mcx::box_in(Fetch { site, relid: crate::rerun::epq::ref_copy::COPY_ROW, col, tid, atts: PgVec::new_in(mcx), build, node: n, quals: qs, params: PgVec::new_in(mcx), held: false, mark: 0, left: false, copy }, mcx)));
    redo.extend_in(ops_at(px, k, from..j, node, sys, ex));
    Epq { atts, redo, ex, union: None, force: PgVec::new_in(mcx), fresh: PgVec::new_in(mcx), key: 0, twin: false, own: PgVec::new_in(mcx) }
}

/// lockrows REFBUILD: pipeline `k`'s ops `range`, each compiled at its own node (a union's emit reads the arms' columns, the
/// ops above a probe the join's OUTER / INNER columns): filters, projections; a gate (a constant the row passed) and the
/// union's marker redo nothing.
fn ops_at<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, range: core::ops::Range<usize>, node: i32, sys: ShapeWhy, ex: &'static ExprFace) -> PgVec<'g, Redo<'g>> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let m = px.mcx();
    let quals = |q: Node<'p>| q.as_list().map_or_else(|| mcx::pg_vec![in m; q], |l| l.iter().collect_in(m));
    let mut redo = PgVec::new_in(m);
    let last = range.end.saturating_sub(1);
    for i in range {
        let at = crate::rerun::inner::op_node(p, i);
        match &p.ops[i] {
            Op::Filter(q) => redo.extend_in(quals(*q).into_iter().map(|q| Redo::Qual(ex.0.compile(px, q, None, at)))),
            Op::Project(l) => {
                let mut cols = PgVec::new_in(m);
                for te in l.iter().filter_map(|n| n.as_target_entry()) {
                    // D3569 REFCOPY: a nest inner's (past the graph's pipelines) has no recorded outputs (`redo_of`'s)
                    let out = gx.out_of(te.expr).or_else(|| (k >= gx.pipelines().len()).then(|| crate::step::project::out_of(gx, te.expr, at)));
                    let Some(out) = out else { crate::refuse!(node, Reason::Shape { why: sys }) };
                    cols.push_in(((te.resno - 1) as usize, ex.0.compile(px, te.expr, None, at), out));
                }
                redo.push_in(Redo::Project(cols));
            }
            Op::Gate(_) | Op::Append(gravity_decompose::ops::shared::append::AppendDesc::Union { prune: None, .. }) => {}
            // D3563 (`epq_of_tail`): a per-row SubPlan step, evaluated again over the row so far (`Redo::Sub`)
            Op::SubPlan(d) => redo.push_in(Redo::Sub(d.plan_id, crate::ops::subplan::per_row::references(p, last, d.plan_id) == 0)),
            _ => crate::refuse!(node, Reason::Bug { payload: BUILD_OP }),
        }
    }
    redo
}

#[allow(clippy::too_many_arguments)]
fn epq_with<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, j: usize, node: i32, relid: u32, sys: ShapeWhy, subs: bool, fetch: &[(usize, i16, u32)]) -> Epq<'g> {
    let ex = iface::expr_face(&node);
    let mcx = px.mcx();
    let atts = atts_of(px.gx, relid);
    let redo = redo_of(px, k, j, node, sys, (subs, fetch), ex);
    Epq { atts, redo, ex, union: None, force: PgVec::new_in(mcx), fresh: PgVec::new_in(mcx), key: 0, twin: false, own: PgVec::new_in(mcx) }
}

/// modifytable D2734 ROWC-UNION (gen mt-0354: `WITH d AS (DELETE FROM ph WHERE k = 26 RETURNING ..) ..`, ph partitioned): the
/// recheck of write op `j` of pipeline `k` whose rows come out of one settled union of its result relations' feeds
/// (merge/route.rs `union_targets`: `feeds[i]` is `targets[i]`'s, a heap range or index scan under filters and projections
/// into the union's slot; `k` reads the union first). PostgreSQL's EvalPlanQual substitutes the row's own relation's scan
/// with the newest version and leaves every other result relation's scan empty (EvalPlanQualStart's relsubs_blocked),
/// so the plan re-run is that relation's scan quals and filters over the newest version, its projection into the
/// Append's row, then the ops above the Append. Here each relation's redo is that feed's (`redo_of` up to its feed op)
/// then `k`'s ops between the union's marker and the write (`ops_at`), over the write site's cells as that relation's
/// attributes (the multi-target host's `epq_row`: the row's own target's newest version, in its layout); the row's
/// relation is its junk tableoid (`tableoid`, as write/update_built.rs `union_rel` reads it).
#[allow(clippy::too_many_arguments)]
pub(crate) fn epq_of_union<'g, 'p>(px: &mut Px<'g, 'p>, (k, j): (usize, usize), feeds: &[usize], relids: &[u32], tableoid: (i16, TypeFacts), node: i32, sys: ShapeWhy) -> Epq<'g> {
    let ex = iface::expr_face(&node);
    let mcx = px.mcx();
    let mut leaves = mcx::reserved_in(feeds.len(), mcx);
    for (&f, &relid) in feeds.iter().zip(relids.iter()) {
        let jf = px.gx.pipelines()[f].ops.len().saturating_sub(1);
        let mut redo = redo_of(px, f, jf, node, sys, (false, &[]), ex);
        redo.extend_in(ops_at(px, k, 0..j, node, sys, ex));
        leaves.push_in((relid, Epq { atts: atts_of(px.gx, relid), redo, ex, union: None, force: PgVec::new_in(mcx), fresh: PgVec::new_in(mcx), key: 0, twin: false, own: PgVec::new_in(mcx) }));
    }
    Epq { atts: PgVec::new_in(mcx), redo: PgVec::new_in(mcx), ex, union: Some((tableoid, leaves)), force: PgVec::new_in(mcx), fresh: PgVec::new_in(mcx), key: 0, twin: false, own: PgVec::new_in(mcx) }
}

/// modifytable D2739 NEST-INNER (subq-0323: `WITH d AS (DELETE FROM t WHERE k IN (SELECT id FROM o WHERE ..) RETURNING ..)`, a
/// nested loop whose outer is o's own scan, REFERENCE-marked, and whose inner is t's index scan keyed by o's column): the
/// recheck of write op `j` of pipeline `k` whose target is the inner `ik` of its INNER nest op `jn`: the inner's redo over
/// the newest version (its recheck quals read the nest's parameters, still the held outer row's), the join to that
/// outer row and the nest's quals (`Redo::InnerOf`), then `k`'s ops after the nest.
pub(crate) fn epq_of_inner<'g, 'p>(px: &mut Px<'g, 'p>, (k, j): (usize, usize), (jn, ik): (usize, usize), relid: u32, node: i32, sys: ShapeWhy) -> Epq<'g> {
    let ex = iface::expr_face(&node);
    let mcx = px.mcx();
    let gx = px.gx;
    let atts = atts_of(gx, relid);
    // D3379 LOCKNEST (ref_nest.rs `lock_form`): up to the inner's Memoize (EvalPlanQual's fresh one caches nothing)
    let ji = crate::rerun::epq::ref_nest::inner_end(&gx.pipelines()[ik]);
    // D3601 (7) (modifytable rowop_epq.rs `nest_inner`): the inner's hashed SubPlan steps too, probing their sets as
    // built (`Redo::Sub`)
    let subs = gx.pipelines()[ik].ops[..ji].iter().any(|o| matches!(o, Op::SubPlan(_)));
    let mut redo = redo_of(px, ik, ji, node, sys, (subs, &[]), ex);
    let p = &gx.pipelines()[k];
    let n = crate::rerun::inner::op_node(p, jn);
    let Op::Join(JoinDesc::Nest { qual, .. }) = &p.ops[jn] else { crate::refuse!(node, Reason::Bug { payload: "a nest-inner recheck whose nest op is not a nest" }) };
    let qs = qual.iter().map(|q| ex.0.compile(px, q, None, n)).collect_in(mcx);
    redo.push_in(Redo::InnerOf { node: n, quals: qs });
    redo.extend_in(ops_at(px, k, jn + 1..j, node, sys, ex));
    Epq { atts, redo, ex, union: None, force: PgVec::new_in(mcx), fresh: PgVec::new_in(mcx), key: 0, twin: false, own: PgVec::new_in(mcx) }
}

/// D3379 LOCKPAIR (ref_nest.rs `pair_form`): the recheck of lock op `j` of pipeline `k` over its INNER nest op `jn` whose
/// outer (the pipeline's own scan of `relid`) and inner `ik` (a scan of `irel`) are both locked: the outer's recheck quals
/// and ops before the nest over the main row (the outer lock site's cells), the nest's parameters from it while the
/// inner's redo up to its Memoize runs over the inner lock site `isite`'s cells (`Fetch::held`: each site the mark's
/// EvalPlanQual test tuple), the join quals over the two, then `k`'s ops after the nest. D3379 PAIRPROBE (ref_nest.rs
/// `limited_probe`): or op `jn` the nest's catalog probe and `ik` its build's pipeline: the build's ops before its Build
/// over the inner site's cells (the Hash's row), then the probe's clauses and join qual (PostgreSQL's Nest Loop join qual).
/// D3727 PAIRHASH: or PostgreSQL's own Hash Join's probe (its hash clauses and join qual), the same redo. D3777 PAIRMERGE
/// (ref_nest.rs `limited_merge`): or its Merge Join, the inner `ik` the merge inner's pipeline (its ops before its Sort),
/// then the merge clauses and join qual; `so`, the outer's Sort producer and its sort build op when that side is sorted:
/// the outer's part that producer's recheck quals and ops before the Sort, then `k`'s ops before the merge (its emit).
#[allow(clippy::too_many_arguments)]
pub(crate) fn epq_of_pair<'g, 'p>(px: &mut Px<'g, 'p>, (k, j): (usize, usize), (jn, ik): (usize, usize), (relid, irel): (u32, u32), isite: Rc<core::cell::Cell<usize>>, node: i32, sys: ShapeWhy, so: Option<(usize, usize)>) -> Epq<'g> {
    let ex = iface::expr_face(&node);
    let mcx = px.mcx();
    let gx = px.gx;
    let atts = atts_of(gx, relid);
    let mut redo = match so {
        Some((q, jq)) => {
            let mut r = redo_of(px, q, jq, node, sys, (false, &[]), ex);
            r.extend_in(ops_at(px, k, 0..jn, node, sys, ex));
            r
        }
        None => redo_of(px, k, jn, node, sys, (false, &[]), ex),
    };
    let p = &gx.pipelines()[k];
    let n = crate::rerun::inner::op_node(p, jn);
    let (build, ps, qs) = match &p.ops[jn] {
        Op::Join(JoinDesc::Nest { params, qual, .. }) => {
            let build = redo_of(px, ik, crate::rerun::epq::ref_nest::inner_end(&gx.pipelines()[ik]), node, sys, (false, &[]), ex);
            let ps = crate::rerun::epq::ref_nest::params(px, params, n, ex);
            (build, ps, qual.iter().map(|q| ex.0.compile(px, q, None, n)).collect_in(mcx))
        }
        Op::Join(JoinDesc::Probe { clauses, qual, .. }) => {
            let pb = &gx.pipelines()[ik];
            let jb = pb.ops.iter().position(|o| matches!(o, Op::Join(JoinDesc::Build { .. }))).unwrap_or(pb.ops.len());
            let build = redo_of(px, ik, jb, node, sys, (false, &[]), ex);
            (build, PgVec::new_in(mcx), clauses.iter().chain(qual.iter()).map(|q| ex.0.compile(px, q, None, n)).collect_in(mcx))
        }
        Op::Join(JoinDesc::Merge { clauses, qual, .. }) => {
            let Some(ie) = crate::rerun::epq::ref_nest::merge_side(&gx.pipelines()[ik]) else { crate::refuse!(node, Reason::Bug { payload: "a pair recheck whose merge inner is not its relation's scan" }) };
            let build = redo_of(px, ik, ie, node, sys, (false, &[]), ex);
            (build, PgVec::new_in(mcx), clauses.iter().chain(qual.iter()).map(|q| ex.0.compile(px, q, None, n)).collect_in(mcx))
        }
        _ => crate::refuse!(node, Reason::Bug { payload: "a pair recheck whose join op is neither a nest, a probe nor a merge" }),
    };
    let Some(tid) = typefacts::facts_of(gx.facts, TIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the tid type the catalog does not know" }) };
    redo.push_in(Redo::Ref(mcx::box_in(Fetch { site: isite, relid: irel, col: 0, tid, atts: atts_of(gx, irel), build, node: n, quals: qs, params: ps, held: true, mark: 0, left: false, copy: None }, mcx)));
    redo.extend_in(ops_at(px, k, jn + 1..j, node, sys, ex));
    Epq { atts, redo, ex, union: None, force: PgVec::new_in(mcx), fresh: PgVec::new_in(mcx), key: 0, twin: false, own: PgVec::new_in(mcx) }
}

/// An empty recheck: a form that carries its own redo (merge/epq.rs `Form::First`) holds it where an `Epq` is asked for.
pub(crate) fn none<'g>(node: i32, mcx: Mcx<'g>) -> Epq<'g> {
    Epq { atts: PgVec::new_in(mcx), redo: PgVec::new_in(mcx), ex: iface::expr_face(&node), union: None, force: PgVec::new_in(mcx), fresh: PgVec::new_in(mcx), key: 0, twin: false, own: PgVec::new_in(mcx) }
}

/// D2015 UNION-OUTER (held-out parts-0083, merge/nest_arms.rs `first`): a MERGE through a partitioned root over a LEFT
/// nested loop whose inner is the Append of its result relations' scans and whose outer is a flattened UNION ALL whose
/// first arm is one constant row (a Result). PostgreSQL's EvalPlanQual hands the union's COPY mark to no scan: the rerun
/// evaluates the arms again from the first, and EvalPlanQualNext takes the first joined row, which is that arm's row
/// joined to the newest version through the row's own leaf's scan (EvalPlanQualStart blocks every other result relation)
/// under the nest's parameters taken from that row, NULL-extended when it does not pass. ExecMergeMatched decides MATCHED
/// by that row's ctid alone, then runs the actions over the newest version with the source columns of the row the write
/// stopped at (`context->planSlot`), or the NOT MATCHED actions over that same row. Here: the first arm's row (`first`: its
/// projection over no row, then the write pipeline's ops before the nest), the nest's parameters set from it while the row's
/// own leaf's redo runs over the newest version (`leaves`, by the row's junk tableoid: its arm's recheck quals, filters and
/// projection, then the inner union's emit ops), and the nest's quals over the two joined. A pass is MATCHED: the step's
/// own outer row (nest.loop's `Env::redo_outers`) joined to the newest inner row, then the write pipeline's ops after the
/// nest (`after`); any failure is NOT MATCHED.
pub(crate) struct FirstOuter<'g> {
    ex: &'static ExprFace,
    first: PgVec<'g, Redo<'g>>,
    nest: i32,
    params: PgVec<'g, (i32, ExId)>,
    quals: PgVec<'g, ExId>,
    /// Per result relation: its relid, its attributes by attno (the write site's cells: the multi-target host's `epq_row`
    /// fills them in that relation's layout) and its redo.
    leaves: PgVec<'g, (u32, PgVec<'g, Option<ExprOut>>, PgVec<'g, Redo<'g>>)>,
    after: PgVec<'g, Redo<'g>>,
    tableoid: (i16, TypeFacts),
}

/// `FirstOuter` for write op `j` of pipeline `k` over its nest op `jn` (its inner `ik`): the outer union's first feed
/// `feed0`; the inner union's feeds `feeds`, each the arm of result relation `relids[i]`; the row's junk tableoid column
/// and the oid type's facts (`tableoid`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn first_outer<'g, 'p>(px: &mut Px<'g, 'p>, (k, j): (usize, usize), (jn, ik): (usize, usize), feed0: usize, (feeds, relids): (&[usize], &[u32]), tableoid: (i16, TypeFacts), node: i32, sys: ShapeWhy) -> FirstOuter<'g> {
    let ex = iface::expr_face(&node);
    let (gx, mcx) = (px.gx, px.mcx());
    let j0 = gx.pipelines()[feed0].ops.len().saturating_sub(1);
    let mut first = redo_of(px, feed0, j0, node, sys, (false, &[]), ex);
    first.extend_in(ops_at(px, k, 1..jn, node, sys, ex));
    let p = &gx.pipelines()[k];
    let n = crate::rerun::inner::op_node(p, jn);
    let Op::Join(JoinDesc::Nest { params, qual, .. }) = &p.ops[jn] else { crate::refuse!(node, Reason::Bug { payload: "a union-outer recheck whose nest op is not a nest" }) };
    let params = crate::rerun::epq::ref_nest::params(px, params, n, ex);
    let quals = qual.iter().map(|q| ex.0.compile(px, q, None, n)).collect_in(mcx);
    // the inner union's marker first (its run-time pruning keeps every leaf whose row passes the arm's quals), then its ops
    let ji = gx.pipelines()[ik].ops.len();
    let mut leaves = mcx::reserved_in(feeds.len(), mcx);
    for (&f, &relid) in feeds.iter().zip(relids.iter()) {
        let jf = gx.pipelines()[f].ops.len().saturating_sub(1);
        let mut redo = redo_of(px, f, jf, node, sys, (false, &[]), ex);
        redo.extend_in(ops_at(px, ik, 1..ji, node, sys, ex));
        leaves.push_in((relid, atts_iter(gx, relid).collect_in(mcx), redo));
    }
    let after = ops_at(px, k, jn + 1..j, node, sys, ex);
    FirstOuter { ex, first, nest: n, params, quals, leaves, after, tableoid }
}

impl FirstOuter<'_> {
    /// Whether this lowering holds the nest's outer row (none past its NULL-extended row: nest.loop's final row).
    pub(crate) fn reaches(&self, env: &Env) -> bool {
        env.redo_outers.iter().any(|x| x.0 == self.nest)
    }

    /// The recheck at write site `site` over the step's row `at`: `matched` over the write's row on a pass (its carry the
    /// result), a branch to `l_miss` (a block taking `at`'s carry) on any failure.
    pub(crate) fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, at: &RowVals, (site, node): (usize, i32), l_miss: Block, matched: Next<'_, 'e>) -> Carry<'e> {
        let e = Epq { atts: PgVec::new_in(env.mcx), redo: PgVec::new_in(env.mcx), ex: self.ex, union: None, force: PgVec::new_in(env.mcx), fresh: PgVec::new_in(env.mcx), key: 0, twin: false, own: PgVec::new_in(env.mcx) };
        let types = at.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let l_out = b.create_block(&types[..]);
        let (oc, ov, ok, _) = match env.redo_outers.iter().rev().find(|x| x.0 == self.nest) {
            Some(x) => (mcx::to_vec_in(&x.1.0[..], env.mcx), mcx::to_vec_in(&x.1.1[..], env.mcx), mcx::to_vec_in(&x.1.2[..], env.mcx), x.1.3),
            None => crate::refuse!(node, Reason::Shape { why: NEST_ROW }),
        };
        let inner = env.inner.take();
        env.memo.clear();
        let mcx = env.mcx;
        let none = RowVals { row: Row::new_in(mcx), vals: mcx::PgVec::new_in(mcx), cols: mcx::PgVec::new_in(mcx), carry: at.carry.copy_in(mcx), next: at.next, at: at.at };
        let r0 = e.run(b, env, mcx, at, none, &self.first, node, l_miss);
        let Some(cr) = at.col(self.tableoid.0) else { crate::refuse!(node, Reason::Bug { payload: "a union-outer MERGE's tableoid past the row the recheck took" }) };
        let mut cx = env.lx(b, node);
        let (rel, _) = crate::rep::datum(&mut cx, cr, &self.tableoid.1);
        env.memo = cx.memo;
        for (relid, atts, redo) in self.leaves.iter() {
            let id = b.iconst(Type::I64, i64::from(*relid));
            let mine = b.icmp(IntCond::Eq, rel, id);
            let (l_mine, l_next) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(mine, (l_mine, &[]), (l_next, &[]));
            b.switch_to(l_mine);
            env.inner = None;
            env.memo.clear();
            let saved = crate::rerun::epq::ref_nest::set(b, env, self.nest, &self.params, &r0, self.ex);
            let l_fail = crate::rerun::epq::ref_nest::fail_to(b, env, &saved, l_miss, &types[..]);
            let newest = site_row(b, env, site, atts, at);
            let ri = e.run(b, env, mcx, at, newest, redo, node, l_fail);
            crate::rerun::epq::ref_nest::restore(b, env, &saved);
            env.memo.clear();
            let (row1, map) = crate::ops::nestloop::lower::joined(&r0, (&ri.row.cells, &ri.vals, &ri.cols), &at.carry, env.mcx);
            env.inner = Some(map);
            for &q in self.quals.iter() {
                let (pass, l_on) = (self.ex.0.pred(b, env, self.nest, &row1, q), b.create_block(&[]));
                b.cond_br(pass, (l_on, &[]), (l_miss, &at.carry.params()));
                b.switch_to(l_on);
            }
            env.memo.clear();
            let outer = RowVals { row: Row { cells: mcx::to_vec_in(&oc[..], mcx) }, vals: mcx::to_vec_in(&ov[..], mcx), cols: mcx::to_vec_in(&ok[..], mcx), carry: at.carry.copy_in(mcx), next: at.next, at: at.at };
            let (row2, map) = crate::ops::nestloop::lower::joined(&outer, (&ri.row.cells, &ri.vals, &ri.cols), &at.carry, env.mcx);
            env.inner = Some(map);
            let r = e.run(b, env, mcx, at, row2, &self.after, node, l_miss);
            env.inner = inner.as_deref().map(|v| mcx::to_vec_in(v, mcx));
            env.memo.clear();
            let c = matched(b, env, &r);
            env.memo.clear();
            b.br(l_out, &c.params());
            b.switch_to(l_next);
        }
        // a row of none of the result relations (no union row is): no newest version joins
        b.br(l_miss, &at.carry.params());
        env.inner = inner;
        b.switch_to(l_out);
        Carry::from_params(b.block_params(l_out), env.mcx)
    }
}

/// The relation's attributes by attno (a dropped one `None`): a site's cells.
pub(crate) fn atts_of<'g>(gx: &crate::graph::Gx<'g, '_>, relid: u32) -> PgVec<'g, Option<ExprOut>> {
    atts_iter(gx, relid).collect_in(gx.mcx)
}

pub(crate) fn atts_iter<'g>(gx: &crate::graph::Gx<'g, '_>, relid: u32) -> impl Iterator<Item = Option<ExprOut>> + 'g {
    let facts = gx.facts;
    let natts = facts.rel(relid).map_or(0, |r| r.natts.max(0) as i16);
    (1..=natts).map(move |a| facts.att(relid, a).map(|f| ExprOut { rep: Rep::of_storage(f.typlen, f.byval), typid: f.typid, life: ExprLife::Row }))
}

/// The redo of pipeline `k`: its source's recheck quals and its ops below op `j`; `subs`: its per-row SubPlan steps too;
/// `fetch` (lockrows REFFETCH): its INNER hash probe of that build (`epq_of_ref`); D3567 REFCHAIN: each of its first joins
/// one fetch, in order.
fn redo_of<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, j: usize, node: i32, sys: ShapeWhy, (subs, fetch): (bool, &[(usize, i16, u32)]), ex: &'static ExprFace) -> PgVec<'g, Redo<'g>> {
    redo_n(px, k, j, node, sys, (subs, fetch), ex).0
}

/// `redo_of` and the count of `fetch`'s entries its joins took. D3802 SORTCHAIN: a fetched nest inner's or probe build's
/// own redo takes the entries after its join's (its own hash probes of REFERENCE builds: the Hash Join a nest's inner
/// is), so a chain's fetches are in that order; the other forms' inners and builds join nothing (their checks), so take
/// none.
fn redo_n<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, j: usize, node: i32, sys: ShapeWhy, (subs, fetch): (bool, &[(usize, i16, u32)]), ex: &'static ExprFace) -> (PgVec<'g, Redo<'g>>, usize) {
    crate::stack_check();
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    // the system-lane test reads the pipeline's first node, as it did before `scan_node`: for a bitmap source a node of
    // its tree, which has no lanes, so a whole-row read over a bitmap leaf stays served (m8_rowc_cte); the expressions
    // compile at the heap scan's node (`src`), whose lanes hold tableoid
    let first = p.node_ops.iter().find(|n| n.first_op == 0).map_or(p.node_id, |n| n.node_id);
    // D655 Row C (a): a whole-row read (the `wholerow` lane's mark) compiles to a row formed from the site row's cells (step
    // expr_row.rs), the newest version's, as EvalPlanQual's slot forms it; not over a relation with a dropped attribute,
    // whose stored value PostgreSQL's image keeps (D2789) and the site row lacks. The visibility words (xmin / xmax / cmin
    // / cmax) are the old version's lanes: refused
    let whole_ok = |relid: u32| gx.facts.rel(relid).is_some_and(|r| (1..=r.natts.max(0) as i16).all(|a| gx.facts.att(relid, a).is_some()));
    if gx.graph.scan_lanes.iter().any(|l| l.node_id == first && l.names().any(|n| n != "ctid" && n != "tableoid" && !(n == "wholerow" && whole_ok(l.relid)))) {
        crate::refuse!(node, Reason::Shape { why: sys });
    }
    let src = scan_node(p);
    // compiled at the scan's node, as the ops below the lock were: a family arm's tableoid junk column is its leaf's
    // constant there (step/expr.rs reads the scan's tableoid lane by that node)
    let m = px.mcx();
    let quals = |q: Node<'p>| q.as_list().map_or_else(|| mcx::pg_vec![in m; q], |l| l.iter().collect_in(m));
    let mut redo = PgVec::new_in(m);
    if let Source::Index { recheck, .. } | Source::Bitmap { recheck, .. } = &p.source {
        redo.extend_in(recheck.iter().map(|q| Redo::Qual(ex.0.compile(px, q, None, src))));
    }
    // D1672: PG's TidRecheck / TidRangeRecheck over the newest version's ctid (the site row's CTID_AT cell): a list's
    // clauses are OR'd (TidListEval's union), a range's AND'd (TidRangeEval's bounds); WHERE CURRENT OF rechecks nothing
    if let Source::Tid { form, quals: tq, scanrelid, .. } = &p.source {
        if *form != TidForm::Current {
            let checks = tq.iter().map(|q| tid_check(px, q, i32::from(*scanrelid), node, src, ex)).collect_in(m);
            redo.push_in(Redo::Tid(checks, *form == TidForm::Range));
        }
    }
    let mut nested = false;
    // D3567 REFCHAIN: the fetches the joins so far took
    let mut nf = 0;
    for (i, o) in p.ops[..j].iter().enumerate() {
        // D1933 (c): past a write's INNER nest (nest_outer.rs) each op compiles at its own node, as its step does (its
        // Vars are the join's OUTER / INNER columns), still with no window road; lockrows REFFETCH: past the probe too
        let at = if nested { crate::rerun::inner::op_node(p, i) } else { src };
        match o {
            Op::Filter(q) => redo.extend_in(quals(*q).into_iter().map(|q| Redo::Qual(ex.0.compile(px, q, None, at)))),
            Op::Project(l) => {
                let mut cols = PgVec::new_in(m);
                for te in l.iter().filter_map(|n| n.as_target_entry()) {
                    // lockrows SUBLOCK: a nested SubPlan's inner (past the graph's pipelines) has no recorded outputs (decompose
                    // outs.rs `annotate` walks the graph's pipelines only): its projection step's cells are step/project.rs `out_of`'s
                    let out = gx.out_of(te.expr).or_else(|| (k >= gx.pipelines().len()).then(|| crate::step::project::out_of(gx, te.expr, at)));
                    let Some(out) = out else { crate::refuse!(node, Reason::Shape { why: sys }) };
                    cols.push_in(((te.resno - 1) as usize, ex.0.compile(px, te.expr, None, at), out));
                }
                redo.push_in(Redo::Project(cols));
            }
            // D3379 REFNEST-SUB: or one past a REFNEST nest (rerun/epq/ref_nest.rs `form`), over the joined row
            // D3451 LAZYSUB: whether only the redo's ops read it (none at or past op j)
            Op::SubPlan(d) if subs || nested && !fetch.is_empty() => redo.push_in(Redo::Sub(d.plan_id, crate::ops::subplan::per_row::references(p, j.saturating_sub(1), d.plan_id) == 0)),
            // producerchain C: each of several such nests, INNER or LEFT (nest_outer.rs); pj211: or SEMI over a Material, its
            // first match the row it handed on (a failure skips the row, as INNER)
            Op::Join(JoinDesc::Nest { jointype: jt @ (JoinType::JOIN_INNER | JoinType::JOIN_LEFT | JoinType::JOIN_SEMI), params, qual, .. }) if subs && params.is_nil() => {
                nested = true;
                let n = crate::rerun::inner::op_node(p, i);
                let qs = qual.iter().map(|q| ex.0.compile(px, q, None, n)).collect_in(m);
                redo.push_in(Redo::Nest { node: n, quals: qs, left: *jt == JoinType::JOIN_LEFT });
            }
            // lockrows REFNEST (rerun/epq/ref_nest.rs): an INNER nest over the REFERENCE-marked relation's own scan, its row
            // refetched, the inner's redo up to its Memoize under the nest's parameters, then the join qual at the nest's node
            // D3560 REFNEST-LEFT: or a LEFT nest's (`Fetch::left`)
            Op::Join(JoinDesc::Nest { jointype: jt @ (JoinType::JOIN_INNER | JoinType::JOIN_LEFT), params, qual, .. }) if nf < fetch.len() => {
                let (q, col, relid) = fetch[nf];
                nf += 1;
                nested = true;
                let n = crate::rerun::inner::op_node(p, i);
                // D3569 REFCOPY: the inner a COPY mark's subquery row enters (ref_copy.rs `inner`): its stored record's
                // fields, the inner's ops from the entry redone over them, then the join qual
                if relid == crate::rerun::epq::ref_copy::COPY_ROW {
                    let s = &gx.pipelines()[q];
                    let entry = gx.graph.record_rows.iter().find_map(|r| crate::rerun::epq::copy_row::boundary(gx, q, r).map(|at| (r, at)));
                    let end = crate::rerun::epq::ref_nest::inner_end(s);
                    // the inner's columns (ref_copy.rs `outs`) known, as D3569 asked for its NULL-extended row's shape (D3818:
                    // that row now takes the stored row's form, `left_merge`)
                    let (Some((r, at)), Some(_), Some(rec), Some(tid)) = (entry, crate::rerun::epq::ref_copy::outs(gx, s, end), typefacts::facts_of(gx.facts, RECORDOID), typefacts::facts_of(gx.facts, TIDOID)) else { crate::refuse!(node, Reason::Shape { why: sys }) };
                    let fields = crate::rerun::epq::copy_row::cols_of(gx, r, node);
                    let build = ops_at(px, q, at..end, node, sys, ex);
                    let qs = qual.iter().map(|q| ex.0.compile(px, q, None, n)).collect_in(m);
                    let copy = Some(mcx::box_in(CopyRow { rec, fields }, m));
                    let site = Rc::new(core::cell::Cell::new(0)); // alloc: Program
                    redo.push_in(Redo::Ref(mcx::box_in(Fetch { site, relid, col, tid, atts: PgVec::new_in(m), build, node: n, quals: qs, params: PgVec::new_in(m), held: false, mark: 0, left: *jt == JoinType::JOIN_LEFT, copy }, m)));
                    continue;
                }
                // D3802 SORTCHAIN: the inner's own joins take the fetches after this one (`redo_n`)
                let (build, used) = redo_n(px, q, crate::rerun::epq::ref_nest::inner_end(&gx.pipelines()[q]), node, sys, (false, &fetch[nf..]), ex);
                nf += used;
                let ps = crate::rerun::epq::ref_nest::params(px, params, n, ex);
                let Some(tid) = typefacts::facts_of(gx.facts, TIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the tid type the catalog does not know" }) };
                let qs = qual.iter().map(|q| ex.0.compile(px, q, None, n)).collect_in(m);
                let natts = gx.facts.rel(relid).map_or(0, |r| r.natts.max(0) as u16);
                // D3802 SORTCHAIN: the locked relation's own inner (`HELD`) reads the lock site (`hold`), no fetch site
                let held = col == HELD;
                let site = match held {
                    true => Rc::new(core::cell::Cell::new(usize::MAX)), // alloc: Program
                    false => px.add_late_site(SiteDesc::write(relid, natts)),
                };
                let left = *jt == JoinType::JOIN_LEFT;
                redo.push_in(Redo::Ref(mcx::box_in(Fetch { site, relid, col, tid, atts: atts_of(gx, relid), build, node: n, quals: qs, params: ps, held, mark: 0, left, copy: None }, m)));
            }
            // lockrows REFFETCH: the build side's redo up to its build op (the form's: only filters, gates and projections
            // before it), then the hash clauses and the join qual at the probe's node
            // lockrows REFMERGE (lock-0009): or an INNER / SEMI merge join whose inner is that relation's own scan (its whole
            // pipeline, no build op), the merge clauses and the join qual at the join's node
            // D3596: or a LEFT hash probe's (`Fetch::left`; its NULL-extended row the build redo's form, `left_merge`)
            Op::Join(JoinDesc::Probe { jointype: jt @ (JoinType::JOIN_INNER | JoinType::JOIN_LEFT), clauses, qual, .. } | JoinDesc::Merge { jointype: jt @ (JoinType::JOIN_INNER | JoinType::JOIN_SEMI), clauses, qual, .. }) if nf < fetch.len() => {
                let (bq, col, relid) = fetch[nf];
                nf += 1;
                nested = true;
                let n = crate::rerun::inner::op_node(p, i);
                let jb = gx.pipelines()[bq].ops.iter().position(|o| matches!(o, Op::Join(JoinDesc::Build { .. }))).unwrap_or(gx.pipelines()[bq].ops.len());
                let (build, used) = redo_n(px, bq, jb, node, sys, (false, &fetch[nf..]), ex);
                nf += used;
                let Some(tid) = typefacts::facts_of(gx.facts, TIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the tid type the catalog does not know" }) };
                let qs = clauses.iter().chain(qual.iter()).map(|q| ex.0.compile(px, q, None, n)).collect_in(m);
                let natts = gx.facts.rel(relid).map_or(0, |r| r.natts.max(0) as u16);
                let site = px.add_late_site(SiteDesc::write(relid, natts));
                let left = *jt == JoinType::JOIN_LEFT;
                redo.push_in(Redo::Ref(mcx::box_in(Fetch { site, relid, col, tid, atts: atts_of(gx, relid), build, node: n, quals: qs, params: PgVec::new_in(m), held: false, mark: 0, left, copy: None }, m)));
            }
            // producerchain D: a write's INNER hash probe in a nest chain (nest_outer.rs), over the record it handed on;
            // modifytable rowc-copy: a data-modifying CTE's SEMI one too (rowop_epq.rs `probed`)
            // D3747: or a LEFT one (rowop_epq.rs `probed`), over its record or its NULL-extended row
            Op::Join(JoinDesc::Probe { jointype: jt @ (JoinType::JOIN_INNER | JoinType::JOIN_SEMI | JoinType::JOIN_LEFT), keys, decls, clauses, qual, .. }) if subs && fetch.is_empty() => {
                if crate::ops::hashjoin::keys::confirms(gx, clauses) {
                    crate::refuse!(node, Reason::Shape { why: CONFIRMS });
                }
                nested = true;
                let n = crate::rerun::inner::op_node(p, i);
                let ks = keys.iter().zip(decls.iter()).map(|(x, d)| crate::ops::hashjoin::keys::key_src(px, x, n, d)).collect_in(m);
                let qs = qual.iter().map(|q| ex.0.compile(px, q, None, n)).collect_in(m);
                redo.push_in(Redo::Probe { node: n, keys: ks, quals: qs, left: *jt == JoinType::JOIN_LEFT });
            }
            // D4664: a data-modifying CTE's write over its own scan merge-joined to a REFERENCE-marked relation's own scan
            // (rowop_epq.rs `own_merge`): the newest version against the inner row the merge handed on, through the merge
            // clauses and the join qual, as EvalPlanQual reruns the merge over that row refetched (the version the join read)
            Op::Join(JoinDesc::Merge { clauses, qual, .. }) if subs && fetch.is_empty() && crate::ops::modifytable::rowop_epq::own_merge(gx, k, i) => {
                nested = true;
                let n = crate::rerun::inner::op_node(p, i);
                let qs = clauses.iter().chain(qual.iter()).map(|q| ex.0.compile(px, q, None, n)).collect_in(m);
                redo.push_in(Redo::Nest { node: n, quals: qs, left: false });
            }
            // D3561 / D3562 REFANTI: an ANTI join (a nest or a hash probe) whose inner the form's check says reads a
            // REFERENCE-marked relation's own scan alone (ref_anti.rs `inner`): no inner opened, its columns NULL
            Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_ANTI, .. } | JoinDesc::Probe { jointype: JoinType::JOIN_ANTI, .. }) if !subs && fetch.is_empty() => {
                let Some(cols) = crate::rerun::epq::ref_anti::cols(gx, k, i) else { crate::refuse!(node, Reason::Shape { why: sys }) };
                nested = true;
                redo.push_in(Redo::Anti(cols));
            }
            _ => {} // a gate (a constant the row passed), an arm's marker
        }
    }
    (redo, nf)
}

/// D655 Row C (V4R-534): the write row-op's recheck is this one (face/end.rs `Arm::Epq`).
impl crate::face::end::EpqRerun for Epq<'_> {
    fn rerun<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, site: usize, node: i32, retry: Next<'_, 'e>) -> Carry<'e> {
        self.lower(b, env, row, site, node, retry)
    }
}

impl<'g> Epq<'g> {
    /// D3379 INITSINK (D3436): the lock entry whose host fetches each REFERENCE row of this recheck (`Fetch::mark`);
    /// D3657: REFBUILD's probe-side row too (`Redo::Outer`).
    pub(crate) fn fetching_by(mut self, mark: usize) -> Self {
        for r in self.redo.iter_mut() {
            if let Redo::Ref(f) | Redo::Outer(f) | Redo::Source(f) = r {
                f.mark = mark;
            }
        }
        self
    }

    /// D3271: this recheck forces `force` at its start (a lock's, ops/lockrows/epq.rs).
    pub(crate) fn forcing(mut self, force: PgVec<'g, (i32, u16)>) -> Self {
        self.force = force;
        self
    }

    /// D3272: this recheck arms `fresh` for its redo (the subs form's, ops/lockrows/epq.rs).
    pub(crate) fn arming(mut self, fresh: PgVec<'g, (u16, i32, u32)>) -> Self {
        self.fresh = fresh;
        self
    }

    /// D3274: `fresh` is armed in EvalPlanQual state `key` (nonzero: exec initplan raw.rs `keyed`).
    pub(crate) fn keyed(mut self, key: u32) -> Self {
        self.key = key;
        self
    }

    /// D3506 SETLOCK: the redo's hashed probes read the sets of its own (`twin`).
    pub(crate) fn twinning(mut self, twin: bool) -> Self {
        self.twin = twin;
        self
    }

    /// D3506 SETLOCK: the redo's probes made for it (`own`).
    pub(crate) fn owning(mut self, own: PgVec<'g, (i32, PgBox<'g, dyn crate::face::step::StepMech + 'g>)>) -> Self {
        self.own = own;
        self
    }

    /// D3274: whether this recheck's state ends at its LockRows' input end (`epq_end`): a keyed arming.
    pub(crate) fn ends(&self) -> bool {
        self.key != 0 && !self.fresh.is_empty()
    }

    /// D3274: PostgreSQL's EvalPlanQualEnd at the LockRows' input end (ExecLockRows): the keyed state of each of `fresh`
    /// ends, so the next recheck's estate arms it afresh.
    pub(crate) fn epq_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        if self.ends() {
            self.epq_call(b, env, gravity_kernels::ops::shared::object::entries::INIT_EPQ_END, false);
        }
    }

    /// D3272: each of `fresh` armed (`arm`) or put back (`m8_init_epq`), its parameters' frame pairs cleared, so the next
    /// read asks the cell (rerun/params.rs `word`): the redo's runs the members again, the statement's after it reads its
    /// own value. A put back with no arming kept does nothing.
    fn epq_inits(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, arm: bool) {
        self.epq_call(b, env, u64::from(arm), true);
    }

    /// `m8_init_epq` with operation `op` in state `key` over each of `fresh`; `clear`: its parameters' frame pairs cleared.
    fn epq_call(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, op: u64, clear: bool) {
        use gravity_kernels::ops::shared::object::entries as m8;
        for &(h, first, n) in self.fresh.iter() {
            let hv = b.load(Type::I64, env.params, i32::from(h) * 8);
            let a = b.iconst(Type::I64, (op | u64::from(self.key) << m8::INIT_EPQ_KEY_SHIFT) as i64);
            let f = crate::ops::subplan::hashed::host(b, "m8_init_epq", &[Type::Ptr, Type::I64, Type::I64], gravity_abi::host_words!(m8::m8_init_epq(ctx, h, arm)));
            let r = b.call_host(f, &[env.ctx, hv, a], Some(Type::I64)).unwrap_or(env.zero);
            let bad = b.icmp(IntCond::Slt, r, env.zero);
            crate::ops::subplan::hashed::raise_if(b, env, bad);
            if clear {
                for id in (first..).take(n as usize) {
                    env.store_word(b, crate::rerun::params::word(id, 1), env.zero);
                }
            }
        }
    }

    /// The recheck at lock site `site`: the carry after the newer version went on, or the row's own when it failed.
    pub(crate) fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, site: usize, node: i32, next: Next<'_, 'e>) -> Carry<'e> {
        // D3271: a pending InitPlan the recheck's plan reads runs first, once, in the statement's own run (EvalPlanQualStart's
        // ExecSetParamPlanMulti); its error raises here, before the recheck reads the newer version
        for &(id, h) in self.force.iter() {
            crate::frame::site::init_key(b, env, id, h);
        }
        // D3272: an InitPlan inside a SubPlan step's plan runs again where the redo first reads it (EvalPlanQualStart's
        // fresh SubPlan), put back before the row goes on (`run_on`) or leaves failed (`l_out`)
        self.epq_inits(b, env, true);
        if let Some((tableoid, leaves)) = &self.union {
            return Self::lower_union(b, env, row, (site, node), (tableoid, leaves), next);
        }
        // D3802 SORTCHAIN: a redo from the source's refetched row (`Redo::Source`) reads the lock site at the locked inner
        let r = match self.redo.first() {
            Some(Redo::Source(_)) => RowVals { row: Row::new_in(env.mcx), vals: mcx::PgVec::new_in(env.mcx), cols: mcx::PgVec::new_in(env.mcx), carry: row.carry.copy_in(env.mcx), next: row.next, at: row.at },
            _ => site_row(b, env, site, &self.atts, row),
        };
        let types = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let l_out = b.create_block(&types[..]);
        let inner = env.inner.take();
        env.memo.clear();
        self.run_on(b, env, (row, r), &self.redo, node, (l_out, &inner), next);
        env.inner = inner;
        env.memo.clear();
        b.switch_to(l_out);
        self.epq_inits(b, env, false);
        Carry::from_params(b.block_params(l_out), env.mcx)
    }

    /// ROWC-UNION (`epq_of_union`): the recheck of the row's own relation, by its junk tableoid; a row of none of them (no
    /// union row is) leaves with its carry, as a skipped row.
    fn lower_union<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, (site, node): (usize, i32), ((col, of), leaves): (&(i16, TypeFacts), &[(u32, Epq<'_>)]), next: Next<'_, 'e>) -> Carry<'e> {
        let Some(cr) = row.col(*col) else { crate::refuse!(node, Reason::Bug { payload: "a union-fed write's tableoid past the row the recheck took" }) };
        let mut cx = env.lx(b, node);
        let (rel, _) = crate::rep::datum(&mut cx, cr, of);
        env.memo = cx.memo;
        let types = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let l_out = b.create_block(&types[..]);
        for (relid, e) in leaves.iter() {
            let id = b.iconst(Type::I64, i64::from(*relid));
            let mine = b.icmp(IntCond::Eq, rel, id);
            let (l_mine, l_next) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(mine, (l_mine, &[]), (l_next, &[]));
            b.switch_to(l_mine);
            let c = e.lower(b, env, row, site, node, &mut *next);
            b.br(l_out, &c.params());
            b.switch_to(l_next);
        }
        b.br(l_out, &row.carry.params());
        b.switch_to(l_out);
        Carry::from_params(b.block_params(l_out), env.mcx)
    }

    /// `redo` over row `r`, then `next` over the row it ends with (`env.inner` the step's own, `inner`), into `l_out` (a
    /// failing qual leaves there with the step's row's carry). producerchain C: a LEFT nest's join quals over a matched
    /// inner row branch: a pass goes on joined to it, a failure NULL-extended (ExecNestLoop's unmatched outer row, over
    /// EvalPlanQual's test tuple). D3818: both enter one block (`left_merge`), so the rest of the redo and `next` lower
    /// once past each LEFT join, and a row a sharing join held may be either (`Filled::At`: its run-time tag goes to the
    /// NULL-extended side before any key or qual runs, as a static NULL-extended row runs none).
    #[allow(clippy::too_many_arguments)]
    fn run_on<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, (row, r): (&RowVals, RowVals), redo: &[Redo<'_>], node: i32, (l_out, inner): (Block, &Option<PgVec<'e, Option<u16>>>), next: Next<'_, 'e>) {
        crate::stack_check();
        // the steps before the first branching LEFT nest run as one `run` (a redo with none emits what it did before C)
        let matched = |n: i32| env.redo_nests.iter().rev().find(|x| x.0 == n).is_some_and(|x| !matches!(x.1.3, Filled::Yes));
        let branches = |d: &Redo| match d {
            Redo::Nest { node: n, quals, left: true } => !quals.is_empty() && matched(*n),
            Redo::Probe { node: n, left: true, .. } => matched(*n),
            Redo::Ref(f) => f.left,
            _ => false,
        };
        let at = redo.iter().position(branches).unwrap_or(redo.len());
        let mcx = env.mcx;
        let r = self.run(b, env, mcx, row, r, &redo[..at], node, l_out);
        // D3560 REFNEST-LEFT: the refetched inner row joined when it is there and passes its redo and the join qual, else
        // the row null-extended, into the rest of the redo. D3818: the NULL-extended row takes the refetched row's form
        // (`left_merge`, every column NULL), so no held row shapes it: D3569's COPY row and D3596's join below a Sort,
        // whose rows are out of reach, need none
        if let Some(Redo::Ref(f)) = redo.get(at) {
            let types = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
            let l_null = b.create_block(&types[..]);
            let ri = self.fetch(b, env, mcx, row, f, node, l_null);
            env.inner = None;
            env.memo.clear();
            let saved = crate::rerun::epq::ref_nest::set(b, env, f.node, &f.params, &r, self.ex);
            let l_fail = crate::rerun::epq::ref_nest::fail_to(b, env, &saved, l_null, &types[..]);
            let ri = self.run(b, env, mcx, row, ri, &f.build, node, l_fail);
            crate::rerun::epq::ref_nest::restore(b, env, &saved);
            env.memo.clear();
            let (row2, map) = crate::ops::nestloop::lower::joined(&r, (&ri.row.cells, &ri.vals, &ri.cols), &row.carry, env.mcx);
            env.inner = Some(map);
            for &q in f.quals.iter() {
                let (pass, l_on) = (self.ex.0.pred(b, env, f.node, &row2, q), b.create_block(&[]));
                b.cond_br(pass, (l_on, &[]), (l_null, &row.carry.params()));
                b.switch_to(l_on);
            }
            let (mc, mv) = left_merge(b, env, (&ri.row.cells, &ri.vals), l_null);
            let (row3, map) = crate::ops::nestloop::lower::joined(&r, (&mc, &mv, &ri.cols), &row.carry, env.mcx);
            env.inner = Some(map);
            return self.run_on(b, env, (row, row3), &redo[at + 1..], node, (l_out, inner), next);
        }
        // D3747: a LEFT probe's matched record: the newest version's key image against the record's and the join qual
        // over it; a NULL key or a failure NULL-extends the row
        if let Some(Redo::Probe { node: n, keys, quals, .. }) = redo.get(at) {
            let (cells, vals, cols, fill) = held(env, *n, node);
            let Some(img0) = env.redo_probes.iter().rev().find(|x| x.0 == *n).map(|x| x.1.to_vec()) else { crate::refuse!(node, Reason::Shape { why: PROBE_ROW }) };
            let l_null = b.create_block(&[]);
            missed(b, fill, l_null);
            let kc = crate::ops::hashjoin::keys::key_cells(b, env, *n, &r, keys, None);
            if let Some(miss) = crate::ops::hashjoin::keys::any_null(b, &kc) {
                let l_on = b.create_block(&[]);
                b.cond_br(miss, (l_null, &[]), (l_on, &[]));
                b.switch_to(l_on);
            }
            let mut lx = env.lx(b, *n);
            let img = crate::ops::hashjoin::keys::image(&mut lx, *n, &kc, keys);
            env.memo = lx.memo;
            if img.words.words.len() != img0.len() {
                crate::refuse!(node, Reason::Bug { payload: "a probe redo's image wider than its record's" });
            }
            let eq = gravity_expr::emit::hash::emit_image_eq(b, &img.words, &gravity_expr::emit::hash::ImageValues { words: img0 });
            let l_on = b.create_block(&[]);
            b.cond_br(eq, (l_on, &[]), (l_null, &[]));
            b.switch_to(l_on);
            let (row2, map) = crate::ops::nestloop::lower::joined(&r, (&cells, &vals, &cols), &row.carry, env.mcx);
            env.inner = Some(map);
            for &q in quals.iter() {
                let (pass, l_on) = (self.ex.0.pred(b, env, *n, &row2, q), b.create_block(&[]));
                b.cond_br(pass, (l_on, &[]), (l_null, &[]));
                b.switch_to(l_on);
            }
            let (mc, mv) = left_merge(b, env, (&cells, &vals), l_null);
            let (row3, map) = crate::ops::nestloop::lower::joined(&r, (&mc, &mv, &cols), &row.carry, env.mcx);
            env.inner = Some(map);
            return self.run_on(b, env, (row, row3), &redo[at + 1..], node, (l_out, inner), next);
        }
        if let Some(Redo::Nest { node: n, quals, .. }) = redo.get(at) {
            let (cells, vals, cols, fill) = held(env, *n, node);
            let l_null = b.create_block(&[]);
            missed(b, fill, l_null);
            let (row2, map) = crate::ops::nestloop::lower::joined(&r, (&cells, &vals, &cols), &row.carry, env.mcx);
            env.inner = Some(map);
            for &q in quals.iter() {
                let (pass, l_on) = (self.ex.0.pred(b, env, *n, &row2, q), b.create_block(&[]));
                b.cond_br(pass, (l_on, &[]), (l_null, &[]));
                b.switch_to(l_on);
            }
            let (mc, mv) = left_merge(b, env, (&cells, &vals), l_null);
            let (row3, map) = crate::ops::nestloop::lower::joined(&r, (&mc, &mv, &cols), &row.carry, env.mcx);
            env.inner = Some(map);
            return self.run_on(b, env, (row, row3), &redo[at + 1..], node, (l_out, inner), next);
        }
        env.inner = inner.as_deref().map(|v| mcx::to_vec_in(v, mcx));
        env.memo.clear();
        self.epq_inits(b, env, false);
        let c = next(b, env, &r);
        env.memo.clear();
        b.br(l_out, &c.params());
    }

    /// `redo` over row `r` (a failing qual leaves to `l_out` with the step's row's carry); the row it ends with. D3506
    /// SETLOCK: under `twin` its hashed probes read the sets of its own (`Env::epq_twin`), the steps after it the
    /// statement's.
    #[allow(clippy::too_many_arguments)]
    fn run<'x>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, mcx: Mcx<'x>, row: &RowVals, r: RowVals<'x>, redo: &[Redo<'_>], node: i32, l_out: Block) -> RowVals<'x> {
        let was = env.epq_twin;
        env.epq_twin = was || self.twin;
        let out = self.run_redo(b, env, mcx, row, r, redo, node, l_out);
        env.epq_twin = was;
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn run_redo<'x>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, mcx: Mcx<'x>, row: &RowVals, mut r: RowVals<'x>, redo: &[Redo<'_>], node: i32, l_out: Block) -> RowVals<'x> {
        let e = self;
        let t = b.bconst(true);
        for (i, d) in redo.iter().enumerate() {
            match d {
                Redo::Qual(q) => {
                    let (pass, l_on) = (e.ex.0.pred(b, env, node, &r, *q), b.create_block(&[]));
                    b.cond_br(pass, (l_on, &[]), (l_out, &row.carry.params()));
                    b.switch_to(l_on);
                }
                Redo::Tid(checks, all) => {
                    let Some(ci) = r.cols.get(CTID_AT as usize - 1).copied().flatten() else { crate::refuse!(node, Reason::Shape { why: TID }) };
                    let at = tid_key(b, r.vals[ci as usize].0);
                    let mut pass = b.bconst(*all);
                    for c in checks.iter() {
                        let one = match c {
                            TidCheck::Pred(q) => e.ex.0.pred(b, env, node, &r, *q),
                            TidCheck::Never => b.bconst(false),
                            TidCheck::Cmp { word, cond } => tid_cmp(b, env, at, *word, *cond),
                        };
                        pass = b.int_bin(if *all { IntBinOp::And } else { IntBinOp::Or }, pass, one);
                    }
                    let l_on = b.create_block(&[]);
                    b.cond_br(pass, (l_on, &[]), (l_out, &row.carry.params()));
                    b.switch_to(l_on);
                }
                Redo::Sub(id, only) => {
                    if !crate::ops::subplan::site::redo(b, env, *id, &r) {
                        // D3451 LAZYSUB: a lazy step: the rest of the redo reads it over the row so far
                        let rest = &redo[i + 1..];
                        let copy = |x: &RowVals| RowVals { row: Row { cells: mcx::to_vec_in(&x.row.cells[..], mcx) }, vals: mcx::to_vec_in(&x.vals[..], mcx), cols: mcx::to_vec_in(&x.cols[..], mcx), carry: x.carry.copy_in(mcx), next: x.next, at: x.at };
                        let out = only.then(|| crate::ops::subplan::site::relend(b, env, *id, &r, &mut |b, env| e.run(b, env, mcx, row, copy(&r), rest, node, l_out))).flatten();
                        // D3506 SETLOCK: or a probe of a claim's Sort producer made for this redo (`own`), lent over
                        // the row so far as its step lends it; EXISTS SORT: or such a step the claim lends its redo
                        // (rerun/epq/sub_lock.rs `SortClaim`)
                        let out = out.or_else(|| {
                            let own: Option<&dyn crate::face::step::StepMech> = e.own.iter().find(|s| s.0 == *id).map(|s| &*s.1 as &dyn crate::face::step::StepMech);
                            let s = own.or_else(|| crate::rerun::epq::sub_lock::redo_own(env, *id)).filter(|_| *only)?;
                            let (mut got, zero) = (None, env.zero);
                            s.lower(b, env, &r, &mut |b, env, x| {
                                got = Some(e.run(b, env, mcx, row, copy(x), rest, node, l_out));
                                Carry::new(zero, env.mcx)
                            });
                            got
                        });
                        return out.unwrap_or_else(|| crate::refuse!(node, Reason::Shape { why: LAZY_SUB }));
                    }
                }
                // D2739 NEST-INNER: the inner row so far joined to the outer row the nest held, then its join quals
                Redo::InnerOf { node: n, quals } => {
                    let outer = match env.redo_outers.iter().rev().find(|x| x.0 == *n) {
                        Some((_, (cells, vals, cols, _))) => RowVals { row: Row { cells: mcx::to_vec_in(&cells[..], mcx) }, vals: mcx::to_vec_in(&vals[..], mcx), cols: mcx::to_vec_in(&cols[..], mcx), carry: row.carry.copy_in(mcx), next: row.next, at: row.at },
                        None => crate::refuse!(node, Reason::Shape { why: NEST_ROW }),
                    };
                    let (row2, map) = crate::ops::nestloop::lower::joined(&outer, (&r.row.cells, &r.vals, &r.cols), &row.carry, env.mcx);
                    env.inner = Some(map);
                    for &q in quals.iter() {
                        let (pass, l_on) = (e.ex.0.pred(b, env, *n, &row2, q), b.create_block(&[]));
                        b.cond_br(pass, (l_on, &[]), (l_out, &row.carry.params()));
                        b.switch_to(l_on);
                    }
                    r = row2;
                }
                // an INNER nest's; a LEFT nest's NULL-extended row, or a matched one with no join qual (`run_on` branches
                // on the others): its row joined as it came, the NULL-extended one with no qual run (no test tuple)
                Redo::Nest { node: n, quals, left } => {
                    let (cells, vals, cols, fill) = held(env, *n, node);
                    let null = matches!(fill, Filled::Yes);
                    let (row2, map) = crate::ops::nestloop::lower::joined(&r, (&cells, &vals, &cols), &row.carry, env.mcx);
                    env.inner = Some(map);
                    if *left && !(null || quals.is_empty()) {
                        crate::refuse!(node, Reason::Bug { payload: "a LEFT nest's redo with join quals over a matched row outside run_on" });
                    }
                    for &q in quals.iter().filter(|_| !null) {
                        let (pass, l_on) = (e.ex.0.pred(b, env, *n, &row2, q), b.create_block(&[]));
                        b.cond_br(pass, (l_on, &[]), (l_out, &row.carry.params()));
                        b.switch_to(l_on);
                    }
                    r = row2;
                }
                // the newest version's outer key image against the matched record's (a NULL key: no match), then the
                // record's payload row joined as it came and the join qual over it
                // D3747: a LEFT probe's NULL-extended record (a matched one branches in `run_on`): joined as it came
                Redo::Probe { node: n, left: true, .. } => {
                    let (cells, vals, cols, fill) = held(env, *n, node);
                    if !matches!(fill, Filled::Yes) {
                        crate::refuse!(node, Reason::Bug { payload: "a LEFT probe's redo over a matched record outside run_on" });
                    }
                    let (row2, map) = crate::ops::nestloop::lower::joined(&r, (&cells, &vals, &cols), &row.carry, env.mcx);
                    env.inner = Some(map);
                    r = row2;
                }
                Redo::Probe { node: n, keys, quals, .. } => {
                    let (cells, vals, cols, _) = held(env, *n, node);
                    let Some(img0) = env.redo_probes.iter().rev().find(|x| x.0 == *n).map(|x| x.1.to_vec()) else { crate::refuse!(node, Reason::Shape { why: PROBE_ROW }) };
                    let carry = row.carry.params();
                    let kc = crate::ops::hashjoin::keys::key_cells(b, env, *n, &r, keys, Some((l_out, &carry)));
                    if let Some(miss) = crate::ops::hashjoin::keys::any_null(b, &kc) {
                        let l_on = b.create_block(&[]);
                        b.cond_br(miss, (l_out, &carry), (l_on, &[]));
                        b.switch_to(l_on);
                    }
                    let mut lx = env.lx(b, *n);
                    let img = crate::ops::hashjoin::keys::image(&mut lx, *n, &kc, keys);
                    env.memo = lx.memo;
                    if img.words.words.len() != img0.len() {
                        crate::refuse!(node, Reason::Bug { payload: "a probe redo's image wider than its record's" });
                    }
                    let eq = gravity_expr::emit::hash::emit_image_eq(b, &img.words, &gravity_expr::emit::hash::ImageValues { words: img0 });
                    let l_on = b.create_block(&[]);
                    b.cond_br(eq, (l_on, &[]), (l_out, &carry));
                    b.switch_to(l_on);
                    let (row2, map) = crate::ops::nestloop::lower::joined(&r, (&cells, &vals, &cols), &row.carry, env.mcx);
                    env.inner = Some(map);
                    for &q in quals.iter() {
                        let (pass, l_on) = (e.ex.0.pred(b, env, *n, &row2, q), b.create_block(&[]));
                        b.cond_br(pass, (l_on, &[]), (l_out, &carry));
                        b.switch_to(l_on);
                    }
                    r = row2;
                }
                Redo::Ref(f) => {
                    if f.left {
                        crate::refuse!(node, Reason::Bug { payload: "a LEFT nest's REFNEST redo outside run_on" });
                    }
                    let ri = e.fetch(b, env, mcx, row, f, node, l_out);
                    env.inner = None;
                    env.memo.clear();
                    // lockrows REFNEST: the nest's parameters from the row so far while the inner's redo runs, restored after
                    let saved = crate::rerun::epq::ref_nest::set(b, env, f.node, &f.params, &r, e.ex);
                    let types = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
                    let l_fail = crate::rerun::epq::ref_nest::fail_to(b, env, &saved, l_out, &types[..]);
                    let ri = e.run(b, env, mcx, row, ri, &f.build, node, l_fail);
                    crate::rerun::epq::ref_nest::restore(b, env, &saved);
                    env.memo.clear();
                    let (row2, map) = crate::ops::nestloop::lower::joined(&r, (&ri.row.cells, &ri.vals, &ri.cols), &row.carry, env.mcx);
                    env.inner = Some(map);
                    for &q in f.quals.iter() {
                        let (pass, l_on) = (e.ex.0.pred(b, env, f.node, &row2, q), b.create_block(&[]));
                        b.cond_br(pass, (l_on, &[]), (l_out, &row.carry.params()));
                        b.switch_to(l_on);
                    }
                    r = row2;
                }
                // D3802 SORTCHAIN: the producer's source row, refetched at its junk ctid (a NULL one: no row)
                Redo::Source(f) => {
                    r = e.fetch(b, env, mcx, row, f, node, l_out);
                    env.inner = None;
                    env.memo.clear();
                }
                Redo::Outer(f) => {
                    let ro = e.fetch(b, env, mcx, row, f, node, l_out);
                    env.inner = None;
                    env.memo.clear();
                    let ro = e.run(b, env, mcx, row, ro, &f.build, node, l_out);
                    env.memo.clear();
                    let (row2, map) = crate::ops::nestloop::lower::joined(&ro, (&r.row.cells, &r.vals, &r.cols), &row.carry, env.mcx);
                    env.inner = Some(map);
                    for &q in f.quals.iter() {
                        let (pass, l_on) = (e.ex.0.pred(b, env, f.node, &row2, q), b.create_block(&[]));
                        b.cond_br(pass, (l_on, &[]), (l_out, &row.carry.params()));
                        b.switch_to(l_on);
                    }
                    r = row2;
                }
                // D3561 / D3562 REFANTI: the row so far joined to the inner's columns NULL (no test tuple: the inner empty)
                Redo::Anti(cols) => {
                    let (c2, mut v2, k2) = null_cols(env, mcx, cols);
                    v2.iter_mut().for_each(|v| v.1 = Some(t));
                    let (row2, map) = crate::ops::nestloop::lower::joined(&r, (&c2, &v2, &k2), &row.carry, env.mcx);
                    env.inner = Some(map);
                    r = row2;
                }
                // D3565 REFDRAIN: the row so far as the drain's inner, the outer columns NULL (no test tuple: the outer empty)
                Redo::Drain(cols) => {
                    let (c2, mut v2, k2) = null_cols(env, mcx, cols);
                    v2.iter_mut().for_each(|v| v.1 = Some(t));
                    let outer = RowVals { row: Row { cells: c2 }, vals: v2, cols: k2, carry: row.carry.copy_in(mcx), next: row.next, at: row.at };
                    let (row2, map) = crate::ops::nestloop::lower::joined(&outer, (&r.row.cells, &r.vals, &r.cols), &row.carry, env.mcx);
                    env.inner = Some(map);
                    r = row2;
                }
                Redo::Project(list) => {
                    let width = list.iter().map(|x| x.0 + 1).max().unwrap_or(0);
                    let (mut c2, mut v2, mut k2) = (mcx::PgVec::new_in(mcx), mcx::PgVec::new_in(mcx), mcx::pg_vec![in mcx; None; width]);
                    for &(i, x, out) in list.iter() {
                        let (v, valid) = e.ex.0.emit(b, env, node, &r, x);
                        k2[i] = Some(c2.len() as u16);
                        c2.push_in(Cell::of_expr(out, c2.len() as u16, c2.len() as u16, Nullable::Maybe));
                        v2.push_in((v, Some(b.int_bin(IntBinOp::Xor, valid, t))));
                    }
                    r = RowVals { row: Row { cells: c2 }, vals: v2, cols: k2, carry: row.carry.copy_in(mcx), next: row.next, at: row.at };
                    env.inner = None; // a projection collapses a joined row (`Env::inner`)
                    env.memo.clear();
                }
            }
        }
        r
    }

    /// lockrows REFFETCH (`Fetch`): the REFERENCE row at the step's row's junk ctid, fetched by the lock host into the fetch
    /// site (`LockSite::FETCH`), as that site's row; a NULL ctid (an outer join's: none under an INNER probe) is no row
    /// (EvalPlanQualFetchRowMark's false), so no joined row (`l_out`); the host's error leaves with the rows before it.
    fn fetch<'x>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, mcx: Mcx<'x>, row: &RowVals, f: &Fetch<'_>, node: i32, l_out: Block) -> RowVals<'x> {
        if f.held {
            return site_row_in(b, env, mcx, f.site.get(), &f.atts, row);
        }
        let Some(cr) = row.col(f.col) else { crate::refuse!(node, Reason::Bug { payload: REF_COL }) };
        let mut cx = env.lx(b, node);
        let (d, null) = crate::rep::datum(&mut cx, cr, f.copy.as_ref().map_or(&f.tid, |c| &c.rec));
        env.memo = cx.memo;
        let l_some = b.create_block(&[]);
        b.cond_br(null, (l_out, &row.carry.params()), (l_some, &[]));
        b.switch_to(l_some);
        // D3569 REFCOPY: the stored record's fields as the Subquery Scan's input row (a NULL record: no row, above)
        if let Some(c) = &f.copy {
            return crate::rerun::epq::copy_row::image_row(b, env, mcx, d, (&c.fields, (0, None)), (row.carry.copy_in(mcx), row.next, row.at));
        }
        // the tid (an ItemPointerData image: bi_hi, bi_lo, ip_posid) as the site's 12-byte position (write/lockrows.rs's form)
        let p = b.conv(ConvOp::IntToPtr, Type::Ptr, d);
        let (hi, lo, off) = (b.load_zext(Type::I16, p, 0), b.load_zext(Type::I16, p, 2), b.load_zext(Type::I16, p, 4));
        let s = site_off(f.site.get()) as i32;
        let (s64, s32) = (b.iconst(Type::I64, 16), b.iconst(Type::I32, 16));
        let hi = b.int_bin(IntBinOp::Shl, hi, s64);
        let block = b.int_bin(IntBinOp::Or, hi, lo);
        let off = b.conv(ConvOp::Trunc, Type::I32, off);
        let off = b.int_bin(IntBinOp::Shl, off, s32);
        let flags = b.iconst(Type::I32, (POS_VALID | POS_ROW) as i64);
        let w0 = b.int_bin(IntBinOp::Or, off, flags);
        b.store(env.ctx, s + site::POS_FLAGS as i32, w0);
        let rel = b.iconst(Type::I32, f.relid as i32 as i64);
        b.store(env.ctx, s + site::POS_REL as i32, rel);
        let blk = b.conv(ConvOp::Trunc, Type::I32, block);
        b.store(env.ctx, s + site::POS_BLOCK as i32, blk);
        let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
        let lock_row = gravity_kernels::ops::lockrows::entries::lock_row as *const () as usize;
        let h = b.declare_host_once("lock_row", &[Type::Ptr, Type::I64], Some(Type::I64), lock_row, mem);
        let at = f.mark.checked_sub(1).map_or(f.site.get() as u64, |i| LockSite::arg(f.site.get(), i));
        let arg = b.iconst(Type::I64, (LockSite::FETCH | at) as i64);
        let st = b.call_host(h, &[env.ctx, arg], Some(Type::I64)).unwrap_or(env.zero);
        let (l_bad, l_ok) = (b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_bad);
        let neg = b.icmp(IntCond::Slt, st, env.zero);
        b.cond_br(neg, (l_bad, &[]), (l_ok, &[]));
        b.switch_to(l_bad);
        b.br(env.x.l_stop_in, &[row.carry.out, row.at, st]);
        b.switch_to(l_ok);
        site_row_in(b, env, mcx, f.site.get(), &f.atts, row)
    }
}

/// The inner row nest `n` handed on (nest.loop's `Env::redo_nests`, the innermost of that node's), else D1933 (c)'s refusal.
/// D3561 / D3569: an inner side's columns `outs` (each one's column and form) as cells, their values 0 (NULL once
/// `nestloop::lower::null_row` or the caller flags them): a row the redo cannot reach, shaped by its projection.
fn null_cols<'x>(env: &Env, mcx: Mcx<'x>, outs: &[(usize, ExprOut)]) -> (PgVec<'x, Cell>, PgVec<'x, (Value, Option<Value>)>, PgVec<'x, Option<u16>>) {
    let width = outs.iter().map(|x| x.0 + 1).max().unwrap_or(0);
    let (mut cells, mut vals, mut cols) = (PgVec::new_in(mcx), PgVec::new_in(mcx), mcx::pg_vec![in mcx; None; width]);
    for &(i, out) in outs.iter() {
        cols[i] = Some(cells.len() as u16);
        cells.push_in(Cell::of_expr(out, cells.len() as u16, cells.len() as u16, Nullable::Maybe));
        vals.push_in((env.zero, None));
    }
    (cells, vals, cols)
}

fn held<'e>(env: &Env<'e, '_>, n: i32, node: i32) -> crate::frame::env::NestRow<'e> {
    match env.redo_nests.iter().rev().find(|x| x.0 == n) {
        Some(x) => (mcx::to_vec_in(&x.1.0[..], env.mcx), mcx::to_vec_in(&x.1.1[..], env.mcx), mcx::to_vec_in(&x.1.2[..], env.mcx), x.1.3),
        None => crate::refuse!(node, Reason::Shape { why: NEST_ROW }),
    }
}

/// D3818: a held row a sharing LEFT join handed on (`Filled::At`) goes to `l_null` when its run-time tag says it was the
/// NULL-extended row: no inner row matched, so EvalPlanQual has no test tuple and runs no key or join qual for it.
fn missed(b: &mut FunctionBuilder<'_>, fill: Filled, l_null: Block) {
    if let Filled::At(tag) = fill {
        let l_go = b.create_block(&[]);
        b.cond_br(tag, (l_null, &[]), (l_go, &[]));
        b.switch_to(l_go);
    }
}

/// D3818: a LEFT join's two outcomes in the redo enter one block: the current block (the join passed) with the inner
/// row `cells` / `vals`, and `l_null` (its parameters unused) with that row's every value NULL, as nest.loop's sharing
/// form does (`lower_shared`). The rest of the redo and `next` then lower once past each LEFT join, where a copy per
/// outcome made 2^n of them. The merged inner row: the joined form's cells, each NULL-able, valued by the block.
fn left_merge<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, (cells, vals): (&[Cell], &[(Value, Option<Value>)]), l_null: Block) -> (PgVec<'e, Cell>, PgVec<'e, (Value, Option<Value>)>) {
    let m = env.mcx;
    let types = vals.iter().map(|&(v, _)| b.value_type(v)).collect_in(m);
    let n = types.len();
    let mut ktypes = mcx::to_vec_in(&types[..], m);
    ktypes.extend_in(core::iter::repeat_n(Type::Bool, n));
    let l_k = b.create_block(&ktypes[..]);
    let f = b.bconst(false);
    let args = vals.iter().map(|v| v.0).chain(vals.iter().map(|v| v.1.unwrap_or(f))).collect_in(m);
    b.br(l_k, &args[..]);
    b.switch_to(l_null);
    // a lane cell's value is its element's address, which a reader loads before it tests NULL: a zeroed pair's
    let zero = env.zero;
    let blank = cells.iter().any(|c| matches!(c.at, At::Lane(_))).then(|| {
        let slot = b.declare_stack_slot(16, 8);
        let a = b.stack_addr(slot);
        b.store(a, 0, zero);
        b.store(a, 8, zero);
        a
    });
    let t = b.bconst(true);
    let mut nargs = mcx::reserved_in::<Value>(2 * n, m);
    for (c, &ty) in cells.iter().zip(types.iter()) {
        let v = match (c.at, blank) {
            (At::Lane(_), Some(a)) if ty == Type::Ptr => a,
            (At::Lane(_), Some(a)) => b.conv(ConvOp::PtrToInt, ty, a),
            _ => b.const_raw(ty, 0),
        };
        nargs.push_in(v);
    }
    nargs.extend_in(core::iter::repeat_n(t, n));
    b.br(l_k, &nargs[..]);
    b.switch_to(l_k);
    env.memo.clear();
    let ps = mcx::to_vec_in(b.block_params(l_k), m);
    let cells = cells.iter().map(|c| Cell { null: Nullable::Maybe, ..*c }).collect_in(m);
    let vals = (0..n).map(|i| (ps[i], Some(ps[n + i]))).collect_in(m);
    (cells, vals)
}

/// Site `site`'s row: its cells by attno (`atts`), then its position as ctid (frame/site.rs's CTID_AT), with `row`'s carry.
pub(crate) fn site_row<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, site: usize, atts: &[Option<ExprOut>], row: &RowVals) -> RowVals<'e> {
    let mcx = env.mcx;
    site_row_in(b, env, mcx, site, atts, row)
}

/// `site_row` with its vectors in `mcx` (a redo under a step's continuation: the caller's context, not the continuation's).
fn site_row_in<'x>(b: &mut FunctionBuilder<'_>, env: &mut Env, mcx: Mcx<'x>, site: usize, atts: &[Option<ExprOut>], row: &RowVals) -> RowVals<'x> {
    let at = site_off(site) as i32;
    let (vp, np) = (b.load(Type::Ptr, env.ctx, at + site::VALUES as i32), b.load(Type::Ptr, env.ctx, at + site::ISNULL as i32));
    let (mut cells, mut vals, mut cols) = (PgVec::new_in(mcx), PgVec::<(Value, Option<Value>)>::new_in(mcx), mcx::pg_vec![in mcx; None; CTID_AT as usize]);
    for (a, out) in atts.iter().enumerate() {
        let Some(out) = out else { continue };
        let (v, n) = (b.load(Type::I64, vp, a as i32 * 8), b.load_zext(Type::I8, np, a as i32));
        let isnull = b.icmp(IntCond::Ne, n, env.zero);
        cols[a] = Some(cells.len() as u16);
        vals.push_in((v, Some(isnull)));
        cells.push_in(Cell::of_expr(*out, cells.len() as u16, cells.len() as u16, Nullable::Maybe));
    }
    // ctid: the version's position (existing() / fetch() stored it), as the heap row's ItemPointerData image (frame/site.rs)
    let w0 = b.load(Type::I32, env.ctx, at + site::POS_FLAGS as i32);
    let (sixteen, blk) = (b.iconst(Type::I32, 16), b.load(Type::I32, env.ctx, at + site::POS_BLOCK as i32));
    let off = b.int_bin(IntBinOp::Ushr, w0, sixteen);
    let (hi, lo) = (b.int_bin(IntBinOp::Ushr, blk, sixteen), b.int_bin(IntBinOp::Shl, blk, sixteen));
    let (bi, slot) = (b.int_bin(IntBinOp::Or, hi, lo), b.declare_stack_slot(8, 8));
    let img = b.stack_addr(slot);
    b.store(img, 0, bi);
    b.store(img, 4, off);
    let tid = ExprOut { rep: OutCol::new("", TIDOID, -1, 6, false).rep, typid: TIDOID, life: ExprLife::Row };
    cols[CTID_AT as usize - 1] = Some(cells.len() as u16);
    cells.push_in(Cell::of_expr(tid, cells.len() as u16, 0, Nullable::Never));
    vals.push_in((b.conv(ConvOp::PtrToInt, Type::I64, img), None));
    RowVals { row: Row { cells }, vals, cols, carry: row.carry.copy_in(mcx), next: row.next, at: row.at }
}

/// mergecmd D655 Row C (merge/epq.rs BUILT, D776's form: a MERGE in a data-modifying CTE whose target the planner hashed on
/// the build side of the write pipeline's probe): the build keys, which the redo images over the build side's row of the
/// newest version and compares with the image of the record the probe matched (`Env::redo_probes` under the probe's
/// node), as EvalPlanQual's rerun of the Hash Join hashes the newest version and probes it with the source row (the
/// version the join read: its REFERENCE mark's refetch).
pub(crate) struct BuildKeys<'g> {
    probe: i32,
    build: i32,
    keys: PgVec<'g, crate::ops::hashjoin::keys::KeySrc>,
}

/// mergecmd D655 Row C: a probe's hash clauses decide its match by the key images alone (none runs again on each
/// candidate: hashjoin keys.rs `confirms`, D1899), so comparing images is the join's test.
pub(crate) fn images_decide(gx: &crate::graph::Gx<'_, '_>, clauses: &types_nodes::list::NodeList<'_>) -> bool {
    !crate::ops::hashjoin::keys::confirms(gx, clauses)
}

impl<'g> BuildKeys<'g> {
    /// The keys of build pipeline `q`'s build op (its last) for probe op `jp` of pipeline `k`.
    pub(crate) fn of(px: &mut Px<'g, '_>, q: usize, (k, jp): (usize, usize)) -> BuildKeys<'g> {
        let (gx, mcx) = (px.gx, px.mcx());
        let pq = &gx.pipelines()[q];
        let build = pq.node_id;
        let Some(Op::Join(JoinDesc::Build { keys, decls, .. })) = pq.ops.last() else { crate::refuse!(build, Reason::Bug { payload: "a hashed MERGE target's producer without its build op" }) };
        let keys = keys.iter().zip(decls.iter()).map(|(x, d)| crate::ops::hashjoin::keys::key_src(px, x, build, d)).collect_in(mcx);
        BuildKeys { probe: crate::rerun::inner::op_node(&gx.pipelines()[k], jp), build, keys }
    }

    /// Whether this lowering holds the record image the probe matched (none past a LEFT probe's NULL-extended row).
    pub(crate) fn reaches(&self, env: &Env) -> bool {
        env.redo_probes.iter().any(|x| x.0 == self.probe)
    }

    /// Whether build row `r` keys as the record the probe matched: its image against the record's; a NULL key (none
    /// matches) leaves for `miss`.
    pub(crate) fn matched(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, r: &RowVals, miss: Block) -> Value {
        let Some(img0) = env.redo_probes.iter().rev().find(|x| x.0 == self.probe).map(|x| x.1.to_vec()) else { crate::refuse!(node, Reason::Shape { why: PROBE_ROW }) };
        let kc = crate::ops::hashjoin::keys::key_cells(b, env, self.build, r, &self.keys, Some((miss, &[])));
        if let Some(null) = crate::ops::hashjoin::keys::any_null(b, &kc) {
            let l_on = b.create_block(&[]);
            b.cond_br(null, (miss, &[]), (l_on, &[]));
            b.switch_to(l_on);
        }
        let mut lx = env.lx(b, self.build);
        let img = crate::ops::hashjoin::keys::image(&mut lx, self.build, &kc, &self.keys);
        env.memo = lx.memo;
        if img.words.words.len() != img0.len() {
            crate::refuse!(node, Reason::Bug { payload: "a MERGE redo's build image wider than its probe's record" });
        }
        gravity_expr::emit::hash::emit_image_eq(b, &img.words, &gravity_expr::emit::hash::ImageValues { words: img0 })
    }
}

/// D1672: one TID clause of the in-IR recheck (`Redo::Tid`).
enum TidCheck {
    /// `ctid = ANY (array)`, compiled at the scan's node.
    Pred(ExId),
    /// The newest version's ctid `cond` the clause's value, the scan's key word `word` (a tid image's address, or NULL).
    Cmp { word: u16, cond: IntCond },
    /// A NULL constant value: no point (TidListEval drops it) and no range (TidRangeEval's NULL bound).
    Never,
}

/// D1672: clause `q` of a TID scan over range table entry `varno` as a `TidCheck`: `ctid = v` / `v = ctid` an equality, a
/// bound its comparison with the operator commuted when ctid is on the right (`v < ctid` is a lower bound), as exec
/// sites_heap.rs `tid_clause` reads it for the host. The value's word is the one the scan's key filed for that node
/// (lifetime/bound.rs `word`: a by-reference constant or an expression is one word per node, a parameter one per id); a value placed
/// otherwise (computed from executor parameters, D1016) refuses by name.
fn tid_check<'p>(px: &mut Px<'_, 'p>, q: Node<'p>, varno: i32, node: i32, src: i32, ex: &'static ExprFace) -> TidCheck {
    use gravity_decompose::catalog::scan::{tid_op, TidOp};
    if q.as_scalar_array_op_expr().is_some() {
        return TidCheck::Pred(ex.0.compile(px, q, None, src));
    }
    let Some(op) = q.as_op_expr().filter(|o| o.args.len() == 2) else { crate::refuse!(node, Reason::Shape { why: TID }) };
    let is_ctid = |n: Node<'_>| n.as_var().is_some_and(|v| v.varno == varno && v.varlevelsup == 0 && v.varattno == -1);
    let flip = !is_ctid(op.args.nth(0));
    let v = if flip { op.args.nth(0) } else { op.args.nth(1) };
    if v.as_const().is_some_and(|c| c.constisnull) {
        return TidCheck::Never;
    }
    let cond = match (tid_op(op.opno), flip) {
        (Some(TidOp::Eq), _) => IntCond::Eq,
        (Some(TidOp::Less { inclusive }), false) | (Some(TidOp::Greater { inclusive }), true) => if inclusive { IntCond::Ule } else { IntCond::Ult },
        (Some(TidOp::Greater { inclusive }), false) | (Some(TidOp::Less { inclusive }), true) => if inclusive { IntCond::Uge } else { IntCond::Ugt },
        (None, _) => crate::refuse!(node, Reason::Shape { why: TID }),
    };
    let k = px.making.0;
    match crate::lifetime::bound::word(px, crate::lifetime::bound::Feed::Recheck, k, v, node) {
        gravity_lower::Place::Word(word) => TidCheck::Cmp { word, cond },
        _ => crate::refuse!(node, Reason::Shape { why: TID }),
    }
}

/// D1672: an ItemPointerData image at address `p` (bi_hi, bi_lo, ip_posid) as one unsigned key, block then offset, so the
/// key order is ItemPointerCompare's.
fn tid_key(b: &mut FunctionBuilder<'_>, p: Value) -> Value {
    let p = b.conv(ConvOp::IntToPtr, Type::Ptr, p);
    let (hi, lo, off) = (b.load_zext(Type::I16, p, 0), b.load_zext(Type::I16, p, 2), b.load_zext(Type::I16, p, 4));
    let sixteen = b.iconst(Type::I64, 16);
    let hi = b.int_bin(IntBinOp::Shl, hi, sixteen);
    let block = b.int_bin(IntBinOp::Or, hi, lo);
    let block = b.int_bin(IntBinOp::Shl, block, sixteen);
    b.int_bin(IntBinOp::Or, block, off)
}

/// D1672: the newest version's ctid key `at` `cond` the tid at statement word `word`; false when the word is NULL.
fn tid_cmp(b: &mut FunctionBuilder<'_>, env: &mut Env, at: Value, word: u16, cond: IntCond) -> Value {
    let (p, nb) = (b.load(Type::I64, env.params, word as i32 * 8), b.load_zext(Type::I8, env.params_null, word as i32));
    let f = b.bconst(false);
    let bt = b.value_type(f);
    let (l_val, l_join) = (b.create_block(&[]), b.create_block(&[bt]));
    let null = b.icmp(IntCond::Ne, nb, env.zero);
    b.cond_br(null, (l_join, &[f]), (l_val, &[]));
    b.switch_to(l_val);
    let k = tid_key(b, p);
    let hit = b.icmp(cond, at, k);
    b.br(l_join, &[hit]);
    b.switch_to(l_join);
    b.block_params(l_join)[0]
}

use core::fmt::{self, Write};

use gravity_abi::breaker::{BreakerKind, Budget, Spill};
use gravity_program::ArenaVec;
use types_nodes::jointype::JoinType;
use types_nodes::plannodes::Sort;
use types_nodes::Node;

use crate::ops::shared::sort::NO_SCALE;
use crate::ops::shared::sort::{indeterminate, INDETERMINATE_COLL};
use crate::expr;
use crate::catalog::{keys, Facts, KeyReq};
use crate::graph::{Bound, Op, Source};
use crate::facts::hints::Hint;
use crate::join::JoinDesc;
use crate::ledger::Kind;
use crate::ops;
use crate::admit::Refusal;
use crate::ops::shared::breaker::{close, emit};
use crate::ops::shared::order::reformed_family;
use crate::ops::shared::sort::dump_keys;
use crate::catalog::typefacts::{self, KeyColl};
use crate::walk::{Builder, Open};
use crate::ops::shared::sort::{keys_layout, SortDesc, SortForm, SortKey};
use crate::ops::shared::sort::{below, reads_whole};
pub(crate) use crate::ops::shared::sort::whole_read;

impl SortKey {
    /// O3 (M3 decision 16): a `name` key (sorted by an ordering operator over `name`: `namelt` / `namegt`), which
    /// rep.word.order images as 64 bytes under a collation that orders by bytes; the dump marks it `#19`.
    pub fn is_name(&self) -> bool {
        keys::key_order(self.opno).is_some_and(|o| o.typing == keys::Typing::Name)
    }

    /// A `bpchar` key: it orders by `bpcharcmp` (trailing blanks ignored), the one fact every run consumer trims by, whatever
    /// its column's type (a merge join's side sorts a text column `USING <` of bpchar's when its clause relabels it, R2
    /// 13b); also bpchar_pattern_ops' `~<~` / `~>~` (core-0241: the same compare under "C").
    pub fn is_bpchar(&self) -> bool {
        keys::key_order(self.opno).is_some_and(|o| o.typing == keys::Typing::Bpchar)
    }

    /// USEROP, the CompareFn key kind: the key orders only by its btree family's comparator (`Facts::key`'s `cmp`,
    /// called through fmgr: `gravity_fn::typesem::keysem::call`, lowerv4 rep.hosted.order), never by its type's image: an operator of a
    /// family that is not its type's default (`own_family`), except text_pattern_ops' / bpchar_pattern_ops' operators
    /// (`keys::key_order`'s `bytes`), whose key `sort_keys` puts under "C" so that its image is their order (D1346 /
    /// core-0241). A user family's operator over text or name under "C" is one (its comparator need not order by bytes).
    /// Every ordered lowering (run sort, top-k, merge join, window, MergeAppend, sorted group) gives such a key its
    /// comparator or refuses it by name; none may order it by an image.
    pub fn compare_fn(&self) -> bool {
        self.own_family && !keys::key_order(self.opno).is_some_and(|o| o.bytes)
    }

    /// A key from its type's catalog facts alone (no child to read a numeric scale through; V4R-84).
    pub fn of_type(facts: &dyn Facts, resno: i16, opno: u32, desc: bool, nulls_first: bool, typid: u32, coll: KeyColl) -> SortKey {
        let (kind, image) = crate::catalog::keysem::image::sort_key_form(facts, typid, coll.oid);
        SortKey { resno, opno, desc, nulls_first, kind, image, scale: NO_SCALE, coll, own_family: false, no_proc: None, eq_tie: false, per_row: false }
    }
}

pub use crate::catalog::keysem::image::order_image;

/// A reader op above a raising Sort that starts it: a projection, a filter, a Limit of a constant count past 0 or of none
/// (a constant OFFSET), a Unique's first-of-run, a merge join reading it as its outer, or (D1292)
/// a window whose frame has no offset (its first call pulls the Sort: nodeWindowAgg's `calculate_frameoffsets`, which can
/// raise first, evaluates offsets only), so PG's begin runs and raises; ops/sort/sort_raise.rs in lowerv4.
pub fn starts_raise(o: &Op<'_>) -> bool {
    use crate::ops::shared::window::FrameBound::{Following, Preceding};
    match o {
        Op::Window(w) => !matches!(w.frame.start, Preceding(_) | Following(_)) && !matches!(w.frame.end, Preceding(_) | Following(_)),
        // v0.4: a Limit of no count (an OFFSET alone) pulls too unless its offset is computed (evaluated first), and so
        // does a Unique's first-of-run (nodeUnique pulls its first tuple at its first call)
        Op::Limit { count: Bound::All, offset, .. } => matches!(offset, Bound::Const(_) | Bound::All),
        // and a merge join over the Sort as its outer: ExecMergeJoin fetches its outer first at its first call
        Op::Join(crate::join::JoinDesc::Merge { .. }) => true,
        _ => matches!(o, Op::Project(_) | Op::Filter(_) | Op::Limit { count: Bound::Const(1..), .. } | Op::FirstOfRun(_)),
    }
}

/// The pipeline other than `k` that reads the breaker pipeline `k` ends in through a Sort, with that Sort (a feeder of a
/// top over an Append: ops/sort/sort_raise.rs `wrap_feeder` in lowerv4 raises at the feeder's open when that Sort raises).
pub fn sort_over_feeder<'a, 'p>(pipes: &'a [crate::graph::PipelineNode<'p>], k: usize) -> Option<(usize, &'a SortDesc<'p>)> {
    let gravity_program::End::Breaker(x) = pipes.get(k)?.desc.ends_in else { return None };
    pipes.iter().enumerate().find_map(|(j, p)| match (&p.source, p.ops.iter().find_map(|o| if let Op::Sort(d) = o { Some(d) } else { None })) {
        (Source::Breaker { slot }, Some(d)) if *slot == x.slot && j != k => Some((j, d)),
        _ => None,
    })
}

/// W8-EQUIVCLASS-SORTOP (MERGE R5, main 09-28 20:26Z): a Sort by an operator whose btree family has no order proc for its
/// type (`SortKey::no_proc`). PG 18.6 raises XX000 at the Sort's first call; lowerv4 serves that raise only for a root merge
/// join's inner (runs/sort_noproc.rs) and refuses every other form by this name.
pub const NO_ORDER_PROC: &str = "W8-EQUIVCLASS-SORTOP: a sort by an operator whose btree family has no order proc for its type, outside a root merge join's inner (PG raises at the Sort's first call)";

/// The `(opcintype, opfamily)` PG's raise names: the first such key's (tuplesort_begin_heap prepares the keys in order).
pub fn no_proc(d: &SortDesc<'_>) -> Option<(u32, u32)> {
    d.keys.iter().find_map(|k| k.no_proc)
}

/// Whether a Sort of `g` (a pipeline's or an embedded inner's) has a `no_proc` key: exec never files a program holding
/// its raise in the plan memo (PG looks the proc up at each Sort's first call; its plancache does not follow pg_amproc).
pub fn holds_no_proc(g: &crate::graph::PipelineGraph<'_>) -> bool {
    fn pipe(p: &crate::graph::PipelineNode<'_>) -> bool {
        crate::walk::stack_check(); // D3748
        p.ops.iter().any(|o| match o {
            Op::Sort(d) => no_proc(d).is_some(),
            Op::Join(JoinDesc::Nest { inner, .. }) => pipe(inner),
            Op::SubPlan(s) => matches!(&s.form, crate::ops::shared::subplan::SubForm::Nested { inner } if pipe(inner)),
            _ => false,
        })
    }
    g.pipelines.iter().any(pipe)
}

fn sort_keys<'p>(b: &Builder<'p, '_>, child: &Open<'p>, s: &Sort<'p>, k: Kind, id: i32) -> Result<ArenaVec<SortKey>, Refusal> {
    let n = s.numCols as usize;
    if n == 0 || s.sortColIdx.len() < n || s.sortOperators.len() < n || s.nullsFirst.len() < n {
        return Err(Refusal::shape(k, id, "a sort without keys"));
    }
    let mut keys = ArenaVec::with_capacity_in(n, b.arena);
    for i in 0..n {
        let opno = s.sortOperators[i];
        // D1346: text_pattern_ops' ~<~ / ~>~ order by bytes whatever the collation (bttext_pattern_cmp): the key under "C",
        // its order its own (`own_family`). core-0241: bpchar_pattern_ops' likewise: its compare
        // (internal_bpchar_pattern_compare: trailing blanks off, memcmp, then length) is bpcharcmp's under "C"
        let pattern = keys::key_order(opno).filter(|o| o.bytes);
        let (resno, coll) = (s.sortColIdx[i], if pattern.is_some() { typefacts::C_COLLATION } else { s.collations.get(i).copied().unwrap_or(0) });
        let e = (resno as usize).checked_sub(1).filter(|&j| j < s.plan.targetlist.len()).and_then(|j| s.plan.targetlist.nth(j).as_target_entry()).ok_or(Refusal::shape(k, id, "a key outside the target list"))?;
        // D1346 / USEROP: another family's operator over any type orders by that family's comparator alone (the CompareFn
        // key kind, `SortKey::compare_fn`: lowerv4 gives the key its comparator or refuses it by name, never its image)
        // KeySem slice 3: the key's one order answer (`Facts::key`): its type's default family is the direction table's
        let (desc, own_family, no_proc) = match b.facts.key(KeyReq::sort(opno)) {
            // D858: an int4 column sorted by oid's `<` (a merge side under a relabel) orders by oid's comparator alone
            Some(o) if o.default_family && reformed_family(b.facts, opno, crate::catalog::expr_type::type_of(e.expr)).is_some() => (o.desc, true, None),
            Some(o) if o.default_family => (o.desc, false, None),
            _ if pattern.is_some() => (pattern.is_some_and(|o| o.desc), true, None),
            Some(o) => {
                // W8-EQUIVCLASS-SORTOP: a full Sort's key PG cannot begin (lowerv4 runs/sort_noproc.rs), in its type's
                // word order and never compared (a hosted type's: its default comparator stands in, lowerv4 keylane.rs)
                let np = (o.no_proc && k == Kind::Sort).then_some((o.opcintype, o.family));
                (o.desc, np.is_none(), np)
            }
            None => return Err(Refusal::shape(k, id, "a sort operator outside the direction table")),
        };
        if k != Kind::Sort && crate::catalog::expr_type::type_of(e.expr).is_some_and(|t| indeterminate(b.facts, t, coll)) {
            return Err(Refusal::shape(k, id, INDETERMINATE_COLL)); // a full Sort's: `map` after its form
        }
        let eq_tie = !own_family && record_eq_tie(b, id, resno, e.expr);
        let per_row = !own_family && crate::catalog::expr_type::type_of(e.expr).is_some_and(crate::catalog::anonymous_record) && crate::facts::record::per_row(b, id, resno, e.expr, false);
        let key = match crate::ops::agg::key_facts(b, child, e.expr, coll) {
            Some((kind, image, scale)) => {
                let (kind, image) = crate::catalog::expr_type::type_of(e.expr).map_or((kind, image), |t| crate::catalog::keysem::image::order_image(b.facts, t, kind, image));
                SortKey { resno, opno, desc, nulls_first: s.nullsFirst[i], kind, image, scale, coll: b.facts.coll(coll), own_family, no_proc, eq_tie, per_row }
            }
            None => SortKey { own_family, no_proc, eq_tie, per_row, ..SortKey::of_type(b.facts, resno, opno, desc, s.nullsFirst[i], crate::catalog::expr_type::type_of(e.expr).ok_or(Refusal::shape(k, id, "a key of unknown type"))?, b.facts.coll(coll)) },
        };
        keys.push(key);
    }
    Ok(keys)
}

/// setop_core-0129: whether anonymous `record` key `e` (resno `resno` of Sort `id`) ties two equal copies with no
/// `record_cmp` call: every producer is one ROW() (`facts::record::key_row`) and each of its columns is a type the type
/// table orders by a default btree comparator over a scalar image (`cmp`, not `Canonical`: no array, range, record or
/// type of a fingerprint image, whose comparators look up more comparators) and is not collatable. `record_cmp` then
/// finds every column's comparator (no "could not identify a comparison function") and never reaches a collated compare
/// (no 42P22 under no collation, D2043), and equal copies are one row type's equal column images, which those
/// comparators call equal. A record of a collatable, container or user-typed column keeps its call, and so does a folded
/// record constant (its columns are the record cache's; reading them allocates, and decompose's steady state does not).
fn record_eq_tie<'p>(b: &Builder<'p, '_>, id: i32, resno: i16, e: Node<'p>) -> bool {
    if !crate::catalog::expr_type::type_of(e).is_some_and(crate::catalog::anonymous_record) {
        return false;
    }
    let Some(mut row) = crate::facts::record::key_row(b, id, resno, e, false) else { return false };
    while let Some(a) = expr::through_relabel(row) {
        row = a;
    }
    let Some(args) = row.as_row_expr().map(|r| &r.args) else { return false };
    let plain = |t: u32| typefacts::facts_of(b.facts, t).is_some_and(|f| f.cmp != 0 && !f.collatable && f.image.equality());
    args.len() != 0 && args.iter().all(|a| crate::catalog::expr_type::type_of(a).is_some_and(plain))
}

const fn row_bytes(natts: usize) -> u64 {
    16 + 8 * natts as u64 + natts.div_ceil(8) as u64 * 8
}

/// A zone walk under a LIMIT / OFFSET whose computed bound acts: the walk reads the bound itself.
const CUT_ACTS: &str = "a zone walk under a LIMIT / OFFSET computed by code that acts";

/// A top with a cut reads its LIMIT / OFFSET itself, where PostgreSQL's Limit evaluates the bound once per open and passes
/// the value down (`ExecSetTupleBound`): the top's own evaluation is a `More` departure, which a bound that acts (`nextval`,
/// a VOLATILE call, a catalog miss) is denied; the Sort is then a full sort and the Limit above it cuts.
fn cut_licensed<'p>(b: &Builder<'p, '_>, count: Bound<'p>, offset: Bound<'p>) -> bool {
    use crate::effects::{licence, of_expr, Cx, Departure, Effects, PLAIN};
    let cx = Cx::new(b.stmt, b.facts, b.arena);
    [count, offset].into_iter().all(|x| match x {
        Bound::Expr(n) => {
            let e = of_expr(&cx, n, &PLAIN);
            e == Effects::NONE || licence(e, Departure::More).is_ok()
        }
        Bound::All | Bound::Const(_) => true,
    })
}

/// A top's breaker budget and spill before lowering: `count + offset` rows of `row_bytes(natts)` held, or, for a bound
/// that is not a constant (or WITH TIES), the share with its runs spilled past it. DC-04: no lowering keeps either; a row
/// top's end (lowerv4 runs.topk_rows) describes its breaker as a Sort over the share whose runs spill, sized by the breaker
/// cost from its run row, and a top over groups (runs.topk_groups) asks no region, its records staying in the group emit's
/// array. C10 (SPILL-DEBT TopKShare deleted): a top over the share declares the runs its lowering spills, so no top is
/// declared to grow past its grant.
fn topk_budget(count: Bound<'_>, offset: Bound<'_>, ties: bool, natts: usize) -> (Budget, Spill) {
    let fixed = |rows: u64| Budget::Fixed((64 + rows * row_bytes(natts)).min(u32::MAX as u64) as u32);
    match (count, offset) {
        _ if ties => (Budget::Share, Spill::Runs),
        (Bound::Const(c), Bound::Const(o)) if c >= 0 && o >= 0 => (fixed(c as u64 + o as u64), Spill::None),
        (Bound::Const(c), Bound::All) if c >= 0 => (fixed(c as u64), Spill::None),
        _ => (Budget::Share, Spill::Runs),
    }
}

pub fn map<'p>(b: &mut Builder<'p, '_>, node: Node<'p>, k: Kind) -> Result<Open<'p>, Refusal> {
    let (s, presorted): (&'p Sort<'p>, u8) = match k {
        Kind::IncrementalSort => {
            let is = node.as_incremental_sort().unwrap();
            (&is.sort, is.nPresortedCols.clamp(0, 255) as u8)
        }
        _ => (node.as_sort().unwrap(), 0),
    };
    let plan = &s.plan;
    let id = plan.plan_node_id;
    if matches!(k, Kind::Sort | Kind::IncrementalSort) && b.absorb.take_if(|a| *a == id).is_some() {
        // D612: the sorted group above orders its records by these keys itself (absorb.rs): the Sort is no pipe; the stop
        // road's DISTINCT group streams over an Incremental Sort's presorted input (facts/stream.rs presort_stop)
        let mut open = b.child(plan, k)?;
        ops::project(&mut open, &plan.targetlist);
        return Ok(open);
    }
    // the Limit's bound for this sort (ops/mod.rs `map_limit`), read before the child maps: a Limit in the child's subtree
    // (a LATERAL's own top, a subquery's LIMIT) sets and clears `b.limit`, so read after it the top lost its bound and
    // became a full sort under a Limit step (rnd-2769: `ORDER BY c_id, x DESC LIMIT 3` over a LATERAL `.. LIMIT 5` read
    // every outer row, where PostgreSQL's bounded Incremental Sort stopped after two)
    let limit = b.limit.filter(|l| l.0 == id).map(|l| (l.1, l.2, l.3));
    let mut open = b.child(plan, k)?;
    if !plan.qual.is_nil() {
        return Err(Refusal::shape(k, id, "a qual on Sort"));
    }
    // an Incremental Sort's presorted prefix over a union that declares no order (a sorted grouping-sets node's arms,
    // gsets.rs `map_sets`: PG reads the node's rollup order, the streamed arms do not keep it) holds nothing: the sort
    // orders by all its keys, a full sort or top (regress groupingsets#76; a bounded one cut PG's rows short)
    let pg_presorted = presorted;
    let presorted = match open.union {
        Some(u) if presorted > 0 && !b.graph.unions[u as usize].order.is_known() => 0,
        _ => presorted,
    };
    let keys = sort_keys(b, &open, s, k, id)?;
    // D1346 / D3036: a non-default family's key sorts rows alone, by its comparator: never a presorted prefix, a zone walk
    // or a breaker's records (a top over groups or a sorted group's records order by key images). So such a key takes the
    // forms that sort rows: a presorted prefix holding one is sorted by all its keys (presorted 0), a zone-walk hint over
    // one is left unused, and over a breaker a CompareFn key's top becomes the full row sort (USEROP K10.y1 .h0: lowerv4
    // runs.sort's row sort through the comparator, never form G's record order), the Limit above cutting its rows as
    // PostgreSQL's Limit over its Sort does (the Limit always keeps its own step). A pattern family's key (text_pattern_ops:
    // its image under "C" is its order) sorts the rows by that image the same way.
    const OWN_KEY: &str = "a presorted, zone-walked or breaker-fed key of a non-default btree family (D3036)";
    let own = keys.iter().any(|x| x.own_family);
    let presorted = if keys.iter().take(presorted as usize).any(|x| x.own_family) { 0 } else { presorted };
    let hint = Hint::of_sort(s).filter(|_| !own);
    let breaker_fed = matches!(open.source, Source::Breaker { .. });
    let layout = keys_layout(b.arena, keys.iter().map(|x| x.kind));
    let natts = plan.targetlist.len();
    let form = match (hint, limit) {
        (Some(Hint::ZoneWalk { .. }), Some((count, offset, _))) if !cut_licensed(b, count, offset) => return Err(Refusal::shape(k, id, CUT_ACTS)),
        (Some(Hint::ZoneWalk { attno, desc, nulls_first }), Some((count, offset, ties))) => SortForm::Walk { attno, desc, nulls_first, count, offset, ties },
        (Some(Hint::ZoneWalk { .. }), None) => return Err(Refusal::shape(k, id, "a ZoneWalk without a Limit above")),
        (Some(_), _) => return Err(Refusal::shape(k, id, "a hint that is not ZoneWalk on Sort")),
        (None, Some(_)) if breaker_fed && own => SortForm::Runs,
        (None, Some((count, offset, ties))) if !matches!(count, Bound::All) && cut_licensed(b, count, offset) => SortForm::TopK { count, offset, ties },
        _ => SortForm::Runs,
    };
    if breaker_fed && own && !matches!(form, SortForm::Runs) {
        return Err(Refusal::shape(k, id, OWN_KEY));
    }
    if pg_presorted > 0 && (presorted == 0 || matches!(form, SortForm::Runs)) {
        let _ahead = lazy_ahead(b, s, limit.is_some_and(|l| !matches!(l.0, Bound::All)), presorted == 0, k, id)?;
    }
    let sd = SortDesc { keys, presorted, form, tlist: &plan.targetlist };
    // Option 1 serves a full sort's raise, and a top's under a constant count past 0 (PG's Limit pulls the Sort, which
    // begins and raises; a count of 0 or a run-time one may leave the Sort unstarted)
    let starts = match sd.form { SortForm::Runs => true, SortForm::TopK { count: Bound::Const(c), .. } => c > 0, _ => false };
    if !starts && crate::effects::sort_raises(b.facts, &sd) {
        return Err(Refusal::shape(k, id, INDETERMINATE_COLL));
    }
    if !matches!(sd.form, SortForm::Runs) && no_proc(&sd).is_some() {
        return Err(Refusal::shape(k, id, NO_ORDER_PROC));
    }
    let (keys, form) = (sd.keys, sd.form);
    let (kind, budget, spill) = match form {
        SortForm::Runs => (BreakerKind::Sort, Budget::Share, Spill::Runs),
        SortForm::TopK { count, offset, ties } | SortForm::Walk { count, offset, ties, .. } => {
            let (budget, spill) = topk_budget(count, offset, ties, natts);
            (BreakerKind::TopK, budget, spill)
        }
    };
    // a grouping-sets union (gsets.rs `map_sets`: each arm projects, the union's open none) under a top keeps its
    // Materialize form (append.rs `copies`): the Sort's own tlist (PG's dummy references) projects the union's row, as
    // map_append's Append tlist does, so the run row has its columns
    if open.union.is_some() && !open.ops.iter().any(|op| matches!(op, Op::Project(_))) && !(matches!(form, SortForm::Runs) && presorted == 0) {
        ops::project(&mut open, &plan.targetlist);
    }
    if matches!(form, SortForm::Runs) && open.desc.is_eligible() && open.desc.driving_site >= 0 {
        open.desc = open.desc.finishing();
    }
    open.push(Op::Sort(SortDesc { keys, presorted, form, tlist: &plan.targetlist }));
    let slot = close(b, open, kind, layout, budget, spill, k, id)?;
    let mut out = emit(b, id, slot);
    ops::project(&mut out, &plan.targetlist);
    b.graph.note(k);
    Ok(out)
}

pub const LAZY_AHEAD: &str = "D2316: an Incremental Sort that reads its whole input before its first row, over input that may raise, under a read PostgreSQL's presorted batches may stop (its lazy form is not built)";

/// D2316 (errorhold): PostgreSQL's Incremental Sort reads its input one presorted group at a time, where this sort reads
/// every input row before its first: the full sort or top over a grouping-sets union (`over_union`), and the unbounded
/// presorted sort, whose end sorts each run of the whole input. Under a read that stops (a LIMIT above: `limited`; a
/// cursor's FETCH n) a later group's input runs where PostgreSQL never runs it: a departure ahead. Input that may raise is
/// granted it with a recheck (an error before any row left re-runs the statement in plan order); there, PostgreSQL's
/// lazy form not being built, a read that may stop refuses by name, and a read PostgreSQL takes to the end (no LIMIT,
/// every node above reads its whole input: `whole_read`, no FETCH count) keeps the form, its error PostgreSQL's. A
/// presorted sort under a LIMIT, or under any other node that may stop, keeps its stop's rule (D449, `facts::stream`),
/// but a merge join that may stop reading the sort's side (`merge_stops`, D3365) is such a read too: no rule above sees it.
/// There the plan-order run marks the sort `held_incs` (the presorted run, not over a union): lowerv4 holds the fill's
/// error at the rows PostgreSQL's batches handed on before the raising row, its reader raising it past them, or refuses
/// by this name where it does not build that form (ops/sort/inc_held.rs). Input that acts keeps today's form outside the
/// plan-order run (the volatile family).
/// Construct C2: the licence the whole read ahead holds comes back: the input's own code shows nothing
/// (`effects::unobservable`), the recheck licence granted, or, denied over a read PostgreSQL takes to the end
/// (`whole`), the whole-read licence of input that acts nowhere (`effects::whole_read`: the guard the run-ahead tracer
/// checks). `None`: no read ahead at this site (no stop it departs from; a LIMIT's own rule), the held form (lowerv4
/// ops/sort/inc_held.rs holds the held error's licence), or a denial that keeps the form with no licence: input that
/// acts, or a read that may stop (a merge that may stop reading the side, a LIMIT over a grouping-sets union: D4121 (b)).
fn lazy_ahead<'p>(b: &mut Builder<'p, '_>, s: &'p Sort<'p>, limited: bool, over_union: bool, k: Kind, id: i32) -> Result<Option<crate::effects::Licence>, Refusal> {
    use crate::effects::{denying, denying_bounded, licence, of_plan, unobservable, Cx, Departure, Effects};
    let whole = !limited && whole_read(b, id);
    let merge = !limited && !whole && merge_stops(b, id);
    if !whole && !merge && !(limited && over_union) {
        return Ok(None);
    }
    let cx = Cx::new(b.stmt, b.facts, b.arena);
    let own = s.plan.lefttree.map_or(Effects::NONE, |c| of_plan(&cx, c));
    if own == Effects::NONE {
        return Ok(unobservable(own));
    }
    let ask = if own.may_raise { own.and(Effects { acts: crate::effects::of_statement_acts(&crate::effects::Cx::new(b.stmt, b.facts, b.arena), &mut b.graph.acts_read), ..Effects::NONE }) } else { own };
    match licence(ask, Departure::Ahead) {
        Ok(l) => {
            b.graph.licensed(&l);
            if l.recheck() {
                b.graph.licensed_nodes.push(id); // C2 R3: the sort's fill runs this departure
            }
            Ok(Some(l))
        }
        Err(_) if denying() && own.may_raise && merge && !over_union && !denying_bounded() => {
            b.graph.held_incs.push(id);
            Ok(None)
        }
        Err(_) if denying() && own.may_raise && (!whole || denying_bounded()) => Err(Refusal::shape(k, id, LAZY_AHEAD)),
        Err(_) => Ok(if whole { crate::effects::whole_read(own).ok() } else { None }),
    }
}

/// D3365: whether a merge join above plan node `id` may stop reading the side `id` is under, with only `whole_read`'s
/// nodes and merge sides read to their end between them. PostgreSQL's merge fetches no more of a side once the other
/// ends, unless it fills that side's unmatched rows (nodeMergejoin.c `mj_FillOuter` / `mj_FillInner`).
fn merge_stops(b: &Builder<'_, '_>, id: i32) -> bool {
    // Some(None): `id` is below, read to its end so far; Some(Some(stops)): decided below
    fn go(n: Node<'_>, id: i32) -> Option<Option<bool>> {
        crate::walk::stack_check(); // D3748
        if n.as_plan()?.plan_node_id == id {
            return Some(None);
        }
        let (seen, outer) = below(n, |c| go(c, id))?;
        if seen.is_some() {
            return Some(seen);
        }
        let fills = |jt: JoinType| match outer {
            true => matches!(jt, JoinType::JOIN_LEFT | JoinType::JOIN_ANTI | JoinType::JOIN_FULL),
            false => matches!(jt, JoinType::JOIN_RIGHT | JoinType::JOIN_RIGHT_ANTI | JoinType::JOIN_FULL),
        };
        Some(match n.as_merge_join() {
            Some(m) => (!fills(m.join.jointype)).then_some(true),
            None => (!reads_whole(n)).then_some(false),
        })
    }
    b.stmt.planTree.and_then(|r| go(r, id)).flatten().unwrap_or(false)
}

pub fn dump_bound(x: Bound<'_>, out: &mut dyn Write) -> fmt::Result {
    match x {
        Bound::All => write!(out, "all"),
        Bound::Const(n) => write!(out, "{n}"),
        Bound::Expr(n) => expr::print(n, out),
    }
}

impl SortDesc<'_> {
    pub fn dump(&self, out: &mut dyn Write) -> fmt::Result {
        write!(out, "  sort keys=")?;
        dump_keys(&self.keys, out)?;
        if self.presorted > 0 {
            write!(out, " presorted={}", self.presorted)?;
        }
        match self.form {
            SortForm::Runs => write!(out, " form=runs"),
            SortForm::TopK { count, offset, ties } => {
                write!(out, " form=topk{} count=", if ties { "-ties" } else { "" })?;
                dump_bound(count, out)?;
                write!(out, " offset=")?;
                dump_bound(offset, out)
            }
            SortForm::Walk { attno, desc, nulls_first, count, offset, ties } => {
                write!(out, " form=zonewalk{} att={attno} {}{} count=", if ties { "-ties" } else { "" }, if desc { "desc" } else { "asc" }, if nulls_first { " nulls_first" } else { "" })?;
                dump_bound(count, out)?;
                write!(out, " offset=")?;
                dump_bound(offset, out)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::NoFacts;
    use crate::ops::shared::sort::KeyImage;
    use crate::catalog::typefacts::KeyColl;

    /// USEROP: a key is CompareFn exactly when its operator's family is not its type's default (an int4, a box, an array
    /// under a user opclass), except a text / bpchar key of a pattern family under a collation that orders by bytes (its
    /// image is its order); a default family's key is not.
    #[test]
    fn compare_fn_is_a_non_default_family_off_its_image() {
        let c = KeyColl { oid: 950, bytes_eq: true, bytes_order: true };
        let libc = KeyColl { oid: 100, bytes_eq: true, bytes_order: false };
        let op = |opno: u32, typid: u32, coll: KeyColl, own_family: bool| SortKey { own_family, ..SortKey::of_type(&NoFacts, 1, opno, false, false, typid, coll) };
        let key = |typid: u32, coll: KeyColl, own_family: bool| op(0, typid, coll, own_family);
        for t in [23, 20, 701, 1082, 1007, 19, 25] {
            assert!(key(t, c, true).compare_fn(), "type {t} under a user family, \"C\" too");
            assert!(!key(t, c, false).compare_fn(), "type {t} under its default family");
        }
        // OID-17 (overfit, keep): 2314 / 2326 are text_pattern_ops' / bpchar_pattern_ops' ~<~, the bootstrap catalog's
        let text = op(2314, 25, c, true);
        assert!(text.image == KeyImage::Text && !text.compare_fn(), "a pattern family's text key under C orders by its image");
        assert!(!op(2326, 1042, c, true).compare_fn(), "bpchar_pattern_ops' likewise");
        assert!(key(25, libc, true).compare_fn(), "a non-default family's text key under a libc collation orders by its comparator");
        assert!(!key(25, libc, false).compare_fn(), "text under its default family");
    }
}

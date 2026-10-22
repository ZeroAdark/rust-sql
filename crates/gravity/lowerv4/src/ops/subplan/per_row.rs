//! Kind `nest.subplan`: a per-row SubPlan, PG's `ExecScanSubPlan` (M8-plan §3.6, cards/M8.md; B2). Owner M8-SUB.
//! The step (`Op::SubPlan`, placed before the SubPlan's first reader) evaluates it for its row into two frame words, the
//! value and its flags (params.rs `SET` / `NULL`); the row's readers load them where their expression reads the node
//! (`Env::subplans`, gravity_expr's `has_subplan` hook, step/expr_ext_lazy.rs; L0-d: the reader compiles nothing under it).
//! Nested (B2b): `args` are written into the parParams (PG's `prm->value = ExecEvalExpr(arg)`), then the inner runs and
//! the link folds its rows in PG's order: EXISTS is true at the first row, which ends the walk; EXPR is the first row's
//! first column; ANY / ALL write each row's columns into `paramIds` and test it, ending the walk at the first true (ANY) or
//! false (ALL) and NULL when a test was NULL and none decided; ROWCOMPARE is the test over the one row. A second row of an
//! EXPR / ROWCOMPARE raises 21000; with no row EXPR / ROWCOMPARE are NULL, ANY false, ALL true. MULTIEXPR (`UPDATE ..
//! SET (a, b) = (SELECT ..)`) writes the one row's columns into its setParams, which the projection's entries read after
//! the step (PG evaluates it before them: execExpr.c `ExecCreateExprSetupSteps`); no row sets them NULL, a second raises
//! 21000, and its own entry reads NULL. The inner is a Result
//! over the parameters (its one-time qual and quals decide its one row, re-armed per call as `ExecReScanResult` does) or
//! nest/inner.rs's row-at-a-time scan or its rerun set's breaker emit (refilled at each open), re-opened per call. A row that a later expanding step's stop re-enters reads the
//! words its first evaluation wrote. B2c (L0-e): under a conditional expression, or a VOLATILE one read in several places,
//! the readers evaluate it where PG's interpreter reaches the node, per reference (`around`, face::nest::SubEval). A
//! by-reference EXPR value is copied per call into the lane's SubResult slot (RT-2a); an ARRAY link accumulates its rows in
//! the lane's SubResult accumulator (RT-2b) and is '{}' without one. A join qual's SubPlan is evaluated over the pair its
//! join reader hands the qual, a VALUES row's by the source's row where ValuesNext reaches its cell (site.rs, V4R-499).
//! Refused by name: a correlated VOLATILE SubPlan whose inner reads a pipeline kept across calls whose rows the keep rule
//! does not answer as PG's own (D1337); an ARRAY of
//! arrays or records; a hashed SubPlan (B2a); one in a ProjectSet's target list (the ending call reads it
//! before the step); one in a write's own lists (D1112, B4) but for its check options' (W6-D1112, V4R-528: host-fed,
//! write/rowop_sub.rs) and one only MERGE actions read (conditional: lazy at the action's read).
use gravity_abi::breaker::BreakerKind;
use gravity_abi::status::raise;
use gravity_decompose::ops::shared::srf::SrfPlace;
use gravity_decompose::ops::shared::subplan::{refers, refers_list};
use gravity_decompose::ops::shared::subplan::{SubForm, SubLink, SubSite};
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_decompose::{Op, PipelineNode, Source};
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::shared::object::entries as m8;
use types_nodes::Node;
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};

use crate::rerun::inner as nest_inner;
use crate::rerun::inner::Inner;
use crate::ops::subplan::result as sr;
use crate::ops::subplan::result::AccOp;
use crate::ops::subplan::site::{self, Lend};
use crate::face::end::EndMech;
use crate::face::kinds::{Face, KindId};
use crate::rerun::face::{SubEval, SubHolds};
use crate::face::step::{ExprFace, StepMech};
use crate::frame::env::Env;
use crate::frame::frame::{EndMaker, Next, StepMaker};
use crate::frame::px::{ExId, Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::rerun::params::{NULL, SET};
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;
use crate::rep;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::NestSubplan, &StepMaker(make)), (KindId::NestSubplan, &EndMaker(make_set)), (KindId::NestSubplan, &SubEval(site::eval_at)), (KindId::NestSubplan, &SubHolds(site::holds_at))], ..crate::ops::NONE };

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Link {
    Exists,
    Expr,
    Any,
    All,
    RowCompare,
    Array,
    MultiExpr,
}

enum Body<'g> {
    /// A Result without a child: its one-time qual and quals in order, and the entries the fold reads; D1262: the rerun set
    /// of its InitPlans, re-armed at every call before the Result reads a parameter.
    Once { tests: PgVec<'g, ExId>, cols: PgVec<'g, ExId>, rerun: Option<crate::rerun::sets::Rerun> },
    Scan(Inner<'g>),
}

struct Sub<'g> {
    node: i32,
    plan_id: i32,
    link: Link,
    /// Where its words are written (site.rs), and a `Source` step's entry. W6-D1112: `fed` = a write's check option reads
    /// it, its arguments the host's.
    lend: Lend,
    early: site::Early,
    fed: bool,
    /// `(parParam, argument)`, written per call.
    args: PgVec<'g, (i32, ExId)>,
    body: Body<'g>,
    /// The inner columns the fold reads (1..): EXPR's result, ANY / ALL / ROWCOMPARE's into `ids` (their paramIds), MULTIEXPR's
    /// into `ids` (its setParams).
    tfs: PgVec<'g, TypeFacts>,
    ids: PgVec<'g, i32>,
    test: Option<ExId>,
    /// updatable_views#978: a check option's ANY / ALL test reads its outer Vars from the written row (`FedRow`).
    row_fed: Option<FedRow<'g>>,
    ex: &'static ExprFace,
    /// ARRAY: the SubResult object's handle word (its accumulator, one per calling lane: RT-2b `m8_array_*`).
    acc: Option<u16>,
    /// A by-reference EXPR or MULTIEXPR column's copy (PG's `curTuple`), one per column: the SubResult object's handle word
    /// and the type's typlen.
    copy: PgVec<'g, Option<(u16, i16)>>,
    /// Frame words (byte offsets): the value, its flags, whether a row came this call, a NULL test seen, a row decided.
    val: i32,
    flags: i32,
    found: i32,
    unk: i32,
    decided: i32,
    /// D3374: lent, no effect, no raise but an EXPR's second row (decompose `quiet_but_rows`): an island's read may run it
    /// where PG skips it, that row held (`fold`).
    holds: bool,
    /// D2117: the licences the inner's kept feeders run once ahead of the SubPlan's first evaluation under (`make`), one
    /// per feeder that runs ahead. Held, not read.
    #[allow(dead_code)]
    ahead: PgVec<'g, gravity_decompose::effects::Licence>,
}

/// updatable_views#978 (D1112 row B): the written row a check option's ANY / ALL test reads, laid by the host in the
/// parked call after the parameters in the plan's layout (hosts write_check.rs `lay_row`): the frame word holding the
/// call's pointer (rowop_sub.rs `serve` stores it), the parameters' count, and each attno the test reads with its type.
struct FedRow<'g> {
    call: i32,
    params: usize,
    cols: PgVec<'g, (i16, TypeFacts)>,
}

const FED_ROW: &str = "a check option's ANY / ALL SubPlan whose test reads other than the written row's columns, or one past its 120th (D1112 row B)";

/// updatable_views#978: the columns compiled test `test` reads (every scan Var of its program; a whole-row Var is its
/// columns, expr.wholerow), read from the call the host laid (word Open 5 of op `j`: rowop_sub.rs names the same).
fn fed_row<'g>(px: &mut Px<'g, '_>, k: usize, j: usize, (test, params): (ExId, usize), ex: &ExprFace, node: i32) -> FedRow<'g> {
    let mcx = px.mcx();
    let mut atts: PgVec<'g, (i16, u32)> = PgVec::new_in(mcx);
    for n in px.placed(test).c.prog.nodes.iter() {
        let gravity_expr::ExprNode::Var(v) = n else { continue };
        if v.src != gravity_expr::Source::Scan || !(1..=120).contains(&v.attno) {
            shape(node, FED_ROW);
        }
        if !atts.iter().any(|a| a.0 == v.attno) {
            atts.push_in((v.attno, v.typ));
        }
    }
    let call = px.frame_word(WordKey::Open { face: Face::Nest, k, j: crate::frame::px::key_j(j), i: 5 }) as i32 * 8;
    FedRow { call, params, cols: atts.into_iter().map(|(a, t)| (a, ex.0.tf_of(px.gx, t))).collect_in(mcx) }
}

/// A hashed SubPlan's key set (a Distinct end, graph.rs `set_kind`): nest/subplan_hash.rs.
fn make_set<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    crate::ops::subplan::hashed::make_end(px, k)
}

/// Columns 1..=n of the SubPlan's tuple: the first entries of the inner's last projection.
fn entries<'g, 'p>(ip: &PipelineNode<'p>, n: usize, mcx: Mcx<'g>) -> Option<PgVec<'g, Node<'p>>> {
    let list = ip.ops.iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None })?;
    let out: PgVec<'g, Node<'p>> = list.iter().take(n).map(|e| e.as_target_entry().map_or(e, |te| te.expr)).collect_in(mcx);
    (out.len() == n).then_some(out)
}

/// The SubPlan nodes of `plan_id` the ops after `j` read (PG evaluates each reference); a write's lists count as two.
pub(crate) fn references(p: &PipelineNode<'_>, j: usize, plan_id: i32) -> usize {
    let count = |x: Node<'_>| uses(x, plan_id);
    p.ops.iter().skip(j + 1).map(|op| match op {
        Op::Filter(q) | Op::Gate(q) => count(*q),
        Op::Project(l) => l.iter().map(count).sum(),
        Op::Join(jd) => jd.qual().into_iter().chain(jd.keys()).flat_map(|l| l.iter()).map(count).sum(),
        Op::Write(w) if gravity_decompose::ops::shared::modifytable::refers(w, plan_id) => 2,
        _ => 0,
    }).sum()
}

/// How many times expression `x` reads SubPlan `plan_id`; an implicit-AND list (a Result's one-time qual) counts its
/// clauses (`expr::walk` has no List arm).
pub(crate) fn uses(x: Node<'_>, plan_id: i32) -> usize {
    crate::stack_check();
    if let Some(l) = x.as_list() {
        return l.iter().map(|c| uses(c, plan_id)).sum();
    }
    let mut n = 0;
    gravity_decompose::expr::walk(x, &mut |y| {
        n += y.as_sub_plan().is_some_and(|s| s.plan_id == plan_id) as usize;
        true
    });
    n
}

const ORDER: &str = "a VOLATILE or raising SubPlan after a VOLATILE or raising node its reader evaluates first, where it cannot be read lazily (an aggregate's argument, a hashed probe, a join or window list: D1911)";

/// D1911: PG evaluates a SubPlan where its reader reaches the node, after the target entries before it (resjunk keys too),
/// a call's earlier arguments and the earlier conjuncts; in an aggregate's argument after the aggregates before it in
/// transition order (each one's FILTER, arguments, then its transition call) and its own FILTER and earlier arguments. The
/// eager step runs it ahead of them all, which shows (rows, call counts, a sequence value, the error raised) when the
/// SubPlan and such a node may each call a VOLATILE function or raise. `true`: the step is read lazily (B2c) by the
/// projection, filter or gate it precedes; any other reader or site refuses by name (a window argument is window.rs's).
/// A conditional SubPlan is lazy already; an acting SubPlan in a ProjectSet's target list is lazy (D2021: per output row and
/// in the ending call); a quiet one there and any argument of a hashed or mixed head's grouping-sets spool (`set_spool`, a
/// VOLATILE input's spool: the fallback runs each once per set, D2090) keep the eager step (`false`).
fn order(px: &mut Px<'_, '_>, k: usize, j: usize, d: &gravity_decompose::ops::shared::subplan::SubPlanDesc<'_>, node: i32) -> bool {
    let (gx, arena) = (px.gx, px.arena);
    let (p, id) = (&gx.pipelines()[k], d.plan_id);
    if d.conditional || !d.effects().acts && !d.effects().may_raise || d.site == SubSite::AggArg && (p.ops.iter().any(|o| matches!(o, Op::Window(_))) || d.set_spool) {
        return false;
    }
    // the lazy hook compiles the arguments and test without the aggregate row (step/expr_ext_lazy.rs `emit_subplan`)
    let agg = |n: Node<'_>| {
        let mut hit = false;
        gravity_decompose::expr::walk(n, &mut |x| {
            hit |= matches!(x.node_tag(), types_nodes::NodeTag::T_Aggref | types_nodes::NodeTag::T_WindowFunc | types_nodes::NodeTag::T_GroupingFunc);
            !hit
        });
        hit
    };
    let (mut earlier, mut road, mut pset, mut at, mut trans) = (Earlier::default(), matches!(d.form, SubForm::Nested { .. } | SubForm::Hashed { .. }) && d.site == SubSite::Row && !d.args.iter().chain(d.test).any(agg), false, At::Fold, Earlier::default());
    let mut whole = false;
    let found = if d.site == SubSite::AggArg {
        // the group whose aggregates read it and where (`At`): a streamed group's (`Op::GroupRun` after the step) before the
        // breaker group the pipeline ends in (a streamed group may feed an outer aggregate in the same pipeline), else past
        // a grouping-sets phase's spool (an empty-key sort the sets' groups read in its lifetime)
        let ps = gx.pipelines();
        let stream = p.ops[j + 1..].iter().find_map(|o| if let Op::GroupRun(i) = o { gx.graph.streams.get(*i as usize) } else { None });
        let spooled = p.desc.produces().filter(|_| matches!(p.ops.last(), Some(Op::Sort(s)) if s.keys.is_empty()));
        let arm = |slot: u8| (k + 1..ps.len()).filter(|&q| matches!(ps[q].source, Source::Breaker { slot: x } if x == slot) && gx.life_of(q, slot) == Some(gravity_program::LifeId(k as u16))).find_map(|q| gx.group_at(q));
        let group = match (stream.filter(|g| g.aggs.iter().any(|a| uses(a.node, id) > 0)), gx.group_at(k), spooled) {
            (Some(g), ..) => {
                at = At::Stream;
                Some(g)
            }
            (None, Some(g), _) => Some(g),
            (None, None, Some(slot)) => {
                at = At::Spool;
                // PG reads the phase whole (as the spool walk does) when no one-time qual may stop it, and every set is a
                // plain fold (its first row needs every input row) or a keyed set whose node's output PG reads whole
                // (`SetDecl::whole`: a Sort above it). A LIMIT that may count 0 (`SubPlanDesc::stops`, D2115) runs none of
                // it: step.limit's feeder stops this pipeline before its first row then, or refuses (result/gate.rs
                // `SPOOL_ZERO`)
                let mut sets = (k + 1..ps.len()).filter(|&q| matches!(ps[q].source, Source::Breaker { slot: x } if x == slot) && gx.life_of(q, slot) == Some(gravity_program::LifeId(k as u16))).filter_map(|q| gx.group_at(q)).peekable();
                whole = !ps.iter().any(|p| p.ops.iter().any(|o| matches!(o, Op::Gate(_)))) && sets.peek().is_some() && sets.all(|g| g.set.map_or(true, |s| s.whole));
                arm(slot)
            }
            (None, None, None) => None,
        };
        let mut aggs: PgVec<'_, _> = group.map_or(&[][..], |g| &g.aggs[..]).iter().filter_map(|a| a.node.as_aggref()).collect_in(gx.mcx);
        aggs.sort_by_key(|a| a.aggtransno);
        let hit = agg_reads(gx, arena, p, &aggs[..], id, &mut earlier, &mut trans);
        // a spooled phase's groups read the computed columns: the walk's projection reads the SubPlan
        match at {
            At::Spool => group.is_some() && p.ops[j + 1..].iter().any(|o| matches!(o, Op::Project(l) if l.iter().any(|n| uses(n, id) > 0))),
            _ => hit,
        }
    } else {
        p.ops[j + 1..].iter().filter(|o| !matches!(o, Op::SubPlan(_))).any(|op| {
            let (hit, lazy) = match op {
                Op::Filter(q) | Op::Gate(q) => (before(gx, arena, p, *q, id, &mut earlier), true),
                Op::Project(l) => (l.iter().any(|n| before(gx, arena, p, n, id, &mut earlier)), true),
                // D2013 slice 2: a nest's parameters first (decompose facts/hoist.rs: a SubPlan hoisted out of its inner's
                // index key), as PG's rescan evaluates the inner's runtime keys after the outer values, before any qual
                Op::Join(jd) => (matches!(jd, gravity_decompose::join::JoinDesc::Nest { params, .. } if gravity_decompose::facts::hoist::nest_values(params).any(|v| before(gx, arena, p, v, id, &mut earlier))) || jd.keys().into_iter().chain(jd.qual()).any(|l| l.iter().any(|n| before(gx, arena, p, n, id, &mut earlier))), false),
                Op::Window(w) => (w.tlist.iter().any(|n| before(gx, arena, p, n, id, &mut earlier)), false),
                // a ProjectSet's target list reading it (decompose subplan.rs `reads`) is its reader: PG evaluates a plain entry per
                // output row and once more in the call that ends the sets (srf/project_set.rs `ending_call`), so an acting SubPlan
                // is read lazily there (D2021); a quiet one keeps the eager step
                Op::Srf(s) if s.place == SrfPlace::TargetList && gravity_decompose::ops::shared::subplan::refers_list(s.tlist, id) => {
                    pset = true;
                    (true, true)
                }
                // a FROM function's arguments reading it (decompose subplan.rs `reads`): PG evaluates them once per scan, before
                // its first row, each function's in turn after the functions before it ran whole (ExecMakeTableFunctionResult
                // per function of a ROWS FROM): a reader that cannot read lazily, the step before it
                Op::Srf(s) if s.place != SrfPlace::TargetList && s.functions.iter().any(|f| gravity_decompose::ops::shared::subplan::refers(f.node, id)) => {
                    (s.functions.iter().any(|f| before(gx, arena, p, f.node, id, &mut earlier)), false)
                }
                // an op between the step and its reader: what it evaluates is not read here
                _ => {
                    earlier = Earlier { loud: true, acts: true };
                    (false, true)
                }
            };
            road &= !hit || lazy;
            hit
        })
    };
    if pset {
        return d.effects().acts;
    }
    // a window function's argument or FILTER on the partition buffer's fill (decompose subplan `attach_window_args`): no op
    // of the fill reads it; the window end stores it in the row's argument cell, which the object reads where PG's frame
    // reads the row. One that may raise rides the fill under that function's recheck licence (an error before any row left
    // re-runs the statement in plan order); order inside a row and which error are free (PLAN-v5 5.1); one that acts is
    // never placed there (D1232)
    if !found && d.site == SubSite::Row && !d.effects().acts && window_fill(gx, k) {
        return false;
    }
    // D3374: an acting one there is the window's one acting argument SubPlan, placed where PostgreSQL evaluates it once per
    // stored row in the buffer's order (decompose `attach_window_args`): the fill evaluates it per row in that order when
    // nothing else of the statement acts or raises among its calls (effects `window_fill_exact`; the window's admission
    // asks it too and refuses by name)
    if !found && d.site == SubSite::Row && d.effects().acts && window_reader(gx, k).is_some_and(|q| crate::effects::window_fill_exact(gx, arena, q, k).is_ok()) {
        return false;
    }
    // an aggregate's argument is read by its fold (agg trans/args.rs `with_args`, step::expr::emit), lazily: per input row
    // as that aggregate folds, after the transitions before it (D1911) and after a streamed group's boundary emit, so an
    // early stop never evaluates it for the next group's first row (D2089); a raising one with nothing loud before stays eager.
    // Not a hashed probe: its set is built ahead of the fold, where PG builds it at the probe's first evaluation, after that
    // row's earlier transitions (sum(nextval(..)) before bool_and(x IN (SELECT nextval(..) ..)) counts differently: D2089
    // H7), so a hashed one keeps D1911's rule below. A streamed group (D1071) folds the row after handing the last group on,
    // where an eager step would run it for the next group's first row, which PG fetches but never advances when a LIMIT
    // above stops at that group: read lazily there. A grouping-sets phase's spool walk computes the arguments into its run in
    // PG's per-row order (gsets.rs `precompute`), so its projection reads it lazily there, when PG reads the phase whole too
    // (`whole` below); but every transition runs after every row's arguments, so a transition that acts, or raises beside an
    // acting SubPlan, shows. A hashed one refuses in both.
    if d.site == SubSite::AggArg && found && at != At::Fold {
        if matches!(d.form, SubForm::Hashed { .. }) || at == At::Spool && (trans.acts || d.effects().acts && trans.loud) {
            shape(node, ORDER);
        }
        // DECISIONS 4 (D) b: a spool PG may not read whole (a set that stops early, a one-time qual) evaluates the argument
        // for rows PG never reaches. A SubPlan that only raises runs ahead there under the unchecked licence (nothing in
        // the statement acts, no user code feeds the pipeline; its error tagged); one that acts keeps the refusal
        if at == At::Spool && spool_ahead(px, k, d, whole).is_none() {
            shape(node, ORDER);
        }
        return true;
    }
    if d.site == SubSite::AggArg && found && at == At::Fold && !matches!(d.form, SubForm::Hashed { .. }) {
        return d.effects().acts || earlier.loud;
    }
    // a raising SubPlan that calls nothing VOLATILE keeps its eager step under a reader that cannot read lazily when nothing
    // before it calls one either: order inside a row and which error are free (PLAN-v5 5.1). After a VOLATILE call PG runs
    // the call, then raises, and its effect stays (nextval is not rolled back: D2089 H2, H8, H9, H12), so that one needs
    // PG's place, as an acting SubPlan does
    if found && (!earlier.loud || !road && !d.effects().acts && !earlier.acts) {
        return false;
    }
    if !(found && road) {
        shape(node, ORDER);
    }
    true
}

/// D2089 (`order`): an aggregate argument SubPlan a grouping-sets phase's spool walk computes into its run, where
/// PostgreSQL may not read the phase whole (`whole` false: a set that stops early, a one-time qual), is evaluated for
/// rows PostgreSQL never reaches. Construct C2: the licence of that run ahead, `Px::ahead_unchecked`'s (asked with the
/// SubPlan's effects: one that acts is denied; one that may raise is tagged); `Some(None)` where PostgreSQL reads the
/// phase whole too (no row ahead); `None` when denied (`order` refuses by `ORDER`).
fn spool_ahead(px: &mut Px<'_, '_>, k: usize, d: &gravity_decompose::ops::shared::subplan::SubPlanDesc<'_>, whole: bool) -> Option<Option<gravity_decompose::effects::Licence>> {
    if whole {
        return Some(None);
    }
    px.ahead_unchecked(k, d.effects()).map(Some)
}

/// Whether pipeline `k` is a WindowAgg's partition buffer fill: a reader whose source is `k`'s breaker runs the window op.
fn window_fill(gx: &crate::graph::Gx<'_, '_>, k: usize) -> bool {
    window_reader(gx, k).is_some()
}

/// The window pipeline whose buffer pipeline `k` fills, if any.
fn window_reader(gx: &crate::graph::Gx<'_, '_>, k: usize) -> Option<usize> {
    gx.pipelines().iter().enumerate().position(|(q, r)| {
        matches!(r.source, Source::Breaker { slot } if gx.life_of(q, slot) == Some(gravity_program::LifeId(k as u16))) && r.ops.iter().any(|o| matches!(o, Op::Window(_)))
    })
}

/// D1911, an aggregate's argument (`SubSite::AggArg`): whether an aggregate of pipeline `k`'s group reads SubPlan `id`
/// (`found`), and whether PG evaluates a node that may raise or call a VOLATILE function before that read (`loud`), by
/// `agg_reads`. rerun/params_key.rs `beside` for a statement holding a computed index key; `order` walks the group it
/// finds for the step (a stream's, a fold's or a spooled phase's arm).
pub(crate) fn agg_arg(gx: &crate::graph::Gx<'_, '_>, k: usize, id: i32) -> (bool, bool) {
    let arena = gravity_program::Arena::new(1 << 12); // alloc: Cold
    let mut earlier = Earlier::default();
    let mut aggs: PgVec<'_, _> = gx.group_at(k).map_or(&[][..], |g| &g.aggs[..]).iter().filter_map(|a| a.node.as_aggref()).collect_in(gx.mcx);
    aggs.sort_by_key(|a| a.aggtransno);
    let found = agg_reads(gx, arena.handle(), &gx.pipelines()[k], &aggs[..], id, &mut earlier, &mut Earlier::default());
    (found, earlier.loud)
}

/// A group's aggregates `aggs`, in transition order: whether one reads SubPlan `id` (its FILTER or arguments); `earlier`
/// gathers what PG evaluates before that read (the aggregates before it, each one's FILTER, arguments, then its
/// transition call, and its own FILTER and earlier arguments), `trans` what every row-wise transition may do. A
/// transition effects calls quiet (`agg_quiet`: count, the integer sums, min / max of a by-value or numeric key, ...)
/// never raises; `acts` is a transition that calls a VOLATILE function (D2089); a DISTINCT or ORDER BY aggregate's
/// transition and direct arguments run at the final call, unless its input comes sorted (then a DISTINCT's equality test
/// runs too).
fn agg_reads(gx: &crate::graph::Gx<'_, '_>, arena: gravity_program::ArenaRef, p: &PipelineNode<'_>, aggs: &[&types_nodes::primnodes::Aggref<'_>], id: i32, earlier: &mut Earlier, trans: &mut Earlier) -> bool {
    let mut found = false;
    for &a in aggs {
        let row = crate::effects::visible::subplan::per_row_transition(a);
        let loud = row && (!a.aggdistinct.is_nil() || !gravity_decompose::effects::agg_quiet(a));
        let acts = row && gravity_decompose::effects::of_agg(gx.facts, a.aggfnoid).acts;
        trans.loud |= loud;
        trans.acts |= acts;
        if !found {
            found = a.aggfilter.is_some_and(|q| before(gx, arena, p, q, id, earlier)) || a.args.iter().any(|n| before(gx, arena, p, n, id, earlier));
            earlier.loud |= !found && loud;
            earlier.acts |= !found && acts;
        }
    }
    found
}

/// Where an aggregate argument's SubPlan is folded: by a breaker's group, a streamed group's `Op::GroupRun` after the step,
/// or past a grouping-sets phase's spool (a sort that keeps its input's order, read by the sets' groups).
#[derive(Clone, Copy, PartialEq, Eq)]
enum At {
    Fold,
    Stream,
    Spool,
}

/// What the nodes PG evaluates before a SubPlan's read may do: `loud`, call a VOLATILE function or raise (`own`); `acts`,
/// call a VOLATILE function (`acts`). A node or op the walk does not know counts for both.
#[derive(Clone, Copy, Default)]
struct Earlier {
    loud: bool,
    acts: bool,
}

/// Whether node `n` reads SubPlan `id`; `earlier` gathers `own` and `acts` over the nodes PG evaluates before that read
/// (children first, in order; GREATEST / LEAST compares between them), over all of `n` when it does not read it; a node
/// the walk does not know counts.
fn before(gx: &crate::graph::Gx<'_, '_>, arena: gravity_program::ArenaRef, p: &PipelineNode<'_>, n: Node<'_>, id: i32, earlier: &mut Earlier) -> bool {
    crate::stack_check(); // D3832
    let is = |s: Node<'_>| s.as_sub_plan().is_some_and(|s| s.plan_id == id);
    if is(n) || n.as_alternative_sub_plan().is_some_and(|a| a.subplans.iter().any(is)) {
        return true;
    }
    if let Some(l) = n.as_list() {
        return l.iter().any(|c| before(gx, arena, p, c, id, earlier));
    }
    let mut found = false;
    let known = gravity_decompose::expr::kids(n, &mut |c| found = found || before(gx, arena, p, c, id, earlier));
    if !found || n.as_min_max_expr().is_some() {
        earlier.loud |= !known || crate::effects::visible::subplan::own(gx, arena, p, n);
        earlier.acts |= !known || crate::effects::visible::subplan::acts(gx, arena, p, n);
    }
    found
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let gx = px.gx;
    let mcx = px.mcx();
    let p = &gx.pipelines()[k];
    let node = nest_inner::op_node(p, j);
    let Some(Op::SubPlan(d)) = p.ops.get(j) else { bug(node, "a nest.subplan step over another op") };
    // D1112: a SubPlan in a write's own lists (RETURNING, a check, ON CONFLICT, a MERGE action) is B4's (decompose marks the
    // write's members, subplan.rs `write_hit`), but for one its check options read (W6-D1112): lazy and host-fed, read at the
    // host qual's read (write/rowop_sub.rs); one of the rows the write reads is this kind's as any
    let w = p.ops.iter().find_map(|o| if let Op::Write(w) = o { Some(w) } else { None });
    let fed = w.is_some_and(|w| w.members.has(gravity_decompose::ops::shared::modifytable::Member::WriteSubPlan) && w.checks.iter().any(|c| refers(c.qual, d.plan_id)));
    // IN-CONFLICT (modifytable): a nested one only an INSERT's ON CONFLICT DO UPDATE reads (a SET entry, the WHERE) is lent
    // with no row: the conflict arm (write.onconflict) reads it where ExecOnConflictUpdate's ExecQual / ExecProject reaches
    // it, over the arm's row (the existing row by attno, EXCLUDED as the inner), its arguments compiled over that row; a
    // MULTIEXPR (D1531's junk entry) too (writesubplan): the arm reads it after the WHERE and before every SET entry, as
    // ExecBuildUpdateProjection's setup steps run each MULTIEXPR SubPlan of the list ahead of the entries reading its Params.
    // D3627: a hashed one too (hashed.rs's probe, its left side compiled over the arm's row): PG's ExecHashSubPlan probes
    // where the arm reaches it, its set built LAZY at that first probe unless proven raise-free with no VOLATILE call
    let conflict = !fed && matches!(d.form, SubForm::Nested { .. } | SubForm::Hashed { .. }) && w.is_some_and(|w| w.conflict_only(d.plan_id));
    // D1112 (writesubplan, B4's MERGE part): a nested, conditional one only MERGE actions read (graph::lazy_nested_sub,
    // WriteDesc::action_only, as merge/admit.rs admits it) is lent with no row too: write.merge
    // reads it where ExecMergeMatched / ExecMergeNotMatched's ExecQual / ExecProject reach it, over the action's row (the
    // target by attno, the child's row as the inner), its arguments compiled over that row, never over the chain's row
    let in_action = !fed && !conflict && crate::graph::lazy_nested_sub(&p.ops[j]).is_some() && w.is_some_and(|w| w.action_only(d.plan_id));
    let reader = conflict || in_action;
    // one only MERGE actions read is conditional (decompose `attach`: an action's
    // WHEN qual and target list run only for the rows it takes), so lent lazily at the action's read (site.rs `mode`);
    // whether the MERGE takes a SubPlan is write.merge's admission (merge/admit.rs)
    let id = d.plan_id;
    // D1112 (writesubplan, B4's RETURNING part): one the RETURNING list reads stands after the write (decompose `place`: the
    // write op reads none), so it runs per written row over the row write.returning hands on, after the row op, as
    // ExecProcessReturning evaluates the list; a MERGE's (write.merge's continuation) or one over several targets keeps B4
    // (writesubplan: a MERGE's too when only its RETURNING list reads it, none of the pre-write lists (a check, ON CONFLICT,
    // an action: decompose `refers`); write.returning `merge_ret_steps` admits the shape)
    let after = w.is_some_and(|w| {
        let oc = w.on_conflict.as_ref().is_some_and(|oc| refers_list(oc.set, id) || oc.filter.is_some_and(|f| refers(f, id)));
        let act = w.merge.as_ref().is_some_and(|m| m.actions.iter().any(|a| refers_list(a.tlist, id) || a.quals.is_some_and(|q| refers_list(q, id))));
        let ret_only = w.returning.is_some_and(|r| refers_list(r, id)) && !oc && !act && !w.checks.iter().any(|c| refers(c.qual, id));
        (w.merge.is_none() || ret_only) && w.targets.len() == 1
    }) && p.ops[..j].iter().any(|o| matches!(o, Op::Write(_)));
    let b4 = w.is_some_and(|w| {
        let oc = w.on_conflict.as_ref().is_some_and(|oc| refers_list(oc.set, id) || oc.filter.is_some_and(|f| refers(f, id)));
        let action = w.merge.as_ref().is_some_and(|m| m.actions.iter().any(|a| refers_list(a.tlist, id) || a.quals.is_some_and(|q| refers_list(q, id))));
        oc || w.checks.iter().any(|c| refers(c.qual, id)) || w.returning.is_some_and(|r| refers_list(r, id)) && !after || action && !d.conditional
    });
    if !fed && !conflict && b4 {
        shape(node, "a SubPlan in a write's own expressions, a check, RETURNING, ON CONFLICT or a MERGE action (D1112: nest.subplan B4)");
    }
    if matches!(d.form, SubForm::Build) {
        return mcx::pg_box_dyn!(mcx, crate::ops::subplan::hashed::Pass => dyn StepMech + 'g);
    }
    // PG's ProjectSet evaluates its target list per output row and again in the call that ends the sets (srf/project_set.rs
    // `ending_call`), which reads the SubPlan before this step
    if p.ops[..j].iter().rev().find(|o| !matches!(o, Op::SubPlan(_))).is_some_and(|o| matches!(o, Op::Srf(s) if s.place == SrfPlace::TargetList)) {
        shape(node, "a SubPlan in a ProjectSet's target list (nest.subplan B2c: per output row and in the ending call)");
    }
    // D1911: read where the reader reaches it when a node PG evaluates first there makes the order show (`order`); a
    // host-fed one (W6-D1112) is read at the host qual's read, in PG's order there, never ahead of it; V4R-499's sites are
    // read where PG's reader reaches them (site.rs `ours`)
    // MULTIEXPR: PG runs it before every entry of its projection, so no entry's order shows
    let ordered = !fed && !reader && !site::ours(d) && d.link != SubLink::MultiExpr && order(px, k, j, d, node);
    let link = match d.link {
        SubLink::Exists => Link::Exists,
        SubLink::Expr => Link::Expr,
        SubLink::Any => Link::Any,
        SubLink::All => Link::All,
        SubLink::RowCompare => Link::RowCompare,
        SubLink::Array => Link::Array,
        SubLink::MultiExpr => Link::MultiExpr,
        SubLink::Cte => bug(node, "a CTE link on a per-row SubPlan"),
    };
    if crate::frame::instr::on() {
        shape(node, "an instrumented SubPlan (EXPLAIN ANALYZE's loop counters)");
    }
    // B2c: evaluated where its reader reaches it, per reference, as PG's interpreter does: under a conditional expression,
    // and a VOLATILE one read in several places; V4R-499: over a join qual's pair, a VALUES row's source row
    let lend = if reader { Lend::Reader } else { site::mode(p, j, d, fed, node) };
    // a reader-lent one (IN-CONFLICT's arm, a MERGE action's: merge#340) is read ahead of its SET entries by its reader
    if link == Link::MultiExpr && lend != Lend::Eager && !reader {
        shape(node, "a MULTIEXPR SubPlan not evaluated ahead of its projection's entries");
    }
    if let SubForm::Hashed { slot } = d.form {
        // D1911 / D3376: a row's hashed probe after a VOLATILE or raising node its projection, filter or gate evaluates first
        // is read lazily there (B2c, `order`), as a conditional one is (D3451): PG's ExecHashSubPlan runs where the reader
        // reaches it, its LAZY set built at that first probe
        let lend = if ordered && lend == Lend::Eager { Lend::Row } else { lend };
        // B2 x W6-D1112: a check option's probe reads the left sides the host laid over the row as written
        return crate::ops::subplan::hashed::make_probe(px, k, j, d, (slot, lend, fed, conflict), node);
    }
    let Some(ik) = gx.pipelines().inner_of(k, j) else { bug(node, "a nested SubPlan without its embedded inner") };
    // an inner spanning several pipelines re-runs its rerun set at each open (nest/inner.rs, PG's chgParam); a pipeline outside
    // the set keeps its rows, which only a VOLATILE call can tell from PG's per-node rescan rule (D1337)
    let set_at = crate::rerun::sets::sub_set_index(gx, k, j, d.plan_id);
    let set = set_at.map(|i| &gx.graph.reruns[i]);
    // construct C2: the licence its place evaluates it ahead of PostgreSQL's demand under (decompose
    // `SubPlanDesc::ahead`: a window argument's on the partition fill, a hoisted key value's on a nest's outer row)
    // covers the SubPlan's own code: stamped for the run-ahead tracer on its inner and its rerun set's members, never
    // on pipeline `k`, whose other code it does not cover (a hashed SubPlan's set holds hashed.rs's build licence)
    if let Some(l) = d.ahead {
        for q in core::iter::once(ik).chain(set.iter().flat_map(|r| r.pipelines.iter().map(|&q| q as usize))) {
            px.hold(q, &l);
        }
    }
    if fed && crate::lifetime::subplan::kept_feeder(gx, k, ik, set.map_or(&[][..], |r| &r.pipelines[..]), false, None) {
        shape(node, "a check option SubPlan whose inner reads a pipeline built once (a build, a Material, an aggregate: D1112 row B)");
    }
    // the keep rule's owner: the set's, else the SubPlan at its op's node (decompose records answers for an empty set too)
    let owner = set.map_or((crate::rerun::inner::op_node(&gx.pipelines()[k], j), d.plan_id), |r| (r.node_id, r.plan_id));
    if d.effects().acts && d.params.iter().next().is_some() && crate::lifetime::subplan::kept_feeder(gx, k, ik, set.map_or(&[][..], |r| &r.pipelines[..]), true, Some(owner)) {
        shape(node, "a correlated VOLATILE SubPlan whose inner reads a pipeline kept across calls (PG re-runs a plain Agg or an unsorted-rewind Sort by its rescan rule: D1337)");
    }
    // D2117: a pipeline outside the set (and all that feeds it) runs once, at the statement's start, where PG 18.6 runs it
    // at the SubPlan's first evaluation, so never under an empty outer or an unreached CASE branch. Read by decompose's
    // effects (`feed_effects`), one with any effect departs, read as code that may raise (an acting feeder is D1337's and
    // build_class's above), ahead of PG's order under v5 errorhold's recheck licence (`Px::run_ahead`; code with no effects
    // departs from nothing): its error before any row left re-runs the statement in
    // plan order, where the licence is denied and this name answers. Denied (an acting statement, a user function, a
    // pipeline that is not ahead of the statement's rows: a cursor's), it refuses by name
    // A kept hash build is not asked here: its raising side is hashjoin's (a lazy build at the first probe row, its licence
    // or its name), and a licence taken for a quiet build sent an error raised elsewhere to the plan-order run, where this
    // name refused it (gen rr-0248 / 0256 / 0263: count(*) / 0 over a join with a kept build). Nor is a hashed SubPlan's
    // set: hashed.rs builds it LAZY at its first probe (PG's order) unless its plan is granted the eager build outright
    // (`lazy_build`: no recheck, so it raises nothing) (sentinel rsc-0314 / 0318: NOT IN / IN over a set in the inner).
    // Nor a plan Material's fill (nest.spool, `spool_fill`): its pull form is PostgreSQL's lazy Material, its eager form
    // admits only a fill that cannot raise (e2e m8_subplan's ALL under the rewinding Material, m8_d1109's D1337 name)
    let kept = kept_feeders(gx, ik, set.map_or(&[][..], |r| &r.pipelines[..]), mcx);
    // D2117 (b): feeders that act (no licence covers them) depart from nothing when PG certainly runs them first
    let own = set.map_or(&[][..], |r| &r.pipelines[..]);
    let certain = if kept.iter().any(|&x| feed_effects(px, x).acts) { certain_first(px, (k, j), lend, &kept[..], (ik, own), d.plan_id) } else { None };
    let kept = if certain.is_some() { PgVec::new_in(mcx) } else { kept };
    let build = |x: usize| {
        let ops = &gx.pipelines()[x].ops;
        matches!(ops.last(), Some(Op::Join(gravity_decompose::join::JoinDesc::Build { .. })))
            || ops.iter().any(|op| matches!(op, Op::SubPlan(s) if matches!(s.form, SubForm::Build)))
            || crate::pipes::spool_fill(gx, x).is_ok()
    };
    // D3378 (m8_d1018_kept_run K6b): a feeder quiet by PG 18.6's reading (effects.rs `fill_quiet`: a sort of quiet
    // keys over a quiet scan, which `feed_effects` reads as unknown) departs from nothing either: no licence, so an error
    // the SubPlan's own steps raise at its first evaluation is PG's, raised there, with no plan-order run that refuses
    let quiet = |x: usize| crate::effects::fill_quiet(gx, x);
    // `DECISIONS.md` 4 (D) b: where no recheck takes the feeder (the plan-order run, rows that may leave before it), it runs
    // ahead unchecked (`Px::ahead_unchecked`, asked with its effects: an unknown op acts) when nothing in it hides a call
    // from the side class that licence reads user code by (`effects::hides_calls`), its error tagged. Each feeder that
    // runs ahead holds the licence it was granted (`Sub::ahead`; a quiet one the no-effects licence)
    let mut ahead: PgVec<'g, gravity_decompose::effects::Licence> = certain.flatten().into_iter().collect_in(mcx);
    for &x in kept.iter().filter(|&&x| !build(x)) {
        use gravity_decompose::effects::{unobservable, Effects};
        let l = if quiet(x) {
            unobservable(Effects::NONE)
        } else {
            let f = feed_effects(px, x);
            px.run_ahead(x, if f == Effects::NONE { Effects::NONE } else { Effects::RAISES }).or_else(|| if crate::effects::hides_calls(px.gx, x, true) { None } else { px.ahead_unchecked(x, f) })
        };
        let Some(l) = l else {
            shape(node, "a SubPlan whose inner reads a pipeline run once ahead of the SubPlan's first evaluation that can raise, its error not held (PG runs it at the first evaluation: D2117)");
        };
        // construct C2 (b): the run-ahead tracer reads it on the feeder (run_ahead / ahead_unchecked hold theirs; the quiet
        // feeder's no-effects licence is held here)
        px.hold(x, &l);
        ahead.push_in(l);
    }
    // a member whose rows another reader than a plain aggregate's emit or a member's hash probe takes (a set operation's
    // sides, a join's drain, a run) is re-run by no proven road: its reader's protocol or position runs once per execution
    if set.is_some_and(|r| !crate::rerun::sets::members_close(gx, k, &r.pipelines)) {
        shape(node, "a SubPlan's rerun set over a pipeline read by other than a plain aggregate (a set operation, a join build, a run: D1337)");
    }
    let ip = &gx.pipelines()[ik];
    let ex: &'static ExprFace = crate::frame::iface::expr_face(&node);
    if d.args.len() != d.params.len() {
        bug(node, "a SubPlan whose arguments are not its parameters");
    }
    let args: PgVec<'g, (i32, ExId)> = d.params.iter().zip(d.args.iter()).filter(|_| !fed).map(|(id, a)| (id, if reader { ex.0.compile(px, a, None, node) } else { ex.0.compile_at(px, (k, j), a, None, node) })).collect_in(mcx);
    let ids: PgVec<'g, i32> = if link == Link::MultiExpr { d.sets.iter().collect_in(mcx) } else { d.ids.iter().collect_in(mcx) };
    let n = match link {
        Link::Exists => 0,
        Link::Expr | Link::Array => 1,
        _ => ids.len(),
    };
    let Some(cols) = entries(ip, n, mcx) else { shape(node, "a SubPlan whose inner projects fewer columns than it reads") };
    let tfs: PgVec<'g, TypeFacts> = cols.iter().map(|&e| ex.0.tf_of(gx, ex.0.out_type(gx, e, node))).collect_in(mcx);
    // ARRAY: the accumulator copies each element as it comes (PG's accumArrayResult), so the element's window may close
    let acc = (link == Link::Array).then(|| sr::array_acc(px, k, node, ex.0.out_type(gx, cols[0], node)));
    // an EXPR's or a MULTIEXPR's by-reference value outlives the inner's window (a scan's page, a Result's scratch): the
    // runtime keeps a copy per lane until the step's next call (RT-2a's SubResult, PG's curTuple); ANY / ALL / ROWCOMPARE
    // read theirs in the walk
    let copy: PgVec<'g, Option<(u16, i16)>> = match link {
        Link::Expr => tfs.iter().take(1).map(|tf| sr::copy_slot(px, k, node, tf)).collect_in(mcx),
        Link::MultiExpr => tfs.iter().map(|tf| sr::copy_slot(px, k, node, tf)).collect_in(mcx),
        _ => PgVec::new_in(mcx),
    };
    // updatable_views#978: a check option's ANY / ALL test compiles over the written row the host lays (`fed_row`)
    let fed_test = fed && matches!(link, Link::Any | Link::All);
    let test = match (link, d.test) {
        (Link::Exists | Link::Expr | Link::Array | Link::MultiExpr, _) => None,
        (_, Some(t)) if n > 0 => Some(if reader || fed_test { ex.0.compile(px, t, None, node) } else { ex.0.compile_at(px, (k, j), t, None, node) }),
        _ => bug(node, "an ANY / ALL / ROWCOMPARE SubPlan without its test or paramIds"),
    };
    let row_fed = test.filter(|_| fed_test).map(|t| fed_row(px, k, j, (t, d.params.iter().count()), ex, node));
    // a Result without a child (its one-time qual, then its projection) reads only parameters: compiled at this op
    let once = matches!(ip.source, Source::Const) && matches!(&ip.ops[..], [.., Op::Project(_)]) && ip.ops[..ip.ops.len() - 1].iter().all(|op| matches!(op, Op::Filter(_) | Op::Gate(_)));
    let body = if once {
        let quals = ip.ops.iter().filter_map(|op| if let Op::Filter(q) | Op::Gate(q) = op { Some(*q) } else { None });
        let tests: PgVec<'g, ExId> = quals.flat_map(|q| q.as_list().map_or_else(|| mcx::pg_vec![in mcx; q], |l| l.iter().collect_in(mcx))).map(|q| ex.0.compile_at(px, (k, j), q, None, node)).collect_in(mcx);
        let cols: PgVec<'g, ExId> = cols.iter().map(|&e| ex.0.compile_at(px, (k, j), e, None, node)).collect_in(mcx);
        Body::Once { tests, cols, rerun: set_at.map(|i| crate::rerun::sets::Rerun::declare(px, k, i)) }
    } else {
        Body::Scan(nest_inner::make(px, k, j))
    };
    // v5 subplan 12: the NO-WIDEN names leave the SubPlan's set (D1339 row C's keyed emits too, D3883 / D3904); a hash-build
    // set D1337(A) serves whose member another rerun set also runs (D2300) still refuses by name. Checked after the inner is
    // made (rerun `declare`: D1654 and the set's own Shapes name their statements first)
    if let Some(why) = set.and_then(|r| crate::rerun::sets::owner_refusal(px, k, &r.pipelines)) {
        shape(node, why);
    }
    sr::rescan(px, k, ik, if let Body::Scan(i) = &body { i.site() } else { None });
    let mut word = |i: u8| px.frame_word(WordKey::Open { face: Face::Nest, k, j: crate::frame::px::key_j(j), i }) as i32 * 8;
    let (val, flags, found, unk, decided) = (word(0), word(1), word(2), word(3), word(4));
    // D2246: a window function's argument is read by the window step only where PG's function reads it (window/lower.rs
    // `fill_cells`, `m10_win_reads`), so it is evaluated there, never ahead at every row
    let window_arg = d.site == SubSite::AggArg && p.ops[j..].iter().find(|o| !matches!(o, Op::SubPlan(_))).is_some_and(|o| matches!(o, Op::Window(_)));
    let lend = if (ordered || window_arg) && lend == Lend::Eager { Lend::Row } else { lend };
    let holds = !fed && d.quiet_but_rows && matches!(link, Link::Exists | Link::Expr | Link::Any | Link::All | Link::Array);
    mcx::pg_box_dyn!(mcx, Sub { node, plan_id: d.plan_id, link, lend, early: site::Early::default(), fed, args, body, tfs, ids, test, row_fed, ex, acc, copy, val, flags, found, unk, decided, holds, ahead } => dyn StepMech + 'g)
}

/// D3506 EXISTS SORT (rerun/epq/sub_lock.rs `redo_subs`): SubPlan op `j` of pipeline `k` made again as a step of its
/// own (`make`: its inner and the inner's steps made again), for a recheck in another pipeline's function: its plan id
/// and step.
pub(crate) fn redo_sub<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> (i32, PgBox<'g, dyn StepMech + 'g>) {
    let id = match px.gx.pipelines()[k].ops.get(j) {
        Some(Op::SubPlan(d)) => d.plan_id,
        _ => bug(nest_inner::op_node(&px.gx.pipelines()[k], j), "a redo SubPlan step over another op"),
    };
    (id, make(px, k, j))
}

/// D2117: the graph pipelines in no rerun set that the inner `ik` reads first, through the members of `set`, by the
/// producers its reads name (`Gx::life_of`: an inner-owned inner's breaker is an inner too, not the graph pipeline before
/// the SubPlan's): each runs once, with all that feeds it, at the statement's start. The walk passes through an embedded
/// inner and a pipeline of another rerun set (a nested SubPlan's own set, run by its owner within this inner's run: psql's
/// \dp policies over pg_roles, regress rowsecurity#39), and skips a CTE's write outside every rerun set (pulled by its
/// reader, at the inner's run).
fn kept_feeders<'g>(gx: &crate::graph::Gx<'_, '_>, ik: usize, set: &[u16], mcx: Mcx<'g>) -> PgVec<'g, usize> {
    let ps = gx.pipelines();
    let reads = |q: usize| producers(gx, q);
    let mut todo: PgVec<'g, usize> = mcx::to_vec_in(&reads(ik)[..], mcx);
    let (mut seen, mut kept): (PgVec<'g, usize>, PgVec<'g, usize>) = (PgVec::new_in(mcx), PgVec::new_in(mcx));
    while let Some(p) = todo.pop() {
        if p == ik || seen.contains(&p) {
            continue;
        }
        seen.push_in(p);
        let rerun = gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(p as u16)));
        if gx.cte_write(p) && !rerun {
            continue;
        }
        if !set.contains(&(p as u16)) && p < ps.len() && !rerun {
            kept.push_in(p);
            continue;
        }
        todo.extend_in(reads(p));
    }
    kept
}

/// D2117: what kept feeder `x` runs, with all that feeds it, by decompose's effects: each pipeline's source (a breaker's
/// rows are its producer's), its ops (`of_ops`: filters, gates, projections, nested loops; a full Sort by its keys'
/// comparators, `effects::sort_compare`, as aggtrans' fold_ahead reads a feed's; any other unknown), its group's aggregate
/// calls and FILTERs (`of_expr`, as aggtrans' fold_ahead reads them); unknown for a grouped one (its keys' hash and
/// equality calls) and for an end that compares or hashes rows (a TopK, a Distinct, a hash build): a plain aggregate's or
/// a Materialize's end, a Sort's whose full sort op the ops read (D3374: a kept merge outer or a LIMIT's ordered input
/// over built-in comparators departs from nothing), or none, adds nothing.
fn feed_effects(px: &Px<'_, '_>, x: usize) -> gravity_decompose::effects::Effects {
    effects_of(px, x, true)
}

/// D2117 (b) (D3374, subplan_d1337_kept_answer E): kept feeders `kept` that PostgreSQL certainly runs at this
/// SubPlan's first evaluation, after nothing that could be seen against them, so running them ahead shows what
/// PostgreSQL shows, whatever they call, write or raise. Every row reaching op `j` of graph pipeline `k` reads the
/// SubPlan there (`Lend::Eager`); `k` runs once (no rerun set); its source makes at least one row quietly (a VALUES
/// list's rows or a Result's one row, every cell `calm`) and its ops before `j` hand each row on quietly (projections of
/// `calm` expressions), so PostgreSQL's first row evaluates the SubPlan before anything else of `k` acts or raises; every
/// row of `k` is pulled (`pulled`, no Limit); and every other graph pipeline before `k` outside the feeders and what feeds
/// them and outside the SubPlan's own inner and rerun set `(ik, own)` (they run at each evaluation, after the feeders in
/// both engines) runs nothing that acts or raises (`effects_of` its own source, ops, end and group), so nothing v4 runs
/// before `k` is ordered against the feeders. A reader that stops early is delegated (`whole_read`). `None`: not
/// certain; else the whole-read licence of the feeders' run (`shared::whole::read` over their effects: granted where
/// every act in the statement is a call-local builtin's), or, for any other act, which PG runs at the same point all the
/// same, the exact licence of the SubPlan-first proof (`first_stamp`); `Some(None)` where neither holds.
fn certain_first(px: &mut Px<'_, '_>, (k, j): (usize, usize), lend: Lend, kept: &[usize], (ik, own): (usize, &[u16]), plan_id: i32) -> Option<Option<gravity_decompose::effects::Licence>> {
    use gravity_decompose::effects::Effects;
    let gx = px.gx;
    let ps = gx.pipelines();
    let calm = |n: Node<'_>| crate::effects::raise_free(gx, n) && !crate::effects::user_or_volatile(gx, n) && !crate::effects::user_typed(gx, n);
    let Some(p) = (k < ps.len()).then(|| &ps[k]) else { return None };
    let rerun = |q: usize| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(q as u16)));
    let rows = match &p.source {
        Source::Values { lists, .. } => lists.iter().next().is_some() && lists.iter().all(|l| l.as_list().is_some_and(|l| l.iter().all(calm))),
        Source::Const => true,
        _ => false,
    };
    let ops = p.ops[..j].iter().all(|o| matches!(o, Op::Project(l) if l.iter().all(calm)));
    // every row of `k` is pulled: no Limit in the program (PG's LIMIT 0 pulls none), `k` an InitPlan's, a CTE's or a
    // lazily read build's never, its rows handed to the statement's sink, or to a full Sort whose drain hands them there
    use crate::graph::EndKind;
    let init = |q: usize| gx.graph.init_plans.iter().any(|i| i.pipeline as usize == q);
    let sink = |q: usize| matches!(gx.end_kind(q), EndKind::Sink) && !init(q) && !rerun(q);
    let drain = |q: usize| (q + 1..ps.len()).any(|r| matches!(ps[r].source, Source::Breaker { slot } if ps[q].desc.produces() == Some(slot)) && sink(r));
    let pulled = match gx.end_kind(k) {
        EndKind::Sink => sink(k),
        EndKind::Breaker(BreakerKind::Sort) => drain(k) && !init(k) && !gx.cte_write(k),
        _ => false,
    };
    let limit = (0..ps.len()).any(|q| crate::frame::iface::holds_limit(&ps[q].ops));
    let mut feeds: PgVec<'_, usize> = mcx::to_vec_in(&kept[..], gx.mcx);
    let mut at = 0;
    while let Some(&q) = feeds.get(at) {
        at += 1;
        let more: PgVec<'_, usize> = producers(gx, q).into_iter().filter(|x| !feeds.contains(x)).collect_in(gx.mcx);
        feeds.extend_in(more);
    }
    let quiet = |e: Effects| !e.acts && !e.may_raise;
    let others = (0..k).filter(|&q| !feeds.contains(&q) && q != ik && !own.contains(&(q as u16))).all(|q| quiet(effects_of(px, q, false)));
    let ok = lend == Lend::Eager && !rerun(k) && rows && ops && pulled && !limit && others && kept.iter().all(|&x| x < k);
    if !ok {
        return None;
    }
    let fed = kept.iter().fold(Effects::NONE, |e, &x| e.and(feed_effects(px, x)));
    // construct C2 (b): the kept feeders and what they read (`feeds`) run first under that licence, asked with their
    // effects (`feed_effects` follows the producers): the run-ahead tracer reads it on each, not on `k` alone. Where
    // whole_read denies the feeders (an act that is not a call-local builtin's: a writing user function), the structural
    // proof this function's reading rests on (`Proof::subplan_first`, re-checked from the statement) licenses them under
    // `exact_first` (AHEAD-EXACT); the fill is the same either way
    let l = crate::ops::shared::whole::read(px, k, fed).or_else(|| first_stamp(px, plan_id, &feeds[..], fed));
    if let Some(l) = l {
        for &q in &feeds {
            px.hold(q, &l);
        }
    }
    Some(l)
}

/// `certain_first`'s licence where whole_read denies its feeders: the exact licence of the SubPlan-first proof.
fn first_stamp(px: &Px<'_, '_>, plan_id: i32, feeds: &[usize], fed: gravity_decompose::effects::Effects) -> Option<gravity_decompose::effects::Licence> {
    use gravity_decompose::effects::{exact_first, Cx, Proof};
    let s = px.gx.graph.stmt?;
    let cx = Cx::new(s, px.gx.facts, px.arena);
    let p = Proof::subplan_first(&cx, px.gx.graph, plan_id, feeds)?;
    exact_first(fed, p).ok()
}

/// `feed_effects`'s walk over pipeline `x`, through its producers when `follow` (the effects home's `pipeline_effects`).
fn effects_of(px: &Px<'_, '_>, x: usize, follow: bool) -> gravity_decompose::effects::Effects {
    crate::effects::pipeline_effects(px.gx, px.arena, x, follow)
}

/// D2117: the pipelines whose rows pipeline `q` reads (the effects home's `producers`).
fn producers<'g>(gx: &crate::graph::Gx<'g, '_>, q: usize) -> PgVec<'g, usize> {
    crate::effects::producers(gx, q)
}

/// A SubPlan step's verdict into its frame words for the row: this file's `Sub`, nest/subplan_hash.rs's probe.
pub(crate) trait Eval {
    fn eval(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals);
    /// W6-D1112: host-fed (`Sub::fed`): `eval_at` runs it at every read, never through `unless_again`.
    fn fed(&self) -> bool { false }
    /// D3374 (`SubHolds`): it may run where PG skips it, its second row held (`Env::hold_rows`).
    fn holds(&self) -> bool { false }
}

impl FedRow<'_> {
    /// The row the test reads: column `a` is the host's pair at the call's `params + a - 1` (NULL where its null word is
    /// not 0); the cells live for the parked call, which outlives the inner's walk.
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals) -> RowVals<'e> {
        use crate::cell::{At, Cell, CellHome, CellLife, Nullable, Row};
        let life = CellLife { until: gravity_program::Until::Row, home: CellHome::FrameSlot };
        let w = env.load_word(b, self.call);
        let p = b.conv(ConvOp::IntToPtr, Type::Ptr, w);
        let mcx = env.mcx;
        let mut cols = mcx::pg_vec![in mcx; None; self.cols.iter().map(|c| c.0 as usize).max().unwrap_or(0)];
        let (mut cells, mut vals) = (mcx::reserved_in(self.cols.len(), mcx), mcx::reserved_in(self.cols.len(), mcx));
        for (c, (a, tf)) in self.cols.iter().enumerate() {
            let a = *a;
            cols[a as usize - 1] = Some(c as u16);
            cells.push_in(Cell { rep: gravity_program::Rep::of_storage(tf.typlen, tf.byval), typid: tf.typid, num: gravity_program::NumTypmod::NONE, life, at: At::Slot(c as u16), null: Nullable::Maybe });
            let at = gravity_abi::program::SUB_ARGS + 16 * (self.params + a as usize - 1) as i32;
            let (v, n) = (b.load(Type::I64, p, at), b.load(Type::I64, p, at + 8));
            vals.push_in((v, Some(b.icmp(IntCond::Ne, n, env.zero))));
        }
        RowVals { row: Row { cells }, vals, cols, carry: row.carry.copy_in(env.mcx), next: row.next, at: row.at }
    }
}

impl Sub<'_> {
    fn put(&self, b: &mut FunctionBuilder<'_>, env: &Env, v: Value, null: Value) {
        env.store_word(b, self.val, v);
        let (is_null, not_null) = (b.iconst(Type::I64, SET | NULL), b.iconst(Type::I64, SET));
        let f = b.select(null, is_null, not_null);
        env.store_word(b, self.flags, f);
    }

    /// 21000 at a second row of an EXPR / ROWCOMPARE / MULTIEXPR SubPlan, checked before the row is read, as PG does.
    fn cardinality(&self, b: &mut FunctionBuilder<'_>, env: &Env) {
        let seen = env.load_word(b, self.found);
        let again = b.icmp(IntCond::Ne, seen, env.zero);
        let (l_again, l_first) = (b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_again);
        b.cond_br(again, (l_again, &[]), (l_first, &[]));
        b.switch_to(l_again);
        let flags = HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() };
        let f = b.declare_host_once("m8_raise", &[Type::Ptr, Type::I64], Some(Type::I64), m8::m8_raise as *const () as usize, flags);
        let code = b.iconst(Type::I64, m8::RAISE_CARDINALITY as i64);
        b.call_host(f, &[env.ctx, code], Some(Type::I64));
        let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
        b.br(env.l_raise, &[e]);
        b.switch_to(l_first);
    }

    /// One inner row, its columns `cols` (datum, NULL `Bool`), folded by the link over the outer `row`: the `Bool` that
    /// ends the walk at it, where the link may end there. D3374 `held` (an island's read, `Env::hold_rows`): an EXPR's
    /// second row ends the walk with `HELD` in its flags word, in place of the raise; the island raises it where it reads
    /// the SubPlan.
    fn fold(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, cols: &[(Value, Value)], held: bool) -> Option<Value> {
        let again = (held && self.link == Link::Expr).then(|| {
            let seen = env.load_word(b, self.found);
            b.icmp(IntCond::Ne, seen, env.zero)
        });
        if again.is_none() && matches!(self.link, Link::Expr | Link::RowCompare | Link::MultiExpr) {
            self.cardinality(b, env);
        }
        env.store_word(b, self.found, env.one);
        match self.link {
            Link::Exists => Some(b.bconst(true)),
            Link::Expr => {
                let v = self.copy[0].map_or(cols[0].0, |c| sr::keep(b, env, c, cols[0].0, cols[0].1));
                self.put(b, env, v, cols[0].1);
                if let Some(a) = again {
                    let f = env.load_word(b, self.flags);
                    let h = b.iconst(Type::I64, crate::rerun::params::HELD);
                    let fh = b.int_bin(IntBinOp::Or, f, h);
                    let f = b.select(a, fh, f);
                    env.store_word(b, self.flags, f);
                }
                again
            }
            Link::MultiExpr => {
                for ((&id, &(v, null)), c) in self.ids.iter().zip(cols.iter()).zip(self.copy.iter()) {
                    let v = c.map_or(v, |c| sr::keep(b, env, c, v, null));
                    crate::rerun::params::store(b, env, self.node, id, v, null);
                }
                None
            }
            Link::Array => {
                let n = b.conv(ConvOp::Zext, Type::I64, cols[0].1);
                sr::acc_call(b, env, (self.acc, self.node), AccOp::Push, &[cols[0].0, n]);
                None
            }
            Link::Any | Link::All | Link::RowCompare => {
                for (&id, &(v, null)) in self.ids.iter().zip(cols.iter()) {
                    crate::rerun::params::store(b, env, self.node, id, v, null);
                }
                let Some(test) = self.test else { bug(self.node, "a SubPlan test without its expression") };
                let laid;
                let at = match &self.row_fed {
                    Some(f) => {
                        laid = f.row(b, env, row);
                        &laid
                    }
                    None => row,
                };
                let (v, valid) = self.ex.0.emit(b, env, self.node, at, test);
                let t = b.bconst(true);
                let null = b.int_bin(IntBinOp::Xor, valid, t);
                if self.link == Link::RowCompare {
                    self.put(b, env, v, null);
                    return None;
                }
                let n = b.conv(ConvOp::Zext, Type::I64, null);
                let u = env.load_word(b, self.unk);
                let u = b.int_bin(IntBinOp::Or, u, n);
                env.store_word(b, self.unk, u);
                let want = if self.link == Link::Any { IntCond::Ne } else { IntCond::Eq };
                let truth = b.icmp(want, v, env.zero);
                let hit = b.int_bin(IntBinOp::And, valid, truth);
                let h = b.conv(ConvOp::Zext, Type::I64, hit);
                env.store_word(b, self.decided, h);
                Some(hit)
            }
        }
    }

    /// The link's value past the rows: EXISTS whether one came; EXPR / ROWCOMPARE NULL without one (a row wrote its
    /// value); ANY true / ALL false when a row decided, else false / true, NULL when a test was NULL; MULTIEXPR NULL (PG's
    /// dummy entry), its setParams NULL without a row.
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        let (t, f) = (b.bconst(true), b.bconst(false));
        match self.link {
            Link::Exists => {
                let seen = env.load_word(b, self.found);
                self.put(b, env, seen, f);
            }
            Link::Expr | Link::RowCompare => {
                let seen = env.load_word(b, self.found);
                let none = b.icmp(IntCond::Eq, seen, env.zero);
                let (l_null, l_join) = (b.create_block(&[]), b.create_block(&[]));
                b.cond_br(none, (l_null, &[]), (l_join, &[]));
                b.switch_to(l_null);
                let zero = env.zero;
                self.put(b, env, zero, t);
                b.br(l_join, &[]);
                b.switch_to(l_join);
            }
            Link::MultiExpr => {
                let zero = env.zero;
                self.put(b, env, zero, t);
                let seen = env.load_word(b, self.found);
                let none = b.icmp(IntCond::Eq, seen, env.zero);
                let (l_null, l_join) = (b.create_block(&[]), b.create_block(&[]));
                b.cond_br(none, (l_null, &[]), (l_join, &[]));
                b.switch_to(l_null);
                for &id in self.ids.iter() {
                    crate::rerun::params::store(b, env, self.node, id, zero, t);
                }
                b.br(l_join, &[]);
                b.switch_to(l_join);
            }
            Link::Array => {
                let p = sr::acc_call(b, env, (self.acc, self.node), AccOp::End, &[]);
                self.put(b, env, p, f);
            }
            Link::Any | Link::All => {
                let d = env.load_word(b, self.decided);
                let decided = b.icmp(IntCond::Ne, d, env.zero);
                let u = env.load_word(b, self.unk);
                let unk = b.icmp(IntCond::Ne, u, env.zero);
                let (yes, no) = if self.link == Link::Any { (env.one, env.zero) } else { (env.zero, env.one) };
                let v = b.select(decided, yes, no);
                let null = b.select(decided, f, unk);
                self.put(b, env, v, null);
            }
        }
    }
}

/// Which of an inner row's first `n` columns are a pgrc cell in its site's dictionary (`Rep::Code`) or window
/// (`PackedNum` `Window`, `PackedNumDyn`) form, which only the inner's site reads (D2213).
fn sited_cols<'g>(r: &RowVals, n: usize, mcx: Mcx<'g>) -> PgVec<'g, bool> {
    use gravity_program::{NumForm, Rep};
    (0..n).map(|i| matches!(r.col(i as i16 + 1).map(|c| c.cell.rep), Some(Rep::Code { .. } | Rep::PackedNum { form: NumForm::Window, .. } | Rep::PackedNumDyn))).collect_in(mcx)
}

impl Eval for Sub<'_> {
    fn eval(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) {
        let held = core::mem::take(&mut env.hold_rows) && self.holds;
        let t = b.bconst(true);
        for &(id, e) in self.args.iter() {
            let (v, valid) = self.ex.0.emit(b, env, self.node, row, e);
            let null = b.int_bin(IntBinOp::Xor, valid, t);
            crate::rerun::params::store(b, env, self.node, id, v, null);
        }
        for w in [self.found, self.unk, self.decided] {
            env.store_word(b, w, env.zero);
        }
        if self.acc.is_some() {
            sr::acc_call(b, env, (self.acc, self.node), AccOp::Begin, &[]);
        }
        match &self.body {
            Body::Once { tests, cols, rerun } => {
                rerun.iter().for_each(|r| r.run(b, env));
                let l_done = b.create_block(&[]);
                for &q in tests.iter() {
                    let pass = self.ex.0.pred(b, env, self.node, row, q);
                    let l_on = b.create_block(&[]);
                    b.cond_br(pass, (l_on, &[]), (l_done, &[]));
                    b.switch_to(l_on);
                }
                let mut vals: PgVec<'_, (Value, Value)> = mcx::reserved_in(cols.len(), env.mcx);
                for &c in cols.iter() {
                    let (v, valid) = self.ex.0.emit(b, env, self.node, row, c);
                    vals.push_in((v, b.int_bin(IntBinOp::Xor, valid, t)));
                }
                let _ = self.fold(b, env, row, &vals[..], held);
                b.br(l_done, &[]);
                b.switch_to(l_done);
            }
            Body::Scan(inner) => {
                let x = env.x;
                // an inner over a plain aggregate's emit sets the arena word its folds read (fold/plain.rs `row`); the
                // outer row's hosted folds read their own after it (regress-full aggregates-051)
                let arena = env.agg_arena;
                let r = inner.open(b, env);
                let bad = b.icmp(IntCond::Slt, r, env.zero);
                let l_go = b.create_block(&[]);
                b.cond_br(bad, (x.l_exit, &[r]), (l_go, &[]));
                b.switch_to(l_go);
                let zero = env.zero;
                let site = inner.site();
                inner.walk(b, env, &x, (row.carry.copy_in(env.mcx), zero), &mut |b, env, r| {
                    // D2213 / D3286: env.lx is the owner's site (inner.rs hand_on), which cannot read a pgrc inner cell's
                    // dictionary or window form; such a cell renders under the inner's site first, as the nest join's
                    // `site_free` (nestloop/lower.rs) does
                    let sited = sited_cols(r, self.tfs.len(), env.mcx);
                    let mut early: PgVec<'_, Option<(Value, Value)>> = mcx::pg_vec![in env.mcx; None; self.tfs.len()];
                    if sited.iter().any(|&x| x) {
                        let Some(s) = site else { shape(self.node, "a SubPlan reading a nest inner column in its site's dictionary or window form (an inner without a scan site: D2213)") };
                        let saved = core::mem::replace(&mut env.site, Some(s));
                        {
                            let mut cx = env.lx(b, self.node);
                            for (i, tf) in self.tfs.iter().enumerate().filter(|(i, _)| sited[*i]) {
                                let Some(c) = r.col(i as i16 + 1) else { bug(self.node, "a SubPlan's inner row without its column") };
                                early[i] = Some(rep::datum(&mut cx, c, tf));
                            }
                        }
                        env.site = saved;
                    }
                    let m = env.mcx;
                    let mut cx = env.lx(b, self.node);
                    let mut vals: PgVec<'_, (Value, Value)> = mcx::reserved_in(self.tfs.len(), m);
                    for (i, tf) in self.tfs.iter().enumerate() {
                        if let Some(v) = early[i] {
                            vals.push_in(v);
                            continue;
                        }
                        let Some(c) = r.col(i as i16 + 1) else { bug(self.node, "a SubPlan's inner row without its column") };
                        vals.push_in(rep::datum(&mut cx, c, tf));
                    }
                    env.memo = cx.memo;
                    let leave = self.fold(b, env, row, &vals[..], held);
                    (r.carry.copy_in(env.mcx), leave)
                });
                env.agg_arena = arena;
            }
        }
        self.finish(b, env);
    }

    fn fed(&self) -> bool { self.fed }

    fn holds(&self) -> bool { self.holds }
}

impl StepMech for Sub<'_> {
    fn declare(&mut self, pb: &mut ProgramBuilder) {
        if let Body::Scan(inner) = &mut self.body {
            inner.declare(pb);
        }
    }

    fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        if let Body::Scan(inner) = &self.body {
            inner.prologue(b, env, x);
        }
        site::prologue(self, (self.lend, &self.early), (self.plan_id, self.val, self.flags), env);
    }

    fn epilogue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        if let Body::Scan(inner) = &self.body {
            inner.epilogue(b, env, x);
        }
    }

    /// A scan inner runs its pipeline serially, as a nest's (until nest.parallel); a once inner reads only frame words,
    /// which are each worker's own.
    fn serial(&self) -> bool {
        gravity_program::step_serial(&gravity_program::StepShape::SubPlan { scan: matches!(self.body, Body::Scan(_)) }).is_some()
    }

    /// The ops after the step read the words `(plan_id, value, flags)`. Eager: `eval` writes them for the row here. Lazy
    /// (B2c): the step and its row are lent to the readers (`Env::subplans`' `lent`), which evaluate it where PG's
    /// interpreter reaches the node (`eval_at`), never where it does not (an untaken CASE arm, past a deciding AND / OR).
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        site::lend(self, self.lend, (self.plan_id, self.val, self.flags), b, env, row, next)
    }
}

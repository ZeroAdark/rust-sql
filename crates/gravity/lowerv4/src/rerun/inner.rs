//! The embedded inner of a nest (M8-plan §3.0, Decision 3). Decompose keeps it inside its outer op; pipes.rs indexes it past
//! the graph pipelines (L0-m), so its source and steps are made by their kinds' makers as a pipeline's are, under the
//! outer's program identity (`Px::making`): their frame words and protocols are the outer pipeline's. Its site is its own:
//! the outer source's drafted site is kept aside while the inner's makers draft theirs, and the inner's is numbered here.
//! Per outer row the owner writes the parameter words, then `open` writes the inner's per-open words (the contract's C2:
//! never the function's open) and opens its source (an index inner seeks with keys read from the parameter words, L0-a);
//! `walk` runs its windows and rows inside the outer row, the environment switched to the inner's while its mechanisms
//! lower and back to the outer's for the owner's continuation. The inner site's cursor names the row handed on while the
//! continuation runs, so a stop there resumes into the outer row and walks on past it (or re-enters it when a later
//! expanding step stopped inside it, D688); its windows persist across the stop as any site's do. Its ops: filters,
//! projections, SubPlans, set functions and nested nests (B1b: each inner expanding step sees the inner's later offsets and
//! the outer's; a stop inside one's row re-enters the inner row) and one-time gates (checked at every re-open, not per
//! call). Its source: a scan site; a siteless Result or VALUES (B1b: its position is the inner's own word `Env::pos`) or a
//! store (its own words); or a breaker in the nest's rerun set (nest.rerun: the set re-runs at every re-open, after the
//! gates and before the source opens, under a Memoize at a miss only; a site-less emit keeps its own position, a single
//! emit is one window, whose end completes a Memoize's entry). No program pipeline, section, grant draw or pool wake.
//! Owner M8-NEST.
use gravity_abi::ctx::site_off;
use gravity_abi::site::site;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::{Op, PipelineNode, Source};
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_kernels::util::RESUME_OPEN;
use mcx::{CollectIn, GrowIn, PgBox, PgVec};

use crate::rerun::sets::Rerun;
use crate::face::kinds::{Face, KindId};
use crate::face::source::{Body, SourceMech};
use crate::face::step::{ExprFace, StepMech};
use crate::frame::diag;
use crate::frame::env::Env;
use crate::frame::iface;
use crate::frame::frame::{Next, SourceMaker, StepMaker, Win};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{ExId, Px, WordKey, DRAFT_SITE};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::admit::refusal::{Reason, ShapeWhy};

pub(crate) struct Inner<'g> {
    pub node: i32,
    k: usize,
    source: PgBox<'g, dyn SourceMech + 'g>,
    steps: PgVec<'g, PgBox<'g, dyn StepMech + 'g>>,
    /// The inner's scan site, or `None`: a siteless source keeps its position in its own words (a store's, a rerun set's
    /// breaker emit) or in `pos` (a Result's, VALUES': `Env::pos`), written back through `SourceMech::write_back`.
    site: Option<usize>,
    pos: Option<i32>,
    /// The inner's expanding steps (step, offset word): a stop inside one's row re-enters it (D688).
    expands: PgVec<'g, (usize, i32)>,
    /// The inner's one-time gates (a Result's resconstantqual), `(node, qual)` top-down (D1915: the upper Result's list first,
    /// each list in its order), and the per-open word a failed one sets: PG's ExecResult checks its gate once per rescan
    /// before pulling its child, so they run at every re-open (after the owner wrote its parameters) and a false or NULL one
    /// leaves the inner empty for that outer row. Not the pipeline's gate step: that one runs once per call and ends it all.
    /// D2228: each gate's op index; `at`: each step's (a node above a gate makes its reads first, `StepMech::reopen`).
    gates: PgVec<'g, (i32, ExId, usize)>,
    at: PgVec<'g, usize>,
    shut: Option<i32>,
    ex: &'static ExprFace,
    /// The per-open words the frame stores (a union run's, a word on another base); the re-open's own are `resets`.
    words: OpenWords<'g>,
    /// The inner's per-open frame words, declared at its re-open in the lifetime home (lifetime/nestloop.rs) and stored
    /// there at every re-open.
    resets: crate::lifetime::RunResets,
    /// The inner's rerun set (nest.rerun, M8-RERUN): the breaker it reads is refilled at every re-open, after the gates.
    rerun: Option<Rerun>,
    /// D2611: the inner has gates and its set holds an InitPlan (`sets::holds_init`): each re-open re-arms the InitPlans
    /// before the gates read them (PG's ExecReScan re-arms them at the rescan, before the Result's first call), also when a
    /// gate fails and the set never runs; the set's run after a passing gate re-arms nothing.
    rearm: bool,
    /// nest.memo (M8-MEMO-CTE): the Memoize and its fill step's index in `steps`.
    memo: Option<(usize, crate::ops::memoize::lower::Memo<'g>)>,
    /// D1018 row A: a LIMIT among the steps, the word an outer stop's cursor waits in (nest/inner_limit.rs).
    limit: Option<i32>,
    /// D1018 row B0: a hash probe among the steps (nest/inner_probe.rs): its empty build ends the re-open's walk.
    probe: bool,
    /// A COPY-marked subquery row's copy entry (marks.rs `nest_copy`) and the first step at or past it.
    copy: Option<(crate::rerun::epq::copy_row::CopyEntry<'g>, usize)>,
    /// D1018 (regress aggregates#527): a streamed group's re-entry word (`group_run`): set while the inner's rows run, so a
    /// stop inside the hand-on of the group a row closes re-enters that row (`offsets`), clear while the input's end flushes.
    held: Option<i32>,
    /// EXPLAIN ANALYZE: the inner's nodes below its Memoize or its first one-time gate, whose loops count only where the
    /// re-open opens the source (PG's Memoize pulls its child at a miss only; a false gate never runs its child); the rest
    /// count a loop at every re-open (frame/instr.rs `count_loops`).
    below: PgVec<'g, i32>,
    /// D3373: a WindowAgg inner's fill, run whole at each re-open ahead of PostgreSQL's spool reads, holds the
    /// licence it runs under (window_arm.rs `inner_fill_ok`'s quiet fill, else `inner_fill_ahead`'s unchecked one).
    #[allow(dead_code)]
    ahead: Option<gravity_decompose::effects::Licence>,
}

/// D1018 (aggregates#527): a streamed group among the inner's steps (`held`). Its hand-on reads a stop by the cursor its own
/// row names (ops/group/stream.rs: past, the group handed on; at, kept open), while a stop in the owner's continuation
/// carries the outer row's cursor. Each hand-on of the inner stops into its own map block, which keeps the outer cursor in
/// `slot` and enters the group's exit as past (the row reached the owner); the inner's steps stop into `unmap`, which
/// leaves by the owner's stop exit `stop_in` with the kept cursor. The resumed call re-enters the outer row and the nest
/// walks the inner again from the closing row (`held` set: nest/loop.rs), whose group then opens anew.
#[derive(Clone, Copy)]
struct Held {
    word: i32,
    slot: Value,
    unmap: gravity_ir::Block,
    stop_in: gravity_ir::Block,
}

/// D1018 (regress aggregates#527): op `i` of inner `ip` is a streamed sorted group (`Op::GroupRun`, ops/group/stream.rs:
/// D1071's form, PG's GroupAggregate over its Sort, which decompose keeps where no licence lets the group fold ahead, e.g.
/// a statement that acts: CTAS, INSERT .. SELECT) over a breaker's run the inner's rerun set refills at every re-open (a
/// Sort's), its other ops filters and projections. Its open words close the open group at every re-open, the input's end
/// hands the open group on (`Inner::walk`'s flush, as frame.rs `flush_path`) and a stop inside a hand-on re-enters the row
/// that closed the group (`Held`), as PG's agg_retrieve_direct hands each group on before it advances the next group's row.
/// D3733: or over a relation scan the re-open opens again (PG's GroupAggregate straight over a presorted Index Scan), and
/// in a SubPlan's inner (`sub`: Django's Limit -> GroupAggregate -> Index Scan) a LIMIT past the group (D1018 row A's word:
/// its end ends the re-open's walk inside the hand-on, before the closing row enters the next group, where PG's Limit
/// stops pulling). A SubPlan's walk never stops inside a hand-on; a join's inner LIMIT there is refused (a stop inside the
/// hand-on with both the LIMIT's ends and the group's `Held` re-entry trapped: D3733's cursor witness).
pub(crate) fn group_run(ip: &PipelineNode<'_>, i: usize, sub: bool) -> bool {
    let source = matches!(ip.source, Source::Breaker { .. }) || ip.source.relation().is_some();
    matches!(ip.ops.get(i), Some(Op::GroupRun(_))) && source && ip.ops.iter().enumerate().all(|(x, o)| x == i || matches!(o, Op::Filter(_) | Op::Project(_)) || sub && x > i && iface::limit_bounds(o).is_some())
}

/// An inner with an op that hands on other than its rows (a LIMIT, a probe, a keeping op) beyond the forms the re-open
/// lowers, or over another breaker than a Material or a CTE (a CteScan). D1018 names the shape and its witnesses.
pub(crate) const INNER_BEYOND_FORMS: &str = "a nest inner with a LIMIT, a join or a keeping op, or over a breaker other than a Material (D1018)";
fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

pub(crate) fn op_node(p: &PipelineNode<'_>, j: usize) -> i32 {
    p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id)
}

/// D3776: op `i` of inner `ip` is a Memoize over one scan of filters and projections, every op past it a filter or
/// projection. Its rows at every key are its child's (PG's logical cache mode compares the keys by their equality
/// operators, which the child's index quals use too; binary mode hits only on equal images), so an owner that lowers its
/// continuation once (nestloop/lower.rs `share`) may read the child at every re-open, where a hit's replay would be a
/// second place the continuation is lowered.
fn plain_memo(ip: &PipelineNode<'_>, i: usize) -> bool {
    matches!(ip.source, Source::Index { .. } | Source::Range { .. }) && ip.ops.iter().enumerate().all(|(x, o)| x == i || matches!(o, Op::Filter(_) | Op::Project(_)))
}

/// A Memoize the inner reads through (`plain_memo`): its rows handed on as they come.
struct Through;

impl StepMech for Through {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        next(b, env, row)
    }
}

/// The inner that op `j` of pipeline `k` embeds, made (its makers' refusals recorded per maker in the all-blockers mode).
pub(crate) fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> Inner<'g> {
    make_through(px, k, j, false)
}

/// `make`; `through`: a `plain_memo` Memoize is read through (D3776).
pub(crate) fn make_through<'g>(px: &mut Px<'g, '_>, k: usize, j: usize, through: bool) -> Inner<'g> {
    let gx = px.gx;
    let mcx = px.mcx();
    let outer_node = op_node(&gx.pipelines()[k], j);
    let Some(ik) = gx.pipelines().inner_of(k, j) else { crate::refuse!(outer_node, Reason::Bug { payload: "a nest without its embedded inner" }) };
    let ip = &gx.pipelines()[ik];
    let node = ip.node_id;
    // filters, projections, a one-time gate, SubPlans, set functions, nested nests (their own kinds), rows A / B0's LIMIT and
    // probe; a keeping op refuses; with a Memoize (nest.memo) its row-at-a-time ops only (a replayed row runs those past it)
    let m = crate::ops::memoize::lower::at(ip);
    let through = m.filter(|&i| through && plain_memo(ip, i));
    let m = m.filter(|_| through.is_none());
    let union = crate::pipes::inner_union(gx, ik).filter(|u| u.0 == ik); // S2b: a union's emit streams its arms (nest/inner_arms.rs)
    let refill = m.is_none() && crate::rerun::union_arms::refill_of(gx, ik).is_some_and(|u| u.0 == ik); // S2r: the set refills it
    let kept = m.is_none() && crate::rerun::union_arms::kept_of(gx, ik).is_some_and(|u| u.0 == ik); // S2k: filled once (D3782)
    // D3773: a union filled once ahead of the owner (rerun/union_arms.rs `filled_once`), its record read from 0 per re-open
    let once = m.is_none() && crate::pipes::union_in_inner(gx, ik).is_some_and(|u| u.0 == ik && crate::rerun::union_arms::filled_once(gx, &u));
    // D1483 (regress inherit#759 / #760): a MergeAppend whose arms the owner's rerun set runs again at every re-open (each
    // child Sort and its arm a member: rerun/sets.rs `members_close`), its merged run read from its start (`emit_ok`)
    let merge = m.is_none() && matches!(ip.source, Source::Breaker { slot } if gx.life_of(ik, slot).is_some_and(|l| crate::rerun::sets::in_owner_set(gx, l.0 as usize)));
    // material W-INNER: a WindowAgg as the inner (PG's ExecReScanWindowAgg per re-open), its partition buffer the source,
    // the buffer's fill a member of the owner's set (`emit_ok`): each open restarts the window object (D2346)
    // and its fill read whole by PostgreSQL too or quiet (rerun/spool/window_arm.rs `inner_fill_ok`). D1018 (heldout
    // wcore-0302): under a Memoize too, below it (`op_ok`): a miss runs the set and streams the window's rows into the
    // fill as PG's ExecMemoize pulls its child (the window rescanned by the changed key: ExecReScanWindowAgg, its Sort
    // re-sorted), a hit opens neither; a run condition ends the reader as a LIMIT below the Memoize does (WINDOWAGG_DONE
    // returns no row, so PG's Memoize completes the entry there too)
    let win = matches!(ip.source, Source::Breaker { .. }) && gx.source_kind(ik) == KindId::WindowAgg;
    // D3373: a fill that may raise and calls no user or VOLATILE function runs whole ahead of PostgreSQL's spool under the
    // unchecked licence (window_arm.rs `inner_fill_ahead`; tagged)
    let fill = match ip.source {
        Source::Breaker { slot } => gx.life_of(ik, slot).map(|l| l.0 as usize),
        _ => None,
    };
    // D4237: or a fill the window pulls (the exact request) and the set re-arms (sets.rs `rearms_window_fill`): it runs only
    // as PostgreSQL's spool reads it, so there is no whole run to license
    let pulls = win && fill.is_some_and(|q| crate::ops::window::lower::window_after_args(&ip.ops).is_some_and(|(_, d)| crate::ops::window::lower::pulled(px, d.plan.plan.plan_node_id)) && crate::rerun::sets::rearms_window_fill(gx, ik, q));
    let ahead = if win && !pulls { fill.and_then(|q| crate::rerun::spool::window_arm::inner_fill_ok(gx, q).or_else(|| crate::rerun::spool::window_arm::inner_fill_ahead(px, q))) } else { None };
    if win && !pulls && ahead.is_none() {
        shape(node, crate::rerun::spool::window_arm::W_INNER_LOUD);
    }
    // D1018: the ops the re-open runs again from their start (lifetime/nestloop.rs `reopens`)
    let form = crate::lifetime::nestloop::InnerForm { memo: m, through, merge, arms: union.is_some() || refill || kept || once, win, sub: matches!(gx.pipelines()[k].ops.get(j), Some(Op::SubPlan(_))) };
    if !crate::lifetime::nestloop::reopens(gx, px.arena, (k, j), ik, ip, &form) {
        shape(node, INNER_BEYOND_FORMS);
    }
    // D2227: a probe of the embedding SubPlan's own set where PG's per-call build order would show (inner_probe.rs)
    if let Some(why) = (0..ip.ops.len()).find_map(|i| crate::rerun::inner_probe::own_order(gx, ik, ip, i)) {
        shape(node, why);
    }
    crate::rerun::inner_once::check(gx, k, j, ip, node); // B1b: D1085
    crate::lifetime::nestloop::zero_count(gx, ik, ip, node); // D1903, D1917
    // a breaker's emit as the inner: a Material's spool (nest.spool), or any breaker whose slot is in the nest's rerun set
    // (nest.rerun: PG re-executes it per outer row, so the set refills it at every re-open); another breaker's run or groups
    // would be read in the outer program pipeline's own region, built once
    // the set the re-open runs (rerun/sets.rs `owner_set`; D1337(H): a probe of its build, the inner's source no breaker).
    // D1018 (regress partition_join#205 / #206): a union's emit whose set also holds other pipelines (a hash build over
    // another union the set refills, the probe side these streamed arms) declares the set as any inner does: its run lays
    // them out before the first window, as PG's rescan rebuilds the Hash whose child's chgParam is set; a feed it holds
    // reads nothing (`Empty`), the arms streamed as PG's Append rescans its children
    let arms_only = |set: usize| union.as_ref().is_some_and(|u| gx.graph.reruns[set].pipelines.iter().all(|&q| u.2.contains(&(q as usize))));
    let rerun = match (&ip.source, crate::rerun::sets::owner_set_at(gx, ik)) {
        (Source::Breaker { slot }, Some(set)) if union.is_some() && arms_only(set) => crate::rerun::inner_arms::rerun(px, (k, set, *slot), union.as_ref(), node),
        (_, set) => set.map(|set| Rerun::declare(px, k, set)),
    };
    let kind = match ip.source {
        Source::Breaker { .. } => match gx.source_kind(ik) {
            KindId::NestSpool if rerun.is_some() => shape(node, crate::pipes::REFILL_CHILD),
            KindId::NestSpool => KindId::NestSpool,
            // INNER-SPOOL (drain/lower.rs `inner_spooled`): a RIGHT / FULL drain's spool reader, re-read at every re-open
            KindId::JoinDrain if rerun.is_none() && gx.spooled_inner(ik) => KindId::JoinDrain,
            KindId::SrcAppend if union.is_some() => KindId::SrcAppend,
            // S2r: a union the set refills at every re-open (rerun/union_arms.rs `refilled`), its record read from 0 per re-open
            KindId::SrcAppend if rerun.is_some() && refill => KindId::SrcAppend,
            // D3782 (S2k): a union no set re-runs under a nest with no parameters, its record filled once ahead of the owner
            // and read from 0 per re-open (rerun/union_arms.rs `kept`); D3773: one filled once (`filled_once`) alike
            KindId::SrcAppend if rerun.is_none() && (kept || once) => KindId::SrcAppend,
            // setop_rec-0043: a CteScan over a CTE the set re-arms (ops/ctescan/lower.rs `rearmed_inner`), its store started
            // again by the set's run before every re-open, read from its start (D1018 row 1's reader). Under a Memoize too
            // (subselect#208/209): a hit never opens the source and nothing pulls the re-armed CTE, as PG's hit never
            // rescans its child; a miss reads it from its start as far as the fill hands rows on (ExecReScanMemoize defers
            // the child's rescan, so the CTE's chgParam re-runs it at the miss's first fetch). With no set and no Memoize
            // (subselect#316/317): a nest's plain CteScan inner inside a recursive term over the term's CTE, which the term
            // set re-arms at every generation; its re-opens within one generation rewind over the one store
            KindId::NestCte if (rerun.is_some() || m.is_none()) && crate::ops::ctescan::lower::rearmed_inner(gx, ik) => KindId::NestCte,
            // the emits proven under a re-open (rerun/sets.rs `emit_ok`: a plain aggregate's record, D1339 row B's sort run)
            kind if rerun.is_some() => crate::rerun::sets::emit_ok(px, (k, ik), kind),
            // D1018 row 1: a CteScan. No statement-wide name refuses it: D2059's (a later reader's pull re-reading rows) was
            // D2178's swept scan site, root-fixed by ROOTFIX-CTE-PARK, as D2178 / D2179 / D2180 / D2194 / D2197 / D2213 are
            KindId::NestCte if m.is_none() => KindId::NestCte,
            // D3417 (D2811: a recursive union in another's term, `x, q` joined by a nest): a WorkTableScan inner reads a
            // working table (its own union's, or an enclosing one's), which is swapped only between that union's
            // generations, so it holds still across every re-open; the scan's cursor is a per-open word
            // (ops/setop/recursive.rs `Scan`), so each re-open reads it from its first row, as ExecReScanWorkTableScan
            // rewinds its tuplestore
            KindId::SetopRecursive if rerun.is_none() && m.is_none() => KindId::SetopRecursive,
            // a sort's run filled once ahead of the owner, read from its start at every re-open (rerun/inner_once.rs `kept_run`)
            // D3378: or, under a nest, one whose fill may raise, filled at the nest's first outer row (`kept_run_lazy`)
            KindId::RunsSort if rerun.is_none() && m.is_none() && crate::rerun::inner_once::kept(px, (k, j, ik)) => KindId::RunsSort,
            // D4101: a keyed group's emit (a set operation's left group too) filled once ahead of the owner, gathered again at
            // every re-open under a quiet owner (rerun/inner_once.rs `kept_emit`)
            kind @ (KindId::KeyedEmitGroup | KindId::RunsGroup | KindId::RunsUnique) if rerun.is_none() && m.is_none() && crate::rerun::inner_once::kept_emit(px, (k, j, ik)) => kind,
            _ => shape(node, INNER_BEYOND_FORMS),
        },
        ref s => crate::graph::source_kind_of(s, node),
    };
    let ident = px.ident(k);
    // the outer source's draft is keyed by `ident`, as the inner's is while its makers run: kept aside meanwhile; the
    // owner's site number is held ahead of the inner's (pipes.rs `reserve_site`)
    // a spool's child site is numbered first (nest/spool.rs: exec admit walks the fill pipeline's source ahead)
    let outer = px.take_site(k);
    // D2455: so are the outer's per-code tables (dict/percode.rs `tables`, keyed by `ident` too): the inner's makers would
    // take the outer's, and the inner's bind would fill the outer's table lanes at the inner's site
    let outer_tables = {
        let ext = crate::step::expr::ext(px);
        ext.tables.iter().position(|t| t.0 == ident as u64).map(|i| ext.tables.swap_remove(i))
    };
    let owner = if k >= gx.pipelines().len() { k } else { ident };
    let spool = kind == KindId::NestSpool;
    if outer.is_some() && !spool {
        crate::pipes::reserve_site(px, owner);
    }
    let protos = px.protos.len();
    let ex: &'static ExprFace = iface::expr_face(&node);
    // D2675: the union markers (frame/px.rs `set_union`: a Drain op's, an Append's) are keyed by the identity the makers
    // declare under, which the inner shares with its owner, and index the owner's ops: under a join drain's tail the
    // owner's Drain marker read the inner's LIMIT as union-scoped (`union_of`), a union word in a remade pipeline (Bug).
    // The inner's makers see only the markers its own ops set; those go with them (they index the inner's ops)
    let owner_unions = core::mem::take(&mut px.unions);
    let (source, steps, gates, memo) = px.making(ik, ident, |px| {
        let source = diag::make(px, k, node, |px| (iface::kind_impl::<SourceMaker>(&node, kind).0)(px, ik), diag::source);
        let mut gates = PgVec::new_in(mcx);
        for (i, op) in ip.ops.iter().enumerate() {
            if let Op::Gate(g) = op {
                let n = op_node(ip, i);
                let quals = g.as_list().map_or_else(|| mcx::pg_vec![in mcx; *g], |l| l.iter().collect_in(mcx));
                let mut top: PgVec<'g, (i32, ExId, usize)> = quals.into_iter().map(|q| (n, ex.0.compile_at(px, (ik, i), q, None, n), i)).collect_in(mcx); // D1915: compiled bottom-up, kept top-down
                top.extend_in(gates);
                gates = top;
            }
        }
        let mut memo = None;
        let steps: PgVec<'g, PgBox<'g, dyn StepMech + 'g>> = (0..ip.ops.len())
            .filter(|&i| !matches!(ip.ops[i], Op::Gate(_)))
            .enumerate()
            .map(|(s, i)| {
                let n = op_node(ip, i);
                if Some(i) == m {
                    return crate::ops::memoize::lower::make(px, (k, j), ik, (i, s), &mut memo);
                }
                if Some(i) == through {
                    return mcx::pg_box_dyn!(mcx, Through => dyn StepMech + 'g);
                }
                diag::make(px, k, n, |px| (iface::kind_impl::<StepMaker>(&n, crate::graph::step_kind(&ip.ops[i])).0)(px, ik, i), diag::step)
            })
            .collect_in(mcx);
        (source, steps, gates, memo)
    });
    px.unions = owner_unions;
    if outer.is_some() && spool {
        crate::pipes::reserve_site(px, owner);
    }
    let mut source = source;
    let draft = px.making(ik, ident, |px| {
        let mut d = px.take_site(ik).or_else(|| source.site_desc(px.arena));
        if let Some(d) = d.as_mut() {
            source.seal(px, ik, d);
        }
        d
    });
    let site = draft.map(|d| crate::pipes::place_site(px, ik, d));
    if site.is_some() { px.set_pipeline_site(ik, site) } // D1933: a write over this nest names its target's site (write/nest_write.rs)
    if let Some(d) = outer {
        px.set_site(k, d);
    }
    {
        let ext = crate::step::expr::ext(px);
        ext.tables.retain(|t| t.0 != ident as u64);
        ext.tables.extend(outer_tables); // alloc: Program
    }
    // B1b: a siteless Result or VALUES keeps its position in the inner's own word (`Env::pos`), written RESUME_OPEN at
    // every re-open as a pipeline's open marks its resume word; a store keeps its own (srf/store_read.rs `from`)
    let pos = match (site, &ip.source) {
        (Some(_), _) | (None, Source::TableFunc { .. } | Source::Named { .. }) => None,
        (None, Source::Const | Source::Values { .. }) => Some(px.frame_word(WordKey::Open { face: Face::Nest, k: ik, j: u64::MAX, i: 0 }) as i32 * 8),
        // join#212/213: a RIGHT SEMI / RIGHT ANTI drain over a kept table (drain/source.rs) walks the build's records from
        // the resume position: the inner's own word, back at record 0 at every re-open
        (None, Source::Breaker { .. }) if matches!(ip.ops.first(), Some(Op::Join(JoinDesc::Drain { .. }))) => Some(px.frame_word(WordKey::Open { face: Face::Nest, k: ik, j: u64::MAX, i: 0 }) as i32 * 8),
        (None, _) if rerun.is_some() || spool || union.is_some() || kind == KindId::NestCte => None, // S2b: a union's emit (its arms' sites follow the owner's); a CteScan's read pointer
        (None, Source::Breaker { .. }) if kept || once => None, // D3782 (S2k), D3773: the eager record's read position is its per-open word (material lower.rs `eager`)
        (None, Source::Breaker { .. }) if kind == KindId::RunsSort => None, // a kept run (rerun/inner_once.rs `kept_run`): the emit's cursor is its open word
        (None, Source::Breaker { .. }) if matches!(kind, KindId::KeyedEmitGroup | KindId::RunsGroup | KindId::RunsUnique) => None, // D4101: a kept emit (`kept_emit`): its cursor is its open word, its gather fresh per open
        (None, Source::Breaker { .. }) if kind == KindId::SetopRecursive => None, // D3417: a WorkTableScan's cursor is its open word
        (None, _) => shape(node, "a nest inner without a scan site (a function, values or breaker inner)"),
    };
    let own = pos.is_some() || matches!(ip.source, Source::TableFunc { .. } | Source::Named { .. });
    for p in px.protos[protos..].iter_mut().filter(|p| p.site == DRAFT_SITE) {
        let Some(s) = site.or(own.then_some(usize::MAX)) else { crate::refuse!(node, Reason::Bug { payload: "a site-less nest inner with a drafted site's protocol" }) };
        p.site = if s == usize::MAX { -1 } else { s as i16 };
    }
    if source.covers() != 0 {
        shape(node, "a nest inner whose windows the condition cache covers");
    }
    if source.once() && rerun.is_none() && pos.is_none() {
        shape(node, "a nest inner over a once source (a Result, a breaker's single emit)");
    }
    let mut words = OpenWords::new_in(px.mcx());
    // D1903 (computed road): a LIMIT whose count may be 0 (a parameter, a computed or bind-time bound) over the inner's
    // rerun set reads its count at the re-open before the set runs (`open`): PG's ExecLimit at a zero count never pulls its
    // child (LIMIT_EMPTY), so the set's members (a raising or acting scan below the aggregate) never run. Under a Memoize
    // the hit road keeps the old order (nest.memo), so only without one
    let zero = crate::lifetime::nestloop::count_before_run(ip, rerun.is_some(), memo.is_some());
    let shut = (!gates.is_empty() || zero).then(|| px.frame_word(WordKey::Open { face: Face::Nest, k: ik, j: u64::MAX, i: 1 }) as i32 * 8);
    if let Some(o) = shut {
        words.declare(OpenWord::open("nest inner gate", Base::Words, o, Type::I64, 0));
    }
    if let Some(o) = pos {
        words.declare(OpenWord::open("nest inner position", Base::Words, o, Type::I64, RESUME_OPEN));
    }
    source.open_words(&mut words);
    steps.iter().for_each(|s| s.open_words(&mut words));
    if words.words.iter().any(|w| w.base == Base::State) {
        shape(node, "a nest inner whose steps keep per-open state in the end's slot");
    }
    let Ok(inner) = u16::try_from(ik) else { crate::refuse!(node, Reason::Bug { payload: "a nest inner index past the lifetime scope's range" }) };
    // D3936: the instance is the identity its makers declared under (a spill pair pass's remade SubPlan steps make it again)
    let Ok(at) = u16::try_from(ident) else { crate::refuse!(node, Reason::Bug { payload: "a nest inner's instance id past the lifetime scope's range" }) };
    let resets = match crate::lifetime::nestloop::declare(&mut px.lifetimes, inner, at, &words) {
        Ok(r) => r,
        Err(_) => crate::refuse!(node, Reason::Bug { payload: "a nest inner's per-open word declared in two roles or scopes" }),
    };
    words.words.retain(|w| !crate::lifetime::nestloop::per_open(w));
    let expands = steps.iter().enumerate().filter_map(|(i, s)| s.expand().map(|e| (i, e.offset))).collect_in(mcx);
    // the Nest face's words at the inner: i 0 its position (Env::pos), 1 its gate, 2 a spool's reader, 3 a LIMIT's stop cursor
    // material W-INNER: a WindowAgg's run condition ends its reader as a LIMIT met (ops/window/lower.rs, PG's WINDOWAGG_DONE)
    // psql#479: and a merge join's DONE (`Env::full` once a side is spent, ops/mergejoin/fill_inner.rs `tails`), as PG's
    // ExecMergeJoin returns no tuple there: that re-open's walk ends, never the outer pipeline
    let stops = |op: &Op<'_>| iface::limit_bounds(op).is_some() || matches!(op, Op::Window(d) if !d.run_condition.is_nil()) || matches!(op, Op::Join(JoinDesc::Merge { .. }));
    // D1909 / D2820: a probe that ends at its first row over an empty table takes the same ends (rerun/inner_probe.rs
    // `needs_end`); the steps are made, so `Px::lazy` names a build of this inner that waits for its first row
    let lazy = px.lazy.iter().any(|&(_, q)| q == ik);
    let limit = (ip.ops.iter().any(stops) || crate::rerun::inner_probe::needs_end(gx, ik, ip, lazy)).then(|| px.frame_word(WordKey::Open { face: Face::Nest, k: ik, j: u64::MAX, i: 3 }) as i32 * 8);
    let at = (0..ip.ops.len()).filter(|&i| !matches!(ip.ops[i], Op::Gate(_))).collect_in(mcx);
    let rearm = crate::lifetime::nestloop::rearm_before_gates(gx, rerun.as_ref().map(|r| r.set), !gates.is_empty() || zero);
    let probe = ip.ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Probe { .. })));
    // A `nest_joins` inner (one heap scan under filters and projections) running a COPY-marked subquery
    // row's SubqueryScan takes the row's copy entry: a recheck's re-open is that one row (`copy_walk`); so does a
    // `nest_copy_scan` inner of a write ops/modifytable/nest_ref.rs `chain` admits (a Function Scan's or CTE Scan's COPY row)
    let o = &gx.pipelines()[k].ops[j];
    // D3705: or an UPDATE / DELETE whose target's scan is a later nest's inner (ops/modifytable/nest_write.rs `dml_form`)
    let write = || gx.pipelines()[k].ops.iter().any(|x| matches!(x, Op::Write(w) if crate::ops::modifytable::nest_ref::chain(gx, k, w) || crate::ops::modifytable::nest_write::dml_form(gx, k, w).is_some()));
    let scan = crate::rerun::epq::marks::nest_copy_scan(o) && k < gx.pipelines().len() && write();
    // D3592: or a producer's INNER / LEFT nest whose inner enters a COPY-marked Subquery Scan's row past its own source
    // (rerun/epq/ref_copy.rs `inner_of`: Odoo's last-progress subquery, a LIMIT over a Sort below the entry), which a
    // lock's runner recheck re-opens over the stored row (rerun/epq/marks.rs `producer_copy`), its rerun set's too
    // (`open`: the set never runs under the stored row)
    let sub = || k < gx.pipelines().len() && gx.graph.record_rows.iter().any(|r| crate::rerun::epq::marks::copied(gx, r.scanrelid) && crate::rerun::epq::ref_copy::inner_of(gx, k, j, r.scanrelid).is_some_and(|(x, _, _)| x == ik));
    let plain = (crate::rerun::epq::marks::nest_joins(o) && site.is_some() || scan) && rerun.is_none() || sub();
    let copy = if plain { crate::rerun::epq::copy_row::entry(px, ik) } else { None }.map(|e| {
        let s0 = at.iter().position(|&i| i >= e.boundary).unwrap_or(at.len());
        (e, s0)
    });
    // D1018 (aggregates#527): a streamed group's re-entry word, its step's resume offset (`Held`); over a source with more
    // than one window only (a once source's end is no input end the flush reads), never under a COPY row's entry
    let held = ip.ops.iter().position(|o| matches!(o, Op::GroupRun(_))).map(|i| crate::frame::stop::resume_word(px, crate::frame::stop::ResumeWord::StepOffset { k: ik, j: crate::frame::px::key_j(i) }));
    // psql#479: so does a merge join's ENDOUTER fill (`StepMech::flushes`), at the same input end; over a once source too
    // (rangefuncs#223: a function scan's one window), whose window's end is the input's (`walk`)
    if held.is_some() && source.once() || (held.is_some() || steps.iter().any(|s| s.flushes())) && (copy.is_some() || memo.is_some()) {
        shape(node, INNER_BEYOND_FORMS);
    }
    // (`merge` holds for any breaker source the owner's set runs, a streamed union's last feed too)
    let below = instr_plan(gx, k, ik, rerun.is_none() && !refill && !win && (union.is_some() || !merge), union.as_ref(), m.or(gates.last().map(|g| g.2)));
    Inner { node, k: ik, source, steps, site, pos, expands, gates, at, shut, ex, words, resets, rerun, rearm, memo, limit, probe, copy, held, below, ahead }
}

/// EXPLAIN ANALYZE (frame/instr.rs `plan_inner`): an inner whose rows its own steps hand on (`simple`: a scan or siteless
/// source, a Material's spool (nest.spool), or a union streamed arm by arm (`union`, nest/inner_arms.rs), no rerun set,
/// window or merge reader) counts its nodes at its boundaries, its loops at each re-open; a streamed union's arms each
/// count theirs as they open (`instr::arm_key`: an arm with a one-time gate is not counted); any other inner's nodes are
/// not counted. A spool's reader counts the Materialize as PostgreSQL's ExecMaterial does: one loop per re-open
/// (ExecReScanMaterial per outer row), the rows it hands on (its child's rows are its fill pipeline's own). Returns
/// the nodes below op `split` (a Memoize's, or the lowest one-time gate's), whose loops count only where the source
/// opens.
fn instr_plan<'g>(gx: &crate::graph::Gx<'g, '_>, k: usize, ik: usize, simple: bool, union: Option<&(usize, u8, Vec<usize>)>, split: Option<usize>) -> PgVec<'g, i32> {
    use crate::frame::instr;
    let mcx = gx.mcx;
    if !instr::on() {
        return PgVec::new_in(mcx);
    }
    let ip = &gx.pipelines()[ik];
    let entries = ip.node_ops.iter().map(|n| (n.node_id, n.first_op, n.producer_top)).collect_in(mcx);
    // an arm's own nodes: not the union's, whose feed op ends the arm
    let entries_of = |p: &PipelineNode<'_>| p.node_ops.iter().filter(|n| !entries.iter().any(|e| e.0 == n.node_id)).map(|n| (n.node_id, n.first_op, n.producer_top)).collect_in(mcx);
    let arms: &[usize] = match union {
        Some(u) => &u.2,
        None => &[],
    };
    let spool = matches!(ip.source, Source::Breaker { .. }) && gx.source_kind(ik) == KindId::NestSpool;
    if !simple || (matches!(ip.source, Source::Breaker { .. }) && union.is_none() && !spool) {
        instr::uncounted(entries.iter().map(|e| e.0));
        arms.iter().for_each(|&kf| instr::uncounted(gx.pipelines()[kf].node_ops.iter().map(|n| n.node_id)));
        return PgVec::new_in(mcx);
    }
    // D3814: nest.selfspool's synthetic Materialize (no plan node: its fill runs the inner node's own ops under its id, where
    // a plan Material's fill runs its child's): PostgreSQL rescans that node per outer row, so its loops are this reader's
    if let (true, Source::Breaker { slot }, Some(&(t, _, _))) = (spool, &ip.source, entries.first()) {
        let fill = gx.life_of(ik, *slot).map(|l| &gx.pipelines()[l.0 as usize]);
        if fill.is_some_and(|f| f.node_ops.iter().any(|n| n.node_id == t && !n.producer_top)) {
            instr::reader_owned(t);
        }
    }
    let lead = ip.ops.iter().take_while(|op| matches!(op, Op::Filter(_))).count();
    // the boundaries by step (`chain`'s index): a one-time gate is no step (`Inner::gates`), so an op's step is its index
    // less the gates before it
    let gates = ip.ops.iter().enumerate().filter(|(_, op)| matches!(op, Op::Gate(_))).map(|(i, _)| i).collect_in(mcx);
    let step_of = |op: u16| op - gates.iter().filter(|&&g| g < op as usize).count() as u16;
    let by_step = entries.iter().map(|&(n, op, top)| (n, step_of(op), top)).collect_in(mcx);
    instr::plan_inner(ik, k, &by_step[..], ip.ops.len() - gates.len(), lead);
    // each arm's nodes but the union's own (the arm's last op feeds it), counted where the arm runs (inner_arms.rs `pull`)
    for &kf in arms {
        let fp = &gx.pipelines()[kf];
        let body = fp.ops.len().saturating_sub(1);
        let es = entries_of(fp);
        if fp.ops[..body].iter().any(|op| matches!(op, Op::Gate(_))) {
            instr::uncounted(es.iter().map(|e| e.0));
            continue;
        }
        let lead = fp.ops[..body].iter().take_while(|op| matches!(op, Op::Filter(_))).count();
        instr::plan_inner(instr::arm_key(kf), ik, &es[..], body, lead);
    }
    let Some(split) = split else { return PgVec::new_in(mcx) };
    let owner = entries.iter().rposition(|e| e.1 as usize <= split).unwrap_or(0);
    entries[..owner].iter().map(|e| e.0).collect_in(mcx)
}

/// The environment one side of the nest lowers under: what `enter` swaps in for the inner and `leave` restores.
struct Side<'e> {
    site: Option<usize>,
    k: usize,
    node: i32,
    step: Option<usize>,
    win: Option<Win>,
    expands: PgVec<'e, (usize, i32)>,
    pos: Option<i32>,
    single: Option<usize>,
    inner: Option<PgVec<'e, Option<u16>>>,
    srfs: Option<crate::frame::env::AggRow<'e>>,
    /// The group row's aggregate cells the steps read (`Env::aggs`): an inner's plain aggregate drain sets its own
    /// (agg/trans/plain_drain.rs), so the owner's come back after the walk and in each row's hand-on (sp-0156: a HAVING
    /// over an embedded count(*) read the inner's cells, IR that does not verify).
    aggs: Option<crate::frame::env::AggRow<'e>>,
    mult: Option<Value>,
    tables: Option<Value>,
    codes: PgVec<'e, gravity_expr::emit::CodeRef>,
    body: Body,
    loc: Option<crate::face::source::Locator>,
    /// D1018 row A: `Env::full`'s and a stop's exits while the inner's steps lower (nest/inner_limit.rs `Ends::side`).
    full: Option<crate::rerun::inner_limit::Exit>,
    /// EXPLAIN ANALYZE's node boundaries the side's rows count at (frame/instr.rs: the inner's own while its rows lower,
    /// the owner's in its continuation).
    bounds: Option<crate::frame::instr::Bounds>,
}

/// D3833: the environment's side and `s` exchanged in place. A `Side` is 456 bytes; swapped by value, its copies sat in
/// the frames of the walk's recursion (`Inner::walk`, `Inner::hand_on`), one set per nest level.
#[inline(never)]
fn swap_in<'e>(env: &mut Env<'e, '_>, s: &mut Side<'e>) {
    core::mem::swap(&mut env.site, &mut s.site);
    core::mem::swap(&mut env.k, &mut s.k);
    core::mem::swap(&mut env.node, &mut s.node);
    core::mem::swap(&mut env.step, &mut s.step);
    core::mem::swap(&mut env.win, &mut s.win);
    core::mem::swap(&mut env.expands, &mut s.expands);
    core::mem::swap(&mut env.pos, &mut s.pos);
    core::mem::swap(&mut env.single, &mut s.single);
    core::mem::swap(&mut env.inner, &mut s.inner);
    core::mem::swap(&mut env.srfs, &mut s.srfs);
    core::mem::swap(&mut env.aggs, &mut s.aggs);
    core::mem::swap(&mut env.mult, &mut s.mult);
    core::mem::swap(&mut env.tables, &mut s.tables);
    core::mem::swap(&mut env.codes, &mut s.codes);
    core::mem::swap(&mut env.body, &mut s.body);
    core::mem::swap(&mut env.loc, &mut s.loc);
    s.full = s.full.take().map(|f| crate::rerun::inner_limit::swap_full(env, f));
    s.bounds = crate::frame::instr::replace_current(s.bounds.take());
}

/// `Inner::walk`'s state across its row loop's body, boxed: the walk's frame is one per nest level of the recursion (D3833).
struct Walk<'e> {
    outer: Option<PgBox<'e, Side<'e>>>,
    row: RowVals<'e>,
    memo: crate::lx::SetAside<'e>,
    htypes: PgVec<'e, Type>,
    j: Value,
    past: Value,
    win_end: Value,
    ends: Option<crate::rerun::inner_limit::Ends>,
    held: Option<Held>,
    l_head: gravity_ir::Block,
    l_done: gravity_ir::Block,
    l_fin: gravity_ir::Block,
    l_row: gravity_ir::Block,
    l_wend: gravity_ir::Block,
    l_end: Option<gravity_ir::Block>,
    l_replay: Option<gravity_ir::Block>,
    l_flush: Option<gravity_ir::Block>,
}

impl<'g> Inner<'g> {
    /// D3014O (nest.parallel): the inner's site when the inner is a bare probe of it: no rerun set, LIMIT, hash probe,
    /// COPY row, streamed group or one-time gate, each of which keeps per-execution state a worker would not copy. A
    /// Memoize over the probe may stay (D3014 M2): its table is one per worker ordinal (exec ops/memoize/proto.rs), and
    /// its per-open words (the replay position, the pending row, the purge word) are each participant's own frame words;
    /// its key functions are the leader's resolution (ops/memoize/lower.rs `Memo::widens`: binary mode or one key).
    pub(crate) fn probe_site(&self) -> Option<usize> {
        let memo = self.memo.as_ref().is_none_or(|(_, m)| m.widens());
        let bare = self.rerun.is_none() && memo && self.limit.is_none() && !self.probe && self.copy.is_none() && self.held.is_none() && self.gates.is_empty();
        self.site.filter(|_| bare)
    }

    /// D4532 (nest.parallel over a spool): the inner reads its site-less source with nothing above it that keeps
    /// per-execution state a worker would not copy: no rerun set, Memoize, LIMIT, hash probe, COPY row, streamed group or
    /// one-time gate (`probe_site`'s terms, with no Memoize and no site). Its read position is a per-open frame word.
    pub(crate) fn bare_siteless(&self) -> bool {
        self.site.is_none() && self.pos.is_none() && self.rerun.is_none() && self.memo.is_none() && self.limit.is_none() && !self.probe && self.copy.is_none() && self.held.is_none() && self.gates.is_empty() && self.shut.is_none()
    }

    /// D3014 M2: the inner keeps a Memoize (nest.memo) above its source.
    pub(crate) fn memoized(&self) -> bool {
        self.memo.is_some()
    }

    /// D3776: `walk` hands rows on from its row loop only (no COPY row, Memoize replay, LIMIT, hash probe, streamed group,
    /// or step acting at the input's end), so its owner's continuation is lowered at one place and a walk re-entered past
    /// the row handed on (`skip` 1) resumes exactly after it. D3810: an expanding step that lowers its own continuation
    /// once (`StepMech::hands_on_once`: a nest of a bushy plan) keeps that: the owner re-enters the inner row while one's
    /// offset is set (`skip` 0, as a resumed call does, nestloop/lower.rs `lower_shared`), and the step walks on. Not under
    /// EXPLAIN ANALYZE: a re-entered inner row would count its nodes' rows again.
    pub(crate) fn walks_once(&self) -> bool {
        self.memo.is_none() && self.limit.is_none() && !self.probe && self.copy.is_none() && self.held.is_none() && (self.expands.is_empty() || !crate::frame::instr::on() && self.expands.iter().all(|&(s, _)| self.steps.get(s).is_some_and(|st| st.hands_on_once()))) && !self.steps.iter().any(|s| s.flushes() || s.ends())
    }

    /// The inner's side swapped into `env`; returns the owner's, boxed. Out of line so no `Side` sits in its callers'
    /// frames (D3833).
    #[inline(never)]
    fn enter<'e>(&self, env: &mut Env<'e, '_>, (expands, full, aggs): (PgVec<'e, (usize, i32)>, Option<crate::rerun::inner_limit::Exit>, Option<crate::frame::env::AggRow<'e>>)) -> PgBox<'e, Side<'e>> {
        let mut s = mcx::box_in(Side { site: self.site, k: self.k, node: self.node, step: None, win: None, expands, pos: self.pos, single: None, inner: None, srfs: None, aggs, mult: None, tables: None, codes: PgVec::new_in(env.mcx), body: Body::default(), loc: None, full, bounds: crate::frame::instr::bounds_of(self.k) }, env.mcx);
        swap_in(env, &mut s);
        s
    }

    /// The owner's side `s` (`enter`'s) back into `env`.
    fn leave<'e>(env: &mut Env<'e, '_>, mut s: PgBox<'_, Side<'e>>) {
        swap_in(env, &mut s);
    }

    /// The offset words of the inner's expanding steps: one set means a stop inside that step's row (nest/loop.rs re-enters
    /// the inner row it names).
    pub(crate) fn offsets(&self) -> impl Iterator<Item = i32> + '_ {
        self.expands.iter().map(|e| e.1).chain(self.held) // D1018 (aggregates#527): a streamed group's re-entry word
    }

    /// The words the inner's owner declares at its own open (nestloop/lower.rs `open_words`): a re-run plan's memo purge
    /// word (ops/memoize/lower.rs `owner_words`).
    pub(crate) fn owner_words(&self, w: &mut OpenWords) {
        self.memo.iter().for_each(|(_, m)| m.owner_words(w));
    }

    /// The inner's scan site (D1478: nest.subplan names it to the EvalPlanQual recheck).
    pub(crate) fn site(&self) -> Option<usize> {
        self.site
    }

    pub(crate) fn declare(&mut self, pb: &mut ProgramBuilder) {
        self.source.declare(pb);
        self.steps.iter_mut().for_each(|s| s.declare(pb)); // D1473: a step's own hosts (a SubPlan's index site), as frame.rs
    }

    pub(crate) fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        let none = (PgVec::new_in(env.mcx), None, None);
        let outer = self.enter(env, none);
        self.source.prologue(b, env, x); // D4062: a union's arms' steps (rerun/inner_arms.rs)
        self.steps.iter().for_each(|s| s.prologue(b, env, x));
        Self::leave(env, outer);
    }

    /// The re-open: the inner's per-open words, its gates (each while every earlier one was true; a false or NULL one sets
    /// the gate word and the source is not opened; D2611: the set's InitPlans re-armed first, `rearm`), then its source's
    /// open; the status (negative: leave the row with it).
    pub(crate) fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        let none = (PgVec::new_in(env.mcx), None, None);
        let outer = self.enter(env, none);
        // EXPLAIN ANALYZE: a loop of each node at or above the Memoize or gate at every re-open (`below`'s at the source's)
        crate::frame::instr::count_loops(b, env.ctx, &|n| !self.below.contains(&n), env.mcx);
        if !crate::lifetime::nestloop::reopen(b, &self.resets, env.words, env.params) {
            crate::refuse!(self.node, Reason::Bug { payload: "a frame word without the words site" });
        }
        self.words.write_all(b, self.node, env.ctx, env.state, env.words);
        // Under a recheck's stored COPY row the source never opens (the walk is that row)
        if let Some((e, _)) = &self.copy {
            let (l_copy, l_src, l_open) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64]));
            let on = crate::rerun::epq::copy_row::copying(b, env, e.site.get());
            b.cond_br(on, (l_copy, &[]), (l_src, &[]));
            b.switch_to(l_copy);
            b.br(l_open, &[env.zero]);
            b.switch_to(l_src);
            // D3592: an inner the nest's rerun set refills (Odoo's last-progress subquery, a LIMIT over a Sort) opens
            // as below otherwise; under the stored row the set never runs, as EvalPlanQual never runs the subquery
            let r = match self.rerun.is_some() {
                true => self.open_src(b, env),
                false => {
                    self.source.reopen(b, env);
                    self.source.open(b, env)
                }
            };
            b.br(l_open, &[r]);
            b.switch_to(l_open);
            let r = b.block_params(l_open)[0];
            Self::leave(env, outer);
            return r;
        }
        let r = self.open_src(b, env);
        Self::leave(env, outer);
        r
    }

    /// `open` past a stored COPY row's entry, under the inner's environment: the source's re-open, its gates, the
    /// rerun set's run, its open; the status.
    fn open_src(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        // D2625: PG's rescan resets (a kept table's match flags) before anything the call runs
        self.source.reopen(b, env);
        let Some(o) = self.shut else {
            // nest.memo: a hit never runs the set nor opens the source (PG's Memoize never pulls its child on a hit); a
            // miss runs the set first, as PG's rescan re-executes the breaker below the Memoize
            let mut source = |b: &mut FunctionBuilder<'_>, env: &mut Env| {
                crate::frame::instr::count_loops(b, env.ctx, &|n| self.below.contains(&n), env.mcx);
                self.rerun.iter().for_each(|r| r.run(b, env));
                self.source.open(b, env)
            };
            return match &self.memo {
                Some((_, m)) => m.open(b, env, &mut source),
                None => source(b, env),
            };
        };
        let none = RowVals { row: crate::cell::Row::new_in(env.mcx), vals: PgVec::new_in(env.mcx), cols: PgVec::new_in(env.mcx), carry: Carry::new(env.zero, env.mcx), next: env.zero, at: env.zero };
        let (l_shut, l_open) = (b.create_block(&[]), b.create_block(&[Type::I64]));
        // D2611: the set's InitPlans re-armed before any gate reads one (a failed gate never reaches the set's run)
        if self.rearm {
            self.rerun.iter().for_each(|r| r.rearm(b, env));
        }
        let mut hi = usize::MAX;
        for &(n, q, gi) in self.gates.iter() {
            // D2228: the steps above this gate (and below the last) make their reads first, top-down; a zero count shuts
            for s in self.steps.iter().zip(self.at.iter()).rev().filter(|&(_, &i)| i > gi && i < hi).map(|(s, _)| s) {
                if let Some(stop) = s.reopen(b, env) {
                    let l_more = b.create_block(&[]);
                    b.cond_br(stop, (l_shut, &[]), (l_more, &[]));
                    b.switch_to(l_more);
                }
            }
            hi = gi;
            let (p, l_more) = (self.ex.0.pred(b, env, n, &none, q), b.create_block(&[]));
            b.cond_br(p, (l_more, &[]), (l_shut, &[]));
            b.switch_to(l_more);
        }
        // D1903 (computed road): the steps below every gate make their reads too, top-down, before the set runs; a zero
        // count shuts (the set never runs, the source never opens), as PG's ExecLimit at 0 pulls nothing
        if self.rerun.is_some() {
            for s in self.steps.iter().zip(self.at.iter()).rev().filter(|&(_, &i)| i < hi).map(|(s, _)| s) {
                if let Some(stop) = s.reopen(b, env) {
                    let l_more = b.create_block(&[]);
                    b.cond_br(stop, (l_shut, &[]), (l_more, &[]));
                    b.switch_to(l_more);
                }
            }
        }
        crate::frame::instr::count_loops(b, env.ctx, &|n| self.below.contains(&n), env.mcx);
        self.rerun.iter().for_each(|r| r.run(b, env));
        let r = self.source.open(b, env);
        b.br(l_open, &[r]);
        b.switch_to(l_shut);
        env.store_word(b, o, env.one);
        b.br(l_open, &[env.zero]);
        b.switch_to(l_open);
        b.block_params(l_open)[0]
    }

    /// The inner's position: its site's cursor (the frame's `write_back` without the rows-out count, which only
    /// EXPLAIN ANALYZE reads: an instrumented nest refuses), or a siteless source's own (the frame's `WriteBack::Own`: `Env::pos`, a store's words, a breaker emit's).
    fn set_cursor(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value) {
        let Some(s) = self.site else { return self.source.write_back(b, env, cur, env.zero) };
        let c32 = b.conv(ConvOp::Trunc, Type::I32, cur);
        b.store(env.ctx, site_off(s) as i32 + site::CURSOR as i32, c32);
    }

    /// Row `r` to the owner's continuation (the walk's, a memo replay's), left for `l_done` where it ends the walk.
    fn hand_on<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, (outer, l_done, ends, held): (&mut Option<PgBox<'_, Side<'e>>>, gravity_ir::Block, Option<crate::rerun::inner_limit::Ends>, Option<Held>), on_row: OnRow<'_, 'e>, r: &RowVals) -> Carry<'e> {
        let Some(mut o) = outer.take() else { crate::refuse!(self.node, Reason::Bug { payload: "a nest inner's row handed on twice at once" }) };
        let at = ends.map(|e| e.at_row(env));
        swap_in(env, &mut o); // the owner's side in, `o` holding the inner's
        let memo = env.memo.set_aside();
        let tr = ends.map(|e| e.tr_begin(b, env));
        // D1018 (aggregates#527): a stop in the owner's continuation keeps its cursor and enters the group's exit as past; as
        // at (the group kept open) when a later expanding step of the owner stopped inside its set (its offset word set,
        // D688; regress stats#10): the resumed walk re-enters the closing row, which hands the same group on again for that
        // step to resume inside
        let later = env.expands.iter().filter(|e| env.step.is_some_and(|j| e.0 > j)).map(|e| e.1).collect_in(env.mcx);
        let map = held.map(|h| {
            let l_map = b.create_block(&[Type::I64, Type::I64, Type::I64]);
            b.mark_cold(l_map);
            (h, l_map, core::mem::replace(&mut env.x.l_stop_in, l_map))
        });
        let (c, leave) = on_row(b, env, r);
        if let Some((h, l_map, group)) = map {
            env.x.l_stop_in = group;
            let here = b.current();
            b.switch_to(l_map);
            let p = mcx::to_vec_in(b.block_params(l_map), env.mcx);
            b.store(h.slot, 0, p[1]);
            let to = match later.is_empty() {
                true => r.next,
                false => {
                    let mut set = env.zero;
                    for &o in later.iter() {
                        let w = env.load_word(b, o);
                        set = b.int_bin(IntBinOp::Or, set, w);
                    }
                    let inside = b.icmp(IntCond::Ne, set, env.zero);
                    b.select(inside, r.at, r.next)
                }
            };
            b.br(group, &[p[0], to, p[2]]);
            b.switch_to(here);
        }
        if let (Some(e), Some(t), Some(a)) = (ends, tr, at) {
            e.tr_end(b, env, t, a, r);
        }
        env.memo.restore(memo);
        swap_in(env, &mut o);
        *outer = Some(o);
        if let Some(l) = leave {
            let l_on = b.create_block(&[]);
            b.cond_br(l, (l_done, &c.params()), (l_on, &[]));
            b.switch_to(l_on);
        }
        c
    }

    /// The inner steps' epilogues (`StepMech::epilogue`), under the inner's environment.
    pub(crate) fn epilogue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        let none = (PgVec::new_in(env.mcx), None, None);
        let outer = self.enter(env, none);
        self.source.epilogue(b, env, x); // D4062: a union's arms' steps (rerun/inner_arms.rs)
        self.steps.iter().for_each(|s| s.epilogue(b, env, x));
        Self::leave(env, outer);
    }

    /// The inner's windows and rows from its current position, `skip` (`I64` 0 or 1) rows past the first window's cursor:
    /// the cursor names the row last handed on, so 1 walks on past it and 0 re-enters it (D688). Each row through the
    /// inner's steps into `on_row` under the outer's environment, the inner's cursor at the row first; `on_row` returns the
    /// carry and, when the walk may end at that row, a `Bool` that ends it. Returns the carry at the inner's end or that
    /// early end (the block after is the caller's); a failing status from the inner's windows exits with it. Only the
    /// continuation stops resumably, the inner's cursor naming its row.
    pub(crate) fn walk<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, x: &Exits, (carry, skip): (Carry<'e>, Value), on_row: OnRow<'_, 'e>) -> Carry<'e> {
        crate::stack_check(); // D3832
        let _level = crate::nest_level();
        let mut w = self.walk_head(b, env, x, (carry, skip), &mut *on_row);
        let (j, l_done, ends, held) = (w.j, w.l_done, w.ends, w.held);
        let Walk { row, outer, .. } = &mut *w;
        let c = chain(b, env, &self.steps, 0, row, &mut |b, env, r| {
            self.set_cursor(b, env, j);
            self.hand_on(b, env, (&mut *outer, l_done, ends, held), &mut *on_row, r)
        });
        self.walk_tail(b, env, x, (w, c), on_row)
    }

    /// `walk` up to its row loop's body: the windows' loop, the inner's side entered, the window's row `j` (D3833: out of
    /// line, its state boxed, so the recursion through the body keeps a small frame).
    #[inline(never)]
    fn walk_head<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, x: &Exits, (carry, skip): (Carry<'e>, Value), on_row: OnRow<'_, 'e>) -> PgBox<'e, Walk<'e>> {
        let ctypes = carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let htypes: PgVec<'e, Type> = core::iter::once(Type::I64).chain(ctypes.iter().copied()).collect_in(env.mcx);
        let (l_head, l_win) = crate::frame::cancel::header(b, env.ctx, x.l_cancel, &htypes, env.mcx);
        let l_done = b.create_block(&ctypes);
        // D3274: a step acting at the input's end (`StepMech::ended`) runs there first
        let l_end = self.steps.iter().any(|s| s.ends()).then(|| b.create_block(&ctypes));
        let l_fin = l_end.unwrap_or(l_done);
        self.copy_walk(b, env, (&carry, skip, l_done), &mut *on_row);
        let l_replay = self.memo.as_ref().map(|(_, mo)| mo.enter(b, env, &htypes, (skip, &carry)));
        b.br(l_head, &mcx::concat_in(&[&[skip][..], &carry.params()[..]], env.mcx));
        let memo = env.memo.set_aside();
        // D688: the outer's expanding steps after the nest, past every inner step, so an inner row a stop there re-enters
        // reloads its steps' held values (stop.rs `held`) instead of running a volatile call twice
        // B1b: and the inner's own before them, each step seeing those after it
        let later = env.expands.iter().filter(|e| env.step.is_some_and(|j| e.0 > j)).map(|e| (usize::MAX, e.1));
        let expands = self.expands.iter().copied().chain(later).collect_in(env.mcx);
        // D1018 row A: a LIMIT's end ends this re-open's walk, never the outer pipeline (nest/inner_limit.rs)
        let ends = self.limit.map(|w| crate::rerun::inner_limit::Ends::new(b, env, &ctypes, w));
        let full = ends.map(|e| e.side(env));
        // the inner's steps start over the owner's aggregate cells (an inner that drains its own plain aggregate replaces them)
        let aggs = env.aggs.clone();
        let outer = Some(self.enter(env, (expands, full, aggs)));
        b.switch_to(l_win);
        let p = b.block_params(l_win);
        let (skip, wc) = (p[0], Carry::from_params(&p[1..], env.mcx));
        // D1018 (aggregates#527): a streamed group's stop exits (`Held`): the inner's steps stop into its unmap
        let held = self.held.map(|word| {
            let s = b.declare_stack_slot(8, 8);
            let slot = b.stack_addr(s);
            let unmap = b.create_block(&[Type::I64, Type::I64, Type::I64]);
            b.mark_cold(unmap);
            Held { word, slot, unmap, stop_in: core::mem::replace(&mut env.x.l_stop_in, unmap) }
        });
        let (wd, xi) = crate::rerun::inner_probe::exits(b, x, ends.map(|e| e.exits(x)), self.probe); // D1018 rows A / B0
        // a gate that failed at this re-open: the inner has no row for this outer row (its source was never opened)
        if let Some(o) = self.shut {
            let (v, l_go) = (env.load_word(b, o), b.create_block(&[]));
            let shut = b.icmp(IntCond::Ne, v, env.zero);
            b.cond_br(shut, (l_fin, &wc.params()), (l_go, &[]));
            b.switch_to(l_go);
        }
        self.steps.iter().for_each(|s| s.window(b, env, &xi));
        ends.iter().for_each(|e| e.window(b, &wc));
        wd.iter().for_each(|w| w.seal(b, l_done, &wc));
        let l_eof = b.create_block(&[Type::I64]);
        if held.is_some() || self.steps.iter().any(|s| s.flushes()) {
            crate::frame::frame::end_again(b, env, self.site, l_eof); // D2827: a walk resumed inside its input end's flush
        }
        let win = self.source.next_window(b, env, l_eof);
        env.win = Some(win);
        let l_window = b.current();
        // the inner's end (status 0) ends the walk; any other status exits with it, as a pipeline's window loop does
        b.switch_to(l_eof);
        let r = b.block_params(l_eof)[0];
        let end = b.icmp(IntCond::Eq, r, env.zero);
        // D1018 (aggregates#527): a streamed group's input end flushes it first (`l_flush`, lowered after the rows); so does
        // a merge join's ENDOUTER fill (psql#479, ops/mergejoin/nest_inner.rs)
        let l_flush = (held.is_some() || self.steps.iter().any(|s| s.flushes())).then(|| b.create_block(&ctypes));
        match (&self.memo, l_flush) {
            (Some((_, mo)), _) => mo.done(b, env, (end, r, x.l_exit), (l_done, &wc)),
            (None, Some(l_f)) => b.cond_br(end, (l_f, &wc.params()), (x.l_exit, &[r])),
            (None, None) => b.cond_br(end, (l_fin, &wc.params()), (x.l_exit, &[r])),
        }
        b.switch_to(l_window);
        self.source.fill_late(b, env, &win, None);
        if let Some(h) = held {
            env.store_word(b, h.word, env.one); // a stop in this window's rows re-enters the closing row (`Held`)
        }
        let l_row = b.create_block(&htypes);
        let start = b.int_bin(IntBinOp::Add, win.cursor, skip);
        b.br(l_row, &mcx::concat_in(&[&[start][..], &wc.params()[..]], env.mcx));
        b.mark_loop_header(l_row);
        env.bounded.push_in(l_row);
        b.switch_to(l_row);
        let p = b.block_params(l_row);
        let (j, rc) = (p[0], Carry::from_params(&p[1..], env.mcx));
        let (l_body, l_wend) = (b.create_block(&[]), b.create_block(&ctypes));
        let more = b.icmp(IntCond::Slt, j, win.end);
        b.cond_br(more, (l_body, &[]), (l_wend, &rc.params()));
        b.switch_to(l_body);
        env.memo.clear();
        let past = b.int_bin(IntBinOp::Add, j, env.one);
        let row = self.source.row(b, env, &win, j, rc, past);
        mcx::box_in(Walk { outer, row, memo, htypes, j, past, win_end: win.end, ends, held, l_head, l_done, l_fin, l_row, l_wend, l_end, l_replay, l_flush }, env.mcx)
    }

    /// `walk` past its row loop's body: the windows' end, the Memoize replay, the LIMIT's end, the flush, the inner's end.
    #[inline(never)]
    fn walk_tail<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, x: &Exits, (w, c): (PgBox<'_, Walk<'e>>, Carry<'e>), on_row: OnRow<'_, 'e>) -> Carry<'e> {
        let Walk { mut outer, memo, htypes, past, win_end, ends, held, l_head, l_done, l_fin, l_row, l_wend, l_end, l_replay, l_flush, .. } = PgBox::into_inner(w);
        b.br(l_row, &mcx::concat_in(&[&[past][..], &c.params()[..]], env.mcx));
        b.switch_to(l_wend);
        let p = mcx::to_vec_in(b.block_params(l_wend), env.mcx);
        self.set_cursor(b, env, win_end);
        let zero = env.zero;
        // a once source (a breaker's single emit, a rerun set's) has one window: its end is the inner's (a Memoize's
        // entry completes there, as at a source's end)
        match self.source.once() {
            true => {
                if let Some((_, mo)) = &self.memo {
                    mo.complete(b, env);
                }
                // rangefuncs#223: a flushing step (a merge join's ENDOUTER fill) hands its rows on at that end first
                match l_flush {
                    Some(l_f) if self.memo.is_none() => b.br(l_f, &p),
                    _ => b.br(l_done, &p),
                }
            }
            false => b.br(l_head, &mcx::concat_in(&[&[zero][..], &p[..]], env.mcx)),
        }
        if let Some(l_r) = l_replay {
            self.replay(b, env, x, (l_r, &htypes, l_done, &mut outer), &mut *on_row);
        }
        if let Some(e) = ends {
            e.seal(b, env, x, self.memo.as_ref().map(|m| &m.1), l_done);
        }
        // D1018 (aggregates#527): the input's end hands each holding step's rows on (frame.rs `flush_path`), the inner's
        // cursor kept at its end (a stop in it resumes there: the step's own words say what it still holds); then the
        // inner's steps' stop exit back to the owner's
        if let Some(l_f) = l_flush {
            self.flush(b, env, (l_f, l_fin, l_done, held, &mut outer), &mut *on_row);
        }
        if let Some(l_e) = l_end {
            if self.memo.is_some() {
                crate::refuse!(self.node, Reason::Bug { payload: "a memoized inner with a step acting at its input end" });
            }
            b.switch_to(l_e);
            let p = mcx::to_vec_in(b.block_params(l_e), env.mcx);
            env.memo.clear();
            self.steps.iter().for_each(|s| s.ended(b, env));
            b.br(l_done, &p);
        }
        b.switch_to(l_done);
        if let Some(o) = outer.take() {
            Self::leave(env, o);
        }
        env.memo.restore(memo);
        Carry::from_params(b.block_params(l_done), env.mcx)
    }

    /// `walk`'s Memoize replay (`l_r`, `Memo::enter`'s): each stored row through the steps past the Memoize into the
    /// owner's continuation. Out of line, as `flush` and `copy_walk`, to keep `walk`'s frame small (D3833).
    #[inline(never)]
    fn replay<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, x: &Exits, (l_r, htypes, l_done, outer): (gravity_ir::Block, &[Type], gravity_ir::Block, &mut Option<PgBox<'_, Side<'e>>>), on_row: OnRow<'_, 'e>) {
        let Some((ms, mo)) = self.memo.as_ref() else { return };
        mo.replay(b, env, x, (l_r, htypes, l_done), &mut |b, env, j, row| {
            chain(b, env, &self.steps[ms + 1..], ms + 1, row, &mut |b, env, r| {
                mo.set_pos(b, env, j);
                self.hand_on(b, env, (&mut *outer, l_done, None, None), &mut *on_row, r)
            })
        });
    }

    /// D1018 (aggregates#527): the input's end (`l_f`) hands each holding step's rows on (frame.rs `flush_path`), the
    /// inner's cursor kept at its end (a stop in it resumes there: the step's own words say what it still holds); then the
    /// inner's steps' stop exit back to the owner's.
    #[inline(never)]
    fn flush<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, (l_f, l_fin, l_done, held, outer): (gravity_ir::Block, gravity_ir::Block, gravity_ir::Block, Option<Held>, &mut Option<PgBox<'_, Side<'e>>>), on_row: OnRow<'_, 'e>) {
        b.switch_to(l_f);
        if let Some(h) = held {
            env.store_word(b, h.word, env.zero);
        }
        env.memo.clear();
        let mut c = Carry::from_params(b.block_params(l_f), env.mcx);
        for (s, step) in self.steps.iter().enumerate().filter(|(_, st)| st.flushes()) {
            let rest = &self.steps[s + 1..];
            env.step = Some(s);
            c = step.flush(b, env, c, &mut |b, env, r| {
                let c = chain(b, env, rest, s + 1, r, &mut |b, env, r| self.hand_on(b, env, (&mut *outer, l_done, None, held), &mut *on_row, r));
                env.step = Some(s);
                c
            });
            env.step = None;
        }
        b.br(l_fin, &c.params());
        if let Some(h) = held {
            env.x.l_stop_in = h.stop_in;
            b.switch_to(h.unmap);
            let p = mcx::to_vec_in(b.block_params(h.unmap), env.mcx);
            let cur = b.load(Type::I64, h.slot, 0);
            b.br(h.stop_in, &[p[0], cur, p[2]]);
        }
    }
}

impl Inner<'_> {
    /// D732 (the copy entry, embedded): under a recheck's stored COPY row (`POS_COPY`) the re-open's one
    /// row is that record's fields as the SubqueryScan's input, entering the steps at its first op (its quals and
    /// projection run again, as ExecScan's EvalPlanQual path runs them) and handed on with the cursor at 0; a walk resumed
    /// past it (`skip` 1) or the empty copy (D1685) has no row. Else the plain walk follows.
    #[inline(never)]
    fn copy_walk<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, (carry, skip, l_done): (&Carry, Value, gravity_ir::Block), on_row: OnRow<'_, 'e>) {
        let Some((e, s0)) = &self.copy else { return };
        let (l_copy, l_row, l_plain) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        let on = crate::rerun::epq::copy_row::copying(b, env, e.site.get());
        b.cond_br(on, (l_copy, &[]), (l_plain, &[]));
        b.switch_to(l_copy);
        let none = crate::rerun::epq::copy_row::none(b, env, e.site.get());
        let past = b.icmp(IntCond::Ne, skip, env.zero);
        let gone = b.int_bin(IntBinOp::Or, none, past);
        b.cond_br(gone, (l_done, &carry.params()), (l_row, &[]));
        b.switch_to(l_row);
        let memo = env.memo.set_aside();
        let later = env.expands.iter().filter(|x| env.step.is_some_and(|j| x.0 > j)).map(|x| (usize::MAX, x.1));
        let expands = self.expands.iter().copied().chain(later).collect_in(env.mcx);
        let aggs = env.aggs.clone();
        let mut outer = Some(self.enter(env, (expands, None, aggs)));
        env.memo.clear();
        let mut row = crate::rerun::epq::copy_row::row(b, env, e, mcx::to_vec_in(&carry.accs[..], env.mcx));
        row.carry = carry.copy_in(env.mcx);
        let zero = env.zero;
        let c = chain(b, env, &self.steps[*s0..], *s0, &row, &mut |b, env, r| {
            self.set_cursor(b, env, zero);
            self.hand_on(b, env, (&mut outer, l_done, None, None), &mut *on_row, r)
        });
        b.br(l_done, &c.params());
        if let Some(o) = outer.take() {
            Self::leave(env, o);
        }
        env.memo.restore(memo);
        b.switch_to(l_plain);
    }
}

/// The owner's continuation per inner row (`Inner::walk`): the carry after, and the `Bool` that ends the walk at it.
pub(crate) type OnRow<'a, 'e> = &'a mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env<'e, '_>, &RowVals) -> (Carry<'e>, Option<Value>);

/// The inner's steps from `j` on over `row`, each through the next's continuation, the last into `last` (frame.rs `chain`
/// without its covered-window elision: an inner's windows run the general body).
fn chain<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, steps: &[PgBox<'_, dyn StepMech + '_>], j: usize, row: &RowVals, last: Next<'_, 'e>) -> Carry<'e> {
    crate::stack_check(); // D3832
    crate::frame::frame::bound(b, env, j); // EXPLAIN ANALYZE: the inner's node boundaries (its own, made current by `swap`)
    match steps.split_first() {
        None => {
            env.step = None;
            last(b, env, row)
        }
        Some((s, rest)) => {
            env.step = Some(j);
            let c = s.lower(b, env, row, &mut |b, env, r| {
                let c = chain(b, env, rest, j + 1, r, &mut *last);
                env.step = Some(j);
                c
            });
            env.step = None;
            c
        }
    }
}

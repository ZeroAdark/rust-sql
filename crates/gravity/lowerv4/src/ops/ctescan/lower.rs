//! Kind `nest.cte` (CteScan over a shared lazy store, PULL form; M8-MEMO-CTE CT-2, on the recursive union's store design
//! with the store and object changes below).
//!
//! PostgreSQL keeps one tuplestore per CTE (nodeCtescan.c): the first CteScan to read past the stored rows pulls the CTE's
//! plan for one more row, and every reader has its own read pointer. Here:
//! - The store: one per CTE, shared by its readers, never reset while the statement runs: nest.spool's record
//!   (nest/spool_rec.rs `Rec`, M8-NEST's; the count word and three more zeroed-at-bind words: the pull's budget, the rows
//!   it took, the end flag). The CTE object (`ProtoKind::Cte`, exec v4/m8_cte.rs, `arg` = the first of the three words)
//!   is the handle `m8_cte_pull` takes.
//! - The end (the CTE's Materialize): each row appended and counted; once the pull's budget is taken the pipeline stops
//!   resumably after the row (a later pull resumes it). No reader's row is ever lost or produced twice.
//! - The reader (each CteScan's source): its position is its own open word, reset at every (re)open. It reads the stored
//!   rows by index and calls `m8_cte_pull(ctx, h, budget)` only past them (0: stopped, 1: the CTE ended, 2: an error held
//!   behind the rows the pull stored, below 0: its error): the runtime runs the CTE's pulled set on through the leader's
//!   frame (RT-2f), so a CTE no reader reaches never runs. The budget (`Pull`) is 1 when the CTE's plan calls user or
//!   VOLATILE code, locks rows or writes: a reader that stops early (a LIMIT, a semi-join, a cursor) then never makes the
//!   CTE run a row PG would not. A quiet CTE takes `WINDOW` rows per pull; one that only may raise takes as many rows as
//!   are stored (doubling, up to WINDOW), its error held by the CTE object until a reader reads past the rows before it
//!   (D2044's rule for a fetch, at the pull). D3256 (`cte.batch`): a plan that may raise whose pulled pipeline reads a
//!   breaker and calls no user or VOLATILE code takes WINDOW rows, its error held as well: its producers run whole at the
//!   first pull whatever the budget (`batches`). No pull goes past the reader's frame::iface bound (a Limit's count + offset
//!   over it), unless the CteScan's own qual drops rows the bound never counts: it then grows as `Pull::Grow` (setop_rec-0043,
//!   `make_source`). Every reader runs on the leader (its pull runs there), but D3256's (`cte.claims`): an order-free reader
//!   of a store filled whole and sealed (D3040) reads it by record claims on the statement's workers. The reader keeps consuming the
//!   CTE's slot, which the driver opens zeroed for the readers (`Entry::skip_pulled`); the rows are the store's.
//! - A data-modifying CTE (D1496's follow-on): PG runs its ModifyTable to its end at the statement's end whatever its
//!   readers read (ExecPostprocessPlan). The Cte proto's DRAIN bit names its pulled set to RT-2f's drain_at_end, which
//!   pulls it to Done after the emit (exec v4/m8_cte.rs). A pull's budget word is cleared after each pull, so the drain's
//!   run (no reader sets it) never stops before the CTE's end. A CTE that only reads a data-modifying CTE is not drained
//!   (D2182): the writer drains on its own.
//! - A row-locking CTE (FOR UPDATE / SHARE inside it; V4R-512, D1496's lock half): PG locks the rows its readers reach
//!   (the CteScan pulls one row per miss), so its budget is 1 whatever its facts say; its lock rechecks in the IR
//!   (write/lockrows.rs), the newer version going into the store.
//! - A CTE a rescan re-runs (D1385; its plan reads a parameter a nest's or a SubPlan's re-open changes, so its pipeline is
//!   a member of that rerun set): PostgreSQL's ExecReScanCteScan clears the store and re-runs the plan lazily. Here the
//!   eager form: the set's `m8_rerun` runs the CTE's pipeline whole at every re-open, the set's owner zeroing the store's
//!   count first (the lifetime home's owner-run words, `eager_resets`), and no pull object is declared (a rerun member is its owner's, never a pull's: exec v4/m8_cte.rs
//!   `members`), so a reader takes the stored rows and ends past them. Only for a quiet plan (raise-free, no VOLATILE
//!   call, no row lock: rows PG would not pull change nothing it shows) every reader of which is a later pipeline of the
//!   same set (an embedded inner reading it, an EXISTS's, is not yet). Else (setop_rec-0043) the RE-ARM form when the
//!   set re-runs the CTE's whole plan (`rearms`): the pull object is declared as for any CTE, and the set's run re-arms it
//!   (exec ops/ctescan/proto.rs `rearm_ctes`: the pull and the store start again, the store's words the owner run's), so
//!   every reader, an embedded inner too, pulls lazily as PG reads; else refused by D1385's name. A recursive union's
//!   term set re-arms a CTE inside the term so at every generation (subselect#316 / #317: its plan reads the working
//!   table, whose filler stays the driver's).
//! - D2633: the store keeps only the CTE's columns some CteScan reads (decompose facts/demand/cte.rs: its quals and the
//!   target entries its parent reads), as PostgreSQL's CteScan deforms only what its readers ask of the stored tuple
//!   (PG stores the whole row; this is a cost choice, never an answer change). A reader's row carries those columns at
//!   their attnos; a bare entry over another passes on as no cell (step/project.rs `carried`), and any other read of
//!   one refuses by its own name as a column the row does not carry.
//! - Refused by name: a CTE whose producer a pipeline outside it also reads
//!   (frame/program.rs), and a CTE whose rerun set reads a once-run producer the pulls do not keep whole (D2081); a set's
//!   read of an append.arms run reads every arm (a SubPlan inner's UNION ALL arms, the run time's rule).
//! - D2059: a CTE whose rerun set reads a kept-whole once-run producer is served with any number of readers. A later
//!   reader's pull resumes the parked plan at the exact row: the re-read heap page (2,184 rows for PG's 1,999) was D2178's
//!   swept scan site, root-fixed by ROOTFIX-CTE-PARK (a parked pull keeps its sites across a reader's hold).
use core::cell::Cell as Once;

use gravity_abi::breaker::BreakerKind;
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::effects::Licence;
use gravity_decompose::graph::InitPlan;
use gravity_decompose::ops::shared::setop::{SetDesc, RECURSIVE_SET};
use gravity_decompose::ops::shared::subplan::SubLink;
use gravity_decompose::catalog::typefacts::{facts_of, TypeFacts};
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::shared::object::entries as m8;
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::{Parallel, Pipeline, Reason as Why};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};
use types_nodes::list::NodeList;

use crate::rerun::spool::rec::Rec;
use crate::face::end::EndMech;
use crate::face::kinds::{Face, KindId};
use crate::face::source::{SourceMech, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::{EndMaker, SourceMaker, Win};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{Late, Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::shared::cte_reads::{reads, WINDOW};
use crate::ops::Table;
use crate::rep::CellRef;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::NestCte, &EndMaker(make_end)), (KindId::NestCte, &SourceMaker(make_source))], ..crate::ops::NONE };

crate::strategies! {
    /// D3256: an order-free reader of a store filled whole and sealed (D3040) reads it by record claims on the statement's
    /// workers (`claimable`); off (`gravity_road_off`), every reader reads the store on the leader.
    CTE_CLAIMS = 133, "cte.claims", Cte;
    /// D3256: a pulled CTE that may raise, whose pulled pipeline reads a breaker and calls no user or VOLATILE
    /// code, takes WINDOW rows a pull, a raise held (`Pull::Batch`, `batches`); off, it pulls one row per miss, or
    /// `Pull::Grow`'s doubling.
    CTE_BATCH = 134, "cte.batch", Cte;
}

/// The words after the store's count, by index: the pull's budget, the rows it took, the end flag.
const W_BUDGET: u16 = 0;
const W_GOT: u16 = 1;
const W_DONE: u16 = 2;

/// The Cte proto's `arg` bit: a data-modifying CTE, drained to its end at the statement's end (exec v4/m8_cte.rs).
const DRAIN: u64 = 1 << 32;

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

/// Pipeline `k` and every pipeline feeding it (the CTE's plan, its producers and the rerun sets its plan reads).
pub(crate) fn fed<'g>(gx: &Gx<'g, '_>, k: usize) -> PgVec<'g, usize> {
    let (mut todo, mut seen) = (mcx::pg_vec![in gx.mcx; k], PgVec::new_in(gx.mcx));
    while let Some(p) = todo.pop() {
        if !seen.contains(&p) {
            seen.push_in(p);
            todo.extend_in(reads(gx, p));
        }
    }
    seen
}

/// Whether pipeline `k` or any pipeline feeding it has an op `hit` names (a data-modifying CTE's ModifyTable, a FOR UPDATE).
pub(crate) fn feeds(gx: &Gx<'_, '_>, k: usize, hit: fn(&Op<'_>) -> bool) -> bool {
    fed(gx, k).into_iter().any(|p| gx.pipelines()[p].ops.iter().any(hit))
}

/// D1385's eager form (the module doc): CTE `k` (InitPlan `i`) is a member of a nest's or a SubPlan's rerun set (not a
/// recursive union's term set) and of no other, its plan quiet (F5 raise-free, no VOLATILE call, no row lock, no write) and
/// holding no recursive union's term (rec-0004: the re-arm form's, `rearms`), and every pipeline reading its store re-runs
/// in that set after it (no embedded inner reads it).
/// The licence of its run ahead of its readers comes back: its quiet code shows nothing there
/// (`effects::unobservable`).
pub(crate) fn eager(gx: &Gx<'_, '_>, k: usize, i: &InitPlan<'_>) -> Option<Licence> {
    let mut sets = gx.graph.reruns.iter().filter(|r| r.pipelines.contains(&(k as u16)));
    let (Some(r), None) = (sets.next(), sets.next()) else { return None };
    let quiet = crate::effects::visible::ctescan::quiet(gx, k, i) && !drains(gx, k);
    let ps = gx.pipelines();
    let readers = (0..ps.len()).filter(|&p| p != k && reads(gx, p).contains(&k));
    let inner = ps.inners().iter().any(|(_, q)| q.desc.consumes_from.contains(&gravity_program::LifeId(k as u16)));
    // surface rec-0004: a CTE whose plan holds a recursive union's term takes the re-arm form (`rearms`), never this one:
    // run whole by the set, the term's members would be run by both sets (D_RERUN_SHARED)
    let term = fed(gx, k).into_iter().any(|q| gx.graph.reruns.iter().any(|t| t.plan_id == RECURSIVE_SET && t.pipelines.contains(&(q as u16))));
    let taken = r.plan_id != RECURSIVE_SET && quiet && !inner && !term && readers.into_iter().all(|p| p > k && r.pipelines.contains(&(p as u16)));
    taken.then(|| gravity_decompose::effects::unobservable(gravity_decompose::effects::Effects::NONE)).flatten()
}

/// D3037 (`CTE_PRODUCERS_AHEAD`): CTE `k`'s producers run ahead in program order, at their own widths, and only its own
/// pipeline is pulled. Its plan is quiet (raise-free, no VOLATILE call, no lock, no write: running a producer PG would not
/// run changes nothing it shows), no rerun set re-runs the CTE or a producer, its plan holds no other CTE and no
/// recursive union's term, and every reader is a graph pipeline whose source is the CTE's store, which no rerun set
/// re-opens and no InitPlan or lazy build runs on demand: it pulls at its first row, where PostgreSQL runs a breaker producer whole (a hashed group, a sort, a hash
/// build reads all its input before its first row). Else the whole plan is pulled, leader-serial, as before.
/// D3095: an InitPlan reader (a scalar SubPlan over the CTE, run on demand) is taken beside them when another reader is
/// such a graph pipeline and reads the store whole (`whole`): that reader runs the producers whole where PostgreSQL's
/// would, and the InitPlan's pull, whenever it runs, pulls only the CTE's own pipeline (a `(SELECT max(c) FROM cte)` filter over the CTE's own scan).
/// Each producer's slot is held through the CTE's last reader; a whole reader has pulled the store to its end by then,
/// so an InitPlan run later reads the store and pulls no producer.
/// `Some(licence)`: the producers' run ahead, of a quiet plan whose run shows nothing (`effects::unobservable`);
/// `Some(None)`: a plan that is not quiet runs at the statement's first fetch (`cte_whole_gate`'s `first_fetch`),
/// PostgreSQL's own point, not ahead.
fn producers_ahead(px: &Px<'_, '_>, k: usize) -> Option<Option<Licence>> {
    let gx = px.gx;
    if fed(gx, k).len() <= 1 || !cte_whole_gate(gx, k, true, false, false) {
        return None;
    }
    let on = on_demand(gx);
    let readers = readers_of(gx, k);
    let pulls = |p: usize| reads_from_start(gx, k, p) && gx.pipelines()[p].desc.lazy_for < 0 && !on.contains(&p) && !gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(p as u16)));
    if readers.iter().any(|p| on.contains(p)) && !readers.iter().any(|&p| pulls(p) && whole(px, p)) {
        return None;
    }
    let i = gx.graph.init_plans.iter().find(|i| i.pipeline as usize == k && matches!(i.link, SubLink::Cte))?;
    let quiet = crate::effects::visible::ctescan::quiet(gx, k, i);
    Some(quiet.then(|| gravity_decompose::effects::unobservable(gravity_decompose::effects::Effects::NONE)).flatten())
}

/// D1: the exact licence of CTE `k`'s plan run from the statement's first fetch (`effects::visible::ctescan::first_fetch`'s
/// D3097 proof, re-checked by the licence home's `Proof::first_fetch`): `None` for a quiet plan (no proof needed, its run
/// shows nothing), a plan that acts (denied) or one whose CteScan is not the first fetch.
fn first_fetch_exact(gx: &Gx<'_, '_>, k: usize) -> Option<Licence> {
    let i = gx.graph.init_plans.iter().find(|i| i.pipeline as usize == k && matches!(i.link, SubLink::Cte))?;
    if crate::effects::visible::ctescan::quiet(gx, k, i) {
        return None;
    }
    let proof = gravity_decompose::effects::Proof::first_fetch(gx.graph.stmt?, i.plan_id)?;
    gravity_decompose::effects::exact(gravity_decompose::effects::Effects::of_init_plan(i), proof).ok()
}

/// D3095: the pipelines reading CTE `k`'s store (graph pipelines and embedded inners).
fn readers_of<'g>(gx: &Gx<'g, '_>, k: usize) -> PgVec<'g, usize> {
    (0..gx.pipelines().end()).filter(|&p| p != k && reads(gx, p).contains(&k)).collect_in(gx.mcx)
}

/// The pipelines an InitPlan that is no CTE runs on demand (a scalar SubPlan's: its reader may pull later, or never).
fn on_demand<'g>(gx: &Gx<'g, '_>) -> PgVec<'g, usize> {
    gx.graph.init_plans.iter().filter(|j| !matches!(j.link, SubLink::Cte)).flat_map(|j| fed(gx, j.pipeline as usize)).collect_in(gx.mcx)
}

/// Whether graph pipeline `p`'s source is CTE `k`'s store.
fn reads_from_start(gx: &Gx<'_, '_>, k: usize, p: usize) -> bool {
    let ps = gx.pipelines();
    p < ps.len() && matches!(ps[p].source, Source::Breaker { slot } if gx.life_of(p, slot).is_some_and(|l| l.0 as usize == k))
}

/// Whether reader pipeline `p` reads its source whole: no LIMIT bound over it (the Request's bound), no gate step in it.
fn whole(px: &Px<'_, '_>, p: usize) -> bool {
    let ps = px.gx.pipelines();
    let node = ps[p].node_ops.first().map_or(ps[p].node_id, |n| n.node_id);
    !matches!(px.request(node).bound, crate::frame::iface::Bound::Rows(_)) && !ps[p].ops.iter().any(|o| matches!(o, Op::Gate(_)))
}

/// D3037 / D3040's gate (`producers_ahead`, `fills_parallel`): CTE `k` is quiet (or, D3097, the statement's first fetch
/// with a plan that neither acts nor locks: `first_fetch`), no rerun set touches it or its
/// plan holds no other CTE, and every reader is a graph pipeline sourced from its store that no rerun set, InitPlan or
/// lazy build runs on demand. D3095: under `demand`, a reader an InitPlan runs on demand (sourced from the store, no rerun
/// set's, no lazy build's) is taken too, so long as another reader is such a graph pipeline (`producers_ahead` asks the
/// rest). D3040's fill never takes one: the InitPlan's members would take in the filled store's pipeline. D3549: under
/// `chain` (`fills_alone`) the plan may hold other CTEs: `fill_set` keeps it only where each of them fills ahead too.
/// D3336 (A2 (iii), `loud`): a plan that may raise and is neither quiet nor the first fetch passes when `cte.wholefill`
/// licenses its run ahead (wholefill.rs `licensable`).
fn cte_whole_gate(gx: &Gx<'_, '_>, k: usize, demand: bool, chain: bool, loud: bool) -> bool {
    let Some(i) = gx.graph.init_plans.iter().find(|i| i.pipeline as usize == k && matches!(i.link, SubLink::Cte)) else { return false };
    let rerun = |p: usize| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(p as u16)));
    let ps = gx.pipelines();
    // an InitPlan's run (a scalar SubPlan over the CTE) is on demand: its reader may pull later, or never
    let on_demand = on_demand(gx);
    let lazy = |p: usize| ps[p].desc.lazy_for >= 0 || on_demand.contains(&p);
    let reader = |p: usize| p < ps.len() && !rerun(p) && !lazy(p) && reads_from_start(gx, k, p);
    let asks = |p: usize| demand && p < ps.len() && !rerun(p) && ps[p].desc.lazy_for < 0 && on_demand.contains(&p) && reads_from_start(gx, k, p);
    let readers = readers_of(gx, k);
    (loud || crate::effects::visible::ctescan::quiet(gx, k, i) || crate::effects::visible::ctescan::first_fetch(gx, k, i))
        && !drains(gx, k)
        && !rerun(k)
        && fed(gx, k).into_iter().filter(|&p| p != k).all(|p| p < ps.len() && !rerun(p) && (chain || !is_cte(gx, p)))
        && readers.iter().any(|&p| reader(p))
        && readers.iter().all(|&p| reader(p) || asks(p))
}

/// D3040 (`CTE_FILL_PARALLEL`): CTE `k` is filled whole in program order at its pipeline's width, into an owned spool, and
/// never pulled: `cte_whole_gate`'s gate, and every reader reads it whole (no LIMIT bound over the reader (the Request's
/// bound), no gate step in the reader's pipeline; D3450: none streaming to a named cursor's sink, unless the plan is quiet)
/// and no reader is in another CTE's plan, so the fill runs only rows PostgreSQL's readers pull; and the statement neither
/// writes nor locks rows (no recheck reruns the fill). D3549: a plan holding other CTEs, or a reader in another CTE's
/// plan, when those CTEs fill ahead too (`fill_set`).
fn fills_parallel(px: &mut Px<'_, '_>, k: usize) -> bool {
    use crate::rerun::params::NestExt;
    if px.ext::<NestExt>(Face::Nest).is_none_or(|x| x.cte_fills.is_none()) {
        let set = fill_set(px);
        px.ext_mut::<NestExt>(Face::Nest).cte_fills = Some(set);
    }
    let fill = px.ext::<NestExt>(Face::Nest).and_then(|x| x.cte_fills.as_ref()).is_some_and(|s| s.contains(&k));
    // D4048: a plan that may raise, filled whole ahead of a reader streaming to the statement's sink, runs rows PostgreSQL's
    // CteScan never pulls only for a reader that stops early, which D3450 reads for a named cursor alone: the program is
    // served to a reader that takes every row (`Px::whole_read`), so a reader whose count shows only at its run (a SQL
    // function's lazy row, an SPI execute's count) starts it again as a partial reader, whose CTE is pulled
    if fill && stop_sees_fill(px.gx, k) {
        px.whole_read(k);
    }
    fill
}

/// D4048: a reader that stops early would see CTE `k`'s fill run past its stop: the plan is not quiet
/// (`effects::visible::ctescan::quiet`) and a reader of its store streams to the statement's sink (D3450's reader).
fn stop_sees_fill(gx: &Gx<'_, '_>, k: usize) -> bool {
    let Some(i) = gx.graph.init_plans.iter().find(|i| i.pipeline as usize == k && matches!(i.link, SubLink::Cte)) else { return true };
    let sink = |p: usize| p != k && gx.end_kind(p) == crate::graph::EndKind::Sink && reads(gx, p).contains(&k);
    !crate::effects::visible::ctescan::quiet(gx, k, i) && (0..gx.pipelines().len()).any(sink)
}

/// D3549 (Workers IO sig-e4133037cabe: 34 chained MATERIALIZED CTEs ran nested past the 32 hosted lanes): the CTEs filled
/// ahead, the greatest set whose every CTE passes `fills_alone` and holds in it each CTE its plan holds and each CTE whose
/// plan holds one of its readers. A filled CTE runs in program order after its producers (the CTEs its plan reads are
/// filled first), so no reader pulls a plan from inside another plan's pull: a chain of `c_i AS (.. FROM c_{i-1})` runs
/// as a sequence of fills where its pulls nested one hosted lane per link. Dropping a CTE that fails can only fail
/// others, so the loop ends at the greatest fixpoint; a chain with one CTE out (a LIMIT reader, a raising plan's reader
/// that may raise first, a rerun set) is pulled whole, as before.
fn fill_set(px: &Px<'_, '_>) -> Vec<usize> {
    let gx = px.gx;
    let mcx = px.mcx();
    let ctes: PgVec<'_, usize> = gx.graph.init_plans.iter().filter(|i| matches!(i.link, SubLink::Cte)).map(|i| i.pipeline as usize).collect_in(mcx);
    let mut set: Vec<usize> = ctes.iter().copied().filter(|&k| fills_alone(px, k)).collect(); // alloc: Program
    if set.is_empty() || ctes.len() == 1 {
        return set;
    }
    let plans: PgVec<'_, PgVec<'_, usize>> = ctes.iter().map(|&c| fed(gx, c)).collect_in(mcx);
    // each candidate's CTE neighbours: the CTEs its plan holds, and the CTEs whose plans hold one of its readers
    let needs: PgVec<'_, (usize, PgVec<'_, usize>)> = ctes
        .iter()
        .zip(&plans)
        .filter(|(k, _)| set.contains(k))
        .map(|(&k, own)| {
            let readers = readers_of(gx, k);
            let held = |(&c, plan): (&usize, &PgVec<'_, usize>)| (c != k && (own.contains(&c) || readers.iter().any(|p| plan.contains(p)))).then_some(c);
            (k, ctes.iter().zip(&plans).filter_map(held).collect_in(mcx))
        })
        .collect_in(mcx);
    let mut on = mcx::pg_vec![in mcx; false; gx.pipelines().end()];
    loop {
        set.iter().for_each(|&k| on[k] = true);
        let kept: Vec<usize> = needs.iter().filter(|(k, near)| on[*k] && near.iter().all(|&c| on[c])).map(|n| n.0).collect(); // alloc: Program
        if kept.len() == set.len() {
            return set;
        }
        on.iter_mut().for_each(|o| *o = false);
        set = kept;
    }
}

/// `fills_parallel`'s test of CTE `k` alone: every condition but the CTE graph's (`fill_set` asks that). D3336 (A2 (iii)):
/// or a plan that may raise whose run ahead `cte.wholefill` licenses (wholefill.rs `licensable`), every reader reading it
/// whole and none streaming to a named cursor's sink.
fn fills_alone(px: &Px<'_, '_>, k: usize) -> bool {
    let gx = px.gx;
    // a write or a row lock reruns producers in its recheck (EvalPlanQual), which an owned spool sealed once cannot take
    if px.write_plan().is_some() || gx.graph.recheck {
        return false;
    }
    let loud = crate::ops::ctescan::wholefill::licensable(px, k);
    if !cte_whole_gate(gx, k, false, true, loud) {
        return false;
    }
    reader_facts(px, k).is_some_and(|(quietly, whole, stops, early)| whole && (quietly || loud && !stops && !early))
}

/// D3336 (A2 (iii)): CTE `k`'s fill (`fills_parallel`) is `cte.wholefill`'s alone: D3040's own gate and test refuse it, so
/// its run ahead needs the road's licence (wholefill.rs `license`).
fn wholefill_only(px: &Px<'_, '_>, k: usize) -> bool {
    !(cte_whole_gate(px.gx, k, false, true, false) && reader_facts(px, k).is_some_and(|(quietly, whole, _, _)| quietly && whole))
}

/// `fills_alone`'s reading of CTE `k`'s readers: whether its plan fills ahead quietly (`fills_ahead_quietly`), whether every
/// reader reads it whole, whether a reader streams to a named cursor's sink, and (D3336 A2 (iii)) whether a reader may
/// stop its read at a count (wholefill.rs `stops_early`); `None` for no CTE InitPlan.
fn reader_facts(px: &Px<'_, '_>, k: usize) -> Option<(bool, bool, bool, bool)> {
    let gx = px.gx;
    let ps = gx.pipelines();
    // D3097: a plan that may raise fills ahead of its readers only when no reader can raise first (PG's interleaved pulls)
    let readers: PgVec<'_, usize> = (0..ps.len()).filter(|&p| p != k && reads(gx, p).contains(&k)).collect_in(px.mcx());
    let i = gx.graph.init_plans.iter().find(|i| i.pipeline as usize == k && matches!(i.link, SubLink::Cte))?;
    // D3450: a named cursor's FETCH may stop the root's stream (graph.partial) where PostgreSQL's CteScan has pulled the
    // plan only one row per miss, so a plan that is not quiet is never filled ahead of a reader streaming to that sink
    // (the same-row rule's `row_first`)
    let stops = |p: usize| gx.graph.partial && gx.end_kind(p) == crate::graph::EndKind::Sink;
    // a reader inside another CTE's plan is that CTE's pulled set's unless that CTE fills ahead too (`fill_set`): the
    // filled store's pipeline would join the pulled set as a producer another pipeline also reads
    let whole_readers = readers.iter().all(|&p| whole(px, p));
    let stops = readers.iter().any(|&p| stops(p));
    let quietly = crate::effects::visible::ctescan::fills_ahead_quietly(gx, k, i, &readers[..], whole_readers, stops).is_some();
    let early = readers.iter().any(|&p| crate::ops::ctescan::wholefill::stops_early(gx, p));
    Some((quietly, whole_readers, stops, early))
}

/// The rerun set that would re-arm CTE `k` (`rearms` decides whether it does): the one set holding it. D2300 (three
/// recursive levels, a union inside a term that is itself inside an outer union's term): every set holding it a recursive
/// union's term set, the innermost of them, whose members every other one holds. The outer term's run re-arms the middle
/// union's CTE (its own `rearms`), whose next pull re-runs the middle term, which re-arms this one, as PG's
/// ExecReScanRecursiveUnion reaches the inner union through the middle one's chgParam.
pub(crate) fn rearm_set(gx: &Gx<'_, '_>, k: usize) -> Option<usize> {
    let rs = &gx.graph.reruns;
    let holds = |i: &usize| rs[*i].pipelines.contains(&(k as u16));
    let all = || (0..rs.len()).filter(holds);
    let mut sets = all();
    let (first, second) = (sets.next()?, sets.next());
    if second.is_none() {
        return Some(first);
    }
    let inside = |a: usize, b: usize| rs[a].pipelines.iter().all(|m| rs[b].pipelines.contains(m));
    if !all().all(|i| rs[i].plan_id == RECURSIVE_SET) {
        return None;
    }
    all().find(|&i| all().all(|j| j == i || inside(i, j)))
}

/// Whether pipeline `p` is a CTE's (an InitPlan's `Cte` link fills its store).
pub(crate) fn is_cte(gx: &Gx<'_, '_>, p: usize) -> bool {
    gx.graph.init_plans.iter().any(|i| i.pipeline as usize == p && matches!(i.link, SubLink::Cte))
}

/// Whether CTE `k` takes D1385's eager form (`cte_of` admitted it): a member of a rerun set, not re-armed (`rearms`).
fn rerun_member(gx: &Gx<'_, '_>, k: usize) -> bool {
    gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(k as u16))) && !rearms(gx, k)
}

/// setop_rec-0043: CTE `k` takes the RE-ARM form (the module doc's D1385 paragraph): a member of one nest's or SubPlan's
/// rerun set and of no other, not D1385's eager form, writing and locking nothing, whose whole plan the set re-runs (every
/// pipeline feeding it is the set's, or a recursive union's term set inside it), and every graph pipeline reading its store
/// a member of the set (an embedded inner, the SubPlan's own CteScan, re-opens after the owner's run). PG's
/// ExecReScanCteScan clears the store and re-runs the plan lazily at the next read (chgParam): the owner's run re-arms the
/// CTE's pull (exec ops/ctescan/proto.rs `rearm`: the store and its pulled set start again), never runs it.
/// subselect#316 / #317: the set may be a recursive union's term set too (a CTE inside the term, read twice, whose plan
/// reads the working table: PG's ExecRecursiveUnion puts `wtParam` in the term's chgParam at every generation, so the
/// CTE's store is cleared and its plan re-run at the next read). The working table's filler (the seed) feeds it but is
/// the union driver's (`work`): the driver swaps each generation in, and the CTE's pull never runs the seed again
/// (program protos.rs `producers_in` stops there too).
pub(crate) fn rearms(gx: &Gx<'_, '_>, k: usize) -> bool {
    let Some(i) = gx.graph.init_plans.iter().find(|i| i.pipeline as usize == k && matches!(i.link, SubLink::Cte)) else { return false };
    let Some(r) = rearm_set(gx, k).map(|s| &gx.graph.reruns[s]) else { return false };
    if eager(gx, k, i).is_some() || drains(gx, k) || feeds(gx, k, |op| matches!(op, Op::Lock(_))) {
        return false;
    }
    // with#45: a recursive union inside a recursive term is re-armed by the outer term set too: its table counts are that
    // set's owner-run words (ops/setop/recursive.rs `rearm_resets`, rerun/sets.rs `rearming_set`), emptied before every
    // outer generation, as PG's ExecReScanRecursiveUnion empties both tables when `wtParam` reaches it
    let body = fed(gx, k);
    let term = |q: usize| gx.graph.reruns.iter().any(|t| t.plan_id == RECURSIVE_SET && t.pipelines.contains(&(q as u16)) && t.pipelines.iter().all(|&m| body.contains(&(m as usize))));
    let ps = gx.pipelines();
    // a graph pipeline outside the set reads the store only through embedded inners whose re-open runs the set first (the
    // SubPlan's or nest's own CteScan inner: `owner_set`)
    let via_owner = |p: usize| {
        let mut ins = (ps.len()..ps.end()).filter(|&i| root_of(gx, i) == p && reads(gx, i).contains(&k)).peekable();
        ins.peek().is_some() && ins.all(|i| crate::rerun::sets::owner_set(gx, i) == Some((r.node_id, r.plan_id)))
    };
    // subselect#316 / #317: the working table of the union whose term set `r` is, which the CTE's plan reads through its
    // WorkTableScan: filled by the seed `q` and swapped in per generation by the union's driver, never by the CTE's pull
    let work = |q: usize| r.plan_id == RECURSIVE_SET && (0..ps.len()).any(|p| ps[p].ops.iter().any(|o| matches!(o, Op::SetOp(SetDesc::Iter(d)) if r.pipelines.contains(&d.body) && gx.life_of(p, d.work).is_some_and(|l| l.0 as usize == q))));
    // with#170 / #171: another CTE no rerun set re-runs, read inside the plan (a Hash over its CteScan), with its plan: its
    // own pull fills its store once and only appends (lifetime/cte.rs `kept_whole`), never re-armed here
    let once = |q: usize| !gx.graph.reruns.iter().any(|t| t.pipelines.contains(&(q as u16)));
    let apart: PgVec<'_, usize> = body.iter().copied().filter(|&c| c != k && is_cte(gx, c) && once(c)).flat_map(|c| fed(gx, c)).collect_in(gx.mcx);
    // with#170 / #171: a once-run producer of the term set's plan (a Hash build over that other CTE's store, read by a
    // member's probe) is kept: the set's decomposer left it out (no node of it reads `wtParam`, PG keeps its one-batch
    // table), its slot is the set's, and a re-arm leaves it run (exec ops/ctescan/proto.rs `rearmed`)
    let kept = |q: usize| r.plan_id == RECURSIVE_SET && q != k && once(q);
    body.iter().all(|&q| r.pipelines.contains(&(q as u16)) || term(q) || work(q) || apart.contains(&q) || kept(q)) && (0..ps.len()).filter(|&p| p != k && reads(gx, p).contains(&k)).all(|p| r.pipelines.contains(&(p as u16)) || via_owner(p))
}

/// The graph pipeline embedding pipeline `q` (`q` itself when it is a graph pipeline, else the root of its inners' chain).
fn root_of(gx: &Gx<'_, '_>, mut q: usize) -> usize {
    let ps = gx.pipelines();
    while q >= ps.len() {
        q = ps.inners()[q - ps.len()].0 .0;
    }
    q
}

/// setop_rec-0043: whether embedded inner `ik` is a CteScan over a CTE its owner's set re-arms (`rearms`, the CTE a member
/// of the set the re-open runs): each re-open re-arms the CTE before the inner opens, so the inner's read pointer (nest/cte.rs
/// `CteRows`, reset at every re-open) reads the store its next pull fills again from the plan's start. subselect#316 /
/// #317: or an inner with no set of its own (a nest's plain CteScan inner) whose graph pipeline is a member of the set
/// re-arming the CTE: each run of the set re-arms the CTE before that pipeline runs again, and the inner's re-opens inside
/// one run read the one store, rewound, as PG's CteScan rescan without a changed parameter rewinds its read pointer.
pub(crate) fn rearmed_inner(gx: &Gx<'_, '_>, ik: usize) -> bool {
    let Source::Breaker { slot } = gx.pipelines()[ik].source else { return false };
    let Some(k) = gx.life_of(ik, slot).map(|l| l.0 as usize) else { return false };
    let set = crate::rerun::sets::owner_set(gx, ik);
    let holds = |r: &gravity_decompose::graph::Rerun| Some((r.node_id, r.plan_id)) == set || set.is_none() && r.pipelines.contains(&(root_of(gx, ik) as u16));
    rearms(gx, k) && gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(k as u16)) && holds(r))
}

/// The CTE whose pipeline is `k`: its InitPlan (link `Cte`).
fn cte_of<'g, 'p>(px: &Px<'g, 'p>, k: usize, node: i32) -> &'g InitPlan<'p> {
    let gx = px.gx;
    let Some(i) = gx.graph.init_plans.iter().find(|i| i.pipeline as usize == k && matches!(i.link, SubLink::Cte)) else { bug(node, "a nest.cte face over no CTE") };
    if let Some(why) = crate::lifetime::cte::rerun_refused(gx, k, i) {
        shape(node, why);
    }
    i
}

/// A data-modifying CTE (its plan or a producer of it writes): drained to its end at the statement's end. D2182: the walk
/// stops at another CTE's pipeline: a CTE that only reads a data-modifying CTE is not drained (PG's ExecPostprocessPlan runs
/// only the ModifyTable on, so the reader's per-row work past its readers' stop never runs); the writer, its own pulled
/// set, takes its own DRAIN bit.
fn drains(gx: &Gx<'_, '_>, k: usize) -> bool {
    let (mut todo, mut seen) = (mcx::pg_vec![in gx.mcx; k], PgVec::new_in(gx.mcx));
    while let Some(p) = todo.pop() {
        if !seen.contains(&p) && (p == k || !is_cte(gx, p)) {
            seen.push_in(p);
            todo.extend_in(reads(gx, p));
        }
    }
    seen.into_iter().any(|p| gx.pipelines()[p].ops.iter().any(|op| matches!(op, Op::Write(_))))
}

/// D2605 PULLED (ops/modifytable/rowop.rs `pulled_ends`): CTE pipeline `k` writes (INSERT / UPDATE / DELETE, with a
/// RETURNING list) in its own pipeline and is pulled by its readers through the Cte object, drained at the statement's end
/// (`object`'s DRAIN form): not D1385's eager form, its store's row its RETURNING projection, no Append arm.
pub(crate) fn pulled_write(gx: &Gx<'_, '_>, k: usize) -> bool {
    let p = &gx.pipelines()[k];
    let writes = p.ops.iter().filter(|o| matches!(o, Op::Write(w) if w.cmd != gravity_program::Cmd::Select)).count();
    let returns = p.ops.iter().any(|o| matches!(o, Op::Write(w) if w.returning.is_some()));
    is_cte(gx, k) && !rerun_member(gx, k) && drains(gx, k) && writes == 1 && returns && matches!(p.ops.last(), Some(Op::Project(_))) && !p.ops.iter().any(crate::graph::is_arm)
}

/// The row the CTE's pipeline `k` hands its end: its projection's entries, the pipeline's LAST op (as nest.spool's end
/// requires): a projection before a later row-changing op (a join's probe, a set function) is not the stored row.
fn cols<'g, 'p>(px: &Px<'g, 'p>, k: usize, node: i32) -> &'g NodeList<'p> {
    match px.gx.pipelines()[k].ops.last() {
        Some(Op::Project(cols)) => cols,
        _ => shape(node, "a stored CTE row whose pipeline does not end in its projection"),
    }
}

/// D2633: the CTE columns (1-based, ascending) CTE `k`'s store keeps, its row `ncols` wide: those some CteScan of the CTE
/// reads (decompose `PipelineGraph::cte_reads`, one per CTE), or `None`, every column as before: a CteScan reading the
/// whole row, a system column or an expression decompose does not know, no CteScan of the CTE, the union being every
/// column, or a recursive union's CTE (fail closed).
pub(crate) fn kept<'g>(gx: &Gx<'g, '_>, k: usize, ncols: usize) -> Option<PgVec<'g, i16>> {
    let i = gx.graph.init_plans.iter().find(|i| i.pipeline as usize == k && matches!(i.link, SubLink::Cte))?;
    if gx.pipelines()[k].ops.iter().any(|o| matches!(o, Op::SetOp(SetDesc::Iter(_)))) {
        return None;
    }
    let (mut out, mut any) = (PgVec::new_in(gx.mcx), false);
    for r in gx.graph.cte_reads.iter().filter(|r| r.plan_id == i.plan_id) {
        any = true;
        for &a in r.cols? {
            if !out.contains(&a) {
                out.push_in(a);
            }
        }
    }
    out.sort_unstable();
    (any && out.len() < ncols && out.iter().all(|&a| a >= 1 && a as usize <= ncols)).then_some(out)
}

/// D2633: the columns reader pipeline `k`'s row carries when its source is a CteScan over a store `kept` narrows
/// (step/project.rs `carried`: a bare entry over another passes on as no cell); `None` for any other pipeline.
pub(crate) fn carried<'g>(gx: &Gx<'g, '_>, k: usize) -> Option<PgVec<'g, i16>> {
    let &Source::Breaker { slot } = &gx.pipelines()[k].source else { return None };
    let q = gx.life_of(k, slot)?.0 as usize;
    if q >= gx.graph.pipelines.len() || gx.spool_kind(q) != KindId::NestCte {
        return None;
    }
    let Some(Op::Project(cols)) = gx.pipelines()[q].ops.last() else { return None };
    kept(gx, q, cols.len())
}

/// The record of the columns `keep` names (`None`: every column), each column's type facts (rerun/spool/rec.rs `types`).
fn kept_types<'g>(px: &Px<'g, '_>, node: i32, cols: &NodeList<'_>, keep: &[i16]) -> PgVec<'g, TypeFacts> {
    keep.iter().map(|&a| {
        let te = cols.iter().nth(a as usize - 1).and_then(|n| n.as_target_entry()).filter(|te| te.resno == a).unwrap_or_else(|| shape(node, "a stored CTE column out of resno order"));
        let typ = gravity_decompose::catalog::expr_type::type_of(te.expr).unwrap_or_else(|| gravity_decompose::catalog::expr_type::result_type(te.expr));
        facts_of(px.gx.facts, typ).unwrap_or_else(|| shape(node, "a stored CTE column whose type the catalog does not know"))
    }).collect_in(px.mcx())
}

/// The CTE's store (its record and the columns it keeps, `kept`) and its object's words (handle, first extra word),
/// declared once per CTE by its end or its first reader.
fn object<'g>(px: &mut Px<'g, '_>, k: usize, node: i32) -> (Rec<'g>, Option<PgVec<'g, i16>>, u16, u16) {
    let cols = cols(px, k, node);
    let keep = kept(px.gx, k, cols.len());
    let find = |px: &Px<'g, '_>| match &keep {
        None => Rec::find(px, k, node, cols),
        Some(keep) => Rec::find_tfs(px, k, node, kept_types(px, node, cols, keep)),
    };
    let declare = |px: &mut Px<'g, '_>| match &keep {
        None => Rec::declare(px, k, k, node, cols, 3),
        Some(keep) => {
            let tfs = kept_types(px, node, cols, keep);
            Rec::declare_tfs(px, k, k, node, tfs, 3)
        }
    };
    if let Some(p) = px.protos.iter().find(|p| p.kind == ProtoKind::Cte && p.pipeline as usize == k) {
        let (h, x) = (p.words.base, p.arg as u16);
        return (find(px), keep, h, x);
    }
    // D1385's eager form: the store alone (the count word sits before the extra words, rerun/spool/rec.rs `declare`)
    if rerun_member(px.gx, k) {
        if let Some(p) = px.protos.iter().find(|p| p.kind == ProtoKind::Spool && p.peer as usize == k) {
            let x = p.words.base + 2;
            return (find(px), keep, 0, x);
        }
        let (rec, x) = declare(px);
        return (rec, keep, 0, x);
    }
    let (rec, x) = declare(px);
    let h = px.proto_words(1);
    let drain = if drains(px.gx, k) { DRAIN } else { 0 };
    let fill = fills_parallel(px, k);
    let rec = if fill { rec.owned(px, k) } else { rec };
    // D3336 (A2 (iii)): a fill `cte.wholefill` alone takes runs ahead under v5 errorhold's licence (wholefill.rs)
    if fill && wholefill_only(px, k) {
        crate::ops::ctescan::wholefill::license(px, k, node);
    }
    let producers = if fill { None } else { producers_ahead(px, k) };
    // D4203: a plan running a node mapping read ahead under a recheck licence that a reader may pull first after a row
    // left (`Px::licensed_late`) runs its producers before any row can leave, under that licence (`licensed_first`).
    // Where program order puts the pull after an emitting pipeline (`Late::Ordered`) and they cannot, the statement
    // refuses: never pulled where the licence's plan-order run cannot answer. On a road the lowering cannot time
    // (`Late::Unanswered`) it keeps the pull it had (C2 debt), never a refusal.
    let producers = match (plan_late(px, k), producers) {
        (Late::No, p) => p,
        (Late::Ordered, None) => Some(Some(licensed_first(px, k).unwrap_or_else(|| shape(node, LICENSED_LATE)))),
        (Late::Ordered, Some(p)) if producers_first(px, k) => Some(p),
        (Late::Ordered, Some(_)) => shape(node, LICENSED_LATE),
        (Late::Unanswered, None) => licensed_first(px, k).map(Some),
        (Late::Unanswered, p) => p,
    };
    let ahead = match fill {
        true => gravity_program::protos::CTE_FILL_PARALLEL,
        false if producers.is_some() => gravity_program::protos::CTE_PRODUCERS_AHEAD,
        false => 0,
    };
    // D3336 (A2 (iii)): under `cte.wholefill`, the form every other CTE took (its census: D3040's own fill, producers run
    // ahead with the CTE's own pipeline pulled, or the whole plan pulled); the road's own fill fired `lower.cte.wholefill`
    if crate::ops::ctescan::wholefill::road_on(px) && !(fill && wholefill_only(px, k)) {
        fires::fire(match (fill, producers.is_some()) {
            (true, _) => "lower.cte.wholefill.d3040", // shapecheck fire signal (D3336 A2 (iii)): D3040's own fill
            (false, true) => "lower.cte.wholefill.no.ahead", // shapecheck fire signal (D3336 A2 (iii)): producers ahead, the CTE pulled
            (false, false) => "lower.cte.wholefill.no.pulled", // shapecheck fire signal (D3336 A2 (iii)): the whole plan pulled
        });
    }
    // D1 exact (FirstFetch), PG 18.6 CteScanNext: the statement's first ExecProcNode reaches this CteScan first and pulls
    // the CTE plan there, where a breaker producer (ExecSort, MultiExecHash, agg_fill_hash_table) reads its input whole
    if let (Some(None), Some(l)) = (producers, first_fetch_exact(px.gx, k)) {
        fed(px.gx, k).into_iter().filter(|&q| q != k).for_each(|q| px.hold(q, &l));
    }
    // D1 exact (FirstFetch), PG 18.6 CteScanNext: pulled from the statement's first fetch by readers that read it whole and
    // raise nothing (`fills_ahead_quietly`), the plan's rows and its first error are the fill's
    if let (true, Some(l)) = (fill, first_fetch_exact(px.gx, k)) {
        fed(px.gx, k).into_iter().for_each(|q| px.hold(q, &l));
    }
    // Construct C2, the accepted class: a fill whole in program order, or producers run ahead, that no licence above
    // covers holds `Px::accept`'s, each pipeline asked with what it runs (tracer only; a held licence keeps its kind)
    if crate::frame::program::stamps_on() && (fill || producers.is_some()) {
        for q in fed(px.gx, k).into_iter().filter(|&q| fill || q != k) {
            let e = crate::effects::pipeline_effects(px.gx, px.arena, q, false);
            let _ = px.accept(q, e);
        }
    }
    // D2605: a drained CTE's rank, its plan id (the statement-end drain runs the higher first, as PG's es_auxmodifytables)
    let rank = match drain {
        0 => 0,
        _ => px.gx.graph.init_plans.iter().find(|i| i.pipeline as usize == k && matches!(i.link, SubLink::Cte)).map_or(0, |i| (i.plan_id.clamp(0, 0xffff) as u64) << gravity_program::protos::CTE_RANK_SHIFT),
    };
    px.declare_proto(Proto::new(ProtoKind::Cte, k as u16).words(h, 1).arg(x as u64 | drain | ahead | rank));
    (rec, keep, h, x)
}

/// D4203: the Shape of a CTE whose plan runs a node mapping read ahead under a recheck licence, that a reader pulls
/// first from a pipeline running after an emitting pipeline (`Px::licensed_late`'s `Late::Ordered`), and whose producers
/// cannot all run before any row can leave (`licensed_first`).
pub(crate) const LICENSED_LATE: &str = "a CTE whose plan reads a group or a window fill ahead under a recheck licence, first pulled where a row may already have left the statement (D4203)";

/// D4203: the latest road (`Late`) by which a reader may pull CTE `k`'s own plan, where it runs a node
/// `Px::licensed_late` names. A CTE read inside the plan answers for its own plan (its own lowering asks this of it), so
/// its pipelines are not this one's to answer (a grouped CTE read inside an outer MATERIALIZED CTE whose own plan reads
/// nothing ahead).
pub(crate) fn plan_late(px: &Px<'_, '_>, k: usize) -> Late {
    let gx = px.gx;
    let inner: PgVec<'_, usize> = fed(gx, k).into_iter().filter(|&c| c != k && is_cte(gx, c)).flat_map(|c| fed(gx, c)).collect_in(px.mcx());
    fed(gx, k).into_iter().filter(|q| !inner.contains(q)).map(|q| px.licensed_late(q)).max().unwrap_or(Late::No)
}

/// D4203: CTE `k`'s producers (its plan's pipelines but its own) all come before the statement's first emitting pipeline
/// (`Px::ahead_of_rows`'s emit) and sit in no rerun set, its own pipeline in none either: run ahead in program order
/// (D3037's `CTE_PRODUCERS_AHEAD`), they finish before any row can leave. None is another CTE's plan, and the CTE drains
/// nothing (a data-modifying CTE's ModifyTable runs at the statement's end).
fn producers_first(px: &Px<'_, '_>, k: usize) -> bool {
    let gx = px.gx;
    let ps = gx.pipelines();
    let body: PgVec<'_, usize> = fed(gx, k).into_iter().filter(|&p| p != k).collect_in(px.mcx());
    let rerun = |p: usize| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(p as u16)));
    let init = |q: usize| gx.graph.init_plans.iter().any(|i| i.pipeline as usize == q);
    let emits = |q: usize| !matches!(gx.end_kind(q), crate::graph::EndKind::Breaker(_)) && !init(q);
    let Some(&last) = body.iter().max() else { return false };
    !body.iter().any(|&p| p >= ps.len() || rerun(p) || is_cte(gx, p)) && !rerun(k) && !drains(gx, k) && !(0..=last).any(emits)
}

/// D4203: a CTE `Px::licensed_late` names, which D3037's own gate (`producers_ahead`) does not take, runs its producers
/// ahead in program order (`CTE_PRODUCERS_AHEAD`) under the recheck licence its mapping took: the node read ahead (a sorted group folding every group before it hands the first on, where PostgreSQL's
/// GroupAggregate hands each group on before it reads the next one's rows) runs before any row can leave, so its error
/// finds nothing sent and the plan-order run answers (exec admit `recheck`), where the group streams as PostgreSQL's.
/// Pulled at a reader's first row instead (a semi nest's inner behind a UNION ALL arm that sent a row), the fold's error
/// on a group PostgreSQL's reader never reaches was the statement's. The producers come first (`producers_first`) and
/// neither they nor what feeds them call user code (`effects::fed_user_code`, D2573: a PL raise can leave SPI state no
/// plan-order run holds) nor hide a call from that class in a statement that calls user code (`Px::run_ahead`'s D3587);
/// the licence is asked with the statement's acting set (`Px::batch_effects`: a VOLATILE call or a write anywhere denies
/// it). `None`: refused by `LICENSED_LATE` on the `Late::Ordered` road, pulled as before on `Late::Unanswered`.
fn licensed_first(px: &mut Px<'_, '_>, k: usize) -> Option<Licence> {
    use gravity_decompose::effects::{licence, Departure, Effects};
    let gx = px.gx;
    let body: PgVec<'_, usize> = fed(gx, k).into_iter().filter(|&p| p != k).collect_in(px.mcx());
    let user = |q: usize| crate::effects::fed_user_code(gx, q) || crate::effects::hides_user_code(gx, q) && crate::effects::statement_user_code(gx, px.arena);
    if !producers_first(px, k) || body.iter().any(|&q| user(q)) {
        return None;
    }
    let l = licence(px.batch_effects(Effects::RAISES), Departure::Ahead).ok()?;
    for &q in &body {
        px.licensed(q, &l);
        px.hold(q, &l);
    }
    Some(l)
}

fn word(b: &mut FunctionBuilder<'_>, env: &Env, w: u16) -> Value {
    b.load(Type::I64, env.params, w as i32 * 8)
}

fn set_word(b: &mut FunctionBuilder<'_>, env: &Env, w: u16, v: Value) {
    b.store(env.params, w as i32 * 8, v);
}

/// A CTE's filler or reader runs on the leader in the plan's order (the pull runs there).
fn serial(d: Pipeline, reason: Why) -> Pipeline {
    match d.is_eligible() {
        true => Pipeline { parallel: Parallel::Ineligible { site: d.driving_site, reason }, ..d },
        false => d,
    }
}

// ----- the end -----

struct CteEnd<'g> {
    rec: Rec<'g>,
    /// D3040 (`CTE_FILL_PARALLEL`): filled whole at its pipeline's width into an owned spool, never pulled (no budget).
    fill: bool,
    /// D3336 (A2 (iii)): a `cte.wholefill` fill a reader reads in the store's order (effects::visible::ctescan
    /// `order_free`): its own pipeline runs serial, so the store holds PostgreSQL's order.
    ordered: bool,
    /// D2633: the columns the store keeps (`kept`); `None`: every column.
    keep: Option<PgVec<'g, i16>>,
    x: u16,
}

fn make_end<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    cte_of(px, k, node);
    let (rec, keep, _, x) = object(px, k, node);
    eager_resets(px, k, x, node);
    let fill = px.protos.iter().any(|p| p.kind == ProtoKind::Cte && p.pipeline as usize == k && p.arg & gravity_program::protos::CTE_FILL_PARALLEL != 0);
    let ordered = fill && wholefill_only(px, k) && readers_of(px.gx, k).iter().any(|&p| !crate::effects::visible::ctescan::order_free(px.gx, p));
    mcx::pg_box_dyn!(px.mcx(), CteEnd { rec, fill, ordered, keep, x } => dyn EndMech + 'g)
}

/// The role of a CTE's eager store words (`eager_resets`).
const EAGER_STORE: &str = "a CTE's eager store count and pull words (D1385)";

/// D1385's eager form, PG ExecReScanCteScan's `tuplestore_clear` (the plan's chgParam set): every run of the rerun set
/// holding CTE `k` (`eager`: one set) starts an empty store, its count and its pull's words (from `x`) zero. They are the
/// lifetime home's words of that set's owner run (`Scope::OwnerRun`), stored by the owner just before each run (rerun/sets.rs
/// `Rerun::run`), which then runs the CTE's pipeline whole, never resumed. The records' by-reference copies stay in the
/// spool's arena to the statement's end (a value read from them may be held above).
fn eager_resets(px: &mut Px<'_, '_>, k: usize, x: u16, node: i32) {
    let Some(set) = px.gx.graph.reruns.iter().position(|r| r.pipelines.contains(&(k as u16))) else { return };
    // a kept member runs at the set's first run only (graph.kept): its store would be cleared under its rows
    if px.gx.graph.kept.contains(&(k as u16)) {
        bug(node, "a CTE in its rerun set's kept members (D1385's eager form re-runs the CTE whole at every run)");
    }
    for w in [x - 1, x + W_BUDGET, x + W_GOT, x + W_DONE] {
        let place = crate::lifetime::Place::Param { off: w as i32 * 8, ty: Type::I64, init: 0 };
        let d = crate::lifetime::Declared { what: EAGER_STORE, scope: crate::lifetime::Scope::OwnerRun { set: set as u16 }, place };
        if px.lifetimes.declare(d).is_err() {
            bug(node, "a CTE's eager store words declared after its rerun set's owner captured its run, or in another role (D1385)");
        }
    }
}

impl<'g> EndMech for CteEnd<'g> {
    /// PG `tuplestore_puttupleslot` (CteScanNext's `eof_cte` read): the row stored and counted; the pull's budget taken, a
    /// resumable stop after it. A zero budget (no reader's pull: the statement-end drain) never stops.
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        match &self.keep {
            None => self.rec.append(b, env, row),
            Some(keep) => {
                let cells: PgVec<'_, Option<CellRef<'_>>> = keep.iter().map(|&a| row.col(a)).collect_in(env.mcx);
                self.rec.store(b, env, &cells[..], true);
            }
        }
        if self.fill {
            // D3040: no pull, no budget; the parameter words are read-only to the fill's workers
            let out = b.int_bin(IntBinOp::Add, row.carry.out, env.one);
            return Carry { out, accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) };
        }
        let got = word(b, env, self.x + W_GOT);
        let got = b.int_bin(IntBinOp::Add, got, env.one);
        set_word(b, env, self.x + W_GOT, got);
        let budget = word(b, env, self.x + W_BUDGET);
        let out = b.int_bin(IntBinOp::Add, row.carry.out, env.one);
        let carry = Carry { out, accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) };
        let (taken, set) = (b.icmp(IntCond::Uge, got, budget), b.icmp(IntCond::Ne, budget, env.zero));
        let (full, l_stop, l_on) = (b.int_bin(IntBinOp::And, taken, set), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(full, (l_stop, &[]), (l_on, &[]));
        b.switch_to(l_stop);
        let st = b.iconst(Type::I64, gravity_abi::status::suspend(false));
        env.stop(b, &carry, row.next, st);
        b.switch_to(l_on);
        carry
    }

    /// One filler, in the plan's order; D3040's owned fill at the pipeline's own width.
    fn pipeline_desc(&self, _px: &Px<'_, '_>, _k: usize, d: Pipeline) -> Pipeline {
        match self.fill && !self.ordered {
            true => d,
            false => serial(d, Why::Sink),
        }
    }
}

// ----- the reader -----

struct CteRows<'g> {
    rec: Rec<'g>,
    /// D2633: the columns the store keeps (`kept`), and the CTE row's width; `None`: every column.
    keep: Option<(PgVec<'g, i16>, usize)>,
    h: u16,
    x: u16,
    pull: Pull,
    /// frame::iface's bound on the rows this reader reads (a Limit's count + offset over it): no pull goes past it.
    bound: Option<u64>,
    cursor: i32,
    win: Once<bool>,
    node: i32,
    /// D1385's eager form: the set ran the CTE whole before this reader; past the stored rows is the end (no pull).
    eager: bool,
    /// D3256: a reader of a store filled whole and sealed (D3040 `CTE_FILL_PARALLEL`, an owned spool) whose rows no order
    /// changes (`claimable`) reads it by record claims (`nest.spool.chunks`, as gsets_spool.rs's reader): the fill's site,
    /// nominal; `None` reads the store in one stream on the leader.
    chunks: Option<usize>,
    /// The fill's predicted rows (a claims reader's).
    rows: u64,
    /// D3336 (A2 (iii)): a claims reader into a sort's runs (`Px::wide`; wholefill.rs `sort_reader`).
    sorts: bool,
}

/// D3256: a claims reader's nominal driving site (exec reads no row through it, runner/plan.rs cuts it no claims): the
/// fill's (`q`, the CTE's pipeline), else the first site of the CTE's plan, as a wide emit names its producer's: a fill out
/// of a group's table or a union's slot has no site of its own.
fn fill_site(px: &Px<'_, '_>, q: usize) -> Option<usize> {
    px.pipeline_site(q).or_else(|| fed(px.gx, q).into_iter().find_map(|p| px.pipeline_site(p)))
}

/// D3256: whether reader `k` of a sealed store may read it by record claims, on many workers: what it hands on no row order
/// changes (a heap scan's rows come in any order at any width; the multiset ruling), so it ends in a hashed or plain fold
/// whose result the order of the rows does not show (effects::visible `wide_visible`: an order-free fold, no LIMIT, lock,
/// visible call, first-row or fold error) through filters, gates, projections and hash probes, or in a hash build through
/// filters, gates and projections with no visible call: the ends a wide group emit takes over its nominal site too
/// (table/emit_group.rs), and the grouping-set spool's readers. A sort or top-k end stays serial: its run takes section ends
/// only from a claim-driven source or a wide emit (ops/sort/lower.rs `wide_row_end`), and a top's serial run under claims
/// read a null run (R-KEEP c-random rnd-3914, SIGSEGV in sort_last_res at W0 and W8). No streamed group, merge join,
/// window, set, SubPlan or append reads it: one may read the store's order (PostgreSQL 17+ carries a CTE's pathkeys up to
/// its CteScan), and the client's sink keeps the reader serial (the rows' order there is the statement's).
fn claimable(gx: &Gx<'_, '_>, k: usize) -> bool {
    use gravity_decompose::join::JoinDesc;
    let ops = &gx.pipelines()[k].ops;
    if record_row(gx, k) {
        return false;
    }
    let plain = |op: &Op<'_>| matches!(op, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    let probe = |op: &Op<'_>| matches!(op, Op::Join(JoinDesc::Probe { .. }));
    match crate::frame::iface::end_breaker(gx, k) {
        Some(BreakerKind::HashAgg | BreakerKind::PlainAgg) => ops.iter().all(|o| plain(o) || probe(o)) && crate::effects::visible::wide_visible(gx, k).is_none(),
        Some(BreakerKind::HashBuild) => ops.iter().all(|o| plain(o) || matches!(o, Op::Join(JoinDesc::Build { .. }))) && crate::effects::visible::calls_visible(gx, k).is_none(),
        _ => false,
    }
}

/// Whether reader `k` makes a `record` whole row (decompose record_row.rs): it is blessed in the leader's record cache, which
/// a pool thread does not see: 'record type has not been registered' (v4/m8_cte_wholerow at W8).
fn record_row(gx: &Gx<'_, '_>, k: usize) -> bool {
    let p = &gx.pipelines()[k];
    gx.graph.record_rows.iter().any(|r| r.node_id == p.node_id || p.node_ops.iter().any(|n| n.node_id == r.node_id))
}

/// The rows one pull asks of the CTE's plan.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pull {
    /// One row per miss (PG's CteScanNext): a plan that calls user or VOLATILE code, locks rows or writes.
    One,
    /// A quiet plan: WINDOW rows.
    Window,
    /// D3256 (`cte.batch`): WINDOW rows of a plan that may raise, whose pulled pipeline reads a breaker and calls no user or
    /// VOLATILE code (`batches`): its producers run whole at the first pull whatever the budget, so only that pipeline's
    /// rows run ahead of the readers, a raise among them held as `Grow`'s. The licence it runs ahead under (`grows`'s).
    Batch(gravity_decompose::effects::Licence),
    /// A plan that may raise but runs no user or VOLATILE code, locks and writes nothing (`grows`): as many rows as are
    /// stored (1 at the first pull, so the store doubles), at most WINDOW. Its plan never runs more than the rows its
    /// readers read again ahead of PG's, and a raise past the read rows is held by the CTE object (exec ops/ctescan
    /// proto.rs `pull`, answer `HELD`) until a reader reads past the rows stored before it, as PG raises it there.
    /// Construct C2: the licence it runs ahead under (`grows`'s, or a quiet plan's `effects::unobservable`).
    Grow(gravity_decompose::effects::Licence),
}

/// The CTE object's answer for an error it holds behind the rows its pull stored (exec ops/ctescan/proto.rs `HELD`).
const HELD: i64 = 2;

/// `Pull::Grow` for CTE `k` (InitPlan `i`), not quiet: the effects licence to run ahead (no VOLATILE call by the InitPlan's
/// effects, no lock or write; never in the plan-order run), and every pipeline of its pulled set and of the recursive
/// union's term set (`RECURSIVE_SET`) reads a table, a constant, a VALUES list or a slot through filters, gates, projections, set-operation steps and unpruned union arms whose
/// expressions hold no user code, no value of a user type (so no key a step compares is one) and no VOLATILE call (the
/// effects home's `user_fn`, which reads decompose's `effects::user_code` as D439's cap does, `user_typed`, `volatile`, `volatile_check`), with no group, ending in a store. Anything
/// else, fail closed, pulls one row per miss: user code's raise leaves SPI, GUC or snapshot state the statement cannot
/// hold (D2044), so no row of it may run ahead to be held.
fn grows(gx: &Gx<'_, '_>, k: usize, i: &InitPlan<'_>) -> Option<gravity_decompose::effects::Licence> {
    use crate::graph::EndKind;
    use types_nodes::Node;
    let still = |n: Node<'_>| !crate::effects::user_fn(gx, n) && !crate::effects::user_typed(gx, n) && !crate::effects::volatile(gx, n) && !crate::effects::volatile_check(gx, n);
    // the effects licence for running the plan ahead of its readers (denied in the plan-order run, `deny_all`); its recheck
    // is not taken: the held error is raised where PG raises it, so no re-run repairs anything
    use gravity_decompose::effects::{licence, Departure, Effects};
    let own = Effects::of_init_plan(i);
    let acts = own.acts || drains(gx, k) || feeds(gx, k, |op| matches!(op, Op::Lock(_)));
    let l = licence(Effects { acts, ..own }, Departure::Ahead).ok()?;
    // the statement's one recursive union's term set (D2212), whichever CTE it is: checking more only fails closed
    let term = gx.graph.reruns.iter().filter(|r| r.plan_id == RECURSIVE_SET).flat_map(|r| r.pipelines.iter().map(|&m| m as usize));
    let grows = fed(gx, k).into_iter().chain(term).all(|q| {
        let p = &gx.pipelines()[q];
        let src = match &p.source {
            Source::Range { .. } | Source::Const | Source::Breaker { .. } => true,
            Source::Values { lists, .. } => lists.iter().all(still),
            _ => false,
        };
        let end = matches!(gx.end_kind(q), EndKind::Sink | EndKind::Breaker(BreakerKind::Materialize));
        src && end && gx.group_at(q).is_none()
            && p.ops.iter().all(|op| match op {
                Op::Filter(n) | Op::Gate(n) => still(*n),
                Op::Project(l) => l.iter().all(still),
                Op::SetOp(_) => true,
                Op::Append(AppendDesc::Arm { prune: None, .. } | AppendDesc::Feed { .. }) => true,
                _ => false,
            })
    });
    grows.then_some(l)
}

/// D3256 (`cte.batch`; a grouped CTE read by an InitPlan and by a merge join's sort): CTE `k` (InitPlan `i`) that
/// may raise is pulled `WINDOW` rows at a time when the pipeline a pull runs reads a breaker's slot and runs only filters,
/// gates and projections that call no user or VOLATILE code and hold no user-typed value (`grows`'s `still`), with no
/// group of its own, ending in the store, and every aggregate whose final it evaluates is a builtin plain one
/// (`effects::builtin_plain_agg`: initdb's code, a numeric sum's overflow all it may raise). Its producers run whole at
/// the first pull whatever the budget (driver `Entry::pull_nested`), where PostgreSQL's HashAggregate, Sort or Hash read
/// their input whole at their first call, so only that pipeline's rows run ahead of the readers, and a raise among them
/// is held by the CTE object behind the rows stored before it (exec ops/ctescan/proto.rs `pull`, answer `HELD`, any pull
/// of more than one row) until a reader reads past them, where PostgreSQL raises it. The licence is `grows`'s (the
/// plan's effects: no VOLATILE call, no lock or write; never in the plan-order run). Pulled one row per miss, the
/// 100,000 groups of a grouped CTE cost a driver call each (`Pull::One`); `grows` refuses the plan for its group.
fn batches(gx: &Gx<'_, '_>, k: usize, i: &InitPlan<'_>) -> Option<Licence> {
    use crate::graph::EndKind;
    use gravity_decompose::effects::{licence, Departure, Effects};
    use types_nodes::Node;
    if crate::face::strategy::off().has(CTE_BATCH) {
        return None;
    }
    let still = |n: Node<'_>| !crate::effects::user_fn(gx, n) && !crate::effects::user_typed(gx, n) && !crate::effects::volatile(gx, n) && !crate::effects::volatile_check(gx, n);
    let p = &gx.pipelines()[k];
    let own = matches!(p.source, Source::Breaker { .. })
        && gx.group_at(k).is_none()
        && matches!(gx.end_kind(k), EndKind::Sink | EndKind::Breaker(BreakerKind::Materialize))
        && p.ops.iter().all(|op| match op {
            Op::Filter(n) | Op::Gate(n) => still(*n),
            Op::Project(l) => l.iter().all(still),
            _ => false,
        });
    let finals = || fed(gx, k).into_iter().filter(|&q| q != k).all(|q| gx.group_at(q).is_none_or(|g| g.aggs.iter().all(|a| crate::effects::builtin_plain_agg(a.node))));
    if !own || !finals() {
        return None;
    }
    let e = Effects::of_init_plan(i);
    let acts = e.acts || drains(gx, k) || feeds(gx, k, |op| matches!(op, Op::Lock(_)));
    let l = licence(Effects { acts, ..e }, Departure::Ahead).ok()?;
    fires::fire("lower.cte.batch"); // shapecheck fire signal (D3256): a pulled CTE takes WINDOW rows a pull
    Some(l)
}

fn make_source<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let &Source::Breaker { slot } = &p.source else { bug(p.node_id, "a CteScan that reads no slot") };
    let Some(life) = gx.life_of(k, slot) else { bug(p.node_id, "a CteScan over no CTE's slot") };
    let q = life.0 as usize;
    let cte = cte_of(px, q, p.node_id);
    let pull = match crate::effects::visible::ctescan::quiet(gx, q, cte) {
        true => Pull::Window,
        false => batches(gx, q, cte).map(Pull::Batch).or_else(|| grows(gx, q, cte).map(Pull::Grow)).unwrap_or(Pull::One),
    };
    let node = p.node_ops.first().map_or(p.node_id, |n| n.node_id);
    let bound = match px.request(node).bound {
        crate::frame::iface::Bound::Rows(r) => Some(r),
        _ => None,
    };
    // setop_rec-0043: the bound counts the CteScan's output; its own qual drops stored rows the bound never counts, so its
    // pulls grow with the store (`Pull::Grow`'s doubling, a quiet plan's too), never capped by the bound: capped, a filter
    // under a LIMIT pulled one row (one recursive generation) a pull
    let own = p.node_ops.get(1).map_or(p.ops.len(), |n| n.first_op as usize);
    let (pull, bound) = match bound {
        Some(_) if p.ops[..own.min(p.ops.len())].iter().any(|o| matches!(o, Op::Filter(_))) => (match pull {
            Pull::Window => gravity_decompose::effects::unobservable(gravity_decompose::effects::Effects::NONE).map_or(pull, Pull::Grow),
            Pull::Batch(l) => Pull::Grow(l),
            p => p,
        }, None),
        b => (pull, b),
    };
    if matches!(pull, Pull::Batch(_)) {
        // the road's own signal: `batches` fires lower.cte.batch when it grants the licence, before a bound turns the
        // pull into `Pull::Grow`; this one fires only where the reader keeps `Pull::Batch`
        fires::fire("cte.batch");
    }
    let (rec, keep, h, x) = object(px, q, p.node_id);
    let keep = keep.map(|keep| (keep, cols(px, q, p.node_id).len()));
    let cursor = px.frame_word(WordKey::Open { face: Face::Source, k, j: u64::MAX, i: 0 }) as i32 * 8;
    let filled = px.protos.iter().any(|c| c.kind == ProtoKind::Cte && c.pipeline as usize == q && c.arg & gravity_program::protos::CTE_FILL_PARALLEL != 0);
    // D3256: a sealed store's order-free reader claims record ranges of it (the fill's site, nominal), as gsets_spool.rs's
    // readers claim the grouping-set spool's
    // D3336 (A2 (iii), `cte.wholefill`): or a reader into a sort's runs (wholefill.rs `sort_reader`), each claim filling
    // its own run, sealed at its section end, as over a wide group emit (`Px::wide`)
    let sort_ok = || !record_row(gx, k) && crate::ops::ctescan::wholefill::sort_reader(px, k);
    let chunks = fill_site(px, q).filter(|_| filled && rec.is_owned() && k < gx.pipelines().len() && !crate::face::strategy::off().has(CTE_CLAIMS) && (claimable(gx, k) || sort_ok()));
    let sorts = chunks.is_some() && !claimable(gx, k);
    if sorts {
        px.wide.push_in(k);
        fires::fire("lower.cte.wholefill.sort"); // shapecheck fire signal (D3336 A2 (iii)): a CTE reader's sort filled by record claims
    }
    if chunks.is_some() {
        let (rh, _) = rec.words();
        px.declare_proto(Proto::new(ProtoKind::SpoolChunks, k as u16).peer(q as u16).words(rh, 1));
        fires::fire("lower.cte.claims"); // shapecheck fire signal (D3256): a CTE reader reads its sealed store by record claims
        fires::fire("cte.claims"); // the road's own name, apart from cte.batch's in this file
    }
    let rows = gx.pipelines()[q].desc.predicted_rows;
    mcx::pg_box_dyn!(px.mcx(), CteRows { rec, keep, h, x, pull, bound, cursor, win: Once::new(false), node: p.node_id, eager: rerun_member(gx, q) || filled, chunks, rows, sorts } => dyn SourceMech + 'g)
}

impl<'g> CteRows<'g> {
    /// D3256: this claim's records `[lo, hi)`, both clamped to the sealed count (a whole run's `[0, u64::MAX)`, a section
    /// end's `[u64::MAX, u64::MAX)`: no record), as gsets_spool.rs's reader reads its claim.
    fn claim(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> (Value, Value) {
        let n = self.rec.count(b, env);
        let lo = b.load(Type::I64, env.ctx, gravity_abi::ctx::hdr::CLAIM_LO as i32);
        let hi = b.load(Type::I64, env.ctx, gravity_abi::ctx::hdr::CLAIM_HI as i32);
        let (lo_past, hi_past) = (b.icmp(IntCond::Ugt, lo, n), b.icmp(IntCond::Ugt, hi, n));
        (b.select(lo_past, n, lo), b.select(hi_past, n, hi))
    }

    /// The rows this pull asks for (`Pull`; `n` the stored count, `cur` the read pointer at it), never past `bound`.
    fn budget(&self, b: &mut FunctionBuilder<'_>, env: &Env, n: Value, cur: Value) -> Value {
        let w = b.iconst(Type::I64, WINDOW);
        let v = match self.pull {
            Pull::One => return b.iconst(Type::I64, 1),
            Pull::Window | Pull::Batch(_) => w,
            Pull::Grow(_) => {
                let g = b.icmp(IntCond::Sgt, n, env.one);
                let g = b.select(g, n, env.one);
                let over = b.icmp(IntCond::Sgt, g, w);
                b.select(over, w, g)
            }
        };
        let Some(r) = self.bound else { return v };
        let r = b.iconst(Type::I64, i64::try_from(r).unwrap_or(i64::MAX));
        let rem = b.int_bin(IntBinOp::Sub, r, cur);
        let low = b.icmp(IntCond::Slt, rem, env.one);
        let rem = b.select(low, env.one, rem);
        let over = b.icmp(IntCond::Sgt, v, rem);
        b.select(over, rem, v)
    }

    /// The window of stored rows from the read pointer `cur` (`l_win`'s parameter: the stored count), at most WINDOW rows.
    fn window(&self, b: &mut FunctionBuilder<'_>, l_win: Block, cur: Value) -> Win {
        b.switch_to(l_win);
        let total = b.block_params(l_win)[0];
        self.win.set(true);
        let w = b.iconst(Type::I64, WINDOW);
        let far = b.int_bin(IntBinOp::Add, cur, w);
        let past = b.icmp(IntCond::Sgt, far, total);
        let end = b.select(past, total, far);
        Win { cursor: cur, end, facts: WinConds::default() }
    }
}

impl<'g> SourceMech for CteRows<'g> {
    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("nest.cte read pointer", Base::Words, self.cursor, Type::I64, 0));
    }

    fn open(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        env.zero
    }

    /// PG CteScanNext: the stored rows past the read pointer, else one pull of the CTE (its rows then stored), else the end.
    /// D3256: a claims reader reads its claim's records from the resume position, at most WINDOW (gsets_spool.rs's reader).
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        if self.chunks.is_some() {
            let (lo, hi) = self.claim(b, env);
            let at = crate::frame::resume::resume_pos(b, env);
            let cur = b.int_bin(IntBinOp::Add, lo, at);
            let (done, l_go) = (b.icmp(IntCond::Sge, cur, hi), b.create_block(&[]));
            b.cond_br(done, (l_eof, &[env.zero]), (l_go, &[]));
            b.switch_to(l_go);
            self.win.set(true);
            let w = b.iconst(Type::I64, WINDOW);
            let far = b.int_bin(IntBinOp::Add, cur, w);
            let lt = b.icmp(IntCond::Slt, far, hi);
            let end = b.select(lt, far, hi);
            return Win { cursor: cur, end, facts: WinConds::default() };
        }
        let cur = env.load_word(b, self.cursor);
        let n = self.rec.count(b, env);
        let (more, l_win, l_past) = (b.icmp(IntCond::Slt, cur, n), b.create_block(&[Type::I64]), b.create_block(&[]));
        b.cond_br(more, (l_win, &[n]), (l_past, &[]));
        b.switch_to(l_past);
        if self.eager {
            b.br(l_eof, &[env.zero]);
            return self.window(b, l_win, cur);
        }
        let done = word(b, env, self.x + W_DONE);
        let (ended, l_pull) = (b.icmp(IntCond::Ne, done, env.zero), b.create_block(&[]));
        b.cond_br(ended, (l_eof, &[env.zero]), (l_pull, &[]));
        b.switch_to(l_pull);
        let budget = self.budget(b, env, n, cur);
        set_word(b, env, self.x + W_BUDGET, budget);
        set_word(b, env, self.x + W_GOT, env.zero);
        let f = b.declare_host_once("m8_cte_pull", &[Type::Ptr, Type::I64, Type::I64], Some(Type::I64), m8::m8_cte_pull as *const () as usize, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() });
        let hv = word(b, env, self.h);
        let r = b.call_host(f, &[env.ctx, hv, budget], Some(Type::I64)).expect("m8_cte_pull returns its status");
        set_word(b, env, self.x + W_BUDGET, env.zero); // the statement-end drain of a data-modifying CTE runs unbudgeted
        // below zero: the pull's error (the host stashed it), raised where PG raises it, inside the reader's read
        let (bad, l_bad, l_ok) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_bad);
        b.cond_br(bad, (l_bad, &[]), (l_ok, &[]));
        b.switch_to(l_bad);
        let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
        b.br(env.l_raise, &[e]);
        b.switch_to(l_ok);
        // 1: the CTE ended (sticky: later reads never pull)
        let ended = b.icmp(IntCond::Eq, r, env.one);
        let old = word(b, env, self.x + W_DONE);
        let flag = b.select(ended, env.one, old);
        set_word(b, env, self.x + W_DONE, flag);
        let n2 = self.rec.count(b, env);
        let more = b.icmp(IntCond::Slt, cur, n2);
        let l_none = b.create_block(&[]);
        b.cond_br(more, (l_win, &[n2]), (l_none, &[]));
        // HELD with no row stored before the error: a second pull answers the held error now (straight-line, no loop)
        b.switch_to(l_none);
        let h = b.iconst(Type::I64, HELD);
        let (held, l_again) = (b.icmp(IntCond::Eq, r, h), b.create_block(&[]));
        b.mark_cold(l_again);
        b.cond_br(held, (l_again, &[]), (l_eof, &[env.zero]));
        b.switch_to(l_again);
        let r2 = b.call_host(f, &[env.ctx, hv, budget], Some(Type::I64)).expect("m8_cte_pull returns its status");
        let bad2 = b.icmp(IntCond::Slt, r2, env.zero);
        b.cond_br(bad2, (l_bad, &[]), (l_eof, &[env.zero]));
        self.window(b, l_win, cur)
    }

    /// Stored row `j` (by reference: the store's copy, alive to the statement's end); D2633: a narrowed store's cells at
    /// their CTE columns' attnos, the other columns not carried.
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        if !self.win.get() {
            bug(self.node, "a CTE row before its window");
        }
        let mut row = self.rec.read(b, env, j, carry, next);
        if let Some((keep, n)) = &self.keep {
            let mut cols = mcx::pg_vec![in env.mcx; None; *n];
            for (i, &a) in keep.iter().enumerate() {
                cols[a as usize - 1] = Some(i as u16);
            }
            row.cols = cols;
        }
        row
    }

    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, _out: Value) {
        if self.chunks.is_some() {
            let lo = self.claim(b, env).0;
            let at = b.int_bin(IntBinOp::Sub, cur, lo);
            crate::frame::resume::write_pos(b, env, at);
            return;
        }
        env.store_word(b, self.cursor, cur);
    }

    /// D3256: the fill's site is a claims reader's nominal driving site (exec reads no row through it).
    fn nominal_site(&self, _px: &Px<'_, '_>) -> Option<usize> {
        self.chunks
    }

    /// On the leader (the pull runs the CTE there), but for D3256's claims reader of a sealed store. The CTE's slot stays consumed (every consumed index the pipeline's steps
    /// take is the graph's): the driver keeps it for the readers, opened zeroed as the producer order steps past the
    /// deferred plan (`Entry::skip_pulled`, exec v4/m8_cte.rs `pull_readers`); the rows are the store's.
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        let d = px.gx.pipelines()[k].desc;
        match self.chunks {
            // D3336 (A2 (iii)): a sort's claims reader the leader keeps (a pool-unsafe call: emit_group.rs `kept_on_leader`,
            // whose end then seals its run itself, `RunEnd::kept_serial`) reads its claim whole on the leader
            Some(_) if self.sorts && crate::table::emit_group::kept_on_leader(px, k) => serial(d, Why::Host),
            // D3256: a claims reader is wide (the fill's predicted rows)
            Some(s) => Pipeline { parallel: Parallel::Eligible, driving_site: s as i16, predicted_rows: self.rows, ..d },
            None => serial(d, Why::Host),
        }
    }
}

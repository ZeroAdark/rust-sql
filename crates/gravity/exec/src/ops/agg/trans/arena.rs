//! M6's per-statement protocol for v4 programs (M6-plan.md §3.11 / §6, cards/M6.md L2): `agg.arena` (V4R-269), the
//! statement's aggregate arena for the hosted aggregate calls (hosts v4/m6.rs `AggArena`: the by-reference states and
//! copies in its slots' aggcontexts, freed at the statement's end and on error: the runner drops this state on every
//! path), the handle the kernels entries (host_abi/v4/m6_agg.rs) reach it through. An arena whose aggregates all have a
//! combinefn and are `PARALLEL SAFE` keeps the program's widths (a slot per worker ordinal) and merges the plain
//! aggregate's partials here, combining two held states on the leader before the runner's own fold (`Rec::fold` moves
//! one state and raises on two); any other runs the program width 1. Created by M6-HOSTS.
use gravity_abi::breaker::{Breaker, BreakerKind, SlotKind};
use gravity_driver::{Merge, Partial, Result, State};
use gravity_hosts::v4::m6::AggArena;
use gravity_kernels::ops::agg::trans::hosted::{AGG_ORDERED_FED, AGG_PRESORTED_FED, AGG_STREAMED, AGG_WORDS, AGG_W_HEAD};
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::{End, Program};
use types_error::PgResult;
use types_nodes::plannodes::PlannedStmt;
use types_nodes::primnodes::Aggref;
use types_nodes::Node;

use crate::ops::{Live, Prepare, Protocol};
use crate::admit::{Admit, Bind};
use crate::glue;

pub const ADMIT: &[Admit] = &[];
/// PG's `AGG_PLAIN` (nodes.h `AggStrategy`): an Agg with no groups.
const AGG_PLAIN: u32 = 0;
pub const BIND: &[Bind] = &[];
pub const PROTOCOLS: &[&dyn Protocol] = &[&Arena];

pub struct Arena;

impl Protocol for Arena {
    fn kind(&self) -> ProtoKind {
        ProtoKind::AggArena
    }

    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let base = p.words.base as usize;
        if p.words.n != AGG_WORDS || base + AGG_WORDS as usize > cx.words.len() {
            return Err(glue::internal("gravity: agg.arena's words are not its declaration's"));
        }
        let pstmt = cx.admitted.pstmt.ok_or_else(|| glue::internal("gravity: agg.arena without its plan"))?;
        let agg = plan_node(pstmt, p.arg as i32).filter(|n| n.as_agg().is_some()).ok_or_else(|| glue::internal("gravity: agg.arena's node is no Agg of the plan"))?;
        let plan = agg.as_plan().expect("an Agg is a plan");
        let mut aggrefs: memledger::ChargedVec<&'static Aggref<'static>> = cx.charged_vec();
        for e in plan.targetlist.iter().chain(plan.qual.iter()) {
            // decompose agg.rs `collect`'s list: every aggregate the Agg runs, a FORMAT JSON value's unread raw one too
            gravity_decompose::expr::run_aggrefs(e, &mut |x| {
                let a = x.as_aggref().expect("run_aggrefs gives aggregates");
                if !aggrefs.iter().any(|b| b.aggno == a.aggno) {
                    aggrefs.with_vec(|v| v.push(a)); // alloc: Exec charged cx.owner
                }
            });
        }
        // D345 / D3728: the kept columns (types), the one list decompose's GroupDecl.pinned is (facts::demand `pinned`:
        // the non-junk target list, the qual, the resjunk entries that compute; each OUTER_VAR read outside an aggregate
        // once, first-seen); lowerv4 numbers them naggs + i
        let mut kept = cx.charged_vec();
        gravity_decompose::facts::demand::pinned(&plan.targetlist, &plan.qual, &mut |v| kept.with_vec(|k| k.push(v.vartype))); // alloc: Exec charged cx.owner
        // agg.arena prepares after the width protocols (frame/program.rs): the widths are the program's. A keyed record
        // holding hosted states (an owner layout with `AggState` slots) homes each group's states (V4R-375): D3062-b, a
        // wide arena's home keeps a state per worker, gathered at the final (`new_keyed`); else it is serial, its folds
        // width 1, so a group's one state advances in input order
        let keyed = cx.program.pipelines.iter().any(|q| q.owner.is_some_and(|d| d.layout.slot_kinds().contains(&SlotKind::AggState)));
        let width = cx.widths.iter().copied().max().unwrap_or(1);
        let mut arena = if keyed { AggArena::new_keyed(&aggrefs, &kept, width)? } else { AggArena::new(&aggrefs, &kept, width)? };
        let wide = arena.wide();
        if p.arg & AGG_PRESORTED_FED != 0 {
            arena.presorted_fed(); // D1596: the Agg fed in its Sort's order (lowerv4 regroup.rs)
        }
        if p.arg & AGG_ORDERED_FED != 0 {
            arena.ordered_fed(); // aggtrans T6k: the group fed in its Sort's order (lowerv4 fold/hosted.rs `ordered_fed`)
        }
        if p.arg & AGG_STREAMED != 0 && !keyed {
            arena.streamed(); // one open group at a time (lowerv4 group/stream.rs): its context resets at the next group
        }
        if !keyed && agg.as_agg().is_some_and(|a| a.aggstrategy == AGG_PLAIN && a.groupingSets.is_nil()) {
            arena.plain_fed(); // D2332: one group per open, each final's image in its aggregate's own buffer
        }
        let plain = agg.as_agg().is_some_and(|a| a.aggstrategy == AGG_PLAIN || !a.groupingSets.is_nil());
        let pipelines = plain_ends(cx.program, p.pipeline as usize, plain, cx.owner);
        if !arena.parallel() || keyed {
            // a serial arena has one slot (a named serial reason: no combinefn or not PARALLEL SAFE, PG's rule; or a keyed
            // hosted record, V4R-375): the pipelines that reach its states run width 1. D3062: only those; every other
            // pipeline of the program (a hash build, a probe that folds into no hosted state, a native fold) keeps its
            // width. A reader of a nested member is pinned to the leader (D1035), so no wide pipeline reaches them.
            // D3062-b: a wide arena's folds keep their width too; only the readers (the finals' gather) run width 1
            let reach = hosted_reach(cx.program, p.pipeline as usize, &pipelines, keyed, wide, cx.owner);
            let any_wide = |ws: &[u16]| ws.iter().any(|&w| w >= 2);
            let before = any_wide(&cx.widths[..]);
            cx.widths.iter_mut().zip(reach.iter()).filter(|(_, &r)| r).for_each(|(w, _)| *w = (*w).min(1));
            if before && wide {
                fires::fire("agg.arena.keyed.wide"); // shapecheck fire signal (D3062-b): a hosted keyed fold kept its width
            } else if before {
                // census: a wide statement this arena narrows, whole (every wide pipeline reached its states) or in part
                fires::fire(if keyed { "agg.arena.clamp.keyed" } else { "agg.arena.clamp.serial" });
                if any_wide(&cx.widths[..]) {
                    fires::fire("agg.arena.clamp.part"); // shapecheck fire signal (D3062): pipelines past the hosted states kept their width
                }
            }
        }
        cx.words[base + AGG_W_HEAD] = arena.handle();
        cx.nulls[base..base + AGG_WORDS as usize].fill(0);
        *cx.nparams = (*cx.nparams).max(base + AGG_WORDS as usize);
        Ok(Some(Box::new(Held { arena: core::cell::UnsafeCell::new(arena), at: p.pipeline as usize, pipelines, homed: core::cell::Cell::new(false), charged: core::cell::Cell::new(0) }))) // alloc: Exec
    }
}

/// One execution's arena, dropped (its slots freed, each node's shutdown callbacks first) on the leader on every path;
/// the merge of its plain aggregate's partials.
pub(crate) struct Held {
    arena: core::cell::UnsafeCell<Box<AggArena>>,
    /// the pipeline that declared the arena (the Agg's own)
    at: usize,
    /// every pipeline ending in the plain breaker the arena's aggregate folds into (an Append's arms: one each, D806)
    pipelines: memledger::ChargedVec<usize>,
    /// D1404: the leader's record's held states were rehomed into the merge slot's context (`merge_hosted`)
    homed: core::cell::Cell<bool>,
    charged: core::cell::Cell<u64>, // D1421: the arena bytes charged to the grant so far
}

impl Held {
    /// D798: the combine of two held states of one aggregate (`into`, `from`: `AGG_STATE_BYTES` slots, the aggregate's
    /// number read from `into`'s) in worker `ordinal`'s slot, the arena's `AggHead.combine` (`from` left zeroed); `Err(())`
    /// with PG's error stashed. For a keyed record fold meeting two partials of one group (owner.rs fold_pair, M6-FOLD).
    #[allow(dead_code)] // its reader is owner.rs fold_pair (M6-FOLD D798), stacked on this row
    pub(crate) fn combiner(&self, ordinal: u16) -> impl Fn(*mut u8, *mut u8) -> core::result::Result<(), ()> + '_ {
        // the owner phase runs on the leader after the workers finished (as `merge_into`)
        move |into, from| super::raw::combine_held(&self.arena, ordinal, into, from)
    }
}

impl Live for Held {
    fn answers(&self) -> u16 {
        crate::ops::ask::MERGE | crate::ops::ask::AFTER | crate::ops::ask::REOPEN
    }

    fn merge_for(&self, pipeline: usize) -> Option<&dyn Merge> {
        self.pipelines.contains(&pipeline).then_some(self as &dyn Merge)
    }

    /// D1421: the arena's growth since the last pipeline end charged to the grant; no step.
    fn after_pipeline(&self, _pipeline: usize, exec: &mut gravity_driver::Exec) -> Option<Result<()>> {
        exec.reservation().charge_growth(&self.charged, super::raw::bytes_unshared(&self.arena)); // no worker runs here
        None
    }

    /// D2289: the Agg re-opened inside its rerun set: the arena is per-open state (PG's ExecReScanAgg resets the
    /// aggcontexts), so a re-open frees the last open's states instead of keeping them to the statement's end.
    fn reopen(&self, members: &[u16]) -> PgResult<()> {
        if members.iter().any(|&m| m as usize == self.at || self.pipelines.contains(&(m as usize))) {
            // the leader re-opens the set before its members run; no worker and no merge holds the arena here
            super::raw::reset_unshared(&self.arena);
            self.homed.set(false);
        }
        Ok(())
    }
}

impl Merge for Held {
    fn merge_into(&self, leader: &mut State<'_>, partial: Partial<'_>) -> Result<()> {
        // `Sync` (raw.rs): the leader's merge alone reaches the arena here.
        super::raw::merge_hosted(leader, partial, &mut |aggno, into, from| super::raw::combine_unshared(&self.arena, aggno, into, from), &self.homed)
    }
}

/// The pipelines ending in the plain breaker whose aggregate declared agg.arena on `k` (`k`'s own end, else the plain end
/// whose breaker slot the drain `k` consumes): each arm of an Append under the aggregate ends in that one breaker, so each
/// arm's partials meet the leader's held states (D806: only the declaring arm's merge combined them, the other arms' took
/// the runner's fold, which raised on two held states). aggtrans T3c: only the drain of a plain Agg or a grouping set's
/// plain arm (`plain`) finds its end by a consumed slot; a keyed group's pipeline reads a correlated SubPlan's plain
/// breaker (record_consumed_at), which taken as this arena's made each SubPlan re-open reset the group's states and
/// homes under its records (D2289's `reopen`).
fn plain_ends(program: &Program, k: usize, plain: bool, owner: Option<(memledger::Owner, memledger::Kind)>) -> memledger::ChargedVec<usize> {
    let slot_of = |q: usize| match program.pipelines[q].ends_in {
        End::Breaker(Breaker { kind: BreakerKind::PlainAgg, slot, .. }) => Some(slot),
        _ => None,
    };
    arm_run(&slot_of, if plain { program.pipelines[k].consumes } else { &[] }, k, program.pipelines.len(), owner)
}

/// `plain_ends` over `slot_of(q)` (pipeline `q`'s plain breaker slot, if it ends in one) of `n` pipelines, `k` consuming
/// `consumes`.
fn arm_run(slot_of: &dyn Fn(usize) -> Option<u8>, consumes: &[u8], k: usize, n: usize, owner: Option<(memledger::Owner, memledger::Kind)>) -> memledger::ChargedVec<usize> {
    let (who, kind) = owner.map_or((None, memledger::Kind::Region), |(o, kd)| (Some(o), kd));
    let mut out: memledger::ChargedVec<usize> = memledger::ChargedVec::new(who, kind); // alloc: Exec charged who
    // the anchor: `k`, else the last plain end before the drain `k` on a slot it consumes
    let at = if slot_of(k).is_some() { Some(k) } else { consumes.iter().find_map(|&s| (0..k).rev().find(|&q| slot_of(q) == Some(s))) };
    let Some(at) = at else { return out };
    // the arms are consecutive pipelines ending in the one breaker (lowerv4 source/append.rs one_run); a later breaker on a
    // reused slot number is not this one
    let slot = slot_of(at);
    let first = (0..at).rev().take_while(|&q| slot_of(q) == slot).last().unwrap_or(at);
    let last = (at + 1..n).take_while(|&q| slot_of(q) == slot).last().unwrap_or(at);
    out.with_vec(|v| v.extend(first..=last)); // alloc: Exec charged owner
    out
}

/// D3062: per pipeline, whether it reaches the hosted states of the arena declared on `k`: `k` itself, the plain ends its
/// aggregate folds into (`arms`), when `keyed` every pipeline whose owner layout holds an `AggState` slot (a fold into a
/// hosted record; the owner phase combines its partials in the arena's one slot), and every reader of a breaker one of
/// those produces (the emit, the final functions' caller): a declared consumer of its life, or the sink pipeline that
/// declares nothing and so reads the nearest earlier producer's slot. D3062-b: of a `wide` arena only those readers
/// (each fold advances its worker's own state; the finals gather a group's states, one thread at a time). D4654: and only
/// one hop: a reader's own breaker holds the finals' values (each final copies its image out of the arena, kernels
/// `m6_agg_final`), so its readers reach no hosted state and keep their width (a wide group emit probing a build of an
/// avg()'s finalized groups). A reader whose product still held states would own an `AggState` record, a seed.
fn hosted_reach(program: &Program, k: usize, arms: &[usize], keyed: bool, wide: bool, owner: Option<(memledger::Owner, memledger::Kind)>) -> memledger::ChargedVec<bool> {
    let ps = &program.pipelines;
    let (who, kind) = owner.map_or((None, memledger::Kind::Region), |(o, kd)| (Some(o), kd));
    let mut seed: memledger::ChargedVec<bool> = memledger::ChargedVec::new(who, kind); // alloc: Exec charged who
    seed.with_vec(|v| v.resize(ps.len(), false)); // alloc: Exec charged owner
    for q in core::iter::once(k).chain(arms.iter().copied()) {
        if let Some(o) = seed.get_mut(q) {
            *o = true;
        }
    }
    if keyed {
        for (q, p) in ps.iter().enumerate() {
            seed[q] |= p.owner.is_some_and(|d| d.layout.slot_kinds().contains(&SlotKind::AggState));
        }
    }
    let mut on: memledger::ChargedVec<bool> = memledger::ChargedVec::new(who, kind); // alloc: Exec charged who
    if wide {
        on.with_vec(|v| v.resize(ps.len(), false)); // alloc: Exec charged owner
    } else {
        on.with_vec(|v| v.extend_from_slice(seed.as_slice())); // alloc: Exec charged owner
    }
    for q in 0..ps.len() {
        // D4654: a wide arena walks from its seeds alone, not from the readers marked on the way
        let from = if wide { seed[q] } else { on[q] || seed[q] };
        if !from || ps[q].produces().is_none() {
            continue;
        }
        for c in program.consumers_of(gravity_program::LifeId(q as u16)) {
            on[c] = true;
        }
        // the chain (program/pipeline.rs): a sink pipeline that declares nothing reads the nearest earlier producer
        if let Some(c) = (q + 1..ps.len()).find(|&c| ps[c].produces().is_some() || ps[c].ends_in == End::Sink) {
            on[c] |= ps[c].ends_in == End::Sink && ps[c].consumes.is_empty();
        }
    }
    on
}

/// The plan node numbered `id` in the statement (the main tree or a subplan).
fn plan_node(pstmt: &'static PlannedStmt<'static>, id: i32) -> Option<Node<'static>> {
    fn under(n: Option<Node<'static>>, id: i32) -> Option<Node<'static>> {
        stack_depth_core::check_stack_depth_or_panic();
        let n = n?;
        let p = n.as_plan()?;
        if p.plan_node_id == id {
            return Some(n);
        }
        let kids = [n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|m| &m.mergeplans)];
        under(p.lefttree, id).or_else(|| under(p.righttree, id)).or_else(|| kids.into_iter().flatten().find_map(|l| l.iter().find_map(|c| under(Some(c), id)))).or_else(|| n.as_subquery_scan().and_then(|s| under(s.subplan, id)))
    }
    under(pstmt.planTree, id).or_else(|| pstmt.subplans.iter().find_map(|s| under(s, id)))
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use gravity_abi::breaker::{HashAgg, RecordLayout, SlotKind, AGGSTATE_HELD};
    use gravity_driver::{BorrowedBatch, Flow, Partial, Sink, SinkRef, State};
    use gravity_kernels::ops::agg::trans::hosted::{STATE_AGGNO, STATE_VALUE};

    use super::{arm_run, hosted_reach};
    use super::super::raw::merge_hosted;

    struct Discard;

    impl Sink for Discard {
        fn push(&mut self, _batch: BorrowedBatch<'_>) -> gravity_driver::Result<Flow> {
            Ok(Flow::Continue)
        }
    }

    /// A plain partial of `[Count, AggState, AggState]`: `rows` counted, aggregate 0's state `a` and 1's `b` (a word each,
    /// `None` = not held: that worker initialized no state).
    fn partial(l: &RecordLayout, rows: i64, a: Option<u64>, b: Option<u64>) -> Vec<u64> {
        let mut p = vec![0u64; HashAgg::bytes(l, 1).div_ceil(8) as usize]; // alloc: Cold
        let r = unsafe { HashAgg::at(p.as_mut_ptr().cast(), l, 1, HashAgg::bytes(l, 1)) }.single();
        r.add_count(0, rows);
        for (j, v, aggno) in [(1, a, 0u32), (2, b, 7)] {
            if let Some(v) = v {
                let s = unsafe { r.as_ptr().add(l.slot_off(j) as usize) };
                unsafe {
                    (s.add(STATE_VALUE) as *mut u64).write_unaligned(v);
                    (s.add(STATE_AGGNO) as *mut u32).write_unaligned(aggno);
                    s.add(AGGSTATE_HELD).write(1);
                }
            }
        }
        p
    }

    /// D806: at W8 and W16 every worker's partial holds hosted states; each after the first meets the leader's held one and is
    /// combined (here: added) before the runner's `Rec::fold`, whose move_state never sees two held states (its debug_assert
    /// would fire), and a worker with no state leaves the leader's as it is.
    #[test]
    fn a_plain_merge_combines_every_pair_of_held_states_at_w8_and_w16() {
        let l = RecordLayout::new(&[], &[SlotKind::Count, SlotKind::AggState, SlotKind::AggState]);
        for w in [8u64, 16] {
            let mut leader = vec![0u64; HashAgg::bytes(&l, 1).div_ceil(8) as usize + 8]; // alloc: Cold
            let mut sink = Discard;
            let mut calls = Vec::new(); // alloc: Cold
            let homed = core::cell::Cell::new(false);
            for i in 0..w {
                let p = partial(&l, 10, Some(i + 1), (i % 3 != 1).then_some(100 * (i + 1)));
                let bytes = unsafe { core::slice::from_raw_parts(p.as_ptr().cast::<u8>(), p.len() * 8) };
                let lb = unsafe { core::slice::from_raw_parts_mut(leader.as_mut_ptr().cast::<u8>(), leader.len() * 8) };
                merge_hosted(&mut State::new(lb, SinkRef::of(&mut sink)), Partial { ordinal: i as u16, bytes, rows_processed: 10 }, &mut |aggno, into, from| {
                    // D1404: a combine into an empty state is a rehome (the arena's init_parts marks it held)
                    calls.push((aggno, unsafe { into.add(AGGSTATE_HELD).read() } != 0));
                    unsafe {
                        (into.add(STATE_VALUE) as *mut u64).write_unaligned((into.add(STATE_VALUE) as *const u64).read_unaligned() + (from.add(STATE_VALUE) as *const u64).read_unaligned());
                        (into.add(STATE_AGGNO) as *mut u32).write_unaligned(aggno);
                        into.add(AGGSTATE_HELD).write(1);
                    }
                    Ok(())
                }, &homed)
                .unwrap();
            }
            let r = unsafe { HashAgg::view(leader.as_ptr().cast()) }.record(0);
            let word = |j: usize| unsafe { (r.as_ptr().add(l.slot_off(j) as usize + STATE_VALUE) as *const u64).read_unaligned() };
            let held = (0..w).filter(|i| i % 3 != 1).map(|i| 100 * (i + 1)).sum::<u64>();
            assert_eq!((r.count(0), word(1), word(2)), (10 * w as i64, w * (w + 1) / 2, held), "W{w}");
            let pairs = (w - 1) as usize + (0..w).filter(|i| i % 3 != 1).count() - 1;
            assert_eq!(calls.iter().filter(|c| c.1).count(), pairs, "W{w}: one combine per partial meeting a held leader state");
            assert_eq!(calls.iter().filter(|c| !c.1).map(|c| c.0).collect::<Vec<_>>(), [0, 7], "W{w}: D1404: each state the leader takes whole is rehomed once");
        }
    }

    /// D806 (partitioned): every arm of an Append under the plain aggregate (consecutive pipelines on the one breaker) takes the
    /// hosted merge, found from the declaring arm or from the drain; a later breaker reusing the slot number does not.
    #[test]
    fn every_append_arm_on_the_plain_breaker_takes_the_hosted_merge() {
        // pipelines: 0 other end, 1..=3 the arms (slot 2), 4 the drain consuming slot 2, 5 another plain end on slot 2
        let ends = [None, Some(2u8), Some(2), Some(2), None, Some(2)];
        let slot_of = |q: usize| ends[q];
        assert_eq!(arm_run(&slot_of, &[], 1, ends.len(), None).as_slice(), [1, 2, 3], "the declaring first arm");
        assert_eq!(arm_run(&slot_of, &[], 3, ends.len(), None).as_slice(), [1, 2, 3], "a later arm");
        assert_eq!(arm_run(&slot_of, &[2], 4, ends.len(), None).as_slice(), [1, 2, 3], "the drain");
        assert_eq!(arm_run(&slot_of, &[7], 4, ends.len(), None).as_slice(), Vec::<usize>::new(), "a drain of no plain end");
        assert_eq!(arm_run(&slot_of, &[], 4, ends.len(), None).as_slice(), Vec::<usize>::new(), "T3c: a keyed group reading a SubPlan's plain breaker");
        assert_eq!(arm_run(&slot_of, &[], 5, ends.len(), None).as_slice(), [5], "the reused slot's own breaker");
    }

    /// D3062: a hash build (0), a probe folding into a keyed hosted record (1), an independent native keyed fold (2), and the
    /// emit reading both (3): the hosted fold and its reader reach the arena's states, the build and the native fold keep
    /// their width; a sink pipeline that declares nothing reads the nearest earlier producer (the chain).
    #[test]
    fn only_the_hosted_folds_and_their_readers_reach_the_arena() {
        use gravity_abi::breaker::{Breaker, BreakerKind, KeyKind, ORDER_NONE};
        use gravity_program::{Arena, Code, End, Layout, OwnerDesc, Pipeline, Program, Reason, SiteDesc, WritePlan};
        let st = Arena::new(1 << 16);
        let sites = [SiteDesc::range_source(st.handle(), 7, &[1])];
        let layout = Layout { nsites: 1, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 };
        let hosted = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count, SlotKind::AggState]);
        let native = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count, SlotKind::SumI64]);
        let brk = |k, s| End::Breaker(Breaker::sized(k, 64).in_slot(s));
        let program = |pipes: Vec<Pipeline>| Program::with_pipelines(layout, &sites, &[], WritePlan::SELECT, pipes.into_iter().map(|p| (p, Code::hand(crate::ops::ctescan::raw::test_noop))));
        let p = program(vec![ // alloc: Cold
            Pipeline::eligible(0, brk(BreakerKind::HashBuild, 0), 10),
            Pipeline::eligible(0, brk(BreakerKind::HashAgg, 1), 10).consuming(&[0]).owner(OwnerDesc::generic(hosted, ORDER_NONE, 0)),
            Pipeline::eligible(0, brk(BreakerKind::HashAgg, 2), 10).owner(OwnerDesc::generic(native, ORDER_NONE, 0)),
            Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[1, 2]),
        ]);
        assert_eq!(hosted_reach(&p, 1, &[], true, false, None).as_slice(), [false, true, false, true], "keyed: the hosted fold and its emit");
        assert_eq!(hosted_reach(&p, 1, &[], true, true, None).as_slice(), [false, false, false, true], "D3062-b wide: the emit alone");
        assert_eq!(hosted_reach(&p, 3, &[], false, false, None).as_slice(), [false, false, false, true], "a serial arena declared on the emit alone");
        let chain = program(vec![ // alloc: Cold
            Pipeline::eligible(0, brk(BreakerKind::HashBuild, 0), 10),
            Pipeline::eligible(0, brk(BreakerKind::PlainAgg, 1), 10).consuming(&[0]),
            Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite),
        ]);
        assert_eq!(hosted_reach(&chain, 1, &[1], false, false, None).as_slice(), [false, true, true], "the sink that declares nothing reads the plain end");
    }

    /// D4654 (a join of two aggregates): a hash build (0), a probe folding into a keyed hosted record (1: avg over an
    /// aggregate output), the finals' emit into a hash build (2), a native keyed fold (3), its emit probing the
    /// finalized build (4) and the sink (5). A wide arena reaches the emit alone: the probe two hops past the hosted
    /// fold reads final values and keeps its width, and so does the sink after it; a reader whose own record holds
    /// hosted states is a seed, so its readers do reach the arena.
    #[test]
    fn a_wide_arena_reaches_its_readers_not_their_readers() {
        use gravity_abi::breaker::{Breaker, BreakerKind, KeyKind, ORDER_NONE};
        use gravity_program::{Arena, Code, End, Layout, OwnerDesc, Pipeline, Program, Reason, SiteDesc, WritePlan};
        let st = Arena::new(1 << 16);
        let sites = [SiteDesc::range_source(st.handle(), 7, &[1])];
        let layout = Layout { nsites: 1, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 };
        let hosted = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count, SlotKind::AggState]);
        let native = RecordLayout::new(&[KeyKind::I64, KeyKind::I64], &[SlotKind::Count, SlotKind::SumI64]);
        let brk = |k, s| End::Breaker(Breaker::sized(k, 64).in_slot(s));
        let program = |pipes: Vec<Pipeline>| Program::with_pipelines(layout, &sites, &[], WritePlan::SELECT, pipes.into_iter().map(|p| (p, Code::hand(crate::ops::ctescan::raw::test_noop))));
        let q65 = program(vec![ // alloc: Cold
            Pipeline::eligible(0, brk(BreakerKind::HashBuild, 0), 10),
            Pipeline::eligible(0, brk(BreakerKind::HashAgg, 1), 10).consuming(&[0]).owner(OwnerDesc::generic(hosted, ORDER_NONE, 0)),
            Pipeline::serial(-1, brk(BreakerKind::HashBuild, 2), Reason::NoDrivingSite).consuming(&[1]),
            Pipeline::eligible(0, brk(BreakerKind::HashAgg, 3), 10).owner(OwnerDesc::generic(native, ORDER_NONE, 0)),
            Pipeline::eligible(0, brk(BreakerKind::Sort, 4), 10).consuming(&[3, 2, 0]),
            Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[4]),
        ]);
        assert_eq!(hosted_reach(&q65, 1, &[], true, true, None).as_slice(), [false, false, true, false, false, false], "wide: the finals' emit alone, not the probe of its build");
        let reheld = program(vec![ // alloc: Cold
            Pipeline::eligible(0, brk(BreakerKind::HashBuild, 0), 10),
            Pipeline::eligible(0, brk(BreakerKind::HashAgg, 1), 10).consuming(&[0]).owner(OwnerDesc::generic(hosted, ORDER_NONE, 0)),
            Pipeline::eligible(0, brk(BreakerKind::HashAgg, 2), 10).consuming(&[1]).owner(OwnerDesc::generic(hosted, ORDER_NONE, 0)),
            Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[2]),
        ]);
        assert_eq!(hosted_reach(&reheld, 1, &[], true, true, None).as_slice(), [false, false, true, true], "wide: a reader holding hosted states is a seed: its reader is reached");
    }
}

// S6 (memory.md §3): every breaker slot is pinned exactly as long as its life is read. The model below derives each
// life's lease from the finished graph alone, then replays every declaration: the slot the decomposer chose must be the
// lowest slot no lease covers (pinned no longer than read) and no live lease may cover it (pinned as long as read).
//
// A lease of the life produced by pipeline l:
//   - [l, last pushed reader]; forever when no pushed pipeline reads it or an embedded inner does (never pushed);
//   - forever from a rerun set's end when a set pipeline fills or reads it (refilled and reread per outer row);
//   - [the first range pipeline touching it + 1, driver] when an iteration's range fills or reads it (a recursive term
//     keeps every slot it touches while it is mapped, D241; the driver re-runs its generation set);
//   - forever for a CTE's slot (read by every CteScan);
//   - forever from the range pipeline touching it + 1 when a held re-run range fills or reads it (a nest's inner, with
//     or without a NestLoopParam (D4584), a correlated nested SubPlan's plan: `graph.held`, D3646; the range runs inside
//     its owner's pipeline, a lazily built fill of it at the first re-open).
// A streamed union's arms read the parent's inputs once per arm and all end in one life: its lease runs to the last arm.
// One rig per consumer kind: sort, rerun join, subplan (CTE and correlated SubPlan), setop (the iteration), union arm.
use std::fs;

use gravity_abi::breaker::BreakerKind;
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::graph::{Op, PipelineGraph, PipelineNode};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::setop::SetDesc;
use gravity_decompose::ops::shared::subplan::SubForm;
use gravity_decompose::{corpus, decompose_in};
use gravity_program::Arena;
use mcx::MemoryContext;

mod harness;

const FOREVER: usize = usize::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Consumer {
    Sort,
    Rerun,
    Subplan,
    Setop,
    Union,
}

/// One life: its slot and producer, and the ranges of declarations its lease covers (a declaration at `p` after the
/// producer is covered when some range has `lo <= p <= hi`).
struct Life {
    slot: u8,
    producer: usize,
    ranges: Vec<(usize, usize)>,
    kinds: Vec<Consumer>,
}

/// The embedded inners' reads (never pushed: read for the statement), through nested inners.
fn embedded_reads(p: &PipelineNode<'_>, out: &mut Vec<u8>) {
    for op in p.ops.iter() {
        let inner = match op {
            Op::Join(JoinDesc::Nest { inner, .. }) => Some(&**inner),
            Op::SubPlan(d) => match &d.form {
                SubForm::Nested { inner } => Some(&**inner),
                _ => None,
            },
            _ => None,
        };
        if let Some(i) = inner {
            out.extend(i.desc.consumes.iter().copied());
            embedded_reads(i, out);
        }
    }
}

/// Whether pipeline `i` feeds the life an earlier Append child declared (one life, many producers): a Feed of the
/// slot, or a streamed union's arm ending in it (every arm ends in the parent's end).
fn co_feeder(g: &PipelineGraph<'_>, i: usize) -> bool {
    let feeds = |p: &PipelineNode<'_>, s: u8| p.ops.iter().any(|o| matches!(o, Op::Append(AppendDesc::Feed { slot, .. }) if *slot == s) || matches!(o, Op::Append(AppendDesc::Arm { .. })));
    let Some(s) = g.pipelines[i].desc.produces() else { return false };
    let Some(j) = (0..i).rev().find(|&j| g.pipelines[j].desc.produces() == Some(s)) else { return false };
    feeds(&g.pipelines[i], s) && feeds(&g.pipelines[j], s) && !g.pipelines[j + 1..i].iter().any(|p| p.desc.consumes.contains(&s))
}

/// The life `slot` names at pipeline `i`: the nearest earlier producer.
fn life_at(g: &PipelineGraph<'_>, slot: u8, i: usize) -> Option<usize> {
    (0..i).rev().find(|&j| g.pipelines[j].desc.produces() == Some(slot))
}

fn lives(g: &PipelineGraph<'_>) -> Vec<Life> {
    let n = g.pipelines.len();
    let mut lives: Vec<Life> = Vec::new();
    let mut ix = vec![usize::MAX; n];
    for (i, p) in g.pipelines.iter().enumerate() {
        if let Some(s) = p.desc.produces() {
            if co_feeder(g, i) {
                ix[i] = ix[life_at(g, s, i).unwrap()];
                continue;
            }
            ix[i] = lives.len();
            let sort = g.breakers.iter().any(|d| d.producer as usize == i && matches!(d.breaker.kind, BreakerKind::Sort | BreakerKind::TopK));
            lives.push(Life { slot: s, producer: i, ranges: Vec::new(), kinds: if sort { vec![Consumer::Sort] } else { Vec::new() } });
        }
    }
    let mut last = vec![None::<usize>; lives.len()];
    for (i, p) in g.pipelines.iter().enumerate() {
        for (j, &s) in p.desc.consumes.iter().enumerate() {
            let l = p.desc.consumes_from.get(j).map(|l| l.0 as usize).or_else(|| life_at(g, s, i));
            if let Some(l) = l.filter(|&l| ix[l] != usize::MAX) {
                let e = &mut last[ix[l]];
                *e = Some(e.map_or(i, |x| x.max(i)));
            }
        }
        let mut emb = Vec::new();
        embedded_reads(p, &mut emb);
        for s in emb {
            // An inner embedded in pipeline i was mapped before i was pushed: the life it reads is the one before i.
            if let Some(l) = life_at(g, s, i + 1).filter(|&l| ix[l] != usize::MAX) {
                last[ix[l]] = Some(FOREVER);
            }
        }
    }
    for (k, life) in lives.iter_mut().enumerate() {
        life.ranges.push((life.producer, last[k].unwrap_or(FOREVER)));
    }
    // The lives a set of pipelines fills or reads; a life read from outside the set is read again per outer row.
    for r in g.reruns.iter() {
        let set = |i: usize| r.pipelines.contains(&(i as u16));
        for (i, p) in g.pipelines.iter().enumerate().filter(|(i, _)| set(*i)) {
            for &s in p.desc.consumes.iter() {
                if let Some(l) = life_at(g, s, i).filter(|&l| ix[l] != usize::MAX && !set(l)) {
                    let from = lives[ix[l]].producer + 1;
                    lives[ix[l]].ranges.push((from, FOREVER));
                }
            }
        }
    }
    let touched = |set: &dyn Fn(usize) -> bool| -> Vec<usize> {
        let mut t = Vec::new();
        for (i, p) in g.pipelines.iter().enumerate().filter(|(i, _)| set(*i)) {
            if p.desc.produces().is_some() {
                t.push(ix[i]);
            }
            for &s in p.desc.consumes.iter() {
                if let Some(l) = life_at(g, s, i).filter(|&l| ix[l] != usize::MAX) {
                    t.push(ix[l]);
                }
            }
        }
        t
    };
    for r in g.reruns.iter() {
        let end = r.pipelines.iter().map(|&p| p as usize).max().unwrap_or(0);
        let kind = if r.plan_id == 0 { Consumer::Rerun } else { Consumer::Subplan };
        for k in touched(&|i| r.pipelines.contains(&(i as u16))) {
            lives[k].ranges.push((end + 1, FOREVER));
            lives[k].kinds.push(kind);
        }
    }
    for i in g.init_plans.iter() {
        if let Some(s) = i.slot {
            if let Some(k) = life_at(g, s, n).map(|l| ix[l]) {
                let from = lives[k].producer + 1;
                lives[k].ranges.push((from, FOREVER));
                lives[k].kinds.push(Consumer::Subplan);
            }
        }
    }
    let mut iters = Vec::new();
    for (d, p) in g.pipelines.iter().enumerate() {
        for op in p.ops.iter() {
            if let Op::SetOp(SetDesc::Iter(it)) = op {
                iters.push((it, d));
            }
        }
    }
    let arm = |p: &PipelineNode<'_>| p.ops.iter().any(|o| matches!(o, Op::Append(AppendDesc::Arm { .. })));
    for (i, p) in g.pipelines.iter().enumerate().filter(|(_, p)| arm(p)) {
        let read = p.desc.consumes.iter().filter_map(|&s| life_at(g, s, i));
        for k in read.chain(p.desc.produces().map(|_| i)).map(|l| ix[l]).filter(|&k| k != usize::MAX) {
            if !lives[k].kinds.contains(&Consumer::Union) {
                lives[k].kinds.push(Consumer::Union);
            }
        }
    }
    for (it, d) in iters {
        let first = life_at(g, it.work, it.body as usize).map_or(it.body as usize, |l| l + 1);
        // held from the first range pipeline that touches it (a recursive term keeps its slots while mapped, D241)
        // to the driver (which re-runs the generation set)
        for i in first..=it.body as usize {
            for k in touched(&|j| j == i) {
                lives[k].ranges.push((i + 1, d));
                if !lives[k].kinds.contains(&Consumer::Setop) {
                    lives[k].kinds.push(Consumer::Setop);
                }
            }
        }
    }
    // D3646: a held re-run range keeps every slot its pipelines fill or read for the statement
    for &(from, end) in g.held.iter() {
        for i in from as usize..end as usize {
            for k in touched(&|j| j == i) {
                lives[k].ranges.push((i + 1, FOREVER));
            }
        }
    }
    lives
}

fn covers(life: &Life, p: usize) -> bool {
    life.producer < p && life.ranges.iter().any(|&(lo, hi)| lo <= p && p <= hi)
}

/// Replays every declaration of `g`: (lives checked per consumer kind, the violations).
fn check(g: &PipelineGraph<'_>) -> ([usize; 5], Vec<String>) {
    let lives = lives(g);
    let mut bad = Vec::new();
    for (p, node) in g.pipelines.iter().enumerate() {
        let Some(chosen) = node.desc.produces().filter(|_| !co_feeder(g, p)) else { continue };
        let mut taken = [false; 256];
        for l in lives.iter().filter(|l| covers(l, p)) {
            taken[l.slot as usize] = true;
        }
        for &c in node.desc.consumes.iter() {
            taken[c as usize] = true;
        }
        if taken[chosen as usize] {
            let who = lives.iter().find(|l| l.slot == chosen && covers(l, p)).map(|l| l.producer);
            bad.push(format!("p{p} took slot {chosen} while the life of p{} still leases it", who.map_or(-1, |x| x as i64)));
        } else if let Some(want) = (0..=255u8).find(|&s| !taken[s as usize]).filter(|&s| s != chosen) {
            bad.push(format!("p{p} took slot {chosen}, but slot {want} was free (a pin outlived its reads)"));
        }
    }
    let mut kinds = [0; 5];
    for l in lives.iter() {
        for (j, k) in [Consumer::Sort, Consumer::Rerun, Consumer::Subplan, Consumer::Setop, Consumer::Union].iter().enumerate() {
            if l.kinds.contains(k) {
                kinds[j] += 1;
            }
        }
    }
    (kinds, bad)
}

/// Every graph of the corpus: (fixture, graph dump, lives per kind, violations).
fn corpus_check() -> Vec<(String, [usize; 5], Vec<String>)> {
    let mut files = harness::fixtures_in(&harness::plans_root());
    files.extend(harness::fixtures_in(&harness::plans_root().join("regress-full")));
    harness::par_map(&files, |f| {
        let text = fs::read_to_string(f).unwrap();
        let ctx = MemoryContext::new("fixture");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { return (String::new(), [0; 5], Vec::new()) };
        let arena = Arena::new(1 << 20);
        let Ok(g) = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { return (String::new(), [0; 5], Vec::new()) };
        let (k, bad) = check(&g);
        (fx.name.clone(), k, bad)
    })
}

fn rig(kind: usize, name: &str, min: usize) {
    let (mut lives, mut graphs, mut bad) = (0, 0, Vec::new());
    for (f, k, b) in corpus_check() {
        if k[kind] > 0 {
            lives += k[kind];
            graphs += 1;
            bad.extend(b.into_iter().map(|x| format!("{f}: {x}")));
        }
    }
    println!("leases {name}: {lives} lives in {graphs} graphs, {} violations", bad.len());
    for x in bad.iter() {
        println!("violation {name}: {x}");
    }
    assert!(bad.is_empty(), "{name}: {} violations", bad.len());
    assert!(graphs >= min, "{name}: only {graphs} graphs exercise the kind");
}

#[test]
fn sort_lives_are_pinned_exactly_as_long_as_read() {
    rig(0, "sort", 100);
}

#[test]
fn rerun_join_lives_are_pinned_exactly_as_long_as_read() {
    rig(1, "rerun join", 10);
}

#[test]
fn subplan_lives_are_pinned_exactly_as_long_as_read() {
    rig(2, "subplan", 10);
}

#[test]
fn setop_iteration_lives_are_pinned_exactly_as_long_as_read() {
    rig(3, "setop iteration", 3);
}

#[test]
fn union_arm_lives_are_pinned_exactly_as_long_as_read() {
    rig(4, "union arm", 50);
}

/// Every graph, whatever its consumers: no declaration takes a leased slot or skips a free one.
#[test]
fn every_declaration_takes_the_lowest_unleased_slot() {
    let all = corpus_check();
    let bad: Vec<String> = all.iter().flat_map(|(f, _, b)| b.iter().map(move |x| format!("{f}: {x}"))).collect();
    println!("leases all: {} graphs, {} violations", all.iter().filter(|x| !x.0.is_empty()).count(), bad.len());
    for x in bad.iter() {
        println!("violation all: {x}");
    }
    assert!(bad.is_empty(), "{} violations", bad.len());
}

/// D4620: road agg.gsets.cascade named on (as `SET pgrust.gravity_road_off = '+agg.gsets.cascade'` on a server): a cascade's
/// base and cascaded sets are pinned exactly as long as read, as every life is. regress groupingsets-099 (a window over
/// GROUPING SETS ((a, b), (a)), the base's emit sorted), -041 (ROLLUP over a VALUES list) and -089 (DISTINCT over rollup x
/// rollup): the cascade's slots stay leased while a reader of the base's sorted emit is open (it reads their records), never
/// for the statement (a pin past its reads made p3 of 099 take slot 3 with slot 0 free).
#[test]
fn cascade_lives_are_pinned_exactly_as_long_as_read() {
    for name in ["groupingsets-041", "groupingsets-089", "groupingsets-099"] {
        let text = fs::read_to_string(harness::plans_root().join("regress-full").join(format!("{name}.plan"))).unwrap();
        let ctx = MemoryContext::new("fixture");
        let mut fx = corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("{name}: {e}"));
        fx.facts.roads_on = vec![gravity_decompose::ops::shared::agg::GSETS_CASCADE_ROAD];
        let arena = Arena::new(1 << 20);
        let Ok(g) = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { panic!("{name}: decompose refused it") };
        assert!(!g.cascades.is_empty(), "{name}: the road folds the coarser sets from the finer");
        let (_, bad) = check(&g);
        assert!(bad.is_empty(), "{name}: {bad:?}");
    }
}

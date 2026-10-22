//! The driver's per-pipeline program constants, built once per Node (`lives::Schedule`), answer as the per-pipeline scans
//! they replace (m4_l20: a LIMIT over 4100 Materialize arms spent 97.7% of its warm 28 s in `free_after`, which walked every
//! earlier producer after each pipeline and scanned every v4 proto for each): `FreeSchedule` ends and frees the same
//! producers' slots after the same pipelines as that walk (`free_after`'s rule, kept here as the reference), `ArmRuns` names
//! the same append.arms run as `arms_of`'s scan, and `footprint` (its own walk over every earlier producer made one due
//! pipeline per slot) reserves the same bytes as before at every grant and width. Random programs: slot numbers reused,
//! arms runs with a feeder between arms, runs that do not hold, spill-partition pipes, `live_through`, owners, rerun sets.
use gravity_driver_rig as common;


use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, KeyKind, RecordLayout, SlotKind, Spill};
use gravity_driver::lives::{ArmRuns, FreeSchedule, DUE_END, DUE_FREE};
use gravity_driver::{footprint, Grant};
use gravity_program::protos::{Proto, ProtoKind, V4Descs};
use gravity_program::{Arena, ClaimKind, Code, End, Layout, OutCol, OwnerDesc, Pipeline, Program, Reason, SiteDesc, WritePlan};

const SUBSETS: [&[u8]; 16] = [&[], &[0], &[1], &[0, 1], &[2], &[0, 2], &[1, 2], &[0, 1, 2], &[3], &[0, 3], &[1, 3], &[0, 1, 3], &[2, 3], &[0, 2, 3], &[1, 2, 3], &[0, 1, 2, 3]];

struct Rng(u64);

impl Rng {
    fn next(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n.max(1)
    }
}

fn breaker(kind: BreakerKind, slot: u8, fixed: bool) -> End {
    let budget = if fixed { Budget::Fixed(4096) } else { Budget::Share };
    End::Breaker(Breaker { kind, slot, bytes: 0, budget, spill: Spill::Partitioned { bits: 3 } })
}

fn build(pipes: Vec<Pipeline>, protos: Vec<Proto>) -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let staged = pipes.into_iter().map(|p| (p, Code::hand(k_noop)));
    let mut p = Program::with_pipelines(Layout { nsites: 1, out_natts: 3, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &sites, &core::array::from_fn::<_, 3, _>(|i| OutCol::new(["c0", "c1", "c2"][i], 20, -1, 8, true)), WritePlan::SELECT, staged);
    if !protos.is_empty() {
        p.set_v4(&V4Descs::new(protos)); // alloc: Cold
    }
    p
}

/// A random program of up to ~30 pipelines over slots 0..4: producers (some with owners, some fixed-budget), consumers of
/// slots already produced, mid-program sinks, `live_through`, spill-partition pipes, arms runs (a feeder between arms
/// sometimes), Arms protos that do not hold, rerun members.
fn random_program(rng: &mut Rng) -> Program {
    let layout = RecordLayout::new(&[KeyKind::I32], &[SlotKind::Count]);
    let target = 2 + rng.next(24) as usize;
    let mut pipes: Vec<Pipeline> = Vec::new(); // alloc: Cold
    let mut protos: Vec<Proto> = Vec::new(); // alloc: Cold
    let mut produced = 0u64;
    let consumes = |rng: &mut Rng, produced: u64, own: Option<u8>| SUBSETS[(rng.next(16) & produced & !own.map_or(0, |s| 1 << s)) as usize];
    while pipes.len() + 1 < target {
        let i = pipes.len();
        if rng.next(4) == 0 {
            // an arms run: k producers of one slot, a feeder of another slot between two of them sometimes
            let s = rng.next(4) as u8;
            let k = 1 + rng.next(5) as usize;
            let start = i;
            for a in 0..k {
                if a > 0 && rng.next(4) == 0 {
                    let f = (s + 1 + rng.next(3) as u8) % 4;
                    pipes.push(Pipeline::eligible(0, breaker(BreakerKind::Sort, f, false), 1000));
                    produced |= 1 << f;
                }
                let mut p = Pipeline::eligible(0, breaker(BreakerKind::HashAgg, s, false), 1000);
                if rng.next(2) == 0 {
                    p.owner = Some(OwnerDesc::generic(layout, 0, 0));
                }
                pipes.push(p);
            }
            produced |= 1 << s;
            let len = (pipes.len() - start) as u64;
            // a held run, or one cut short / past its end (its last pipeline then may not produce the slot)
            let arg = match rng.next(5) { 0 => len.saturating_sub(1), 1 => len + 1, _ => len };
            protos.push(Proto::new(ProtoKind::Arms, start as u16).slot(s).arg(arg));
            continue;
        }
        let mut p = match rng.next(6) {
            0 => Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(consumes(rng, produced, None)),
            k => {
                let s = rng.next(4) as u8;
                let kind = [BreakerKind::HashAgg, BreakerKind::Sort, BreakerKind::HashBuild, BreakerKind::TopK, BreakerKind::PlainAgg][(k - 1) as usize];
                let mut p = Pipeline::eligible(0, breaker(kind, s, rng.next(5) == 0), 1000).consuming(consumes(rng, produced, Some(s)));
                if rng.next(3) == 0 {
                    p.parallel = gravity_program::Parallel::Ineligible { site: 0, reason: Reason::Sink };
                }
                if kind == BreakerKind::HashAgg && rng.next(2) == 0 {
                    p.owner = Some(OwnerDesc::generic(layout, 0, 0));
                }
                produced |= 1 << s;
                p
            }
        };
        if rng.next(4) == 0 {
            p.live_through = (i + 1 + rng.next(8) as usize) as i16;
        }
        if !p.consumes.is_empty() && rng.next(4) == 0 {
            p = p.over_spill_partitions(p.consumes[0]);
        }
        if rng.next(8) == 0 {
            protos.push(Proto::new(ProtoKind::Rerun, i as u16));
        }
        pipes.push(p);
    }
    pipes.push(Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(consumes(rng, produced, None)));
    // a stray Arms proto (any start, slot, length): the rule must skip it unless it holds
    if rng.next(3) == 0 {
        protos.push(Proto::new(ProtoKind::Arms, rng.next(pipes.len() as u64) as u16).slot(rng.next(4) as u8).arg(rng.next(6)));
    }
    build(pipes, protos)
}

/// `Node::arms_of`'s scan over every proto, as it was.
fn arms_of_scan(program: &Program, slot: u8, q: usize) -> Option<core::ops::Range<usize>> {
    let v4 = program.v4.as_deref()?;
    let produces = |q: usize| program.pipelines.get(q).and_then(|x| x.produces()) == Some(slot);
    v4.protos.iter().filter(|p| p.kind == ProtoKind::Arms && p.slot == slot).map(|p| p.pipeline as usize..p.pipeline as usize + p.arg as usize).find(|run| run.contains(&q) && produces(run.start) && produces(run.end - 1))
}

/// `Node::free_after`'s walk over every earlier producer, as it was: (producer, `DUE_*`) after pipeline `p`.
fn free_after_walk(program: &Program, p: usize) -> Vec<(u32, u8)> {
    let ps = &program.pipelines;
    let mut out = Vec::new(); // alloc: Cold
    for q in 0..=p {
        if let Some(slot) = ps[q].produces() {
            let next = (q + 1..ps.len()).find(|&k| ps[k].produces() == Some(slot)).unwrap_or(ps.len());
            let parts = (q + 1..next).filter(|&k| matches!(ps[k].claims, ClaimKind::SpillPartitions { .. })).max();
            let last = match arms_of_scan(program, slot, q) {
                Some(run) => program.last_consumer(run.end - 1),
                None => program.last_consumer(q),
            };
            let flags = (if parts.unwrap_or(0).max(last) == p { DUE_END } else { 0 }) | (if last == p { DUE_FREE } else { 0 });
            if flags != 0 {
                out.push((q as u32, flags));
            }
        }
    }
    out
}

fn assert_same(program: &Program, what: &str) {
    let arms = ArmRuns::of_in(program, allocator_api2::alloc::Global);
    let frees = FreeSchedule::of(program, &arms);
    for q in 0..program.pipelines.len() {
        if let Some(slot) = program.pipelines[q].produces() {
            let scan = arms_of_scan(program, slot, q);
            assert_eq!(arms.run_of(q), scan, "{what}: pipeline {q}'s arms run");
            assert_eq!(arms.continues(q), scan.is_some_and(|r| q + 1 < r.end), "{what}: pipeline {q} continues");
        }
        assert_eq!(frees.after(q), &free_after_walk(program, q)[..], "{what}: due after pipeline {q}");
    }
}

#[test]
fn the_free_schedule_ends_and_frees_as_the_walk_did() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for k in 0..4000 {
        let program = random_program(&mut rng);
        assert_same(&program, &format!("random program {k}")); // alloc: Cold
    }
}

/// m4_l20's shape: 4100 arms of one slot (one Arms proto), the Sort over them, the emit. Every arm's slot is due at the Sort
/// (its run's last arm's last consumer), the Sort's at the emit; nothing is due after any other pipeline.
fn many_arms(arms: usize) -> Program {
    let mut pipes: Vec<Pipeline> = (0..arms).map(|_| Pipeline::eligible(0, breaker(BreakerKind::Materialize, 0, false), 12)).collect(); // alloc: Cold
    pipes.push(Pipeline::serial(-1, breaker(BreakerKind::TopK, 1, false), Reason::NoDrivingSite).consuming_one(0));
    pipes.push(Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(1));
    build(pipes, vec![Proto::new(ProtoKind::Arms, 0).slot(0).arg(arms as u64)]) // alloc: Cold
}

#[test]
fn many_arms_free_at_their_consumer() {
    let program = many_arms(4100);
    assert_same(&program, "4100 arms");
    let frees = FreeSchedule::of(&program, &ArmRuns::of_in(&program, allocator_api2::alloc::Global));
    assert!((0..4100).all(|p| frees.after(p).is_empty()), "no arm's slot is due before the Sort");
    assert_eq!(frees.after(4100).len(), 4100, "every arm's production is due after the Sort");
    assert_eq!(frees.after(4101), &[(4100, DUE_END | DUE_FREE)][..], "the Sort's slot after the emit");
}

/// `footprint` at floor-carrying and plain grants, W0 / W2 / W8, over the random programs and the 4100-arm shape: the same
/// bytes as its walk over every earlier producer reserved (the digest was taken with that walk, trunk 583e87bbf1a; re-taken
/// for D2894, whose hash floors past 8 MiB stop at each breaker's modeled need, the rest of the floor address space: the
/// third grant's bytes moved, the plain and the default-floored ones' not; re-taken for D3034, whose share divisor counts
/// share slots only: a program with a fixed slot live beside a share slot gives that share the whole grant).
#[test]
fn the_footprint_is_unchanged() {
    let grants = [Grant::of(4 << 20), Grant::of(64 << 20).with_floors(4 << 20, 8 << 20), Grant::of(1 << 30).with_floors(64 << 20, 128 << 20)];
    let mut digest = 0xcbf2_9ce4_8422_2325u64;
    let mut fold = |v: u64| digest = (digest ^ v).wrapping_mul(0x0000_0100_0000_01b3);
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let programs = (0..1500).map(|_| random_program(&mut rng)).chain([many_arms(4100)]);
    for program in programs {
        for g in grants {
            for w in [1u16, 2, 8] {
                fold(footprint(g, &program, w));
            }
        }
    }
    assert_eq!(digest, FOOTPRINT_DIGEST, "footprint digest {digest:#x}");
}

const FOOTPRINT_DIGEST: u64 = 0xa116_3649_bc6a_9b3f;

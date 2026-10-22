//! Breaker lives (refactor-ledger.md §5), each a context L under G; driver-local until FACTS lands `Program::lives`.

use core::mem::size_of;

use allocator_api2::alloc::{Allocator, Global};
use allocator_api2::vec::Vec;
use gravity_program::protos::ProtoKind;
use gravity_program::Program;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Life {
    pub id: u16,
    pub slot: u8,
    pub producer: usize,
    pub last_consumer: usize,
    pub parallel: bool,
}

pub fn lives(program: &Program) -> impl Iterator<Item = Life> + '_ {
    program.pipelines.iter().enumerate().filter_map(|(p, pipe)| pipe.produces().map(|slot| (p, slot, pipe))).enumerate().map(|(id, (p, slot, pipe))| Life { id: id as u16, slot, producer: p, last_consumer: program.last_consumer(p), parallel: pipe.parallel == gravity_program::Parallel::Eligible })
}

pub fn life_of(program: &Program, p: usize) -> Option<Life> {
    lives(program).find(|l| l.producer == p)
}

/// What the driver asks of a program per pipeline, built once per Node in its arena (`Node::build`; `image_bytes` reserves
/// `Schedule::arena_bytes`): asked by scanning per pipeline these cost O(pipelines^2 x protos) per execution, 97.7% of
/// m4_l20's LIMIT over 4100 Materialize arms (warm 28 s at W0).
pub struct Schedule<A: Allocator + Clone> {
    pub arms: ArmRuns<A>,
    pub frees: FreeSchedule<A>,
    /// `node::program_images` (every `grant::share` reads it; it walks every pipeline's owner).
    pub images: bool,
}

impl<A: Allocator + Clone> Schedule<A> {
    pub fn of_in(program: &Program, a: A) -> Schedule<A> {
        let arms = ArmRuns::of_in(program, a.clone());
        let frees = FreeSchedule::of_in(program, &arms, a);
        Schedule { arms, frees, images: crate::node::program_images(program) }
    }

    /// The most `of_in` takes from an arena: the arms runs (a word per pipeline, when an Arms proto is declared), the free
    /// schedule's offsets and at most two entries per producer.
    pub fn arena_bytes(program: &Program) -> usize {
        let n = program.pipelines.len();
        let producers = program.pipelines.iter().filter(|p| p.produces().is_some()).count();
        let arms = program.v4.as_deref().is_some_and(|d| d.protos.iter().any(|x| x.kind == ProtoKind::Arms));
        let words = |bytes: usize| bytes.div_ceil(8) * 8;
        words((n + 1) * size_of::<u32>()) + words(2 * producers * size_of::<(u32, u8)>()) + if arms { words(n * size_of::<(u32, u32)>()) } else { 0 }
    }
}

/// D393 (V4R-47), per pipeline: the append.arms run holding it as an arm of the slot it produces (`Node::arms_of`' rule): the
/// first `Arms` proto naming that slot whose run holds the pipeline and whose first and last pipelines produce the slot.
/// Empty when the program has no such run.
pub struct ArmRuns<A: Allocator = Global>(Vec<(u32, u32), A>);

impl<A: Allocator + Clone> ArmRuns<A> {
    pub fn of_in(program: &Program, a: A) -> ArmRuns<A> {
        let ps = &program.pipelines;
        let mut runs = Vec::new_in(a);
        let Some(v4) = program.v4.as_deref() else { return ArmRuns(runs) };
        for x in v4.protos.iter().filter(|x| x.kind == ProtoKind::Arms) {
            let (start, end) = (x.pipeline as usize, (x.pipeline as usize).saturating_add(x.arg as usize));
            let produces = |q: usize| ps.get(q).and_then(|p| p.produces()) == Some(x.slot);
            if start >= end || !produces(start) || !produces(end - 1) {
                continue;
            }
            if runs.is_empty() {
                runs.reserve_exact(ps.len());
                runs.resize(ps.len(), (0, 0));
            }
            // an earlier proto's run keeps a pipeline it holds (the rule's first match); a pipeline between that produces
            // another slot (MergeAppend's per-arm feeder) is no arm
            for (q, r) in runs.iter_mut().enumerate().take(end).skip(start) {
                if *r == (0, 0) && produces(q) {
                    *r = (start as u32, end as u32);
                }
            }
        }
        ArmRuns(runs)
    }

    /// The run holding producer `q` as an arm of the slot it produces.
    pub fn run_of(&self, q: usize) -> Option<core::ops::Range<usize>> {
        self.0.get(q).filter(|r| r.0 < r.1).map(|r| r.0 as usize..r.1 as usize)
    }

    /// D393b: `q` is an arm of a run other than its last (it folds only).
    pub fn continues(&self, q: usize) -> bool {
        self.run_of(q).is_some_and(|r| q + 1 < r.end)
    }
}

/// `FreeSchedule` flags: the producer's slot's stream ends after the pipeline (its last reader, or the last spill-partition
/// pipe before the slot's next producer, S5).
pub const DUE_END: u8 = 1;
/// `FreeSchedule` flags: the producer's slot frees after the pipeline (its last consumer; an arm's, its run's last arm's, D393).
pub const DUE_FREE: u8 = 2;

/// `Node::free_after`'s program constants: per pipeline `p`, each producer whose slot's stream ends or whose slot frees after
/// `p` (`DUE_END` / `DUE_FREE`), producers ascending as `free_after` met them walking every earlier producer.
pub struct FreeSchedule<A: Allocator = Global> {
    at: Vec<u32, A>,
    due: Vec<(u32, u8), A>,
}

impl FreeSchedule {
    pub fn of(program: &Program, arms: &ArmRuns<impl Allocator + Clone>) -> FreeSchedule {
        FreeSchedule::of_in(program, arms, Global)
    }
}

impl<A: Allocator + Clone> FreeSchedule<A> {
    pub fn of_in(program: &Program, arms: &ArmRuns<impl Allocator + Clone>, a: A) -> FreeSchedule<A> {
        let ps = &program.pipelines;
        let n = ps.len();
        // producer q's (stream end, free) pipelines. Each scan stops at the slot's next producer, so the scans of one slot
        // never overlap (O(slots x pipelines) in all); an arms run's arms share its last arm's last consumer, taken once
        let due_of = |q: usize, slot: u8, run_last: &mut (usize, usize)| -> (usize, usize) {
            let next = (q + 1..n).find(|&k| ps[k].produces() == Some(slot)).unwrap_or(n);
            let parts = (q + 1..next).filter(|&k| matches!(ps[k].claims, gravity_program::ClaimKind::SpillPartitions { .. })).max();
            let free = match arms.run_of(q) {
                Some(run) => {
                    if run_last.0 != run.end {
                        *run_last = (run.end, program.last_consumer(run.end - 1));
                    }
                    run_last.1
                }
                None => program.last_consumer(q),
            };
            (parts.unwrap_or(0).max(free), free)
        };
        let mut at = Vec::with_capacity_in(n + 1, a.clone());
        at.resize(n + 1, 0u32);
        let mut run_last = (usize::MAX, 0);
        for q in 0..n {
            if let Some(slot) = ps[q].produces() {
                let (end, free) = due_of(q, slot, &mut run_last);
                at[end + 1] += 1;
                if free != end {
                    at[free + 1] += 1;
                }
            }
        }
        let mut sum = 0;
        for x in at.iter_mut() {
            sum += *x;
            *x = sum;
        }
        let mut due = Vec::with_capacity_in(sum as usize, a);
        due.resize(sum as usize, (0u32, 0u8));
        // `at[p]` is pipeline p's cursor, from its first entry to its last + 1 (the next pipeline's first), then shifted back
        run_last = (usize::MAX, 0);
        for q in 0..n {
            if let Some(slot) = ps[q].produces() {
                let (end, free) = due_of(q, slot, &mut run_last);
                let puts = if end == free { [(end, DUE_END | DUE_FREE), (n, 0)] } else { [(end, DUE_END), (free, DUE_FREE)] };
                for (p, flags) in puts.into_iter().filter(|&(p, _)| p < n) {
                    due[at[p] as usize] = (q as u32, flags);
                    at[p] += 1;
                }
            }
        }
        at.copy_within(0..n, 1);
        at[0] = 0;
        FreeSchedule { at, due }
    }

    /// After pipeline `p`: (producer, `DUE_*` flags), producers ascending.
    pub fn after(&self, p: usize) -> &[(u32, u8)] {
        &self.due[self.at[p] as usize..self.at[p + 1] as usize]
    }
}

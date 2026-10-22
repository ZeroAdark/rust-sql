//! A program's run-again declarations, lowered by lowerv4's lifetime home (lowerv4 lifetime `program`): every pipeline the
//! runtime runs again with its product reset in place (driver `Entry::rerun_nested`), with the one runner that runs it
//! again and how long its product lives across that runner's runs. The runtime resets exactly what is declared here: a
//! member it does not declare is a Bug by name, never a reset the runtime works out for itself.

use allocator_api2::alloc::{Allocator, Global};
use allocator_api2::vec::Vec as AVec;

use crate::arena::ArenaRef;
use crate::program::round8;

/// What runs a member again (each with its product reset first), as PostgreSQL's ExecReScan re-executes the node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Runner {
    /// Rerun set `set` (its `graph.reruns` index, as its Rerun protos' `arg`): its owner's re-open runs it (exec rerun `run`),
    /// PG's chgParam at a nest's inner re-open or a SubPlan's call.
    Set(u16),
    /// The InitPlan whose cell is handle word `word`: its next read after a re-arm (a rerun set's, D1262; a recheck's, D2673),
    /// PG's ExecReScanSetParamPlan.
    Init(u16),
    /// The LAZY hashed SubPlan set whose handle is word `word`: its next probe after a re-arm, PG's buildSubPlanHash after a
    /// rescan that changed its parameters.
    SubHash(u16),
    /// The CTE whose handle is word `word`, re-armed by a rerun set holding its pipeline: its next pull runs these members of
    /// its pulled set again (PG's ExecReScanCteScan: tuplestore_clear, the plan re-run lazily at the next read), never the
    /// set's run.
    Cte(u16),
    /// The WindowAgg whose handle is word `word`, its fill pulled (the exact request, W4) and re-armed by a rerun set holding
    /// the fill: its next pull runs these members again into the emptied spool (PG's ExecReScanWindowAgg: the buffer
    /// emptied, the child read again from its start as spool_tuples reads it), never the set's run.
    Window(u16),
}

/// A rerun set's member proto argument: the set's `graph.reruns` index, with `RERUN_KEPT` for a member PG keeps across the
/// set's runs (decompose `graph.kept`, the keep table's answer): it runs at the set's first run only and its state serves
/// the later runs (`RunLife::FirstRun`).
pub const fn rerun_arg(set: usize, kept: bool) -> u64 {
    set as u64 | if kept { crate::protos::RERUN_KEPT } else { 0 }
}

/// The rerun set index a member proto argument names (`rerun_arg`), past the kept flag.
pub const fn rerun_set_of(arg: u64) -> u64 {
    arg & !crate::protos::RERUN_KEPT
}

/// How long a member's product lives across its runner's runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RunLife {
    /// Reset and run at every run (lowerv4 lifetime `Scope::OwnerRun`).
    EveryRun,
    /// Kept for the execution: run until a run fills it, never reset after (`Scope::Execution`; D1018 item 9 (b), D2612:
    /// a node PG keeps at a rescan that changes none of its params).
    FirstRun,
}

/// `RunMember::lazy_probe` of a member that is no lazy build.
pub const NO_PROBE: u16 = u16::MAX;

/// One member: program pipeline `pipeline`, run again by `runner`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RunMember {
    pub pipeline: u16,
    pub runner: Runner,
    pub life: RunLife,
    /// The member probe whose first row runs this lazy build (D824 / D1700: PG's rescan of a LEFT / ANTI hash join fetches
    /// the outer's first row before every build), or `NO_PROBE`.
    pub lazy_probe: u16,
}

impl RunMember {
    pub const fn lazy_probe(&self) -> Option<u16> {
        if self.lazy_probe == NO_PROBE { None } else { Some(self.lazy_probe) }
    }
}

/// What one run of a runner's members needs from their declarations (`Lifetimes::run_of`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunNeeds {
    /// A member is kept (`RunLife::FirstRun`): its slot is reset only before a run produced it.
    pub kept: bool,
    /// A member is the lazy build of a member probe (`RunMember::lazy_probe`).
    pub lazy: bool,
}

/// What one run of a rerun set re-arms instead of running (PG's ExecReScan of the node, each piece run again lazily at its
/// next read): each list in protocol order, by handle word.
///
/// Generic over where its lists live (planprod P1-b): `Global` while the lowerer builds it, the program's own side arena once
/// `Program::with_v4` froze it (`ArenaRef`). Only the heap form clones: a frozen copy takes nothing more from its arena.
#[derive(Debug)]
pub struct SetRearms<A: Allocator + Clone = Global> {
    /// The InitPlans whose own pipeline the set holds: their cell's handle word and first exec param. Their cells go stale
    /// and their next read runs them (`Runner::Init`), PG's ExecReScanSetParamPlan (D1262).
    pub inits: AVec<(u16, i32), A>,
    /// The LAZY hashed SubPlan sets a member builds: their next probe builds them again (`Runner::SubHash`).
    pub subhash: AVec<u16, A>,
    /// The CTEs whose pipeline the set holds: their next pull starts the plan again (`Runner::Cte`).
    pub ctes: AVec<u16, A>,
    /// The spools declared for those CTEs' pipelines (each store, a recursive union's two tables): their copies restart.
    pub spools: AVec<u16, A>,
    /// The WindowAggs whose pulled fill the set holds (their handle words): their next pull fills the spool again from the
    /// fill's start (`Runner::Window`).
    pub windows: AVec<u16, A>,
    /// The partition prunings whose first arm the set holds and whose exec steps read its owner's params: decided again
    /// before every run (PG's ExecFindMatchingSubPlans at each rescan of the Append).
    pub prunes: AVec<u16, A>,
    /// The set's members declared for a runner the set does not re-arm (an InitPlan's cell, a LAZY hashed set or a CTE whose
    /// closure holds the set: the set runs inside that runner's run), ascending, each with its life across the set's runs:
    /// the set's run runs them too (`Lifetimes::set_run`).
    pub nested: AVec<(u16, RunLife), A>,
    /// D4120: the handle words of the rerun sets nested in this set's inner whose kept members (`RunLife::FirstRun`) fill
    /// from a param this set's owner changes: each run marks them stale, so their next run fills those members again (PG's
    /// ExecReScanAgg rebuilds a kept table whose input's chgParam is set).
    pub refills: AVec<u16, A>,
}

/// The program's members, sorted by pipeline, one runner each, with each pipeline's index into them, and what each rerun set
/// re-arms.
/// Generic over where its lists live, as `SetRearms` (planprod P1-b); only the heap form clones.
#[derive(Debug)]
pub struct Lifetimes<A: Allocator + Clone = Global> {
    members: AVec<RunMember, A>,
    /// Pipeline -> its member's index (`u16::MAX`: none), so a run looks each member up once, by index.
    at: AVec<u16, A>,
    /// Rerun set index -> its re-arms.
    rearms: AVec<SetRearms<A>, A>,
}

impl Default for SetRearms {
    fn default() -> SetRearms {
        SetRearms { inits: AVec::new(), subhash: AVec::new(), ctes: AVec::new(), spools: AVec::new(), windows: AVec::new(), prunes: AVec::new(), nested: AVec::new(), refills: AVec::new() }
    }
}

/// The derived fold's bytes whatever the lists' home: each list hashes as its slice (as the boxed slices did), so a frozen
/// copy and its builder fold alike and the content key is unchanged.
impl<A: Allocator + Clone> core::hash::Hash for SetRearms<A> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        let SetRearms { inits, subhash, ctes, spools, windows, prunes, nested, refills } = self;
        inits[..].hash(state);
        subhash[..].hash(state);
        ctes[..].hash(state);
        spools[..].hash(state);
        windows[..].hash(state);
        prunes[..].hash(state);
        nested[..].hash(state);
        refills[..].hash(state);
    }
}

impl<A: Allocator + Clone, B: Allocator + Clone> PartialEq<SetRearms<B>> for SetRearms<A> {
    fn eq(&self, o: &SetRearms<B>) -> bool {
        self.inits[..] == o.inits[..]
            && self.subhash[..] == o.subhash[..]
            && self.ctes[..] == o.ctes[..]
            && self.spools[..] == o.spools[..]
            && self.windows[..] == o.windows[..]
            && self.prunes[..] == o.prunes[..]
            && self.nested[..] == o.nested[..]
            && self.refills[..] == o.refills[..]
    }
}

impl<A: Allocator + Clone> Eq for SetRearms<A> {}

impl<A: Allocator + Clone> SetRearms<A> {
    /// The bytes `copy_in` takes from an arena: each list rounded up to the arena's 8-byte step.
    pub(crate) fn arena_bytes(&self) -> usize {
        [size_of_val(&self.inits[..]), size_of_val(&self.subhash[..]), size_of_val(&self.ctes[..]), size_of_val(&self.spools[..]), size_of_val(&self.windows[..]), size_of_val(&self.prunes[..]), size_of_val(&self.nested[..]), size_of_val(&self.refills[..])]
            .into_iter()
            .map(round8)
            .sum()
    }

    /// The same re-arms with every list copied into allocator `b`: an arena (`arena_bytes` of it), or the global heap.
    pub(crate) fn copy_in<B: Allocator + Clone>(&self, b: &B) -> SetRearms<B> {
        SetRearms {
            inits: copy(&self.inits, b),
            subhash: copy(&self.subhash, b),
            ctes: copy(&self.ctes, b),
            spools: copy(&self.spools, b),
            windows: copy(&self.windows, b),
            prunes: copy(&self.prunes, b),
            nested: copy(&self.nested, b),
            refills: copy(&self.refills, b),
        }
    }
}

/// A heap copy: tests and rigs only (feature `rig`, D4533); the server's one copy is the frozen one (`copy_in` an arena).
#[cfg(any(test, feature = "rig"))]
impl Clone for SetRearms {
    fn clone(&self) -> SetRearms {
        self.copy_in(&Global)
    }
}

/// A heap copy: tests and rigs only (feature `rig`, D4533); the server's one copy is the frozen one (`copy_in` an arena).
#[cfg(any(test, feature = "rig"))]
impl Clone for Lifetimes {
    fn clone(&self) -> Lifetimes {
        self.copy_in(&Global)
    }
}

/// `v` copied into allocator `b`, its exact length (an empty list takes nothing from it).
fn copy<T: Copy, B: Allocator + Clone>(v: &[T], b: &B) -> AVec<T, B> {
    let mut o = AVec::with_capacity_in(v.len(), b.clone()); // alloc: Program
    o.extend_from_slice(v);
    o
}

impl Lifetimes {
    /// `None` when two members name one pipeline: a pipeline two runners run again would be reset under the other's rows.
    pub fn new(mut members: Vec<RunMember>) -> Option<Lifetimes> {
        members.sort_unstable_by_key(|m| m.pipeline);
        if members.windows(2).any(|w| w[0].pipeline == w[1].pipeline) {
            return None;
        }
        let mut at = vec![u16::MAX; members.last().map_or(0, |m| m.pipeline as usize + 1)]; // alloc: Program
        for (i, m) in members.iter().enumerate() {
            at[m.pipeline as usize] = i as u16;
        }
        Some(Lifetimes { members: heap_vec(members), at: heap_vec(at), rearms: AVec::new() })
    }

    /// The rerun sets' re-arms, by set index: one entry for every set up to the program's last.
    pub fn with_rearms(self, rearms: Vec<SetRearms>) -> Lifetimes {
        Lifetimes { rearms: heap_vec(rearms), ..self }
    }
}

/// The lowerer's list as the heap form's, in place: the same block of the global heap, no copy and no allocation (the
/// allocation is the lowerer's, where it built the list).
pub(crate) fn heap_vec<T>(v: Vec<T>) -> AVec<T> {
    let mut v = core::mem::ManuallyDrop::new(v);
    // SAFETY: allocator_api2's `Global` serves and frees through the global allocator, as std's `Vec` does, with the same
    // layout for a (pointer, length, capacity) triple, so the block `v` owned is this vector's to free.
    unsafe { AVec::from_raw_parts(v.as_mut_ptr(), v.len(), v.capacity()) }
}

impl Default for Lifetimes {
    fn default() -> Lifetimes {
        Lifetimes { members: AVec::new(), at: AVec::new(), rearms: AVec::new() }
    }
}

/// As `SetRearms`' fold: each list as its slice, so the content key is the boxed slices' (planprod P1-b).
impl<A: Allocator + Clone> core::hash::Hash for Lifetimes<A> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        let Lifetimes { members, at, rearms } = self;
        members[..].hash(state);
        at[..].hash(state);
        rearms[..].hash(state);
    }
}

impl<A: Allocator + Clone, B: Allocator + Clone> PartialEq<Lifetimes<B>> for Lifetimes<A> {
    fn eq(&self, o: &Lifetimes<B>) -> bool {
        self.members[..] == o.members[..] && self.at[..] == o.at[..] && self.rearms.len() == o.rearms.len() && self.rearms.iter().zip(o.rearms.iter()).all(|(x, y)| x == y)
    }
}

impl<A: Allocator + Clone> Eq for Lifetimes<A> {}

impl<A: Allocator + Clone> Lifetimes<A> {
    /// The bytes `copy_in` takes from an arena (the re-arms list at its frozen element's size).
    pub(crate) fn arena_bytes(&self) -> usize {
        round8(size_of_val(&self.members[..])) + round8(size_of_val(&self.at[..])) + round8(self.rearms.len() * size_of::<SetRearms<ArenaRef>>()) + self.rearms.iter().map(|r| r.arena_bytes()).sum::<usize>()
    }

    /// The same declarations with every list copied into allocator `b`: an arena (`arena_bytes` of it), the program's
    /// frozen copy, or the global heap (`V4Descs::thaw`).
    pub(crate) fn copy_in<B: Allocator + Clone>(&self, b: &B) -> Lifetimes<B> {
        let mut rearms = AVec::with_capacity_in(self.rearms.len(), b.clone()); // alloc: Program
        for r in self.rearms.iter() {
            rearms.push(r.copy_in(b));
        }
        Lifetimes { members: copy(&self.members, b), at: copy(&self.at, b), rearms }
    }

    pub fn members(&self) -> &[RunMember] {
        &self.members
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Pipeline `p`'s declaration.
    pub fn member(&self, p: u16) -> Option<RunMember> {
        self.at.get(p as usize).and_then(|&i| self.members.get(i as usize)).copied()
    }

    /// The members rerun set `set`'s run runs: its first run's (a later run's: those with `RunLife::EveryRun`). Its own
    /// (`Runner::Set`) in program order, then those of a runner it runs inside (`SetRearms::nested`) in program order, each
    /// with its life across the set's runs.
    pub fn set_run(&self, set: u16) -> impl Iterator<Item = RunMember> + '_ {
        let nested = self.rearms(set).map_or(&[][..], |r| &r.nested[..]);
        let own = self.members.iter().copied().filter(move |m| m.runner == Runner::Set(set));
        own.chain(nested.iter().filter_map(move |&(p, life)| self.member(p).map(|m| RunMember { life, ..m })))
    }

    /// What rerun set `set`'s run re-arms; `None`: the program has no set `set`.
    pub fn rearms(&self, set: u16) -> Option<&SetRearms<A>> {
        self.rearms.get(set as usize)
    }

    /// One look at each of `members`: `None` unless every one is declared and they all share one runner (and for none);
    /// else whether any is kept and whether any is the lazy build of a member probe.
    pub fn run_of(&self, members: &[u16]) -> Option<RunNeeds> {
        let r = self.member(*members.first()?)?.runner;
        let mut needs = RunNeeds::default();
        for &m in members {
            let x = self.member(m).filter(|x| x.runner == r)?;
            needs.kept |= x.life == RunLife::FirstRun;
            needs.lazy |= x.lazy_probe().is_some_and(|q| members.contains(&q));
        }
        Some(needs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn m(pipeline: u16, runner: Runner) -> RunMember {
        RunMember { pipeline, runner, life: RunLife::EveryRun, lazy_probe: NO_PROBE }
    }

    #[test]
    fn one_runner_per_pipeline_found_by_pipeline() {
        let l = Lifetimes::new(vec![m(4, Runner::Set(1)), m(1, Runner::Set(0)), m(2, Runner::Init(9))]).unwrap();
        assert_eq!(l.members().iter().map(|x| x.pipeline).collect::<Vec<_>>(), [1, 2, 4]);
        assert_eq!(l.member(2).map(|x| x.runner), Some(Runner::Init(9)));
        assert_eq!(l.member(3), None);
        assert!(Lifetimes::new(vec![m(1, Runner::Set(0)), m(1, Runner::Init(3))]).is_none(), "two runners of one pipeline");
        assert!(Lifetimes::default().is_empty());
    }

    /// A run is of one runner's declared members only; it says whether one is kept or a member probe's lazy build.
    #[test]
    fn a_run_names_one_runners_declared_members() {
        let kept = RunMember { life: RunLife::FirstRun, ..m(5, Runner::Set(2)) };
        let lazy = RunMember { lazy_probe: 7, ..m(6, Runner::Set(2)) };
        let l = Lifetimes::new(vec![m(0, Runner::Set(0)), m(1, Runner::Set(0)), m(2, Runner::SubHash(5)), m(3, Runner::Set(1)), kept, lazy, m(7, Runner::Set(2))]).unwrap();
        let none = RunNeeds::default();
        assert_eq!(l.run_of(&[0, 1]), Some(none));
        assert_eq!(l.run_of(&[2]), Some(none));
        assert_eq!(l.run_of(&[0, 3]), None, "two sets' members in one run");
        assert_eq!(l.run_of(&[0, 9]), None, "an undeclared member");
        assert_eq!(l.run_of(&[4]), None, "an undeclared member below the last declared");
        assert_eq!(l.run_of(&[]), None);
        assert_eq!(l.run_of(&[5, 6, 7]), Some(RunNeeds { kept: true, lazy: true }));
        assert_eq!(l.run_of(&[6]), Some(none), "a lazy build whose probe is not in the run");
        assert_eq!(RunMember { lazy_probe: 4, ..m(0, Runner::Set(0)) }.lazy_probe(), Some(4));
        assert_eq!(m(0, Runner::Set(0)).lazy_probe(), None);
    }

    /// A set's run is its own members, in program order; what it re-arms is found by set index, none past the last set.
    #[test]
    fn a_sets_run_and_rearms_by_set_index() {
        let kept = RunMember { life: RunLife::FirstRun, ..m(3, Runner::Set(1)) };
        let l = Lifetimes::new(vec![m(5, Runner::Set(1)), m(0, Runner::Init(4)), kept, m(1, Runner::Set(1)), m(2, Runner::Cte(6)), m(4, Runner::Set(0)), m(6, Runner::Window(11))]).unwrap();
        assert_eq!(l.set_run(1).map(|x| x.pipeline).collect::<Vec<_>>(), [1, 3, 5], "a re-armed window's fill is its pull's");
        assert_eq!(l.set_run(1).filter(|x| x.life == RunLife::EveryRun).map(|x| x.pipeline).collect::<Vec<_>>(), [1, 5]);
        assert_eq!(l.set_run(0).map(|x| x.pipeline).collect::<Vec<_>>(), [4]);
        assert_eq!(l.set_run(2).count(), 0);
        assert_eq!(l.rearms(0), None, "no re-arms declared");
        let r = SetRearms { inits: [(4, 7)].into(), ctes: [6].into(), spools: [8, 9].into(), windows: [11].into(), refills: [10].into(), ..SetRearms::default() };
        let l = l.with_rearms(vec![SetRearms::default(), r.clone()]);
        assert_eq!(l.rearms(0), Some(&SetRearms::default()));
        assert_eq!(l.rearms(1), Some(&r));
        assert_eq!(l.rearms(2), None);
        assert_eq!(l.member(2).map(|x| x.runner), Some(Runner::Cte(6)));
        assert_eq!(l.member(6).map(|x| x.runner), Some(Runner::Window(11)));
    }

    /// A set nested inside a runner it does not re-arm runs that runner's members it holds too, with the life the set gives
    /// them; a nested pipeline with no declaration is no member of the run.
    #[test]
    fn a_nested_sets_run_takes_its_members_declared_for_the_runner_around_it() {
        let l = Lifetimes::new(vec![m(0, Runner::Init(4)), m(1, Runner::Init(4)), m(2, Runner::Init(4)), m(3, Runner::Set(0))]).unwrap();
        let r = SetRearms { nested: [(1, RunLife::EveryRun), (2, RunLife::FirstRun), (9, RunLife::EveryRun)].into(), ..SetRearms::default() };
        let l = l.with_rearms(vec![r]);
        assert_eq!(l.set_run(0).map(|x| (x.pipeline, x.life)).collect::<Vec<_>>(), [(3, RunLife::EveryRun), (1, RunLife::EveryRun), (2, RunLife::FirstRun)]);
        assert!(l.set_run(0).filter(|x| x.pipeline != 3).all(|x| x.runner == Runner::Init(4)), "their declaration stays the cell's");
        assert_eq!(l.set_run(1).count(), 0);
    }

    #[test]
    fn rerun_args() {
        for set in [0usize, 1, 7, 255, 65535] {
            assert_eq!(rerun_set_of(rerun_arg(set, true)), set as u64);
            assert_eq!(rerun_set_of(rerun_arg(set, false)), set as u64);
            assert_ne!(rerun_arg(set, true), rerun_arg(set, false));
            assert_eq!(rerun_arg(set, true) & crate::protos::RERUN_KEPT, crate::protos::RERUN_KEPT);
        }
    }
}

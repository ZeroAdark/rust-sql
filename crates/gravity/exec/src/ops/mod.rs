//! Lowerer v4's runtime protocols (docs/gravity/lowerv4/plan.md §2.2, §4.3): one module per milestone (`m<k>.rs`) exporting
//! its site admissions (`ADMIT`, `BIND`) and its protocols (`PROTOCOLS`). admit.rs's site table is `RUNGS`; the runner's seams
//! (runner/plan.rs) call `State`, which runs each `gravity_program::v4::Proto` a program declares through the milestone that
//! built its kind. APPEND-ONLY: a milestone adds its module's name on its own line right above the sentinel and edits nothing
//! else in this file.
use std::rc::Rc;

use gravity_driver::{Exec, Grant, Merge};
use gravity_parallel::MorselSource;
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::Program;
use mcx::Mcx;
use types_error::{PgError, PgResult};
use types_snapshot::SnapshotData;

use crate::admit::{Admit, Bind};
use crate::shapes::Admitted;

/// Each milestone's site admissions, in landing order (admit.rs's `RUNGS`; v5 serial step, landing 5: was
/// `milestones!`).
pub const RUNGS: &[(&[Admit], &[Bind])] = &[
    (crate::statement::ADMIT, crate::statement::BIND),
    (crate::ops::scan::sites_heap::ADMIT, crate::ops::scan::sites_heap::BIND),
    (crate::ops::sort::proto::ADMIT, crate::ops::sort::proto::BIND),
    (crate::ops::modifytable::statement::ADMIT, crate::ops::modifytable::statement::BIND),
    (crate::ops::srf::proto::ADMIT, crate::ops::srf::proto::BIND),
    (crate::ops::agg::trans::arena::ADMIT, crate::ops::agg::trans::arena::BIND),
    (crate::ops::hashjoin::drain::proto::ADMIT, crate::ops::hashjoin::drain::proto::BIND),
    (&[crate::ops::scan::param_key::admit, crate::ops::scan::param_sample::admit], &[crate::ops::scan::param_key::bind, crate::ops::scan::param_sample::bind]),
    (crate::ops::setop::proto::ADMIT, crate::ops::setop::proto::BIND),
    (crate::ops::window::proto::ADMIT, crate::ops::window::proto::BIND),
    (crate::admit::explain::ADMIT, crate::admit::explain::BIND),
];
/// Each milestone's runtime protocols, in landing order.
const PROTOCOLS: &[&[&dyn Protocol]] = &[
    crate::statement::PROTOCOLS,
    crate::ops::append::proto::PROTOCOLS,
    crate::ops::sort::proto::PROTOCOLS,
    crate::ops::modifytable::statement::PROTOCOLS,
    crate::ops::srf::proto::PROTOCOLS,
    crate::ops::agg::trans::arena::PROTOCOLS,
    crate::ops::agg::emit_runs::PROTOCOLS,
    crate::ops::shared::user_keys::PROTOCOLS,
    crate::ops::hashjoin::drain::proto::PROTOCOLS,
    &[&crate::ops::subplan::initplan::init::Init, &crate::ops::material::proto::Spool, &crate::ops::material::proto::SpoolChunks, &crate::ops::subplan::hash::SubHash, &crate::ops::subplan::hash::SubHashEpq, &crate::ops::subplan::hash::SubResult, &crate::ops::subplan::Rescan, &crate::rerun::Rerun, &crate::ops::memoize::proto::Memo, &crate::ops::ctescan::proto::Cte],
    crate::ops::setop::proto::PROTOCOLS,
    crate::ops::window::proto::PROTOCOLS,
    crate::ops::mergejoin::proto::PROTOCOLS,
    crate::admit::explain::PROTOCOLS,
];
/// What the runner hands a protocol before the section: this execution's widths and parameter words, which it may change.
pub struct Prepare<'a> {
    pub mcx: Mcx<'static>,
    pub program: &'a Program,
    pub admitted: &'a Admitted,
    pub snap: &'a Rc<SnapshotData<'static>>,
    pub width: u16,
    pub grant: Grant,
    pub widths: &'a mut [u16],
    pub words: &'a mut [u64],
    pub nulls: &'a mut [u8],
    pub nparams: &'a mut usize,
    /// D3586: the initial pruning decided before the claims were sized (`append::proto::Initial`), which `src.prune`
    /// takes; `None`: each decides at its own prepare.
    pub initial: Option<&'a crate::ops::append::proto::Initial>,
    /// The ledger owner a prepare's working memory is charged to (cvfan-gx E1), with `Kind::Region`: `memledger::charge::
    /// context()` read through `gravity_driver::stmt_context` by the runner on the leader before the prepares, so the
    /// portal the backend executes, else its running statement; `None` outside both (a rig, a session-level call). Never
    /// `statement_owner()`: a cursor's prepare products live in its portal (`Serve.producers` / `Serve.serial`,
    /// `PortalPins.serial`) until CLOSE. No operator owner exists yet at prepare time, so the statement or portal holds it;
    /// what an operator builds once its own owner exists (bind, capture) is charged there instead.
    /// RULE: nothing charged to this owner is ever stored into a cached product (the `Program`, a hosted owner, a
    /// template): those outlive the portal. What a prepare builds from it lives in the execution's state (`Live`, the
    /// runner's plan) and dies with it.
    pub owner: Option<(memledger::Owner, memledger::Kind)>,
}

impl Prepare<'_> {
    /// An empty `ChargedVec` on this execution's prepare owner (`owner`; uncharged when `None`). Its line's tag:
    /// `// alloc: Exec charged cx.owner`. See `owner`'s RULE: never stored into a cached product.
    pub fn charged_vec<T>(&self) -> memledger::ChargedVec<T> {
        match self.owner {
            Some((o, k)) => memledger::ChargedVec::new(Some(o), k), // alloc: Exec charged o
            None => memledger::ChargedVec::none(),
        }
    }
}

/// One runtime protocol kind, run by the milestone that builds it.
pub trait Protocol: Sync {
    fn kind(&self) -> ProtoKind;
    /// Before the section, once per execution: the declared `p`'s words and widths; its state for the later calls, if any.
    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>>;
}

/// M8 (RT-1): the runner's frame while it runs the program (`State::set_frame`, gravity_parallel `publish`): a host inside
/// one of its calls runs other pipelines through it, nested (`gravity_driver::OnDemand`).
pub use gravity_driver::Frame;
pub mod agg;
pub mod append;
pub mod ctescan;
pub mod hashjoin;
pub mod material;
pub mod memoize;
pub mod mergejoin;
pub mod modifytable;
pub mod scan;
pub mod setop;
pub mod shared;
pub mod sort;
pub mod srf;
pub mod subplan;
pub mod window;

/// rearm E2: the questions `State` asks a live, one bit per `Live` method past `answers`; a live declares the ones it
/// implements (`Live::answers`), and the runners' per-pipeline questions (each rescan asks several per member) walk only
/// the lives that answer them. The `lives_answers` test reads every `impl Live` and checks its declaration against its
/// methods.
pub mod ask {
    pub const CLAIMS: u16 = 1 << 0;
    pub const MERGE: u16 = 1 << 1;
    pub const AFTER: u16 = 1 << 2;
    pub const END: u16 = 1 << 3;
    pub const DEFERRED: u16 = 1 << 4;
    pub const FRAME: u16 = 1 << 5;
    pub const RERUN_OWNER: u16 = 1 << 6;
    pub const PULL_READERS: u16 = 1 << 7;
    pub const DRAIN: u16 = 1 << 8;
    pub const LATE: u16 = 1 << 9;
    pub const PRUNED: u16 = 1 << 10;
    pub const REOPEN: u16 = 1 << 11;
    pub const NONE: u16 = 0;
    pub const ALL: u16 = (1 << 12) - 1;
}

/// memsig E-pub: a list of lives in one block on its maker's owner: exec's prepare makes it with `Prepare::charged_vec`,
/// the crate rig (lowerv4 tests/common, each protocol's `prepare_bare`) with `LiveList::none()` (no execution: uncharged).
pub type LiveList = memledger::ChargedVec<Box<dyn Live>>;

/// A protocol's per-execution state: each answer `None` leaves the runner's own.
pub trait Live {
    /// rearm E2: the questions this live answers (`ask`: the methods below it implements); `State` never asks it another.
    /// Default: every one.
    fn answers(&self) -> u16 {
        ask::ALL
    }
    fn claims_for(&self, _pipeline: usize) -> Option<&dyn MorselSource> {
        None
    }
    fn merge_for(&self, _pipeline: usize) -> Option<&dyn Merge> {
        None
    }
    /// After `pipeline` completed; `None` when this protocol has nothing to do there.
    fn after_pipeline(&self, _pipeline: usize, _exec: &mut Exec) -> Option<gravity_driver::Result<()>> {
        None
    }
    /// EXPLAIN ANALYZE: a memo object's handle word and its cache counters so far (hits, misses, evictions, overflows,
    /// peak bytes: PostgreSQL's MemoizeInstrumentation); `None` for any other protocol. Read only at a recorded run's end.
    fn memo_stats(&self) -> Option<(u32, [u64; 5])> {
        None
    }
    /// The execution's normal end (PostgreSQL's ExecutorEnd: its ExprContext callbacks run); an error's end only drops it.
    fn end(&self) -> PgResult<()> {
        Ok(())
    }
    /// M8 (RT-1): `pipeline` runs only on demand (an Init's member): the runner steps past it in the producer order.
    fn deferred(&self, _pipeline: usize) -> bool {
        false
    }
    /// M8 (RT-1): the runner's frame while it runs the program, `None` outside (`State::set_frame`).
    fn frame(&self, _f: Option<Frame>) {}
    /// M8 (RT-2e): a deferred `pipeline` that is a rerun set's member: the set's owner (the pipeline calling `m8_rerun`).
    fn rerun_owner(&self, _pipeline: usize) -> Option<usize> {
        None
    }
    /// M8 (RT-2f): a deferred `pipeline` that is a pulled set's CTE plan: its readers (`Plan::pull_readers`).
    fn pull_readers(&self, _pipeline: usize) -> Option<&[usize]> {
        None
    }
    /// M8 (RT-2f): a pulled set run to its end at the statement's end (a data-modifying CTE; `Plan::drain_at_end`).
    fn drain_at_end(&self) -> Option<&[u16]> {
        None
    }
    /// D2605: the rank of the set `drain_at_end` names: the statement-end drain runs the higher first (PostgreSQL's
    /// `es_auxmodifytables` order); equal ranks keep the protocols' order.
    fn drain_rank(&self) -> u16 {
        0
    }
    /// M8 (L0-k): `pipeline`'s late words, decided as it starts (D899's late step, `Plan::late_words`): appended to `out`,
    /// the block the runner made on the execution's owner (`State::charged_vec`): grow it through `out.with_vec`, tagged
    /// `// alloc: Exec charged out`.
    fn late_words(&self, _pipeline: usize, _out: &mut gravity_parallel::LateWords) -> PgResult<()> {
        Ok(())
    }
    /// D2105: `pipeline` is an arm this run's exec pruning removed, decided before it starts: the runner skips it whole
    /// (no late word, no claim), as PostgreSQL's ExecReScanAppend never opens it.
    fn pruned_whole(&self, _pipeline: usize) -> bool {
        false
    }
    /// D2289: the rerun set `members` is re-opened (PG's ExecReScan of the set's nodes), before the members run again: a
    /// protocol holding per-open state of one of them resets it. On the leader, no worker running.
    fn reopen(&self, _members: &[u16]) -> PgResult<()> {
        Ok(())
    }
}

/// The milestone implementation of `kind`, if it has landed.
pub fn protocol(kind: ProtoKind) -> Option<&'static dyn Protocol> {
    PROTOCOLS.iter().flat_map(|l| l.iter()).copied().find(|p| p.kind() == kind)
}

/// The v4 protocols of one execution (empty for a program that declares none).
#[derive(Default)]
pub struct State {
    lives: memledger::ChargedVec<Box<dyn Live>>,
    /// rearm E2: each live's `answers`, read once at the prepare.
    asks: memledger::ChargedVec<u16>,
    /// M8 (RT-1): the frame `set_frame` published last.
    frame: core::cell::Cell<Option<Frame>>,
    /// memsig E-pub: the owner the prepare charged (`Prepare::owner`), kept for what the execution builds later on its
    /// behalf (`charged_vec`: the late words); `None`: uncharged. The same RULE as `Prepare::owner`: never into a cached
    /// product.
    owner: Option<(memledger::Owner, memledger::Kind)>,
}

impl State {
    /// Every declared protocol prepared in declaration order; a kind no landed milestone runs is an ERROR by name (the
    /// lowerer refuses its kind first, so this is a bug's wall, never a served statement's).
    pub fn prepare(cx: &mut Prepare<'_>) -> PgResult<State> {
        let program = cx.program;
        let Some(d) = program.v4.as_deref() else { return Ok(State::default()) };
        let mut lives = cx.charged_vec();
        for p in d.protos.iter() {
            let Some(imp) = protocol(p.kind) else {
                let msg = format!("gravity: v4 protocol {} not built (M{})", p.kind.name(), p.kind.milestone()); // alloc: Cold
                return Err(Box::new(PgError::error(msg))); // alloc: Cold
            };
            if let Some(l) = imp.prepare(p, cx)? {
                lives.with_vec(|v| v.push(l)); // alloc: Exec charged cx.owner
            }
        }
        let mut asks = cx.charged_vec();
        asks.with_vec(|v| v.extend(lives.iter().map(|l| l.answers()))); // alloc: Exec charged cx.owner
        Ok(State { lives, asks, frame: core::cell::Cell::new(None), owner: cx.owner })
    }

    /// An empty `ChargedVec` on the owner this execution's prepare charged (`Prepare::charged_vec`'s, kept past the
    /// prepare; uncharged when `None`). Its line's tag: `// alloc: Exec charged v4.owner`.
    pub fn charged_vec<T>(&self) -> memledger::ChargedVec<T> {
        match self.owner {
            Some((o, k)) => memledger::ChargedVec::new(Some(o), k), // alloc: Exec charged o
            None => memledger::ChargedVec::none(),
        }
    }

    /// rearm E2: the lives that answer question `q` (`ask`), in declaration order.
    fn asked(&self, q: u16) -> impl Iterator<Item = &dyn Live> + '_ {
        self.lives.iter().zip(self.asks.iter()).filter(move |(_, &a)| a & q != 0).map(|(l, _)| &**l)
    }

    pub fn claims_for(&self, pipeline: usize) -> Option<&dyn MorselSource> {
        self.asked(ask::CLAIMS).find_map(|l| l.claims_for(pipeline))
    }

    pub fn merge_for(&self, pipeline: usize) -> Option<&dyn Merge> {
        self.asked(ask::MERGE).find_map(|l| l.merge_for(pipeline))
    }

    /// Every protocol's step after `pipeline`, in declaration order: the first error stops; `Some` when any acted, so the
    /// runner's own after-pipeline steps (the shape table's) are skipped.
    pub fn after_pipeline(&self, pipeline: usize, exec: &mut Exec) -> Option<gravity_driver::Result<()>> {
        let mut acted = None;
        for l in self.asked(ask::AFTER) {
            match l.after_pipeline(pipeline, exec) {
                Some(Err(e)) => return Some(Err(e)),
                Some(Ok(())) => acted = Some(Ok(())),
                None => {}
            }
        }
        acted
    }

    /// EXPLAIN ANALYZE: the counter cells no program adds to, written from the protocols' state as a recorded run ends:
    /// each Memoize's five cache counters (lowerv4 `Ctr::Hits` .. `Ctr::MemPeak`) from the memo object the `Ctr::MemoOf`
    /// cell after them names. `instr`: the program's cells (`Program.instr`), `cells` their values.
    pub fn fill_cells(&self, instr: &[(i32, u8)], cells: &mut [u64]) {
        const MEMO_OF: u8 = 14;
        for (i, &(word, ctr)) in instr.iter().enumerate() {
            if ctr != MEMO_OF || i < 5 || i >= cells.len() {
                continue;
            }
            if let Some((_, s)) = self.lives.iter().filter_map(|l| l.memo_stats()).find(|(w, _)| *w as i64 == word as i64) {
                cells[i - 5..i].copy_from_slice(&s);
            }
        }
    }

    /// Every protocol's normal end, in declaration order; the first error stops.
    pub fn end(&self) -> PgResult<()> {
        self.asked(ask::END).try_for_each(|l| l.end())
    }

    /// M8 (L0-k): every protocol's late words for `pipeline`, appended to `out` (made by `charged_vec`).
    pub fn late_words(&self, pipeline: usize, out: &mut gravity_parallel::LateWords) -> PgResult<()> {
        self.asked(ask::LATE).try_for_each(|l| l.late_words(pipeline, out))
    }

    /// D2105: whether a protocol pruned arm `pipeline` whole for this run (`Live::pruned_whole`).
    pub fn pruned_whole(&self, pipeline: usize) -> bool {
        self.asked(ask::PRUNED).any(|l| l.pruned_whole(pipeline))
    }

    /// D2289: every protocol's re-open of the rerun set `members`, in declaration order; the first error stops.
    pub fn reopen(&self, members: &[u16]) -> PgResult<()> {
        self.asked(ask::REOPEN).try_for_each(|l| l.reopen(members))
    }

    /// M8 (RT-1): whether any protocol runs `pipeline` only on demand.
    pub fn deferred(&self, pipeline: usize) -> bool {
        self.asked(ask::DEFERRED).any(|l| l.deferred(pipeline))
    }

    /// M8 (RT-2e): the owner of the rerun set `pipeline` is a member of, if any protocol names one.
    pub fn rerun_owner(&self, pipeline: usize) -> Option<usize> {
        self.asked(ask::RERUN_OWNER).find_map(|l| l.rerun_owner(pipeline))
    }

    /// M8 (RT-2f): the readers of the pulled CTE plan `pipeline`, if any protocol names them.
    pub fn pull_readers(&self, pipeline: usize) -> Option<&[usize]> {
        self.asked(ask::PULL_READERS).find_map(|l| l.pull_readers(pipeline))
    }

    /// M8 (RT-2f): every pulled set a protocol names to run to its end at the statement's end; D2605: the higher drain rank
    /// first (PostgreSQL's `es_auxmodifytables`, the later CTE plan first), else in the protocols' order.
    /// memsig E-pub: each set borrowed from its live (no copy), the order in one block on the prepare owner (`charged_vec`);
    /// none when no live drains. The sort is unstable on (rank, declaration index): the same order, with no merge buffer.
    pub fn drain_at_end(&self) -> gravity_parallel::DrainSets<'_> {
        if !self.answered(ask::DRAIN) {
            return gravity_parallel::DrainSets::none();
        }
        let mut sets: memledger::ChargedVec<(core::cmp::Reverse<u16>, usize, &[u16])> = self.charged_vec();
        sets.with_vec(|v| v.extend(self.asked(ask::DRAIN).filter_map(|l| l.drain_at_end().map(|m| (l.drain_rank(), m))).enumerate().map(|(i, (r, m))| (core::cmp::Reverse(r), i, m)))); // alloc: Exec charged v4.owner
        sets.sort_unstable_by_key(|s| (s.0, s.1));
        let mut out = self.charged_vec();
        out.with_vec(|v| v.extend(sets.iter().map(|s| s.2))); // alloc: Exec charged v4.owner
        out
    }

    /// Whether any live answers question `q` (`ask`).
    pub fn answered(&self, q: u16) -> bool {
        self.asks.iter().any(|&a| a & q != 0)
    }

    /// M8 (RT-1): publish `f` to every protocol (the runner's frame while it runs the program, `None` after); the frame
    /// published before, which the caller puts back when it stops (gravity_parallel `Published`: nested runs stack).
    pub fn set_frame(&self, f: Option<Frame>) -> Option<Frame> {
        if self.lives.is_empty() {
            return None;
        }
        self.asked(ask::FRAME).for_each(|l| l.frame(f));
        self.frame.replace(f)
    }

    /// D2584: the emits from pipeline `from` on (bit p = pipeline p, the first 64) a protocol pruned whole for this run, as
    /// decided so far: the runner marks them `Exec::run_empty` before they start, so the driver never opens them (an arm
    /// that streams into the sink has no producer arm to consult `pruned_whole`).
    pub fn pruned_emits(&self, program: &Program, from: usize) -> u64 {
        if self.lives.is_empty() {
            return 0;
        }
        (from..program.pipelines.len().min(64)).filter(|&p| gravity_driver::is_emit(program, p) && self.pruned_whole(p)).fold(0, |m, p| m | 1 << p)
    }
}

/// D2914 (2a): whether `reader`, an eager Init's or an AHEAD set's one reader, can run as a section, where its start
/// runs the members ahead (runner/producers.rs `prerun_members`: not an emit, and `Section::plan` with no sink legs: an
/// eligible pipeline with a driving site that does not end in the sink). An emit or a serial reader runs nothing ahead:
/// its first read runs the members nested. (A reader that itself runs only nested, bound at width 0, neither:
/// `nested_members`.)
pub(crate) fn reader_runs_wide(program: &Program, reader: usize) -> bool {
    reader < program.pipelines.len() && !gravity_driver::is_emit(program, reader) && {
        let r = &program.pipelines[reader];
        r.is_eligible() && r.driving_site >= 0 && r.ends_in != gravity_program::End::Sink
    }
}

/// D2914 (2a): per pipeline, whether it runs only nested, on the leader through its width-1 arm (a deferred member,
/// `Live::deferred`, that never runs ahead): a member of an Init but the eager one whose reader can run on workers (its
/// members run ahead at the statement's width there, D1035 step 2), of a rerun set, of a LAZY hashed set's build but an
/// AHEAD one whose reader can run on workers (run ahead as the eager Init's), and of a pulled set (a CTE's plan, a
/// window's pulled fill, a merge join's pulled inner). The runner binds them at width 0 (runner/plan.rs
/// `RunnerPlan::prepare`), the width they run at, as the planner prices a SubLink's subplan and a CTE's plan (planner
/// gravity_paths.rs `gravity_width`).
///
/// An eager Init's or an AHEAD set's reader that itself runs only nested is bound at width 0, so it runs nothing ahead
/// either: its members are nested too, to a fixed point (each round marks at least one more pipeline, so at most one
/// round per pipeline).
///
/// D3137: with nested sections on (`sections`), a rerun set's members and an Init's keep the statement's width: their
/// nested run is a section over a big scan (gravity_parallel `nested_section`, decided at each run), the leader alone
/// otherwise.
pub fn nested_members(program: &Program, sections: bool) -> Vec<bool> {
    let protos = program.v4.as_deref().map_or(&[][..], |d| &d.protos[..]);
    let mut nested = vec![false; program.pipelines.len()]; // alloc: Exec
    loop {
        let mut grew = false;
        for p in protos {
            if sections && matches!(p.kind, ProtoKind::Rerun | ProtoKind::Init) {
                continue;
            }
            let set = match p.kind {
                // a rerun set declares each member in a proto of its own
                ProtoKind::Rerun => {
                    if let Some(n) = nested.get_mut(p.pipeline as usize).filter(|n| !**n) {
                        *n = true;
                        grew = true;
                    }
                    continue;
                }
                ProtoKind::Init => subplan::initplan::init::runs_nested(p, program),
                ProtoKind::SubHash => subplan::hash::builds_nested(p, program),
                _ => gravity_program::protos::pulled(p),
            };
            // an eager Init or an AHEAD set whose reader runs only nested (`NONE` names none: never in `nested`)
            let ahead = match p.kind {
                ProtoKind::Init => subplan::initplan::init::eager(p),
                ProtoKind::SubHash => subplan::hash::ahead(p),
                _ => false,
            };
            let set = set || ahead && nested.get(p.peer as usize).copied().unwrap_or(false);
            // D3095: with nested sections on, a CTE's pulled producers keep the width (the pull runs each whole, as a section
            // when `nested_section` says so, runner/producers.rs `pull_members`): its pulled pipeline and lazy builds alone run nested
            let cte = sections && p.kind == ProtoKind::Cte;
            if set && (p.pipeline as usize) < nested.len() {
                for m in program.members(p.pipeline as usize).members {
                    if cte && m != p.pipeline && program.pipelines[m as usize].lazy_for < 0 {
                        continue;
                    }
                    grew |= !core::mem::replace(&mut nested[m as usize], true);
                }
            }
        }
        if !grew {
            return nested;
        }
    }
}

#[cfg(test)]
mod nested_tests {
    use gravity_abi::breaker::{Breaker, BreakerKind};
    use gravity_kernels::ops::shared::object::entries as abi;
    use gravity_program::protos::{Proto, ProtoKind, V4Descs, NONE, WINDOW_PULLED};
    use gravity_program::{Arena, Code, End, Layout, Pipeline, Reason, SiteDesc, WritePlan};

    /// A scan into slot 0, read into slot 1 (pipelines 0, 1), another scan into slot 2 (pipeline 2), and the sink reading
    /// slots 1 and 2, declaring `protos`.
    fn program(protos: Vec<Proto>) -> gravity_program::Program {
        let st = Arena::new(1 << 16);
        let sites = [SiteDesc::range_source(st.handle(), 7, &[1])];
        let layout = Layout { nsites: 1, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 };
        let agg = |s| End::Breaker(Breaker::sized(BreakerKind::PlainAgg, 64).in_slot(s));
        let pipes = [Pipeline::eligible(0, agg(0), 10), Pipeline::eligible(0, agg(1), 10).consuming(&[0]), Pipeline::eligible(0, agg(2), 10), Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[1, 2])];
        let p = gravity_program::Program::with_pipelines(layout, &sites, &[], WritePlan::SELECT, pipes.map(|p| (p, Code::hand(super::ctescan::raw::test_noop))));
        p.with_v4(V4Descs::new(protos))
    }

    /// D2914 (2a): the members of an Init but the eager one, a rerun set's, a LAZY set's build but an AHEAD one's and a
    /// pulled set's run only nested; an eager Init's or an AHEAD set's whose reader can run on workers, a plain set's and
    /// every other pipeline keep the width.
    #[test]
    fn nested_members_are_the_on_demand_sets_that_never_run_ahead() {
        let at = |protos: Vec<Proto>| super::nested_members(&program(protos), false);
        let init = |peer: u16, eager: bool| Proto::new(ProtoKind::Init, 1).peer(peer).arg(1 | if eager { abi::INIT_EAGER } else { 0 });
        assert_eq!(at(vec![]), [false; 4]); // alloc: Cold
        assert_eq!(at(vec![init(NONE, false)]), [true, true, false, false], "an Init's members: its pipeline and its exclusive producer"); // alloc: Cold
        assert_eq!(at(vec![init(NONE, true)]), [true, true, false, false], "INIT_EAGER with no reader named: lazy"); // alloc: Cold
        assert_eq!(at(vec![init(2, true)]), [false; 4], "the eager Init, its reader eligible: run ahead at the statement's width"); // alloc: Cold
        assert_eq!(at(vec![init(3, true)]), [true, true, false, false], "the eager Init read by the emit: nothing runs ahead"); // alloc: Cold
        let eager2 = Proto::new(ProtoKind::Init, 2).peer(1).arg(1 | abi::INIT_EAGER);
        assert_eq!(at(vec![init(NONE, false), eager2]), [true, true, true, false], "an eager Init read by a nested reader"); // alloc: Cold
        assert_eq!(at(vec![eager2, init(NONE, false)]), [true, true, true, false], "the same, its reader's Init declared after it"); // alloc: Cold
        assert_eq!(at(vec![eager2]), [false; 4], "the same reader, not nested: run ahead"); // alloc: Cold
        let set = |arg: u64, peer: u16| Proto::new(ProtoKind::SubHash, 1).peer(peer).arg(arg);
        assert_eq!(at(vec![set(abi::SUBHASH_LAZY, NONE)]), [true, true, false, false], "a LAZY set's build"); // alloc: Cold
        assert_eq!(at(vec![set(abi::SUBHASH_LAZY | abi::SUBHASH_AHEAD, 2)]), [false; 4], "an AHEAD set's build, its reader eligible"); // alloc: Cold
        assert_eq!(at(vec![set(abi::SUBHASH_LAZY | abi::SUBHASH_AHEAD, 3)]), [true, true, false, false], "an AHEAD set read by the emit"); // alloc: Cold
        assert_eq!(at(vec![set(0, NONE)]), [false; 4], "a set built in program order"); // alloc: Cold
        assert_eq!(at(vec![Proto::new(ProtoKind::Rerun, 2).peer(3)]), [false, false, true, false], "a rerun set's member, one proto each"); // alloc: Cold
        assert_eq!(at(vec![Proto::new(ProtoKind::Cte, 1)]), [true, true, false, false], "a CTE's pulled plan"); // alloc: Cold
        assert_eq!(at(vec![Proto::new(ProtoKind::Window, 1).arg(WINDOW_PULLED)]), [true, true, false, false], "a window's pulled fill"); // alloc: Cold
        assert_eq!(at(vec![Proto::new(ProtoKind::Window, 1)]), [false; 4], "a window that reads its fill whole"); // alloc: Cold
    }

    /// D3137: with nested sections on, a rerun set's member and every Init's keep the statement's width (D3014 S2':
    /// a rerun member over a big inner runs as a section; D2998: so does an Init's), and an eager Init read by an Init's
    /// member too; a LAZY set's build and a pulled set stay nested.
    #[test]
    fn d3137_rerun_and_init_members_keep_the_width_while_nested_sections_are_on() {
        let at = |protos: Vec<Proto>| super::nested_members(&program(protos), true);
        let init = |peer: u16, eager: bool| Proto::new(ProtoKind::Init, 1).peer(peer).arg(1 | if eager { abi::INIT_EAGER } else { 0 });
        assert_eq!(at(vec![Proto::new(ProtoKind::Rerun, 2).peer(3)]), [false; 4], "a rerun set's member"); // alloc: Cold
        assert_eq!(at(vec![init(NONE, false)]), [false; 4], "a lazy Init's members"); // alloc: Cold
        assert_eq!(at(vec![init(3, true)]), [false; 4], "the eager Init read by the emit"); // alloc: Cold
        let eager2 = Proto::new(ProtoKind::Init, 2).peer(1).arg(1 | abi::INIT_EAGER);
        assert_eq!(at(vec![init(NONE, false), eager2]), [false; 4], "an eager Init read by an Init's member"); // alloc: Cold
        assert_eq!(at(vec![Proto::new(ProtoKind::SubHash, 1).arg(abi::SUBHASH_LAZY)]), [true, true, false, false], "a LAZY set's build"); // alloc: Cold
        // D3095: a CTE's pulled pipeline runs nested; its producer, which the pull runs whole, keeps the width
        assert_eq!(at(vec![Proto::new(ProtoKind::Cte, 1)]), [false, true, false, false], "a CTE's pulled plan"); // alloc: Cold
        let eager_cte = Proto::new(ProtoKind::Init, 2).peer(1).arg(1 | abi::INIT_EAGER);
        assert_eq!(at(vec![Proto::new(ProtoKind::Cte, 1), eager_cte]), [false, true, false, false], "an eager Init read by a pulled member keeps the width"); // alloc: Cold
    }
}

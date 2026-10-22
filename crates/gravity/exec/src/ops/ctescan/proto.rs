//! `nest.cte`'s runtime object (M8-MEMO-CTE CT-2): the CTE object behind
//! `m8_cte_pull` (`ProtoKind::Cte`, lowerv4 nest/cte.rs; kernels host_abi/v4/m8.rs `CTE_MAGIC` / `OP_CTE_PULL`).
//!
//! PostgreSQL keeps one tuplestore per CTE (nodeCtescan.c); a CteScan past the stored rows pulls the CTE's plan on. The rows
//! live in the program's nest.spool record store (spool_rec.rs; the count and the pull's words are parameter words the IR
//! reads and writes). A reader past the stored rows calls `m8_cte_pull(ctx, h, n)`: the object's members (the CTE's
//! pipeline, last, and its exclusive producers) run on through the runner's frame (`OnDemand::pull_members`, RT-2f) until
//! the CTE pipeline's end took `n` more rows (it stops resumably) or ends. The answer: 0 stopped, 1 ended (sticky), ERROR
//! with the stashed error; after an error every later pull is an error by name, never a stale count. A pull that asked for
//! more than one row (lowerv4 ops/ctescan/lower.rs `Pull::Grow` / `Window`: a plan that runs no user or VOLATILE code)
//! may raise past the rows its readers read: the object holds the plan's error (`HELD`, 2) behind the rows the pull stored
//! before it, and the next pull answers it, so a reader raises it where PG raises it, at its read past those rows, and a
//! reader that stops before never sees it (as the pull adapter holds a fetch's error behind its completed rows, driver
//! pull.rs `failed`). A cancel, a refusal or any error not the plan's own is never held. The members are
//! deferred: the producer order steps past them, so a CTE no reader pulls never runs, as PG's. The CTE's readers keep its
//! slot (`pull_readers`: the driver opens it zeroed for them as it steps past the deferred plan).
//!
//! A data-modifying CTE (the proto's DRAIN bit, lowerv4 nest/cte.rs): its pulled set is named to RT-2f's `drain_at_end`,
//! so the statement's end pulls it to its end whatever its readers read (PG's ExecPostprocessPlan); a set a reader pulled
//! to its end runs nothing again (`Pulled::Done` is sticky).
//!
//! The object lives in the protocol's `Live`, made at each execution's prepare (its handle laid in its word) and dropped at
//! its end. Created by M8-MEMO-CTE.
use core::cell::Cell;

use gravity_kernels::ops::shared::object::entries as abi;
/// The object's head, magic and operation code, for ops/ctescan/raw.rs.
pub(crate) use abi::{Head, CTE_MAGIC, OP_CTE_PULL};
use gravity_program::protos::{Proto, ProtoKind};
use types_error::PgResult;

use super::super::{Frame, Live, Prepare, Protocol};
use crate::glue;

pub struct Cte;

/// The object's words: the head, then the address of its state.
pub(crate) const WORDS: usize = core::mem::size_of::<abi::Head>() / 8 + 1;
/// The proto's `arg` bit (lowerv4 nest/cte.rs `DRAIN`): a data-modifying CTE, drained at the statement's end.
const DRAIN: u64 = 1 << 32;

pub(crate) struct CteRt {
    /// The pulled set in program order: the CTE's pipeline last.
    members: Box<[u16]>,
    /// with#170 / #171: the members a re-arm runs again (`rearmed`): those of the re-arming set, the CTE's pipeline last.
    rearmed: Box<[u16]>,
    frame: Cell<Option<Frame>>,
    running: Cell<bool>,
    done: Cell<bool>,
    failed: Cell<bool>,
    /// The plan's error held behind the rows its pull stored (`HELD`): the next pull answers it.
    held: Cell<Option<Box<types_error::PgError>>>,
}

/// `OP_CTE_PULL`'s answer for an error held behind the rows the pull stored (lowerv4 ops/ctescan/lower.rs `HELD`).
const HELD: i64 = 2;

struct CteLive {
    _cell: Box<[u64]>,
    rt: Box<CteRt>,
    /// The CTE's pipeline and its readers (every pipeline reading its slot's life).
    k: usize,
    readers: Vec<usize>,
    drain: bool,
    /// D2605: the drain rank (the CTE's plan id, `CTE_RANK_SHIFT`): the statement-end drain runs the higher first.
    rank: u16,
}

impl Live for CteLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::DEFERRED | crate::ops::ask::FRAME | crate::ops::ask::PULL_READERS | crate::ops::ask::DRAIN
    }

    fn deferred(&self, pipeline: usize) -> bool {
        u16::try_from(pipeline).is_ok_and(|p| self.rt.members.contains(&p))
    }

    fn frame(&self, f: Option<Frame>) {
        self.rt.frame.set(f);
    }

    fn pull_readers(&self, pipeline: usize) -> Option<&[usize]> {
        (pipeline == self.k && !self.readers.is_empty()).then(|| &self.readers[..])
    }

    fn drain_at_end(&self) -> Option<&[u16]> {
        self.drain.then_some(&self.rt.members[..])
    }

    fn drain_rank(&self) -> u16 {
        self.rank
    }
}

impl Protocol for Cte {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Cte
    }

    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        // D3040: a CTE filled in program order (its owned spool sealed after the fill) has no pulled set and no object
        if p.arg & gravity_program::protos::CTE_FILL_PARALLEL != 0 {
            return Ok(None);
        }
        lay(p, cx.program, cx.words, cx.nulls, cx.nparams).map(Some)
    }
}

/// The crate rig's prepare (lowerv4 tests/common/on_demand.rs): each Cte proto's object, as `Cte::prepare` lays it.
pub fn prepare_bare(program: &gravity_program::Program, words: &mut [u64], nulls: &mut [u8], nparams: &mut usize, out: &mut crate::ops::LiveList) -> PgResult<()> {
    let protos = program.v4.as_ref().map_or(&[][..], |d| &d.protos[..]);
    for p in protos.iter().filter(|p| p.kind == ProtoKind::Cte && p.arg & gravity_program::protos::CTE_FILL_PARALLEL == 0) {
        let l = lay(p, program, words, nulls, nparams)?;
        out.with_vec(|v| v.push(l)); // alloc: Exec charged out
    }
    Ok(())
}

/// The CTE's pulled set: pipeline `p` and its exclusive producers (`Program::members`), in program order, less every member
/// of a rerun set not holding `p` (a recursive union's term, a nest's inner): its owner's `m8_rerun` runs it, never the pull
/// (W8-RECURSE F7). setop_rec-0043: a set holding `p` re-arms the CTE (lowerv4 ops/ctescan/lower.rs `rearms`, `rearm`
/// below): its members here stay the pull's, which runs them again after each re-arm; such a set holding `p` but not every
/// member of the pull is a Bug by name (exec rerun/mod.rs checks the other half).
/// D4189: less every pipeline that waits for such a member's first row too (`Pipeline::lazy_for`: D824's lazy build of a
/// term's probe and the producers fed into it, D1005; one waiting for one of those in turn): the probe's wait runs them
/// once, inside its owner's run (`Entry::build_wait`). Left in the pull, whose lazy test asks for the probe among these
/// members, they ran at its first call, ahead of the term's first row, and the build again at the wait, over its
/// feeder's table already freed (D4103's 'unsupported execution (kind 2)').
pub(crate) fn members(program: &gravity_program::Program, p: usize) -> PgResult<Vec<u16>> {
    let protos = program.v4.as_ref().map_or(&[][..], |d| &d.protos[..]);
    let holds_p = |x: &Proto| protos.iter().any(|y| y.kind == ProtoKind::Rerun && y.words.base == x.words.base && y.pipeline as usize == p);
    let rerun = |q: usize| protos.iter().any(|x| x.kind == ProtoKind::Rerun && x.pipeline as usize == q && !holds_p(x));
    if rerun(p) {
        return Err(bad(&format!("Bug: nest.cte's pipeline {p} is a rerun set's member (W8-RECURSE F7)"))); // alloc: Cold
    }
    // a lazy build precedes its probe (lowerv4 frame/program.rs), so the walk ends
    let waits = |q: usize| core::iter::successors(Some(q), |&x| usize::try_from(program.pipelines[x].lazy_for).ok().filter(|&r| r > x && r < program.pipelines.len())).skip(1).any(rerun);
    let mut m: Vec<u16> = program.members(p).members.into_iter().filter(|&q| !rerun(q as usize) && !waits(q as usize)).collect(); // alloc: Exec
    m.sort_unstable();
    m.dedup();
    Ok(m)
}

/// setop_rec-0043 (PG's ExecReScanCteScan: tuplestore_clear, the plan re-run lazily at the next read): the CTE re-armed
/// by a rerun set's run (ops/ctescan/raw.rs `rearm_ctes`, as `rearm_sets`): its answers start again (no end, no error,
/// nothing held) and the driver's pull state and kept slots of its pulled set are reset through the runner's frame
/// (`OnDemand::rearm_pulled`); its store's words are the owner run's. A re-arm inside its own pull is a Bug by name.
pub(crate) fn rearm(rt: &CteRt) -> PgResult<()> {
    if rt.running.get() {
        return Err(bad("m8_rerun: a CTE re-armed inside its own pull (setop_rec-0043)"));
    }
    let Some(mut f) = rt.frame.get() else {
        return Err(bad("m8_rerun: no runner frame to re-arm a CTE (bug: a re-open on a worker; setop_rec-0043)"));
    };
    rt.held.take();
    rt.done.set(false);
    rt.failed.set(false);
    super::raw::on_demand(&mut f).rearm_pulled(&rt.rearmed).map_err(glue::pg_error)
}

/// with#170 / #171: the members of CTE `p`'s pulled set a re-arm runs again: those of a rerun set holding `p` (the term's
/// set re-arming it), `p` last. A member no rerun set holds (a Hash build over another CTE's store the set's members probe:
/// lowerv4 ops/ctescan/lower.rs `rearms`' kept member) ran at the first pull and keeps its product, as PG's ExecReScanHashJoin
/// keeps a one-batch table whose inner reads no changed parameter; with no set holding `p` this is every member.
fn rearmed(program: &gravity_program::Program, p: usize, members: &[u16]) -> Vec<u16> {
    let protos = program.v4.as_ref().map_or(&[][..], |d| &d.protos[..]);
    let sets = || protos.iter().filter(|x| x.kind == ProtoKind::Rerun && x.pipeline as usize == p).map(|x| x.words.base);
    let none = sets().next().is_none();
    let held = |m: u16| protos.iter().any(|x| x.kind == ProtoKind::Rerun && x.pipeline == m && sets().any(|b| b == x.words.base));
    members.iter().copied().filter(|&m| none || held(m)).collect() // alloc: Exec
}

fn lay(p: &Proto, program: &gravity_program::Program, words: &mut [u64], nulls: &mut [u8], nparams: &mut usize) -> PgResult<Box<dyn Live>> {
    let base = p.words.base as usize;
    let k = p.pipeline as usize;
    if p.words.n != 1 || base >= words.len() || base >= nulls.len() || k >= program.pipelines.len() {
        return Err(bad("nest.cte's declaration is not one handle word over one of the program's pipelines"));
    }
    let members = members(program, k)?;
    if members.last() != Some(&p.pipeline) {
        return Err(bad("nest.cte's pulled set does not end in the CTE's pipeline"));
    }
    let rearmed = rearmed(program, k, &members).into_boxed_slice();
    let rt = Box::new(CteRt { members: members.into_boxed_slice(), rearmed, frame: Cell::new(None), running: Cell::new(false), done: Cell::new(false), failed: Cell::new(false), held: Cell::new(None) }); // alloc: Exec
    let mut cell = vec![0u64; WORDS].into_boxed_slice(); // alloc: Exec
    super::raw::lay_head(&mut cell, &rt);
    words[base] = cell.as_ptr() as u64;
    nulls[base] = 0;
    *nparams = (*nparams).max(base + 1);
    let readers = program.consumers_of(gravity_program::LifeId(p.pipeline)).collect(); // alloc: Exec
    let rank = (p.arg >> gravity_program::protos::CTE_RANK_SHIFT) as u16;
    Ok(Box::new(CteLive { _cell: cell, rt, k, readers, drain: p.arg & DRAIN != 0, rank })) // alloc: Exec
}

/// `OP_CTE_PULL`: the members run on until the CTE pipeline's end took `n` more rows (0) or the CTE ended (1, sticky); a
/// plan error under a pull of more than one row is held (`HELD`) and answered by the next pull.
pub(super) fn pull(rt: &CteRt, n: u64) -> PgResult<i64> {
    if let Some(e) = rt.held.take() {
        return Err(e);
    }
    if rt.done.get() {
        return Ok(1);
    }
    if rt.failed.get() {
        return Err(bad("m8_cte_pull: a CTE read again after its plan's error (M8 RT-2f)"));
    }
    if rt.running.get() {
        return Err(bad("m8_cte_pull: a CTE pulled inside its own plan's run"));
    }
    let Some(mut f) = rt.frame.get() else {
        return Err(bad("m8_cte_pull: no runner frame (bug: a pull on a worker; the lowering pins every reader to the leader)"));
    };
    rt.running.set(true);
    // The frame is the runner's, published on this thread while it runs the program (gravity_parallel `publish`, whose
    // guard takes it back on every exit); this call is inside one of its calls, and nothing else uses the frame while the
    // members run (raw.rs `on_demand`'s contract).
    let r = super::raw::on_demand(&mut f).pull_members(&rt.members, n.max(1));
    rt.running.set(false);
    match r {
        Ok(gravity_driver::Pulled::Done) => {
            rt.done.set(true);
            Ok(1)
        }
        Ok(gravity_driver::Pulled::Stopped) => Ok(0),
        Err(e) => {
            rt.failed.set(true);
            let plan = matches!(e, gravity_driver::Error::Raised(_) | gravity_driver::Error::Host(_));
            let e = glue::pg_error(e);
            if n > 1 && plan && holds(&e) {
                rt.held.set(Some(e));
                return Ok(HELD);
            }
            Err(e)
        }
    }
}

/// An error a pull may hold behind its stored rows: an ERROR, not a cancel or shutdown (SQLSTATE class 57: a held one a
/// reader never reaches would drop it).
fn holds(e: &types_error::PgError) -> bool {
    e.level() == types_error::ERROR && types_error::errcode_to_category(e.sqlstate()) != types_error::ERRCODE_OPERATOR_INTERVENTION
}

#[cold]
#[inline(never)]
pub(super) fn bad(what: &str) -> Box<types_error::PgError> {
    glue::internal(&format!("gravity: {what}")) // alloc: Cold
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rec {
        pulls: Vec<(Vec<u16>, u64)>, // alloc: Cold
        answer: Vec<gravity_driver::Result<gravity_driver::Pulled>>, // alloc: Cold
    }

    impl gravity_driver::OnDemand for Rec {
        fn run_members(&mut self, _members: &[u16]) -> gravity_driver::Result<()> {
            unreachable!("a CTE pulls; it never runs its members whole")
        }
        fn pull_members(&mut self, members: &[u16], budget: u64) -> gravity_driver::Result<gravity_driver::Pulled> {
            self.pulls.push((members.to_vec(), budget)); // alloc: Cold
            self.answer.remove(0)
        }
    }

    fn rt(members: &[u16]) -> CteRt {
        CteRt { members: members.into(), rearmed: members.into(), frame: Cell::new(None), running: Cell::new(false), done: Cell::new(false), failed: Cell::new(false), held: Cell::new(None) } // alloc: Cold
    }

    fn with_frame<R>(rt: &CteRt, rec: &mut Rec, f: impl FnOnce(&CteRt) -> R) -> R {
        // the test's frame outlives every pull below
        let fr = super::super::raw::test_frame(rec);
        rt.frame.set(Some(fr));
        let r = f(rt);
        rt.frame.set(None);
        r
    }

    #[test]
    fn a_pull_runs_the_set_to_its_budget_and_its_end_is_sticky() {
        let r = rt(&[2, 5]);
        let mut rec = Rec { pulls: Vec::new(), answer: vec![Ok(gravity_driver::Pulled::Stopped), Ok(gravity_driver::Pulled::Done)] }; // alloc: Cold
        with_frame(&r, &mut rec, |r| {
            assert_eq!(pull(r, 1).unwrap(), 0);
            assert_eq!(pull(r, 4096).unwrap(), 1);
            assert_eq!(pull(r, 1).unwrap(), 1);
        });
        assert_eq!(rec.pulls, vec![(vec![2, 5], 1), (vec![2, 5], 4096)]); // alloc: Cold
    }

    #[test]
    fn a_failed_pull_is_the_error_and_every_later_pull_is_one_by_name() {
        let r = rt(&[3]);
        let mut rec = Rec { pulls: Vec::new(), answer: vec![Err(gravity_driver::Error::Refused("boom".into()))] }; // alloc: Cold
        with_frame(&r, &mut rec, |r| {
            assert!(pull(r, 1).is_err());
            let e = pull(r, 1).unwrap_err();
            assert!(e.message().contains("after its plan's error"), "{}", e.message());
        });
        assert_eq!(rec.pulls.len(), 1);
    }

    /// A plan error under a pull of more than one row is held behind the rows the pull stored: the pull answers HELD, the
    /// next answers the error, and every later one is an error by name; under a one-row pull it is the pull's own error.
    #[test]
    fn a_wide_pull_holds_its_plan_error_for_the_next_pull() {
        let r = rt(&[3]);
        let mut rec = Rec { pulls: Vec::new(), answer: vec![Ok(gravity_driver::Pulled::Stopped), Err(gravity_driver::Error::Host("boom".into()))] }; // alloc: Cold
        with_frame(&r, &mut rec, |r| {
            assert_eq!(pull(r, 1).unwrap(), 0);
            assert_eq!(pull(r, 2).unwrap(), HELD);
            let e = pull(r, 2).unwrap_err();
            assert!(e.message().contains("boom"), "{}", e.message());
            assert!(pull(r, 4).unwrap_err().message().contains("after its plan's error"));
        });
        assert_eq!(rec.pulls, vec![(vec![3], 1), (vec![3], 2)]); // alloc: Cold
        let r = rt(&[3]);
        let mut rec = Rec { pulls: Vec::new(), answer: vec![Err(gravity_driver::Error::Host("boom".into()))] }; // alloc: Cold
        with_frame(&r, &mut rec, |r| assert!(pull(r, 1).unwrap_err().message().contains("boom")));
    }

    /// A run-time refusal is never held (nor a cancel: `plan` takes only Raised and Host): a reader that stops before
    /// reaching a held one would drop it.
    #[test]
    fn a_refusal_is_never_held() {
        let r = rt(&[3]);
        let mut rec = Rec { pulls: Vec::new(), answer: vec![Err(gravity_driver::Error::Refused("wall".into()))] }; // alloc: Cold
        with_frame(&r, &mut rec, |r| assert!(pull(r, 4).is_err()));
    }

    /// A build (slot 0), a probe reading it into slot 1, the CTE's pipeline reading slot 1 (slot 2) and its reader (the
    /// sink); `rerun`: the pipelines a Rerun proto names (owner 2).
    fn program(rerun: &[u16]) -> gravity_program::Program {
        program_with(rerun, false)
    }

    /// `program`; `lazy`: the build is the probe's lazy build (D824: `lazy_for` 1).
    fn program_with(rerun: &[u16], lazy: bool) -> gravity_program::Program {
        use gravity_abi::breaker::{Breaker, BreakerKind};
        use gravity_program::protos::V4Descs;
        use gravity_program::{Arena, Code, End, Layout, Pipeline, SiteDesc, WritePlan};
        let st = Arena::new(1 << 16);
        let sites = [SiteDesc::range_source(st.handle(), 7, &[1])];
        let layout = Layout { nsites: 1, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 };
        let agg = |s| End::Breaker(Breaker::sized(BreakerKind::PlainAgg, 64).in_slot(s));
        let read = Pipeline::serial(-1, End::Sink, gravity_program::Reason::NoDrivingSite).consuming(&[2]);
        let build = Pipeline { lazy_for: if lazy { 1 } else { -1 }, ..Pipeline::eligible(0, agg(0), 10) };
        let pipes = [build, Pipeline::eligible(0, agg(1), 10).consuming(&[0]), Pipeline::eligible(0, agg(2), 10).consuming(&[1]), read];
        let p = gravity_program::Program::with_pipelines(layout, &sites, &[], WritePlan::SELECT, pipes.map(|p| (p, Code::hand(super::super::raw::test_noop))));
        p.with_v4(V4Descs::new(rerun.iter().map(|&m| Proto::new(ProtoKind::Rerun, m).peer(2).words(0, 1)).collect()))
    }

    /// F7: a rerun member leaves the pull (its owner reruns it); the CTE's pipeline as one is a Bug by name.
    #[test]
    fn a_rerun_member_leaves_the_pulled_set_and_a_set_holding_the_ctes_own_pipeline_rearms_it() {
        assert_eq!(members(&program(&[]), 2).unwrap(), vec![0, 1, 2]); // alloc: Cold
        assert_eq!(members(&program(&[1]), 2).unwrap(), vec![0, 2], "the probe (a rerun member) is its owner's"); // alloc: Cold
        assert_eq!(members(&program(&[2]), 2).unwrap(), vec![0, 1, 2], "setop_rec-0043: the set re-arms the CTE; the pull keeps its set"); // alloc: Cold
        assert_eq!(members(&program(&[0, 1, 2]), 2).unwrap(), vec![0, 1, 2]); // alloc: Cold
    }

    /// D4189: the lazy build of a probe the pull leaves to its owner leaves with it (the probe's wait runs it, once); the
    /// lazy build of a probe the pull keeps stays (`Entry::pull_nested` steps past it and the wait runs it).
    #[test]
    fn a_lazy_build_follows_its_probe_out_of_the_pulled_set() {
        assert_eq!(members(&program_with(&[1], true), 2).unwrap(), vec![2], "the probe is its owner's, so its lazy build is the probe's wait's"); // alloc: Cold
        assert_eq!(members(&program_with(&[], true), 2).unwrap(), vec![0, 1, 2], "the pull's own probe: its lazy test steps past the build"); // alloc: Cold
        assert_eq!(members(&program_with(&[2], true), 2).unwrap(), vec![0, 1, 2], "a set holding the CTE's pipeline re-arms it: the pull keeps its set"); // alloc: Cold
    }

    /// setop_rec-0043: a re-arm clears the end, the error and a held one, and resets the pull through the frame; inside the
    /// CTE's own pull it is a Bug by name.
    #[test]
    fn a_rearm_starts_the_cte_again_and_inside_its_pull_is_a_bug() {
        struct Re(Vec<Vec<u16>>); // alloc: Cold
        impl gravity_driver::OnDemand for Re {
            fn run_members(&mut self, _members: &[u16]) -> gravity_driver::Result<()> {
                unreachable!()
            }
            fn rearm_pulled(&mut self, members: &[u16]) -> gravity_driver::Result<()> {
                self.0.push(members.to_vec()); // alloc: Cold
                Ok(())
            }
        }
        let r = rt(&[0, 2]);
        r.done.set(true);
        r.held.set(Some(bad("held")));
        let mut re = Re(Vec::new()); // alloc: Cold
        // the test's frame outlives the re-arms below
        let fr = super::super::raw::test_frame(&mut re);
        r.frame.set(Some(fr));
        rearm(&r).unwrap();
        assert!(!r.done.get() && !r.failed.get() && r.held.take().is_none());
        r.running.set(true);
        assert!(rearm(&r).unwrap_err().message().contains("inside its own pull"));
        r.frame.set(None);
        assert_eq!(re.0, vec![vec![0u16, 2]]); // alloc: Cold
    }

    #[test]
    fn a_pull_with_no_frame_is_a_bug_by_name() {
        let e = pull(&rt(&[1]), 1).unwrap_err();
        assert!(e.message().contains("no runner frame"), "{}", e.message());
    }
}

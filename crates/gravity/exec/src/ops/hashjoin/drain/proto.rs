//! M7's per-statement protocols for v4 programs (M7-plan.md §3.6 / §6, cards/M7.md L2 hosts): the drain's order edge
//! (`join.follows`, below) and, as their readers land, the drain's seeded `m` (the probe pipeline's partial handed to the
//! drain end's merge, `m7_absorb`) and the drain's runtime words. The spill runtime is M7-SPILL's (v4/m7_spill.rs).
//! Created by M7-DRAIN.
use std::cell::Cell;

use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::Program;
use types_error::PgResult;

use crate::ops::{Live, Prepare, Protocol};
use crate::admit::{Admit, Bind};
use crate::glue;

pub const ADMIT: &[Admit] = &[];
pub const BIND: &[Bind] = &[];
pub const PROTOCOLS: &[&dyn Protocol] = &[&Follows];

/// `join.follows`: the drain `pipeline` opens only after its probe `peer` ended. The order
/// itself is the producer order every scheduler reads (`Program::producers_of` yields the peer: the DAG barrier's check
/// at the drain's open, the on-demand member sets, the lazy runs' feeders); this protocol refuses a declaration those
/// cannot honour and names a drain that ran before its probe ended.
struct Follows;

impl Protocol for Follows {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Follows
    }
    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        check(p, cx.program)?;
        Ok(Some(Box::new(Ran { drain: p.pipeline as usize, probe: p.peer as usize, ended: Cell::new(false) }))) // alloc: Exec
    }
}

/// A declaration the schedulers honour: the probe runs before the drain in program order, and neither is a lazy build
/// (D824 / D1005 run a lazy build out of order, inside its probe's wait) unless both are, for one probe (D2143: the drain
/// is the build or a producer fed into it, and its producers name its probe, so the driver's wait runs the probe, then
/// the drain, in program order, each with its after-pipeline step: lowerv4 frame/program.rs defers both or refuses).
fn check(p: &Proto, program: &Program) -> PgResult<()> {
    let (d, q) = (p.pipeline as usize, p.peer as usize);
    if d >= program.pipelines.len() || q >= d {
        return Err(glue::internal("gravity: join.follows: a drain that does not run after its probe"));
    }
    let (ld, lq) = (program.pipelines[d].lazy_for, program.pipelines[q].lazy_for);
    if (ld >= 0 || lq >= 0) && ld != lq {
        return Err(glue::internal("gravity: join.follows: a lazy build as a drain or its probe (it runs out of order)"));
    }
    Ok(())
}

/// The probe's end, seen by the runner's after-pipeline step (the section joined): the drain's own step names a drain
/// that ran first. Never an answer otherwise, so the runner's own after-pipeline steps run as without the protocol.
struct Ran {
    drain: usize,
    probe: usize,
    ended: Cell<bool>,
}

impl Ran {
    fn after(&self, pipeline: usize) -> Option<gravity_driver::Result<()>> {
        if pipeline == self.probe {
            self.ended.set(true);
        }
        let msg = || format!("gravity: join.follows: pipeline {} ran before its probe {} ended", self.drain, self.probe); // alloc: Cold
        (pipeline == self.drain && !self.ended.get()).then(|| Err(gravity_driver::Error::host(msg())))
    }
}

impl Live for Ran {
    fn answers(&self) -> u16 {
        crate::ops::ask::AFTER
    }

    fn after_pipeline(&self, pipeline: usize, _exec: &mut gravity_driver::Exec) -> Option<gravity_driver::Result<()>> {
        self.after(pipeline)
    }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use std::sync::Arc;

    use gravity_abi::breaker::{Breaker, BreakerKind};
    use gravity_abi::Ctx;
    use gravity_parallel::Barrier;
    use gravity_program::protos::V4Descs;
    use gravity_program::{Arena, Code, End, Layout, Pipeline, Reason, SiteDesc, WritePlan};

    use super::*;

    unsafe extern "C-unwind" fn noop(_: *mut Ctx) -> i64 {
        0
    }

    /// A build (slot 0), its probe and its drain (both read the build; each ends in the sink), the drain following the
    /// probe when `follows`; `lazy`: the probe's and the drain's `lazy_for` (-1: not deferred).
    fn program(follows: Option<Proto>, lazy: (i16, i16)) -> Program {
        let st = Arena::new(1 << 16);
        let sites = [SiteDesc::range_source(st.handle(), 7, &[1])];
        let layout = Layout { nsites: 1, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 };
        let build = Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::HashBuild, 64)), 10);
        let mut probe = Pipeline::eligible(0, End::Sink, 10).consuming(&[0]);
        probe.lazy_for = lazy.0;
        let mut drain = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0]);
        drain.lazy_for = lazy.1;
        let p = Program::with_pipelines(layout, &sites, &[], WritePlan::SELECT, [build, probe, drain].map(|p| (p, Code::hand(noop))));
        p.with_v4(V4Descs::new(follows.into_iter().collect()))
    }

    fn follows(drain: u16, probe: u16) -> Proto {
        Proto::new(ProtoKind::Follows, drain).peer(probe)
    }

    /// The order edge rides the producer order: the drain's producers are the build (its slot) and the probe.
    #[test]
    fn the_producer_order_yields_the_probe() {
        assert_eq!(program(None, (-1, -1)).producers_of(2).collect::<Vec<_>>(), vec![0]); // alloc: Cold
        assert_eq!(program(Some(follows(2, 1)), (-1, -1)).producers_of(2).collect::<Vec<_>>(), vec![0, 1]); // alloc: Cold
        assert_eq!(program(Some(follows(2, 1)), (-1, -1)).producers_of(1).collect::<Vec<_>>(), vec![0], "the probe follows nothing"); // alloc: Cold
    }

    /// A declaration no scheduler can honour is refused at prepare: a probe at or after its drain, a drain past the
    /// program, a lazy build as the probe or the drain alone, or each for another probe.
    #[test]
    fn a_follows_out_of_order_is_refused() {
        let p = program(None, (-1, -1));
        assert!(check(&follows(2, 1), &p).is_ok());
        for bad in [follows(1, 2), follows(2, 2), follows(3, 1)] {
            let e = check(&bad, &p).expect_err("refused");
            assert!(format!("{e:?}").contains("join.follows"), "{e:?}"); // alloc: Cold
        }
        for lazy in [(2, -1), (-1, 3), (3, 4)] {
            assert!(format!("{:?}", check(&follows(2, 1), &program(None, lazy)).expect_err("lazy")).contains("lazy build")); // alloc: Cold
        }
    }

    /// D2143: a probe and its drain both deferred for one probe run in its wait, in program order: honoured.
    #[test]
    fn a_probe_and_its_drain_deferred_together_are_honoured() {
        assert!(check(&follows(2, 1), &program(None, (3, 3))).is_ok());
    }

    /// A drain that ran before its probe ended is an error by name; in order, the protocol answers nothing (the runner's
    /// own after-pipeline steps run).
    #[test]
    fn a_drain_before_its_probe_is_named() {
        let ran = Ran { drain: 2, probe: 1, ended: Cell::new(false) };
        match ran.after(2) {
            Some(Err(gravity_driver::Error::Host(m))) => assert!(m.contains("join.follows: pipeline 2 ran before its probe 1 ended"), "{m}"),
            other => panic!("{:?}", other.map(|r| r.is_ok())),
        }
        let ran = Ran { drain: 2, probe: 1, ended: Cell::new(false) };
        assert!((0..3).all(|p| ran.after(p).is_none()));
    }

    /// The DAG barrier (parallel/barrier.rs) holds the drain until its probe is done.
    #[test]
    fn the_barrier_opens_the_drain_after_its_probe() {
        let p = Arc::new(program(Some(follows(2, 1)), (-1, -1)));
        let mut b = Barrier::new(&p);
        for q in 0..3 {
            assert_eq!(b.next(), Some(q));
            b.done(q);
        }
        assert!(b.finished());
    }

    /// A forced early open: the probe left unfinished, the drain's open fails by name.
    #[test]
    #[should_panic(expected = "pipeline 2 started before its producers merged")]
    fn a_forced_early_open_fails_by_name() {
        let p = Arc::new(program(Some(follows(2, 1)), (-1, -1)));
        let mut b = Barrier::new(&p);
        assert_eq!(b.next(), Some(0));
        b.done(0);
        assert_eq!(b.next(), Some(1));
        b.skip_for_test(1);
        b.next();
    }
}

use std::sync::Arc;

use gravity_program::protos::ProtoKind;
use gravity_program::Program;

/// The DAG barrier (§2 decision 4): pipelines run in program order, one in flight, a pipeline
/// only once its producers are done (every pipeline below `next` but the one a test skipped).
pub struct Barrier {
    program: Arc<Program>,
    n: usize,
    in_flight: Option<usize>,
    next: usize,
    skipped: Option<usize>,
    /// Some order edge (`ProtoKind::Follows`) names a pipeline at or after its own: the one way a producer can sit at or
    /// past `next` when its consumer is next (a slot's producer precedes its consumer by `producer_of_slot`'s definition).
    /// Found once here, with no allocation (a run allocates nothing after warm-up); with none, `next`'s release check reads
    /// only a test's skipped pipeline, not a walk of every proto per pipeline.
    forward: bool,
}

impl Barrier {
    pub fn new(program: &Arc<Program>) -> Barrier {
        let forward = program.v4.iter().flat_map(|d| d.protos.iter()).any(|q| q.kind == ProtoKind::Follows && q.peer as usize >= q.pipeline as usize);
        Barrier { program: Arc::clone(program), n: program.pipelines.len(), in_flight: None, next: 0, skipped: None, forward }
    }

    pub fn producers(&self, p: usize) -> impl Iterator<Item = usize> + '_ {
        self.program.producers_of(p)
    }

    /// Pipeline `p`, the next one (`p == next`), has a producer that has not merged: one at or past it, or the one a test
    /// skipped.
    fn waiting(&self, p: usize) -> bool {
        debug_assert_eq!(p, self.next);
        match self.forward {
            true => self.producers(p).any(|q| q >= self.next || Some(q) == self.skipped),
            false => self.skipped.is_some_and(|s| self.producers(p).any(|q| q == s)),
        }
    }

    pub fn next(&mut self) -> Option<usize> {
        assert!(self.in_flight.is_none(), "pipeline {} still in flight", self.in_flight.unwrap());
        if self.next >= self.n {
            return None;
        }
        let p = self.next;
        assert!(!self.waiting(p), "pipeline {p} started before its producers merged");
        self.in_flight = Some(p);
        Some(p)
    }

    pub fn done(&mut self, pipeline: usize) {
        assert_eq!(self.in_flight, Some(pipeline), "done for a pipeline not in flight");
        self.in_flight = None;
        self.next = pipeline + 1;
    }

    /// Tests: leave `pipeline` unfinished and move on, so `next()` can prove the producer check.
    pub fn skip_for_test(&mut self, pipeline: usize) {
        assert_eq!(self.in_flight, Some(pipeline));
        assert!(self.skipped.is_none(), "one skipped pipeline at a time");
        self.skipped = Some(pipeline);
        self.in_flight = None;
        self.next = pipeline + 1;
    }

    pub fn finished(&self) -> bool {
        self.next >= self.n && self.in_flight.is_none()
    }
}

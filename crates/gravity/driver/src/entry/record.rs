//! EXPLAIN ANALYZE's per-pipeline record (docs/gravity/entry.md §4): the arm and wall time the parallel runner already
//! takes for every pipeline, and the claims it ran, kept on the Entry for one program's run while a record of that
//! program is open (the exec entry opens one for a served EXPLAIN ANALYZE only). A run of any other program (a nested
//! statement's) records nothing; a run with no record open pays one field read per pipeline. The served line's `pipes=`
//! and `lead_ms=` (PGRUST_GRAVITY_REPORT; serial census M0) read the same record: one row type, one renderer
//! (types_core::instrument::GravityPipeline).
use gravity_program::{Parallel, Program, Reason};

use super::Entry;

/// One pipeline's run: its arm (`section`, `serial`, `emit`, `empty`, `held`), the section width (0 off a section), its
/// wall time and the claims the pool ran for it; and why the lowering kept it on the leader (serial census M0): `why` =
/// its Ineligible verdict's reason (`""` for an Eligible pipeline, which runs off a section only by the width rule or
/// a nested arm), `site` = the verdict's site (-1: none, a breaker reader's).
/// `at`: when the pipeline started, in ns since the record opened (EXPLAIN ANALYZE's node times are read off it); `buf`:
/// the leader's buffer counters' delta over the pipeline (`Bufs`; zero when the record has no probe).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PipeRun {
    pub pipeline: u16,
    pub arm: &'static str,
    pub width: u16,
    pub ns: u64,
    pub claims: u64,
    pub why: &'static str,
    pub site: i16,
    pub at: u64,
    pub buf: Bufs,
}

/// A participant's buffer counters as EXPLAIN's BUFFERS prints them, in PostgreSQL's BufferUsage order: shared hit,
/// read, dirtied, written; local hit, read, dirtied, written; temp read, written.
pub type Bufs = [i64; 10];

/// The counters' delta `now - then`.
pub fn bufs_delta(now: &Bufs, then: &Bufs) -> Bufs {
    core::array::from_fn(|i| now[i] - then[i])
}

/// A pipeline's lowered verdict as its record row carries it: the Ineligible reason's name and site, `("", -1)` for an
/// Eligible one.
pub fn verdict(p: Parallel) -> (&'static str, i16) {
    let Parallel::Ineligible { site, reason } = p else { return ("", -1) };
    let why = match reason {
        Reason::NoDrivingSite => "NoDrivingSite",
        Reason::IndexSource => "IndexSource",
        Reason::SortedSource => "SortedSource",
        Reason::TempSource => "TempSource",
        Reason::Write => "Write",
        Reason::Lock => "Lock",
        Reason::Host => "Host",
        Reason::Sink => "Sink",
    };
    (why, site)
}

/// The leader-only time of a recorded run (the serial census's upper bound): its wall time less the pool's section
/// time. Sections run one at a time, so the difference is the time no section ran: every serial / emit arm, the gaps
/// between pipelines and the statement's start and end. A section's leader-only owner phase (`own_single`) counts as
/// section time here.
pub fn lead_ns(runs: &[PipeRun], run_ns: u64) -> u64 {
    run_ns.saturating_sub(runs.iter().filter(|r| r.arm == "section").map(|r| r.ns).sum())
}

/// The program recorded (its address and content key), the pipelines it ran, the clock when the record opened and the
/// counter cells of an instrumented program as its run ended (`Program.instr` order; empty for a plain program).
pub struct Record {
    program: usize,
    /// A parked node another admission of the same program built runs its own copy (`park.take` matches the content key):
    /// that run is this program's run too.
    key: gravity_program::ContentKey,
    runs: Vec<PipeRun>,
    t0: u64,
    cells: Vec<u64>,
    /// Reads the leader's buffer counters (the exec entry's, which owns pgBufferUsage's reader); `None`: none kept.
    probe: Option<fn() -> Bufs>,
}

/// The record `open` replaced (a nested EXPLAIN ANALYZE's open inside a recorded run), put back by `close`.
pub struct Token(Option<Box<Record>>);

fn addr(program: &Program) -> usize {
    program as *const Program as usize
}

/// The monotonic clock an EXPLAIN ANALYZE's times are read from (pg_clock's).
pub fn now_ns() -> u64 {
    pg_clock::mono_ns()
}

impl Entry {
    /// Record `program`'s pipelines until `record_close` (call it on every exit, an error's included).
    pub fn record_open(&mut self, program: &Program) -> Token {
        self.record_open_probed(program, None)
    }

    /// `record_open` keeping each pipeline's buffer counters' delta through `probe` (an EXPLAIN ANALYZE's record).
    pub fn record_open_probed(&mut self, program: &Program, probe: Option<fn() -> Bufs>) -> Token {
        Token(self.record.replace(Box::new(Record { program: addr(program), key: program.key, runs: Vec::new(), t0: now_ns(), cells: Vec::new(), probe }))) // alloc: Exec
    }

    /// A pipeline's start as the record keeps it: ns since the record opened and the buffer counters then (zero without a
    /// probe); `None` without a record.
    pub fn record_mark(&self) -> Option<(u64, Bufs)> {
        let r = self.record.as_ref()?;
        Some((now_ns().saturating_sub(r.t0), r.probe.map_or([0; 10], |p| p())))
    }

    /// The buffer counters' delta since `then` (a `record_mark`'s), zero without a probe.
    pub fn record_bufs_since(&self, then: &Bufs) -> Bufs {
        match self.record.as_ref().and_then(|r| r.probe) {
            Some(p) => bufs_delta(&p(), then),
            None => [0; 10],
        }
    }

    /// The pipelines recorded since the `record_open` that gave `token`, in the order they ran, the wall time since it and
    /// the counter cells the run ended with; the record before it back.
    pub fn record_close(&mut self, token: Token) -> (Vec<PipeRun>, u64, Vec<u64>) {
        let r = core::mem::replace(&mut self.record, token.0);
        r.map_or_else(|| (Vec::new(), 0, Vec::new()), |r| (r.runs, now_ns().saturating_sub(r.t0), r.cells)) // alloc: Exec
    }

    /// The recorded program's counter cells as its run ends (the node's, every section's slots folded in).
    pub fn record_cells(&mut self, cells: &[u64]) {
        if let Some(r) = self.record.as_mut() {
            r.cells = cells.to_vec(); // alloc: Exec
        }
    }

    /// Whether `program`'s run is being recorded.
    pub fn recording(&self, program: &Program) -> bool {
        self.record.as_ref().is_some_and(|r| r.program == addr(program) || r.key == program.key)
    }

    pub fn record_push(&mut self, run: PipeRun) {
        if let Some(r) = self.record.as_mut() {
            r.runs.push(run); // alloc: Exec
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(arm: &'static str, ns: u64) -> PipeRun {
        PipeRun { pipeline: 0, arm, width: 0, ns, claims: 0, why: "", site: -1, at: 0, buf: [0; 10] }
    }

    #[test]
    fn lead_is_the_wall_less_the_sections() {
        let runs = [run("section", 70), run("emit", 10), run("section", 5), run("serial", 3)];
        assert_eq!(lead_ns(&runs, 100), 25);
        assert_eq!(lead_ns(&runs, 60), 0);
        assert_eq!(lead_ns(&[], 9), 9);
    }

    #[test]
    fn verdict_names_the_reason_and_site() {
        assert_eq!(verdict(Parallel::Eligible), ("", -1));
        assert_eq!(verdict(Parallel::Ineligible { site: 3, reason: Reason::Host }), ("Host", 3));
        assert_eq!(verdict(Parallel::Ineligible { site: -1, reason: Reason::NoDrivingSite }), ("NoDrivingSite", -1));
    }
}

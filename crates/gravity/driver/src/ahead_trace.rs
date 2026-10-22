//! Construct C2 (b), the run-ahead tracer (reports BUG-RECURRENCE-20261011 §3 C2): a pipeline whose source is a plan node
//! PostgreSQL 18.6 may never run (program `Pipeline::pg_gate`, lowerv4's stamp from decompose `pg_demand::never_run`) and
//! whose first run is not one PostgreSQL's demand asked for (the runner's program order, or an eager Init's members run
//! at its reader's start) runs ahead of PostgreSQL's executor: what it raises or does PostgreSQL may never raise or do. Such
//! a run must hold a licence (`Pipeline::recheck`: an errorhold recheck; `Pipeline::ahead`: a tagged unchecked licence;
//! `Pipeline::licence`: the kind of any licence the lowering held for it, lowerv4 `Px::hold`), or its code must be
//! unobservable (`Pipeline::quiet`). An error held for PostgreSQL's point (op 14's `held_sort` / `held_at` /
//! `recheck_hold`) is no licence by itself: such a run is licensed only by `effects::held_error`'s licence held for it
//! (`LicenceStamp::Held`); without it the run is Unlicensed, its hit line saying it held its error (`held error, no
//! HeldError licence`). A post
//! pipeline (ExecPostprocessPlan's) and a plan-order program (lowered inside `effects::deny_all`: no stamp) are never
//! judged.
//! D1: a run under an exact licence (`LicenceStamp::Exact`: a structural proof that PostgreSQL 18.6 runs the same code to
//! the same first error) is counted apart (AHEAD-EXACT, a report line naming the proof and the node it cites), never as
//! licensed, so the count of proofs nothing checks at run time stays visible; a proof whose lowering check failed is a hit.
//! The accepted class (`LicenceStamp::Accepted`: decompose `effects::accepted`, early work whose only departure is an
//! error PostgreSQL never hits, and `effects::volatile_free`, extra VOLATILE calls, which are free) is counted apart
//! too (AHEAD-ACCEPTED, a report line), never as a key nor as licensed.
//!
//! The origin of a run is this thread's: the runner's frame (`Entry::with_current_slot`) runs in `Order`; `build_wait`,
//! `run_nested`, `pull_nested`, `rerun_nested` and the EvalPlanQual rerun scope theirs (PostgreSQL's demand: a probe's
//! first row, a param's first read, a CTE reader's pull, a rescan, a recheck); `run_ahead` scopes `Ahead`. D4191: such a
//! demand runs one gate region (decompose `pg_demand::regions`, program `Pipeline::pg_regions`): the region of the
//! pipeline that asked (the lazy build, the Init's last member, the rerun set's last member, the pulled pipeline) and
//! the regions around it (`demand`). A run it makes of a pipeline whose innermost region is none of those runs code
//! PostgreSQL's demand does not reach yet (an outer-first Hash inside a pulled CTE's plan, D4189): it is judged as an
//! Order run is. EvalPlanQual's rerun is PostgreSQL's own and never judged. The verdict is
//! taken at the pipeline's first open on the leader (its first claim, its section's end), not at its first row: a run
//! that stops before reading (a probe over an empty build) is judged as run. A Material's pull-form fill, whose function
//! leaves at its open (its one reader makes each row at its pull), carries no stamp (lowerv4 `gate_stamp`).
//!
//! The whole-read licence (decompose `effects::whole_read`) rests on a guard the tracer checks at run time, not trusts: a
//! program it covers is served only to a reader that takes every row (program `Pipeline::whole_read`, lowerv4
//! `Px::whole_read`; exec's entry delegates the others), so a run of such a program that its reader ends before it
//! completes broke that guard (`whole_read_cut`, asked at exec admit/entry `executor_end`).
//!
//! R3: a recheck licence rests on the plan-order re-run, which repairs an error only while the statement has sent no row
//! (exec `runner::recheck`). The tracer reads it from the stamp (`LicenceStamp::Recheck`, or `Rests`: code under a
//! recheck licence asked for other code, which the verdict above takes for no licence), not from `Pipeline::recheck`,
//! which the root pipeline also carries for the statement as a whole. A first run of such a pipeline after a row of its
//! statement left (`sent`, scoped per statement by `statement`) raises an error nothing repairs, whatever its origin. A
//! recheck licence is granted only to code that does not act (decompose `effects::licence`; the statement's recheck
//! licence a `Rests` run sits under is asked with the effects of the batch it re-runs), so that error is the
//! accepted class (an error PostgreSQL never hits, from early or extra work): such a run is counted apart
//! (`late`, the report line `accepted: .. after the statement sent a row`), never a hit. A licence that holds its error
//! for PostgreSQL's point, proves the same first error, runs only for a whole reader or tags its error rests on no
//! re-run; a tagged run is safe only for a data error, and which classes a tagged part may raise is effects' row (B), not
//! read here.
//!
//! `PGRUST_GRAVITY_AHEAD_TRACE`: `off` (the default until AHEAD-DEBT is 0), `report` (a server log line per hit),
//! `abort` (a panic: tests and rigs). `report` and `abort` also turn lowerv4's gate stamps on (exec knobs/diag.rs
//! `ahead_trace`, read once where the lowerer is registered): off, a lowering stamps no gate and pays one load for it.
use core::cell::Cell;
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use gravity_program::Pipeline;

/// `PGRUST_GRAVITY_AHEAD_TRACE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    Report,
    Abort,
}

/// Who asked for a pipeline's run (module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// The runner's program order: no row asked.
    Order,
    /// An eager Init's members at its reader's start (`Entry::run_ahead`).
    Ahead,
    /// A lazy build at its probe's first row (`Entry::build_wait`).
    BuildWait,
    /// An Init's members at its param's first read (`Entry::run_nested`).
    Nested,
    /// A rerun set again (`Entry::rerun_nested`: a rescan).
    Rerun,
    /// A pulled set at its reader's pull (`Entry::pull_nested`).
    Pull,
    /// EvalPlanQual's rerun (`Entry::rerun`).
    Recheck,
}

/// The tracer's answer for a first run (`verdict`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// PostgreSQL's demand asked for it, or PostgreSQL runs its source whenever its consumer runs.
    Demanded,
    /// Ahead, under a licence.
    Licensed,
    /// Ahead, under an exact licence (D1: `LicenceStamp::Exact`): a structural proof says PostgreSQL 18.6 runs the same
    /// code to the same first error. Counted apart (AHEAD-EXACT), never as licensed; a failed check is a hit.
    Exact { failed: bool },
    /// Ahead, under the accepted class (`LicenceStamp::Accepted`): counted apart (AHEAD-ACCEPTED), never a key.
    Accepted,
    /// Ahead, nothing in it may raise or act.
    Quiet,
    /// Ahead, with no licence (an error it holds for PostgreSQL's point is none: module doc).
    Unlicensed,
}

std::thread_local! {
    /// This thread's origin of a run and the gate regions its demand opened, innermost first (module doc).
    static ORIGIN: Cell<(Origin, [i32; 4])> = const { Cell::new((Origin::Order, NONE)) };
    /// R3: whether a row of this thread's running statement left for its receiver (module doc).
    static SENT: Cell<bool> = const { Cell::new(false) };
}

static HITS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

/// No gate region (`Pipeline::pg_regions` of a pipeline PostgreSQL runs whenever its statement runs).
const NONE: [i32; 4] = [-1; 4];

/// The mode, read from the environment at the first call (a pure read: two threads racing it store the same value).
#[inline]
pub fn mode() -> Mode {
    static MODE: AtomicU8 = AtomicU8::new(0);
    let m = match MODE.load(Ordering::Relaxed) {
        0 => {
            let m = match crate::env::text("PGRUST_GRAVITY_AHEAD_TRACE").as_deref() {
                Some("report") => 2,
                Some("abort") => 3,
                _ => 1,
            };
            MODE.store(m, Ordering::Relaxed);
            m
        }
        m => m,
    };
    match m {
        2 => Mode::Report,
        3 => Mode::Abort,
        _ => Mode::Off,
    }
}

/// Hits since the process started: [Unlicensed runs holding their error for PostgreSQL's point, the other Unlicensed
/// runs, whole-read programs cut short (`whole_read_cut`), exact runs (D1, not hits), exact runs whose proof's check
/// failed, recheck-licensed runs after a row was sent (R3: the accepted class, not hits), accepted runs (not hits)].
pub fn hits() -> [u64; 7] {
    core::array::from_fn(|i| HITS[i].load(Ordering::Relaxed))
}

/// `scope`'s guard: the thread's origin before it, put back on drop (`None`: the tracer is off).
#[must_use]
pub struct Scope(Option<(Origin, [i32; 4])>);

impl Drop for Scope {
    fn drop(&mut self) {
        if let Some(o) = self.0 {
            ORIGIN.with(|c| c.set(o));
        }
    }
}

/// The runs inside the guard's life have origin `o` (a demand that opens no gate region: `demand` for one that does).
#[inline]
pub fn scope(o: Origin) -> Scope {
    if mode() == Mode::Off {
        return Scope(None);
    }
    Scope(Some(ORIGIN.with(|c| c.replace((o, NONE)))))
}

/// D4191: the runs inside the guard's life have origin `o`, a demand pipeline `asker` of `program` made: it opened the
/// gate regions holding that pipeline's source (`Pipeline::pg_regions`; module doc).
#[inline]
pub fn demand(o: Origin, program: &gravity_program::Program, asker: usize) -> Scope {
    if mode() == Mode::Off {
        return Scope(None);
    }
    let opened = program.pipelines.get(asker).map_or(NONE, |x| x.pg_regions);
    Scope(Some(ORIGIN.with(|c| c.replace((o, opened)))))
}

/// `statement`'s guard: the flag of the statement around it, put back on drop (`None`: the tracer is off).
#[must_use]
pub struct Statement(Option<bool>);

impl Drop for Statement {
    fn drop(&mut self) {
        if let Some(s) = self.0 {
            SENT.with(|c| c.set(s));
        }
    }
}

/// R3: a statement's execution runs inside the guard; `sent`: a row of it already left (a portal's earlier fetch). A
/// statement nested in it (SPI) keeps its own flag.
#[inline]
pub fn statement(sent: bool) -> Statement {
    if mode() == Mode::Off {
        return Statement(None);
    }
    Statement(Some(SENT.with(|c| c.replace(sent))))
}

/// R3: a row of the running statement leaves for its receiver.
#[inline]
pub fn sent() {
    if mode() != Mode::Off {
        SENT.with(|c| c.set(true));
    }
}

/// R3: whether a run of pipeline `x` rests on the plan-order re-run (module doc): its stamp is a recheck licence's.
pub fn rests_on_rerun(x: &Pipeline) -> bool {
    matches!(x.licence, gravity_program::LicenceStamp::Recheck | gravity_program::LicenceStamp::Rests)
}

/// R3: whether a first run of pipeline `x` after its statement sent a row (`sent`) rests on a re-run that can no longer
/// repair its error (`late`: the accepted class, counted apart).
pub fn late_run(x: &Pipeline, sent: bool) -> bool {
    sent && rests_on_rerun(x)
}

/// The verdict on pipeline `x`'s first run from `origin`, whose demand opened the gate regions `opened` (module doc).
pub fn verdict(x: &Pipeline, origin: Origin, opened: &[i32; 4]) -> Verdict {
    if x.pg_gate < 0 || x.post || !judged(x, origin, opened) {
        Verdict::Demanded
    } else if x.recheck || x.ahead.is_some() {
        Verdict::Licensed
    } else if let gravity_program::LicenceStamp::Exact { check, .. } = x.licence {
        Verdict::Exact { failed: check == gravity_program::ProofCheck::Failed }
    } else if x.licence == gravity_program::LicenceStamp::Accepted {
        Verdict::Accepted
    } else if !matches!(x.licence, gravity_program::LicenceStamp::None | gravity_program::LicenceStamp::Rests) {
        Verdict::Licensed
    } else if x.quiet {
        Verdict::Quiet
    } else {
        Verdict::Unlicensed
    }
}

/// D4191: whether the tracer judges a run of gated pipeline `x` from `origin`: a run in program order (`Order`, an eager
/// Init's `Ahead`), or one a demand made (a build wait, an Init's first read, a rescan, a pull) of a pipeline whose
/// innermost gate region is none the demand opened (`opened`); never EvalPlanQual's rerun.
fn judged(x: &Pipeline, origin: Origin, opened: &[i32; 4]) -> bool {
    match origin {
        Origin::Order | Origin::Ahead => true,
        Origin::BuildWait | Origin::Nested | Origin::Rerun | Origin::Pull => x.pg_regions[0] >= 0 && !opened.contains(&x.pg_regions[0]),
        Origin::Recheck => false,
    }
}

/// Pipeline `p` of `program` runs for the first time (its open on the leader): judged from this thread's origin.
#[inline]
pub fn first_run(program: &gravity_program::Program, p: usize) {
    if mode() == Mode::Off {
        return;
    }
    let Some(x) = program.pipelines.get(p) else { return };
    let (origin, opened) = ORIGIN.with(Cell::get);
    match verdict(x, origin, &opened) {
        Verdict::Unlicensed => hit(p, x, (origin, opened), x.held_sort || x.held_at || x.recheck_hold),
        Verdict::Exact { failed } => exact(p, x, origin, failed),
        Verdict::Accepted => accepted(p, x, origin),
        _ => {}
    }
    if late_run(x, SENT.with(Cell::get)) {
        late(p, x, origin);
    }
}

/// R3: recheck-licensed pipeline `p` ran first after its statement sent a row (module doc): the accepted class, counted
/// apart (no hit, no abort).
#[cold]
fn late(p: usize, x: &Pipeline, origin: Origin) {
    HITS[5].fetch_add(1, Ordering::Relaxed);
    eprintln!("{}", late_line(p, x, origin));
}

/// `late`'s report line: an accepted run's words first (the harness counts it apart, as `accepted`'s), then why.
fn late_line(p: usize, x: &Pipeline, origin: Origin) -> String {
    let node = x.pg_gate;
    format!("gravity: ahead-trace: accepted: pipeline {p} (plan node id {node}) ran first in {origin:?} after the statement sent a row (Recheck-after-send: an error past the plan-order re-run, {:?}){}", x.licence, Kinds(x)) // alloc: Cold
}

/// D1: an exact run (AHEAD-EXACT): its report line names the proof, the node it cites and whether it was checked at run
/// time (`checked`) or rests on its constructor's static evidence alone (`static-only`); one whose check failed is a hit
/// (`exact proof failed`).
#[cold]
fn exact(p: usize, x: &Pipeline, origin: Origin, failed: bool) {
    let gravity_program::LicenceStamp::Exact { proof, node, check } = x.licence else { return };
    HITS[if failed { 4 } else { 3 }].fetch_add(1, Ordering::Relaxed);
    let at = x.pg_gate;
    if failed {
        eprintln!("gravity: ahead-trace: exact proof failed: {proof:?} (cites node {node}): pipeline {p} (plan node id {at}) ran first in {origin:?}{}", Kinds(x));
        if mode() == Mode::Abort {
            panic!("gravity: ahead-trace: exact proof failed: {proof:?} (cites node {node}): pipeline {p} (plan node id {at})");
        }
    } else {
        // a proof nothing checks at run time (FirstFetch's timing: the runner cannot tell a node's first fetch from the
        // run's start) is reported static-only, so no reader takes it as run-time proven
        let how = if check == gravity_program::ProofCheck::Checked { "checked" } else { "static-only" };
        eprintln!("gravity: ahead-trace: exact {proof:?} (cites node {node}, {how}): pipeline {p} (plan node id {at}) ran first in {origin:?}{}", Kinds(x));
    }
}

/// The accepted class (AHEAD-ACCEPTED): a report line naming the run, counted apart, never a hit (no panic under
/// `abort`).
#[cold]
fn accepted(p: usize, x: &Pipeline, origin: Origin) {
    HITS[6].fetch_add(1, Ordering::Relaxed);
    eprintln!("gravity: ahead-trace: accepted: pipeline {p} (plan node id {}) ran first in {origin:?}{}", x.pg_gate, Kinds(x));
}

#[cold]
fn hit(p: usize, x: &Pipeline, (origin, opened): (Origin, [i32; 4]), held: bool) {
    HITS[usize::from(!held)].fetch_add(1, Ordering::Relaxed);
    let (how, node, tags) = (if held { " (held error, no HeldError licence)" } else { "" }, x.pg_gate, Kinds(x));
    // D4191: a run a demand made names its region and the demand's (the words the harness keys on come first)
    let at = if matches!(origin, Origin::Order | Origin::Ahead) { String::new() } else { format!(" (region {}, the demand's {})", x.pg_regions[0], opened[0]) }; // alloc: Cold
    eprintln!("gravity: ahead-trace: Unlicensed{how}: pipeline {p} (plan node id {node}) ran first in {origin:?}{at}, before PostgreSQL 18.6 asks for that node{tags}");
    if mode() == Mode::Abort {
        panic!("gravity: ahead-trace: Unlicensed{how}: pipeline {p} (plan node id {node}) ran first in {origin:?}");
    }
}

/// A hit line's last words (`Pipeline::gate_tags`, a diagnosis): ` [<the gated node's kind> under <its parent's>]`, after
/// the words the harness keys on, so a reader of the older line keys it alike; nothing when the lowering stamped none.
struct Kinds<'a>(&'a Pipeline);

impl core::fmt::Display for Kinds<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0.gate_tags {
            Some((n, up)) => write!(f, " [{n} under {up}]"),
            None => Ok(()),
        }
    }
}

/// Whether a served run broke the whole-read guard at its end: its program is served only to a reader that takes every
/// row (`whole`), and the run was opened (`open`) but did not complete (`done`): its reader ended it early.
pub const fn cut_short(whole: bool, open: bool, done: bool) -> bool {
    whole && open && !done
}

/// A whole-read program's run ended before it completed (`cut_short`, module doc): the reader that ended it read part
/// of what the licensed code ran ahead for.
#[cold]
pub fn whole_read_cut(shape: &str) {
    if mode() == Mode::Off {
        return;
    }
    HITS[2].fetch_add(1, Ordering::Relaxed);
    eprintln!("gravity: ahead-trace: whole-read guard broken: a program served only to a reader that takes every row ({shape}) ended before its run completed");
    if mode() == Mode::Abort {
        panic!("gravity: ahead-trace: whole-read guard broken ({shape})");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pipeline whose source (node 3) sits in gate region 5, itself inside region 2.
    fn gated(f: impl FnOnce(&mut Pipeline)) -> Pipeline {
        let mut x = Pipeline::serial(-1, gravity_program::End::Sink, gravity_program::Reason::NoDrivingSite);
        x.pg_gate = 3;
        x.pg_regions = [5, 2, -1, -1];
        f(&mut x);
        x
    }

    /// `verdict` of a run in program order.
    fn ordered(x: &Pipeline, origin: Origin) -> Verdict {
        verdict(x, origin, &NONE)
    }

    #[test]
    fn a_gated_run_in_order_needs_a_licence() {
        assert_eq!(ordered(&gated(|_| ()), Origin::Order), Verdict::Unlicensed);
        assert_eq!(ordered(&gated(|_| ()), Origin::Ahead), Verdict::Unlicensed);
        assert_eq!(ordered(&gated(|x| x.recheck = true), Origin::Order), Verdict::Licensed);
        let tag = gravity_program::AheadTag { node: 2, node_type: "Hash" };
        assert_eq!(ordered(&gated(|x| x.ahead = Some(tag)), Origin::Ahead), Verdict::Licensed);
        assert_eq!(ordered(&gated(|x| x.quiet = true), Origin::Order), Verdict::Quiet);
        assert_eq!(ordered(&gated(|x| x.held_sort = true), Origin::Order), Verdict::Unlicensed, "a held error is no licence by itself");
        let held = |x: &mut Pipeline| {
            x.held_sort = true;
            x.licence = gravity_program::LicenceStamp::Held;
        };
        assert_eq!(ordered(&gated(held), Origin::Order), Verdict::Licensed, "the held-error licence the lowering held");
        assert_eq!(ordered(&gated(|x| x.licence = gravity_program::LicenceStamp::Plain), Origin::Ahead), Verdict::Licensed);
    }

    /// D1: an exact licence is its own bucket, never licensed; its failed check is a hit; another licence wins.
    #[test]
    fn an_exact_run_is_counted_apart() {
        use gravity_program::{LicenceStamp, ProofCheck, ProofStamp};
        let ex = |check: ProofCheck| move |x: &mut Pipeline| x.licence = LicenceStamp::Exact { proof: ProofStamp::FirstFetch, node: 4, check };
        assert_eq!(ordered(&gated(ex(ProofCheck::Unchecked)), Origin::Order), Verdict::Exact { failed: false });
        assert_eq!(ordered(&gated(ex(ProofCheck::Checked)), Origin::Ahead), Verdict::Exact { failed: false });
        assert_eq!(ordered(&gated(ex(ProofCheck::Failed)), Origin::Order), Verdict::Exact { failed: true });
        let both = |x: &mut Pipeline| {
            ex(ProofCheck::Failed)(x);
            x.recheck = true;
        };
        assert_eq!(ordered(&gated(both), Origin::Order), Verdict::Licensed, "a recheck licence covers the run whatever the proof");
        assert_eq!(verdict(&gated(ex(ProofCheck::Failed)), Origin::Pull, &[5, 2, -1, -1]), Verdict::Demanded);
    }

    /// The accepted class is its own bucket, never licensed nor a key; a recheck or tag on the pipeline wins, and a
    /// demanded run is Demanded.
    #[test]
    fn an_accepted_run_is_counted_apart() {
        use gravity_program::LicenceStamp;
        let acc = |x: &mut Pipeline| x.licence = LicenceStamp::Accepted;
        assert_eq!(ordered(&gated(acc), Origin::Order), Verdict::Accepted);
        assert_eq!(ordered(&gated(acc), Origin::Ahead), Verdict::Accepted);
        assert_eq!(verdict(&gated(acc), Origin::Pull, &[2, -1, -1, -1]), Verdict::Accepted, "a demand's run beyond its region");
        assert_eq!(verdict(&gated(acc), Origin::Pull, &[5, 2, -1, -1]), Verdict::Demanded);
        let both = |x: &mut Pipeline| {
            acc(x);
            x.recheck = true;
        };
        assert_eq!(ordered(&gated(both), Origin::Order), Verdict::Licensed);
        let quiet = |x: &mut Pipeline| {
            acc(x);
            x.quiet = true;
        };
        assert_eq!(ordered(&gated(quiet), Origin::Order), Verdict::Accepted, "the licence held is read before quiet");
        assert!(!rests_on_rerun(&gated(acc)), "it rests on no re-run");
    }

    /// The hit line's diagnosis words come last, after the words the harness keys on.
    #[test]
    fn the_gated_kinds_close_the_line() {
        assert_eq!(Kinds(&gated(|x| x.gate_tags = Some(("Hash", "HashJoin")))).to_string(), " [Hash under HashJoin]"); // alloc: Cold
        assert_eq!(Kinds(&gated(|_| ())).to_string(), ""); // alloc: Cold
    }

    /// A demand runs its own gate region and the regions around it (D4191).
    #[test]
    fn a_demanded_or_ungated_run_passes() {
        for o in [Origin::BuildWait, Origin::Nested, Origin::Rerun, Origin::Pull] {
            assert_eq!(verdict(&gated(|_| ()), o, &[5, 2, -1, -1]), Verdict::Demanded, "{o:?}: the demand's own region");
            assert_eq!(verdict(&gated(|_| ()), o, &[9, 5, 2, -1]), Verdict::Demanded, "{o:?}: a region around the demand's");
        }
        assert_eq!(ordered(&gated(|_| ()), Origin::Recheck), Verdict::Demanded, "EvalPlanQual's rerun is PostgreSQL's");
        assert_eq!(ordered(&Pipeline::serial(-1, gravity_program::End::Sink, gravity_program::Reason::NoDrivingSite), Origin::Order), Verdict::Demanded);
        assert_eq!(ordered(&gated(|x| x.post = true), Origin::Order), Verdict::Demanded);
    }

    /// D4191 (D4189: a hashed aggregate under an outer-first Hash inside a pulled CTE's plan, run at the CTE's first
    /// pull): a demand's run of a pipeline in a region nested in the demand's, or outside it, is judged as an Order run.
    #[test]
    fn a_demand_run_beyond_its_region_is_judged() {
        for o in [Origin::BuildWait, Origin::Nested, Origin::Rerun, Origin::Pull] {
            assert_eq!(verdict(&gated(|_| ()), o, &[2, -1, -1, -1]), Verdict::Unlicensed, "{o:?}: region 5 inside the demand's 2");
            assert_eq!(verdict(&gated(|_| ()), o, &NONE), Verdict::Unlicensed, "{o:?}: a demand that opened no region");
            assert_eq!(verdict(&gated(|_| ()), o, &[8, -1, -1, -1]), Verdict::Unlicensed, "{o:?}: another subplan's region");
            assert_eq!(verdict(&gated(|x| x.quiet = true), o, &[2, -1, -1, -1]), Verdict::Quiet, "{o:?}");
            assert_eq!(verdict(&gated(|x| x.licence = gravity_program::LicenceStamp::Plain), o, &[2, -1, -1, -1]), Verdict::Licensed, "{o:?}");
        }
    }

    /// R3: a recheck licence's stamp rests on the plan-order re-run; a licence that holds, proves, reads whole or tags
    /// rests on none, nor does the root pipeline's mark for the statement.
    #[test]
    fn a_recheck_licence_rests_on_the_rerun() {
        use gravity_program::{LicenceStamp, ProofCheck, ProofStamp};
        assert!(rests_on_rerun(&gated(|x| x.licence = LicenceStamp::Recheck)), "a feeder held under the recheck licence");
        let mut ungated = Pipeline::serial(-1, gravity_program::End::Sink, gravity_program::Reason::NoDrivingSite);
        ungated.licence = LicenceStamp::Rests;
        assert!(late_run(&ungated, true), "ungated too: the re-run repairs any departure only before a row left");
        assert!(!late_run(&ungated, false), "before the first row the re-run repairs it");
        assert_eq!(ordered(&gated(|x| x.licence = LicenceStamp::Rests), Origin::Order), Verdict::Unlicensed, "no licence held for this run");
        assert!(!rests_on_rerun(&gated(|x| x.recheck = true)), "the root pipeline's mark for the statement");
        assert!(!rests_on_rerun(&gated(|_| ())));
        assert!(!rests_on_rerun(&gated(|x| x.licence = LicenceStamp::Plain)));
        for l in [LicenceStamp::Held, LicenceStamp::Whole, LicenceStamp::Tagged, LicenceStamp::Accepted, LicenceStamp::Exact { proof: ProofStamp::PgFailsToo, node: 4, check: ProofCheck::Unchecked }] {
            assert!(!rests_on_rerun(&gated(|x| {
                x.recheck = true;
                x.licence = l;
            })), "{l:?}");
        }
    }

    /// R3, the accepted class: a late run's line is an accepted run's (the words the harness counts apart come first),
    /// with why after them.
    #[test]
    fn a_late_run_is_accepted() {
        let mut x = gated(|x| x.licence = gravity_program::LicenceStamp::Recheck);
        x.gate_tags = Some(("Agg", "CteScan"));
        let l = late_line(5, &x, Origin::Pull);
        assert!(l.starts_with("gravity: ahead-trace: accepted: pipeline 5 (plan node id 3) ran first in Pull after the statement sent a row"), "{l}");
        assert!(l.ends_with(" [Agg under CteScan]"), "{l}");
        assert!(!l.contains("Unlicensed") && !l.starts_with("gravity: ahead-trace: Recheck-after-send"), "{l}");
    }

    /// R3: `sent` marks the running statement only; a nested statement keeps its own flag and puts the caller's back.
    #[test]
    fn a_statement_scopes_its_sent_flag() {
        let flag = || SENT.with(Cell::get);
        SENT.with(|c| c.set(false));
        let at = |sent: bool| Statement(Some(SENT.with(|c| c.replace(sent))));
        {
            let _outer = at(false);
            assert!(!flag());
            SENT.with(|c| c.set(true));
            {
                let _spi = at(false);
                assert!(!flag(), "a nested statement starts with none sent");
            }
            assert!(flag(), "the caller's sent row is back");
            {
                let _fetch = at(true);
                assert!(flag(), "a portal's later fetch starts sent");
            }
        }
        assert!(!flag());
    }

    #[test]
    fn a_whole_read_run_ended_early_breaks_the_guard() {
        assert!(cut_short(true, true, false), "opened, not completed: the reader stopped early");
        assert!(!cut_short(true, true, true), "a soft run stays open after it completed");
        assert!(!cut_short(true, false, false), "never run: nothing ran ahead");
        assert!(!cut_short(false, true, false), "a program any reader may stop");
    }
}

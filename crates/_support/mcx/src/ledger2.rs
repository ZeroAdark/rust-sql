//! LEDGER2 step 4: the owner tree's reading of a running statement beside today's numbers. What a cancel of a statement
//! frees is, by the tree, its statement owner's subtree and the subtree of the portal it executes (memledger `usage`:
//! its memory contexts, its operators' arenas, its regions). Two uses:
//! - R6's rank under `pgrust.memory_ledger_v2` (default off): max(the tree's bytes, today's rank), the transition rule
//!   until the unattributed row reaches zero (governor `ranks_of`).
//! - Shadow lines, whatever the flag: at R6's pick, the fair-share cancel, R4a (the asker's own spill) and R4b (revoke),
//!   what the tree would choose next to today's choice: one rate-limited `ledger2-shadow:` line on stderr (the server log)
//!   per decision, only when they differ, and per-decision counts (`counts`). No allocation.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

static ON: AtomicBool = AtomicBool::new(false);

/// `pgrust.memory_ledger_v2` as memruntime publishes it each cadence.
pub fn set_on(on: bool) {
    ON.store(on, Relaxed);
}

#[inline]
pub fn on() -> bool {
    ON.load(Relaxed)
}

/// The bytes the owner tree says a cancel of the statement running on backend `procno` frees: its statement owner's
/// subtree and its active portal's, in memory (design A: each subtree's held less what the residency pass found not
/// resident; 0: none running, or no ledger in this build). One scan of the operator cells per owner while any is keyed
/// (memledger `usage`).
pub fn owned(procno: Option<usize>) -> u64 {
    #[cfg(feature = "std")]
    {
        owned_now(procno)
    }
    #[cfg(not(feature = "std"))]
    {
        let _ = procno;
        0
    }
}

/// `(scans, total ns, max ns)` of the owner tree's operator-cell scans since start (memledger `scan_cost`; Q2: the
/// scan's cost at R6's pick, over `memledger::charge::OP_CELLS` cells while any operator is keyed); zeros without a ledger.
pub fn scan_cost() -> (u64, u64, u64) {
    #[cfg(feature = "std")]
    {
        memledger::charge::scan_cost()
    }
    #[cfg(not(feature = "std"))]
    {
        (0, 0, 0)
    }
}

#[cfg(feature = "std")]
fn owned_now(procno: Option<usize>) -> u64 {
    let Some(p) = procno.and_then(|p| u32::try_from(p).ok()) else { return 0 };
    let a = memledger::Acct::backend(p);
    if !a.is_backend() {
        return 0;
    }
    let (stmt, portal) = memledger::charge::running_owners(a);
    let mut t = 0i64;
    if stmt != 0 {
        t = t.saturating_add(memledger::charge::usage(memledger::Owner::Statement { acct: a, stmt }).subtree_resident());
    }
    if portal != 0 {
        t = t.saturating_add(memledger::charge::usage(memledger::Owner::Portal { acct: a, portal }).subtree_resident());
    }
    t.max(0) as u64
}

/// The decisions the shadow lines cover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// R6: the statement the cancel pick takes (governor `pick_victim`).
    Cancel = 0,
    /// The fair-share (projected) cancel's statement (governor `cancel_fair_victim`).
    FairCancel = 1,
    /// R4a: whether the asker spills its own (its bytes over its fair share).
    OwnSpill = 2,
    /// R4b: the first statement revoke asks (`revoke::request_revoke`).
    Revoke = 3,
}

pub const DECISIONS: usize = 4;
pub const DECISION_NAMES: [&str; DECISIONS] = ["R6 cancel pick", "fair-share cancel", "R4a own spill", "R4b revoke"];

/// At most one line per decision this often (ms).
pub const LINE_EVERY_MS: u64 = 1_000;

struct Rec {
    runs: AtomicU64,
    differ: AtomicU64,
    /// Decisions not compared (`skip`): R4a's asks from an asker with no spillable bytes, where R4a cannot run.
    skipped: AtomicU64,
    last_ms: AtomicU64,
}

static RECS: [Rec; DECISIONS] = [const { Rec { runs: AtomicU64::new(0), differ: AtomicU64::new(0), skipped: AtomicU64::new(0), last_ms: AtomicU64::new(0) } }; DECISIONS];

/// Per decision since the server started: how many ran, how many the owner tree would have decided otherwise, how many
/// were not compared (`skip`).
pub fn counts() -> [(u64, u64, u64); DECISIONS] {
    core::array::from_fn(|i| (RECS[i].runs.load(Relaxed), RECS[i].differ.load(Relaxed), RECS[i].skipped.load(Relaxed)))
}

/// Decision `d` was asked where it cannot run (R4a: an asker with no spillable bytes, e.g. at its admission, before it
/// drew anything): not compared, counted, so the summary line (`summary_line`) shows what the filter left out.
pub fn skip(d: Decision) {
    RECS[d as usize].skipped.fetch_add(1, Relaxed);
}

/// The shadow's summary since start, one line (memruntime logs it at each ladder episode's close): per decision its runs,
/// how many differ and how many were skipped, and the operator-cell scans' cost. `out` gets the text; its length returned.
pub fn summary_line(out: &mut [u8]) -> usize {
    use core::fmt::Write as _;
    let mut e = Buf { b: out, n: 0 };
    let _ = write!(e, "ledger2-shadow summary since start:");
    for (i, (runs, differ, skipped)) in counts().iter().enumerate() {
        let _ = write!(e, " {} {runs} run, {differ} differ, {skipped} skipped;", DECISION_NAMES[i]);
    }
    let (scans, ns, max) = scan_cost();
    let _ = write!(e, " operator-cell scans {scans}, mean {} us, max {} us", ns / scans.max(1) / 1_000, max / 1_000);
    e.n
}

struct Buf<'a> {
    b: &'a mut [u8],
    n: usize,
}

impl core::fmt::Write for Buf<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let k = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
        self.n += k;
        Ok(())
    }
}

/// One choice: a backend (its procno; `None`: no statement chosen) and the bytes it was ranked by. For R4a the backend
/// is the asker's when it spills, `None` when it does not.
pub type Choice = (Option<usize>, u64);

/// Decision `d` ran: today's choice and the tree's, at `now_ms`. Counted; when they differ, a line at most once per
/// `LINE_EVERY_MS` per decision.
pub fn note(d: Decision, today: Choice, tree: Choice, now_ms: u64) {
    let r = &RECS[d as usize];
    let runs = r.runs.fetch_add(1, Relaxed) + 1;
    if today.0 == tree.0 {
        return;
    }
    let differ = r.differ.fetch_add(1, Relaxed) + 1;
    let last = r.last_ms.load(Relaxed);
    if last != 0 && now_ms.saturating_sub(last) < LINE_EVERY_MS {
        return;
    }
    if r.last_ms.compare_exchange(last, now_ms.max(1), Relaxed, Relaxed).is_err() {
        return;
    }
    #[cfg(feature = "std")]
    write_line(d, today, tree, runs, differ);
    #[cfg(not(feature = "std"))]
    let _ = (runs, differ);
}

#[cfg(feature = "std")]
struct LineBuf {
    b: [u8; 384],
    n: usize,
}

#[cfg(feature = "std")]
impl core::fmt::Write for LineBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let k = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
        self.n += k;
        Ok(())
    }
}

#[cfg(feature = "std")]
#[cold]
fn write_line(d: Decision, today: Choice, tree: Choice, runs: u64, differ: u64) {
    use core::fmt::Write as _;
    let mut e = LineBuf { b: [0; 384], n: 0 };
    let side = |e: &mut LineBuf, c: Choice| match (d, c.0) {
        (Decision::OwnSpill, Some(_)) => write!(e, "spills ({} kB held)", c.1 >> 10),
        (Decision::OwnSpill, None) => write!(e, "does not spill ({} kB held)", c.1 >> 10),
        (_, Some(p)) => write!(e, "backend {p} ({} kB)", c.1 >> 10),
        (_, None) => write!(e, "none ({} kB)", c.1 >> 10),
    };
    let _ = write!(e, "ledger2-shadow: {}: today ", DECISION_NAMES[d as usize]);
    let _ = side(&mut e, today);
    let _ = write!(e, "; the owner tree ");
    let _ = side(&mut e, tree);
    let _ = write!(e, "; {differ} of {runs} such decisions differ since start");
    let (scans, ns, max) = scan_cost();
    let _ = write!(e, "; operator-cell scans {scans}, mean {} us, max {} us", ns / scans.max(1) / 1_000, max / 1_000);
    e.n = e.n.min(e.b.len() - 1);
    e.b[e.n] = b'\n';
    // D4812: through the server's stderr writer (deferred on a memory safety thread), never a raw stderr write
    crate::server_log_line(&e.b[..e.n + 1]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decision_counts_and_differs_only_when_the_choices_do() {
        let before = counts()[Decision::Revoke as usize];
        note(Decision::Revoke, (Some(3), 10), (Some(3), 99), 5);
        note(Decision::Revoke, (Some(3), 10), (Some(4), 99), 6);
        note(Decision::Revoke, (None, 0), (Some(4), 99), 7);
        let after = counts()[Decision::Revoke as usize];
        assert_eq!((after.0 - before.0, after.1 - before.1), (3, 2));
    }

    /// The R4a filter (main 09:50:42Z): a skipped decision is counted apart, never as run or differing, and the summary
    /// line shows it.
    #[test]
    fn a_skipped_decision_is_counted_and_summarized_not_compared() {
        let before = counts()[Decision::OwnSpill as usize];
        skip(Decision::OwnSpill);
        skip(Decision::OwnSpill);
        let after = counts()[Decision::OwnSpill as usize];
        assert_eq!((after.0 - before.0, after.1 - before.1), (0, 0), "not compared");
        assert!(after.2 - before.2 >= 2, "counted as skipped");
        let mut b = [0u8; 512];
        let n = summary_line(&mut b);
        let line = core::str::from_utf8(&b[..n]).unwrap();
        assert!(line.starts_with("ledger2-shadow summary since start:"), "{line}");
        // other tests may note decisions meanwhile: the shape, not the numbers
        assert!(line.contains("R4a own spill ") && line.contains(" skipped;") && line.contains("operator-cell scans "), "{line}");
    }
}

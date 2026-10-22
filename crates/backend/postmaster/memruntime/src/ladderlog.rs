//! S1 criterion 11: the memory ladder's takes from each sized cache and its raises of one as server log lines
//! (`ladder_log_ms`, default 1000; 0 writes none). Only while the ladder acts (pgrust.memory_ladder = on with cache_floor). The memory watchdog's thread calls [`sample`]
//! after each ladder sample. It reads mcx's per-cache counters, the ones the views read (`mcx::ladder::sized_takes`,
//! `sized_counts`), so a take made on a query thread (a draw's inline take, a gate's climb) costs nothing more there.
//! The line is formatted here and elog hands it to memlog's ring (D4812): no thread of the ladder writes the log itself.
//! One line per cache and kind (take, raise) at most every `ladder_log_ms`: the moves inside the window fold into the
//! next line, which counts them.

use mcx::ladder::SIZED_N;

/// Per sized cache (`mcx::ladder::SIZED_*`): the ladder's takes (count, bytes) and raises (count, bytes) since start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Moves {
    pub takes: [(u64, u64); SIZED_N],
    pub raises: [(u64, u64); SIZED_N],
}

impl Moves {
    pub fn now() -> Moves {
        let raised = mcx::ladder::sized_counts();
        Moves { takes: mcx::ladder::sized_takes(), raises: core::array::from_fn(|c| (raised[c].0, raised[c].1)) }
    }
}

/// The sized caches' names, as the views and the ledger name them.
pub const NAMES: [&str; SIZED_N] =
    [memledger::Consumer::PartCache.name(), memledger::Consumer::FpTables.name(), memledger::Consumer::CondCache.name(), "pool_reserve"];

/// What the lines have said so far: the counts through each cache's last line per kind, the takes made on the ladder's own
/// passes since its last take line, and when each last line was written (decide's clock, ms; 0 = none yet).
#[derive(Clone, Copy, Debug, Default)]
pub struct Logged {
    through: Moves,
    pass: [(u64, u64); SIZED_N],
    at_ms: [[u64; 2]; SIZED_N],
}

static LOGGED: pgsync::Mutex<Option<Logged>> = pgsync::Mutex::new(None);

/// The ladder sample's view of the caches for a line: each budget (`u64::MAX` = at its start) and floor, the read and the
/// line.
#[derive(Clone, Copy, Debug)]
pub struct At {
    pub now_ms: u64,
    pub read: u64,
    pub line: u64,
    pub budgets: [u64; SIZED_N],
    pub floors: [u64; SIZED_N],
}

/// The watchdog thread after the ladder's sample: `before` was read before the sample's passes (the hook's and the
/// background one), so the takes since are the ladder's own (a draw's take in those microseconds counts as one); the rest
/// since the last line were the draws' and the gates'.
pub fn sample(before: &Moves, now_ms: u64, read: u64, line: u64) {
    if !mempolicy::ladder::take_log_on() {
        return;
    }
    let at = At { now_ms, read, line, budgets: mcx::ladder::sized_budgets(), floors: mcx::ladder::sized_floors() };
    let Ok(mut g) = LOGGED.try_lock() else { return };
    let logged = g.get_or_insert_with(|| Logged { through: *before, ..Logged::default() });
    lines(logged, before, &Moves::now(), &at, &mut |l| crate::log_line(&l));
}

fn mb(b: u64) -> u64 {
    b >> 20
}

/// The read against the line, in words.
fn against(read: u64, line: u64) -> String {
    if read > line { format!("{} MB past", mb(read - line)) } else { format!("{} MB under", mb(line - read)) }
}

fn budget(b: u64) -> String {
    if b == u64::MAX { "at its start".to_owned() } else { format!("{} MB", mb(b)) }
}

/// The lines due at `at` (`mempolicy::ladder::take_log_due` per cache and kind), `before` and `after` the counts around
/// the ladder's own passes this sample; `logged` advanced for each line written.
pub fn lines(logged: &mut Logged, before: &Moves, after: &Moves, at: &At, out: &mut dyn FnMut(String)) {
    let r2a = mempolicy::ladder::Rung::EvictElastic.name();
    for c in 0..SIZED_N {
        let own = (after.takes[c].0.saturating_sub(before.takes[c].0), after.takes[c].1.saturating_sub(before.takes[c].1));
        logged.pass[c] = (logged.pass[c].0 + own.0, logged.pass[c].1 + own.1);
        let took = (after.takes[c].0.saturating_sub(logged.through.takes[c].0), after.takes[c].1.saturating_sub(logged.through.takes[c].1));
        if took.0 != 0 && mempolicy::ladder::take_log_due(at.now_ms, logged.at_ms[c][0]) {
            let pass = (logged.pass[c].0.min(took.0), logged.pass[c].1.min(took.1));
            out(format!(
                "ladder take: {} gave {} MB to {r2a} in {} take{} since its last take line (on the ladder's passes {} / {} MB, by draws and gate climbs {} / {} MB); budget {}, floor {} MB; read {} MB, line {} MB ({})",
                NAMES[c],
                mb(took.1),
                took.0,
                if took.0 == 1 { "" } else { "s" },
                pass.0,
                mb(pass.1),
                took.0 - pass.0,
                mb(took.1 - pass.1),
                budget(at.budgets[c]),
                mb(at.floors[c]),
                mb(at.read),
                mb(at.line),
                against(at.read, at.line)
            ));
            logged.through.takes[c] = after.takes[c];
            logged.pass[c] = (0, 0);
            logged.at_ms[c][0] = at.now_ms;
        }
        let raised = (after.raises[c].0.saturating_sub(logged.through.raises[c].0), after.raises[c].1.saturating_sub(logged.through.raises[c].1));
        if raised.0 != 0 && mempolicy::ladder::take_log_due(at.now_ms, logged.at_ms[c][1]) {
            out(format!(
                "ladder raise: {} budget raised {} MB in {} raise{} since its last raise line, to {}; read {} MB, line {} MB ({})",
                NAMES[c],
                mb(raised.1),
                raised.0,
                if raised.0 == 1 { "" } else { "s" },
                budget(at.budgets[c]),
                mb(at.read),
                mb(at.line),
                against(at.read, at.line)
            ));
            logged.through.raises[c] = after.raises[c];
            logged.at_ms[c][1] = at.now_ms;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    fn at(now_ms: u64) -> At {
        At { now_ms, read: 15_200 * MIB, line: 15_100 * MIB, budgets: [1024 * MIB, u64::MAX, 320 * MIB, u64::MAX], floors: [1030 * MIB, 0, 0, 0] }
    }

    #[test]
    fn a_take_is_one_line_with_its_cache_bytes_rung_and_read_against_the_line_and_the_window_folds_the_rest() {
        let _one = pgsync::lock(&mempolicy::consts::TEST_LOCK);
        mempolicy::consts::reset();
        let mut logged = Logged::default();
        let mut seen = Vec::new();
        let zero = Moves::default();
        let mut one = zero;
        one.takes[mcx::ladder::SIZED_PART] = (1, 812 * MIB);
        mempolicy::consts::set(mempolicy::consts::K::LadderLogMs, 0);
        lines(&mut logged, &zero, &one, &at(5_000), &mut |l| seen.push(l));
        assert!(seen.is_empty(), "ladder_log_ms 0: no line");
        mempolicy::consts::reset();
        assert_eq!(mempolicy::consts::get(mempolicy::consts::K::LadderLogMs), 1_000, "the default");
        let mut logged = Logged::default();
        // the take on the ladder's own pass this sample
        lines(&mut logged, &zero, &one, &at(5_000), &mut |l| seen.push(l));
        assert_eq!(
            seen,
            ["ladder take: part_cache gave 812 MB to R2a in 1 take since its last take line (on the ladder's passes 1 / 812 MB, by draws and gate climbs 0 / 0 MB); budget 1024 MB, floor 1030 MB; read 15200 MB, line 15100 MB (100 MB past)"]
        );
        // two draws' takes between passes and a raise of the condition cache, 300 ms later: the takes inside the window
        // wait, the raise (its own kind) is due
        let mut two = one;
        two.takes[mcx::ladder::SIZED_PART] = (3, 1012 * MIB);
        two.raises[mcx::ladder::SIZED_COND] = (1, 64 * MIB);
        seen.clear();
        lines(&mut logged, &two, &two, &at(5_300), &mut |l| seen.push(l));
        assert_eq!(seen, ["ladder raise: condition_cache budget raised 64 MB in 1 raise since its last raise line, to 320 MB; read 15200 MB, line 15100 MB (100 MB past)"]);
        seen.clear();
        lines(&mut logged, &two, &two, &at(5_999), &mut |l| seen.push(l));
        assert!(seen.is_empty(), "inside the window: folded");
        lines(&mut logged, &two, &two, &at(6_000), &mut |l| seen.push(l));
        assert_eq!(seen.len(), 1);
        assert!(seen[0].starts_with("ladder take: part_cache gave 200 MB to R2a in 2 takes since its last take line (on the ladder's passes 0 / 0 MB, by draws and gate climbs 2 / 200 MB)"), "{}", seen[0]);
        mempolicy::consts::reset();
    }

    /// The rate limit: a second take line of the same cache inside `ladder_log_ms` is dropped; its take
    /// is counted in the cache's next line.
    #[test]
    fn a_second_take_line_inside_the_window_is_dropped_and_its_take_counted_in_the_next() {
        let _one = pgsync::lock(&mempolicy::consts::TEST_LOCK);
        mempolicy::consts::reset();
        let (mut logged, mut seen) = (Logged::default(), Vec::new());
        let zero = Moves::default();
        let mut first = zero;
        first.takes[mcx::ladder::SIZED_PART] = (1, 100 * MIB);
        lines(&mut logged, &zero, &first, &at(10_000), &mut |l| seen.push(l));
        assert_eq!(seen.len(), 1, "the first take: a line");
        let mut second = first;
        second.takes[mcx::ladder::SIZED_PART] = (2, 150 * MIB);
        lines(&mut logged, &first, &second, &at(10_400), &mut |l| seen.push(l));
        assert_eq!(seen.len(), 1, "the second take 400 ms later, inside the 1000 ms window: its line dropped");
        lines(&mut logged, &second, &second, &at(11_000), &mut |l| seen.push(l));
        assert_eq!(seen.len(), 2, "the window over: the next line");
        assert!(seen[1].starts_with("ladder take: part_cache gave 50 MB to R2a in 1 take since its last take line (on the ladder's passes 1 / 50 MB"), "{}", seen[1]);
        mempolicy::consts::reset();
    }

    /// The logger reads the counters the takes write: a take noted through mcx (as the driver's R2a does) is a line.
    #[test]
    fn a_take_noted_in_mcx_reaches_the_line() {
        let _one = pgsync::lock(&mempolicy::consts::TEST_LOCK);
        mempolicy::consts::reset();
        let mut logged = Logged { through: Moves::now(), ..Logged::default() };
        let before = Moves::now();
        mcx::ladder::note_take_bytes(mcx::ladder::SIZED_FP, 96 * MIB);
        let mut seen = Vec::new();
        lines(&mut logged, &before, &Moves::now(), &at(7_000), &mut |l| seen.push(l));
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].starts_with("ladder take: fp_tables gave 96 MB to R2a in 1 take"), "{}", seen[0]);
        mempolicy::consts::reset();
    }
}

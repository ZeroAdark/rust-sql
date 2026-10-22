//! D4872: the host's memory pressure, for the page pool's give-back. The pool's other release triggers read the memory
//! ladder's own line (its accounting against the budget), and a server well under that line keeps its free memory while
//! the kernel evicts the page cache around it: a bulk load on a 32 GiB box (x86) evicted ~20 GiB of page cache and
//! ran 19 % slower under the pool than under mimalloc, the pool's free superpages held the whole time. The kernel says
//! when memory is short before any line does: the memory PSI's `some` stall time (tasks waiting on reclaim, compaction or
//! a thrashing refault) and the file pages refaulted (`workingset_refault_file`: page cache evicted and read back).
//! Either one past its knob over a window presses the host; it stays pressed `pool_host_hold_ms` after the last window
//! past a knob. A counter going backwards (another cgroup's file, a reopened one) restarts the window. By default only the
//! refaults press (`pool_host_psi_permille` 0): on that load mimalloc never refaulted while Hearth refaulted past 32 MiB/s
//! from 12 s in, but mimalloc's own PSI crossed 10 per mille in 9 one-second windows.

use crate::consts::{get, K};

/// One reading of the host's pressure counters: the PSI `some` total (µs) and the file refaults (bytes); `None` when the
/// file is unreadable (that signal does not press).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reading {
    pub psi_some_us: Option<u64>,
    pub refault_bytes: Option<u64>,
}

/// The window's measure, as last closed: the stall share (per mille of the window's wall time) and the refault rate
/// (bytes a second).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Window {
    pub psi_permille: u64,
    pub refault_per_s: u64,
}

/// The knobs a window is judged by (`pool_host_*`): 0 turns that signal off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bars {
    pub window_ms: u64,
    pub psi_permille: u64,
    pub refault_per_s: u64,
    pub hold_ms: u64,
}

impl Bars {
    pub fn now() -> Self {
        Bars {
            window_ms: get(K::PoolHostWindowMs).max(1),
            psi_permille: get(K::PoolHostPsiPermille),
            refault_per_s: get(K::PoolHostRefaultMibS).saturating_mul(1 << 20),
            hold_ms: get(K::PoolHostHoldMs),
        }
    }
}

/// The tracker: the window's start (ms and readings) and the hold's end.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostPress {
    mark_ms: u64,
    mark: Reading,
    until_ms: u64,
    last: Window,
}

impl HostPress {
    pub const fn new() -> Self {
        HostPress { mark_ms: 0, mark: Reading { psi_some_us: None, refault_bytes: None }, until_ms: 0, last: Window { psi_permille: 0, refault_per_s: 0 } }
    }

    /// A sample at `now_ms` (a monotonic clock from 1): whether the host is pressed now.
    pub fn step(&mut self, now_ms: u64, r: Reading, bars: &Bars) -> bool {
        if self.mark_ms == 0 || now_ms < self.mark_ms {
            self.restart(now_ms, r);
            return now_ms < self.until_ms;
        }
        let span = now_ms - self.mark_ms;
        if span >= bars.window_ms {
            let delta = |a: Option<u64>, b: Option<u64>| match (a, b) {
                (Some(a), Some(b)) if b >= a => Some(b - a),
                _ => None,
            };
            let psi = delta(self.mark.psi_some_us, r.psi_some_us).map_or(0, |us| us / span);
            let refault = delta(self.mark.refault_bytes, r.refault_bytes).map_or(0, |b| b.saturating_mul(1_000) / span);
            self.last = Window { psi_permille: psi, refault_per_s: refault };
            if (bars.psi_permille != 0 && psi >= bars.psi_permille) || (bars.refault_per_s != 0 && refault >= bars.refault_per_s) {
                self.until_ms = now_ms.saturating_add(bars.hold_ms.max(1));
            }
            self.restart(now_ms, r);
        }
        now_ms < self.until_ms
    }

    /// The last closed window's measure.
    pub fn last(&self) -> Window {
        self.last
    }

    fn restart(&mut self, now_ms: u64, r: Reading) {
        self.mark_ms = now_ms.max(1);
        self.mark = r;
    }
}

#[cfg(test)]
mod tests {
    use super::{Bars, HostPress, Reading, Window};

    const MIB: u64 = 1 << 20;
    const BARS: Bars = Bars { window_ms: 500, psi_permille: 50, refault_per_s: 64 * MIB, hold_ms: 2_000 };

    fn r(psi_us: u64, refault: u64) -> Reading {
        Reading { psi_some_us: Some(psi_us), refault_bytes: Some(refault) }
    }

    /// Nothing presses until a window closes past a knob; a window's stall share and refault rate are per its wall time.
    #[test]
    fn a_window_past_either_knob_presses_and_one_under_both_does_not() {
        let mut h = HostPress::new();
        assert!(!h.step(1, r(0, 0), &BARS), "the first sample opens a window");
        assert!(!h.step(400, r(20_000, 0), &BARS), "the window is still open (its stall so far unread)");
        assert!(h.step(501, r(25_000, 0), &BARS), "25 ms stalled in 500 ms = 50 per mille: pressed");
        assert_eq!(h.last(), Window { psi_permille: 50, refault_per_s: 0 });

        let mut h = HostPress::new();
        h.step(1, r(0, 0), &BARS);
        assert!(!h.step(501, r(20_000, 31 * MIB), &BARS), "40 per mille and 62 MiB/s: under both");
        assert!(h.step(1_001, r(20_000, 31 * MIB + 32 * MIB + 1), &BARS), "past 64 MiB/s of refaults: pressed");
    }

    /// Pressed lasts `hold_ms` past the last window over a knob, then lets go; a later window past a knob presses again.
    #[test]
    fn the_press_holds_then_lets_go() {
        let mut h = HostPress::new();
        h.step(1, r(0, 0), &BARS);
        assert!(h.step(501, r(100_000, 0), &BARS));
        assert!(h.step(1_001, r(100_000, 0), &BARS), "a quiet window: still held");
        assert!(h.step(2_500, r(100_000, 0), &BARS), "held until 2,501");
        assert!(!h.step(2_501, r(100_000, 0), &BARS), "the hold is over");
        assert!(h.step(3_001, r(100_000, 128 * MIB), &BARS), "refaults again: pressed");
    }

    /// A knob at 0 turns its signal off; an unreadable counter (None) or one that went back presses nothing.
    #[test]
    fn an_off_knob_an_unreadable_file_or_a_counter_going_back_never_press() {
        let off = Bars { psi_permille: 0, refault_per_s: 0, ..BARS };
        let mut h = HostPress::new();
        h.step(1, r(0, 0), &off);
        assert!(!h.step(501, r(500_000, 1 << 40), &off), "both knobs off");

        let mut h = HostPress::new();
        h.step(1, Reading::default(), &BARS);
        assert!(!h.step(501, Reading::default(), &BARS), "no files: nothing to read");

        let mut h = HostPress::new();
        h.step(1, r(500_000, 1 << 40), &BARS);
        assert!(!h.step(501, r(0, 0), &BARS), "both counters went back (a reopened file): the window restarts");
        assert!(!h.step(1_001, r(1_000, MIB), &BARS), "the restarted window is quiet");
    }

    /// The clock going back (a test's, a new epoch) restarts the window instead of underflowing.
    #[test]
    fn a_clock_going_back_restarts_the_window() {
        let mut h = HostPress::new();
        h.step(1_000, r(0, 0), &BARS);
        assert!(!h.step(10, r(1_000_000, 0), &BARS));
        assert!(!h.step(400, r(1_000_000, 0), &BARS), "the window restarted at 10");
    }
}

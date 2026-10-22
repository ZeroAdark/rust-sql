//! Memory step 6a: the broker's signals. A 1 Hz ring per sampled series (the work pool's touched bytes, the unattributed
//! bytes) and, every interval, the census the policy reads: the budget's snapshot, the part cache's and the fp tables'
//! re-used working sets, the registry's removals, the queued admissions' needs.

use gravity_driver::mem::budget::{self, Consumer, Snapshot, CONSUMERS};
use gravity_driver::mem::policy::{Input, Signals};

/// The p90 window: 300 one-second samples.
pub const RING: usize = 300;

/// The latest `RING` samples of one series (1 Hz).
#[derive(Clone, Copy, Debug)]
pub struct Ring {
    v: [u64; RING],
    n: usize,
    at: usize,
}

impl Default for Ring {
    fn default() -> Ring {
        Ring { v: [0; RING], n: 0, at: 0 }
    }
}

impl Ring {
    pub fn push(&mut self, x: u64) {
        self.v[self.at] = x;
        self.at = (self.at + 1) % RING;
        self.n = (self.n + 1).min(RING);
    }

    /// The nearest-rank p90 of the samples held (0 with none).
    pub fn p90(&self) -> u64 {
        if self.n == 0 {
            return 0;
        }
        let mut s = [0u64; RING];
        s[..self.n].copy_from_slice(&self.v[..self.n]);
        s[..self.n].sort_unstable();
        let rank = (self.n * 9).div_ceil(10).max(1);
        s[rank - 1]
    }

    /// The latest sample (0 with none).
    pub fn last(&self) -> u64 {
        if self.n == 0 {
            0
        } else {
            self.v[(self.at + RING - 1) % RING]
        }
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }
}

/// The census beside the policy's inputs (the view and the replay TSV show every field).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Census {
    /// The re-used working set per consumer over 60 / 300 / 900 s (part cache and fp tables; the others none).
    pub reused: [[u64; 3]; CONSUMERS],
    /// `(entries, segment bytes)` the part registry's janitor removed so far.
    pub registry_removed: (u64, u64),
    /// The file bytes of the part streams opened inside 900 s (the page-cache floor's signal).
    pub file_bytes_900: u64,
    /// The registry's parts, those a statement used inside 900 s, and those re-used inside 900 s.
    pub parts: u64,
    pub used_parts_900: u64,
    pub reused_parts_900: u64,
    /// The work pool's sampled signal now ([`work_sample`]: the bytes it touches, else its held bytes), its p90 over
    /// 300 s, the running statements' floor, the queued admissions' needs.
    pub work_signal: u64,
    pub work_p90: u64,
    pub running_floor: u64,
    pub queued: u64,
    /// The unattributed bytes' p90 over 300 s, and whether the fit counts it (the governor samples resident memory).
    pub unattributed_p90: u64,
    pub unattributed_counted: bool,
    /// The broker second a re-used part was last evicted (0 = never).
    pub reused_evicted_at: u32,
}

/// The current bytes of every registered consumer (the 1 Hz sample: currents only, no floor walks).
pub fn currents() -> [Option<u64>; CONSUMERS] {
    let mut c = [None; CONSUMERS];
    for k in Consumer::ALL {
        c[k as usize] = budget::hooks(k).map(|h| (h.current)());
    }
    c
}

/// The work pool's 1 Hz sample. A reservation is every participant's region at the work_mem / hash_mem floor, touched
/// late and partly (about a sixth of the reserved bytes, measured over whole runs), so the pool's held bytes overstate its
/// demand several times over. With the governor sampling resident memory (`projected` > 0), the sample is what is
/// resident beyond every other row's current, at most the held bytes: the pool's touched bytes, plus any memory no row
/// attributes. Without it the sample is the held bytes.
pub fn work_sample(held: u64, others: u64, projected: u64) -> u64 {
    if projected == 0 {
        held
    } else {
        projected.saturating_sub(others).min(held)
    }
}

/// The census at broker second `now`, from the snapshot and the rings.
pub fn census(now: u32, snap: &Snapshot, work: &Ring, unattributed: &Ring) -> Census {
    let registry = pgrc_am::inval::registry();
    let parts = registry.reuse_census(now);
    let mut reused = [[0u64; 3]; CONSUMERS];
    reused[Consumer::PartCache as usize] = parts.reused;
    reused[Consumer::FpTables as usize] = crate::dict::text_hosts::fp_reused_bytes(now);
    let wp = snap.row(Consumer::WorkPool);
    Census {
        reused,
        registry_removed: registry.removed(),
        file_bytes_900: parts.file_bytes_900,
        parts: parts.parts,
        used_parts_900: parts.used_parts_900,
        reused_parts_900: parts.reused_parts[2],
        work_signal: work.last(),
        work_p90: work.p90(),
        running_floor: wp.floor,
        queued: gravity_driver::mem::grant::queued_bytes(),
        unattributed_p90: unattributed.p90(),
        unattributed_counted: snap.bar != 0,
        reused_evicted_at: pgrc_read::partcache::reused_evicted_at(),
    }
}

/// The policy's inputs from the snapshot and the census (pure; the tests and the replay feed it).
pub fn signals(now: u32, snap: &Snapshot, c: &Census) -> Signals {
    let mut inputs = [Input::default(); CONSUMERS];
    for k in Consumer::ALL {
        let r = snap.row(k);
        inputs[k as usize] = Input {
            registered: r.registered,
            current: r.current,
            floor: r.floor,
            cap: r.cap,
            reused: matches!(k, Consumer::PartCache | Consumer::FpTables).then_some(c.reused[k as usize]),
            reused_evicted: k == Consumer::PartCache && c.reused_evicted_at != 0 && now.saturating_sub(c.reused_evicted_at) <= 300,
            // shared_buffers' configured size (its floor until BP1); the explicit part cache joins at step 7
            minimum: if k == Consumer::SharedBuffers { r.floor } else { 0 },
            doors: None,
        };
    }
    let fixed = snap.fixed.saturating_add(if c.unattributed_counted { c.unattributed_p90 } else { 0 });
    Signals {
        now: u64::from(now),
        b: snap.budget.b,
        r_high: snap.budget.r_high,
        fixed,
        work_demand: c.work_p90.max(c.running_floor).saturating_add(c.queued),
        queued: c.queued,
        inputs,
        pool_on_tick: budget::pool_follows(),
        value: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_driver::mem::budget::{compose, Cfg, Row};

    const GIB: u64 = 1 << 30;

    #[test]
    fn the_policy_inputs_come_from_the_snapshot_and_the_census() {
        let mut rows = [Row::default(); CONSUMERS];
        let reg = |current, floor, cap| Row { registered: true, current, floor, cap, target: None };
        rows[Consumer::SharedBuffers as usize] = reg(2 * GIB, 2 * GIB, 2 * GIB);
        rows[Consumer::PartCache as usize] = reg(GIB, GIB / 4, 4 * GIB);
        rows[Consumer::FpTables as usize] = reg(0, 0, 4 * GIB);
        rows[Consumer::ZoneStore as usize] = reg(0, 0, GIB / 2);
        rows[Consumer::ProgramCaches as usize] = reg(GIB / 4, GIB / 4, GIB / 4);
        rows[Consumer::WorkPool as usize] = reg(3 * GIB, GIB, 20 * GIB);
        let snap = compose(Cfg::DEFAULT, 31 * GIB, 28 * GIB, 0, rows, 0);
        let mut c = Census { work_p90: 2 * GIB, running_floor: GIB, queued: GIB / 2, unattributed_p90: GIB, ..Census::default() };
        c.reused[Consumer::PartCache as usize] = [1, 2, 3];
        c.reused[Consumer::FpTables as usize] = [4, 5, 6];
        c.reused_evicted_at = 1000;
        let s = signals(1200, &snap, &c);
        assert_eq!((s.b, s.r_high, s.now), (snap.budget.b, snap.budget.r_high, 1200));
        assert_eq!(s.inputs[Consumer::PartCache as usize].reused, Some([1, 2, 3]));
        assert_eq!(s.inputs[Consumer::FpTables as usize].reused, Some([4, 5, 6]));
        assert_eq!(s.inputs[Consumer::ZoneStore as usize].reused, None);
        assert!(s.inputs[Consumer::PartCache as usize].reused_evicted, "evicted 200 s ago: inside 300 s");
        assert!(!signals(1400, &snap, &c).inputs[Consumer::PartCache as usize].reused_evicted);
        assert_eq!(s.inputs[Consumer::SharedBuffers as usize].minimum, 2 * GIB);
        assert_eq!(s.inputs[Consumer::PartCache as usize].minimum, 0);
        assert!(!s.inputs[Consumer::BuildPool as usize].registered);
        assert_eq!(s.work_demand, 2 * GIB + GIB / 2, "max(p90, running floor) + queued");
        assert_eq!(s.fixed, GIB / 4, "the unattributed p90 counts only when the governor samples resident memory");
        c.unattributed_counted = true;
        assert_eq!(signals(1200, &snap, &c).fixed, GIB / 4 + GIB);
    }

    #[test]
    fn the_work_sample_is_the_touched_bytes_when_the_governor_samples_resident_memory() {
        assert_eq!(work_sample(12 * GIB, 3 * GIB, 0), 12 * GIB, "no resident sample: the held bytes");
        assert_eq!(work_sample(12 * GIB, 3 * GIB, 4 * GIB), GIB, "resident past the other rows: what the pool touches");
        assert_eq!(work_sample(GIB, 3 * GIB, 9 * GIB), GIB, "never past the held bytes");
        assert_eq!(work_sample(GIB, 3 * GIB, 2 * GIB), 0, "the other rows hold more than is resident");
        let mut r = Ring::default();
        assert_eq!(r.last(), 0);
        for x in 1..=(RING as u64 + 5) {
            r.push(x);
        }
        assert_eq!(r.last(), RING as u64 + 5, "the latest sample, after the ring wrapped");
    }

    #[test]
    fn the_ring_keeps_300_samples_and_its_p90_is_the_nearest_rank() {
        let mut r = Ring::default();
        assert_eq!(r.p90(), 0);
        for x in 1..=10 {
            r.push(x);
        }
        assert_eq!(r.p90(), 9);
        for x in 1..=1000 {
            r.push(x);
        }
        assert_eq!(r.len(), RING);
        // the latest 300: 701..=1000; rank 270 -> 970
        assert_eq!(r.p90(), 970);
    }
}

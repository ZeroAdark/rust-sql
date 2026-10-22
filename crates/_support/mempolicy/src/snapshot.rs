use memledger::{Cfg, Consumer, Mode, Row, CLASSES, CONSUMERS};

use crate::sizes::{budget_of, work_pool_capacity, Budget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub mode: Mode,
    pub budget: Budget,
    /// 0 = the governor is off.
    pub bar: u64,
    pub rows: [Row; CONSUMERS],
    pub sum_current: u64,
    pub fixed: u64,
    pub accounted: u64,
    /// 0 = not sampled.
    pub resident: u64,
    /// Resident memory no account holds, past `retained`.
    pub unattributed: u64,
    /// ALLOC-LADDER (a): the allocator's retained bytes (free blocks it keeps resident; `memledger::arena::retained`),
    /// counted as used until they are returned, whatever the accounts read.
    pub retained: u64,
    pub free_b: u64,
    pub work_pool_capacity: u64,
    /// Resident bytes by reclaim class (`memledger::class_bytes`, `Class as usize`); 0 until `with_classes` fills them.
    pub by_class: [i64; CLASSES],
}

impl Snapshot {
    pub fn row(&self, c: Consumer) -> Row {
        self.rows[c as usize]
    }

    pub fn line(&self) -> String {
        let mib = |b: u64| b >> 20;
        let mut s = format!( // alloc: Cold
            "mode {} limit {} B_auto {} B {} r_low {} r_high {} bar {} free_B {} fixed {} unattributed {} retained {} work_pool_capacity {}",
            self.mode.name(),
            mib(self.budget.limit),
            mib(self.budget.b_auto),
            mib(self.budget.b),
            mib(self.budget.r_low),
            mib(self.budget.r_high),
            mib(self.bar),
            mib(self.free_b),
            mib(self.fixed),
            mib(self.unattributed),
            mib(self.retained),
            mib(self.work_pool_capacity),
        );
        for c in Consumer::ALL {
            let r = self.row(c);
            if r.registered {
                s.push_str(&format!(" {}={}/{}/{}", c.name(), mib(r.current), mib(r.floor), mib(r.cap))); // alloc: Cold
            }
        }
        s.push_str(" (MiB: current/floor/cap)"); // alloc: Cold
        s
    }
}

pub fn compose(cfg: Cfg, limit: u64, bar: u64, resident: u64, rows: [Row; CONSUMERS], managed: u64) -> Snapshot {
    compose_with(cfg, limit, bar, resident, rows, managed, 0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResidentFrom {
    Projected,
    Probe,
    None,
}

/// The governor's projection when it samples; under on the probe's reading otherwise; else none (off / advise).
#[inline]
pub fn resident_from(bar: u64, mode: Mode) -> ResidentFrom {
    match (bar, mode) {
        (0, Mode::On) => ResidentFrom::Probe,
        (0, _) => ResidentFrom::None,
        _ => ResidentFrom::Projected,
    }
}

pub fn b_now(limit: u64, bar: u64) -> u64 {
    let c = memledger::cfg();
    budget_of(limit, c.target, c.page_cache_floor, c.reserve, bar).b
}

pub fn compose_with(cfg: Cfg, limit: u64, bar: u64, resident: u64, rows: [Row; CONSUMERS], managed: u64, accounted: u64) -> Snapshot {
    compose_retained(cfg, limit, bar, resident, rows, managed, accounted, 0)
}

/// `compose_with` with the allocator's retained bytes (ALLOC-LADDER (a)): they count as used beside the accounts
/// (`used = max(sum + retained, resident)`), so they lower free_B even when the accounts read above resident, and they
/// leave `unattributed` (no double count) while the pool's capacity still takes them off.
#[allow(clippy::too_many_arguments)]
pub fn compose_retained(cfg: Cfg, limit: u64, bar: u64, resident: u64, rows: [Row; CONSUMERS], managed: u64, accounted: u64, retained: u64) -> Snapshot {
    let budget = budget_of(limit, cfg.target, cfg.page_cache_floor, cfg.reserve, bar);
    let sum_current = rows.iter().fold(0u64, |a, r| a.saturating_add(r.current)).saturating_add(accounted);
    let fixed = Consumer::ALL.iter().filter(|c| c.is_fixed()).fold(0u64, |a, c| a.saturating_add(rows[*c as usize].current)).saturating_add(accounted);
    let used = sum_current.saturating_add(retained);
    let unattributed = resident.saturating_sub(used);
    let free_b = budget.b.saturating_sub(used.max(resident));
    let caches = Consumer::ALL.iter().filter(|c| c.is_cache()).fold(0u64, |a, c| {
        let r = rows[*c as usize];
        a.saturating_add(if managed & (1u64 << (*c as u32)) != 0 { r.floor } else { r.cap })
    });
    let work_pool_capacity = work_pool_capacity(&budget, fixed, unattributed.saturating_add(retained), rows[Consumer::SharedBuffers as usize].current, caches);
    Snapshot { mode: cfg.mode, budget, bar, rows, sum_current, fixed, accounted, resident, unattributed, retained, free_b, work_pool_capacity, by_class: [0; CLASSES] }
}

/// `s` with the accounts' resident bytes by reclaim class (`memledger::class_bytes`, or a test's sums): information for the
/// decisions; nothing above (B, free_B, the pool's capacity) reads them.
pub fn with_classes(mut s: Snapshot, by_class: [i64; CLASSES]) -> Snapshot {
    s.by_class = by_class;
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sizes::WORK_POOL_MIN;
    use memledger::{Consumer, Row, CONSUMERS};

    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;

    fn row(current: u64, floor: u64, cap: u64) -> Row {
        Row { registered: true, current, floor, cap, target: None }
    }

    fn rows() -> [Row; CONSUMERS] {
        let mut r = [Row::default(); CONSUMERS];
        r[Consumer::SharedBuffers as usize] = row(128 * MIB, 128 * MIB, 128 * MIB);
        r[Consumer::PartCache as usize] = row(GIB, 0, 4 * GIB);
        r[Consumer::FpTables as usize] = row(GIB, 0, 4 * GIB);
        r[Consumer::BuildPool as usize] = row(0, 0, 4 * GIB);
        r[Consumer::ZoneStore as usize] = row(0, 0, GIB / 2);
        r[Consumer::CondCache as usize] = row(0, 0, GIB / 2);
        r[Consumer::ProgramCaches as usize] = row(256 * MIB, 256 * MIB, 256 * MIB);
        r[Consumer::WorkPool as usize] = row(2 * GIB, 0, 14 * GIB);
        r[Consumer::PerBackend as usize] = row(128 * MIB, 128 * MIB, 128 * MIB);
        r
    }

    #[test]
    fn the_snapshot_sums_and_the_work_pool_counts_unmanaged_caches_at_their_caps() {
        let l = 32 * GIB;
        let bar = l / 100 * 90;
        let s = compose(Cfg::DEFAULT, l, bar, 0, rows(), 0);
        assert_eq!(s.budget.b, bar);
        assert_eq!(s.sum_current, 128 * MIB + 2 * GIB + 256 * MIB + 2 * GIB + 128 * MIB);
        assert_eq!(s.fixed, 384 * MIB);
        assert_eq!(s.free_b, bar - s.sum_current);
        let caps = 4 * GIB * 3 + GIB;
        assert_eq!(s.work_pool_capacity, bar - s.budget.r_high - 384 * MIB - 128 * MIB - caps);
        let all = (1u64 << Consumer::PartCache as u32) | (1 << Consumer::FpTables as u32) | (1 << Consumer::BuildPool as u32) | (1 << Consumer::ZoneStore as u32) | (1 << Consumer::CondCache as u32);
        let m = compose(Cfg::DEFAULT, l, bar, 0, rows(), all);
        assert_eq!(m.work_pool_capacity, bar - m.budget.r_high - 384 * MIB - 128 * MIB);
    }

    #[test]
    fn unattributed_memory_counts_when_the_governor_samples_resident() {
        let l = 32 * GIB;
        let bar = l / 100 * 90;
        let base = compose(Cfg::DEFAULT, l, bar, 0, rows(), 0);
        let s = compose(Cfg::DEFAULT, l, bar, base.sum_current + 3 * GIB, rows(), 0);
        assert_eq!(s.unattributed, 3 * GIB);
        assert_eq!(s.free_b, base.free_b - 3 * GIB);
        assert_eq!(s.work_pool_capacity, base.work_pool_capacity - 3 * GIB);
        let under = compose(Cfg::DEFAULT, l, bar, GIB, rows(), 0);
        assert_eq!((under.unattributed, under.free_b), (0, base.free_b), "resident under the sum: nothing unattributed");
    }

    #[test]
    fn a_target_only_lowers_b_and_the_pool_keeps_its_minimum() {
        let l = 32 * GIB;
        let low = Cfg { target: 4 * GIB, ..Cfg::DEFAULT };
        let s = compose(low, l, 0, 0, rows(), 0);
        assert_eq!(s.budget.b, 4 * GIB);
        assert_eq!(s.work_pool_capacity, WORK_POOL_MIN);
        let high = Cfg { target: 64 * GIB, ..Cfg::DEFAULT };
        assert_eq!(compose(high, l, 0, 0, rows(), 0).budget.b, compose(Cfg::DEFAULT, l, 0, 0, rows(), 0).budget.b);
        let z = compose(Cfg::DEFAULT, 0, 0, 0, rows(), 0);
        assert_eq!((z.budget.b, z.free_b), (0, 0));
    }

    /// ALLOC-LADDER (a): retained bytes are used whatever the accounts read: with resident under the accounts' sum (gap 2,
    /// seen live as 'unattributed -242 MiB') they still lower free_B and the pool; above it they leave unattributed and
    /// free_B moves by none of them twice.
    #[test]
    fn retained_bytes_count_as_used_even_when_the_accounts_read_above_resident() {
        let l = 32 * GIB;
        let bar = l / 100 * 90;
        let base = compose(Cfg::DEFAULT, l, bar, GIB, rows(), 0);
        assert!(GIB < base.sum_current, "resident under the accounts");
        let r = compose_retained(Cfg::DEFAULT, l, bar, GIB, rows(), 0, 0, 512 * MIB);
        assert_eq!(r.retained, 512 * MIB);
        assert_eq!(r.free_b, base.free_b - 512 * MIB, "counted as used");
        assert_eq!(r.work_pool_capacity, base.work_pool_capacity - 512 * MIB, "and off the pool");
        let res = base.sum_current + 3 * GIB;
        let over = compose(Cfg::DEFAULT, l, bar, res, rows(), 0);
        let s = compose_retained(Cfg::DEFAULT, l, bar, res, rows(), 0, 0, GIB);
        assert_eq!((s.unattributed, s.retained), (2 * GIB, GIB), "retained leaves unattributed");
        assert_eq!((s.free_b, s.work_pool_capacity), (over.free_b, over.work_pool_capacity), "inside resident: no double count");
    }

    #[test]
    fn accounted_bytes_are_fixed_memory() {
        let l = 32 * GIB;
        let bar = l / 100 * 90;
        let base = compose(Cfg::DEFAULT, l, bar, 0, rows(), 0);
        let s = compose_with(Cfg::DEFAULT, l, bar, 0, rows(), 0, 2 * GIB);
        assert_eq!((s.accounted, s.fixed, s.sum_current), (2 * GIB, base.fixed + 2 * GIB, base.sum_current + 2 * GIB));
        assert_eq!(s.free_b, base.free_b - 2 * GIB);
        assert_eq!(s.work_pool_capacity, base.work_pool_capacity - 2 * GIB);
        let r = compose_with(Cfg::DEFAULT, l, bar, base.sum_current + 3 * GIB, rows(), 0, 2 * GIB);
        assert_eq!(r.unattributed, GIB, "the governor's resident past the sum: the accounted part is no longer unattributed");
    }
}

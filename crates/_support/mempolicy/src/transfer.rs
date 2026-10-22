//! Broker (b): a unit at a time from the cache losing least per byte (tail hits x miss price / tail bytes) to the one
//! gaining most (ghost hits within the unit x miss price / unit) while the gain is larger (STMM's transfer).

use memledger::CONSUMERS;

/// `memledger::value::BUCKETS`: bucket `k` holds ghost hits at `[2^k, 2^(k+1))` evicted bytes.
pub const BUCKETS: usize = 40;

pub const SCALE: u64 = 1024;

/// 1024ths of a rate an interval keeps: a half-life of 3 intervals.
const KEEP: u64 = 813;

/// Cumulative door counters, but `tail_bytes` (what the tail mark covers now).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Doors {
    pub tail_hits: u64,
    pub tail_bytes: u64,
    pub ghosts: [u64; BUCKETS],
    pub cost_ns: u64,
    pub cost_samples: u64,
}

impl Default for Doors {
    fn default() -> Doors {
        Doors { tail_hits: 0, tail_bytes: 0, ghosts: [0; BUCKETS], cost_ns: 0, cost_samples: 0 }
    }
}

/// Decayed per-interval rates x `SCALE`, and the counters last read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Worth {
    pub seen: Doors,
    pub tail: u64,
    pub ghosts: [u64; BUCKETS],
    pub miss_ns: u64,
    pub intervals: u32,
}

impl Default for Worth {
    fn default() -> Worth {
        Worth { seen: Doors::default(), tail: 0, ghosts: [0; BUCKETS], miss_ns: 0, intervals: 0 }
    }
}

fn ewma(prev: u64, delta: u64) -> u64 {
    ((prev as u128 * KEEP as u128) / 1024 + delta as u128 * (1024 - KEEP) as u128).min(u64::MAX as u128) as u64
}

/// The first read only marks the start.
pub fn decay(w: &Worth, now: &Doors) -> Worth {
    if w.intervals == 0 {
        return Worth { seen: *now, intervals: 1, ..Worth::default() };
    }
    let d = |a: u64, b: u64| a.saturating_sub(b);
    let miss_ns = if now.cost_samples > 0 { now.cost_ns / now.cost_samples } else { w.miss_ns };
    Worth {
        seen: *now,
        tail: ewma(w.tail, d(now.tail_hits, w.seen.tail_hits)),
        ghosts: core::array::from_fn(|k| ewma(w.ghosts[k], d(now.ghosts[k], w.seen.ghosts[k]))),
        miss_ns,
        intervals: w.intervals.saturating_add(1),
    }
}

pub fn measured(w: &Worth) -> bool {
    w.intervals >= 2 && w.miss_ns > 0 && w.seen.tail_bytes > 0
}

fn loss(w: &Worth) -> Option<(u128, u128)> {
    measured(w).then(|| (w.tail as u128 * w.miss_ns as u128, w.seen.tail_bytes as u128))
}

fn ghosts_in(w: &Worth, lo: u64, hi: u64) -> u128 {
    let mut n = 0u128;
    for (k, &g) in w.ghosts.iter().enumerate() {
        let (a, b) = if k == 0 { (0u64, 2u64) } else { (1u64 << k, 1u64.checked_shl(k as u32 + 1).unwrap_or(u64::MAX)) };
        let (s, e) = (a.max(lo), b.min(hi));
        if g > 0 && s < e {
            n += g as u128 * (e - s) as u128 / (b - a) as u128;
        }
    }
    n
}

fn gain(w: &Worth, k: u64, unit: u64) -> (u128, u128) {
    let lo = k.saturating_mul(unit);
    (ghosts_in(w, lo, lo.saturating_add(unit)) * w.miss_ns as u128, unit as u128)
}

fn more(a: (u128, u128), b: (u128, u128)) -> bool {
    a.0.saturating_mul(b.1) > b.0.saturating_mul(a.1)
}

/// ns per MiB: the walk's rank.
pub fn loss_per_mib(w: &Worth) -> Option<u64> {
    loss(w).map(|(n, d)| ((n << 20) / d.max(1) / SCALE as u128).min(u64::MAX as u128) as u64)
}

#[derive(Clone, Copy, Debug)]
pub struct Side<'a> {
    pub worth: &'a Worth,
    pub give: u64,
    pub take: u64,
}

/// Bytes given (negative) or taken, zero-sum.
pub fn transfer(sides: &[Option<Side<'_>>; CONSUMERS], unit: u64) -> [i64; CONSUMERS] {
    let mut moved = [0i64; CONSUMERS];
    let (mut given, mut got) = ([0u64; CONSUMERS], [0u64; CONSUMERS]);
    let unit = unit.max(1);
    for _ in 0..4096 {
        let donor = (0..CONSUMERS)
            .filter_map(|i| {
                let s = sides[i]?;
                (given[i] + unit <= s.give && got[i] == 0).then_some((i, loss(s.worth)?))
            })
            .reduce(|a, b| if more(a.1, b.1) { b } else { a });
        let Some((d, l)) = donor else { break };
        let taker = (0..CONSUMERS)
            .filter(|&i| i != d && given[i] == 0)
            .filter_map(|i| {
                let s = sides[i]?;
                (measured(s.worth) && got[i] + unit <= s.take).then_some((i, gain(s.worth, got[i] / unit, unit)))
            })
            .reduce(|a, b| if more(b.1, a.1) { b } else { a });
        let Some((t, g)) = taker else { break };
        if !more(g, l) {
            break;
        }
        given[d] += unit;
        got[t] += unit;
        moved[d] -= unit as i64;
        moved[t] += unit as i64;
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;
    const UNIT: u64 = 64 * MIB;

    fn worth(tail: u64, tail_bytes: u64, ghosts: u64, bucket: usize, ns: u64) -> Worth {
        let mut a = Doors { tail_bytes, cost_ns: ns, cost_samples: 1, ..Doors::default() };
        let w = decay(&Worth::default(), &a);
        a.tail_hits += tail;
        a.ghosts[bucket] += ghosts;
        decay(&w, &a)
    }

    #[test]
    fn b2_the_lower_loss_donor_gives_units_to_the_higher_gain_one_while_the_gain_is_larger() {
        let (cold, hot) = (worth(10, 512 * MIB, 0, 0, 1000), worth(500, 512 * MIB, 4000, 25, 1000));
        let mut sides = [None; CONSUMERS];
        sides[1] = Some(Side { worth: &cold, give: 4 * UNIT, take: 4 * UNIT });
        sides[5] = Some(Side { worth: &hot, give: 4 * UNIT, take: 4 * UNIT });
        let m = transfer(&sides, UNIT);
        assert_eq!((m[1], m[5]), (-(UNIT as i64), UNIT as i64), "one unit: past 64 MiB the hot cache's ghosts end, its gain is 0");
        assert_eq!(m.iter().sum::<i64>(), 0);
        assert!(loss_per_mib(&cold) < loss_per_mib(&hot));
    }

    #[test]
    fn b2_no_move_without_a_gain_past_the_loss_or_before_two_reads() {
        let a = worth(500, 512 * MIB, 0, 0, 1000);
        let b = worth(10, 512 * MIB, 0, 0, 1000);
        let mut sides = [None; CONSUMERS];
        sides[1] = Some(Side { worth: &a, give: 4 * UNIT, take: 4 * UNIT });
        sides[5] = Some(Side { worth: &b, give: 4 * UNIT, take: 4 * UNIT });
        assert_eq!(transfer(&sides, UNIT), [0; CONSUMERS], "no ghosts: nothing to gain");
        let once = decay(&Worth::default(), &Doors { tail_hits: 7, tail_bytes: MIB, ..Doors::default() });
        assert!(!measured(&once) && loss_per_mib(&once).is_none(), "one read marks the start only");
    }

    #[test]
    fn b2_rates_decay_with_a_half_life_of_three_intervals() {
        let mut d = Doors { tail_bytes: MIB, cost_ns: 1, cost_samples: 1, ..Doors::default() };
        let mut w = decay(&Worth::default(), &d);
        for _ in 0..40 {
            d.tail_hits += 100;
            w = decay(&w, &d);
        }
        assert!(w.tail.abs_diff(100 * SCALE) < SCALE, "steady: the rate x SCALE, {}", w.tail);
        for _ in 0..3 {
            w = decay(&w, &d);
        }
        assert!(w.tail.abs_diff(50 * SCALE) < 2 * SCALE, "three quiet intervals halve it: {}", w.tail);
    }
}

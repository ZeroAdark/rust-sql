//! Step 3 (C3): decide(Grow / Start / Tick) under `pgrust.memory_cancel_rule = projection`, the threshold cancel its backstop
//! (D3934). The rule (§2; the rest is reports/MEMORY-STEP3-C3-DESIGN-20261011.md): F = (B - R) + the fast rungs' landed
//! bytes, F* = rise99(cancel landing) + U. Grow: within its fair share or F >= F* goes on, else it spills itself. Start:
//! F < F* waits only while relief lands (D3941). Cancel: only F = 0 and R + rise99 - pending > L - margin; never an idle
//! holder (D3910 (a)).

use core::sync::atomic::{AtomicU8, Ordering};

use crate::consts::{get, K};

const MIB: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Rule {
    Threshold = 0,
    Projection = 1,
}

impl Rule {
    pub const fn from_index(i: i32) -> Rule {
        if i == 1 { Rule::Projection } else { Rule::Threshold }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Rule::Threshold => "threshold",
            Rule::Projection => "projection",
        }
    }
}

static RULE: AtomicU8 = AtomicU8::new(Rule::Threshold as u8);

#[inline]
pub fn rule() -> Rule {
    if RULE.load(Ordering::Relaxed) == Rule::Projection as u8 { Rule::Projection } else { Rule::Threshold }
}

#[inline]
pub fn projection() -> bool {
    RULE.load(Ordering::Relaxed) == Rule::Projection as u8
}

pub fn set_rule(r: Rule) {
    RULE.store(r as u8, Ordering::Relaxed);
}

pub const SAMPLE_MS: u64 = 100;
pub const PRESSED_MS: u64 = 10;

pub fn sample_period_ms(projection: bool, r: u64, b: u64) -> u64 {
    if projection && b != 0 && r > b / 2 { PRESSED_MS } else { SAMPLE_MS }
}

pub fn fast_landed(bytes: u64) -> u64 {
    (bytes as u128 * get(K::FastLandPct) as u128 / 100) as u64
}

pub fn fast_want(i: &Inputs) -> u64 {
    i.target().saturating_sub(i.free()).max(get(K::TrancheMin))
}
pub const RING: usize = 600;
/// D4389: the samples `Rise::rise_p99_scaled` needs before it measures (one second at `SAMPLE_MS`).
pub const RISE_MIN_SAMPLES: usize = 10;

/// R and the unattributed bytes (MiB) per `SAMPLE_MS` over 60 s (`RING`); one writer, no allocation.
#[derive(Clone, Debug)]
pub struct Rise {
    r: [u32; RING],
    ua: [i32; RING],
    at: usize,
    n: usize,
}

impl Default for Rise {
    fn default() -> Rise {
        Rise::new()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rise99 {
    pub bytes: u64,
    pub prior: bool,
}

impl Rise {
    pub const fn new() -> Rise {
        Rise { r: [0; RING], ua: [0; RING], at: 0, n: 0 }
    }

    pub fn push(&mut self, r: u64, unattributed: i64) {
        self.r[self.at] = (r / MIB).min(u32::MAX as u64) as u32;
        self.ua[self.at] = (unattributed / MIB as i64).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        self.at = (self.at + 1) % RING;
        self.n = (self.n + 1).min(RING);
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    fn back(&self, i: usize) -> u32 {
        self.r[(self.at + RING - 1 - i) % RING]
    }

    /// The p99 rise over windows of `h_ms`, at least max(`rise_floor_min`, B / `rise_floor_div` per s x h); else `prior`.
    pub fn rise99(&self, h_ms: u64, b: u64) -> Rise99 {
        if self.n < RING {
            return Rise99 { bytes: prior(h_ms, b), prior: true };
        }
        let k = ((h_ms / SAMPLE_MS) as usize).clamp(1, RING - 1);
        let mut rises = [0u32; RING];
        let m = RING - k;
        for (i, slot) in rises.iter_mut().enumerate().take(m) {
            *slot = self.back(i).saturating_sub(self.back(i + k));
        }
        let rises = &mut rises[..m];
        rises.sort_unstable();
        let p99 = rises[(m * 99 / 100).min(m - 1)] as u64 * MIB;
        let floor = get(K::RiseFloorMin).max(b / get(K::RiseFloorDiv).max(1) * h_ms.max(SAMPLE_MS) / 1000);
        Rise99 { bytes: p99.max(floor), prior: false }
    }

    /// D4389 (the ladder's measured margin): the p99 rise (bytes) of R over windows of `h_ms` among the samples held, with
    /// no prior and no floor. With less than `h_ms` of history the windows are as long as the history and the rise is
    /// scaled up to `h_ms`; `None` under `RISE_MIN_SAMPLES` samples. No allocation (one stack array of the ring's size).
    pub fn rise_p99_scaled(&self, h_ms: u64) -> Option<u64> {
        if self.n < RISE_MIN_SAMPLES {
            return None;
        }
        let k = ((h_ms / SAMPLE_MS) as usize).clamp(1, RING - 1);
        let kk = k.min(self.n - 1);
        let m = self.n - kk;
        let mut rises = [0u32; RING];
        for (i, slot) in rises.iter_mut().enumerate().take(m) {
            *slot = self.back(i).saturating_sub(self.back(i + kk));
        }
        let rises = &mut rises[..m];
        rises.sort_unstable();
        let p99 = rises[(m * 99 / 100).min(m - 1)] as u64 * MIB;
        Some(p99.saturating_mul(k as u64) / kk as u64)
    }

    pub fn u_band(&self, b: u64) -> u64 {
        if self.n < RING {
            return get(K::RiseFloorMin).max(b / get(K::RiseFloorDiv).max(1));
        }
        let mean = self.ua.iter().map(|&v| v as i64).sum::<i64>() / RING as i64;
        let mut dev = [0u32; RING];
        for (d, &v) in dev.iter_mut().zip(self.ua.iter()) {
            *d = (v as i64 - mean).unsigned_abs().min(u32::MAX as u64) as u32;
        }
        dev.sort_unstable();
        dev[RING * 90 / 100] as u64 * MIB
    }
}

/// rise99's cold-start prior (condition 4, never 0): through `prior_100ms_pct` of B at 100 ms, `prior_1s_pct` at 1 s,
/// then `prior_per_s_pct` per s, at most B.
pub fn prior(h_ms: u64, b: u64) -> u64 {
    let (p100, p1s, per_s) = (get(K::Prior100msPct) as u128, get(K::Prior1sPct) as u128, get(K::PriorPerSPct) as u128);
    let h = h_ms.max(1) as u128;
    let milli = if h <= SAMPLE_MS as u128 {
        p100 * 1000 * h / SAMPLE_MS as u128
    } else if h < 1000 {
        p100 * 1000 + p1s.saturating_sub(p100) * 1000 * (h - SAMPLE_MS as u128) / (1000 - SAMPLE_MS as u128)
    } else {
        p1s * 1000 + per_s * (h - 1000)
    };
    ((b as u128 * milli / 100_000).min(b as u128) as u64).max(get(K::RiseFloorMin))
}

/// One decide's snapshot: L the kill limit, B the budget, R governed resident, U the unattributed band, the fast rungs'
/// landed bytes, pending bytes not landed, rise99 at the sampler's period and at the cancel's landing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Inputs {
    pub l: u64,
    pub b: u64,
    pub r: u64,
    pub u: u64,
    pub fast: u64,
    pub pending: u64,
    pub rise_fast: Rise99,
    pub rise_slow: Rise99,
    pub holders: u32,
    pub revoke_on: bool,
    pub falling: bool,
}

impl Inputs {
    pub fn free(&self) -> u64 {
        self.b.saturating_sub(self.r).saturating_add(self.fast)
    }

    pub fn target(&self) -> u64 {
        self.rise_slow.bytes.saturating_add(self.u)
    }

    pub fn margin(&self) -> u64 {
        self.u.saturating_add(self.rise_fast.bytes)
    }

    pub fn crosses(&self) -> bool {
        self.relief_pending().projected(self.r.saturating_add(self.rise_slow.bytes)) > self.l.saturating_sub(self.margin())
    }

    /// H4: the governor's pending bytes (D4023) as relief (`crate::relief`).
    fn relief_pending(&self) -> crate::relief::Relief {
        crate::relief::Relief::of(crate::relief::Source::Asked, self.pending)
    }

    pub fn share(&self) -> u64 {
        self.b / self.holders.max(1) as u64
    }

    /// D3941: relief is landing (pending bytes, or R `falling`): a start waits for nothing else.
    pub fn relief(&self) -> bool {
        !self.relief_pending().is_none() || self.falling
    }

    /// The bytes the projection is past L - margin: what a cancel must take out of R.
    pub fn overshoot(&self) -> u64 {
        self.relief_pending().projected(self.r.saturating_add(self.rise_slow.bytes)).saturating_sub(self.l.saturating_sub(self.margin()))
    }
}

/// D3941: under projection the backstop's victim holds at least R - B (the allocator keeps a large cancel's pages, so R
/// stays high with every statement small), `cancel_min` alone once R is past L - margin.
pub fn backstop_least(i: &Inputs, resident: u64) -> u64 {
    let min = get(K::CancelMin).max(1);
    if resident > i.l.saturating_sub(i.margin()) {
        min
    } else {
        min.max(resident.saturating_sub(i.b))
    }
}

/// D3941: R's fall from `prev` to `now` in one tick, kept up for `throttle_max_ms`, closes `shortfall`.
pub fn falling(prev: u64, now: u64, shortfall: u64) -> bool {
    let ticks = (get(K::ThrottleMaxMs) / SAMPLE_MS).max(1);
    let fell = prev.saturating_sub(now);
    fell != 0 && fell.saturating_mul(ticks) >= shortfall
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Grow { held: u64 },
    Start,
    Tick,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cancel {
    #[default]
    None,
    Victim,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub reclaim_fast: bool,
    pub throttle: bool,
    pub spill_self: bool,
    pub cancel: Cancel,
    /// F* - F for Revoke (C5) while F < F* and it is on; 0 otherwise.
    pub revoke: u64,
    pub tranche: u64,
    pub reason: &'static str,
}

pub fn decide(ev: Event, i: &Inputs) -> Plan {
    let (f, target) = (i.free(), i.target());
    let short = f < target;
    let empty = i.b <= i.r && i.fast == 0;
    let mut p = Plan { reclaim_fast: short && i.fast != 0, tranche: tranche(f, i.holders), ..Plan::default() };
    if short && i.revoke_on && !matches!(ev, Event::Start) {
        p.revoke = target - f;
    }
    if empty && i.crosses() && matches!(ev, Event::Grow { .. } | Event::Tick) {
        p.cancel = Cancel::Victim;
        p.reason = "F = 0 and R + rise99(cancel landing) - pending > L - margin";
    }
    match ev {
        Event::Grow { held } => {
            if empty {
                p.spill_self = true;
                if p.reason.is_empty() {
                    p.reason = "F = 0: every grow takes its own road";
                }
            } else if short && held > i.share() {
                p.spill_self = true;
                p.reason = "F < F* and past its fair share";
            } else if p.reason.is_empty() {
                p.reason = if short { "F < F* within its fair share" } else { "F >= F*" };
            }
        }
        Event::Start => {
            p.throttle = short && i.relief();
            p.reason = if !short {
                "F >= F*"
            } else if p.throttle {
                "F < F* and relief is landing: the start waits"
            } else {
                "F < F* and nothing is landing: the start goes on"
            };
        }
        Event::Tick => {
            if p.reason.is_empty() {
                p.reason = if short { "F < F*" } else { "F >= F*" };
            }
        }
    }
    p
}

/// Whether decide(Start)'s throttle holds a start asking `ask`: an ask within the grant floor never waits, since holding
/// it frees nothing the projection is short of.
pub fn start_holds(throttled: bool, ask: u64) -> bool {
    throttled && ask > crate::admit::GRANT_FLOOR
}

/// How long a held start waits: its caller's `bound`, at most `throttle_max_ms`.
pub fn throttle_cap(bound: core::time::Duration) -> core::time::Duration {
    bound.min(crate::consts::get_ms(K::ThrottleMaxMs))
}

/// T, the credit between checks: clamp(F / (`tranche_k` x holders), `tranche_min`, `tranche_max`).
pub fn tranche(f: u64, holders: u32) -> u64 {
    let k = get(K::TrancheK).max(1).saturating_mul(holders.max(1) as u64);
    (f / k).clamp(get(K::TrancheMin), get(K::TrancheMax).max(get(K::TrancheMin)))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cand {
    pub held: u64,
    pub elapsed_ms: u64,
    pub idle: bool,
    pub canceled: bool,
}

/// The statement furthest over `share`, ties to the youngest; with none over it the one holding the most (at least
/// `cancel_min` and `least`, D3934 / D3941). Never an idle holder or a condemned one.
pub fn victim(cands: impl IntoIterator<Item = Cand>, share: u64, least: u64) -> Option<usize> {
    let least = get(K::CancelMin).max(1).max(least);
    let (mut over_best, mut held_best): (Option<(usize, u64, u64)>, Option<(usize, u64, u64)>) = (None, None);
    let better = |best: Option<(usize, u64, u64)>, v: u64, e: u64| !matches!(best, Some((_, o, b)) if v < o || v == o && e >= b);
    for (n, c) in cands.into_iter().enumerate() {
        if c.idle || c.canceled {
            continue;
        }
        if c.held > share && better(over_best, c.held - share, c.elapsed_ms) {
            over_best = Some((n, c.held - share, c.elapsed_ms));
        }
        if c.held >= least && better(held_best, c.held, c.elapsed_ms) {
            held_best = Some((n, c.held, c.elapsed_ms));
        }
    }
    over_best.or(held_best).map(|(n, _, _)| n)
}

pub fn record(ev: Event, i: &Inputs, p: &Plan) -> String {
    let src = |r: Rise99| if r.prior { "prior" } else { "ring" };
    format!( // alloc: Cold
        "event={} R={} L={} B={} U={} fast={} pending={} holders={} revoke_on={} falling={} rise99_fast={}:{} rise99_slow={}:{} F={} F*={} reclaim_fast={} throttle={} spill_self={} revoke={} cancel={} tranche={} reason={}",
        match ev {
            Event::Grow { held } => format!("grow:{held}"), // alloc: Cold
            Event::Start => "start".to_string(),            // alloc: Cold
            Event::Tick => "tick".to_string(),              // alloc: Cold
        },
        i.r,
        i.l,
        i.b,
        i.u,
        i.fast,
        i.pending,
        i.holders,
        i.revoke_on,
        i.falling,
        i.rise_fast.bytes,
        src(i.rise_fast),
        i.rise_slow.bytes,
        src(i.rise_slow),
        i.free(),
        i.target(),
        p.reclaim_fast,
        p.throttle,
        p.spill_self,
        p.revoke,
        matches!(p.cancel, Cancel::Victim),
        p.tranche,
        p.reason,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn inputs(r: u64) -> Inputs {
        let rise = |b| Rise99 { bytes: b, prior: false };
        Inputs { l: 16 * GIB, b: 14 * GIB, r, u: 256 * MIB, fast: 0, pending: 0, rise_fast: rise(512 * MIB), rise_slow: rise(3 * GIB), holders: 8, revoke_on: false, falling: false }
    }

    #[test]
    fn the_default_rule_is_todays_threshold() {
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        assert_eq!(rule(), Rule::Threshold);
        assert!(!projection());
        assert_eq!((Rule::from_index(0), Rule::from_index(1), Rule::from_index(7)), (Rule::Threshold, Rule::Projection, Rule::Threshold));
        set_rule(Rule::Projection);
        assert!(projection());
        set_rule(Rule::Threshold);
    }

    #[test]
    fn grow_and_start_follow_the_fast_buffer() {
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        let roomy = inputs(4 * GIB);
        assert_eq!(decide(Event::Grow { held: 8 * GIB }, &roomy).spill_self, false);
        assert!(!decide(Event::Start, &roomy).throttle);
        let mut short = inputs(12 * GIB); // F = 2 GiB < F* = 3.25 GiB
        assert!(!decide(Event::Start, &short).throttle, "D3941: nothing is landing, so the start does not wait");
        short.pending = GIB;
        assert!(decide(Event::Start, &short).throttle, "a cancel's bytes are landing: the start waits for them");
        (short.pending, short.falling) = (0, true);
        assert!(decide(Event::Start, &short).throttle, "R is falling: the start waits");
        let floor = crate::admit::GRANT_FLOOR;
        assert!(!start_holds(true, floor) && start_holds(true, floor + 1) && !start_holds(false, GIB));
        short.falling = false;
        assert!(!decide(Event::Grow { held: GIB }, &short).spill_self, "within its share (14 / 8 GiB)");
        assert!(decide(Event::Grow { held: 2 * GIB }, &short).spill_self, "past its share");
        let empty = inputs(14 * GIB);
        assert!(decide(Event::Grow { held: MIB }, &empty).spill_self);
    }

    #[test]
    fn a_cancel_needs_an_empty_buffer_and_a_crossing() {
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        let mut i = inputs(14 * GIB); // R = B; R + 3 GiB = 17 GiB > 16 - 0.75 GiB
        assert_eq!(decide(Event::Tick, &i).cancel, Cancel::Victim);
        assert_eq!(decide(Event::Grow { held: 0 }, &i).cancel, Cancel::Victim);
        assert_eq!(decide(Event::Start, &i).cancel, Cancel::None, "a start throttles, never cancels");
        i.pending = 2 * GIB; // 17 - 2 = 15 GiB < 15.25 GiB
        assert_eq!(decide(Event::Tick, &i).cancel, Cancel::None, "pending bytes land first");
        let mut j = inputs(14 * GIB);
        j.fast = 64 * MIB;
        let p = decide(Event::Tick, &j);
        assert_eq!((p.cancel, p.reclaim_fast), (Cancel::None, true), "F > 0: the fast rungs run, no cancel");
        let mut k = inputs(14 * GIB);
        k.rise_slow.bytes = 256 * MIB; // 14.25 GiB < 15.25
        assert_eq!(decide(Event::Tick, &k).cancel, Cancel::None, "the projection stays under L - margin");
    }

    #[test]
    fn the_sampler_quickens_past_half_of_b_under_projection_only() {
        assert_eq!((sample_period_ms(true, 51, 100), sample_period_ms(true, 50, 100), sample_period_ms(false, 99, 100), sample_period_ms(true, 9, 0)), (10, 100, 100, 100));
    }

    #[test]
    fn falling_closes_the_gap_within_the_wait() {
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        assert!(falling(10 * GIB, 10 * GIB - 200 * MIB, 3 * GIB), "200 MiB per tick x 20 >= 3 GiB");
        assert!(!falling(10 * GIB, 10 * GIB - 100 * MIB, 3 * GIB), "100 MiB per tick x 20 < 3 GiB");
        assert!(!falling(10 * GIB, 10 * GIB, 0) && !falling(10 * GIB, 11 * GIB, GIB), "flat or rising: never");
    }

    #[test]
    fn the_backstop_takes_no_small_statement_unless_the_kill_is_next() {
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        let i = inputs(14 * GIB); // L 16 GiB, B 14 GiB, margin 0.75 GiB
        assert_eq!(backstop_least(&i, 14 * GIB + 512 * MIB), 512 * MIB, "R past B by 512 MiB");
        assert_eq!(backstop_least(&i, 14 * GIB), 32 * MIB, "cancel_min at least");
        assert_eq!(backstop_least(&i, 15 * GIB + 512 * MIB), 32 * MIB, "past L - margin: any statement past cancel_min");
        let mut j = inputs(14 * GIB); // R + rise99 3 GiB - L + margin 0.75 GiB = 1.75 GiB
        assert_eq!(j.overshoot(), GIB + 768 * MIB);
        j.pending = 2 * GIB;
        assert_eq!(j.overshoot(), 0);
    }

    /// D4389: the ring's p99 rise over a window, no prior and no floor: none before a second of samples; a flat R rises 0;
    /// a steady 10 MiB per sample rises 10 MiB x the window's samples, scaled up from the shorter windows while the history
    /// is shorter than the window; a single sample's step stays under the p99 once it is under 1 % of the windows.
    #[test]
    fn d4389_rise_p99_is_the_measured_rise_over_the_window() {
        let mut ring = Rise::new();
        for _ in 0..RISE_MIN_SAMPLES - 1 {
            ring.push(GIB, 0);
        }
        assert_eq!(ring.rise_p99_scaled(2850), None, "under a second of samples");
        ring.push(GIB, 0);
        assert_eq!(ring.rise_p99_scaled(2850), Some(0), "flat");
        let mut g = Rise::new();
        for i in 0..20u64 {
            g.push(GIB + i * 10 * MIB, 0);
        }
        assert_eq!(g.rise_p99_scaled(2800), Some(280 * MIB), "19 samples of history, scaled to the window's 28");
        for i in 20..100u64 {
            g.push(GIB + i * 10 * MIB, 0);
        }
        assert_eq!(g.rise_p99_scaled(2800), Some(280 * MIB), "the whole window held");
        let mut b = Rise::new();
        for _ in 0..RING {
            b.push(GIB, 0);
        }
        b.push(2 * GIB, 0);
        assert_eq!(b.rise_p99_scaled(100), Some(0), "one 1 GiB step in 599 one-sample windows: under the p99");
    }

    #[test]
    fn tranches_shrink_with_free_space() {
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        assert_eq!(tranche(8 * GIB, 16), 128 * MIB);
        assert_eq!(tranche(2 * GIB, 16), 32 * MIB);
        assert_eq!(tranche(512 * MIB, 16), 8 * MIB);
        assert_eq!(tranche(0, 16), MIB, "never under 1 MiB");
        assert_eq!(tranche(u64::MAX, 1), 256 * MIB, "never over 256 MiB");
    }

    #[test]
    fn the_victim_is_the_furthest_over_its_share() {
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        let c = |held, elapsed_ms, idle, canceled| Cand { held, elapsed_ms, idle, canceled };
        let share = GIB;
        assert_eq!(victim([c(2 * GIB, 10, false, false), c(3 * GIB, 10, false, false)], share, 0), Some(1));
        assert_eq!(victim([c(3 * GIB, 50, false, false), c(3 * GIB, 10, false, false)], share, 0), Some(1), "ties: the youngest");
        assert_eq!(victim([c(9 * GIB, 10, true, false), c(2 * GIB, 10, false, false)], share, 0), Some(1), "an idle holder is skipped");
        assert_eq!(victim([c(9 * GIB, 10, false, true), c(2 * GIB, 10, false, false)], share, 0), Some(1), "a condemned one is skipped");
        assert_eq!(victim([c(GIB, 10, false, false), c(GIB / 2, 5, false, false)], share, 0), Some(0), "none past its share: the most held");
        assert_eq!(victim([c(GIB, 10, true, false), c(GIB, 10, false, true)], share, 0), None, "idle or condemned: never");
        assert_eq!(victim([c(1, 10, false, false)], share, 0), None, "under cancel_min: none");
        assert_eq!(victim([c(GIB, 10, false, false)], share, 2 * GIB), None, "D3941: under the overshoot, none past its share: none");
        assert_eq!(victim([c(2 * GIB, 10, false, false)], GIB, 4 * GIB), Some(0), "past its share: the overshoot does not apply");
    }

    #[test]
    fn rise99_is_the_prior_then_the_ring() {
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        let b = 14 * GIB;
        let mut r = Rise::new();
        assert_eq!(r.rise99(1000, b), Rise99 { bytes: prior(1000, b), prior: true });
        assert!(prior(100, b) > 0 && prior(1000, b) > prior(100, b) && prior(10_000, b) <= b);
        assert!((1..20_000).step_by(37).all(|h| prior(h + 37, b) >= prior(h, b)), "never falls as h grows");
        assert_eq!(prior(1000, b), b * 35 / 100);
        assert_eq!(prior(5000, b), b * 55 / 100, "sub-linear past 1 s");
        assert_eq!(prior(100, b), b * 9 / 100);
        for s in 0..RING as u64 {
            r.push(GIB + s * 100 * MIB, 0);
        }
        let x = r.rise99(1000, b);
        assert!(!x.prior);
        assert_eq!(x.bytes, 1000 * MIB);
        let mut f = Rise::new();
        for _ in 0..RING {
            f.push(4 * GIB, 0);
        }
        assert_eq!(f.rise99(1000, b).bytes, (64 * MIB).max(b / 64));
        assert_eq!(f.u_band(b), 0, "no unattributed movement");
    }

    #[test]
    fn a_held_start_waits_its_bound_at_most_throttle_max_ms() {
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        crate::consts::reset();
        let ms = core::time::Duration::from_millis;
        assert_eq!((throttle_cap(ms(500)), throttle_cap(ms(60_000))), (ms(500), ms(2_000)));
        crate::consts::set(K::ThrottleMaxMs, 100);
        assert_eq!(throttle_cap(ms(500)), ms(100));
        crate::consts::reset();
    }
}

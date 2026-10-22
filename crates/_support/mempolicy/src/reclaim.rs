use memledger::Consumer;

/// A rung of the reclaimer's road: parked regions, parked slot sets but the newest (D3169), managed caches to their keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rung {
    Regions,
    Slots,
    Caches,
}

/// Cheapest refill first; never shared_buffers, never a running statement's bytes.
pub const RUNGS: [Rung; 3] = [Rung::Regions, Rung::Slots, Rung::Caches];

/// Cheapest refill first, but the part cache before the condition cache and fp tables (after `over_goal`'s pass).
pub const ORDER: [Consumer; 5] = [Consumer::BuildPool, Consumer::ZoneStore, Consumer::PartCache, Consumer::CondCache, Consumer::FpTables];

/// Broker (b): those with a published loss re-ranked lowest first in their own places; the rest (all, cold) keep theirs.
pub fn by_loss<const N: usize>(order: [Consumer; N], loss: impl Fn(Consumer) -> Option<u64>) -> [Consumer; N] {
    let mut out = order;
    let (mut at, mut ranked, mut n) = ([0usize; N], [(0u64, 0usize, order[0]); N], 0);
    for (i, &c) in order.iter().enumerate() {
        if let Some(l) = loss(c) {
            (at[n], ranked[n], n) = (i, (l, i, c), n + 1);
        }
    }
    ranked[..n].sort_unstable_by_key(|r| (r.0, r.1));
    for j in 0..n {
        out[at[j]] = ranked[j].2;
    }
    out
}

pub const CACHES: [Consumer; 5] = [Consumer::FpTables, Consumer::BuildPool, Consumer::ZoneStore, Consumer::CondCache, Consumer::PartCache];

/// D3992: the walk frees managed caches, and under the watchdog's backstop every cache, in every broker mode.
#[inline]
pub fn cache_in_reach(managed: bool, backstop: bool) -> bool {
    managed || backstop
}

/// The cache rung's first pass: a cache's bytes past its own setting (`cap`), never under its floor; `None`: none.
#[inline]
pub fn over_goal(cur: u64, floor: u64, cap: u64, left: u64) -> Option<u64> {
    let goal = cur.saturating_sub(left).max(cap.max(floor));
    (goal < cur).then_some(goal)
}

/// Never under its keep (floor with `burst`, else max(target, floor); no target: what it holds); `None`: frees nothing.
#[inline]
pub fn cache_goal(cur: u64, floor: u64, target: Option<u64>, burst: bool, left: u64) -> Option<u64> {
    let keep = if burst { floor } else { target.unwrap_or(cur).max(floor) };
    let goal = cur.saturating_sub(left).max(keep);
    (goal < cur).then_some(goal)
}

#[inline]
pub fn below_low(free_b: u64, r_low: u64) -> bool {
    free_b < r_low
}

#[inline]
pub fn low_want(free_b: u64, r_high: u64) -> u64 {
    r_high.saturating_sub(free_b)
}

#[inline]
pub fn burst_want(free: u64, demand: u64) -> Option<u64> {
    (demand > free).then(|| demand - free)
}

#[inline]
pub fn inline_want(short: u64, r_low: u64, max: u64) -> u64 {
    short.saturating_add(r_low).min(max)
}

#[inline]
pub fn inline_burst_want(bytes: u64, free: u64, left: u64) -> Option<u64> {
    (free < bytes && left != 0).then(|| (bytes - free).min(left))
}

/// D4314 (b): the breaker a revoke cut lands on (the kind byte of the region the participant fills).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CutKind {
    /// A hash aggregate's phase-1 table (T1): its flush scatters the table and gives the pool's growth back
    /// (driver `give_back_scattered`).
    HashAggT1,
    HashAgg,
    /// A lowered (kind-7) hash build: cut, it spills down to the length it opened at and gives the rest back (driver
    /// `give_back_build`, D4265). The hand kernels' build (`TABLE_BUILD`) reads `Other`.
    HashBuild,
    /// A sort's run: cut, it stops growing but keeps its region's pages until its statement ends (no give-back road
    /// yet; lifetime's sort give-back adds one, then this kind gives).
    Sort,
    Owned,
    Other,
}

/// D4314 (b) (memlead's ruling, approved): an ask counts only by the bytes it gives back, so a breaker whose cut keeps its
/// pages is no revoke candidate; its spillable bytes are the ones a statement offers (`RevokeCand::spillable`), and such a
/// kind offers none, so the ask moves to the next largest. The ruling names the cut sort (it keeps its region's pages
/// until its statement ends). Narrowed after D4265: only the kinds whose cut gives pages back are candidates, the phase-1
/// hash aggregate (driver `give_back_scattered`) and the kind-7 build (driver `give_back_build`). The sort, the table
/// aggregate (no lowered spilling one: the hand kernels'), the owned tables and a region whose table is not laid out yet
/// (`Other`: its bytes are noted once its kind is known, at its next room ask) offer none.
pub fn cut_gives_back(k: CutKind) -> bool {
    matches!(k, CutKind::HashAggT1 | CutKind::HashBuild)
}

/// C5: a running statement the spill-request registry (`mcx::revoke`) offers as a revoke's victim.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RevokeCand {
    pub slot: usize,
    /// Bytes it holds in breaker regions that can spill now (a fill phase's region with a spill road) and give their pages
    /// back when cut (D4314 (b): `cut_gives_back`).
    pub spillable: u64,
    pub held: u64,
    /// Input consumed, 0..1024 (0: not tracked).
    pub progress: u16,
    /// When it was last asked (the registry's ms clock); 0 = never.
    pub revoked_ms: u64,
    pub revokes: u32,
    /// It waits for memory itself (a waiter is never a victim: no cycles).
    pub waiting: bool,
    /// Ladder 1.3 (`fair_share`): its fair share (`fair_level`, at least its work_mem); 0 = none set.
    pub share: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RevokeAsk {
    pub slot: usize,
    pub bytes: u64,
}

/// Whether `c` may be asked at `now_ms`: it holds at least `revoke_floor` in spillable breakers, waits for nothing, was
/// not asked within `revoke_quiet_ms` nor `revoke_max` times, and is not nearly done (`revoke_progress_max`).
fn revocable(c: &RevokeCand, now_ms: u64) -> bool {
    use crate::consts::{get, K};
    c.spillable >= get(K::RevokeFloor).max(1)
        && !c.waiting
        && (c.revoked_ms == 0 || now_ms.saturating_sub(c.revoked_ms) >= get(K::RevokeQuietMs))
        && u64::from(c.revokes) < get(K::RevokeMax)
        && u64::from(c.progress) < get(K::RevokeProgressMax)
}

/// `a` before `b`: furthest over its fair `share`, then more spillable bytes (cheaper per byte), least progress, lower slot.
fn before(a: &RevokeCand, b: &RevokeCand, share: u64) -> bool {
    let over = |c: &RevokeCand| c.held.saturating_sub(share);
    (over(b), b.spillable, a.progress, a.slot) < (over(a), a.spillable, b.progress, b.slot)
}

/// C5: who spills how much so `short` bytes come back, in `out` (the count returned): the `revocable` candidates in
/// `before`'s order, each min(its spillable / `revoke_frac_div` from `revoke_min` on, else all of it, what is short).
/// The caller leaves out the requester.
pub fn revoke_asks(short: u64, share: u64, now_ms: u64, cands: &[RevokeCand], out: &mut [RevokeAsk]) -> usize {
    revoke_asks_after(short, share, now_ms, cands, out, 0)
}

/// `revoke_asks` with `out[..n0]` asked already: those candidates are left out and the asks follow them.
fn revoke_asks_after(short: u64, share: u64, now_ms: u64, cands: &[RevokeCand], out: &mut [RevokeAsk], n0: usize) -> usize {
    use crate::consts::{get, K};
    let (div, min) = (get(K::RevokeFracDiv).max(1), get(K::RevokeMin));
    let (mut left, mut n) = (short, n0);
    while left != 0 && n < out.len() {
        let asked = |c: &RevokeCand| out[..n].iter().any(|a| a.slot == c.slot);
        let mut best: Option<&RevokeCand> = None;
        for c in cands.iter().filter(|c| revocable(c, now_ms) && !asked(c)) {
            if best.is_none_or(|b| before(c, b, share)) {
                best = Some(c);
            }
        }
        let Some(c) = best else { break };
        let bytes = (if c.spillable >= min { (c.spillable / div).max(1) } else { c.spillable }).min(left);
        out[n] = RevokeAsk { slot: c.slot, bytes };
        left -= bytes;
        n += 1;
    }
    n
}

/// Ladder 1.3 (`fair_share`, the user's approval 10-16): the max-min level over the running queries' `held` bytes in
/// `total`: every query holding less than the level keeps what it holds, and the queries above it split what is left
/// evenly. `u64::MAX` when they all fit. `held` is sorted in place (the caller's scratch; no allocation).
pub fn fair_level(held: &mut [u64], total: u64) -> u64 {
    held.sort_unstable();
    if held.iter().fold(0u64, |s, &h| s.saturating_add(h)) <= total {
        return u64::MAX;
    }
    let mut left = total;
    for (i, &h) in held.iter().enumerate() {
        let k = (held.len() - i) as u64;
        if h.saturating_mul(k) > left {
            return left / k;
        }
        left -= h;
    }
    0
}

/// Ladder 1.3: a query's fair share at `level`: never under its `floor` (its work_mem).
pub fn fair_share(level: u64, floor: u64) -> u64 {
    level.max(floor)
}

/// Ladder 1.3-1.4 (`fair_share`): who spills how much so `short` bytes come back, in `out` (the count returned): only
/// the queries over their own fair share (`RevokeCand::share`), `revocable` as today, furthest over first; each asked for
/// what is still short, never more than its part over its share nor than its spillable bytes ("give back N"; a query
/// within its share is never asked so another can run). The caller leaves out the requester.
pub fn revoke_asks_fair(short: u64, now_ms: u64, cands: &[RevokeCand], out: &mut [RevokeAsk]) -> usize {
    let over = |c: &RevokeCand| c.held.saturating_sub(c.share);
    let (mut left, mut n) = (short, 0);
    while left != 0 && n < out.len() {
        let asked = |c: &RevokeCand| out[..n].iter().any(|a| a.slot == c.slot);
        let mut best: Option<&RevokeCand> = None;
        for c in cands.iter().filter(|c| c.share != 0 && over(c) != 0 && revocable(c, now_ms) && !asked(c)) {
            if best.is_none_or(|b| (over(b), b.spillable, c.progress, c.slot) < (over(c), c.spillable, b.progress, b.slot)) {
                best = Some(c);
            }
        }
        let Some(c) = best else { break };
        let bytes = over(c).min(c.spillable).min(left);
        out[n] = RevokeAsk { slot: c.slot, bytes };
        left -= bytes;
        n += 1;
    }
    n
}

/// Ladder 1.3-1.4, kill-avoidance (the one exception the user's approval names: a query within its share is revoked only
/// to avoid a kernel kill): `revoke_asks_fair`'s asks first, then, for what is still short, today's rule (`revoke_asks`)
/// over the queries not asked yet, within their share too. The callers are the ones that cancel next (the backstop past
/// its share of the limit, the projection's act); the ladder's own revoke rung uses `revoke_asks_fair` alone.
pub fn revoke_asks_fair_or_kill(short: u64, share: u64, now_ms: u64, cands: &[RevokeCand], out: &mut [RevokeAsk]) -> usize {
    let k = revoke_asks_fair(short, now_ms, cands, out);
    let asked = out[..k].iter().fold(0u64, |s, a| s.saturating_add(a.bytes));
    revoke_asks_after(short.saturating_sub(asked), share, now_ms, cands, out, k)
}

#[cfg(test)]
mod tests {
    use super::{below_low, burst_want, by_loss, cache_goal, fair_level, fair_share, inline_burst_want, inline_want, low_want, over_goal, revoke_asks, revoke_asks_fair, revoke_asks_fair_or_kill, RevokeAsk, RevokeCand, ORDER};
    use memledger::Consumer;

    #[test]
    fn b2_the_walk_takes_the_lowest_published_loss_first_and_a_cold_start_keeps_order() {
        assert_eq!(by_loss(ORDER, |_| None), ORDER, "nothing published: the fixed order");
        let loss = |c| match c {
            Consumer::PartCache => Some(900),
            Consumer::CondCache => Some(5),
            Consumer::FpTables => Some(40),
            _ => None,
        };
        let want = [Consumer::BuildPool, Consumer::ZoneStore, Consumer::CondCache, Consumer::FpTables, Consumer::PartCache];
        assert_eq!(by_loss(ORDER, loss), want, "the measured three re-ranked in their own places; the build pool and zone store stay");
    }

    #[test]
    fn a_cache_past_its_setting_goes_first() {
        assert_eq!(over_goal(3000, 100, 1290, 1500), Some(1500), "what is asked, of its excess");
        assert_eq!(over_goal(3000, 100, 1290, 9000), Some(1290), "never under its setting in this pass");
        assert_eq!(over_goal(3000, 2000, 1290, 9000), Some(2000), "nor its floor");
        assert_eq!((over_goal(1024, 0, 1024, 500), over_goal(5, 0, 9, 5)), (None, None), "at or under its setting: none");
        let pos = |c| super::ORDER.iter().position(|o| *o == c);
        assert!(pos(memledger::Consumer::PartCache) < pos(memledger::Consumer::CondCache));
        assert!(pos(memledger::Consumer::PartCache) < pos(memledger::Consumer::FpTables));
    }

    #[test]
    fn the_reclaimer_asks_as_before() {
        assert_eq!(cache_goal(100, 10, Some(40), false, 1000), Some(40), "down to its target");
        assert_eq!(cache_goal(100, 50, Some(40), false, 1000), Some(50), "never under its floor");
        assert_eq!(cache_goal(100, 10, Some(40), true, 1000), Some(10), "burst: down to its floor");
        assert_eq!(cache_goal(100, 10, None, false, 1000), None, "no target yet: it keeps what it holds");
        assert_eq!(cache_goal(100, 10, Some(40), false, 30), Some(70), "only what is still wanted");
        assert!(below_low(9, 10) && !below_low(10, 10));
        assert_eq!((low_want(3, 10), low_want(30, 10)), (7, 0));
        assert_eq!((burst_want(10, 11), burst_want(10, 10)), (Some(1), None));
        assert_eq!((inline_want(5, 10, 100), inline_want(95, 10, 100)), (15, 100));
        assert_eq!((inline_burst_want(10, 4, 3), inline_burst_want(10, 10, 3), inline_burst_want(10, 4, 0)), (Some(3), None, None));
    }

    const MIB: u64 = 1 << 20;

    fn cand(slot: usize, spillable: u64, held: u64) -> RevokeCand {
        RevokeCand { slot, spillable, held, ..RevokeCand::default() }
    }

    #[test]
    fn revoke_asks_the_most_over_its_share_first_for_what_is_short() {
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        crate::consts::reset();
        let mut out = [RevokeAsk::default(); 4];
        let share = 1024 * MIB;
        let cands = [cand(1, 400 * MIB, 1500 * MIB), cand(2, 800 * MIB, 3000 * MIB), cand(3, 600 * MIB, 900 * MIB)];
        let n = revoke_asks(500 * MIB, share, 10_000, &cands, &mut out);
        assert_eq!(&out[..n], &[RevokeAsk { slot: 2, bytes: 400 * MIB }, RevokeAsk { slot: 1, bytes: 100 * MIB }], "furthest over its share first, half its spillable, then only the rest");
        let n = revoke_asks(10_000 * MIB, share, 10_000, &cands, &mut out);
        assert_eq!(n, 3, "every candidate once");
        assert_eq!(out[2], RevokeAsk { slot: 3, bytes: 300 * MIB }, "under its share: last");
        assert_eq!(revoke_asks(0, share, 10_000, &cands, &mut out), 0, "nothing short: no ask");
        assert_eq!(revoke_asks(MIB, share, 10_000, &cands, &mut []), 0, "no room for an ask");
        let never = [
            cand(4, 4 * MIB - 1, 4000 * MIB),
            RevokeCand { waiting: true, ..cand(5, 900 * MIB, 4000 * MIB) },
            RevokeCand { revoked_ms: 9_000, revokes: 1, ..cand(6, 900 * MIB, 4000 * MIB) },
            RevokeCand { revoked_ms: 1, revokes: 3, ..cand(7, 900 * MIB, 4000 * MIB) },
            RevokeCand { progress: 900, ..cand(8, 900 * MIB, 4000 * MIB) },
        ];
        assert_eq!(revoke_asks(10_000 * MIB, share, 10_000, &never, &mut out), 0, "under revoke_floor, waiting, quiet, past revoke_max, nearly done");
        let quiet_over = [RevokeCand { revoked_ms: 7_000, revokes: 1, ..cand(6, 900 * MIB, 4000 * MIB) }];
        assert_eq!(revoke_asks(MIB, share, 10_000, &quiet_over, &mut out), 1, "asked again once revoke_quiet_ms passed");
        let tie = [cand(9, 200 * MIB, 2000 * MIB), cand(10, 600 * MIB, 2000 * MIB)];
        assert_eq!(revoke_asks(MIB, share, 10_000, &tie, &mut out), 1);
        assert_eq!(out[0].slot, 10, "equally over: the larger spillable (cheaper per byte)");
    }

    /// D4314 (b): only a kind whose cut gives pages back is a revoke candidate: the phase-1 hash aggregate and (D4265) the
    /// kind-7 build; a cut sort keeps its region's pages until its give-back road exists (lifetime's).
    #[test]
    fn d4314_only_a_breaker_whose_cut_gives_pages_back_is_asked() {
        use super::{cut_gives_back, CutKind::*};
        assert!(cut_gives_back(HashAggT1), "the phase-1 hash aggregate gives its pool's growth back at its scatter");
        assert!(cut_gives_back(HashBuild), "D4265: a cut kind-7 build spills down to its opened length and gives the rest back");
        assert!(!cut_gives_back(Sort), "a cut sort keeps its region's pages: not asked");
        // D4314 (b), narrowed after D4265: the kinds that still keep their pages when cut offer nothing, nor does a region
        // whose table is not laid out yet (Other: its bytes are noted once its kind is known)
        for k in [HashAgg, Owned, Other] {
            assert!(!cut_gives_back(k), "{k:?}: its cut gives nothing back: not asked");
        }
    }

    /// D4314: at the default work_mem a statement's spillable bytes are W x 4 MiB, under `revoke_min`; such a statement
    /// is asked for all of them, one spill, and the walk goes on until what is short is covered.
    #[test]
    fn d4314_a_default_work_mem_statement_is_asked_for_its_whole_spillable() {
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        crate::consts::reset();
        let mut out = [RevokeAsk::default(); 16];
        let at = |slot, mib: u64| cand(slot, mib * MIB, mib * MIB);
        let cands = [at(1, 32), at(2, 32), at(3, 12), at(4, 2)];
        let n = revoke_asks(100 * MIB, 0, 10_000, &cands, &mut out);
        let want = [RevokeAsk { slot: 1, bytes: 32 * MIB }, RevokeAsk { slot: 2, bytes: 32 * MIB }, RevokeAsk { slot: 3, bytes: 12 * MIB }];
        assert_eq!(&out[..n], &want, "largest first, each its whole spillable; the 2 MiB one is under revoke_floor");
        assert_eq!(out[..n].iter().map(|a| a.bytes).sum::<u64>(), 76 * MIB);
        let n = revoke_asks(40 * MIB, 0, 10_000, &cands, &mut out);
        assert_eq!(&out[..n], &[RevokeAsk { slot: 1, bytes: 32 * MIB }, RevokeAsk { slot: 2, bytes: 8 * MIB }], "only what is still short");
        let n = revoke_asks(1000 * MIB, 0, 10_000, &[at(5, 200)], &mut out);
        assert_eq!(&out[..n], &[RevokeAsk { slot: 5, bytes: 100 * MIB }], "from revoke_min on: spillable / revoke_frac_div");
        let floor = [cand(6, 4 * MIB, 4 * MIB), cand(7, 4 * MIB - 1, 4 * MIB)];
        let n = revoke_asks(100 * MIB, 0, 10_000, &floor, &mut out);
        assert_eq!(&out[..n], &[RevokeAsk { slot: 6, bytes: 4 * MIB }], "revoke_floor is asked, a byte under it is not");
    }

    /// Ladder 1.3: the max-min level: the small queries keep what they hold, the big ones split the rest evenly; all fit,
    /// no level; the work_mem floor.
    #[test]
    fn ladder13_the_max_min_level_keeps_the_small_and_splits_the_rest() {
        assert_eq!(fair_level(&mut [100, 200, 300], 600), u64::MAX, "all fit: no level");
        assert_eq!(fair_level(&mut [], 600), u64::MAX);
        assert_eq!(fair_level(&mut [900, 100, 2000], 1000), 450, "100 keeps its 100; 900 and 2000 split the 900 left");
        assert_eq!(fair_level(&mut [500, 500, 500], 900), 300, "equal: an equal split");
        assert_eq!(fair_level(&mut [10, 20, 5000, 6000], 1030), 500);
        assert_eq!((fair_share(450, 64), fair_share(10, 64)), (450, 64), "never under work_mem");
    }

    /// Ladder 1.3-1.4: only the queries over their own share are asked, furthest over first, each for what is short and
    /// never past its part over its share or its spillable bytes; a waiter or a query within its share never.
    #[test]
    fn ladder14_a_revoke_asks_only_over_share_queries_for_bytes() {
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        crate::consts::reset();
        let mut out = [RevokeAsk::default(); 16];
        let fair = |slot, spill: u64, held: u64, share: u64| RevokeCand { share: share * MIB, ..cand(slot, spill * MIB, held * MIB) };
        let cands = [
            fair(1, 500, 900, 400),
            fair(2, 800, 1200, 400),
            fair(3, 300, 300, 400),
            RevokeCand { waiting: true, ..fair(4, 2000, 3000, 400) },
            fair(5, 100, 700, 400),
        ];
        let n = revoke_asks_fair(2000 * MIB, 10_000, &cands, &mut out);
        let want = [RevokeAsk { slot: 2, bytes: 800 * MIB }, RevokeAsk { slot: 1, bytes: 500 * MIB }, RevokeAsk { slot: 5, bytes: 100 * MIB }];
        assert_eq!(&out[..n], &want, "over share only, furthest first; each its part over its share, at most its spillable");
        let n = revoke_asks_fair(600 * MIB, 10_000, &cands, &mut out);
        assert_eq!(&out[..n], &[RevokeAsk { slot: 2, bytes: 600 * MIB }], "only what is still short");
        let n = revoke_asks_fair(600 * MIB, 10_000, &[fair(6, 900, 900, 0)], &mut out);
        assert_eq!(n, 0, "no share set: none");
        // kill-avoidance: the over-share asks, then today's rule for the rest over the others, within their share too
        // (3: 300 spillable, half of it; never the waiter); the over-share ones are not asked twice
        let n = revoke_asks_fair_or_kill(1600 * MIB, 0, 10_000, &cands, &mut out);
        let want = [
            RevokeAsk { slot: 2, bytes: 800 * MIB },
            RevokeAsk { slot: 1, bytes: 500 * MIB },
            RevokeAsk { slot: 5, bytes: 100 * MIB },
            RevokeAsk { slot: 3, bytes: 150 * MIB },
        ];
        assert_eq!(&out[..n], &want, "over share first, then within its share for what is short");
        let n = revoke_asks_fair_or_kill(600 * MIB, 0, 10_000, &cands, &mut out);
        assert_eq!(&out[..n], &[RevokeAsk { slot: 2, bytes: 600 * MIB }], "the over-share asks cover it: no one within its share");
    }
}

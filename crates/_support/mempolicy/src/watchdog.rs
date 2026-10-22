use crate::consts::{get, K};

/// One log volley per tier per excursion, strictly escalating.
pub fn tiers(base: i32) -> [u64; 3] {
    let t = base.clamp(1, 100) as u64;
    [t, t + (100 - t) / 2, t + (100 - t) * 3 / 4]
}

pub fn crossed(pct: u64, base: i32) -> u32 {
    tiers(base).iter().filter(|&&t| pct >= t).count() as u32
}

pub fn rearms(pct: u64, base: i32) -> bool {
    pct + get(K::WatchdogRearmPct) <= tiers(base)[0]
}

pub fn cancel_armed(pct: i32) -> bool {
    pct > 0
}

/// `None` under it or with no limit.
pub fn cancel_at(resident: u64, limit: u64, pct: i32) -> Option<(usize, usize)> {
    let p = pct.clamp(1, 100);
    if limit == 0 || resident < (limit / 100).saturating_mul(p as u64) {
        return None;
    }
    Some((p as usize, get(K::CancelMin) as usize))
}


/// Four sessions building 64 MB hash tables crossed a 512 MB limit inside one 1 s tick.
pub const GOVERNOR_CADENCE_MS: i64 = 100;

pub const BROKER_LIMIT_CADENCE_MS: i64 = 1000;

/// D4290: the kernel's throttling at a soft limit (memory.events 'high' rising) is warned at most once a minute.
pub const HIGH_WARN_EVERY_MS: u64 = 60_000;


/// One 2 MiB huge page; a smaller region's account counts it whole.
pub const RESIDENCY_MIN: usize = 2 << 20;

/// Past B / 2 the ladder sizes its rungs and picks victims from the accounts, so the pass runs at the pressed period.
pub const RESIDENCY_CADENCE_MS: i64 = 1000;
pub const RESIDENCY_PRESSED_MS: i64 = 200;

/// Re-reading unchanged, partly resident ranges keeps a pass under 0.25 ms at any shared_buffers (mincore: 1 GiB/ms).
pub const RESIDENCY_READ_BYTES: usize = 256 << 20;

#[inline]
pub fn residency_period_ms(resident: u64, b: u64) -> i64 {
    if b != 0 && resident > b / 2 {
        RESIDENCY_PRESSED_MS
    } else {
        RESIDENCY_CADENCE_MS
    }
}

/// A cancel unanswered this long no longer blocks the next one (16 GiB stress: 31 of 32 acks within 0.29 s).
pub const CANCEL_ACK_MS: u64 = 1000;

/// The next victim among the live statements `(bytes, canceled_ms)` (0 = not canceled): none while a cancel younger
/// than `CANCEL_ACK_MS` is pending; else the largest not yet canceled of at least `min` bytes, never chosen twice.
pub fn cancel_victim(stmts: impl IntoIterator<Item = (usize, u64)>, now_ms: u64, min: usize) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for (i, (bytes, canceled_ms)) in stmts.into_iter().enumerate() {
        if canceled_ms != 0 {
            if now_ms.saturating_sub(canceled_ms) < CANCEL_ACK_MS {
                return None;
            }
            continue;
        }
        if bytes >= min.max(1) && best.is_none_or(|(_, b)| bytes > b) {
            best = Some((i, bytes));
        }
    }
    best.map(|(i, _)| i)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backstop {
    pub trim: u64,
    pub revoke: u64,
    pub cancel: bool,
}

/// D3992: the rungs free the excess over `watchdog_rearm_pct` under `pct` (at most `reclaimable`), revokes ask the rest;
/// the cancel waits a sample only after a trim (`cancels`); after a trim the next sample past the line cancels, and the
/// top tier always does. D4314 (a): an ask never holds it.
pub fn backstop(resident: u64, limit: u64, pct: i32, top_pct: u64, reclaimable: u64, trimmed: bool) -> Option<Backstop> {
    let (p, _) = cancel_at(resident, limit, pct)?;
    let hundredth = limit / 100;
    let target = hundredth.saturating_mul((p as u64).saturating_sub(get(K::WatchdogRearmPct)));
    let excess = resident.saturating_sub(target);
    let trim = excess.min(reclaimable);
    let top = resident >= hundredth.saturating_mul(top_pct.max(p as u64));
    Some(Backstop { trim, revoke: excess - trim, cancel: trimmed || top })
}

/// Whether this backstop sample cancels. D4314 (a) (memlead's ruling, approved): the bytes asked of running statements
/// (`_asked`) are never relief: an ask sent does not hold the cancel, so with revoke on the cancel comes no later than
/// with the threshold alone. Relief is a measured drop only: resident under the line at the next sample (`backstop`
/// returns `None`). A trim's wait of one sample is the rungs' (they free at once), unchanged.
pub fn cancels(step: Backstop, _asked: u64) -> bool {
    step.cancel || step.trim == 0
}

/// D4262: the least share of the governed resident (the limiting cgroup's) this server's own view (its anon + shmem and
/// the cgroup's unmapped bytes) holds for its backstop to act on its own statements. CHOSEN, not measured (main
/// 2026-10-12T21:33:39Z): a quarter, so a server beside a sidecar or another server still acts as the big tenant.
pub const TENANT_PCT: u64 = 25;

/// D4262: whether this server's backstop acts (reclaims, asks spills, cancels) past the line: its own view (`own`) holds
/// at least `TENANT_PCT` of the governed `resident`, or its largest live statement (`largest`, as the victim pick ranks
/// it) alone holds the bytes over the line (`line`). Otherwise it is a bystander in a cgroup other processes fill: no
/// cancel of its statements relieves the overage, and the kernel's decision is the cgroup's.
pub fn acts(resident: u64, own: u64, largest: u64, line: u64) -> bool {
    own.saturating_mul(100) >= resident.saturating_mul(TENANT_PCT) || largest >= resident.saturating_sub(line)
}

/// D4023: no next cancel while the cancels landing (`pending`) cover the excess over the line, but at the top tier.
pub fn landing_holds(resident: u64, limit: u64, pct: i32, top_pct: u64, pending: u64) -> bool {
    let Some((p, _)) = cancel_at(resident, limit, pct) else { return false };
    let hundredth = limit / 100;
    let top = resident >= hundredth.saturating_mul(top_pct.max(p as u64));
    // H4: the backstop's own rule, not the ladder's (`crate::relief::Relief::backstop_covers`)
    !top && crate::relief::Relief::of(crate::relief::Source::Asked, pending).backstop_covers(resident.saturating_sub(hundredth.saturating_mul(p as u64)))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoVictim {
    Wait,
    Terminate,
    Smaller,
    Line,
}

/// D4029, no statement at `cancel_min`: `stalled` = the oldest cancel unanswered past `CANCEL_ACK_MS` (grew, ms since terminate).
pub fn no_victim(pending_ack: bool, stalled: Option<(bool, Option<u64>)>, top: bool) -> NoVictim {
    if pending_ack {
        return NoVictim::Wait;
    }
    match stalled {
        Some((true, None)) => return NoVictim::Terminate,
        Some((_, Some(t))) if t < CANCEL_ACK_MS => return NoVictim::Wait,
        _ => {}
    }
    if top { NoVictim::Smaller } else { NoVictim::Line }
}

pub const NO_VICTIM_LINE_MS: u64 = 1000;

/// D4092: the backends whose memory context trees a watchdog tier crossing dumps (`pgrust.memory_watchdog_dump`): the
/// would-be victim and the next two by the watchdog's rank (`rank_bytes`), not every live backend.
pub const DUMP_TOP: usize = 3;

/// D3992 / D3999: the ledger row's resident bytes plus the in-use `parts` it alone pins, never the charge (D3996) unless no
/// row. D4035: `row` = (resident, the statement's signed net); a net below its start (another account's bytes freed on it)
/// hides the statement, so it ranks by its charge plus `parts`.
pub fn rank_bytes(row: Option<(u64, i64)>, parts: u64, charged: u64) -> u64 {
    match row {
        None => charged,
        Some((_, net)) if net < 0 => charged.saturating_add(parts),
        Some((resident, _)) => resident.saturating_add(parts),
    }
}

/// C8 (D4087, the per-owner rank): the account an in-use part counts on in the ranking (`rank_bytes`'s `parts`), from its
/// users as (account, owner key, uses): the one account whose running owners (`running(account)` = (statement id, the
/// portal it executes), 0 = none) own every use, so canceling its statement lets the cache evict the part. A use its
/// session or a cache owns (a parked Node's kept cursor, D1507; a parked worker set's), or an owner that ended (an exited
/// backend's), frees on no cancel: such a part counts on no account.
pub fn cancel_frees(uses: &[(u32, u64, u32)], running: impl Fn(u32) -> (u64, u64)) -> Option<u32> {
    let &(acct, _, _) = uses.first()?;
    let (stmt, portal) = running(acct);
    uses.iter().all(|&(a, k, _)| a == acct && k != 0 && (k == stmt || k == portal)).then_some(acct)
}

pub fn resident_of(committed: u64, not_resident: i64) -> u64 {
    committed.saturating_sub(not_resident.max(0) as u64)
}

/// D4059: a statement's not-resident bytes are as old as the residency pass's last read of its ranges (an unchanged,
/// partly resident range is read again only every span / `RESIDENCY_READ_BYTES` passes: about 3 s for 4 GiB), so a region
/// that filled since that read hides up to its size from the rank. Read again, its rank is at most `unstale` (its rank
/// with none of its bytes not resident). A statement other than the pick goes to the residency thread to be read again
/// (D4812: never on the watchdog's thread) when that could change the pick: `unstale` is above the rank it has (`stale`)
/// and above `top`, the pick's rank (with no pick, one under the least it cancels).
pub fn stale_may_outrank(stale: u64, unstale: u64, top: u64) -> bool {
    unstale > stale && unstale > top
}

/// D4059 / D4812: at most this many statements one cancel's pick hands to the residency thread to read again (each read
/// is one mincore over the statement's ranges, about 1 GiB a ms, on that thread).
pub const STALE_REFRESH_MAX: usize = 4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pending_cancel_blocks_until_its_deadline_then_the_next_largest_goes() {
        const MIB: usize = 1 << 20;
        let t = 10_000;
        assert_eq!(cancel_victim([(64 * MIB, 0), (40 * MIB, 0)], t, 32 * MIB), Some(0));
        assert_eq!(cancel_victim([(64 * MIB, t), (40 * MIB, 0)], t + CANCEL_ACK_MS - 1, 32 * MIB), None, "still pending");
        assert_eq!(cancel_victim([(64 * MIB, t), (40 * MIB, 0)], t + CANCEL_ACK_MS, 32 * MIB), Some(1), "past it: the next");
        assert_eq!(cancel_victim([(64 * MIB, t), (16 * MIB, 0)], t + CANCEL_ACK_MS, 32 * MIB), None, "none past the least");
        assert_eq!(cancel_victim([(40 * MIB, 0), (64 * MIB, 1), (48 * MIB, 0)], t, 32 * MIB), Some(2), "never the condemned one");
        assert_eq!(cancel_victim(core::iter::empty(), t, 32 * MIB), None);
    }

    #[test]
    fn the_backstop_reclaims_before_it_cancels() {
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        let gib = 1u64 << 30;
        let limit = 16 * gib;
        let at = |pct: u64| limit / 100 * pct;
        assert_eq!(backstop(at(90) - 1, limit, 90, 95, gib, false), None);
        let over = at(91) - at(85);
        let s = |b: Option<Backstop>, asked: u64| b.map(|b| (b.trim, b.revoke, cancels(b, asked)));
        assert_eq!(s(backstop(at(91), limit, 90, 95, 10 * gib, false), 0), Some((over, 0, false)), "trim to 85 %, then wait");
        assert_eq!(s(backstop(at(91), limit, 90, 95, gib / 2, false), 0), Some((gib / 2, over - gib / 2, false)), "what the rungs hold");
        assert_eq!(s(backstop(at(91), limit, 90, 95, 0, false), 0), Some((0, over, true)), "nothing to reclaim or revoke");
        assert_eq!(s(backstop(at(91), limit, 90, 95, 0, false), over), Some((0, over, true)), "a revoke asked is no relief: cancel as with none (D4314 (a))");
        assert_eq!(s(backstop(at(91), limit, 90, 95, 10 * gib, true), 0), Some((over, 0, true)), "past after a trim or an ask");
        assert_eq!(s(backstop(at(95), limit, 90, 95, 0, false), at(95) - at(85)), Some((0, at(95) - at(85), true)), "top tier: all");
        assert_eq!(backstop(at(91), 0, 90, 95, gib, false), None, "no limit");
    }

    /// D4314 (a): an ask sent is never relief. Whatever was asked, the sample decides as with no ask: the cancel comes no
    /// later with revoke on than with the threshold alone; only a measured drop at the next sample (resident under the
    /// line) spares the statement.
    #[test]
    fn an_outstanding_ask_leaves_the_cancel_decision_unchanged() {
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        let gib = 1u64 << 30;
        let limit = 16 * gib;
        let at = |pct: u64| limit / 100 * pct;
        for (resident, reclaimable, trimmed) in [(at(91), 0, false), (at(91), gib / 2, false), (at(91), 10 * gib, false), (at(91), 0, true), (at(95), 0, false)] {
            let b = backstop(resident, limit, 90, 95, reclaimable, trimmed).expect("past the line");
            for asked in [1, b.revoke, b.revoke.max(1) * 4, u64::MAX] {
                assert_eq!(cancels(b, asked), cancels(b, 0), "asked {asked} at {resident} (reclaimable {reclaimable}, trimmed {trimmed})");
            }
        }
        assert!(cancels(backstop(at(91), limit, 90, 95, 0, false).unwrap(), 4 * gib), "nothing to reclaim, 4 GiB asked: cancel now");
    }

    #[test]
    fn a_landing_cancel_holds_the_next_until_the_top_tier() {
        const MIB: u64 = 1 << 20;
        let limit = 15741 * MIB;
        let line = limit / 100 * 90;
        assert!(landing_holds(14217 * MIB, limit, 90, 95, 10730 * MIB), "the decider's 15:04:55: 10.7 GB landing, 0.1 GB past the line");
        assert!(!landing_holds(14217 * MIB, limit, 90, 95, 0), "nothing landing");
        assert!(!landing_holds(line + 400 * MIB, limit, 90, 95, 43 * MIB), "what lands is short of the excess");
        assert!(!landing_holds(limit / 100 * 95, limit, 90, 95, 10730 * MIB), "the top tier");
        assert!(!landing_holds(line - 1, limit, 90, 95, 10730 * MIB), "under the line");
    }

    #[test]
    fn with_no_victim_a_stalled_grower_is_terminated_then_the_smaller_ones_go_at_the_top() {
        use NoVictim::*;
        assert_eq!(no_victim(true, Some((true, None)), true), Wait, "a cancel inside its ack window");
        assert_eq!(no_victim(false, Some((true, None)), false), Terminate, "unanswered past it and still growing");
        assert_eq!(no_victim(false, Some((false, None)), false), Line, "unanswered, not growing: only the line");
        assert_eq!(no_victim(false, Some((true, Some(CANCEL_ACK_MS - 1))), true), Wait, "the terminate's own window");
        assert_eq!(no_victim(false, Some((true, Some(CANCEL_ACK_MS))), true), Smaller, "the terminate unanswered too, at the top");
        assert_eq!(no_victim(false, Some((true, Some(CANCEL_ACK_MS))), false), Line);
        assert_eq!((no_victim(false, None, true), no_victim(false, None, false)), (Smaller, Line), "nothing condemned");
    }

    #[test]
    fn the_backstop_ranks_by_resident_bytes() {
        const MIB: u64 = 1 << 20;
        assert_eq!(rank_bytes(Some((resident_of(85 * MIB, 0), 85 << 20)), 0, 4208 * MIB), 85 * MIB);
        assert_eq!(rank_bytes(Some((85 * MIB, 85 << 20)), 400 * MIB, 4208 * MIB), 485 * MIB, "with the parts it pins");
        assert_eq!(rank_bytes(None, 400 * MIB, 4208 * MIB), 4208 * MIB);
        assert_eq!(rank_bytes(Some((0, -(200 << 20))), 100 * MIB, 400 * MIB), 500 * MIB, "D4035: a row below its start");
        assert_eq!(rank_bytes(Some((0, 0)), 0, 400 * MIB), 0, "at its start: the row");
        assert_eq!(resident_of(3021 * MIB, 2218 * MIB as i64), 803 * MIB);
        assert_eq!((resident_of(MIB, 2 * MIB as i64), resident_of(MIB, -5)), (0, MIB));
        let ranked = [(rank_bytes(Some((85 * MIB, 1)), 0, 4208 * MIB) as usize, 0), (rank_bytes(Some((600 * MIB, 1)), 0, 700 * MIB) as usize, 0)];
        assert_eq!(cancel_victim(ranked, 1, 32 << 20), Some(1), "the resident one, not the reserved one");
    }

    #[test]
    fn d4059_a_statement_is_read_again_only_when_its_fresh_rank_could_win() {
        const MIB: u64 = 1 << 20;
        assert!(stale_may_outrank(0, 800 * MIB, 300 * MIB), "its region read untouched; the pick holds 300 MiB");
        assert!(!stale_may_outrank(800 * MIB, 800 * MIB, 300 * MIB), "nothing not resident: its rank is fresh");
        assert!(!stale_may_outrank(0, 300 * MIB, 300 * MIB), "at most a tie: the pick stands");
        assert!(stale_may_outrank(0, 64 * MIB, 32 * MIB - 1), "no pick: past one under the least it cancels");
    }

    #[test]
    fn the_residency_pass_quickens_past_half_the_budget() {
        use super::residency_period_ms;
        assert_eq!((residency_period_ms(50, 100), residency_period_ms(51, 100), residency_period_ms(u64::MAX, 0)), (1000, 200, 1000));
    }

    #[test]
    fn tiers_escalate_from_base() {
        assert_eq!(tiers(80), [80, 90, 95]);
        assert_eq!(tiers(90), [90, 95, 97]);
        assert_eq!(tiers(50), [50, 75, 87]);
        assert_eq!(tiers(100), [100, 100, 100]);
        assert_eq!(tiers(0), [1, 50, 75]); // clamped base
        assert_eq!((crossed(79, 80), crossed(80, 80), crossed(94, 80), crossed(95, 80)), (0, 1, 2, 3));
    }

    #[test]
    fn rearm_and_cancel_keep_todays_edges() {
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        assert!(rearms(75, 80) && !rearms(76, 80));
        assert!(!cancel_armed(0) && cancel_armed(1));
        let gib = 1u64 << 30;
        let at = 10 * gib / 100 * 90; // the limit's hundredth, times the percent (integer, as before)
        assert_eq!(cancel_at(at - 1, 10 * gib, 90), None);
        assert_eq!(cancel_at(at, 10 * gib, 90), Some((90, 32 << 20)));
        assert_eq!(cancel_at(gib, 0, 90), None, "no limit, no cancel");
        assert_eq!(cancel_at(gib / 100, gib, 0), Some((1, 32 << 20)), "the percent clamps to 1..=100");
    }

    #[test]
    fn d4087_a_part_ranks_only_on_the_running_owners_that_hold_every_use() {
        // account 7 runs statement 40 and executes portal 41; account 8 runs none
        let running = |a: u32| if a == 7 { (40, 41) } else { (0, 0) };
        assert_eq!(cancel_frees(&[(7, 40, 2)], running), Some(7), "its statement's uses: a cancel frees the part");
        assert_eq!(cancel_frees(&[(7, 41, 1), (7, 40, 1)], running), Some(7), "its statement's and its portal's");
        assert_eq!(cancel_frees(&[(7, 39, 1)], running), None, "a use an ended statement or portal of it left");
        assert_eq!(cancel_frees(&[(7, 0, 1)], running), None, "its session's: a kept cursor (D1507)");
        assert_eq!(cancel_frees(&[(7, 40, 1), (7, 0, 1)], running), None, "beside its session's kept cursor: stays in use");
        assert_eq!(cancel_frees(&[(7, 40, 1), (9, 12, 1)], running), None, "another account uses it too");
        assert_eq!(cancel_frees(&[(8, 5, 1)], running), None, "an exited backend's: its slot runs no owner 5");
        assert_eq!(cancel_frees(&[(8, 0, 1)], running), None, "taken between statements");
        assert_eq!(cancel_frees(&[], running), None);
    }

    /// D4262: past the line, a server acts on its own statements only as a tenant of its cgroup (its own view at least a
    /// quarter of the governed resident) or when one statement alone holds the bytes over the line; a bystander beside
    /// other processes' memory does not (box 73: 200 MiB of a 13,550 MiB slot, a 53 MB statement canceled).
    #[test]
    fn d4262_a_bystander_in_a_full_cgroup_does_not_act() {
        const M: u64 = 1 << 20;
        let (resident, line) = (13_550 * M, 14_717 * M / 100 * 90);
        assert!(!acts(resident, 350 * M, 53 * M, line), "the D4262 bystander: 350 MiB of 13,550, its largest 53 MB");
        assert!(acts(resident, resident / 4, 53 * M, line), "a quarter of the cgroup: the tenant acts");
        assert!(acts(resident, 10_000 * M, 0, line), "the big tenant beside a sidecar (10 GB of 14 GB) acts");
        assert!(acts(resident, 350 * M, resident - line, line), "one statement alone holds the bytes over the line");
        assert!(!acts(resident, 350 * M, resident - line - 1, line));
        assert!(acts(resident, resident, 0, line), "alone in its cgroup: its own view is the resident");
        assert!(acts(0, 0, 0, 0), "nothing held: acts as before");
    }
}

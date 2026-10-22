//! Admission decisions; the driver's grant pool holds the atomics and runs the wait.

use crate::consts::{get, get_ms, K, KNOBS};

pub const BYTES_WAIT: core::time::Duration = core::time::Duration::from_millis(KNOBS[K::BytesWaitMs as usize].default);

#[inline]
pub fn bytes_wait() -> core::time::Duration {
    get_ms(K::BytesWaitMs)
}

/// D4403: the small buffer's configured size when no knob sets it (`small_buffer`).
#[inline]
pub fn small_buffer_default() -> u64 {
    get(K::SmallBuffer)
}

/// D4403: the small buffer for a configured `size` (0 = none) under `capacity` and, with the memory budget on, B (`b`; 0 =
/// no B clamp): at most a sixteenth of each (`small_buffer_div`). Its takes skip B's make_room and the governor's bar, so its
/// holders sit past B by at most this.
pub fn small_buffer(size: u64, capacity: u64, b: u64) -> u64 {
    let div = get(K::SmallBufferDiv).max(1);
    let r = size.min(capacity / div);
    if b != 0 {
        r.min(b / div)
    } else {
        r
    }
}

/// D4403: a statement whose reservation at its unchanged ask is `need` takes the small buffer first.
#[inline]
pub fn small_statement(need: u64) -> bool {
    need <= get(K::SmallStatementMax)
}

/// D4403: a draw of `bytes` with no reservation takes the small buffer first.
#[inline]
pub fn small_draw(bytes: u64) -> bool {
    bytes <= get(K::SmallDrawMax)
}

pub const QUEUE_STALL: core::time::Duration = core::time::Duration::from_millis(KNOBS[K::QueueStallMs as usize].default);

/// A room high by at least this much restarts a queued waiter's `QUEUE_STALL` clock.
pub const STALL_PROGRESS: u64 = KNOBS[K::StallProgress as usize].default;

pub const TURN_CAP: core::time::Duration = core::time::Duration::from_millis(KNOBS[K::TurnCapMs as usize].default);

#[derive(Clone, Copy, Debug)]
pub struct StallBounds {
    pub stall: core::time::Duration,
    pub cap: core::time::Duration,
}

impl StallBounds {
    pub const DEFAULT: StallBounds = StallBounds { stall: QUEUE_STALL, cap: TURN_CAP };

    pub fn current() -> StallBounds {
        StallBounds { stall: get_ms(K::QueueStallMs), cap: get_ms(K::TurnCapMs) }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct StallClock {
    began: u64,
    since: u64,
    high: u64,
}

impl StallClock {
    pub fn new(room: u64, now: u64) -> StallClock {
        StallClock { began: now, since: now, high: room }
    }

    /// `Some` halves the waiter: at the cap, or after `stall` without a new high.
    pub fn observe(&mut self, room: u64, now: u64, bounds: StallBounds) -> Option<Admitted> {
        if room >= self.high.saturating_add(get(K::StallProgress)) {
            (self.high, self.since) = (room, now);
        }
        let ns = |d: core::time::Duration| d.as_nanos().min(u64::MAX as u128) as u64;
        if now.saturating_sub(self.began) >= ns(bounds.cap) {
            Some(Admitted::Capped)
        } else if now.saturating_sub(self.since) >= ns(bounds.stall) {
            Some(Admitted::Stall)
        } else {
            None
        }
    }
}

pub const GOVERNOR_SETTLE: core::time::Duration = core::time::Duration::from_millis(KNOBS[K::GovernorSettleMs as usize].default);

#[inline]
pub fn governor_settle() -> core::time::Duration {
    get_ms(K::GovernorSettleMs)
}

pub const SETTLE_FALL: usize = KNOBS[K::SettleFall as usize].default as usize;

#[inline]
pub fn settle_fall() -> usize {
    get(K::SettleFall) as usize
}

pub const SETTLE_FLAT_POLLS: u32 = KNOBS[K::SettleFlatPolls as usize].default as u32;

#[inline]
pub fn settle_flat_polls() -> u32 {
    get(K::SettleFlatPolls) as u32
}

pub fn settle_on_track(fallen: usize, deficit: usize, waited: core::time::Duration) -> bool {
    let rest = governor_settle().saturating_sub(waited);
    waited.is_zero() || fallen as u128 * rest.as_nanos() >= deficit as u128 * waited.as_nanos()
}

#[inline]
pub fn whole_fits(need: u64, own: u64, kept: u64, limit: u64) -> bool {
    need.saturating_add(own).saturating_add(kept) <= limit
}

/// The kept holds are never given back by a statement's end, so a need past `limit` beside them can never be seated.
#[inline]
pub fn past_lane(bytes: u64, kept: u64, limit: u64) -> bool {
    bytes.saturating_add(kept) > limit
}

/// D4923 diag: why a grant pool take failed: the governor's bar (off / advise), past the limit only by the caches' stuck
/// bytes (`mcx::ladder::caches_stuck`: it would have fit without them), past the limit regardless, and under the one
/// budget applied B's room (`make_room`) or the bar. The pool counts them; no decision reads them.
pub const TAKE_FAIL_BAR: usize = 0;
pub const TAKE_FAIL_STUCK: usize = 1;
pub const TAKE_FAIL_LIMIT: usize = 2;
pub const TAKE_FAIL_BROKER: usize = 3;
pub const TAKE_FAILS: usize = 4;
pub const TAKE_FAIL_NAMES: [&str; TAKE_FAILS] = ["bar", "stuck", "limit", "broker"];

/// D4923 diag: a take of `bytes` beside `cur` held past `limit` (the caches' `stuck` bytes already off it) is the stuck
/// bytes' when it fits once they are back, the limit's otherwise.
pub fn past_limit_why(cur: u64, bytes: u64, limit: u64, stuck: u64) -> usize {
    if stuck != 0 && cur.saturating_add(bytes) <= limit.saturating_add(stuck) { TAKE_FAIL_STUCK } else { TAKE_FAIL_LIMIT }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Floor {
    pub past: bool,
    pub alone: bool,
    pub idle_start: bool,
}

/// A nested ask refuses at once (the pool is never empty for it), and so does a floor past the governor's bar.
pub fn floor(need: u64, kept: u64, limit: u64, nested: bool, bar: u64, on: bool) -> Floor {
    let past = past_lane(need, kept, limit);
    let fits_bar = !nested && (bar == 0 || need <= bar);
    let f = Floor { past, alone: past && fits_bar, idle_start: on && fits_bar };
    if crate::trace::on() {
        crate::trace::note("admit.floor", &format!("need {need} kept {kept} lane {limit} nested {nested} bar {bar} on {on}: {f:?}")); // alloc: Cold
    }
    f
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admitted {
    Whole,
    Stall,
    Capped,
    Short,
    Idle,
    Bound,
    Overdrawn,
    Solo,
}

impl Admitted {
    pub fn name(self) -> &'static str {
        match self {
            Admitted::Whole => "whole",
            Admitted::Stall => "halved at a queue stall",
            Admitted::Capped => "halved at the queue ticket's cap",
            Admitted::Short => "halved after the short wait",
            Admitted::Idle => "halved with nothing held to wait for",
            Admitted::Bound => "halved at the wait's bound",
            Admitted::Overdrawn => "past the pool (overdraw)",
            Admitted::Solo => "alone past the pool",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartSize {
    Small,
    Share,
}

/// Alone (no statement in flight on another session) a statement's hash breakers start small; beside others at their
/// shares, so admission seats it only where its footprint at them fits. `forced` overrides.
pub const fn decide_start(others: u64, forced: Option<StartSize>) -> StartSize {
    match forced {
        Some(s) => s,
        None if others == 0 => StartSize::Small,
        None => StartSize::Share,
    }
}

/// Grow mode (`pgrust.gravity_grow`): every statement's hash breakers start small, beside others too:
/// admission claims the start and the breakers grow on demand. `forced` still overrides; off: `decide_start`.
pub const fn decide_start_in(others: u64, forced: Option<StartSize>, grow: bool) -> StartSize {
    match forced {
        Some(s) => s,
        None if grow => StartSize::Small,
        None => decide_start(others, None),
    }
}

pub fn start(others: u64, running: u64, forced: Option<StartSize>, grow: bool) -> StartSize {
    let s = decide_start_in(others, forced, grow);
    if crate::trace::on() {
        crate::trace::note("admit.start", &format!("others {others} running {running} forced {forced:?}{}: {s:?}", if grow { " grow" } else { "" })); // alloc: Cold
    }
    s
}

pub const GRANT_FLOOR: u64 = 64 << 20;

pub const POOL_FRACTION_DEFAULT: u32 = 1_000_000;
pub const POOL_FRACTION_WHOLE: u32 = 1_000_000;

pub const ARENA_ALLOWANCE: u64 = 4 << 20;

pub const FMGR_SITE_ALLOWANCE: u64 = 8 << 10;

/// memfree Part 2: a grant of `bytes` whose `need` is past the statement's `limit` halves, never under `floor` (None: stop).
#[inline]
pub fn halve_for_limit(need: u64, limit: u64, bytes: u64, floor: u64) -> Option<u64> {
    (limit != 0 && need > limit && bytes / 2 >= floor).then_some(bytes / 2)
}

#[inline]
pub fn others_hold(holders: u64, mine: u64) -> bool {
    holders > mine
}

/// The least a statement's bound past G may be: one `GRANT_FLOOR` past its grant or its reservation, the larger. The
/// breakers' shares round up to whole pages, so together they come to a page past the grant.
#[inline]
pub fn granted_floor(granted: u64, reserved: u64) -> u64 {
    granted.max(reserved).saturating_add(GRANT_FLOOR)
}

#[inline]
pub fn within_bounds(held: u64, floor: u64, cap: u64) -> bool {
    cap == 0 || held <= cap.max(floor)
}

#[inline]
pub fn statement_cap(lane: u64, ppm: u32) -> u64 {
    if ppm >= POOL_FRACTION_WHOLE {
        return 0;
    }
    (lane as u128 * ppm as u128 / POOL_FRACTION_WHOLE as u128) as u64
}

/// D4078: the part of the work pool one serial owner region may hold, in parts per million: half. A one-round owner past
/// half the pool leaves no room for a second statement; `own_split` already exists (the serial owners fold one after
/// another, each under the wall, and a chunk the pool refuses is a temp file mapped shared, D491). Precedent: DR-13's
/// half-pool slice. CHOSEN, not measured. The format's wall (the u32 words of `Owned`, ~4 GiB, D089) was the only wall: a
/// COUNT(DISTINCT URL) under a 3 GB memory.max (a 2080 MiB pool) was seated alone past the pool at 4165 MiB, its owner
/// region laid up to that wall, and the next statement re-took the ~2 GB parked region and was the watchdog's cancel.
pub const OWNER_WALL_POOL_PPM: u32 = 500_000;

/// D4078: the least one-owner wall: one split owner's table (the driver's `SPLIT_TABLE`, 64 MiB) and as many bytes of
/// runs beside it, so a small pool's wall never splits an owner into many tiny ones. CHOSEN, not measured.
pub const OWNER_WALL_MIN: u64 = 128 << 20;

/// D4078: the one-owner wall of a statement admitted now, one rule for both pool modes. `pool_follows` (the broker re-arms
/// the pool from B every tick: memledger `pool_follows`): `OWNER_WALL_POOL_PPM` of the work pool's row cap `row_cap`
/// (`sizes::work_pool_capacity` with no unattributed bytes, which swing 0-1.6 GiB per snapshot), never of the tick's armed
/// capacity (1,165 MiB at idle on cbsub-limit, a new value every tick, far under what the pool can reach). Otherwise (advise,
/// off, an explicit or railed capacity, all static): of the armed big lane `armed_lane`. At least `OWNER_WALL_MIN`, never
/// past `format` (the format's wall), 8-byte aligned; `format` when the quantity is 0 (an unarmed pool).
#[inline]
pub fn owner_wall(pool_follows: bool, row_cap: u64, armed_lane: u64, format: u64) -> u64 {
    let pool = if pool_follows { row_cap } else { armed_lane };
    if pool == 0 {
        return format;
    }
    ((pool as u128 * OWNER_WALL_POOL_PPM as u128 / POOL_FRACTION_WHOLE as u128) as u64).max(OWNER_WALL_MIN).min(format) & !7
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Road {
    W,
    G,
    PastG,
}

/// Past both only for an admitted statement or a worker; any other takes G's road, where the pool's room refuses it.
#[inline]
pub fn road(admitted: bool, w_fits: bool, g_fits: bool) -> Road {
    if w_fits {
        Road::W
    } else if g_fits || !admitted {
        Road::G
    } else {
        Road::PastG
    }
}

/// Past G only when neither the parked region nor the ask fits G, so an ask G holds is never served by a bigger region.
#[inline]
pub fn parked_road(admitted: bool, w_fits: bool, g_fits_region: bool, g_fits_ask: bool) -> Road {
    if w_fits {
        Road::W
    } else if admitted && !g_fits_region && !g_fits_ask {
        Road::PastG
    } else {
        Road::G
    }
}

/// D4291: the ladder of a leader's draw past W and G the pool cannot hold at once. A statement whose finished sort no
/// reader has begun, inside the open of a pipeline (`own`): the depots (no I/O), then its own sorts put to their temp files
/// (the draw refused at once; the open puts them and retries once through `DepotsThenWait`). Any other draw, or a worker's:
/// the depots, then GOVWAIT's revoke and wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PastLadder {
    DepotsThenWait,
    DepotsThenOwnSpill,
}

#[inline]
pub fn past_ladder(own: bool, worker: bool) -> PastLadder {
    if own && !worker {
        PastLadder::DepotsThenOwnSpill
    } else {
        PastLadder::DepotsThenWait
    }
}

#[inline]
pub fn under_bar(projected: usize, n: usize, bar: usize) -> bool {
    bar == 0 || projected.saturating_add(n) <= bar
}

/// The governor's bar pre-solved at a sample (`limit`: the context footprint the bar allows then, its `under_bar`): `n`
/// more context bytes on the footprint `footprint` pass it.
#[inline]
pub fn past_footprint(footprint: usize, n: usize, limit: usize) -> bool {
    footprint.saturating_add(n) > limit
}

/// D4812 (2), the block gate (`ladder_block_gate`, the governor's): the least memory-context block it weighs. The
/// AllocSet ramp's blocks are 1-8 MiB; a smaller block's overshoot past the line is a bounded error.
pub const BLOCK_GATE_BYTES: usize = 1 << 20;

/// D4812 (2): the block gate's sleep between two tests of its projection while it waits (a new sample or bytes freed end
/// the wait at the next test).
pub const BLOCK_GATE_STEP: core::time::Duration = core::time::Duration::from_millis(5);

/// D4812 (2): the block gate weighs a block of `n` bytes.
#[inline]
pub fn block_gated(n: usize) -> bool {
    n >= BLOCK_GATE_BYTES
}

/// D4812 (2): a block of `n` bytes takes the governor's projection `projected` past the watchdog's cancel line
/// `cancel_line` (0 = no limit known: never).
#[inline]
pub fn past_cancel_line(projected: usize, n: usize, cancel_line: usize) -> bool {
    cancel_line != 0 && projected.saturating_add(n) > cancel_line
}

/// D4812 (2): the context footprint the cancel line allows past a sample whose read is `read` bytes, taken at the
/// context footprint `footprint` (the gate's pre-solved `past_cancel_line`): saturated at 0 when the read is past the
/// line by more than that footprint (every block then meets the gate); none (`usize::MAX`) with no line.
#[inline]
pub fn cancel_line_footprint(cancel_line: usize, read: usize, footprint: usize) -> usize {
    if cancel_line == 0 {
        return usize::MAX;
    }
    cancel_line.saturating_add(footprint).saturating_sub(read)
}

/// D4812 (2): the block gate has waited its bound (`cancel_landing_ms`, one R6 cancel's landing).
#[inline]
pub fn block_gate_spent(waited: core::time::Duration) -> bool {
    waited >= get_ms(K::CancelLandingMs)
}

pub const SMALL_SESSION: usize = 32 << 20;

#[inline]
pub fn small_session_grace(want: usize, bar: usize, thread_bytes: isize) -> bool {
    thread_bytes < SMALL_SESSION as isize && want <= bar.saturating_add(bar / 20)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrawWait {
    Wait,
    /// No room can come back: one last try after a fresh governor sample, then out of memory.
    LastTry,
    Refuse,
}

/// A draw past its grant waits, at most `bytes_wait`, only while a holder could give bytes back or a reclaim is due.
#[inline]
pub fn draw_wait(others_could_give: bool, reclaim_due: bool, waited: core::time::Duration) -> DrawWait {
    if !others_could_give && !reclaim_due {
        DrawWait::LastTry
    } else if waited >= bytes_wait() {
        DrawWait::Refuse
    } else {
        DrawWait::Wait
    }
}

/// D4439 (`draw_wait_cap`): a statement's draw-gate waits in all, in `bytes_wait` budgets: one draw's own most
/// (`ladder::WAIT_CAP_X`), for the whole statement. CHOSEN.
pub const DRAW_WAIT_CAP_X: u32 = 4;

/// D4439: the cap on a statement's draw-gate waits in all with the ladder on (`draw_wait_cap`; `None` = no cap).
#[inline]
pub fn draw_wait_cap() -> Option<core::time::Duration> {
    (get(K::DrawWaitCap) != 0).then(|| bytes_wait().saturating_mul(DRAW_WAIT_CAP_X))
}

/// D4439: what is left of a statement's draw-wait cap `cap` after it waited `waited_ns` (zero = spent).
#[inline]
pub fn draw_wait_left(cap: core::time::Duration, waited_ns: u64) -> core::time::Duration {
    cap.saturating_sub(core::time::Duration::from_nanos(waited_ns))
}

/// Ladder step 1.1 (`gate_now`): who asks at a pool miss. `Spills`: the draw has a road past the pool (an owner's or a
/// session's mapped temp file); `Refuses`: its refusal is the statement's (53200, or `scratch_draw`'s overdraft).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Asker {
    Spills,
    Refuses,
}

/// Ladder step 1.1: the gate's rule is on (`PGRUST_MEMPOLICY=gate_now=1`; default off).
#[inline]
pub fn gate_now() -> bool {
    get(K::GateNow) != 0
}

/// Ladder step 1.1: how long a `Refuses` asker waits for room with no bytes on their way.
#[inline]
pub fn gate_refuse() -> core::time::Duration {
    get_ms(K::GateRefuseMs)
}

/// Ladder step 1.1 (`gate_now`): a pool miss waits only for bytes already on their way (`coming`: revoke asks, a cancel
/// landing, the reaper's unmaps, a reclaim due), within R5's budget counted from the last free (`ladder::wait_spent`).
/// Otherwise a `Spills` asker takes its road at once (`Refuse` = the caller's road) and a `Refuses` asker waits
/// `gate_refuse` for room from its first miss, then refuses (the user's 10-14 ruling: no victim of ours, wait ~200 ms for
/// room, then 53200; never a crawl). Another holder that gives nothing back is no reason to wait. `coming` is a bool: a
/// small pending ask holds a large waiter for the budget (bytes-sized relief is step 1.2).
#[inline]
pub fn gate_wait(asker: Asker, coming: bool, since_free: core::time::Duration, total: core::time::Duration) -> DrawWait {
    if coming && !crate::ladder::wait_spent(since_free, total, bytes_wait()) {
        return DrawWait::Wait;
    }
    match asker {
        Asker::Refuses if total < gate_refuse() => DrawWait::Wait,
        _ => DrawWait::Refuse,
    }
}

#[cfg(test)]
mod gate {
    use super::{bytes_wait, gate_refuse, gate_wait, Asker, DrawWait};
    use core::time::Duration;

    /// Ladder step 1.1: with nothing on its way a spillable draw takes its road at once and a non-spillable one waits
    /// 200 ms; with bytes on their way both wait the budget from the last free, at most 4 budgets in all.
    #[test]
    fn a_miss_waits_only_for_bytes_on_their_way() {
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        let (ms, b, r) = (Duration::from_millis, bytes_wait(), gate_refuse());
        assert_eq!(r, ms(200));
        assert_eq!(gate_wait(Asker::Spills, false, Duration::ZERO, Duration::ZERO), DrawWait::Refuse, "nothing coming: the road now");
        assert_eq!(gate_wait(Asker::Refuses, false, Duration::ZERO, r - ms(1)), DrawWait::Wait);
        assert_eq!(gate_wait(Asker::Refuses, false, Duration::ZERO, r), DrawWait::Refuse, "200 ms, then the refusal");
        assert_eq!(gate_wait(Asker::Spills, true, b - ms(1), b * 3), DrawWait::Wait, "bytes coming: the budget from the last free");
        assert_eq!(gate_wait(Asker::Spills, true, b, b), DrawWait::Refuse, "no free for a whole budget");
        assert_eq!(gate_wait(Asker::Refuses, true, ms(10), b * 4), DrawWait::Refuse, "4 budgets in all");
        assert_eq!(gate_wait(Asker::Refuses, true, b, b), DrawWait::Refuse);
    }
}

#[cfg(test)]
mod draw {
    use super::{bytes_wait, draw_wait, others_hold, parked_road, road, statement_cap, DrawWait, Road, POOL_FRACTION_WHOLE};
    use core::time::Duration;

    #[test]
    fn a_draw_takes_w_then_g_then_past_g_admitted() {
        assert_eq!(road(true, true, false), Road::W);
        assert_eq!(road(true, false, true), Road::G);
        assert_eq!(road(true, false, false), Road::PastG);
        assert_eq!(road(false, false, false), Road::G, "not admitted: G's road, the pool's room refuses");
        assert_eq!(parked_road(true, true, false, false), Road::W);
        assert_eq!(parked_road(true, false, false, true), Road::G, "D3107: the ask fits G: never a bigger region past it");
        assert_eq!(parked_road(true, false, false, false), Road::PastG);
        assert_eq!(parked_road(false, false, false, false), Road::G);
        assert!(others_hold(2, 1) && !others_hold(1, 1));
        assert_eq!(statement_cap(1 << 30, POOL_FRACTION_WHOLE), 0, "the whole lane: no cap");
        assert_eq!(statement_cap(1 << 30, POOL_FRACTION_WHOLE / 2), 1 << 29);
    }

    #[test]
    fn the_governor_refuses_past_its_bar_with_a_small_sessions_grace() {
        use super::{small_session_grace, under_bar, SMALL_SESSION};
        assert!(under_bar(usize::MAX, 1, 0) && under_bar(60, 40, 100) && !under_bar(60, 41, 100));
        assert!(small_session_grace(105, 100, 0) && !small_session_grace(106, 100, 0));
        assert!(!small_session_grace(101, 100, SMALL_SESSION as isize), "a large session: no grace");
    }

    /// D4291: an open's draw with its statement's own finished sorts to put takes them before any wait; a worker's draw
    /// and every other keep the wait.
    #[test]
    fn d4291_own_sorts_before_the_wait() {
        use super::{past_ladder, PastLadder};
        assert_eq!(past_ladder(true, false), PastLadder::DepotsThenOwnSpill);
        assert_eq!(past_ladder(false, false), PastLadder::DepotsThenWait);
        assert_eq!(past_ladder(true, true), PastLadder::DepotsThenWait, "a worker's draw: its leader's sorts are not its own");
        assert_eq!(past_ladder(false, true), PastLadder::DepotsThenWait);
    }

    #[test]
    fn a_draw_waits_only_while_room_can_come_back() {
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        let w = bytes_wait();
        assert_eq!(draw_wait(false, false, Duration::ZERO), DrawWait::LastTry);
        assert_eq!(draw_wait(true, false, Duration::ZERO), DrawWait::Wait);
        assert_eq!(draw_wait(false, true, w - Duration::from_millis(1)), DrawWait::Wait);
        assert_eq!(draw_wait(true, false, w), DrawWait::Refuse);
        assert_eq!(w, Duration::from_millis(2_000));
    }
}

#[cfg(test)]
mod d3127 {
    use super::{settle_on_track, GOVERNOR_SETTLE};
    use core::time::Duration;

    const MIB: usize = 1 << 20;

    #[test]
    fn a_settle_is_on_track_only_while_its_fall_rate_covers_the_deficit() {
        let ms = Duration::from_millis;
        assert!(settle_on_track(200 * MIB, 2800 * MIB, ms(30)));
        assert!(!settle_on_track(0, 1, ms(2)));
        assert!(!settle_on_track(20 * MIB, 3000 * MIB, ms(10)));
        assert!(settle_on_track(20 * MIB, 1500 * MIB, ms(10)));
        assert!(settle_on_track(0, 0, ms(5)));
        assert!(!settle_on_track(500 * MIB, 1, GOVERNOR_SETTLE));
    }
}

#[cfg(test)]
mod d3214 {
    use super::{Admitted, StallBounds, StallClock, STALL_PROGRESS};
    use core::time::Duration;

    const MS: u64 = 1_000_000;
    const B: StallBounds = StallBounds { stall: Duration::from_millis(300), cap: Duration::from_millis(600) };

    #[test]
    fn only_a_new_high_by_the_progress_grain_restarts_the_stall_clock() {
        let mut c = StallClock::new(500 << 20, 0);
        for t in 1..300 {
            assert_eq!(c.observe((500 << 20) + (t % 2) * (STALL_PROGRESS - 1), t * MS, B), None, "{t} ms");
        }
        assert_eq!(c.observe(500 << 20, 300 * MS, B), Some(Admitted::Stall));
        let mut c = StallClock::new(4 * STALL_PROGRESS, 0);
        assert_eq!(c.observe(0, 100 * MS, B), None);
        assert_eq!(c.observe(4 * STALL_PROGRESS, 200 * MS, B), None);
        assert_eq!(c.observe(4 * STALL_PROGRESS, 300 * MS, B), Some(Admitted::Stall), "back to its high is no new high");
    }

    #[test]
    fn the_cap_halves_a_waiter_that_keeps_making_progress() {
        let mut c = StallClock::new(0, 0);
        for t in 1..600 {
            assert_eq!(c.observe(t / 100 * STALL_PROGRESS, t * MS, B), None, "{t} ms");
        }
        assert_eq!(c.observe(7 * STALL_PROGRESS, 600 * MS, B), Some(Admitted::Capped));
    }
}

#[cfg(test)]
mod d3380 {
    use super::{floor, past_lane, whole_fits, Floor};

    const MIB: u64 = 1 << 20;

    #[test]
    fn a_floor_past_the_lane_runs_alone_unless_nested_or_past_the_bar() {
        let f = |past, alone| Floor { past, alone, idle_start: false };
        assert_eq!(floor(48 * MIB, 0, 64 * MIB, false, 0, false), f(false, false));
        assert_eq!(floor(96 * MIB, 0, 64 * MIB, false, 0, false), f(true, true));
        assert_eq!(floor(96 * MIB, 0, 64 * MIB, true, 0, false), f(true, false), "nested: refused at once");
        assert_eq!(floor(96 * MIB, 0, 64 * MIB, false, 80 * MIB, false), f(true, false), "past the bar: refused at once");
        assert_eq!(floor(96 * MIB, 0, 64 * MIB, false, 96 * MIB, false), f(true, true));
        assert!(whole_fits(32 * MIB, 0, 32 * MIB, 64 * MIB) && !whole_fits(32 * MIB, MIB, 32 * MIB, 64 * MIB));
    }

    #[test]
    fn a_floor_inside_the_kept_holds_is_past_the_lane_and_on_starts_when_idle() {
        assert!(!past_lane(48 * MIB, 16 * MIB, 64 * MIB) && past_lane(48 * MIB, 17 * MIB, 64 * MIB));
        assert_eq!(floor(48 * MIB, 32 * MIB, 64 * MIB, false, 0, false), Floor { past: true, alone: true, idle_start: false });
        assert_eq!(floor(48 * MIB, 0, 64 * MIB, false, 0, true), Floor { past: false, alone: false, idle_start: true });
        assert_eq!(floor(48 * MIB, 0, 64 * MIB, true, 0, true), Floor { past: false, alone: false, idle_start: false }, "nested");
        assert_eq!(floor(48 * MIB, 0, 64 * MIB, false, 32 * MIB, true), Floor { past: false, alone: false, idle_start: false }, "past the bar");
    }
}

#[cfg(test)]
mod d4078 {
    use super::{owner_wall, OWNER_WALL_MIN};

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    const FORMAT: u64 = u32::MAX as u64 & !7;

    #[test]
    fn under_advise_the_owner_wall_is_half_the_armed_big_lane() {
        assert_eq!(owner_wall(false, 0, 2080 * MIB, FORMAT), 1040 * MIB, "a 3 GB memory.max's pool: half its lane");
        assert_eq!(owner_wall(false, 0, 13 * GIB, FORMAT), FORMAT, "a measurement box's pool: the format's wall, as before");
        assert_eq!(owner_wall(false, 40 * GIB, 2080 * MIB, FORMAT), 1040 * MIB, "advise never reads the row cap");
        assert_eq!(owner_wall(false, 0, 0, FORMAT), FORMAT, "unarmed: the format's wall");
        assert_eq!(owner_wall(false, 0, 64 * MIB, FORMAT), OWNER_WALL_MIN, "a small pool: the least wall");
        assert_eq!(owner_wall(false, 0, 2080 * MIB + 15, FORMAT) % 8, 0, "8-byte aligned");
        assert_eq!(owner_wall(false, 0, 2080 * MIB, OWNER_WALL_MIN), OWNER_WALL_MIN, "a lowered format wall still bounds it");
    }

    #[test]
    fn under_the_broker_the_owner_wall_is_half_the_row_cap_never_the_ticks_armed_pool() {
        let row_cap = 5590 * MIB;
        assert_eq!(owner_wall(true, row_cap, 1165 * MIB, FORMAT), row_cap / 2, "cbsub-limit: ~2.7 GiB, not half the idle 1,165 MiB");
        assert_eq!(owner_wall(true, row_cap, 5 * GIB, FORMAT), row_cap / 2, "the tick's armed capacity moves, the wall does not");
        assert_eq!(owner_wall(true, 11 * GIB, 1165 * MIB, FORMAT), FORMAT, "a row cap past twice the format's: the format's");
        assert_eq!(owner_wall(true, 0, 1165 * MIB, FORMAT), FORMAT, "no row cap: the format's");
    }
}

#[cfg(test)]
mod d3798_s2 {
    use super::{decide_start, StartSize};

    #[test]
    fn a_statement_starts_at_its_shares_only_beside_others() {
        assert_eq!(decide_start(0, None), StartSize::Small, "alone: E1's small start");
        assert_eq!(decide_start(1, None), StartSize::Share, "beside one other: at its shares, the admission seats it");
        assert_eq!(decide_start(9, None), StartSize::Share);
        assert_eq!(decide_start(0, Some(StartSize::Share)), StartSize::Share, "PGRUST_GRAVITY_START=share");
        assert_eq!(decide_start(9, Some(StartSize::Small)), StartSize::Small, "PGRUST_GRAVITY_START=small");
    }

    /// Grow mode: beside others a statement still starts small (its admission claims the start, its breakers grow on
    /// demand); off, the start is `decide_start`'s; the developer override still wins.
    #[test]
    fn grow_mode_starts_small_beside_others_too() {
        use super::decide_start_in;
        assert_eq!(decide_start_in(9, None, true), StartSize::Small, "grow: small beside others");
        assert_eq!(decide_start_in(0, None, true), StartSize::Small);
        assert_eq!(decide_start_in(9, None, false), StartSize::Share, "off: as today");
        assert_eq!(decide_start_in(0, None, false), StartSize::Small, "off: as today");
        assert_eq!(decide_start_in(9, Some(StartSize::Share), true), StartSize::Share, "PGRUST_GRAVITY_START=share wins");
    }
}

#[cfg(test)]
mod d4812_block_gate {
    use super::{block_gated, cancel_line_footprint, past_cancel_line, past_footprint, BLOCK_GATE_BYTES};

    const MIB: usize = 1 << 20;

    /// D4812 (2): the gate's limit is the cancel line's room past the sample, saturated at 0 once the sample is past
    /// the line; its test and its least block.
    #[test]
    fn the_block_gates_limit_saturates_at_0_past_the_line() {
        assert_eq!(cancel_line_footprint(600 * MIB, 100 * MIB, 50 * MIB), 550 * MIB, "500 MiB of room past the sample's footprint");
        assert_eq!(cancel_line_footprint(600 * MIB, 700 * MIB, 50 * MIB), 0, "the sample past the line: 0, every block meets the gate");
        assert_eq!(cancel_line_footprint(600 * MIB, 650 * MIB, 80 * MIB), 30 * MIB, "under the sample's footprint");
        assert_eq!(cancel_line_footprint(0, 700 * MIB, 50 * MIB), usize::MAX, "no line: none");
        assert!(!past_cancel_line(599 * MIB, MIB, 600 * MIB) && past_cancel_line(599 * MIB, MIB + 1, 600 * MIB));
        assert!(!past_cancel_line(usize::MAX, MIB, 0), "no line: never");
        assert!(block_gated(BLOCK_GATE_BYTES) && !block_gated(BLOCK_GATE_BYTES - 1));
        assert!(!past_footprint(10, 5, 15) && past_footprint(10, 6, 15) && past_footprint(usize::MAX, 1, usize::MAX - 1));
    }
}

#[cfg(test)]
mod d4923_take_fails {
    use super::{past_limit_why, TAKE_FAIL_LIMIT, TAKE_FAIL_STUCK};

    /// A take past the limit is the caches' stuck bytes' when it fits once they are back, the pool's limit otherwise.
    #[test]
    fn a_take_past_the_limit_names_the_stuck_bytes_only_when_they_made_it_miss() {
        const MIB: u64 = 1 << 20;
        assert_eq!(past_limit_why(760 * MIB, 2 * MIB, 700 * MIB, 900 * MIB), TAKE_FAIL_STUCK, "fits once the stuck bytes are back");
        assert_eq!(past_limit_why(760 * MIB, 2 * MIB, 700 * MIB, 50 * MIB), TAKE_FAIL_LIMIT, "past the limit even without them");
        assert_eq!(past_limit_why(760 * MIB, 2 * MIB, 700 * MIB, 0), TAKE_FAIL_LIMIT, "no stuck bytes: the limit");
    }
}

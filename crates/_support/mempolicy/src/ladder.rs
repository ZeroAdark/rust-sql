//! D4324 (the user's design, ruled 2026-10-13; lanes/memlead/LADDER-DESIGN.md): the memory ladder. Every gate (an ask that
//! does not fit) and the allocator's commit hook climb it; each rung's relief is the MEASURED drop of the line it reads,
//! never what it asked for. The order (the protected-cache revision):
//! R1 allocator return -> (R2p, under the parked_trim_rung knob: trim the parked regions and slot sets) -> R2a evict
//! elastic cache bytes -> R3 fewer whole parts in flight -> R4a the asker spills its own
//! (only over its fair share) -> R4b revoke others -> R5 wait (woken on free) -> R2b evict protected cache bytes (only to
//! avoid a cancel) -> R6 cancel the largest holder (the asker too, when it is the largest; one cancel in flight). The hook
//! (the ladder thread) has no R4a, and its R5 is one tick (`HOOK_GRACE_MS`, D4353).
//! Pure: a function of the state the caller read. The mechanisms (gates, the ladder thread, the hook) live elsewhere and
//! are wired behind `pgrust.memory_ladder` (off / shadow / on, default off).

const MIB: u64 = 1 << 20;

/// The protected share of the memory budget B the caches keep below every rung but R2b, percent. CHOSEN (main's
/// suggestion, the user's fixed-size ruling): measurement line = the analytic suite at W0 + W8 and tpch10 W8 with every cache capped at its
/// floor alone vs unbounded, the smallest share whose hot cells stay within the +10 ms rule (not yet run).
pub const PROTECTED_PCT: u64 = 25;
/// D4298 (T4, main GO 2026-10-13): the holders R2a takes AFTER every cache of `reclaim::ORDER` reached its protected floor,
/// in this order, down to their bytes in use: the shared catalog L2 (a re-build from the catalog), then the compiled-code
/// cache (a re-compile). Outside section 0b's protected split (they had no reclaim when it was ruled). Only the ladder
/// runs their reclaim (`memledger::register_ladder_reclaim`): the broker, the watchdog's backstop and the snapshot read
/// their hooks as before, so the ladder off is unchanged.
pub const TAIL: [memledger::Consumer; 2] = [memledger::Consumer::CatalogL2, memledger::Consumer::ProgramCaches];
/// The margin between the ladder's line and the limit: at least this (bytes). CHOSEN; the measured term is
/// `margin_for` (p99 growth over a cancel's landing and one tick), D4389's `margin`.
pub const MARGIN_MIN: u64 = 64 * MIB;
/// D4389: the measured margin is at most this percent of the limit (and at least `MARGIN_MIN`), so a bursty box cannot
/// pull the line down by more than a tenth of the limit. CHOSEN.
pub const MARGIN_CAP_PCT: u64 = 10;
/// R1's yield under which it is not run again within `R1_COOL_MS` unless memory rose by more than that yield (bytes;
/// perfflow's ALLOC-LADDER (b): 0-6 MiB measured on the analytic suite). CHOSEN.
pub const R1_FLOOR: u64 = 16 * MIB;
pub const R1_COOL_MS: u64 = 1_000;
/// The ladder thread's period, and the term of the margin past a cancel's landing (ms; today's watchdog cadence).
pub const TICK_MS: u64 = 100;
/// D4353 (USER ruling 2026-10-13 14:21Z, option (a)): the hook's wait (its R5): its R2b and R6 run only when the need is
/// still there this long after a pass ran R1-R4b. One tick, the margin's own term (section 7); an episode the earlier rungs
/// or a statement's end close within it cancels no one.
pub const HOOK_GRACE_MS: u64 = TICK_MS;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Rung {
    /// R1: every heap-owning thread returns its free pages, then one purge.
    AllocReturn,
    /// R2a: cache bytes above each cache's protected floor, in the cache's own eviction order; the depots.
    EvictElastic,
    /// R3: claims split inside parts and a lower scan width for claims taken from now.
    NarrowScans,
    /// R4a: the asker spills its own spillable memory, down to its fair share.
    OwnSpill,
    /// R4b: other statements are asked to spill (furthest over their fair share first).
    Revoke,
    /// R5: the asker waits, woken on free.
    Wait,
    /// R2b: cache bytes below the protected floor, down to what is in use.
    EvictProtected,
    /// R6: cancel the largest holder.
    Cancel,
    /// R2p (P3, `parked_trim_rung`; last in the enum so the earlier rungs keep their indices): the parked regions and slot
    /// sets, least recently parked first, up to the need. Its order is after R1, before R2a (`ASKER`, `HOOK`).
    TrimParked,
}

impl Rung {
    /// Its LADDER-DESIGN name.
    pub const fn name(self) -> &'static str {
        match self {
            Rung::AllocReturn => "R1",
            Rung::EvictElastic => "R2a",
            Rung::NarrowScans => "R3",
            Rung::OwnSpill => "R4a",
            Rung::Revoke => "R4b",
            Rung::Wait => "R5",
            Rung::EvictProtected => "R2b",
            Rung::Cancel => "R6",
            Rung::TrimParked => "R2p",
        }
    }
}

/// Who climbs: a gate's asker (it can wait and spill itself), or the allocator hook's wake of the ladder thread (no asker).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Who {
    Asker,
    Hook,
}

/// The asker's order.
pub const ASKER: [Rung; 9] = [Rung::AllocReturn, Rung::TrimParked, Rung::EvictElastic, Rung::NarrowScans, Rung::OwnSpill, Rung::Revoke, Rung::Wait, Rung::EvictProtected, Rung::Cancel];
/// The hook's order: no asker, so no own spill; its wait is one tick (`HOOK_GRACE_MS`).
pub const HOOK: [Rung; 8] = [Rung::AllocReturn, Rung::TrimParked, Rung::EvictElastic, Rung::NarrowScans, Rung::Revoke, Rung::Wait, Rung::EvictProtected, Rung::Cancel];

pub fn order(who: Who) -> &'static [Rung] {
    match who {
        Who::Asker => &ASKER,
        Who::Hook => &HOOK,
    }
}

/// What the rungs read, as one climb saw it at its last read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State {
    /// The cgroup's memory less its clean page cache (`memory.current - inactive_file`, the user's read).
    pub read: u64,
    /// The ladder's line: the limit less the margin.
    pub line: u64,
    /// The asker's ask (0 for the hook).
    pub ask: u64,
    /// R1 ran within its cool-down and returned under `R1_FLOOR`, and memory has not risen past that yield since.
    pub r1_cooling: bool,
    /// The caches' bytes above their protected floors that are not in use, and the depots' parked bytes (R2p's instead
    /// under the parked_trim_rung knob).
    pub elastic: u64,
    /// P3 (`parked_trim_rung`): the parked regions' and trimmable slot sets' bytes R2p may trim; 0 with the knob off.
    pub parked: u64,
    /// The caches' bytes at or under their floors that are not in use.
    pub protected: u64,
    /// R3 is already set.
    pub narrowed: bool,
    /// The asker's held bytes (section 4's size less the parts it pins), its fair share, and its spillable bytes now.
    pub asker_held: u64,
    pub asker_fair: u64,
    pub asker_spillable: u64,
    /// Other statements' bytes revoke may ask for now (`reclaim::revocable` candidates' asks).
    pub revocable: u64,
    /// The wait ended with the need still there: the asker's budget spent, or the hook's grace past (`hook_waited`).
    pub waited: bool,
    /// A cancel is in flight (picked, not yet landed): no second one.
    pub cancel_in_flight: bool,
    /// `ladder_kill_read` (USER 10-18): the kill decisions' read, the governed resident (max(anon + shmem,
    /// memory.current less the clean page cache)), never above `read`; `None` = `read` (the switch off, or no reading).
    pub kill_read: Option<u64>,
}

/// The bytes the climb must free: past the line once the ask is in.
pub fn need(s: &State) -> u64 {
    s.read.saturating_add(s.ask).saturating_sub(s.line)
}

/// `ladder_kill_read`: the bytes a kill (R6) is for: the kill read (`State::kill_read`, else the read) past the line once
/// the ask is in. Never more than `need`: the kill read is never above the read.
pub fn kill_need(s: &State) -> u64 {
    s.kill_read.map_or(s.read, |k| k.min(s.read)).saturating_add(s.ask).saturating_sub(s.line)
}

/// `ladder_kill_read` (USER 10-18): kill decisions read the governed resident (default on).
pub fn kill_read_on() -> bool {
    crate::consts::get(crate::consts::K::LadderKillRead) != 0
}

/// R1's cool-down (section 5): R1 last ran at `at_ms` (0 = never) when the read was `read_at` and returned `yield_`.
/// It cools while it is within `R1_COOL_MS` of that run, its yield was under `R1_FLOOR`, and the read has not risen past
/// that run's read by more than the yield (what it returned then could be back to return again).
pub fn r1_cooling(now_ms: u64, at_ms: u64, yield_: u64, read_at: u64, read: u64) -> bool {
    at_ms != 0 && now_ms.saturating_sub(at_ms) < R1_COOL_MS && yield_ < R1_FLOOR && read <= read_at.saturating_add(yield_)
}

/// A rung's relief: the measured drop of the read across it (an ask sent counts nothing until it lands).
pub fn relief(before: u64, after: u64) -> u64 {
    before.saturating_sub(after)
}

/// Whether `r` has anything to give in `s` (a rung that has nothing is skipped, not waited on).
pub fn applies(r: Rung, who: Who, s: &State) -> bool {
    match r {
        Rung::AllocReturn => !s.r1_cooling,
        Rung::TrimParked => s.parked != 0,
        Rung::EvictElastic => s.elastic != 0,
        Rung::NarrowScans => !s.narrowed,
        Rung::OwnSpill => who == Who::Asker && s.asker_held > s.asker_fair && s.asker_spillable != 0,
        Rung::Revoke => s.revocable != 0,
        Rung::Wait => !s.waited,
        // the protected cache goes only to avoid a cancel: past the wait (the hook's is one tick, D4353)
        Rung::EvictProtected => s.protected != 0 && s.waited,
        // `ladder_kill_read`: and only while the unreclaimable bytes are past the line: file pages the kernel drops first
        // never cancel a statement
        Rung::Cancel => !s.cancel_in_flight && s.waited && kill_need(s) != 0,
    }
}

/// The next rung to run after `after` (`None`: the first), or `None` when nothing is needed or nothing is left. A climb
/// re-reads `s` after each rung and asks again: a rung that covered the need ends it.
pub fn next(who: Who, s: &State, after: Option<Rung>) -> Option<Rung> {
    if need(s) == 0 {
        return None;
    }
    let ord = order(who);
    let from = after.and_then(|a| ord.iter().position(|&r| r == a)).map_or(0, |i| i + 1);
    ord[from..].iter().copied().find(|&r| applies(r, who, s))
}

/// D4353: the hook's wait ended: a pass ran R1-R4b at `since_ms` (0 = none in this episode) and the need is still there
/// `HOOK_GRACE_MS` later.
pub fn hook_waited(since_ms: u64, now_ms: u64) -> bool {
    since_ms != 0 && now_ms.saturating_sub(since_ms) >= HOOK_GRACE_MS
}

/// R4a: how much the asker spills of its own: down to its fair share, at most its spillable bytes, at most the need.
pub fn own_spill(s: &State) -> u64 {
    if s.asker_held <= s.asker_fair {
        return 0;
    }
    (s.asker_held - s.asker_fair).min(s.asker_spillable).min(need(s))
}

/// Section 0c: a statement's fair share: its own admitted grant (PostgreSQL's work_mem promise), or an equal split of the
/// budget's elastic part over the statements holding grant memory now, whichever is more.
pub fn fair(grant: u64, elastic_budget: u64, active: u64) -> u64 {
    grant.max(elastic_budget / active.max(1))
}

/// R2a's elastic bytes of one cache (D4353): above its floor, and no more than its own reclaim can free now (`trimmable`,
/// where the cache counts it). A floor that leaves out bytes no reclaim frees made R2a promise what it could not give.
pub fn elastic_of(current: u64, floor: u64, trimmable: Option<u64>) -> u64 {
    let over = current.saturating_sub(floor);
    trimmable.map_or(over, |t| over.min(t))
}

/// D4374 (section 4, one cancel in flight): R6's last cancel (raised at `at_ms`, the read then `read_at`, the victim's
/// `victim` bytes) is still landing at `now_ms`: within `landing_ms` of it (the measured landing, `cancel_landing_ms`) and
/// the read has not fallen by half the victim's bytes. A victim that answered and ended still gives its bytes back over
/// that time; a second cancel then takes an innocent statement for bytes already on their way.
/// D4426: a read that rose past `read_at` by more than the victim's bytes is not that victim landing (every byte of it
/// back would still leave the read above where the cancel found it): another statement grows, and R6 picks again. The
/// hold kept R6 off for the whole window while a part-cache fill took the read 1.6 GB past the line to a kernel OOM kill.
pub fn r6_landing(now_ms: u64, at_ms: u64, landing_ms: u64, read_at: u64, read_now: u64, victim: u64) -> bool {
    at_ms != 0
        && now_ms < at_ms.saturating_add(landing_ms)
        && read_now.saturating_add(victim / 2) > read_at
        && read_now <= read_at.saturating_add(victim)
}

/// D4374 (`r6_land_past_cancel`): past the watchdog's cancel line, an answered R6 victim's landing holds the next R6 only
/// while the read is not rising: `read_now` at most `read_prev`, the read the last test saw (the read at the cancel
/// before any; 0 = none noted: no hold).
pub fn r6_holds_past_cancel(read_now: u64, read_prev: u64) -> bool {
    read_prev != 0 && read_now <= read_prev
}

/// The R6 landing judged by attribution: whether the victim's statement still `holds` its bytes (its account, latched at
/// the cancel by its backend and its statement's id, above the slack its threads' unfolded door-1 bytes leave; a closed
/// statement holds none). A byte the victim freed is no longer its own, whether the allocator gave the page back at once
/// or keeps it committed for the next allocation (a purger's later tick, reuse before release), so the read alone can
/// stay up while the cancel has landed. Within `landing_ms` of the cancel only, and D4426's rise still ends the hold.
pub fn r6_victim_landing(now_ms: u64, at_ms: u64, landing_ms: u64, read_at: u64, read_now: u64, victim: u64, holds: bool) -> bool {
    at_ms != 0 && now_ms < at_ms.saturating_add(landing_ms) && holds && read_now <= read_at.saturating_add(victim)
}

/// The purger periods the landing waits at most after its victim let go (`r6_let_go_pending`).
pub const R6_PURGE_WAIT_TICKS: u64 = 2;

/// After the victim let go of its bytes, their pages are still on their way out of the memory: the freeing threads
/// decommit them over the next milliseconds, or, under a purger that owns the arenas' purge (`owns`), the purger's next
/// tick does. Meanwhile the read still counts them, and a cancel now would take a second statement for bytes on their way
/// back. So the landing holds after the let-go (at `let_go_ms`) until the read fell by half the victim's bytes since the
/// cancel (`read_at`, D4374's test), or, with an owning purger, one of its passes begun after the let-go has finished
/// (its count `passes_now` reached `mark`, the count it had to reach when the let-go was first seen), whichever comes first. Never
/// longer than `R6_PURGE_WAIT_TICKS` of the purger's period (`tick_ms`): a starved purger or a grower that keeps the read
/// up does not hold R6 longer.
#[allow(clippy::too_many_arguments)]
pub fn r6_let_go_pending(
    owns: bool,
    mark: u64,
    passes_now: u64,
    let_go_ms: u64,
    now_ms: u64,
    tick_ms: u64,
    read_at: u64,
    read_now: u64,
    victim: u64,
) -> bool {
    now_ms < let_go_ms.saturating_add(tick_ms.saturating_mul(R6_PURGE_WAIT_TICKS))
        && read_now.saturating_add(victim / 2) > read_at
        && (!owns || passes_now < mark)
}

/// The purger's ticks in a row with no cause left (no ask, the read under the line) that end its pressure mode.
pub const PURGE_MODE_QUIET_TICKS: u8 = 2;

/// The purger's pressure mode: on, and the quiet ticks in a row so far (`purge_mode_tick`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PurgeMode {
    pub on: bool,
    pub quiet: u8,
}

/// What made a purger tick enter its pressure mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PurgeModeCause {
    /// The tick's own read past the ladder's line.
    OwnRead,
    /// A ladder pass that left a need asked for it.
    Ask,
    /// An R6 victim's let-go asked for it.
    LetGo,
}

/// One purger tick under its pressure mode `m`: whether it is a pressure tick (every free committed run of every arena
/// purged), the mode after it, and what made it enter the mode, if it did. A tick enters the mode when its own read is
/// `past` the ladder's line, or a ladder pass that left a need (`ask`) or an R6 victim's let-go (`let_go`) asked since the
/// last tick. In the mode every tick is a pressure tick until the read is `back` at or under the line less the ladder's
/// HIGH (the room R2a takes to; that tick no longer purges), or `PURGE_MODE_QUIET_TICKS` ticks in a row had no ask and the
/// read under the line (the last of them still purges), whichever comes first. A read that dips just under the line no
/// longer ends the purges while the pages freed by then wait out the purge delay and the next grower commits fresh pages
/// beside them (objects past one bitmap chunk never take a committed free run).
pub fn purge_mode_tick(m: PurgeMode, past: bool, ask: bool, let_go: bool, back: bool) -> (bool, PurgeMode, Option<PurgeModeCause>) {
    if !m.on {
        let cause = if past {
            PurgeModeCause::OwnRead
        } else if let_go {
            PurgeModeCause::LetGo
        } else if ask {
            PurgeModeCause::Ask
        } else {
            return (false, m, None);
        };
        return (true, PurgeMode { on: true, quiet: 0 }, Some(cause));
    }
    if back && !past {
        return (false, PurgeMode::default(), None);
    }
    let quiet = if past || ask || let_go { 0 } else { m.quiet.saturating_add(1) };
    if quiet >= PURGE_MODE_QUIET_TICKS {
        return (true, PurgeMode::default(), None);
    }
    (true, PurgeMode { on: true, quiet }, None)
}

/// Kill safety: past the watchdog's cancel line R6's cancel not yet answered holds the next one this long at most. A
/// victim that ignores its cancel (or answers late) must not keep R6 off while the read climbs to a kernel kill. CHOSEN.
pub const R6_ACK_BOUND_MS: u64 = 250;

/// Kill safety: whether R6's cancel raised at `raised_ms` and not yet answered still holds the next one at `now_ms`:
/// under the cancel line always, past it (`past_cancel`) for `R6_ACK_BOUND_MS`.
pub fn r6_ack_holds(now_ms: u64, raised_ms: u64, past_cancel: bool) -> bool {
    !past_cancel || raised_ms == 0 || now_ms < raised_ms.saturating_add(R6_ACK_BOUND_MS)
}

/// Section 0b: R2a's floor for one cache: the bytes in use, or its protected share, whichever is more.
pub fn r2a_floor(in_use: u64, protected_c: u64) -> u64 {
    in_use.max(protected_c)
}

/// Section 0b: one cache's protected bytes not in use, R2b's: what R2a leaves above the bytes in use, at most what its
/// reclaim can free (`trimmable`).
pub fn protected_left(current: u64, in_use: u64, protected_c: u64, trimmable: Option<u64>) -> u64 {
    elastic_of(current, in_use, trimmable).saturating_sub(elastic_of(current, r2a_floor(in_use, protected_c), trimmable))
}

/// Section 0b: the protected cache bytes of a budget `b`.
pub fn protected_of(b: u64) -> u64 {
    b / 100 * PROTECTED_PCT + b % 100 * PROTECTED_PCT / 100
}

/// Section 0b: `protected` split across caches by their caps (fixed at the pool's arming), into `out` in `caps`' order;
/// a cache with no cap gets none. The parts sum to at most `protected`.
pub fn split_floors(protected: u64, caps: &[u64], out: &mut [u64]) {
    let total: u128 = caps.iter().map(|&c| c as u128).sum();
    for (o, &c) in out.iter_mut().zip(caps) {
        *o = if total == 0 { 0 } else { (protected as u128 * c as u128 / total) as u64 };
    }
}

/// Ladder C1 (CACHE-IMPLEMENTATION-PLAN section C1 + section 7): the caches counted at their protected floors, the
/// ladder the one that evicts above them (`PGRUST_MEMPOLICY=cache_floor=1`; default off, and acting only with the ladder
/// on: it flips together with `gate_now`).
#[inline]
pub fn cache_floor() -> bool {
    crate::consts::get(crate::consts::K::CacheFloor) != 0
}

/// Ladder C1 try (`inline_on_draw`): the inline take at a statement's fresh maps (use), not at a grant's reservation.
pub fn inline_on_draw() -> bool {
    crate::consts::get(crate::consts::K::InlineOnDraw) != 0
}

/// Ladder C1 (`cache_floor`): R2a's cache order, cheapest refill first by C0's measured costs (cachemeasure, 10-16: the
/// part cache >> the condition cache > fp tables > the zone store), until cachetrack's curves rank them: the build pool
/// (unmeasured) and the zone store, fp tables, the condition cache, the part cache last. `reclaim::ORDER`'s caches in
/// another order (off: `ORDER`, the part cache third).
pub const C1_ORDER: [memledger::Consumer; 5] = [
    memledger::Consumer::BuildPool,
    memledger::Consumer::ZoneStore,
    memledger::Consumer::FpTables,
    memledger::Consumer::CondCache,
    memledger::Consumer::PartCache,
];

/// Ladder C1 (the user's give-back bar, 10-16: 90 % of a cut of at most 2 GiB back within 100 ms, a rate in ms/GiB past
/// that): the most one background pass reclaims; a drain past it goes on at the next samples, a batch each.
pub const C1_BATCH_MAX: u64 = 2 << 30;

/// Ladder C1b (the user via the broker, 10-16: the ladder sizes the part cache, the fp tables and the condition cache
/// under `cache_floor`): a sized cache's own trim past its budget stops this far under it (at most a quarter), not a
/// quarter under, so a cache at its budget does not drop 25 % per overflow. 32 MiB was the first value tried: MEASURED it
/// costs q30 / q35 instructions in the overflow cell; `cache_trim_step_mb` defaults to 256 (a quarter under a raised
/// budget evicted the parts of the statement that filled it).
pub const C1_TRIM_STEP: u64 = 32 * MIB;

/// Ladder C1c (the user's three marks, plan section 10): TOP = HIGH + the gap. Under LOW the ladder takes to HIGH; past
/// TOP it raises the budgets of the sized caches with demand until the headroom would be back at HIGH; between, nothing.
pub fn marks3(limit: u64, burst: Option<u64>) -> (u64, u64, u64) {
    let (low, high) = marks_at(limit, burst);
    (low, high, high.saturating_add(gap(limit)))
}

/// Ladder C1c: a raise's step (a sized cache's budget up by this much at a time, never force-filled). CHOSEN, not
/// measured. C1d: the least raise (`raise_bytes`), and how close to its budget a cache must be to have demand.
pub const C1_RAISE_STEP: u64 = 32 * MIB;
/// Ladder C1c: a sized cache has demand when it is within one step of its budget and evicted or trimmed within this long.
/// C1d v2 (the C1d re-take: raises filled down to HIGH from a room read between
/// statements, the next W8 statement's draw went under LOW and took them back, 8 reversals in 16 analytic-suite passes): also the
/// window a raise looks back over, for the lowest room (`raise_room`) and the bytes the cache evicted (`raise_bytes`). A
/// minute holds many W8 statements' peaks (a hot analytic-suite W8 statement draws its peak within a second, a TPC-H SF10 W8 one
/// within a few), so a raise leaves the largest recent draw its bytes; and the room a draw that stopped coming held back
/// is the caches' again a window later (read in two halves: 30-60 s).
pub const C1_DEMAND_MS: u64 = 60_000;
/// Ladder C1c: a ladder take of a cache within this long of its last raise is a reversal (the raise was too much).
pub const C1_REVERSAL_MS: u64 = 10_000;
/// Ladder C1c: each recent reversal doubles a cache's raise interval (one tick), at most this many times (~51 s); a cache
/// with no reversal for `C1_DEMAND_MS` is back at one tick. CHOSEN.
pub const C1_BACKOFF_MAX: u32 = 9;

/// Ladder C1c: the bytes the raises may give this sample: past TOP, what brings the headroom `room` back to HIGH; else 0.
/// C1d v2: what brings the lowest headroom of the last `C1_DEMAND_MS` (`low`, the ladder's samples' every tick; this
/// sample's at most) back to HIGH, so a raise never takes the bytes the window's largest statement used.
pub fn raise_room(room: u64, low: u64, high: u64, top: u64) -> u64 {
    if room > top { room.min(low).saturating_sub(high) } else { 0 }
}

/// Ladder C1: `raise_room`, at most `claimed`: the least room the claims left in the same window (the grant pool's room
/// after a take, B's free bytes over its low mark; `u64::MAX` = none noted). Room by use the claims hold back is not the
/// caches': a raise into it was taken back at the next take the pool or B was short for.
pub fn raise_room_within(room: u64, low: u64, high: u64, top: u64, claimed: u64) -> u64 {
    raise_room(room, low, high, top).min(claimed)
}

/// Ladder C1b, the hold's reach (H5a knobs): of the raise room `left` after a sample's raises, what the part cache's
/// hold may reach past its budget. All of it with `reach` (cache_hold_reach) where a raise's timing would let one run
/// now (`due`), or whatever the timing with the timing gate off (`gate` = cache_hold_reach_gate 0); none with `reach`
/// off. Both on (the defaults) is the landed shape: `left` when due, else 0.
pub fn hold_reach_room(left: u64, due: bool, reach: bool, gate: bool) -> u64 {
    if reach && (due || !gate) { left } else { 0 }
}

/// Ladder C1c: whether a sized cache holding `held` under its ladder `budget` has demand at `now_ms`: within one step of
/// the budget and its last eviction or trim (`evicted_at_ms`, 0 = none seen) within `C1_DEMAND_MS`.
pub fn has_demand(held: u64, budget: u64, evicted_at_ms: u64, now_ms: u64) -> bool {
    held.saturating_add(C1_RAISE_STEP) >= budget && evicted_at_ms != 0 && now_ms.saturating_sub(evicted_at_ms) <= C1_DEMAND_MS
}

/// Ladder C1b: the bytes the part cache, holding `held`, sits past its ladder `budget` once its floor (`floor`, the
/// bytes its trim cannot drop) is back under the budget: the bytes its own trim kept while the floor held it, demand a
/// raise may give back (`raise_bytes`) when there is room, so the budget follows the cache and the ladder stays its
/// sizer. 0 while the floor holds it, at or under the budget, or with no cheap count of its bytes (`u64::MAX`).
pub fn held_past(held: u64, budget: u64, floor: u64) -> u64 {
    if held == u64::MAX || floor >= budget { 0 } else { held.saturating_sub(budget) }
}

/// Ladder C1d (the C1d bar: 32 MiB raises grew the part cache 4.8 -> 7.3 GiB in 16
/// analytic-suite W8 hot passes, still evicting 7.8 GiB a pass): a raise gives back what the cache evicted itself under its budget
/// since its last raise (or the ladder's last take from it), at least `C1_RAISE_STEP`, at most `room` (what keeps the
/// headroom at HIGH); 0 when `room` is under a step. A raise moves only the budget: the cache fills only what it loads.
/// C1d v2: `evicted_since` counts only the last `C1_DEMAND_MS` (the caller's window), so evictions older than the window
/// (before a long backoff or a quiet spell) are no raise.
pub fn raise_bytes(evicted_since: u64, room: u64) -> u64 {
    let b = evicted_since.max(C1_RAISE_STEP).min(room);
    if b < C1_RAISE_STEP { 0 } else { b }
}

/// Ladder C1c: a cache's least time between raises (ms) after `recent` reversals: one tick, doubled per reversal.
pub fn raise_interval_ms(recent: u32) -> u64 {
    TICK_MS << recent.min(C1_BACKOFF_MAX)
}

/// Ladder C1: the most a sized cache's refill quiet period grows to (`refill_quiet_ms`). CHOSEN.
pub const C1_QUIET_MAX_MS: u64 = 600_000;

/// Ladder C1c, the refault gate (`cache_refault_gate`): the raises wait while the limiting cgroup's file pages refault
/// past this percent of the limit a minute. A raise grows anon into memory the page cache would hold; a refault is a
/// file page read again soon after it was dropped. CHOSEN from the coremem c-floor stress at a 16 GiB limit: its
/// fixed-size arm refaulted ~0.7 GiB a minute (4 %), its quietest minutes ~1 %; the raises-on arm ~1.1 GiB (7 %).
/// MEASURED with the gate on in that stress: the rate stayed past 2 % nearly all run (the gate held 12102 passes) and
/// refaults did not fall, so the gate is off by default (`cache_refault_gate` 0).
pub const C1_REFAULT_PCT: u64 = 2;
/// Ladder C1c: the span the refault gate counts over.
pub const C1_REFAULT_WINDOW_MS: u64 = 60_000;

/// Ladder C1c: whether `per_min` file bytes refaulted in a minute leave the raises open under `limit`.
pub fn refaults_allow_raise(per_min: u64, limit: u64) -> bool {
    per_min.saturating_mul(100) <= limit.saturating_mul(C1_REFAULT_PCT)
}

/// Ladder C1 (the cache plan's backoff after repeated cuts of the same cache): a sized cache is raised only this long
/// after the ladder's last take from it: `base` (`cache_refill_quiet_ms`) doubled per reversal since its quiet period
/// last reset, at most `C1_QUIET_MAX_MS`. In a stress whose claims come in bursts a raise in a calm stretch was taken
/// back at the next burst (22 reversals in a 1320 s run at a 60 s window); each such reversal now waits twice as long.
pub fn refill_quiet_ms(base: u64, shift: u32) -> u64 {
    base.saturating_mul(1u64 << shift.min(20)).min(C1_QUIET_MAX_MS.max(base))
}

/// Ladder C1, S1 criterion 11 (`ladder_log_ms`): whether a sized cache's takes (or raises) since its last such line are
/// logged at `now_ms` (the ladder sample's clock), that line at `last_ms` (0 = none yet): the knob past 0 and at least that
/// long since. 0 = never: the views still count every take and raise.
pub fn take_log_due(now_ms: u64, last_ms: u64) -> bool {
    let every = crate::consts::get(crate::consts::K::LadderLogMs);
    every != 0 && (last_ms == 0 || now_ms.saturating_sub(last_ms) >= every)
}

/// Ladder C1, S1 criterion 11: whether the takes and raises are logged at all (`ladder_log_ms` past 0).
pub fn take_log_on() -> bool {
    crate::consts::get(crate::consts::K::LadderLogMs) != 0
}

/// Ladder C1: the background trigger stays quiet (no walk) under this many reclaimable bytes (the broker's 10-16 note:
/// today's reclaimer ticked 284 times with 0 bytes to free). CHOSEN.
pub const C1_QUIET_MIN: u64 = 4 * MIB;

/// Ladder C1: the give-back rate (ns per GiB, a reclaim's start to its bytes given back) until one is timed: memacct2's
/// measure of the allocator's decommit, ~44 ms per GiB on one thread.
pub const GIVE_BACK_NS_PER_GIB_PRIOR: u64 = 44_000_000;

/// Ladder C1: the least reclaim timed into the give-back rate (a smaller one's fixed costs read as a slow rate). CHOSEN.
pub const GIVE_BACK_TIMED_MIN: u64 = 64 * MIB;

fn pct_of(limit: u64, p: u64) -> u64 {
    limit / 100 * p + limit % 100 * p / 100
}

/// Ladder C1: the gap between the marks (`cache_gap_pct` of the limit; the user's decision 10-15: 5 %).
pub fn gap(limit: u64) -> u64 {
    pct_of(limit, crate::consts::get(crate::consts::K::CacheGapPct))
}

/// Ladder C1: one background batch at `limit`: the gap, at most `C1_BATCH_MAX` (the 5 % gap is under it to ~40 GiB).
pub fn batch(limit: u64) -> u64 {
    gap(limit).clamp(1, C1_BATCH_MAX)
}

/// Ladder C1: the window LOW covers (ms): one batch's give-back at `ns_per_gib`, and one tick (the sample that sees the
/// headroom under LOW). LOW is the burst over it: the p99 rise of the ladder's read over windows this long.
pub fn low_window_ms(limit: u64, ns_per_gib: u64) -> u64 {
    let ns = (batch(limit) as u128 * ns_per_gib as u128).div_ceil(1 << 30);
    (ns.div_ceil(1_000_000) as u64).saturating_add(TICK_MS)
}

/// Ladder C1: the (LOW, HIGH) marks on the headroom under the ladder's line for `limit` (bytes), before a burst is
/// measured: LOW at its most, `cache_low_pct` of the limit; HIGH the gap more.
pub fn marks(limit: u64) -> (u64, u64) {
    marks_at(limit, None)
}

/// Ladder C1: the marks with `burst` (the p99 rise over `low_window_ms`; `None` = not measured yet): LOW the burst, at
/// least `MARGIN_MIN` and at most `cache_low_pct` of the limit (its value before a measure); HIGH = LOW + the gap.
pub fn marks_at(limit: u64, burst: Option<u64>) -> (u64, u64) {
    let most = pct_of(limit, crate::consts::get(crate::consts::K::CacheLowPct));
    let low = burst.map_or(most, |b| b.clamp(MARGIN_MIN.min(most), most));
    (low, low.saturating_add(gap(limit)))
}

/// Ladder C1, the background trigger (the ladder's thread, every sample): the headroom a drain aims at with `room` (the
/// line less the read) and `marks`, `draining` the one it aimed at the sample before (0 = none): HIGH from under LOW,
/// then on to it at the next samples (a batch each) until it is reached; 0 = nothing to reclaim.
pub fn drain_to(room: u64, marks: (u64, u64), draining: u64) -> u64 {
    let (low, high) = marks;
    if room < low {
        high
    } else if room < draining {
        draining
    } else {
        0
    }
}

/// Ladder C1, the on-demand path: a draw of `ask` bytes with `room` left under the line (the last sample's headroom less
/// what the pool drew since) evicts what keeps `low` free after it; 0 when it does already.
pub fn inline_want(ask: u64, room: u64, low: u64) -> u64 {
    ask.saturating_add(low).saturating_sub(room)
}

/// Ladder C1, the refill guard: a cache grows past its protected floor only while the headroom `room` is past `high` (C1c:
/// the callers pass TOP, the user's third mark) and
/// `since_reclaim_ms` (from the ladder's last reclaim; `u64::MAX` = none yet) is at least `cache_refill_quiet_ms`.
pub fn refill_open(room: u64, high: u64, since_reclaim_ms: u64) -> bool {
    room > high && since_reclaim_ms >= crate::consts::get(crate::consts::K::CacheRefillQuietMs)
}

/// Ladder C1: the give-back rate (ns per GiB) after a reclaim of `bytes` that took `ns` from its start to its bytes given
/// back: a quarter of the way from `prev` (0 = none yet: the prior) to its own; one under `GIVE_BACK_TIMED_MIN` leaves it.
pub fn give_back_rate(prev: u64, bytes: u64, ns: u64) -> u64 {
    let prev = if prev == 0 { GIVE_BACK_NS_PER_GIB_PRIOR } else { prev };
    if bytes < GIVE_BACK_TIMED_MIN {
        return prev;
    }
    let own = ((ns as u128) << 30).div_ceil(bytes as u128).min(u64::MAX as u128) as u64;
    (prev / 4 * 3).saturating_add(own / 4).max(1)
}

/// Ladder C1: the caches' term of the grant pool armed out of B (`sizes::derived_pool_in_b`): every cache at its cap
/// (`at_caps`, today's), or under `cache_floor` their protected floors, `protected_of(b)` split by their caps, never more
/// than the caps.
pub fn pool_caches(at_caps: u64, b: u64, floors: bool) -> u64 {
    if floors { at_caps.min(protected_of(b)) } else { at_caps }
}

/// Section 7 + step 3b (main, after the 8 GiB probe): the ladder's line for `limit`. It is the limit less the margin,
/// and while the watchdog's cancel is armed at `cancel_pct` percent (`pgrust.memory_cancel_threshold` > 0) it is no
/// higher than that cancel's line less the margin, so the gates climb before the backstop cancels. On the line of
/// `limit - MARGIN_MIN` (8128 MB at 8 GiB, above the 90 % cancel's 7373 MB) no rung ever fired: the watchdog acted
/// first every time. Step 6 removes the watchdog's cancel and with it the second term.
pub fn line(limit: u64, cancel_pct: i32) -> u64 {
    line_with(limit, cancel_pct, MARGIN_MIN)
}

/// D4389: `line` with a measured `margin` (`margin`) in place of `MARGIN_MIN`.
pub fn line_with(limit: u64, cancel_pct: i32, margin: u64) -> u64 {
    let own = limit.saturating_sub(margin);
    if cancel_pct <= 0 {
        return own;
    }
    // the cancel's own test: resident >= (limit / 100) x pct (mempolicy::watchdog::cancel_at)
    let cancel = (limit / 100).saturating_mul(cancel_pct.clamp(1, 100) as u64);
    own.min(cancel.saturating_sub(margin))
}

/// D4389 (section 7, the measured margin): the p99 `rise` of the governed resident over a cancel's landing and one tick
/// (bytes over `cancel_landing_ms + TICK_MS`: `margin_for` of that growth; `None` = no history yet, a server's first
/// second), at least `MARGIN_MIN` (a quiet server: today's line), at most `MARGIN_CAP_PCT` of `limit`.
pub fn margin(limit: u64, rise: Option<u64>) -> u64 {
    let cap = (limit / 100).saturating_mul(MARGIN_CAP_PCT).max(MARGIN_MIN);
    rise.unwrap_or(0).clamp(MARGIN_MIN, cap)
}

/// D4389: the window `margin`'s rise is measured over (ms): a cancel's landing and one tick.
pub fn margin_window_ms(land_ms: u64) -> u64 {
    land_ms.saturating_add(TICK_MS)
}

/// Section 8: a waiter's budget (`bytes_wait`) counts from the last free it saw, so one whose bytes are coming is not refused
/// while they come; at most `WAIT_CAP_X` budgets in all, so frees that others keep taking cannot hold it forever. CHOSEN.
pub const WAIT_CAP_X: u32 = 4;

/// Section 8: whether a wait `since_free` after the last free it saw and `total` after its start has spent its `budget`.
pub fn wait_spent(since_free: core::time::Duration, total: core::time::Duration, budget: core::time::Duration) -> bool {
    since_free >= budget || total >= budget.saturating_mul(WAIT_CAP_X)
}

/// Section 7: the margin for a p99 growth of `g99` bytes a second over a cancel's landing of `land_ms` and one tick.
pub fn margin_for(g99: u64, land_ms: u64) -> u64 {
    let ms = land_ms.saturating_add(TICK_MS) as u128;
    ((g99 as u128 * ms / 1_000) as u64).max(MARGIN_MIN)
}

/// A cancel candidate: its size (section 4: pool bytes + parts pinned alone + resident heap no draw backs), its age, and
/// whether it may be picked (an idle cursor's statement and one already condemned may not).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cand {
    pub size: u64,
    pub elapsed_ms: u64,
    pub idle: bool,
    pub condemned: bool,
}

/// R6: the largest candidate of at least `least` bytes, the asker included; ties to the youngest; `None` when none may
/// be picked.
pub fn victim(cands: impl IntoIterator<Item = Cand>, least: u64) -> Option<usize> {
    let mut best: Option<(usize, u64, u64)> = None;
    for (n, c) in cands.into_iter().enumerate() {
        if c.idle || c.condemned || c.size < least {
            continue;
        }
        let better = match best {
            None => true,
            Some((_, size, elapsed)) => c.size > size || (c.size == size && c.elapsed_ms < elapsed),
        };
        if better {
            best = Some((n, c.size, c.elapsed_ms));
        }
    }
    best.map(|b| b.0)
}

/// The rungs a climb from `s` would run in order, as a bit set over `Rung`'s discriminants (`s` read once: the shadow's
/// view, where nothing runs and nothing is re-read). 0 when nothing is needed.
pub fn plan(who: Who, s: &State) -> u16 {
    let mut mask = 0u16;
    let mut at = None;
    while let Some(r) = next(who, s, at) {
        mask |= 1 << r as u8;
        at = Some(r);
    }
    mask
}

/// `plan`'s rungs by name in order, space-separated ("none" for an empty plan), into `out`.
pub fn plan_names(who: Who, mask: u16, out: &mut String) {
    let start = out.len();
    for &r in order(who) {
        if mask & (1 << r as u8) != 0 {
            if out.len() != start {
                out.push(' ');
            }
            out.push_str(r.name());
        }
    }
    if out.len() == start {
        out.push_str("none");
    }
}

/// Section 10 step 2 (`pgrust.memory_ladder = shadow`): the ladder decides at every sample and acts on nothing; the
/// watchdog still acts. One episode is the run of samples whose need is not 0: a line when it opens, when the climb's plan
/// changes, and when it closes (its peak need and length), so a stress's log says what the ladder would have done and
/// when, beside what the watchdog did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Shadow {
    open_ms: Option<u64>,
    plan: u16,
    peak_need: u64,
    /// Episodes opened so far.
    pub episodes: u64,
}

/// What a shadow sample says (`Shadow::observe`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShadowLine {
    /// An episode opened: its need and plan.
    Open { need: u64, plan: u16 },
    /// The plan of an open episode changed (a rung's input moved).
    Plan { need: u64, plan: u16 },
    /// The episode closed (need back to 0): its peak need and length.
    Close { peak_need: u64, ms: u64 },
}

impl Shadow {
    pub const fn new() -> Shadow {
        Shadow { open_ms: None, plan: 0, peak_need: 0, episodes: 0 }
    }

    /// One sample's `need` and `plan` at `now_ms`: the line to write, if any.
    pub fn observe(&mut self, need: u64, plan: u16, now_ms: u64) -> Option<ShadowLine> {
        match (self.open_ms, need) {
            (None, 0) => None,
            (None, _) => {
                *self = Shadow { open_ms: Some(now_ms), plan, peak_need: need, episodes: self.episodes + 1 };
                Some(ShadowLine::Open { need, plan })
            }
            (Some(at), 0) => {
                let line = ShadowLine::Close { peak_need: self.peak_need, ms: now_ms.saturating_sub(at) };
                *self = Shadow { episodes: self.episodes, ..Shadow::new() };
                Some(line)
            }
            (Some(_), _) => {
                self.peak_need = self.peak_need.max(need);
                if plan == self.plan {
                    return None;
                }
                self.plan = plan;
                Some(ShadowLine::Plan { need, plan })
            }
        }
    }

    /// Whether an episode is open.
    pub fn open(&self) -> bool {
        self.open_ms.is_some()
    }
}

pub const BUFFERS_EXTENT: u64 = 2 * MIB;

/// L6 (plan section 8; C0: a cut pool costs pgbench -S x0.924 at 50 %): shared_buffers after R2a, for what its `elastic`
/// bytes cannot cover of `short`, in whole extents, at most `reclaimable`.
pub fn buffers_ask(short: u64, elastic: u64, reclaimable: u64) -> u64 {
    match short.saturating_sub(elastic) {
        0 => 0,
        want => want.div_ceil(BUFFERS_EXTENT).saturating_mul(BUFFERS_EXTENT).min(reclaimable),
    }
}

/// L6: ms `landing` pool bytes count as landing: 1 s per GiB clean, plus all of them dirty at `write_bw` (0: none), a tick.
pub fn buffers_due_ms(landing: u64, write_bw: u64) -> u64 {
    let clean = (landing as u128 * 1_000).div_ceil(1 << 30);
    let dirty = if write_bw == 0 { 0 } else { (landing as u128 * 1_000).div_ceil(write_bw as u128) };
    ((clean + dirty).min(u64::MAX as u128) as u64).saturating_add(TICK_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    /// `ladder_kill_read` (USER 10-18): past the wait R6 runs only while the kill read (the governed resident) with the
    /// ask is past the line; the shrink rungs climb on the ladder's read whatever it.
    #[test]
    fn r6_reads_the_unreclaimable_bytes_and_the_shrink_rungs_the_ladder_read() {
        let s = State { read: 16 * GIB, line: 15 * GIB, waited: true, protected: GIB, ..State::default() };
        assert!(applies(Rung::Cancel, Who::Hook, &s), "no kill read: the read, as before");
        let k = State { kill_read: Some(10 * GIB), ..s };
        assert_eq!((need(&k), kill_need(&k)), (GIB, 0));
        assert!(!applies(Rung::Cancel, Who::Hook, &k), "the unreclaimable bytes under the line: no cancel");
        assert!(applies(Rung::EvictProtected, Who::Hook, &k), "the shrink rungs still climb on the ladder's read");
        assert_eq!(next(Who::Hook, &k, Some(Rung::EvictProtected)), None, "nothing after R2b while under the line");
        let k = State { kill_read: Some(15 * GIB + MIB), ..s };
        assert_eq!(kill_need(&k), MIB);
        assert!(applies(Rung::Cancel, Who::Hook, &k));
        assert_eq!(kill_need(&State { kill_read: Some(20 * GIB), ..s }), GIB, "never above the read");
        assert_eq!(kill_need(&State { kill_read: Some(14 * GIB), ask: 2 * GIB, ..s }), GIB, "the ask counts in it");
    }

    fn short() -> State {
        State { read: 15 * GIB, line: 15 * GIB, ask: 256 * MIB, elastic: GIB, protected: GIB, revocable: GIB, asker_held: 2 * GIB, asker_fair: GIB, asker_spillable: GIB, ..State::default() }
    }

    fn climb(who: Who, s: &State) -> Vec<Rung> {
        let mut seen = Vec::new();
        let mut at = None;
        while let Some(r) = next(who, s, at) {
            seen.push(r);
            at = Some(r);
        }
        seen
    }

    #[test]
    fn the_holds_reach_knobs_all_on_are_the_landed_shape() {
        let left = 3 * GIB;
        assert_eq!(hold_reach_room(left, true, true, true), left, "all on, due: the raise room (the landed shape)");
        assert_eq!(hold_reach_room(left, false, true, true), 0, "all on, not due: none (the landed shape)");
        assert_eq!(hold_reach_room(left, false, true, false), left, "the timing gate off: the room whatever the timing");
        assert_eq!(hold_reach_room(left, true, false, true), 0, "the reach off: none, the floor plus a step bounds the hold");
        assert_eq!(hold_reach_room(left, true, false, false), 0, "the reach off whatever the gate");
    }

    #[test]
    fn the_asker_climbs_in_the_ruled_order_and_the_protected_cache_goes_only_past_the_wait() {
        let s = short();
        assert_eq!(climb(Who::Asker, &s), [Rung::AllocReturn, Rung::EvictElastic, Rung::NarrowScans, Rung::OwnSpill, Rung::Revoke, Rung::Wait]);
        let waited = State { waited: true, ..s };
        assert_eq!(climb(Who::Asker, &waited), [Rung::AllocReturn, Rung::EvictElastic, Rung::NarrowScans, Rung::OwnSpill, Rung::Revoke, Rung::EvictProtected, Rung::Cancel]);
        assert_eq!(next(Who::Asker, &waited, Some(Rung::Revoke)), Some(Rung::EvictProtected), "R2b only after the wait, before the cancel");
    }

    /// P3: R2p (the parked regions and slot sets) comes after R1 and before R2a in both orders, only when the climb was
    /// handed parked bytes (the parked_trim_rung knob; with it off `parked` stays 0 and the orders are today's).
    #[test]
    fn p3_the_parked_trim_runs_after_r1_and_before_r2a_only_with_parked_bytes() {
        let s = State { parked: 512 * MIB, ..short() };
        assert_eq!(climb(Who::Asker, &s)[..3], [Rung::AllocReturn, Rung::TrimParked, Rung::EvictElastic]);
        assert_eq!(climb(Who::Hook, &s)[..3], [Rung::AllocReturn, Rung::TrimParked, Rung::EvictElastic]);
        assert!(!climb(Who::Asker, &short()).contains(&Rung::TrimParked), "no parked bytes handed: no R2p");
        assert!(!climb(Who::Hook, &short()).contains(&Rung::TrimParked));
        assert_eq!(Rung::TrimParked.name(), "R2p");
    }

    #[test]
    fn the_hook_has_no_own_spill_and_cancels_only_past_its_one_tick_wait() {
        let s = short();
        assert_eq!(climb(Who::Hook, &s), [Rung::AllocReturn, Rung::EvictElastic, Rung::NarrowScans, Rung::Revoke, Rung::Wait], "no cancel in the pass that ran R1-R4b");
        let waited = State { waited: true, ..s };
        assert_eq!(climb(Who::Hook, &waited), [Rung::AllocReturn, Rung::EvictElastic, Rung::NarrowScans, Rung::Revoke, Rung::EvictProtected, Rung::Cancel]);
        assert!(!hook_waited(0, 10_000), "no pass ran R1-R4b in this episode");
        assert!(!hook_waited(10_000, 10_000 + HOOK_GRACE_MS - 1), "within the tick: the earlier rungs and the statements' ends get it");
        assert!(hook_waited(10_000, 10_000 + HOOK_GRACE_MS), "a tick on, the need still there: R2b, then R6");
    }

    #[test]
    fn nothing_is_climbed_while_the_ask_fits_under_the_line() {
        let s = State { read: 10 * GIB, line: 15 * GIB, ask: GIB, ..short() };
        assert_eq!(need(&s), 0);
        assert_eq!(next(Who::Asker, &s, None), None);
        assert_eq!(need(&State { ask: 6 * GIB, ..s }), GIB);
    }

    #[test]
    fn a_rung_with_nothing_to_give_is_skipped() {
        let s = State { r1_cooling: true, elastic: 0, narrowed: true, revocable: 0, ..short() };
        assert_eq!(next(Who::Asker, &s, None), Some(Rung::OwnSpill));
        let s = State { asker_held: GIB, ..s };
        assert_eq!(next(Who::Asker, &s, None), Some(Rung::Wait), "within its fair share the asker does not spill itself");
    }

    #[test]
    fn the_asker_spills_only_past_its_fair_share_and_only_down_to_it() {
        let s = short();
        assert_eq!(own_spill(&s), 256 * MIB, "at most the need");
        assert_eq!(own_spill(&State { ask: 4 * GIB, ..s }), GIB, "down to its fair share (2 GiB held, 1 GiB fair)");
        assert_eq!(own_spill(&State { asker_spillable: 64 * MIB, ask: 4 * GIB, ..s }), 64 * MIB, "at most its spillable bytes");
        assert_eq!(own_spill(&State { asker_held: GIB, ..s }), 0);
    }

    /// D4389: the measured margin: a quiet server (or no history) keeps `MARGIN_MIN`, today's line; a steady growth sets
    /// the margin to its rise over the window; a burst past a tenth of the limit is capped there (at least `MARGIN_MIN`).
    #[test]
    fn d4389_the_margin_is_the_measured_rise_between_the_minimum_and_a_tenth_of_the_limit() {
        let limit = 1536 * MIB;
        assert_eq!(margin(limit, None), MARGIN_MIN, "no history: today's margin");
        assert_eq!(margin(limit, Some(0)), MARGIN_MIN, "a quiet server: today's margin");
        assert_eq!(margin(limit, Some(100 * MIB)), 100 * MIB, "the rise over the window");
        assert_eq!(margin(limit, Some(4 * GIB)), limit / 100 * MARGIN_CAP_PCT, "a burst: capped at a tenth of the limit");
        assert_eq!(margin(256 * MIB, Some(GIB)), MARGIN_MIN, "a small limit: the cap is never under the minimum");
        assert_eq!(line(limit, 90), line_with(limit, 90, MARGIN_MIN), "the constant line is the measured one at the minimum");
        assert_eq!(line_with(limit, 90, margin(limit, Some(4 * GIB))), limit / 100 * 90 - limit / 100 * MARGIN_CAP_PCT);
        assert_eq!(margin_window_ms(2750), 2850, "a cancel's landing and one tick");
    }

    #[test]
    fn r6_lands_by_the_read_or_by_its_landing_time() {
        const M: u64 = MIB;
        assert!(r6_landing(10_006, 10_000, 2_750, 1500 * M, 1490 * M, 960 * M), "bytes still there");
        assert!(!r6_landing(10_006, 10_000, 2_750, 1500 * M, 1020 * M, 960 * M), "fell by half the victim's bytes");
        assert!(!r6_landing(12_750, 10_000, 2_750, 1500 * M, 1490 * M, 960 * M), "past its landing time");
        assert!(!r6_landing(10_006, 0, 2_750, 1500 * M, 1490 * M, 960 * M), "no cancel yet");
    }

    #[test]
    fn r6_lands_when_the_victim_lets_go_whatever_the_read() {
        const M: u64 = MIB;
        assert!(r6_victim_landing(20_815, 20_811, 2_750, 1351 * M, 1292 * M, 600 * M, true), "the victim still holds");
        assert!(!r6_victim_landing(20_815, 20_811, 2_750, 1351 * M, 1292 * M, 600 * M, false), "it let go: the read need not fall");
        assert!(!r6_victim_landing(23_561, 20_811, 2_750, 1351 * M, 1292 * M, 600 * M, true), "past its landing time");
        assert!(!r6_victim_landing(20_900, 20_811, 2_750, 1351 * M, 1952 * M, 600 * M, true), "D4426: another grower");
        assert!(!r6_victim_landing(20_815, 0, 2_750, 1351 * M, 1292 * M, 600 * M, true), "no cancel yet");
    }

    #[test]
    fn after_the_let_go_the_landing_waits_for_one_whole_purger_tick() {
        const M: u64 = MIB;
        let (at, p) = (10_000, |passes, now| r6_let_go_pending(true, 42, passes, 10_000, now, 50, 1351 * M, 1292 * M, 600 * M));
        assert!(p(40, at), "the let-go seen with a pass running at 40 passes: the mark is 42");
        assert!(p(41, at + 60), "that pass finished: it may have begun before the free");
        assert!(!p(42, at + 70), "a pass begun after the let-go finished: its freed pages went back");
    }

    #[test]
    fn a_purger_that_never_ticks_holds_the_landing_two_periods_at_most() {
        const M: u64 = MIB;
        let p = |now| r6_let_go_pending(true, 41, 40, 10_000, now, 50, 1351 * M, 1292 * M, 600 * M);
        assert!(p(10_099), "under two periods: still waiting");
        assert!(!p(10_100), "two periods and no tick: the wait ends");
        assert!(!p(12_000), "a starved purger never holds R6 for the landing time");
    }

    #[test]
    fn with_no_owning_purger_the_let_go_waits_for_the_read_to_fall_two_periods_at_most() {
        const M: u64 = MIB;
        let p = |now, read| r6_let_go_pending(false, 41, 40, 10_000, now, 50, 1414 * M, read, 816 * M);
        assert!(p(10_008, 1314 * M), "8 ms after the let-go the freeing threads are still giving back: 100 of 816 MB fell");
        assert!(!p(10_008, 1005 * M), "the read fell by half the victim's bytes: landed");
        assert!(!p(10_100, 1314 * M), "two periods: landed whatever the read");
        assert!(!r6_let_go_pending(true, 41, 41, 10_000, 10_008, 50, 1414 * M, 1314 * M, 816 * M), "an owning purger's pass ends it too");
    }

    // The ladder wiring witness at 1536M under load, the purger owning the arenas' purge: after the first hog's cancel
    // its freed ranges (500 MB) sat free and committed while the read had dipped to 1211 MB, under the line (1228): the
    // purger's ticks read no pressure and purged only expired ranges, the next hog grew on fresh pages beside them, and R6
    // canceled it 204 ms later at a read of 1407 MB.
    #[test]
    fn a_read_that_dips_under_the_line_keeps_the_purger_in_pressure_mode() {
        let off = PurgeMode::default();
        let (purge, m, cause) = purge_mode_tick(off, false, false, true, false);
        assert!(purge && m.on && cause == Some(PurgeModeCause::LetGo), "the let-go enters the mode with the read under the line");
        let (purge, m, _) = purge_mode_tick(m, false, false, false, false);
        assert!(purge && m.on, "a quiet tick, the read still above the line less HIGH: a pressure tick");
        let (purge, m2, _) = purge_mode_tick(m, true, false, false, false);
        assert!(purge && m2.on && m2.quiet == 0, "the read past the line again: the quiet count restarts");
        let (purge, m3, _) = purge_mode_tick(m, false, false, false, false);
        assert!(purge && !m3.on, "the second quiet tick in a row still purges, then the mode ends");
        let (purge, m4, _) = purge_mode_tick(m2, false, true, false, true);
        assert!(!purge && !m4.on, "the read back at the line less HIGH ends it at once, ask or not");
    }

    #[test]
    fn without_a_cause_no_tick_enters_the_pressure_mode() {
        let off = PurgeMode::default();
        assert_eq!(purge_mode_tick(off, false, false, false, false), (false, off, None), "the read under the line, no ask: an ordinary tick");
        assert_eq!(purge_mode_tick(off, true, true, true, false).2, Some(PurgeModeCause::OwnRead), "its own read counts first");
        assert_eq!(purge_mode_tick(off, false, true, false, false).2, Some(PurgeModeCause::Ask), "a pass that left a need");
    }

    #[test]
    fn past_the_cancel_line_an_unanswered_cancel_holds_r6_for_250_ms_at_most() {
        assert!(r6_ack_holds(10_249, 10_000, true), "249 ms after the cancel: still in flight");
        assert!(!r6_ack_holds(10_250, 10_000, true), "250 ms unanswered past the cancel line: R6 may act again");
        assert!(r6_ack_holds(60_000, 10_000, false), "under the cancel line: held until it is answered");
        assert!(r6_ack_holds(60_000, 0, true), "no cancel raised: the registry's own view holds");
    }

    // D4426 witness (box 19, va-r2, 8G): R6 canceled a 391 MB holder at a read of 6606 MB; within the 2750 ms window the
    // part cache filled and the read rose to 8175 MB, 1569 MB past the read at the cancel, and the hold kept R6 from
    // picking again until the kernel's OOM kill 2.33 s after the cancel. A read past `read_at + victim` is not the victim
    // still landing: all of its bytes back would leave the read above where the cancel found it.
    #[test]
    fn a_read_that_rises_past_the_victims_bytes_ends_the_landing_hold() {
        const M: u64 = MIB;
        assert!(r6_landing(7_368, 5_368, 2_750, 6606 * M, 6606 * M + 391 * M, 391 * M), "up to the victim's bytes: still landing");
        assert!(!r6_landing(7_041, 5_368, 2_750, 6606 * M, 8175 * M, 391 * M), "1569 MB past the read at the cancel: another grower");
        assert!(!r6_landing(5_600, 5_368, 2_750, 6606 * M, 6606 * M + 392 * M, 391 * M), "one byte past the victim's bytes");
    }

    #[test]
    fn one_cancel_in_flight() {
        let s = State { waited: true, protected: 0, cancel_in_flight: true, ..short() };
        assert_eq!(next(Who::Asker, &s, Some(Rung::Revoke)), None, "no second cancel while one lands");
        assert_eq!(next(Who::Hook, &s, Some(Rung::Revoke)), None);
    }

    #[test]
    fn the_victim_is_the_largest_asker_included_ties_to_the_youngest() {
        let c = |size, elapsed_ms| Cand { size, elapsed_ms, idle: false, condemned: false };
        // the asker (index 0) is the largest: it is the one canceled
        assert_eq!(victim([c(3 * GIB, 10), c(2 * GIB, 10)], 32 * MIB), Some(0));
        assert_eq!(victim([c(GIB, 50), c(GIB, 10)], 32 * MIB), Some(1), "ties: the youngest");
        assert_eq!(victim([Cand { idle: true, ..c(9 * GIB, 1) }, Cand { condemned: true, ..c(9 * GIB, 1) }, c(GIB, 1)], 32 * MIB), Some(2));
        assert_eq!(victim([c(MIB, 1)], 32 * MIB), None);
    }

    #[test]
    fn the_shadow_plan_lists_the_rungs_in_order_by_name() {
        let s = short();
        let mut out = String::new();
        plan_names(Who::Hook, plan(Who::Hook, &s), &mut out);
        assert_eq!(out, "R1 R2a R3 R4b R5");
        out.clear();
        plan_names(Who::Hook, plan(Who::Hook, &State { waited: true, ..s }), &mut out);
        assert_eq!(out, "R1 R2a R3 R4b R2b R6");
        out.clear();
        plan_names(Who::Asker, plan(Who::Asker, &s), &mut out);
        assert_eq!(out, "R1 R2a R3 R4a R4b R5");
        out.clear();
        plan_names(Who::Hook, plan(Who::Hook, &State { read: 0, ..s }), &mut out);
        assert_eq!(out, "none", "nothing needed, nothing planned");
    }

    #[test]
    fn a_shadow_episode_opens_changes_and_closes_with_its_peak_and_length() {
        let mut sh = Shadow::new();
        assert_eq!(sh.observe(0, 0, 0), None, "under the line: no episode");
        assert_eq!(sh.observe(GIB, 0b11, 100), Some(ShadowLine::Open { need: GIB, plan: 0b11 }));
        assert_eq!(sh.observe(2 * GIB, 0b11, 200), None, "same plan: no line");
        assert_eq!(sh.observe(GIB, 0b111, 300), Some(ShadowLine::Plan { need: GIB, plan: 0b111 }));
        assert_eq!(sh.observe(0, 0, 900), Some(ShadowLine::Close { peak_need: 2 * GIB, ms: 800 }));
        assert!(!sh.open());
        assert_eq!(sh.observe(0, 0, 1000), None);
        assert_eq!(sh.observe(MIB, 1, 1100), Some(ShadowLine::Open { need: MIB, plan: 1 }));
        assert_eq!(sh.episodes, 2);
    }

    #[test]
    fn a_wait_budget_counts_from_the_last_free_up_to_its_cap() {
        use core::time::Duration as D;
        let b = D::from_secs(2);
        assert!(!wait_spent(D::from_millis(1500), D::from_secs(5), b), "a free 1.5 s ago: still within the budget");
        assert!(wait_spent(D::from_secs(2), D::from_secs(2), b), "no free for the whole budget: spent");
        assert!(wait_spent(D::from_millis(10), D::from_secs(8), b), "frees others keep taking: the cap ends it");
    }

    #[test]
    fn under_an_armed_cancel_the_line_sits_below_the_cancels_line() {
        let gib8 = 8 * GIB;
        // the 8 GiB probe: the watchdog's 90 % line is 7373 MB; the ladder's must be under it by the margin
        assert_eq!(line(gib8, 90), (gib8 / 100) * 90 - MARGIN_MIN);
        assert!(line(gib8, 90) < (gib8 / 100) * 90, "the gates climb before the backstop cancels");
        assert_eq!(line(gib8, 0), gib8 - MARGIN_MIN, "no cancel armed: the limit less the margin");
        assert_eq!(line(gib8, 100), gib8 / 100 * 100 - MARGIN_MIN, "a 100 % cancel: its own line less the margin");
        assert_eq!(line(0, 90), 0, "no limit: no line (the climb does nothing)");
    }

    #[test]
    fn r2a_stops_at_the_protected_share_and_r2b_has_the_rest_down_to_the_bytes_in_use() {
        assert_eq!(r2a_floor(64 * MIB, GIB), GIB);
        assert_eq!(r2a_floor(2 * GIB, GIB), 2 * GIB, "bytes in use past the share: nothing protected is left");
        assert_eq!(elastic_of(2000 * MIB, r2a_floor(64 * MIB, 1024 * MIB), None), 976 * MIB);
        assert_eq!(protected_left(2000 * MIB, 64 * MIB, 1024 * MIB, None), 960 * MIB);
        assert_eq!(protected_left(1024 * MIB, 64 * MIB, 1024 * MIB, Some(100 * MIB)), 100 * MIB, "at most what the reclaim frees");
        assert_eq!(protected_left(3 * GIB, 2 * GIB, GIB, None), 0);
    }

    #[test]
    fn r2a_counts_no_more_than_the_cache_can_free() {
        assert_eq!(elastic_of(3 * GIB, GIB, None), 2 * GIB, "no count: above the floor");
        assert_eq!(elastic_of(3 * GIB, 0, Some(15 * MIB)), 15 * MIB, "the 8 GiB rehearsal: 2-3 GB said, 0-15 MB freed");
        assert_eq!(elastic_of(GIB, GIB, Some(GIB)), 0, "at its floor: nothing");
    }

    #[test]
    fn relief_is_the_measured_drop_only() {
        assert_eq!(relief(10 * GIB, 9 * GIB), GIB);
        assert_eq!(relief(10 * GIB, 11 * GIB), 0, "a rise is no relief");
    }

    #[test]
    fn fair_share_floor_and_margin() {
        assert_eq!(fair(GIB, 8 * GIB, 4), 2 * GIB);
        assert_eq!(fair(3 * GIB, 8 * GIB, 4), 3 * GIB, "never under its own grant");
        assert_eq!(fair(0, 8 * GIB, 0), 8 * GIB);
        assert_eq!(protected_of(16 * GIB), 4 * GIB);
        let mut f = [0u64; 3];
        split_floors(4 * GIB, &[6 * GIB, 2 * GIB, 0], &mut f);
        assert_eq!(f, [3 * GIB, GIB, 0]);
        split_floors(4 * GIB, &[0, 0, 0], &mut f);
        assert_eq!(f, [0, 0, 0]);
        assert_eq!(margin_for(300 * MIB, 250), 105 * MIB, "D4324's 300 MB/s over a 250 ms landing and one tick");
        assert_eq!(margin_for(MIB, 250), MARGIN_MIN);
    }

    #[test]
    fn r1_cools_only_after_a_small_yield_within_its_window_while_memory_has_not_risen_past_it() {
        assert!(!r1_cooling(5_000, 0, 0, 0, 1 << 30), "never ran: runs");
        assert!(r1_cooling(5_000, 4_500, 2 * MIB, 1 << 30, 1 << 30), "ran 500 ms ago, 2 MiB back: cools");
        assert!(!r1_cooling(5_600, 4_500, 2 * MIB, 1 << 30, 1 << 30), "past the window: runs again");
        assert!(!r1_cooling(5_000, 4_500, R1_FLOOR, 1 << 30, 1 << 30), "a yield at the floor is worth repeating");
        assert!(!r1_cooling(5_000, 4_500, 2 * MIB, 1 << 30, (1 << 30) + 3 * MIB), "memory rose past the yield: runs");
    }

    /// Ladder C1 at the defaults (LOW 5 %, gap 5 % of the limit): the background trigger takes the headroom from under LOW
    /// to HIGH, a draw evicts what keeps LOW free after it, a cache refills only past HIGH and a second after a reclaim, and
    /// the pool's caches term is their protected floors (never more than their caps).
    #[test]
    fn c1_marks_triggers_refill_guard_and_the_pools_caches_term() {
        let limit = 8 * GIB;
        let (low, high) = marks(limit);
        assert_eq!((low, high), (limit / 20, limit / 20 * 2));
        let m = (low, high);
        assert_eq!(drain_to(low, m, 0), 0, "at LOW: nothing");
        assert_eq!(drain_to(low - 1, m, 0), high, "under LOW: to HIGH");
        assert_eq!(drain_to(low + 1, m, high), high, "draining: on to HIGH past LOW");
        assert_eq!(drain_to(high, m, high), 0, "at HIGH: done");
        assert_eq!(inline_want(GIB, 2 * GIB, low), 0, "LOW stays free after the draw");
        assert_eq!(inline_want(GIB, GIB, low), low, "the draw takes the room: LOW back");
        assert_eq!(inline_want(GIB, 0, low), GIB + low);
        assert!(refill_open(high + 1, high, u64::MAX), "past HIGH, no reclaim yet");
        assert!(!refill_open(high, high, u64::MAX), "at HIGH: closed");
        assert!(!refill_open(high + 1, high, 999), "within a second of a reclaim: closed");
        assert!(refill_open(high + 1, high, 1_000));
        let b = limit - limit / 8 - limit / 16;
        assert_eq!(pool_caches(3 * GIB + GIB / 4, b, false), 3 * GIB + GIB / 4, "off: every cache at its cap");
        assert_eq!(pool_caches(3 * GIB + GIB / 4, b, true), protected_of(b), "on: the protected floors");
        assert_eq!(pool_caches(GIB, b, true), GIB, "never more than the caps");
        assert!(!cache_floor(), "default off");
    }

    /// Ladder C1's measured LOW (the broker's 10-16 note): the burst over one batch's give-back and a tick, within
    /// [MARGIN_MIN, cache_low_pct of the limit]; the batch is the gap, at most 2 GiB (the gap passes it past ~40 GiB);
    /// the give-back rate moves a quarter of the way to each timed reclaim of 64 MiB or more.
    #[test]
    fn c1_low_is_the_burst_over_a_batchs_give_back_and_a_tick() {
        let limit = 8 * GIB;
        let gap = limit / 20;
        assert_eq!(batch(limit), gap, "8 GiB: the 5 % gap, one batch");
        assert_eq!(batch(40 * GIB), 2 * GIB, "40 GiB: the gap is the cap");
        assert_eq!(batch(100 * GIB), C1_BATCH_MAX, "100 GiB: a 5 GiB gap in batches of 2 GiB");
        // 409.6 MiB at 44 ms/GiB: 17.6 ms, then a tick
        assert_eq!(low_window_ms(limit, GIVE_BACK_NS_PER_GIB_PRIOR), 18 + TICK_MS);
        assert_eq!(low_window_ms(100 * GIB, GIVE_BACK_NS_PER_GIB_PRIOR), 88 + TICK_MS, "2 GiB at 44 ms/GiB");
        assert_eq!(marks_at(limit, None), marks(limit), "no burst measured: LOW at its most");
        assert_eq!(marks_at(limit, Some(300 * MIB)), (300 * MIB, 300 * MIB + gap), "LOW = the burst");
        assert_eq!(marks_at(limit, Some(0)), (MARGIN_MIN, MARGIN_MIN + gap), "an idle box: at least MARGIN_MIN");
        assert_eq!(marks_at(limit, Some(4 * GIB)), marks(limit), "at most cache_low_pct of the limit");
        assert_eq!(give_back_rate(0, MIB, 1_000_000_000), GIVE_BACK_NS_PER_GIB_PRIOR, "too small to time: the prior");
        assert_eq!(give_back_rate(0, GIB, 44_000_000), GIVE_BACK_NS_PER_GIB_PRIOR, "at the prior: unchanged");
        assert_eq!(give_back_rate(40_000_000, GIB / 2, 40_000_000), 30_000_000 + 20_000_000, "a quarter of the way to 80 ms/GiB");
        // C1's R2a order: ORDER's caches, the part cache last
        let mut a = C1_ORDER.map(|c| c as usize);
        let mut o = crate::reclaim::ORDER.map(|c| c as usize);
        a.sort_unstable();
        o.sort_unstable();
        assert_eq!(a, o, "the same caches");
        assert_eq!(C1_ORDER[4], memledger::Consumer::PartCache);
    }

    /// Ladder C1c: the three marks (TOP = HIGH + the gap); raises only past TOP, back to HIGH; demand = at budget
    /// and a recent eviction; the backoff doubles per reversal; the self-trim's step.
    #[test]
    fn c1b_a_take_sets_the_budget_and_raises_only_past_top_with_demand() {
        let limit = 8 * GIB;
        let (low, high, top) = marks3(limit, None);
        assert_eq!((low, high), marks(limit));
        assert_eq!(top, high + gap(limit), "TOP = HIGH + 5 % of the limit");
        assert_eq!(raise_room(top, top, high, top), 0, "between HIGH and TOP: nothing");
        assert_eq!(raise_room(high + 1, high + 1, high, top), 0);
        assert_eq!(raise_room(top + 1, top + 1, high, top), top + 1 - high, "past TOP: back to HIGH");
        // C1d v2: the window's lowest headroom bounds it: a draw that took the room to LOW within the window keeps it
        assert_eq!(raise_room(top + GIB, top, high, top), top - high, "the window's low, back to HIGH");
        assert_eq!(raise_room(top + GIB, high - MIB, high, top), 0, "a draw under HIGH within the window: none");
        assert_eq!(raise_room(top + 1, u64::MAX, high, top), top + 1 - high, "no low noted: this sample's");
        assert_eq!(raise_room_within(top + GIB, top + GIB, high, top, GIB / 2), GIB / 2, "the claims' least room bounds it");
        assert_eq!(raise_room_within(top + GIB, top + GIB, high, top, 0), 0, "a take the pool was short for: none");
        assert_eq!(raise_room_within(top + GIB, top + GIB, high, top, u64::MAX), top + GIB - high, "no claim noted");
        assert_eq!(raise_room_within(top, top, high, top, u64::MAX), 0, "under TOP: none, whatever the claims left");
        assert!(has_demand(GIB - MIB, GIB, 5_000, 60_000), "at budget, evicted 55 s ago");
        assert!(!has_demand(GIB - 64 * MIB, GIB, 5_000, 6_000), "more than a step under its budget: no demand");
        assert!(!has_demand(GIB, GIB, 0, 6_000), "never evicted: no demand");
        assert!(!has_demand(GIB, GIB, 5_000, 70_000), "its last eviction over a minute ago: no demand");
        assert_eq!((raise_interval_ms(0), raise_interval_ms(1), raise_interval_ms(3), raise_interval_ms(20)), (100, 200, 800, 100 << 9));
        assert_eq!((refill_quiet_ms(1_000, 0), refill_quiet_ms(1_000, 1), refill_quiet_ms(1_000, 3)), (1_000, 2_000, 8_000), "doubled per reversal");
        assert_eq!((refill_quiet_ms(1_000, 10), refill_quiet_ms(1_000, 40)), (C1_QUIET_MAX_MS, C1_QUIET_MAX_MS), "at most 10 min");
        assert!(refaults_allow_raise(16 * GIB / 50, 16 * GIB), "2 % of the limit a minute: open");
        assert!(!refaults_allow_raise(16 * GIB / 50 + 1, 16 * GIB), "past it: the raises wait");
        assert!(refaults_allow_raise(0, 0), "no limit, no refaults: open");
        // C1d: a raise gives back the bytes the cache evicted under its budget since its last raise, a step at least,
        // within the room that keeps HIGH; under a step of room, none
        assert_eq!(raise_bytes(0, 1 << 40), C1_RAISE_STEP);
        assert_eq!(raise_bytes(3 << 30, 1 << 40), 3 << 30);
        assert_eq!(raise_bytes(3 << 30, 1 << 30), 1 << 30, "within the room");
        assert_eq!(raise_bytes(3 << 30, C1_RAISE_STEP - 1), 0, "under a step of room: none");
        assert_eq!(held_past(5 * GIB, 4 * GIB, GIB), GIB, "the floor back under the budget: the bytes past it");
        assert_eq!(held_past(5 * GIB, 4 * GIB, 4 * GIB), 0, "while the floor holds it: none");
        assert_eq!(held_past(3 * GIB, 4 * GIB, 0), 0, "under the budget: none");
        assert_eq!(held_past(u64::MAX, 4 * GIB, 0), 0, "no cheap count: none");
        assert_eq!(C1_TRIM_STEP, 32 * MIB);
        assert_eq!(crate::cache::part_cache_trim_target_by(GIB, C1_TRIM_STEP), GIB - 32 * MIB, "one step under");
        assert_eq!(crate::cache::part_cache_trim_target_by(64 * MIB, C1_TRIM_STEP), 48 * MIB, "at most a quarter");
        assert_eq!(crate::cache::part_cache_trim_target_by(GIB, 0), GIB - GIB / 4, "off: a quarter under");
    }

    #[test]
    fn shared_buffers_is_asked_only_past_the_caches_elastic_bytes_and_lands_within_the_bound() {
        assert_eq!(buffers_ask(GIB, GIB, GIB), 0, "the caches cover it: no ask");
        assert_eq!(buffers_ask(GIB, 2 * GIB, GIB), 0);
        assert_eq!(buffers_ask(GIB, GIB - MIB, GIB), BUFFERS_EXTENT, "a MiB short: one whole extent");
        assert_eq!(buffers_ask(GIB, 0, GIB), GIB);
        assert_eq!(buffers_ask(2 * GIB, 0, GIB), GIB, "at most what the pool can still give");
        assert_eq!(buffers_ask(GIB, 0, 0), 0, "at the configured size (or the knob off): none");
        assert_eq!(buffers_due_ms(0, 100 * MIB), TICK_MS);
        assert_eq!(buffers_due_ms(GIB, 100 * MIB), 1_000 + 10_240 + TICK_MS, "1 GiB: the clean second, 10.24 s at 100 MiB/s");
        assert_eq!(buffers_due_ms(GIB, 0), 1_000 + TICK_MS, "unpaced: the clean term");
        assert_eq!(buffers_due_ms(u64::MAX, 1), u64::MAX, "saturates");
    }

    #[test]
    fn a_take_line_is_due_only_with_the_knob_past_0_and_its_window_over() {
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        crate::consts::reset();
        assert!(take_log_on(), "the default 1000: on");
        assert!(take_log_due(5_000, 0), "no line yet: due");
        assert!(!take_log_due(5_999, 5_000), "inside the window: folded into the next line");
        assert!(take_log_due(6_000, 5_000), "the window over");
        assert!(!take_log_due(4_000, 5_000), "a clock behind the last line: not due");
        crate::consts::set(crate::consts::K::LadderLogMs, 0);
        assert!(!take_log_due(5_000, 0) && !take_log_on(), "0: never");
        crate::consts::reset();
    }
}

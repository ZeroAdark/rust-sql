/// Who sizes the part cache. Under ladder C1 (`cache_floor` with the ladder on) the ladder does, the memory broker on or
/// not: the ladder is the one evicter (the broker keeps its advice), so its raises and takes are the budget. With the
/// broker's target sizing it beside C1, the ladder's take set a budget nothing read and its raises never came (the
/// coremem stress at a 16 GB limit: the part cache cut to 2280 MB, then 0 raises in 23 min). Else the broker when its
/// part-cache actuator acts, else the cache's own budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sizer {
    Own,
    Broker,
    Ladder,
}

pub fn part_cache_sizer(broker: bool, ladder: bool) -> Sizer {
    if ladder {
        Sizer::Ladder
    } else if broker {
        Sizer::Broker
    } else {
        Sizer::Own
    }
}

/// A server setting is the cache's minimum: the broker's target never takes it under.
pub fn part_cache_budget(server: Option<u64>, target: Option<u64>, derived: impl FnOnce() -> u64) -> u64 {
    match target {
        Some(t) => t.max(server.unwrap_or(0)),
        None => server.unwrap_or_else(derived),
    }
}

/// The last quarter of the bar is for the statements: at half, a wide aggregate's hot repeat released its warm parts.
pub fn short_pressure(bar: u64, projected: u64, bytes: u64, budget: u64) -> bool {
    if bar != 0 {
        projected > bar - bar / 4
    } else {
        bytes / 4 > budget
    }
}

/// D3994 (B.1 W): when a content trim evicts the parts only parked executors use (their kept cursors hold none of their
/// images and read nothing until a statement binds them again). ONE switch, [`PARKED_GATE`]; its arms are
/// [`parked_evictable`]'s.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParkedGate {
    /// Under memory pressure only: the statements' last quarter of the bar (the governor on), else B's free bytes under its
    /// last quarter; with the broker sizing the part cache, the cache past the broker's target too. A box with room keeps
    /// the parts a repeated scan reads resident: releasing them at the cap made every repeat fault its parts in again.
    Pressure,
    /// At the cap: whenever the part cache is past its budget (D3268's first cut's rule).
    Cap,
}

/// D3994 (B.1 W): the parked parts' gate the server runs.
pub const PARKED_GATE: ParkedGate = ParkedGate::Pressure;

/// D3994 (B.1 W): what [`parked_evictable`] reads (atomics only: a trim runs on the reclaimer's and the memory watchdog's
/// threads too).
#[derive(Clone, Copy, Debug)]
pub struct ParkedSignals {
    /// The governor's bar (0: off) and its projected bytes.
    pub bar: u64,
    pub projected: u64,
    /// B's free bytes (u64::MAX before the first snapshot) and B.
    pub free_b: u64,
    pub b: u64,
    /// The memory broker sizes the part cache (on, with its `part_cache` actuator): `budget` is the broker's target.
    pub broker: bool,
    /// The part cache's bytes and budget (u64::MAX: unbounded).
    pub bytes: u64,
    pub budget: u64,
}

/// D3994: memory is short: the statements' last quarter of the bar (the governor on), else B's free bytes under its last
/// quarter. No snapshot yet (`free_b` u64::MAX) or no budget: not short. The one pressure signal the parked parts' gate
/// ([`parked_evictable`]) and a running statement's part release (`release::part_release_due`) read.
pub fn memory_short(s: &ParkedSignals) -> bool {
    if s.bar != 0 {
        bar_short(s.bar, s.projected)
    } else {
        b_short(s.b, s.free_b)
    }
}

/// [`memory_short`]'s arm with the governor on: the statements' last quarter of the bar.
#[inline]
fn bar_short(bar: u64, projected: u64) -> bool {
    projected > bar - bar / 4
}

/// [`memory_short`]'s arm with the governor off: B's free bytes under its last quarter (none before the first snapshot).
#[inline]
fn b_short(b: u64, free_b: u64) -> bool {
    b != 0 && free_b != u64::MAX && free_b < b / 4
}

/// D3994 (level gate): the release side of the same switch: a running statement's part every claim left goes back past the
/// cache's level (`release::part_release_due`), and besides under `gate`: Pressure, when memory is short ([`memory_short`]);
/// Cap, nothing more (the cap is the level itself).
pub fn release_short(gate: ParkedGate, s: &ParkedSignals) -> bool {
    match gate {
        ParkedGate::Pressure => memory_short(s),
        ParkedGate::Cap => false,
    }
}

/// [`release_short`] read per part credit (a scan's part enter and leave, about a thousand a statement over a 511-part
/// table): the same answer from the governor's `bar` and only the signals its arm reads, cheapest first (`projected` with
/// the governor on; else B's free bytes, then B only past a first snapshot). Reading every [`ParkedSignals`] field each
/// time cost a short scan ~157 k instructions (+6%) on a box with room.
#[inline]
pub fn release_short_by(gate: ParkedGate, bar: u64, projected: impl FnOnce() -> u64, free_b: impl FnOnce() -> u64, b: impl FnOnce() -> u64) -> bool {
    if gate == ParkedGate::Cap {
        return false;
    }
    if bar != 0 {
        return bar_short(bar, projected());
    }
    let free = free_b();
    free != u64::MAX && b_short(b(), free)
}

/// D3994 (B.1 W): whether a content trim may evict the parts only parked executors use now, under `gate`.
pub fn parked_evictable(gate: ParkedGate, s: &ParkedSignals) -> bool {
    let past_budget = s.bytes > s.budget;
    match gate {
        ParkedGate::Cap => past_budget,
        ParkedGate::Pressure => memory_short(s) || (s.broker && past_budget),
    }
}

/// A quarter under `budget`, so a cache at its budget does not trim at every statement.
#[inline]
pub fn part_cache_trim_target(budget: u64) -> u64 {
    budget - budget / 4
}

/// Ladder C1b: the trim target with `step` (`ladder::C1_TRIM_STEP` while the ladder sizes the part cache, else 0): one
/// step under the budget, at most a quarter; 0 = a quarter under (`part_cache_trim_target`).
#[inline]
pub fn part_cache_trim_target_by(budget: u64, step: u64) -> u64 {
    if step == 0 { part_cache_trim_target(budget) } else { budget - step.min(budget / 4) }
}

/// Ladder C1b, while the ladder sizes the part cache with `step` (> 0): while the bytes its own trim cannot drop
/// (`floor`: in use, and the images part lists hold, as the ladder's last sample read them) are at or past the
/// `budget`, its own trims (a statement's start, a scan's pass, a load past the budget) are bounded by a cushion, the
/// floor plus one step, not the budget: past it they evict the unused parts, oldest first, down to it. A target under
/// the floor would take every unused part at each insert, the parts the next reads want, and past them the later tiers
/// would take the extents other readers have yet to reach and the parked sets' cursors. `None`: no hold (no step, or
/// the floor under the budget). The ladder's takes are not own trims: they run as before.
///
/// `reach`: the budget plus the bytes a raise could give it at the ladder's last sample (`ladder::raise_room_within`
/// past TOP over the window's LOW, after that sample's raises; the budget alone at or under TOP, and while the cache's
/// raise timing holds raises back: a take's refill quiet period, the raise backoff; 0 = none). The cushion
/// is at least it, so the bound runs smoothly from the floor plus one step under pressure to the room a raise has past
/// TOP, where the held bytes are a raise's to come (the C1b held-past demand), not a trim's. Floor plus one step at any
/// room re-read a large scan's passed parts every run while the budget lagged its working set (a 9.1-9.5 GiB budget
/// against 10.4-10.7 GiB held); an unbounded hold kept them but held 3.8 GiB past the budget under memory pressure.
#[inline]
pub fn part_cache_own_cushion(budget: u64, floor: u64, step: u64, reach: u64) -> Option<u64> {
    (step != 0 && floor >= budget).then(|| floor.saturating_add(step).max(reach))
}

/// Ladder C1b, while the ladder sizes the part cache with `step` (> 0): an own trim of a cache holding `have` sheds at
/// most one step: down to the step-under target (`part_cache_trim_target_by`) or `have - step`, whichever is higher. A
/// cache the floor held past its budget then comes back a step per trim, so the parts of the statement that just ran
/// stay for its next run, and each step is an eviction the raises read as demand.
#[inline]
pub fn part_cache_shed_target(budget: u64, have: u64, step: u64) -> u64 {
    part_cache_trim_target_by(budget, step).max(have.saturating_sub(step))
}

/// Ladder C1b, the level the part cache's own trims bring it down to (pgrc_read's `own_trim_target`), holding `have`
/// under `budget`: with no `step` the trim target; with a step the hold's `cushion` where one bounds it, else the shed
/// (`part_cache_shed_target`), or with the shed off (`shed` = cache_hold_shed 0, H5a) the step-under target at once.
/// `shed` on (the default) is the landed shape.
#[inline]
pub fn part_cache_own_target(budget: u64, have: u64, step: u64, cushion: Option<u64>, shed: bool) -> u64 {
    match step {
        0 => part_cache_trim_target_by(budget, 0),
        step => cushion.unwrap_or_else(|| if shed { part_cache_shed_target(budget, have, step) } else { part_cache_trim_target_by(budget, step) }),
    }
}

/// Ladder C1b, the level an own trim of a cache holding `have`, asked for `level`, stops at (pgrc_read's `own_level`, a
/// load's room): `level`; with a `step` and the shed on (`shed`, H5a), at most one step under `have`.
#[inline]
pub fn part_cache_own_level(have: u64, level: u64, step: u64, shed: bool) -> u64 {
    if step == 0 || !shed { level } else { level.max(have.saturating_sub(step)) }
}

/// The part cache is past its `budget` and past the level a failed trim set (`retry_at`; pgrc_read's `over_budget`).
#[inline]
pub fn part_cache_over(bytes: u64, budget: u64, retry_at: u64) -> bool {
    bytes > budget && bytes >= retry_at
}

/// After a trim left `have` bytes past `target`, the next waits for budget / 8 more (0 = no wait).
#[inline]
pub fn part_cache_retry_at(have: u64, target: u64, budget: u64) -> u64 {
    if have > target {
        have.saturating_add((budget / 8).max(1))
    } else {
        0
    }
}

/// LOAD GATE (LADDER-DESIGN section 3): a load of `n` bytes fits the part cache at `budget` holding `have`: it asks nothing.
#[inline]
pub fn part_cache_load_fits(have: u64, n: u64, budget: u64) -> bool {
    have.saturating_add(n) <= budget
}

/// LOAD GATE (LADDER-DESIGN section 3): the level a load of `n` bytes into a part cache at `budget` makes room down to.
#[inline]
pub fn part_cache_load_target(budget: u64, n: u64) -> u64 {
    budget.saturating_sub(n)
}

/// LOAD GATE: whether a load past the part cache's budget, with only pinned bytes left to keep it from fitting (`short`
/// bytes over the target), waits for them. Only a demand load (`demand`; a read ahead never waits) of a statement that
/// has not spent its wait this statement epoch (`spent`), and only for bytes OTHER running statements pin (`others`):
/// its own statement's pins cannot go while it waits.
#[inline]
pub fn part_cache_load_waits(demand: bool, spent: bool, others: u64, short: u64) -> bool {
    demand && !spent && short != 0 && others >= short
}

pub const REUSE_WINDOW_S: u32 = 900;

/// What the short hook releases: nothing, the parked slot sets but the newest (D3110 (a)), every kept cursor (D3268).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShortRelease {
    None,
    Older,
    All,
}

/// The short hook, each release once per statement epoch: under `pressure` all, else the older sets when two are parked.
#[inline]
pub fn short_release(pressure: bool, sets: usize, older_done: impl FnOnce() -> bool, all_done: impl FnOnce() -> bool) -> ShortRelease {
    if !pressure {
        if sets < 2 || older_done() {
            return ShortRelease::None;
        }
        return ShortRelease::Older;
    }
    if all_done() {
        ShortRelease::None
    } else {
        ShortRelease::All
    }
}

/// cachepolicy (C5): the condition cache replaces by W-TinyLFU (`cache_policy` = 2), not lru (the fp tables stay lru).
pub fn tinylfu_on() -> bool {
    crate::consts::get(crate::consts::K::CachePolicy) == 2
}

/// cachepolicy (C5, `cache_policy` = 2, W-TinyLFU for the condition cache): the window's bytes, 1 % of
/// the cap (at least 1).
pub fn tinylfu_window(cap: u64) -> u64 {
    (cap / 100).max(1)
}

/// cachepolicy (C5): the protected segment's bytes, 80 % of main (the cap less the window).
pub fn tinylfu_protected(cap: u64) -> u64 {
    cap.saturating_sub(tinylfu_window(cap)) / 5 * 4
}

/// cachepolicy (C5): the window's least recent entry enters main only when the sketch counts it more often than the main
/// entries it would push out, summed (size-aware TinyLFU admission).
pub fn tinylfu_admit(candidate: u32, victims: u32) -> bool {
    candidate > victims
}

/// cachepolicy (C5): the frequency sketch's counters per row (4 rows of 4-bit counters: 32 KiB a cache).
pub const TINYLFU_SKETCH_WIDTH: usize = 1 << 14;

/// cachepolicy (C5): the sketch halves every counter after this many adds (TinyLFU's reset: 10 x its width).
pub const TINYLFU_RESET_ADDS: u64 = 10 * TINYLFU_SKETCH_WIDTH as u64;

/// Capacity guidance only; pins are law.
pub const REGISTRY_BUDGET_BYTES: u64 = 256 << 20;

/// At least the default, so its janitor never drops entries the part cache keeps, with their re-use history.
pub fn registry_budget(part_cache: Option<u64>) -> u64 {
    part_cache.map_or(REGISTRY_BUDGET_BYTES, |b| b.max(REGISTRY_BUDGET_BYTES))
}

/// pgrust.point_noplan's memo (gravity exec runner/point/skip.rs): the point plans one session keeps, by statement
/// shape, as at most this many entries. A point shape is one table, one unique key and one statement form: an
/// application has tens of them, so the cap is met only by generated shapes, which then cycle through it least recently
/// used first.
pub const POINT_MEMO_ENTRIES: usize = 1024;

/// The same memo's bytes (its entries' contexts and their recorded reads), a few KiB per entry: the entry cap at about
/// 4 KiB each, so wide target lists meet this cap first.
pub const POINT_MEMO_BYTES: usize = 4 << 20;

/// The point memo holds more than its caps: its least recently used entry goes.
pub fn point_memo_over(entries: usize, bytes: usize) -> bool {
    entries > POINT_MEMO_ENTRIES || bytes > POINT_MEMO_BYTES
}

#[cfg(test)]
mod tests {
    use super::{parked_evictable, part_cache_sizer, part_cache_trim_target, registry_budget, ParkedGate, ParkedSignals, Sizer, PARKED_GATE, REGISTRY_BUDGET_BYTES};

    /// Under ladder C1 the ladder sizes the part cache whether the broker's part-cache actuator acts or not.
    #[test]
    fn under_c1_the_ladder_sizes_the_part_cache_broker_or_not() {
        assert_eq!(part_cache_sizer(true, true), Sizer::Ladder, "C1 under the broker: the ladder, one evicter");
        assert_eq!(part_cache_sizer(false, true), Sizer::Ladder);
        assert_eq!(part_cache_sizer(true, false), Sizer::Broker, "C1 off: the broker's target, as before");
        assert_eq!(part_cache_sizer(false, false), Sizer::Own);
    }

    const G: u64 = 1 << 30;

    /// LOAD GATE: a load makes room down to the budget less itself; it waits only as a demand load, once per statement
    /// epoch, and only when other running statements pin at least the bytes it is short of.
    #[test]
    fn a_load_waits_only_for_other_statements_pins_once_per_epoch() {
        use super::{part_cache_load_target, part_cache_load_waits};
        assert_eq!(part_cache_load_target(10 * G, G), 9 * G);
        assert_eq!(part_cache_load_target(G / 2, G), 0, "a load larger than the budget empties what it can");
        assert!(part_cache_load_waits(true, false, 2 * G, G), "others pin enough: wait for them");
        assert!(!part_cache_load_waits(true, false, G / 2, G), "others pin too little: its own pins fill the cache, no wait");
        assert!(!part_cache_load_waits(false, false, 2 * G, G), "a read ahead never waits");
        assert!(!part_cache_load_waits(true, true, 2 * G, G), "a wait spent this epoch: no second wait");
        assert!(!part_cache_load_waits(true, false, 2 * G, 0), "the trim made room: nothing to wait for");
    }

    /// Ladder C1b: while the bytes the own trim cannot drop reach the budget, it is bounded by the floor plus a step,
    /// or past TOP by the budget plus the room a raise could give, whichever is more; once they fall back it sheds one
    /// step a trim, never under the step-under target.
    #[test]
    fn the_own_trim_keeps_a_step_past_the_floor_and_sheds_one_step_a_trim() {
        use super::{part_cache_own_cushion, part_cache_shed_target};
        const M: u64 = 1 << 20;
        assert_eq!(part_cache_own_cushion(4 * G, 4 * G, 256 * M, 0), Some(4 * G + 256 * M), "the floor at the budget: a step past it");
        assert_eq!(part_cache_own_cushion(4 * G, 5 * G, 256 * M, 4 * G), Some(5 * G + 256 * M), "at or under TOP (the reach the budget): a step past the floor");
        assert_eq!(part_cache_own_cushion(4 * G, 5 * G, 256 * M, 9 * G), Some(9 * G), "past TOP: the room a raise could give");
        assert_eq!(part_cache_own_cushion(4 * G, 5 * G, 256 * M, 5 * G), Some(5 * G + 256 * M), "room falling toward TOP: never under a step past the floor");
        assert_eq!(part_cache_own_cushion(4 * G, 3 * G, 256 * M, 9 * G), None, "under it: the budget bounds the trim");
        assert_eq!(part_cache_own_cushion(4 * G, 5 * G, 0, 9 * G), None, "no step (a fixed budget): no hold");
        assert_eq!(part_cache_shed_target(4 * G, 6 * G, 256 * M), 6 * G - 256 * M, "2 GiB past the budget: one step");
        assert_eq!(part_cache_shed_target(4 * G, 4 * G + 64 * M, 256 * M), 4 * G - 192 * M, "a load just past it: one step");
        assert_eq!(part_cache_shed_target(4 * G, 4 * G - 64 * M, 256 * M), 4 * G - 256 * M, "never under the step-under target");
        assert_eq!(part_cache_shed_target(G / 2, G, 256 * M), G - 256 * M, "one step a trim at a small budget too");
        assert_eq!(part_cache_shed_target(G / 2, G / 2 + M, 256 * M), G / 2 - G / 8, "at most a quarter under the budget");
    }

    /// H5a: cache_hold_shed on (the default) is the landed shape: the cushion where the hold bounds the own trim, else
    /// the one-step shed; off, the step-under target at once. With no step neither applies.
    #[test]
    fn the_shed_knob_on_is_the_landed_shape_and_off_trims_to_the_step_under_target() {
        use super::{part_cache_own_level, part_cache_own_target, part_cache_shed_target, part_cache_trim_target_by};
        const M: u64 = 1 << 20;
        let (b, have, step) = (4 * G, 6 * G, 256 * M);
        assert_eq!(part_cache_own_target(b, have, step, None, true), part_cache_shed_target(b, have, step), "on, no hold: the shed (landed)");
        assert_eq!(part_cache_own_target(b, have, step, Some(5 * G), true), 5 * G, "on, the hold: its cushion (landed)");
        assert_eq!(part_cache_own_target(b, have, step, None, false), b - step, "off: the step-under target at once");
        assert_eq!(part_cache_own_target(b, have, step, Some(5 * G), false), 5 * G, "off: the hold's cushion still bounds it");
        assert_eq!(part_cache_own_target(b, have, 0, None, true), part_cache_trim_target_by(b, 0), "no step: the trim target");
        assert_eq!(part_cache_own_level(have, b - step, step, true), have - step, "on: one step under what it holds (landed)");
        assert_eq!(part_cache_own_level(have, b - step, step, false), b - step, "off: the level asked");
        assert_eq!(part_cache_own_level(have, b - step, 0, true), b - step, "no step: the level asked");
    }

    /// B 15 GiB with `free_b` free, the governor off, the broker off, a 3.6 GiB part cache at 18.6 GiB (D3268's).
    fn sig(free_b: u64) -> ParkedSignals {
        ParkedSignals { bar: 0, projected: 0, free_b, b: 15 * G, broker: false, bytes: 18 * G + G / 2, budget: 3 * G + G / 2 }
    }

    /// D3994 (B.1 W): the parts only parked executors use go under memory pressure only: B's free bytes under its last
    /// quarter (D3994's broker-on stress at the terminate: B 15,032 MiB, free 0); a box with room keeps them though the
    /// cache is past its budget (D3268).
    #[test]
    fn d3994_parked_parts_go_only_under_pressure() {
        let b = 15 * G;
        assert_eq!(PARKED_GATE, ParkedGate::Pressure, "the default arm");
        let p = |s: ParkedSignals| parked_evictable(ParkedGate::Pressure, &s);
        assert!(p(sig(0)), "B full");
        assert!(p(sig(b / 4 - 1)), "past three quarters of B");
        assert!(!p(sig(b / 4)), "at the line: not yet");
        assert!(!p(sig(12 * G)), "a box with room keeps them past the cache's budget");
        assert!(!p(sig(u64::MAX)), "no budget snapshot yet");
        assert!(!p(ParkedSignals { b: 0, ..sig(0) }), "no budget");
        // the governor on: its bar's last quarter, as the short hook's
        assert!(p(ParkedSignals { bar: 8 * G, projected: 7 * G, ..sig(12 * G) }), "past the bar's three quarters");
        assert!(!p(ParkedSignals { bar: 8 * G, projected: 6 * G, ..sig(0) }), "the bar has room");
        // the broker on: the cache past its target
        assert!(p(ParkedSignals { broker: true, ..sig(12 * G) }), "past the broker's target");
        assert!(!p(ParkedSignals { broker: true, bytes: 3 * G, ..sig(12 * G) }), "under the broker's target");
        assert!(!p(ParkedSignals { broker: true, budget: u64::MAX, ..sig(12 * G) }), "an unbounded cache");
    }

    /// D3994 (level gate): the release side of the switch: Pressure adds memory short to the level, Cap nothing.
    #[test]
    fn d3994_release_short_follows_the_switch() {
        use super::release_short;
        assert!(release_short(ParkedGate::Pressure, &sig(0)), "Pressure: B full");
        assert!(!release_short(ParkedGate::Pressure, &sig(12 * G)), "Pressure: room");
        assert!(!release_short(ParkedGate::Cap, &sig(0)), "Cap: the level alone");
    }

    /// D3994 (level gate): the per-credit read gives `release_short`'s answer on every arm, and reads nothing it does not need.
    #[test]
    fn d3994_release_short_by_matches_release_short() {
        use super::{release_short, release_short_by};
        let never = || -> u64 { panic!("a signal this arm does not read") };
        for gate in [ParkedGate::Pressure, ParkedGate::Cap] {
            for s in [sig(0), sig(12 * G), sig(u64::MAX), ParkedSignals { bar: 8 * G, projected: 7 * G, ..sig(12 * G) }, ParkedSignals { bar: 8 * G, projected: G, ..sig(0) }, ParkedSignals { b: 0, ..sig(0) }] {
                let by = release_short_by(gate, s.bar, || s.projected, || s.free_b, || s.b);
                assert_eq!(by, release_short(gate, &s), "{gate:?} {s:?}");
            }
        }
        assert!(!release_short_by(ParkedGate::Cap, 8 * G, never, never, never), "Cap reads no signal");
        assert!(!release_short_by(ParkedGate::Pressure, 0, never, || u64::MAX, never), "no snapshot: B is not read");
        assert!(release_short_by(ParkedGate::Pressure, 8 * G, || 7 * G, never, never), "governor on: B's free bytes are not read");
    }

    /// D3994 (B.1 W): the cap arm (behind the same switch) evicts them whenever the cache is past its budget.
    #[test]
    fn d3994_parked_parts_cap_arm() {
        let c = |s: ParkedSignals| parked_evictable(ParkedGate::Cap, &s);
        assert!(c(sig(12 * G)), "past the budget, room or not");
        assert!(!c(ParkedSignals { bytes: 3 * G, ..sig(0) }), "under the budget, pressure or not");
        assert!(!c(ParkedSignals { budget: u64::MAX, ..sig(0) }), "an unbounded cache");
    }

    #[test]
    fn the_registry_budget_is_at_least_its_default() {
        assert_eq!(registry_budget(None), REGISTRY_BUDGET_BYTES);
        assert_eq!(registry_budget(Some(1 << 20)), REGISTRY_BUDGET_BYTES);
        assert_eq!(registry_budget(Some(1 << 30)), 1 << 30);
        assert_eq!((part_cache_trim_target(1 << 30), part_cache_trim_target(0)), (3 << 28, 0), "a quarter under the budget");
    }

    /// cachepolicy (C5): W-TinyLFU's segments and admission.
    #[test]
    fn cachepolicy_tinylfu_segments_and_admission() {
        use super::{tinylfu_admit, tinylfu_protected, tinylfu_window};
        assert_eq!((tinylfu_window(10_000), tinylfu_window(50), tinylfu_window(0)), (100, 1, 1), "1 % of the cap, at least 1");
        assert_eq!(tinylfu_protected(10_000), 7_920, "80 % of main (the cap less the window)");
        assert_eq!(tinylfu_protected(0), 0);
        assert!(tinylfu_admit(3, 2) && !tinylfu_admit(2, 2) && !tinylfu_admit(0, 0), "strictly more often");
    }
}

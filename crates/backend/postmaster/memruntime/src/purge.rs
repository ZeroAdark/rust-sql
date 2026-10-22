//! C3 (`cache_purge_now`, memacct2 2026-10-16; cache plan section C3, Broker's conditions): the cache purger, one
//! thread of its own (it never touches the 100 ms memory sample loop). Before it, a cut of a cache's cap
//! (a reload, the broker's target) did nothing until the next Gravity statement: that statement ran the part cache's trim
//! on its own thread (~50 ms for a 4 GiB cut) and its idle release at the end decommitted the freed cache arena on the
//! same thread (~45-55 ms per GiB, mimalloc's forced purge of every arena), so the session's next statement waited
//! behind it. The purger applies the cut itself (`mcx::cache_cut`: the hosts re-read each cache's cap and trim it when
//! it is past it) on its tick and at once on `wake`, then purges the cache arena alone (`mcx::cache_purge`, the vendored
//! `mi_arena_purge_now`: the general arenas keep mimalloc's own purge) in a burst every `BURST_MS` until the freed pages
//! stop coming (the owners' collects answer the ask it sends first). Between cuts each tick purges only the cache
//! arena's ranges whose purge delay ran out, on this thread rather than on a query thread. Once it runs, the cache arena
//! is the purger's alone (`mcx::cache_purge_owner`, the vendored per-arena flag): every other collect skips it (the
//! session's forced collect at a statement's end, the postmaster's at a connection's exit), unless this thread has not
//! called the purge for 1 s while expired ranges wait (mimalloc's dead-man fallback, `mcx::cache_purge_fallbacks`).
//! C4 (`query_purge_bg`, D3257): under `PGRUST_QUERY_PURGE_BG=1` the purger owns the general arenas'
//! purge too and purges their EXPIRED ranges every tick (each owned arena visited every `TICK_MS`; the 1 s purge delay
//! kept, so hot pages are not re-faulted), and at once on a wake (the ladder's R1 reaches
//! `mcx::give_back_caches`, which wakes it): no query thread, thread exit or postmaster connection exit purges an arena
//! any more, so the decommit syscalls leave bind (D3257's spikes) and ServerLoop. No forced purge of a general arena's
//! UNEXPIRED ranges at all: the ladder's pressure path purges the expired ones at once, the last second's frees wait out
//! the delay as today, and forcing would re-fault hot pages.
//! The hold (`PGRUST_PURGE_AFTER_IDLE_S`, whole seconds, default 1 = today; `never` = no timer at all): above 1, the freed
//! pages of an arena the purger owns stay committed for the next allocation (the reuse claim takes such runs before fresh
//! slices) instead of going back to the OS when mimalloc's 1 s purge delay runs out: a tick purges an owned arena's
//! expired ranges only once no slice was claimed from that arena for that many seconds (`mcx::idle_purge`; it keeps the
//! owner alive, so the dead-man fallback stays off; under `never` it only keeps the owner alive), and a cap cut or a
//! give-back's wake no longer bursts by itself. Under ladder pressure (`pressure`: the whole memory reading over the
//! ladder's line, the freed pages the hold keeps included) every tick and wake purges as today.
//! Counted apart (`hold_counts`, the accounts rows purge_ladder_* / purge_idle_*): the ticks under ladder pressure and
//! the bytes the purger purged on them, the idle purges and their bytes.
//! The pressure mode (`mempolicy::ladder::purge_mode_tick`; `PGRUST_PURGE_PRESSURE_MODE=1` turns it on): a tick is a
//! pressure tick from the first that has a cause (its own read past the line, a ladder pass that left a need, an R6
//! victim's let-go: `ask`) until the read is back at the line less the ladder's HIGH or two ticks in a row had none. Without
//! it a read that dipped just under the line ended the pressure purges while the freed pages waited out the purge delay.
//! Counted (`mode_counts`): the time in the mode and its entries per cause.
//! Switches: `PGRUST_CACHE_PURGE_NOW` and `PGRUST_QUERY_PURGE_BG` (1 = on; anything else, or unset, = off); with both off
//! the purger does not start (and the hold, which only applies to the arenas it owns, does nothing).

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed, Ordering::SeqCst};
use mempolicy::ladder::PurgeMode;
use core::time::Duration;

/// The tick: the cut re-read (a few relaxed loads and each cache's cap) and the expired purge.
pub const TICK_MS: u64 = 50;
/// A burst's period, and the empty purges that end it (the freed pages stopped coming), and its cap.
const BURST_MS: u64 = 10;
const BURST_IDLE_END: u32 = 3;
const BURST_MAX: u32 = 100;
/// A burst's forced purges end this long after it began, whatever is left (a forced burst while
/// statements reload parts is the D4571 class); after it the ticks purge expired ranges only.
const BURST_WINDOW_NS: u64 = 1_000_000_000;
const STACK: usize = 256 << 10;

static WAKE: AtomicBool = AtomicBool::new(false);
static THREAD: pgsync::OnceLock<pgsync::thread::Thread> = pgsync::OnceLock::new();
/// Counters (tests, the report line): cuts that freed bytes, bytes freed, bytes held past the caps at the last cut,
/// purge bursts, purges that purged.
static CUTS: AtomicU64 = AtomicU64::new(0);
static FREED: AtomicU64 = AtomicU64::new(0);
static HELD: AtomicU64 = AtomicU64::new(0);
static BURSTS: AtomicU64 = AtomicU64::new(0);
static PURGES: AtomicU64 = AtomicU64::new(0);
/// C4: ticks that swept the general arenas, and arenas a sweep purged something in.
static SWEEPS: AtomicU64 = AtomicU64::new(0);
static GENERAL_PURGES: AtomicU64 = AtomicU64::new(0);
/// The hold: ticks under ladder pressure and the bytes purged on them; arenas an idle purge purged something in and the
/// bytes it purged.
static PRESSED: AtomicU64 = AtomicU64::new(0);
static PRESSED_BYTES: AtomicU64 = AtomicU64::new(0);
static IDLE_PURGES: AtomicU64 = AtomicU64::new(0);
static IDLE_BYTES: AtomicU64 = AtomicU64::new(0);
/// The purger's passes finished (the R6 landing's wait for the purge after its victim let go, `passes`): a tick under
/// ladder pressure counts once its pressure purge is done, any other tick at its end. `PURGING` while one runs.
static PASSES: AtomicU64 = AtomicU64::new(0);
static PURGING: AtomicBool = AtomicBool::new(false);
/// The pressure mode's asks since the purger's last tick (`ask`): `ASK_NEED` and `ASK_LET_GO` bits.
static ASKED: AtomicU8 = AtomicU8::new(0);
/// A burst ended early for a pressure tick (`burst_yields`): the next tick runs at once, not after a park.
static AGAIN: AtomicBool = AtomicBool::new(false);
/// Bursts ended early for a pressure tick.
static BURST_YIELDS: AtomicU64 = AtomicU64::new(0);
const ASK_NEED: u8 = 1;
const ASK_LET_GO: u8 = 2;
/// The pressure mode: its stretches' summed ns, the start of the open one (0 = none), its entries per cause
/// (`mempolicy::ladder::PurgeModeCause`: own read, ask, let-go).
static MODE_NS: AtomicU64 = AtomicU64::new(0);
static MODE_SINCE_NS: AtomicU64 = AtomicU64::new(0);
static MODE_ENTRIES: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// The hold's longest idle stretch (seconds) the knob takes as a number.
const HOLD_MAX_S: u64 = 3600;
/// The hold with no timer (`never`): only pressure purges.
const HOLD_NEVER: u64 = u64::MAX;

/// Whether C3's switch is on (`PGRUST_CACHE_PURGE_NOW=1`).
pub fn enabled() -> bool {
    switches().0
}

/// Whether C4's switch is on (`PGRUST_QUERY_PURGE_BG=1`).
pub fn query_bg() -> bool {
    switches().1
}

/// Whether this thread owns the general arenas' purge (C4's switch on and the purger running): a freed page then leaves
/// the memory only at one of its ticks, not at the free.
pub fn owns_general() -> bool {
    #[cfg(test)]
    {
        if TEST_OWNS.load(Relaxed) {
            return true;
        }
    }
    switches().1 && THREAD.get().is_some()
}

/// The purger's passes finished since start (one relaxed load; only the purger writes it).
pub fn passes() -> u64 {
    PASSES.load(Relaxed)
}

/// The count `passes` reaches once a pass begun after this call has finished: one more, or two while a pass runs now (it
/// may have begun before). Read in that order (SeqCst with the purger's stores), it never names a pass begun earlier.
pub fn pass_mark() -> u64 {
    let running = PURGING.load(SeqCst);
    PASSES.load(SeqCst) + if running { 2 } else { 1 }
}

/// The pressure mode's ask: a ladder pass left a need, or an R6 victim let go (`let_go`); the purger's next tick enters the
/// mode (or stays in it), and it is unparked for it now. A no-op with no purger thread.
pub fn ask(let_go: bool) {
    if THREAD.get().is_none() {
        return;
    }
    ASKED.fetch_or(if let_go { ASK_LET_GO } else { ASK_NEED }, SeqCst);
    nudge();
}

/// Whether the pressure mode's switch is on (`PGRUST_PURGE_PRESSURE_MODE=1`).
pub fn pressure_mode() -> bool {
    switches().3
}

/// The pressure mode: `(ms in it, entries on the purger's own read, on a ladder pass's ask, on an R6 let-go)`.
pub fn mode_counts() -> (u64, u64, u64, u64) {
    let open = match MODE_SINCE_NS.load(Relaxed) {
        0 => 0,
        since => mcx::ladder_stats::clock_ns().saturating_sub(since),
    };
    let entries = |i: usize| MODE_ENTRIES[i].load(Relaxed);
    ((MODE_NS.load(Relaxed) + open) / 1_000_000, entries(0), entries(1), entries(2))
}

/// Unpark the purger for its next pass now (no burst, unlike `wake`); a no-op with no purger thread.
pub fn nudge() {
    if let Some(t) = THREAD.get() {
        t.unpark();
    }
}

/// Tests: a purger that owns the general arenas' purge without its thread, and its ticks.
#[cfg(test)]
pub(crate) static TEST_OWNS: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
pub(crate) fn test_tick() {
    PASSES.fetch_add(1, SeqCst);
}

#[cfg(test)]
pub(crate) fn test_purging(on: bool) {
    PURGING.store(on, SeqCst);
}

/// The hold's idle stretch in ms (`PGRUST_PURGE_AFTER_IDLE_S` above 1, at most `HOLD_MAX_S`; `HOLD_NEVER` for `never`);
/// 0 = no hold (today).
pub fn hold_ms() -> u64 {
    switches().2
}

/// (C3's switch, C4's switch, the hold in ms, the pressure mode's switch `PGRUST_PURGE_PRESSURE_MODE`), read once.
fn switches() -> (bool, bool, u64, bool) {
    static ON: pgsync::OnceLock<(bool, bool, u64, bool)> = pgsync::OnceLock::new();
    *ON.get_or_init(|| {
        let var = |name: &str| std::env::var(name).ok();
        let on = |name: &str| var(name).is_some_and(|v| v.trim() == "1");
        (
            on("PGRUST_CACHE_PURGE_NOW"),
            on("PGRUST_QUERY_PURGE_BG"),
            hold_from(var("PGRUST_PURGE_AFTER_IDLE_S").as_deref()),
            on("PGRUST_PURGE_PRESSURE_MODE"),
        )
    })
}

/// The knob's value to the hold in ms: whole seconds above 1 (capped at `HOLD_MAX_S`), `never` = `HOLD_NEVER`; unset, 1,
/// 0 or unparsable = 0.
fn hold_from(v: Option<&str>) -> u64 {
    let v = v.map(str::trim);
    if v == Some("never") {
        return HOLD_NEVER;
    }
    match v.and_then(|v| v.parse::<u64>().ok()) {
        Some(s) if s > 1 => s.min(HOLD_MAX_S) * 1000,
        _ => 0,
    }
}

/// Ladder pressure while the purger holds: the ladder's whole read past its line (the climb's own test at ask 0; the
/// freed pages the hold keeps are part of the reading, and the purge is what gives them back). With the ladder not on, its
/// line is still published at every memory sample (the limit less the margin, under the cancel's), read against that
/// sample's resident bytes. No limit known (line 0): never (only the timer, if any, ends the hold).
fn pressure() -> bool {
    mcx::ladder::pressed_now()
}

/// This tick under the pressure mode `mode` (`mempolicy::ladder::purge_mode_tick`): the asks since the last tick taken,
/// the mode stepped, its time and entries counted. Whether the tick is a pressure tick.
fn pressed_in_mode(mode: &mut PurgeMode) -> bool {
    let asked = ASKED.swap(0, SeqCst);
    let past = pressure();
    let back = mode.on && mcx::ladder::read_now().is_some_and(mcx::ladder::at_take_target);
    let (pressed, next, cause) = mempolicy::ladder::purge_mode_tick(*mode, past, asked & ASK_NEED != 0, asked & ASK_LET_GO != 0, back);
    let now = mcx::ladder_stats::clock_ns().max(1);
    if let Some(c) = cause {
        MODE_ENTRIES[c as usize].fetch_add(1, Relaxed);
        MODE_SINCE_NS.store(now, Relaxed);
    }
    if mode.on && !next.on {
        let since = MODE_SINCE_NS.swap(0, Relaxed);
        MODE_NS.fetch_add(now.saturating_sub(since), Relaxed);
    }
    *mode = next;
    pressed
}


/// Wake the purger now (`mcx::wake_purger` calls it): it applies the cut and purges in a burst. Never waits.
pub fn wake() {
    WAKE.store(true, Relaxed);
    if let Some(t) = THREAD.get() {
        t.unpark();
    }
}

/// `(cuts that freed bytes, bytes freed, bytes held at the last cut, bursts, purges that purged)`.
pub fn counts() -> (u64, u64, u64, u64, u64) {
    (CUTS.load(Relaxed), FREED.load(Relaxed), HELD.load(Relaxed), BURSTS.load(Relaxed), PURGES.load(Relaxed))
}

/// C4: `(general sweeps, arenas a sweep purged something in)`.
pub fn general_counts() -> (u64, u64) {
    (SWEEPS.load(Relaxed), GENERAL_PURGES.load(Relaxed))
}

/// The hold: `(ticks under ladder pressure, bytes purged on them, arenas an idle purge purged something in, bytes the idle
/// purges purged)`.
pub fn hold_counts() -> (u64, u64, u64, u64) {
    (PRESSED.load(Relaxed), PRESSED_BYTES.load(Relaxed), IDLE_PURGES.load(Relaxed), IDLE_BYTES.load(Relaxed))
}

/// Start the purger when the switch is on (memruntime's `start`, once, before any backend). Whether it started.
pub fn start() -> bool {
    let (cache, general, _, _) = switches();
    if !(cache || general) || THREAD.get().is_some() {
        return false;
    }
    // at the default priority (nice 0): at nice 10 a saturating load starved it past the dead-man stretch (1 s), and an
    // ordinary collect on a query thread purged its arenas instead
    let spawned = std::thread::Builder::new().name("pg:mempurge".into()).stack_size(STACK).spawn(pagegive::pinned(|| {
        mcx::set_purge_wake(wake);
        // unwind-ok: log-then-die
        let _ = std::panic::catch_unwind(run);
    }));
    match spawned {
        Ok(h) => {
            let _ = THREAD.set(h.thread().clone());
            mcx::set_purge_policy(hold_counts);
            // Purge owned: from now on only this thread purges the cache arena (mimalloc's dead-man fallback
            // lets an ordinary collect purge it after 1 s without this thread's purge call); C4: the general arenas too
            if cache {
                mcx::cache_purge_owner(true);
            }
            if general {
                mcx::general_purge_owner(true);
            }
            true
        }
        Err(_) => false,
    }
}

fn run() {
    let (cache, general, hold, mode_on) = switches();
    let mut mode = PurgeMode::default();
    loop {
        let mut woke = WAKE.swap(false, Relaxed);
        if !woke && !AGAIN.swap(false, Relaxed) {
            pgsync::thread::park_timeout(Duration::from_millis(TICK_MS));
            woke = WAKE.swap(false, Relaxed);
        }
        PURGING.store(true, SeqCst);
        let pressed = if mode_on {
            pressed_in_mode(&mut mode)
        } else {
            ASKED.store(0, Relaxed);
            pressure()
        };
        tick_with(&Live, cache, general, hold, woke, pressed);
        pass_done();
    }
}

/// What a tick reads and does, through one seam: the purger's own (`Live`: the hooks, the clock, the sleep) or a test's
/// fixed inputs (a scripted cut and purges, a clock that moves only at a pause, no sleep). The decisions (`tick_with`,
/// `step_with`, `burst_with`) and their counters read nothing else.
trait Io {
    /// C3's cut (`mcx::cache_cut`): each cache's cap re-read, a cache past it trimmed.
    fn cut(&self) -> mcx::CacheCut;
    /// The cache arena purged (`force`: every free page, else its expired ranges only); whether it found pages.
    fn purge(&self, force: bool) -> bool;
    /// C4: the general arenas' expired ranges purged; the arenas that purged something.
    fn general_purge(&self) -> u32;
    /// The owners asked to collect their pages (a burst's start).
    fn collect(&self);
    /// Whether a burst ends now for a pressure tick (`burst_yields`).
    fn yields(&self) -> bool;
    /// Whether a burst keeps C4's sweep (C4's switch).
    fn sweeps(&self) -> bool;
    /// The clock (ns), and the pause between a burst's purges.
    fn now(&self) -> u64;
    fn pause(&self);
    /// Ladder pressure's purge (the parked regions trimmed first under `PGRUST_PURGE_PRESSURE_BG`, then every free
    /// committed run of every arena); the bytes it purged.
    fn pressure_purge(&self) -> u64;
    /// The tick's pass done (`pass_done`).
    fn pass_done(&self);
    /// The hold's idle purge of the owned arenas (`mcx::idle_purge`); the arenas that purged something.
    fn idle_purge(&self, general: bool, cache: bool, hold: u64) -> u32;
    /// The bytes purged so far (`mcx::purged_bytes`).
    fn purged(&self) -> u64;
}

/// The purger thread's `Io`.
struct Live;

impl Io for Live {
    fn cut(&self) -> mcx::CacheCut {
        mcx::cache_cut()
    }
    fn purge(&self, force: bool) -> bool {
        mcx::cache_purge(force)
    }
    fn general_purge(&self) -> u32 {
        mcx::general_purge()
    }
    fn collect(&self) {
        let _ = mcx::ask_collect();
    }
    fn yields(&self) -> bool {
        burst_yields(switches().3, ASKED.load(SeqCst), pressure())
    }
    fn sweeps(&self) -> bool {
        switches().1
    }
    fn now(&self) -> u64 {
        mcx::ladder_stats::clock_ns()
    }
    fn pause(&self) {
        pgsync::thread::sleep(Duration::from_millis(BURST_MS));
    }
    fn pressure_purge(&self) -> u64 {
        // PGRUST_PURGE_PRESSURE_BG: the parked regions and slot sets go first (their pages then purge with the rest)
        if mcx::purge_bg() {
            mcx::pressure_trim();
        }
        mcx::pressure_purge(true)
    }
    fn pass_done(&self) {
        pass_done();
    }
    fn idle_purge(&self, general: bool, cache: bool, hold: u64) -> u32 {
        mcx::idle_purge(general, cache, hold)
    }
    fn purged(&self) -> u64 {
        mcx::purged_bytes().0
    }
}

/// One tick: with no hold (or under ladder pressure), C4's sweep and C3's step as they were; while holding, an idle purge
/// of the owned arenas instead (C3's cut still applies: the cache gives its bytes up, the pages stay committed for reuse).
/// Under the hold, the bytes the tick purged are counted as ladder pressure's or the idle purge's.
fn tick_with(io: &dyn Io, cache: bool, general: bool, hold: u64, woke: bool, pressed: bool) {
    if hold == 0 {
        // ladder pressure without the hold: the freed pages the purge delay still keeps go back at once (every free and
        // committed run of every arena, `mcx::pressure_purge`), then the tick as before
        if pressed {
            PRESSED.fetch_add(1, Relaxed);
            PRESSED_BYTES.fetch_add(io.pressure_purge(), Relaxed);
            // every free committed run is purged: a let-go victim's pages are back (the R6 landing's wait reads this)
            io.pass_done();
            // a pressure tick never starts a burst: its purge just took every free committed run, the cache arena's too;
            // the sweep keeps the owner marked alive, the cut keeps the caches to their caps
            if general {
                sweep_with(io);
            }
            if cache {
                let _ = cut_with(io);
            }
            return;
        }
        return tick_as(io, cache, general, hold, woke, false);
    }
    let before = io.purged();
    tick_as(io, cache, general, hold, woke, pressed);
    let purged = io.purged().saturating_sub(before);
    if pressed {
        PRESSED.fetch_add(1, Relaxed);
        PRESSED_BYTES.fetch_add(purged, Relaxed);
    } else {
        IDLE_BYTES.fetch_add(purged, Relaxed);
    }
}

fn tick_as(io: &dyn Io, cache: bool, general: bool, hold: u64, woke: bool, pressed: bool) {
    let held = hold != 0 && !pressed;
    if held {
        if cache {
            let _ = cut_with(io);
        }
        IDLE_PURGES.fetch_add(u64::from(io.idle_purge(general, cache, hold)), Relaxed);
        return;
    }
    if general {
        sweep_with(io);
    }
    if cache {
        let _ = step_with(io, woke);
    }
}

/// A pass is done (`passes`), once per tick: the first call of the tick counts.
fn pass_done() {
    if PURGING.swap(false, SeqCst) {
        PASSES.fetch_add(1, SeqCst);
    }
}

/// C4: the general arenas' expired ranges purged (every owned arena, one cheap call each when nothing expired).
fn sweep_with(io: &dyn Io) {
    SWEEPS.fetch_add(1, Relaxed);
    GENERAL_PURGES.fetch_add(u64::from(io.general_purge()), Relaxed);
}

/// What C3's step did: a burst, or the quiet tick's one purge of the expired ranges (whether it found pages).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Burst(Burst),
    Quiet(bool),
}

/// A burst: its purges, those that found pages, and why it ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Burst {
    purges: u32,
    found: u32,
    end: BurstEnd,
}

/// Why a burst ended: `BURST_IDLE_END` empty purges in a row, `BURST_WINDOW_NS` since it began, `BURST_MAX` purges, or a
/// pressure tick asked for (`burst_yields`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BurstEnd {
    Dry,
    Window,
    Max,
    Yield,
}

/// C3's tick: the cut; a burst when it freed bytes or a wake asked for one, else the cache arena's expired ranges purged.
fn step_with(io: &dyn Io, woke: bool) -> Step {
    if cut_with(io) > 0 || woke {
        Step::Burst(burst_with(io))
    } else {
        Step::Quiet(purge_with(io, false))
    }
}

/// Whether a C3 burst ends now for a pressure tick: its own read past the line, or under the pressure mode (`mode`) an ask
/// pending (`asked`: a ladder pass's need, an R6 victim's let-go). A burst runs inside one tick for up to
/// `BURST_WINDOW_NS`, and no pressure tick can run until it ends: an R6 victim's freed pages then waited behind the cache
/// arena's purges while a grower took the last of the memory. The pressure tick purges the cache arena too.
fn burst_yields(mode: bool, asked: u8, pressed: bool) -> bool {
    pressed || (mode && asked != 0)
}

/// Bursts ended early for a pressure tick (`burst_yields`).
pub fn burst_yield_count() -> u64 {
    BURST_YIELDS.load(Relaxed)
}

/// C3's cut, counted; the bytes it freed.
fn cut_with(io: &dyn Io) -> u64 {
    let cut = io.cut();
    HELD.store(cut.held, Relaxed);
    if cut.freed > 0 {
        CUTS.fetch_add(1, Relaxed);
        FREED.fetch_add(cut.freed, Relaxed);
    }
    cut.freed
}

/// The cache arena purged, counted when it found pages; whether it did.
fn purge_with(io: &dyn Io, force: bool) -> bool {
    let found = io.purge(force);
    if found {
        PURGES.fetch_add(1, Relaxed);
    }
    found
}

/// A cut's purge: the owners asked to collect their pages (a freed image's page leaves its owner's heap only on that
/// thread), then the cache arena purged every `BURST_MS` until `BURST_IDLE_END` purges in a row find nothing, or
/// `BURST_WINDOW_NS` after it began. Only a real cut (a cap that went down) or a give-back's wake starts one.
fn burst_with(io: &dyn Io) -> Burst {
    BURSTS.fetch_add(1, Relaxed);
    io.collect();
    let start = io.now();
    let mut b = Burst { purges: 0, found: 0, end: BurstEnd::Max };
    let mut idle = 0;
    for i in 0..BURST_MAX {
        if io.now().saturating_sub(start) >= BURST_WINDOW_NS {
            b.end = BurstEnd::Window;
            break;
        }
        if io.yields() {
            BURST_YIELDS.fetch_add(1, Relaxed);
            AGAIN.store(true, Relaxed);
            b.end = BurstEnd::Yield;
            break;
        }
        // C4: the general arenas keep their sweep (and their owner's mark) during a burst
        if i % 5 == 4 && io.sweeps() {
            sweep_with(io);
        }
        b.purges += 1;
        if purge_with(io, true) {
            b.found += 1;
            idle = 0;
        } else {
            idle += 1;
            if idle >= BURST_IDLE_END {
                b.end = BurstEnd::Dry;
                break;
            }
        }
        io.pause();
    }
    b
}

#[cfg(test)]
mod tests {
    use super::{Burst, BurstEnd, Io, Step};
    use core::sync::atomic::Ordering::Relaxed;
    use std::cell::{Cell, RefCell};

    /// The counters are process-wide: one test at a time reads their deltas. Every input is the test's own (`Fake`): no
    /// hook, no clock, no ladder reading another test publishes, no purger thread.
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn turn() -> std::sync::MutexGuard<'static, ()> {
        TURN.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What a tick did, in order.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum D {
        Cut,
        Purge(bool),
        Sweep,
        Collect,
        Pause,
        PressurePurge,
        PassDone,
        IdlePurge(bool, bool, u64),
    }

    /// Fixed inputs: the next cut's freed bytes (`freed`), the purges that find pages before the arena runs dry (`finds`),
    /// the `yields` call that asks for a pressure tick (`yield_at`, 0 = none), C4's switch (`sweeps`), and a clock that
    /// moves `pause_ns` at each pause and nowhere else. Each idle purge purges 2 MiB, ladder pressure's 5 MiB.
    #[derive(Default)]
    struct Fake {
        freed: Cell<u64>,
        finds: Cell<u32>,
        yield_at: Cell<u32>,
        asked: Cell<u32>,
        sweeps: bool,
        pause_ns: u64,
        clock: Cell<u64>,
        purged: Cell<u64>,
        did: RefCell<Vec<D>>,
    }

    impl Fake {
        fn new(pause_ns: u64) -> Fake {
            Fake { pause_ns, ..Fake::default() }
        }

        fn log(&self, d: D) {
            self.did.borrow_mut().push(d);
        }

        /// What it did since the last call.
        fn take(&self) -> Vec<D> {
            self.did.take()
        }
    }

    impl Io for Fake {
        fn cut(&self) -> mcx::CacheCut {
            self.log(D::Cut);
            mcx::CacheCut { freed: self.freed.take(), held: 7 }
        }
        fn purge(&self, force: bool) -> bool {
            self.log(D::Purge(force));
            let n = self.finds.get();
            self.finds.set(n.saturating_sub(1));
            n > 0
        }
        fn general_purge(&self) -> u32 {
            self.log(D::Sweep);
            3
        }
        fn collect(&self) {
            self.log(D::Collect);
        }
        fn yields(&self) -> bool {
            self.asked.set(self.asked.get() + 1);
            self.asked.get() == self.yield_at.get()
        }
        fn sweeps(&self) -> bool {
            self.sweeps
        }
        fn now(&self) -> u64 {
            self.clock.get()
        }
        fn pause(&self) {
            self.log(D::Pause);
            self.clock.set(self.clock.get() + self.pause_ns);
        }
        fn pressure_purge(&self) -> u64 {
            self.log(D::PressurePurge);
            5 << 20
        }
        fn pass_done(&self) {
            self.log(D::PassDone);
        }
        fn idle_purge(&self, general: bool, cache: bool, hold: u64) -> u32 {
            self.log(D::IdlePurge(general, cache, hold));
            self.purged.set(self.purged.get() + (2 << 20));
            2
        }
        fn purged(&self) -> u64 {
            self.purged.get()
        }
    }

    const P: D = D::Purge(true);
    const BURST_PAUSE_NS: u64 = super::BURST_MS * 1_000_000;

    /// C3: a cut that freed bytes purges the cache arena in a burst (forced, until three purges in a row find nothing); a
    /// tick with no cut and no wake purges only the expired ranges, once; a wake bursts even when the cut freed nothing.
    #[test]
    fn a_cut_bursts_until_the_freed_pages_stop_and_a_quiet_tick_purges_once() {
        let _turn = turn();
        let f = Fake::new(BURST_PAUSE_NS);
        let (cuts0, freed0, _, bursts0, purges0) = super::counts();
        f.freed.set(4 << 20);
        f.finds.set(2);
        let idle = super::BURST_IDLE_END;
        assert_eq!(
            super::step_with(&f, false),
            Step::Burst(Burst { purges: 2 + idle, found: 2, end: BurstEnd::Dry }),
            "two purges found pages, then three found none"
        );
        assert_eq!(f.take(), [D::Cut, D::Collect, P, D::Pause, P, D::Pause, P, D::Pause, P, D::Pause, P]);
        let (cuts, freed, held, bursts, purges) = super::counts();
        assert_eq!((cuts - cuts0, freed - freed0, held, bursts - bursts0, purges - purges0), (1, 4 << 20, 7, 1, 2));
        assert_eq!(super::step_with(&f, false), Step::Quiet(false), "a quiet tick");
        assert_eq!(f.take(), [D::Cut, D::Purge(false)], "one purge, of the expired ranges only (not forced)");
        f.finds.set(1);
        assert_eq!(
            super::step_with(&f, true),
            Step::Burst(Burst { purges: 1 + idle, found: 1, end: BurstEnd::Dry }),
            "a wake bursts without a cut"
        );
        assert_eq!(super::counts().3 - bursts0, 2);
    }

    /// A burst whose purges keep finding pages ends `BURST_WINDOW_NS` after it began, or at `BURST_MAX` purges; C4's sweep
    /// runs at every fifth purge when its switch is on.
    #[test]
    fn a_burst_ends_at_its_window_or_its_cap() {
        let _turn = turn();
        let f = Fake::new(300_000_000);
        f.finds.set(1_000);
        assert_eq!(
            super::burst_with(&f),
            Burst { purges: 4, found: 4, end: BurstEnd::Window },
            "purges at 0, 300, 600 and 900 ms; the window closed at 1.2 s"
        );
        let f = Fake { sweeps: true, ..Fake::new(0) };
        f.finds.set(1_000);
        let max = super::BURST_MAX;
        assert_eq!(super::burst_with(&f), Burst { purges: max, found: max, end: BurstEnd::Max });
        let did = f.take();
        assert_eq!(did[..12], [D::Collect, P, D::Pause, P, D::Pause, P, D::Pause, P, D::Pause, D::Sweep, P, D::Pause]);
        assert_eq!(did.iter().filter(|d| **d == D::Sweep).count(), (max / 5) as usize);
    }

    /// A burst ends at its first purge's turn when a pressure tick is asked for (an R6 victim let go under the pressure mode,
    /// or the read is past the line), and the next tick runs at once; with none asked it runs as before. A pressure tick
    /// never starts a burst.
    #[test]
    fn a_burst_ends_at_once_for_a_pressure_tick() {
        let _turn = turn();
        assert!(super::burst_yields(true, super::ASK_LET_GO, false), "the pressure mode, a let-go asked");
        assert!(super::burst_yields(false, 0, true), "its own read past the line, mode or not");
        assert!(!super::burst_yields(false, super::ASK_LET_GO, false), "an ask with the mode off is not a pressure tick");
        assert!(!super::burst_yields(true, 0, false), "nothing asked, the read under the line: the burst goes on");
        let yields0 = super::burst_yield_count();
        let f = Fake::new(BURST_PAUSE_NS);
        f.finds.set(1_000);
        f.yield_at.set(1);
        assert_eq!(super::burst_with(&f), Burst { purges: 0, found: 0, end: BurstEnd::Yield });
        assert_eq!(f.take(), [D::Collect], "no cache-arena purge before the pressure tick");
        assert_eq!(super::burst_yield_count(), yields0 + 1);
        assert!(super::AGAIN.swap(false, Relaxed), "the next tick does not park");
        let f = Fake::new(BURST_PAUSE_NS);
        f.finds.set(1_000);
        f.yield_at.set(3);
        assert_eq!(
            super::burst_with(&f),
            Burst { purges: 2, found: 2, end: BurstEnd::Yield },
            "asked during the burst: it ends at its next turn"
        );
        assert!(super::AGAIN.swap(false, Relaxed));
        // a pressure tick never starts a burst, woken or not, whatever its cut freed
        let f = Fake::new(BURST_PAUSE_NS);
        f.freed.set(4 << 20);
        f.finds.set(1_000);
        let (pressed0, bytes0, _, _) = super::hold_counts();
        super::tick_with(&f, true, false, 0, true, true);
        assert_eq!(f.take(), [D::PressurePurge, D::PassDone, D::Cut], "the pressure purge, its pass, the cut; no burst");
        let (pressed, bytes, _, _) = super::hold_counts();
        assert_eq!((pressed - pressed0, bytes - bytes0), (1, 5 << 20));
    }

    /// C4: a sweep purges the general arenas' expired ranges once and counts the arenas that purged.
    #[test]
    fn a_sweep_purges_the_general_arenas_expired_ranges_once() {
        let _turn = turn();
        let f = Fake::new(0);
        let (s0, p0) = super::general_counts();
        super::sweep_with(&f);
        assert_eq!(f.take(), [D::Sweep]);
        let (s1, p1) = super::general_counts();
        assert_eq!((s1 - s0, p1 - p0), (1, 3));
    }

    /// The hold: the knob in whole seconds above 1 (capped), anything else no hold; a held tick runs the owned arenas'
    /// idle purge and no sweep, even when woken (the cut still applies, no burst); a tick under pressure sweeps as without
    /// the hold; no hold: today's sweep.
    #[test]
    fn the_hold_purges_after_idle_and_pressure_ends_it() {
        let _turn = turn();
        let ms: Vec<u64> =
            [None, Some("1"), Some("0"), Some("x"), Some(" 60 "), Some("999999"), Some(" never")].into_iter().map(super::hold_from).collect();
        assert_eq!(ms, [0, 0, 0, 0, 60_000, super::HOLD_MAX_S * 1000, super::HOLD_NEVER]);
        let f = Fake::new(0);
        let (pressed0, _, idle0, idle_bytes0) = super::hold_counts();
        super::tick_with(&f, false, true, 60_000, true, false);
        assert_eq!(f.take(), [D::IdlePurge(true, false, 60_000)], "held: the idle purge only");
        let (pressed, _, idle, idle_bytes) = super::hold_counts();
        assert_eq!((pressed, idle - idle0, idle_bytes - idle_bytes0), (pressed0, 2, 2 << 20));
        super::tick_with(&f, false, true, 60_000, false, true);
        assert_eq!(f.take(), [D::Sweep], "pressed: today's sweep");
        assert_eq!(super::hold_counts().0, pressed0 + 1);
        super::tick_with(&f, false, true, 0, false, false);
        assert_eq!(f.take(), [D::Sweep], "no hold: today's sweep");
        assert_eq!(super::hold_counts().0, pressed0 + 1);
        super::tick_with(&f, false, true, 0, false, true);
        assert_eq!(f.take(), [D::PressurePurge, D::PassDone, D::Sweep], "no hold, pressed: ladder pressure's purge, then the sweep");
        assert_eq!(super::hold_counts().0, pressed0 + 2, "and counted");
        f.freed.set(4 << 20);
        f.finds.set(1_000);
        super::tick_with(&f, true, false, 60_000, true, false);
        assert_eq!(f.take(), [D::Cut, D::IdlePurge(false, true, 60_000)], "held with the cache's switch: the cut, no burst");
    }
}

//! Lane ladderview (the broker session, 2026-10-15; reports/LADDER-REVIEW-20261015.md section 3 item 10): the memory
//! ladder made readable in SQL. Read-only counters on the slow paths, and a ring of the last `RING` ladder episodes;
//! gravity_hosts `membroker/view.rs` serves them as `pgrust_memory_ladder_stats()`, `pgrust_memory_ladder_episodes()`
//! and `pgrust_memory_revoke_stats()` (scripts/memory-broker-views.sql).
//!
//! Observe only: no decision reads anything here. Every note is a relaxed add on a path that already missed (a gate
//! whose take missed, a rung that ran, a cancel, a revoke ask settled), never on the pool's fast path. The gates count
//! with `pgrust.memory_ladder` off too, so the default config's waits and refusals show.
//!
//! - Per site ([`Site`]: the gates of LADDER-DESIGN section 3, the D4403 small draw's wait and the allocator hook):
//!   misses (waits + refusals with no wait), waits, wait ns (sum, max), refusals, LastTry ends.
//! - Per rung: the ns its runs took (`mcx::ladder::counts` keeps the runs and their measured relief).
//! - Cancels by path: the watchdog's, the ladder's R6 and D4029's terminate (the projection rule's two are
//!   `governor::decide_counts`).
//! - Revoke: give-backs and their bytes (asks are `revoke::ASKS` / `ASKED_BYTES`), and per settled ask two log2-ms
//!   histograms: clock 1, ask to stopped growing (REVOKESLA's last cut), and clock 2, ask to the last bytes given back.
//! - The episode ring: one entry per gate wait and per hook episode, with the rungs that ran on its thread and their
//!   measured relief, and its outcome.

use core::sync::atomic::{fence, AtomicU64, AtomicU8, Ordering::*};

use mempolicy::ladder::Rung;

/// The rungs, in `Rung`'s order (R1 R2a R3 R4a R4b R5 R2b R6 R2p).
pub const RUNGS: usize = 9;

/// Where the ladder is met: a gate whose asker waits (LADDER-DESIGN section 3), or the allocator hook (the ladder thread).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Site {
    /// The grant pool's draws past W and G (`GrantPool::take_wait`; refusals at `hold` too).
    Draw = 0,
    /// Admission (`GrantPool::admit_need_within`).
    Admit = 1,
    /// D4329's out-of-grant working memory (`mem::region::scratch_draw`).
    Scratch = 2,
    /// D4403: a small draw with no reservation whose buffer and big lane were both full (`small_draw_wait`).
    SmallDraw = 3,
    /// The pgrcolumnar load gate (pgrc_write `ladder_step`).
    Load = 4,
    /// The ANALYZE sample gate (pgrc_am `sample_window_dop`; its refusal is the serial road).
    Sample = 5,
    /// The allocator hook's episode on the ladder thread (memruntime's sample past the line); its refusal is a victim
    /// canceled.
    Hook = 6,
    /// D4812 (2): the block gate (`ladder_block_gate`): a memory-context block of at least 1 MiB past the watchdog's
    /// cancel line waits on its allocating thread; its refusal is its own statement's 53200.
    Block = 7,
}

impl Site {
    pub const ALL: [Site; 8] = [Site::Draw, Site::Admit, Site::Scratch, Site::SmallDraw, Site::Load, Site::Sample, Site::Hook, Site::Block];

    pub const fn name(self) -> &'static str {
        match self {
            Site::Draw => "draw",
            Site::Admit => "admit",
            Site::Scratch => "scratch",
            Site::SmallDraw => "small_draw",
            Site::Load => "load",
            Site::Sample => "sample",
            Site::Hook => "hook",
            Site::Block => "block",
        }
    }

    /// The site of a ladder gate.
    pub const fn of(g: crate::ladder::Gate) -> Site {
        match g {
            crate::ladder::Gate::Draw => Site::Draw,
            crate::ladder::Gate::Admit => Site::Admit,
            crate::ladder::Gate::Scratch => Site::Scratch,
        }
    }

    const fn from_u8(x: u8) -> Site {
        match x {
            0 => Site::Draw,
            1 => Site::Admit,
            2 => Site::Scratch,
            3 => Site::SmallDraw,
            4 => Site::Load,
            5 => Site::Sample,
            7 => Site::Block,
            _ => Site::Hook,
        }
    }
}

pub const SITES: usize = Site::ALL.len();

/// How an episode ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Outcome {
    /// The asker got its bytes (the hook: the read went back under the line with no cancel).
    Covered = 1,
    /// Admission seated it below its ask (halved down to the floor, or alone).
    Halved = 2,
    /// The gate refused: the caller takes its refusal road (a spill, a 53200, the sample's serial road).
    Refused = 3,
    /// The asker's own statement was interrupted while it waited.
    Canceled = 4,
    /// The hook's episode ran R6: a victim was canceled.
    VictimCanceled = 5,
    /// D4812 (2): the block gate's bound passed with the reading still past the cancel line, and the block was taken
    /// (another statement was R6's pick, or a cancel was in flight).
    Overran = 6,
    /// D4812 (2): the block gate stopped waiting for a pending interrupt (a cancel, a timeout, a die) and took the block.
    Interrupted = 7,
}

impl Outcome {
    pub const fn name(self) -> &'static str {
        match self {
            Outcome::Covered => "covered",
            Outcome::Halved => "halved",
            Outcome::Refused => "refused",
            Outcome::Canceled => "canceled",
            Outcome::VictimCanceled => "victim_canceled",
            Outcome::Overran => "overran",
            Outcome::Interrupted => "interrupted",
        }
    }

    const fn from_u8(x: u8) -> Outcome {
        match x {
            2 => Outcome::Halved,
            3 => Outcome::Refused,
            4 => Outcome::Canceled,
            5 => Outcome::VictimCanceled,
            6 => Outcome::Overran,
            7 => Outcome::Interrupted,
            _ => Outcome::Covered,
        }
    }
}

/// The cancels counted here, by path (the projection rule's `decide_tick` / `serve_due` cancels are
/// `governor::decide_counts`'s `backstop_cancels` / `inline_cancels`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Cancel {
    /// The memory watchdog's backstop (memruntime, threshold rule).
    Watchdog = 0,
    /// The ladder's R6 (memruntime `ladder_cancel`, or its no-victim road's smaller statement).
    LadderR6 = 1,
    /// D4029: a canceled statement that never answered: its session terminated.
    Terminate = 2,
    /// Kill safety: past the cancel line an R6 cancel went unanswered for `R6_ACK_BOUND_MS`, so R6 could act again
    /// (counted once per cancel).
    R6AckTimeout = 3,
}

impl Cancel {
    pub const ALL: [Cancel; 4] = [Cancel::Watchdog, Cancel::LadderR6, Cancel::Terminate, Cancel::R6AckTimeout];

    pub const fn name(self) -> &'static str {
        match self {
            Cancel::Watchdog => "watchdog",
            Cancel::LadderR6 => "ladder_r6",
            Cancel::Terminate => "terminate",
            Cancel::R6AckTimeout => "r6_ack_timeout",
        }
    }
}

/// D4914: the branch that refused a draw past W and G (the statement's grant), counted per branch (`whys`) and kept on
/// the statement (`governor::note_stmt_refusal`), so the 53200 a refusing road raises names its branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Why {
    /// The statement's own part of the pool (`Reservation::grow_fits`) or a worker's ledger cap: refused before the pool.
    PastCap = 1,
    /// D4439: the statement's draw-gate waits had spent their cap (`draw_wait_cap`): one take, then refused at once.
    DrawCapped = 2,
    /// C3: decide(Grow) sent the statement down its own road: refused at once.
    SpillsItself = 3,
    /// The wait ended with no other holder that could give bytes back (`DrawWait::LastTry`) and the last take missed.
    LastTry = 4,
    /// The wait's budget spent (`DrawWait::Refuse`, or `gate_now`'s road).
    WaitSpent = 5,
    /// The statement was canceled while the draw waited.
    Canceled = 6,
    /// A draw that takes no wait (`road_held_now`, an open's own-spill road) found no room.
    NoWait = 7,
    /// The G road's hold missed (`GrantPool::hold`).
    Hold = 8,
    /// The grown region would pass `MAX_REGION`, or cannot grow: no pool asked.
    MaxRegion = 9,
}

impl Why {
    pub const ALL: [Why; 9] = [Why::PastCap, Why::DrawCapped, Why::SpillsItself, Why::LastTry, Why::WaitSpent, Why::Canceled, Why::NoWait, Why::Hold, Why::MaxRegion];

    pub const fn name(self) -> &'static str {
        match self {
            Why::PastCap => "past_cap",
            Why::DrawCapped => "draw_capped",
            Why::SpillsItself => "spills_itself",
            Why::LastTry => "last_try",
            Why::WaitSpent => "wait_spent",
            Why::Canceled => "canceled",
            Why::NoWait => "no_wait",
            Why::Hold => "hold",
            Why::MaxRegion => "max_region",
        }
    }

    /// The branch `x` names (`Why as u8`); `None` for 0 (no refusal) or a value no branch has.
    pub const fn from_u8(x: u8) -> Option<Why> {
        match x {
            1 => Some(Why::PastCap),
            2 => Some(Why::DrawCapped),
            3 => Some(Why::SpillsItself),
            4 => Some(Why::LastTry),
            5 => Some(Why::WaitSpent),
            6 => Some(Why::Canceled),
            7 => Some(Why::NoWait),
            8 => Some(Why::Hold),
            9 => Some(Why::MaxRegion),
            _ => None,
        }
    }
}

pub const WHYS: usize = Why::ALL.len();

// ---------------------------------------------------------------------------------------------------------------------
// The counters.
// ---------------------------------------------------------------------------------------------------------------------

struct Counters {
    misses: AtomicU64,
    waits: AtomicU64,
    wait_ns: AtomicU64,
    wait_max_ns: AtomicU64,
    refusals: AtomicU64,
    last_tries: AtomicU64,
}

impl Counters {
    const fn new() -> Counters {
        Counters { misses: AtomicU64::new(0), waits: AtomicU64::new(0), wait_ns: AtomicU64::new(0), wait_max_ns: AtomicU64::new(0), refusals: AtomicU64::new(0), last_tries: AtomicU64::new(0) }
    }
}

static SITE: [Counters; SITES] = [const { Counters::new() }; SITES];
static RUNG_NS: [AtomicU64; RUNGS] = [const { AtomicU64::new(0) }; RUNGS];
static CANCELS: [AtomicU64; Cancel::ALL.len()] = [const { AtomicU64::new(0) }; Cancel::ALL.len()];
static WHY_COUNT: [AtomicU64; WHYS] = [const { AtomicU64::new(0) }; WHYS];
static WHY_BYTES: [AtomicU64; WHYS] = [const { AtomicU64::new(0) }; WHYS];

/// One site's counters, read now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SiteStats {
    /// Waits plus refusals that never waited.
    pub misses: u64,
    pub waits: u64,
    pub wait_ns: u64,
    pub wait_max_ns: u64,
    /// Refused (or interrupted) asks, after a wait or at once.
    pub refusals: u64,
    /// Waits that ended through `DrawWait::LastTry` (no other holder could give bytes back).
    pub last_tries: u64,
}

pub fn site(s: Site) -> SiteStats {
    let c = &SITE[s as usize];
    SiteStats {
        misses: c.misses.load(Relaxed),
        waits: c.waits.load(Relaxed),
        wait_ns: c.wait_ns.load(Relaxed),
        wait_max_ns: c.wait_max_ns.load(Relaxed),
        refusals: c.refusals.load(Relaxed),
        last_tries: c.last_tries.load(Relaxed),
    }
}

/// The ns each rung's runs took, in `Rung`'s order (R5's is its waits' time: the gates' `wait_ns`; it is 0 here).
pub fn rung_ns() -> [u64; RUNGS] {
    core::array::from_fn(|i| RUNG_NS[i].load(Relaxed))
}

/// The cancels per path, in `Cancel::ALL`'s order.
pub fn cancels() -> [u64; Cancel::ALL.len()] {
    core::array::from_fn(|i| CANCELS[i].load(Relaxed))
}

/// A cancel raised on `path`.
#[cold]
pub fn note_cancel(path: Cancel) {
    CANCELS[path as usize].fetch_add(1, Relaxed);
}

/// D4914: a draw of `bytes` past W and G refused on branch `why`.
#[cold]
pub fn note_why(why: Why, bytes: u64) {
    let i = Why::ALL.iter().position(|&w| w == why).unwrap_or(0);
    WHY_COUNT[i].fetch_add(1, Relaxed);
    WHY_BYTES[i].fetch_add(bytes, Relaxed);
}

/// D4914: the refusals and their bytes per branch, in `Why::ALL`'s order.
pub fn whys() -> [(u64, u64); WHYS] {
    core::array::from_fn(|i| (WHY_COUNT[i].load(Relaxed), WHY_BYTES[i].load(Relaxed)))
}

/// A refusal at `site` with no wait (a miss refused at once).
#[cold]
pub fn note_refused(site: Site) {
    let c = &SITE[site as usize];
    c.misses.fetch_add(1, Relaxed);
    c.refusals.fetch_add(1, Relaxed);
}

/// The ladder's clock for these counters: ns since its first read (0 without std).
pub fn clock_ns() -> u64 {
    #[cfg(any(feature = "std", test))]
    {
        static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        EPOCH.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64 + 1
    }
    #[cfg(not(any(feature = "std", test)))]
    0
}

// ---------------------------------------------------------------------------------------------------------------------
// The rungs that ran on this thread, for the episode open on it.
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Ran {
    mask: u16,
    relief: [u64; RUNGS],
}

#[cfg(any(feature = "std", test))]
std::thread_local! {
    static RAN: core::cell::Cell<Ran> = const { core::cell::Cell::new(Ran { mask: 0, relief: [0; RUNGS] }) };
}

/// This thread's rungs since the last take, cleared.
fn take_ran() -> Ran {
    #[cfg(any(feature = "std", test))]
    {
        RAN.try_with(|r| r.replace(Ran::default())).unwrap_or_default()
    }
    #[cfg(not(any(feature = "std", test)))]
    Ran::default()
}

/// A rung ran on this thread (`mcx::ladder`'s climb, after its re-read): its measured `relief` and the `ns` it took.
#[cold]
pub fn note_rung(rung: Rung, relief: u64, ns: u64) {
    RUNG_NS[rung as usize].fetch_add(ns, Relaxed);
    #[cfg(any(feature = "std", test))]
    let _ = RAN.try_with(|r| {
        let mut x = r.get();
        x.mask |= 1 << rung as u16;
        x.relief[rung as usize] = x.relief[rung as usize].saturating_add(relief);
        r.set(x);
    });
    #[cfg(not(any(feature = "std", test)))]
    let _ = relief;
}

// ---------------------------------------------------------------------------------------------------------------------
// A gate's wait.
// ---------------------------------------------------------------------------------------------------------------------

/// A gate's wait, from its miss to its end (dropped): counted at `site` and recorded in the ring with the rungs that ran on
/// this thread meanwhile. It ends covered unless marked otherwise. Begin it before the climb.
#[derive(Debug)]
pub struct Wait {
    site: Site,
    need: u64,
    read: u64,
    line: u64,
    t0: u64,
    outcome: AtomicU8,
}

impl Wait {
    /// A wait at `site` for `need` bytes begins (a miss).
    #[cold]
    pub fn begin(site: Site, need: u64) -> Wait {
        Wait::begin_at(site, need, crate::ladder::read_now().unwrap_or(0))
    }

    /// `begin` with the read already taken (D4812 (2): the block gate's projected reading; inside an allocation the
    /// ladder's fresh read, a cgroup file read, is never taken).
    #[cold]
    pub fn begin_at(site: Site, need: u64, read: u64) -> Wait {
        let c = &SITE[site as usize];
        c.misses.fetch_add(1, Relaxed);
        c.waits.fetch_add(1, Relaxed);
        let _ = take_ran();
        Wait { site, need, read, line: crate::ladder::line(), t0: clock_ns(), outcome: AtomicU8::new(Outcome::Covered as u8) }
    }

    /// The wait took its last try (`DrawWait::LastTry`).
    pub fn last_try(&self) {
        SITE[self.site as usize].last_tries.fetch_add(1, Relaxed);
    }

    /// It ends refused.
    pub fn refused(&self) {
        self.outcome.store(Outcome::Refused as u8, Relaxed);
    }

    /// It ends with its statement interrupted.
    pub fn canceled(&self) {
        self.outcome.store(Outcome::Canceled as u8, Relaxed);
    }

    /// It ends for a pending interrupt with its block taken (D4812 (2)).
    pub fn interrupted(&self) {
        self.outcome.store(Outcome::Interrupted as u8, Relaxed);
    }

    /// It ends past its bound with its block taken (D4812 (2)).
    pub fn overran(&self) {
        self.outcome.store(Outcome::Overran as u8, Relaxed);
    }

    /// It ends seated below its ask, unless refused or canceled.
    pub fn halved(&self) {
        let _ = self.outcome.compare_exchange(Outcome::Covered as u8, Outcome::Halved as u8, Relaxed, Relaxed);
    }
}

impl Drop for Wait {
    fn drop(&mut self) {
        let end = clock_ns();
        let outcome = Outcome::from_u8(self.outcome.load(Relaxed));
        let ran = take_ran();
        ended(self.site, end.saturating_sub(self.t0), matches!(outcome, Outcome::Refused | Outcome::Canceled));
        record(&Episode { seq: 0, start_ns: self.t0, end_ns: end, site: self.site, need: self.need, read: self.read, line: self.line, ran: ran.mask, relief: ran.relief, outcome });
    }
}

fn ended(site: Site, ns: u64, refused: bool) {
    let c = &SITE[site as usize];
    c.wait_ns.fetch_add(ns, Relaxed);
    c.wait_max_ns.fetch_max(ns, Relaxed);
    if refused {
        c.refusals.fetch_add(1, Relaxed);
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The hook's episode (the ladder thread: memruntime's sample, one thread).
// ---------------------------------------------------------------------------------------------------------------------

static HOOK_T0: AtomicU64 = AtomicU64::new(0);
static HOOK_NEED: AtomicU64 = AtomicU64::new(0);
static HOOK_READ: AtomicU64 = AtomicU64::new(0);
static HOOK_LINE: AtomicU64 = AtomicU64::new(0);

/// The ladder thread's episode opens: the read is `need` bytes past `line`. The rungs its pass ran in the same sample,
/// before the episode was seen, are its own (kept).
#[cold]
pub fn hook_open(need: u64, read: u64, line: u64) {
    if HOOK_T0.load(Relaxed) != 0 {
        return;
    }
    let c = &SITE[Site::Hook as usize];
    c.misses.fetch_add(1, Relaxed);
    c.waits.fetch_add(1, Relaxed);
    HOOK_NEED.store(need, Relaxed);
    HOOK_READ.store(read, Relaxed);
    HOOK_LINE.store(line, Relaxed);
    HOOK_T0.store(clock_ns(), Relaxed);
}

/// The ladder thread's episode closes (the read is back under the line): a victim canceled when R6 ran in it.
#[cold]
pub fn hook_close() {
    let t0 = HOOK_T0.swap(0, Relaxed);
    let ran = take_ran();
    if t0 == 0 {
        return;
    }
    let end = clock_ns();
    let outcome = if ran.mask & (1 << Rung::Cancel as u16) != 0 { Outcome::VictimCanceled } else { Outcome::Covered };
    ended(Site::Hook, end.saturating_sub(t0), outcome == Outcome::VictimCanceled);
    record(&Episode {
        seq: 0,
        start_ns: t0,
        end_ns: end,
        site: Site::Hook,
        need: HOOK_NEED.load(Relaxed),
        read: HOOK_READ.load(Relaxed),
        line: HOOK_LINE.load(Relaxed),
        ran: ran.mask,
        relief: ran.relief,
        outcome,
    });
}

// ---------------------------------------------------------------------------------------------------------------------
// The episode ring.
// ---------------------------------------------------------------------------------------------------------------------

/// The episodes kept: the last `RING`.
pub const RING: usize = 256;

/// One episode as recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Episode {
    /// Its number since the server started (from 1).
    pub seq: u64,
    /// Its start and end on [`clock_ns`].
    pub start_ns: u64,
    pub end_ns: u64,
    pub site: Site,
    /// The bytes asked (a gate) or the need past the line at the start (the hook).
    pub need: u64,
    /// The ladder's read and line at the start (0: none; the read needs the ladder on).
    pub read: u64,
    pub line: u64,
    /// The rungs that ran (a bit per `Rung`) and each one's measured relief, bytes.
    pub ran: u16,
    pub relief: [u64; RUNGS],
    pub outcome: Outcome,
}

/// start, end, need, read, line, site | outcome << 8 | ran << 16, then the reliefs.
const WORDS: usize = 6 + RUNGS;

struct Slot {
    /// 2n - 1 while episode n is written, 2n once written; 0 empty.
    seq: AtomicU64,
    w: [AtomicU64; WORDS],
}

static SLOTS: [Slot; RING] = [const { Slot { seq: AtomicU64::new(0), w: [const { AtomicU64::new(0) }; WORDS] } }; RING];
static NEXT: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Episodes recorded since start, and those dropped (their slot was being written by another: a ring lapped).
pub fn ring_counts() -> (u64, u64) {
    (NEXT.load(Relaxed), DROPPED.load(Relaxed))
}

fn record(e: &Episode) {
    let n = NEXT.fetch_add(1, Relaxed) + 1;
    let s = &SLOTS[((n - 1) % RING as u64) as usize];
    let cur = s.seq.load(Relaxed);
    if cur & 1 == 1 || s.seq.compare_exchange(cur, 2 * n - 1, Acquire, Relaxed).is_err() {
        DROPPED.fetch_add(1, Relaxed);
        return;
    }
    fence(Release);
    let meta = e.site as u64 | (e.outcome as u64) << 8 | (e.ran as u64) << 16;
    for (i, x) in [e.start_ns, e.end_ns, e.need, e.read, e.line, meta].into_iter().chain(e.relief).enumerate() {
        s.w[i].store(x, Relaxed);
    }
    s.seq.store(2 * n, Release);
}

/// Each episode in the ring, oldest first; one being written is skipped.
pub fn for_each_episode(f: &mut dyn FnMut(&Episode)) {
    let next = NEXT.load(Acquire);
    for n in next.saturating_sub(RING as u64) + 1..=next {
        let s = &SLOTS[((n - 1) % RING as u64) as usize];
        let seq = s.seq.load(Acquire);
        if seq != 2 * n {
            continue;
        }
        let w: [u64; WORDS] = core::array::from_fn(|i| s.w[i].load(Relaxed));
        fence(Acquire);
        if s.seq.load(Relaxed) != seq {
            continue;
        }
        let meta = w[5];
        f(&Episode {
            seq: n,
            start_ns: w[0],
            end_ns: w[1],
            need: w[2],
            read: w[3],
            line: w[4],
            site: Site::from_u8(meta as u8),
            outcome: Outcome::from_u8((meta >> 8) as u8),
            ran: (meta >> 16) as u16,
            relief: core::array::from_fn(|i| w[6 + i]),
        });
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Revoke (REVOKESLA's timeline, `mcx::revoke`).
// ---------------------------------------------------------------------------------------------------------------------

/// The histograms' buckets: bucket 0 holds 0 ms, bucket k >= 1 holds [2^(k-1), 2^k) ms; the last one is open.
pub const BUCKETS: usize = 24;

static GIVE_BACKS: AtomicU64 = AtomicU64::new(0);
static GIVEN_BYTES: AtomicU64 = AtomicU64::new(0);
static SETTLED: AtomicU64 = AtomicU64::new(0);
static NO_CUT: AtomicU64 = AtomicU64::new(0);
static NOTHING_GIVEN: AtomicU64 = AtomicU64::new(0);
static CLOCK1: [AtomicU64; BUCKETS] = [const { AtomicU64::new(0) }; BUCKETS];
static CLOCK2: [AtomicU64; BUCKETS] = [const { AtomicU64::new(0) }; BUCKETS];

/// The bucket of `ms`.
pub const fn bucket(ms: u64) -> usize {
    if ms == 0 {
        return 0;
    }
    let k = 64 - ms.leading_zeros() as usize;
    if k < BUCKETS {
        k
    } else {
        BUCKETS - 1
    }
}

/// The most ms bucket `k` holds (the open last bucket: its least).
pub const fn bucket_ms(k: usize) -> u64 {
    match k {
        0 => 0,
        k if k >= BUCKETS - 1 => 1 << (BUCKETS - 2),
        k => (1 << k) - 1,
    }
}

/// The bucket bound under which `permille` of the samples of `h` fall (`None`: no samples).
pub fn quantile_ms(h: &[u64; BUCKETS], permille: u64) -> Option<u64> {
    let total: u64 = h.iter().sum();
    if total == 0 {
        return None;
    }
    let want = (total * permille).div_ceil(1000).max(1);
    let mut seen = 0;
    for (k, &n) in h.iter().enumerate() {
        seen += n;
        if seen >= want {
            return Some(bucket_ms(k));
        }
    }
    Some(bucket_ms(BUCKETS - 1))
}

/// A participant gave `bytes` back to the kernel for the ask it serviced (`revoke::sla_given`).
#[cold]
pub fn note_given(bytes: u64) {
    GIVE_BACKS.fetch_add(1, Relaxed);
    GIVEN_BYTES.fetch_add(bytes, Relaxed);
}

/// An ask settled (its REVOKESLA line): ms from the ask to its last cut (stopped growing; `None`: none cut) and to its
/// last give-back (`None`: nothing given).
#[cold]
pub fn note_settled(cut_last_ms: Option<u32>, given_ms: Option<u32>) {
    SETTLED.fetch_add(1, Relaxed);
    match cut_last_ms {
        Some(ms) => CLOCK1[bucket(ms as u64)].fetch_add(1, Relaxed),
        None => NO_CUT.fetch_add(1, Relaxed),
    };
    match given_ms {
        Some(ms) => CLOCK2[bucket(ms as u64)].fetch_add(1, Relaxed),
        None => NOTHING_GIVEN.fetch_add(1, Relaxed),
    };
}

/// Revoke, read now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RevokeStats {
    pub asks: u64,
    pub asked_bytes: u64,
    pub give_backs: u64,
    pub given_bytes: u64,
    /// Asks settled (by the statement's next ask or its end): the histograms' samples and their misses.
    pub settled: u64,
    pub no_cut: u64,
    pub nothing_given: u64,
    pub clock1: [u64; BUCKETS],
    pub clock2: [u64; BUCKETS],
}

pub fn revoke() -> RevokeStats {
    RevokeStats {
        asks: crate::revoke::ASKS.load(Relaxed),
        asked_bytes: crate::revoke::ASKED_BYTES.load(Relaxed),
        give_backs: GIVE_BACKS.load(Relaxed),
        given_bytes: GIVEN_BYTES.load(Relaxed),
        settled: SETTLED.load(Relaxed),
        no_cut: NO_CUT.load(Relaxed),
        nothing_given: NOTHING_GIVEN.load(Relaxed),
        clock1: core::array::from_fn(|k| CLOCK1[k].load(Relaxed)),
        clock2: core::array::from_fn(|k| CLOCK2[k].load(Relaxed)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The counters and the ring are process state: one test holds them, so the deltas read are its own.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn last(site: Site) -> Option<Episode> {
        let mut out = None;
        for_each_episode(&mut |e| {
            if e.site == site {
                out = Some(*e);
            }
        });
        out
    }

    #[test]
    fn a_wait_counts_at_its_site_and_lands_in_the_ring_with_the_rungs_its_thread_ran() {
        let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let before = site(Site::Draw);
        let ns_before = rung_ns();
        {
            let w = Wait::begin(Site::Draw, 4 << 20);
            note_rung(Rung::EvictElastic, 3 << 20, 1_000);
            note_rung(Rung::Revoke, 1 << 20, 2_000);
            w.last_try();
            w.refused();
        }
        let after = site(Site::Draw);
        assert_eq!(after.misses, before.misses + 1);
        assert_eq!(after.waits, before.waits + 1);
        assert_eq!(after.refusals, before.refusals + 1);
        assert_eq!(after.last_tries, before.last_tries + 1);
        assert!(after.wait_max_ns >= after.wait_ns - before.wait_ns);
        assert_eq!(rung_ns()[Rung::EvictElastic as usize], ns_before[Rung::EvictElastic as usize] + 1_000);
        let e = last(Site::Draw).expect("the episode is in the ring");
        assert_eq!((e.need, e.outcome), (4 << 20, Outcome::Refused));
        assert_eq!(e.ran, 1 << Rung::EvictElastic as u16 | 1 << Rung::Revoke as u16);
        assert_eq!((e.relief[Rung::EvictElastic as usize], e.relief[Rung::Revoke as usize]), (3 << 20, 1 << 20));
        assert!(e.end_ns >= e.start_ns);
    }

    #[test]
    fn a_covered_wait_is_no_refusal_and_rungs_from_before_it_are_not_its_own() {
        let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        note_rung(Rung::AllocReturn, 9, 1);
        let before = site(Site::Admit);
        {
            let w = Wait::begin(Site::Admit, 1 << 20);
            w.halved();
        }
        let after = site(Site::Admit);
        assert_eq!((after.waits, after.refusals), (before.waits + 1, before.refusals));
        let e = last(Site::Admit).expect("the episode is in the ring");
        assert_eq!((e.outcome, e.ran), (Outcome::Halved, 0));
        // a refusal at once is a miss with no wait
        note_refused(Site::Admit);
        let now = site(Site::Admit);
        assert_eq!((now.misses, now.waits, now.refusals), (after.misses + 1, after.waits, after.refusals + 1));
    }

    #[test]
    fn the_hooks_episode_keeps_its_first_passes_rungs_and_ends_a_victim_canceled_when_r6_ran() {
        let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let before = site(Site::Hook);
        let _ = take_ran();
        note_rung(Rung::EvictProtected, 5, 10);
        hook_open(7 << 20, 100 << 20, 93 << 20);
        hook_open(1, 1, 1);
        note_rung(Rung::Cancel, 64 << 20, 10);
        hook_close();
        hook_close();
        let after = site(Site::Hook);
        assert_eq!((after.misses, after.refusals), (before.misses + 1, before.refusals + 1));
        let e = last(Site::Hook).expect("the hook's episode is in the ring");
        assert_eq!((e.need, e.read, e.line, e.outcome), (7 << 20, 100 << 20, 93 << 20, Outcome::VictimCanceled));
        assert_eq!(e.ran, 1 << Rung::EvictProtected as u16 | 1 << Rung::Cancel as u16);
    }

    #[test]
    fn the_ring_keeps_the_last_episodes_in_order() {
        let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        for i in 0..(RING as u64 + 10) {
            drop(Wait::begin(Site::Sample, i));
        }
        let (mut n, mut prev, mut samples, mut first, mut last) = (0, 0, 0, None, None);
        for_each_episode(&mut |e| {
            assert!(e.seq > prev, "oldest first");
            prev = e.seq;
            n += 1;
            if e.site == Site::Sample {
                samples += 1;
                first = first.or(Some(e.need));
                last = Some(e.need);
            }
        });
        assert_eq!((n, samples), (RING, RING));
        assert_eq!((first, last), (Some(10), Some(RING as u64 + 9)));
    }

    #[test]
    fn the_revoke_clocks_bucket_by_log2_ms_and_read_their_quantiles() {
        let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!((bucket(0), bucket(1), bucket(2), bucket(3), bucket(4), bucket(1023), bucket(1024)), (0, 1, 2, 2, 3, 10, 11));
        assert_eq!(bucket(u64::MAX), BUCKETS - 1);
        assert_eq!((bucket_ms(0), bucket_ms(1), bucket_ms(2), bucket_ms(11)), (0, 1, 3, 2047));
        let mut h = [0u64; BUCKETS];
        h[bucket(5)] = 98;
        h[bucket(900)] = 2;
        assert_eq!(quantile_ms(&h, 500), Some(7));
        assert_eq!(quantile_ms(&h, 990), Some(1023));
        assert_eq!(quantile_ms(&[0; BUCKETS], 500), None);
        let before = revoke();
        note_settled(Some(5), None);
        note_settled(None, Some(40));
        note_given(1 << 20);
        let after = revoke();
        assert_eq!(after.settled, before.settled + 2);
        assert_eq!((after.no_cut, after.nothing_given), (before.no_cut + 1, before.nothing_given + 1));
        assert_eq!(after.clock1[bucket(5)], before.clock1[bucket(5)] + 1);
        assert_eq!(after.clock2[bucket(40)], before.clock2[bucket(40)] + 1);
        assert_eq!((after.give_backs, after.given_bytes), (before.give_backs + 1, before.given_bytes + (1 << 20)));
    }

    #[test]
    fn cancels_count_by_path() {
        let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let before = cancels();
        note_cancel(Cancel::LadderR6);
        note_cancel(Cancel::Terminate);
        let after = cancels();
        assert_eq!(after[Cancel::Watchdog as usize], before[Cancel::Watchdog as usize]);
        assert_eq!(after[Cancel::LadderR6 as usize], before[Cancel::LadderR6 as usize] + 1);
        assert_eq!(after[Cancel::Terminate as usize], before[Cancel::Terminate as usize] + 1);
    }
}

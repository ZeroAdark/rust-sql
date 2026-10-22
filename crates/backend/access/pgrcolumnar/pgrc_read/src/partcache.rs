//! D3003: the process-wide budget of pgrc's part cache CONTENTS: the segment, unwrapped, and staged images every
//! `OpenPart` keeps (`OpenPart::resident`) plus the dictionary payload images (`BlockLazyPayload`, part or handle
//! grain). The catalog pins every part of every live generation for the server's life, so the registry's entry budget
//! (`PartRegistry::budget`) never evicted anything, and these bytes grew with every column and predicate a long-lived
//! server saw (cbgen's corpus: 9.3 GB of dictionary images + 1.8 GB of segments past shared_buffers and the pool).
//!
//! - **Counted**: every charge / debit of a part's segment cache and every dictionary block fault lands in one process
//!   atomic ([`bytes`]); a dropped part or payload debits what it still held. An image a holder still shares stays
//!   cached and counted until the holder lets it go (D3113: the census is the bytes alive, never fewer).
//! - **Budget**: [`set_budget`] (the `pgrust.pgrc_part_cache_mb` option, Gravity's runner arms it; `u64::MAX` =
//!   unbounded, the state before anything arms it).
//! - **Evicted by contents, never by entry**: `PartRegistry::trim_contents` drops, from the parts no statement uses
//!   ([`crate::openpart::PartUse`]), first the streams their last statement did not open, then whole parts' cache maps
//!   in [`stamp`] order (older statements' parts first; inside one statement its latest first touches first, D3109) —
//!   cache-only, a later read re-faults CRC-checked
//!   exactly as a first read (the `evict_extent_images` law); a holder's `SegBuf` / `Arc<BlockLazyPayload>` keeps its
//!   bytes valid. The evicted hook ([`set_evicted_hook`]) lets the owner of longer-lived dictionary handles (Gravity's
//!   per-generation part lists) drop them so the images actually free.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

static BYTES: AtomicU64 = AtomicU64::new(0);
static BUDGET: AtomicU64 = AtomicU64::new(u64::MAX);
/// D3109: the first-use clock of the current epoch (restarted by [`statement_boundary`]).
static CLOCK: AtomicU64 = AtomicU64::new(0);
/// D3109: the statement epoch ([`statement_boundary`] bumps it), the high bits of a use [`stamp`].
static EPOCH: AtomicU64 = AtomicU64::new(1);
/// D3109: a stamp's low bits, the epoch's first-use clock inverted. The first 2^20 first uses of an epoch keep their
/// order (the later ones tie, ordered by part key); 2^44 epochs.
const SEQ_BITS: u32 = 20;
const SEQ_MASK: u64 = (1 << SEQ_BITS) - 1;
static TRIMS: AtomicU64 = AtomicU64::new(0);
static EVICTED_PARTS: AtomicU64 = AtomicU64::new(0);
static EVICTED_BYTES: AtomicU64 = AtomicU64::new(0);
static HOOK: AtomicUsize = AtomicUsize::new(0);
/// D3113 (the line's class): the bytes of images a part let go of while another holder kept them, still counted in
/// [`bytes`] until that holder drops them ([`owe`], [`repay`]).
static OWED: AtomicU64 = AtomicU64::new(0);
/// The images that became owed, counted ([`owe`]), and that count when the evicted hook last ran ([`run_evicted_hook`]):
/// the images owed by then whose holders the hook's owner had dropped are freed, so only a later one is the hook's.
static OWES: AtomicU64 = AtomicU64::new(0);
static OWES_AT_HOOK: AtomicU64 = AtomicU64::new(0);
/// D4504: the faces hook ([`set_faces_hook`]).
static FACES_HOOK: AtomicUsize = AtomicUsize::new(0);
/// A trim that could not reach its target (the parts in use hold more than it): the level the next trim waits for, so a
/// statement whose own working set is past the budget does not walk every part at each part it touches (0 = none).
static RETRY_AT: AtomicU64 = AtomicU64::new(0);

/// Memory step 6a: the memory broker's coarse clock in seconds, set every second while the broker runs
/// (never 0 then); 0 = the broker is off and nothing below records re-use (one relaxed load per part use).
static NOW_SEC: AtomicU32 = AtomicU32::new(0);
/// Memory step 6a: the broker second a content eviction last freed a part re-used inside [`REUSE_WINDOW_S`] (0 = never).
static REUSED_EVICT_SEC: AtomicU32 = AtomicU32::new(0);
/// Memory step 6a: the next statement re-use token ([`next_stmt_token`]; 0 = unknown).
static STMT_TOKENS: AtomicU64 = AtomicU64::new(1);
pub use mempolicy::cache::REUSE_WINDOW_S;

/// Memory step 6a: the broker's clock (0 stops the re-use counters).
pub fn set_now_sec(s: u32) {
    NOW_SEC.store(s, Ordering::Relaxed);
}

#[inline]
pub fn now_sec() -> u32 {
    NOW_SEC.load(Ordering::Relaxed)
}

/// Memory step 6a: the broker second a re-used part's contents were last evicted (0 = never).
pub fn reused_evicted_at() -> u32 {
    REUSED_EVICT_SEC.load(Ordering::Relaxed)
}

pub(crate) fn note_reused_evict(now: u32) {
    REUSED_EVICT_SEC.store(now, Ordering::Relaxed);
}

/// Memory step 6a: a statement execution's re-use token (`OpenPart::use_part_in`): uses under one token are one statement,
/// whatever thread or epoch makes them.
pub fn next_stmt_token() -> u64 {
    STMT_TOKENS.fetch_add(1, Ordering::Relaxed)
}

/// Part-cache bytes resident in the process (segments + unwrapped + staged + dictionary images).
pub fn bytes() -> u64 {
    BYTES.load(Ordering::Relaxed)
}

/// The budget in bytes (`u64::MAX` = unbounded).
pub fn budget() -> u64 {
    BUDGET.load(Ordering::Relaxed)
}

pub fn set_budget(b: u64) {
    BUDGET.store(b, Ordering::Relaxed);
}

/// The cache is past its budget and past the level a failed trim set, and (ladder C1b) past its cushion while the floor
/// holds it ([`own_cushion`]) (a few relaxed loads; the floor's hook only past the budget).
#[inline]
pub fn over_budget() -> bool {
    let have = bytes();
    mempolicy::cache::part_cache_over(have, budget(), RETRY_AT.load(Ordering::Relaxed)) && own_cushion().is_none_or(|c| have > c)
}

/// The level a trim brings the cache down to (`mempolicy::cache::part_cache_trim_target_by`: a quarter under the budget,
/// or one `TRIM_STEP` under while the memory ladder sizes the cache).
pub fn trim_target() -> u64 {
    mempolicy::cache::part_cache_trim_target_by(budget(), TRIM_STEP.load(Ordering::Relaxed))
}

/// The level the cache's own trims (`PartRegistry::trim_if_over`) bring it down to: with no step, [`trim_target`];
/// ladder C1b, with a step, the cushion while the floor holds it ([`own_cushion`]), else at most one step under the
/// bytes it holds, or with cache_hold_shed = 0 the step-under target (`mempolicy::cache::part_cache_own_target`). The
/// ladder's takes and the idle / pressure releases trim to their own levels.
pub fn own_trim_target() -> u64 {
    match TRIM_STEP.load(Ordering::Relaxed) {
        0 => trim_target(),
        step => mempolicy::cache::part_cache_own_target(budget(), bytes(), step, own_cushion(), shed_on()),
    }
}

/// The level an own trim of a cache holding `have` stops at, asked for `level`: `level`; ladder C1b, with a step and
/// the shed on (cache_hold_shed), at most one step under `have` (`mempolicy::cache::part_cache_own_level`).
pub fn own_level(have: u64, level: u64) -> u64 {
    match TRIM_STEP.load(Ordering::Relaxed) {
        0 => level,
        step => mempolicy::cache::part_cache_own_level(have, level, step, shed_on()),
    }
}

fn shed_on() -> bool {
    mempolicy::consts::get(mempolicy::consts::K::CacheHoldShed) != 0
}

/// Ladder C1b: the step a trim stops under the budget (0 = a quarter), set at each statement start by the owner.
static TRIM_STEP: AtomicU64 = AtomicU64::new(0);

pub fn set_trim_step(step: u64) {
    TRIM_STEP.store(step, Ordering::Relaxed);
}

/// Ladder C1b: the owner's reader of the memory ladder's last sample for the cache's hold: the bytes its own trim
/// cannot drop (in use, and the images its part lists hold) and the hold's reach (the budget plus what a raise could
/// give it then); unset, none.
static HOLD_HOOK: AtomicUsize = AtomicUsize::new(0);

pub fn set_hold_hook(f: fn() -> (u64, u64)) {
    HOLD_HOOK.store(f as usize, Ordering::Release);
}

fn hold() -> (u64, u64) {
    let p = HOLD_HOOK.load(Ordering::Acquire);
    if p == 0 {
        return (0, 0);
    }
    // SAFETY: only set_hold_hook stores here, always from a fn() -> (u64, u64).
    let f: fn() -> (u64, u64) = unsafe { core::mem::transmute::<usize, fn() -> (u64, u64)>(p) };
    f()
}

/// Ladder C1b: while the ladder sizes the cache with a step and the bytes its own trim cannot drop are at or past the
/// budget, the level its own trims keep it at: that floor plus one step, or the hold's reach past TOP, whichever is
/// more (`mempolicy::cache::part_cache_own_cushion`). `None`: the budget bounds them.
#[inline]
pub fn own_cushion() -> Option<u64> {
    match TRIM_STEP.load(Ordering::Relaxed) {
        0 => None,
        step => {
            let (floor, reach) = hold();
            mempolicy::cache::part_cache_own_cushion(budget(), floor, step, reach)
        }
    }
}

/// LOAD GATE (LADDER-DESIGN section 3): what a load into the cache is. A demand load is a scan's read it needs now; an
/// ahead load is a read issued before its need (the prefetch runs, a batch of listed sections), which never waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Load {
    Demand,
    Ahead,
}

/// LOAD GATE: the owner's answer to a load past the budget ([`set_load_hook`]: Gravity's runner wires the process part
/// registry's `PartRegistry::load_room` and the wait for other statements' pins). The one place the memory ladder's
/// `climb()` replaces.
static LOAD_HOOK: AtomicUsize = AtomicUsize::new(0);
/// LOAD GATE counters: the loads that asked past the budget, the bytes their trims evicted, the waits, the waits' ms, and
/// the loads that went on past the budget (only pinned bytes were left).
static LOAD_GATES: AtomicU64 = AtomicU64::new(0);
static LOAD_TRIMMED: AtomicU64 = AtomicU64::new(0);
static LOAD_WAITS: AtomicU64 = AtomicU64::new(0);
static LOAD_WAIT_MS: AtomicU64 = AtomicU64::new(0);
static LOAD_OVER: AtomicU64 = AtomicU64::new(0);
/// LOAD GATE: the waits that ran their whole `bytes_wait_ms` with the pins still in place.
static LOAD_WAIT_TIMEOUTS: AtomicU64 = AtomicU64::new(0);
/// LOAD GATE: the statement epoch in which a load's wait for other statements' pins ran out (0 = none): no load waits
/// again until the next statement boundary, so a statement whose loads the pins keep out pays the wait once.
static LOAD_WAIT_SPENT: AtomicU64 = AtomicU64::new(0);

pub fn set_load_hook(f: fn(u64, Load)) {
    LOAD_HOOK.store(f as usize, Ordering::Release);
}

/// LOAD GATE: the ask a load makes before it brings `n` bytes into the cache, at the one place each load path retains an
/// image (never under a part's state lock). Inside the budget: two relaxed loads. Past it: the owner's hook makes room
/// ([`set_load_hook`]); then the load goes on, past the budget when only pinned bytes are left (a running scan's read is
/// never refused). `mempolicy`'s `load_gate` knob (0) turns it off.
#[inline]
pub fn load_gate(n: u64, kind: Load) {
    if mempolicy::cache::part_cache_load_fits(bytes(), n, budget()) {
        return;
    }
    load_past_budget(n, kind);
}

#[cold]
fn load_past_budget(n: u64, kind: Load) {
    if mempolicy::consts::get(mempolicy::consts::K::LoadGate) == 0 {
        return;
    }
    LOAD_GATES.fetch_add(1, Ordering::Relaxed);
    let p = LOAD_HOOK.load(Ordering::Acquire);
    if p != 0 {
        // SAFETY: only set_load_hook stores here, always from a fn(u64, Load).
        let f: fn(u64, Load) = unsafe { core::mem::transmute::<usize, fn(u64, Load)>(p) };
        f(n, kind);
    }
    if !mempolicy::cache::part_cache_load_fits(bytes(), n, budget()) {
        LOAD_OVER.fetch_add(1, Ordering::Relaxed);
    }
}

/// LOAD GATE: a trim the gate ran (`PartRegistry::load_room`) is due at `level` (the bytes after the load): not while a
/// failed trim's retry level stands above it ([`note_result`]), so a statement whose own parts fill the cache does not
/// walk every part at each load.
pub(crate) fn load_trim_due(level: u64) -> bool {
    let at = RETRY_AT.load(Ordering::Relaxed);
    at == 0 || level >= at
}

pub(crate) fn note_load_trim(freed: u64) {
    LOAD_TRIMMED.fetch_add(freed, Ordering::Relaxed);
}

/// LOAD GATE: a load's wait for other statements' pins ended after `ms`; `ran_out` = it ended on its budget with the pins
/// still in place (no load waits again this statement epoch).
pub fn note_load_wait(ms: u64, ran_out: bool) {
    LOAD_WAITS.fetch_add(1, Ordering::Relaxed);
    LOAD_WAIT_MS.fetch_add(ms, Ordering::Relaxed);
    if ran_out {
        LOAD_WAIT_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
        LOAD_WAIT_SPENT.store(epoch(), Ordering::Relaxed);
    }
}

/// LOAD GATE: the waits that ran out (`bytes_wait_ms` passed with the pins in place).
pub fn load_wait_timeouts() -> u64 {
    LOAD_WAIT_TIMEOUTS.load(Ordering::Relaxed)
}

/// LOAD GATE, ladder C1b: the loads past the budget the floor held (trimmed to its cushion only, no wait).
static LOAD_HELD: AtomicU64 = AtomicU64::new(0);

pub(crate) fn note_load_held() {
    LOAD_HELD.fetch_add(1, Ordering::Relaxed);
}

pub fn load_held() -> u64 {
    LOAD_HELD.load(Ordering::Relaxed)
}

/// LOAD GATE: a load's wait ran out this statement epoch.
pub fn load_wait_spent() -> bool {
    LOAD_WAIT_SPENT.load(Ordering::Relaxed) == epoch()
}

/// LOAD GATE counters: (loads that asked past the budget, bytes their trims evicted, waits, waits' ms, loads that went on
/// past the budget).
pub fn load_counters() -> (u64, u64, u64, u64, u64) {
    (
        LOAD_GATES.load(Ordering::Relaxed),
        LOAD_TRIMMED.load(Ordering::Relaxed),
        LOAD_WAITS.load(Ordering::Relaxed),
        LOAD_WAIT_MS.load(Ordering::Relaxed),
        LOAD_OVER.load(Ordering::Relaxed),
    )
}

/// A statement boundary: a trim that failed may succeed now (the finished statements' parts are unused), and the parts
/// used from here on stamp a newer epoch than every part used before it (D3109, [`stamp`]).
pub fn statement_boundary() {
    RETRY_AT.store(0, Ordering::Relaxed);
    EPOCH.fetch_add(1, Ordering::Relaxed);
    CLOCK.store(0, Ordering::Relaxed);
}

static DROPPED_PARTS: AtomicU64 = AtomicU64::new(0);
static DROPPED_BYTES: AtomicU64 = AtomicU64::new(0);

/// D4570: `parts`' files left their relation (DROP TABLE / DATABASE, TRUNCATE or a rewrite's old storage, a generation the
/// catalog retired). The registry's entries go by key ([`crate::PartRegistry::invalidate`]), but a holder outside it (a
/// part list, a parked source's kept cursor) kept each part, its cached contents counted, until the budget's trim, which
/// walks only the registry and the catalog, never reached it. Here each part's contents and faces go at once unless a
/// running statement uses it (`OpenPart::evict_dropped`); such a part gives them back when its last running use ends
/// (`PartUse`'s drop, or its move to a parked executor). The faces hook's owner drops its stale holds. Returns the bytes
/// freed now.
pub fn drop_parts<'a>(parts: impl IntoIterator<Item = &'a std::sync::Arc<crate::OpenPart>>) -> u64 {
    let (mut n, mut freed, mut faces) = (0u64, 0u64, 0u64);
    // bounded("other: the parts of the relations a DROP, TRUNCATE, rewrite or retire just took out of the catalog, each once")
    for p in parts {
        if !p.mark_dropped() {
            continue;
        }
        n += 1;
        let (b, f) = p.evict_dropped();
        freed += b;
        faces += f;
    }
    note_dropped(n, freed);
    if faces > 0 {
        run_faces_hook();
    }
    freed
}

/// D4570: `parts` more parts dropped, `bytes` given back by their drops or by their last running uses' ends.
pub(crate) fn note_dropped(parts: u64, bytes: u64) {
    DROPPED_PARTS.fetch_add(parts, Ordering::Relaxed);
    DROPPED_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

/// D4570: `(parts dropped, bytes their drops gave back)` (tests; the census).
pub fn dropped_counters() -> (u64, u64) {
    (DROPPED_PARTS.load(Ordering::Relaxed), DROPPED_BYTES.load(Ordering::Relaxed))
}

/// D4315: `n` cached bytes became evictable while their part stays in use (a scan read past them): a trim that failed
/// for parts in use waits for evictable bytes as for new ones, so they bring its retry level down.
pub(crate) fn note_passed(n: u64) {
    if RETRY_AT.load(Ordering::Relaxed) != 0 {
        let _ = RETRY_AT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |r| (r != 0).then(|| r.saturating_sub(n).max(1)));
    }
    if over_budget() {
        let p = TRIM_HOOK.load(Ordering::Acquire);
        if p != 0 {
            // SAFETY: only set_trim_hook stores here, always from a fn().
            let f: fn() = unsafe { core::mem::transmute::<usize, fn()>(p) };
            f();
        }
    }
}

/// D4315: the trim a pass past the budget runs (its owner's registry: Gravity's process registry), outside every pgrc
/// lock: passed images go as they pass, not at the next part touch. Unset: they wait for the next trim.
static TRIM_HOOK: AtomicUsize = AtomicUsize::new(0);

pub fn set_trim_hook(f: fn()) {
    TRIM_HOOK.store(f as usize, Ordering::Release);
}

/// After a trim to `target` left `have` bytes: past the target, the next trim waits for an eighth of the budget more.
pub(crate) fn note_result(have: u64, target: u64) {
    RETRY_AT.store(mempolicy::cache::part_cache_retry_at(have, target, budget()), Ordering::Relaxed);
}

/// D3063 (2a): the account the cache's bytes are reported to as they are faulted (`true`) and freed (`false`): the
/// owner wires the running statement's memory ledger and the memory governor here (Gravity's runner, `mcx::governor::
/// charge_cache` / `release_cache`), so a statement's part cache counts against its limit and the governor's bar.
static ACCOUNT: AtomicUsize = AtomicUsize::new(0);

/// D4795 (2a): the statement a holder's census bytes were charged to (the governor's record, kept by the holder: 16 bytes),
/// so the free takes them back from that statement only while it runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Charged {
    pub slot: u32,
    pub epoch: u32,
    pub bytes: u64,
}

pub fn set_account_hook(f: fn(u64, bool, &mut Charged)) {
    ACCOUNT.store(f as usize, Ordering::Release);
}

#[inline]
fn account(n: u64, charged: bool, rec: &mut Charged) {
    let p = ACCOUNT.load(Ordering::Acquire);
    if p != 0 {
        // SAFETY: only set_account_hook stores here, always from a fn(u64, bool, &mut Charged).
        let f: fn(u64, bool, &mut Charged) = unsafe { core::mem::transmute::<usize, fn(u64, bool, &mut Charged)>(p) };
        f(n, charged, rec);
    }
}

/// G1-A: the part cache's account in the memory ledger. Its bytes move there from the faulting thread's account as they are
/// counted here, and back to the evicting thread's just before that thread frees them, so this account is the census and
/// a backend's account nets to zero over a part's contents. C8 move 1: the holders' charges move them ([`holder_charge`]).
pub(crate) const LEDGER: memledger::Acct = memledger::Acct::of(memledger::Consumer::PartCache);

/// C8: the owner of every byte the part cache holds.
pub(crate) const OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache));

/// D4552 (a): the part cache's account as a home: a registry-opened part's state (its maps' nodes, its stream directory,
/// its part-grain dictionary payloads) is allocated and freed there, whichever thread changes or drops it.
pub(crate) fn home() -> memledger::Home {
    memledger::Home::of(OWNER.acct())
}

/// D4084 (C8 move 1): the charge of `n` bytes a holder of the cache's contents just allocated on this thread: on the
/// cache's account from now, back on the freeing thread's at its drop.
pub(crate) fn holder_charge(n: u64) -> memledger::Charge {
    memledger::Charge::open(OWNER, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, n)
}

/// D4802: the bytes into the census since start ([`count`]): with the census level, the bytes that left it (a
/// cross-check of the registry's real evictions, `PartRegistry::real_evictions`).
static LOADED: AtomicU64 = AtomicU64::new(0);

/// D4802: the bytes into the census since start.
pub fn loaded_bytes() -> u64 {
    LOADED.load(Ordering::Relaxed)
}

/// `n` bytes into the census whose memory a holder's charge has on the cache's account already.
#[inline]
pub(crate) fn count(n: u64, rec: &mut Charged) {
    BYTES.fetch_add(n, Ordering::Relaxed);
    LOADED.fetch_add(n, Ordering::Relaxed);
    account(n, true, rec);
}

/// `n` bytes out of the census (saturating: a debit never wraps it); the bytes it held of them.
#[inline]
pub(crate) fn uncount(n: u64, rec: &mut Charged) -> u64 {
    let cur = match BYTES.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| Some(c.saturating_sub(n))) {
        Ok(c) | Err(c) => c,
    };
    account(n.min(cur), false, rec);
    n.min(cur)
}

/// D3113: the bytes still counted for images their parts let go of while another holder kept them.
pub fn owed() -> u64 {
    OWED.load(Ordering::Relaxed)
}

/// An image its part let go of stays counted while another holder keeps it (`openpart`'s owed chunk).
pub(crate) fn owe(n: u64) {
    OWED.fetch_add(n, Ordering::Relaxed);
    OWES.fetch_add(1, Ordering::Relaxed);
}

/// An owed image's last holder dropped it: out of the owed bytes and the census.
pub(crate) fn repay(n: u64) {
    let _ = OWED.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| Some(v.saturating_sub(n)));
    // D4795 (2a): an image outlives its part's record here, so its statement keeps it to its end (as before)
    uncount(n, &mut Charged::default());
}

/// A trim past its target may run the evicted hook for owed images: some are counted, and one became owed since the
/// hook last ran (the hook's owner may hold it; what was owed before then, the hook already could not free).
pub(crate) fn owed_since_hook() -> bool {
    owed() > 0 && OWES.load(Ordering::Relaxed) != OWES_AT_HOOK.load(Ordering::Relaxed)
}

/// Stamp a part's use into `at` (its use stamp, never wall time), the key the content trim evicts in ascending order: the
/// statement epoch in the high bits, the epoch's first-use clock INVERTED in the low bits. Only a part's FIRST use in an
/// epoch stamps; its later uses in the same epoch (another lane's cursor, a late lane, a second pass, a final read of its
/// head) keep that place, so the order is the statement's first-touch order, never its last-touch order. Older
/// statements' parts go first (least recently used across statements); inside one statement its LATEST first-touches go
/// first, so it keeps the parts it read first.
///
/// D3109: a repeat statement whose working set W is past the budget (two long text columns' LIKE + GROUP BY at W0 on a
/// 30 GiB box: W 5.5 GiB, budget 3.5 GiB) under plain LRU re-faulted all of W every run: the trim at its start kept the
/// previous run's TAIL, which the run's own first-touch trims then evicted (those parts were not in use yet) while it
/// re-faulted its head. Keeping the head instead, the run reads it first (in use from then on, never evicted under it)
/// and re-faults only W minus what the trim kept. Concurrent statements interleave epochs: a statement's parts then
/// order nearer plain LRU.
///
/// `at` is the part's own atomic (`OpenPart::cache_used`), on the crate's loom shim like the part's other counters:
/// `pgsync::atomic` (std's natively, loom's under `cfg(loom)`); the epoch and clock are process statics (std).
///
/// True at the part's first use in this epoch (memory step 6a's fallback re-use rule reads it).
#[inline]
pub(crate) fn stamp(at: &pgsync::atomic::AtomicU64) -> bool {
    let epoch = EPOCH.load(Ordering::Relaxed);
    if at.load(Ordering::Relaxed) >> SEQ_BITS != epoch {
        let seq = CLOCK.fetch_add(1, Ordering::Relaxed).min(SEQ_MASK);
        at.store((epoch << SEQ_BITS) | (SEQ_MASK - seq), Ordering::Relaxed);
        return true;
    }
    false
}

/// Broker (b): the use stamp at the cache's coldest tenth of bytes (`PartRegistry::publish_tail`, each broker plan); a use
/// of a resident part stamped at or below it is a tail hit. 0: none published.
static TAIL_STAMP: AtomicU64 = AtomicU64::new(0);

pub fn tail_stamp() -> u64 {
    TAIL_STAMP.load(Ordering::Relaxed)
}

pub(crate) fn set_tail_stamp(stamp: u64) {
    TAIL_STAMP.store(stamp, Ordering::Relaxed);
}

/// D3109: the current statement epoch (D3268: the short hook's owner runs it once per epoch).
#[inline]
pub fn epoch() -> u64 {
    EPOCH.load(Ordering::Relaxed)
}

/// Memory step 6a: the current statement epoch (the fp cache's re-use counter reads it).
pub fn statement_epoch() -> u64 {
    epoch()
}

/// D3109: the statement epoch of a use stamp ([`stamp`]).
#[inline]
pub(crate) fn epoch_of(stamp: u64) -> u64 {
    stamp >> SEQ_BITS
}

/// Register the hook a trim that evicted anything runs (outside every pgrc lock).
pub fn set_evicted_hook(f: fn()) {
    HOOK.store(f as usize, Ordering::Release);
}

/// D4504: register the hook a trim that let parts' faces go runs (outside every pgrc lock): its owner drops the
/// [`crate::FaceHold`]s that went stale ([`crate::OpenPart::evict_faces`]) with the bodies they count.
pub fn set_faces_hook(f: fn()) {
    FACES_HOOK.store(f as usize, Ordering::Release);
}

/// D4504: run the faces hook (outside every pgrc lock); `false` when none is set.
pub(crate) fn run_faces_hook() -> bool {
    let p = FACES_HOOK.load(Ordering::Acquire);
    if p == 0 {
        return false;
    }
    // SAFETY: only set_faces_hook stores here, always from a fn().
    let f: fn() = unsafe { core::mem::transmute::<usize, fn()>(p) };
    f();
    true
}

/// `dict_images`: the trim evicted dictionary images, which the hook's owner may hold handles to.
pub(crate) fn note_trim(parts: u64, bytes: u64, dict_images: bool) {
    TRIMS.fetch_add(1, Ordering::Relaxed);
    EVICTED_PARTS.fetch_add(parts, Ordering::Relaxed);
    EVICTED_BYTES.fetch_add(bytes, Ordering::Relaxed);
    if dict_images {
        run_evicted_hook();
    }
}

/// Run the evicted hook (outside every pgrc lock); `false` when none is set.
pub(crate) fn run_evicted_hook() -> bool {
    let p = HOOK.load(Ordering::Acquire);
    if p == 0 {
        return false;
    }
    // SAFETY: only set_evicted_hook stores here, always from a fn().
    let f: fn() = unsafe { core::mem::transmute::<usize, fn()>(p) };
    OWES_AT_HOOK.store(OWES.load(Ordering::Relaxed), Ordering::Relaxed);
    f();
    true
}

/// D3268: the hook a content trim runs when it ended past its target with parts in use, which it never evicts. Its owner
/// lets go the uses it keeps for no running statement (Gravity: the decode cursors its parked worker sets keep across
/// statements, each a [`crate::openpart::PartUse`] that pins every cached stream of its part) and returns whether it did;
/// the trim then passes over the parts again. Unset: the trim stops short, as before.
static SHORT_HOOK: AtomicUsize = AtomicUsize::new(0);

pub fn set_short_hook(f: fn() -> bool) {
    SHORT_HOOK.store(f as usize, Ordering::Release);
}

/// Run the short hook (outside every pgrc lock); `false` when none is set or it let nothing go.
pub(crate) fn run_short_hook() -> bool {
    let p = SHORT_HOOK.load(Ordering::Acquire);
    if p == 0 {
        return false;
    }
    // SAFETY: only set_short_hook stores here, always from a fn() -> bool.
    let f: fn() -> bool = unsafe { core::mem::transmute::<usize, fn() -> bool>(p) };
    f()
}

/// D3994 (B.1 W): the gate a content trim asks before it evicts a part whose every use is a parked executor's
/// (`OpenPart::live_users` 0, `users` not): its owner answers from the memory policy (Gravity: the memory budget's free
/// bytes). Unset (a process without Gravity, the crate's tests that set none): `false`, such parts stay as before W.
static PARKED_HOOK: AtomicUsize = AtomicUsize::new(0);

pub fn set_parked_hook(f: fn() -> bool) {
    PARKED_HOOK.store(f as usize, Ordering::Release);
}

/// Whether a trim may evict the parts only parked executors use now ([`set_parked_hook`]'s answer, read once per trim).
pub(crate) fn parked_evictable() -> bool {
    let p = PARKED_HOOK.load(Ordering::Acquire);
    if p == 0 {
        return false;
    }
    // SAFETY: only set_parked_hook stores here, always from a fn() -> bool.
    let f: fn() -> bool = unsafe { core::mem::transmute::<usize, fn() -> bool>(p) };
    f()
}

/// D3994 (B.1 W) under D3823's check (its owner's switch: Gravity's debug builds and `PGRUST_GRAVITY_RELEASE_POISON=1`): a
/// trim poisons the images no holder shares that it evicts from a part only parked executors use, so a pointer a parked
/// executor kept into one (its cursors let theirs go at the park) reads 0xDB bytes, not the column's.
static PARK_POISON: AtomicBool = AtomicBool::new(false);

pub fn set_park_poison(on: bool) {
    PARK_POISON.store(on, Ordering::Relaxed);
}

pub(crate) fn park_poison() -> bool {
    PARK_POISON.load(Ordering::Relaxed)
}

/// D3994 (B.1 W): the bytes the parked-only evictions poisoned (the poisoned check run's witness that it ran; tests).
static PARK_POISONED: AtomicU64 = AtomicU64::new(0);

pub(crate) fn note_park_poisoned(b: u64) {
    PARK_POISONED.fetch_add(b, Ordering::Relaxed);
}

pub fn park_poisoned_bytes() -> u64 {
    PARK_POISONED.load(Ordering::Relaxed)
}

/// `(trims, parts evicted, bytes evicted)` (the census; tests).
pub fn counters() -> (u64, u64, u64) {
    (TRIMS.load(Ordering::Relaxed), EVICTED_PARTS.load(Ordering::Relaxed), EVICTED_BYTES.load(Ordering::Relaxed))
}

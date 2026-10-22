//! C5 (revoke): the spill-request registry and the act of a revoke (reports/MEMORY-STEP2C-REVOKE-DESIGN-20261010.md §2,
//! the core design's `Revoke` rung, MEMORY-CORE-DESIGN-20261010.md §4). Opt-in: `pgrust.memory_revoke` (default off),
//! published here by memruntime (`set_enabled`).
//!
//! - The registry: each running statement's `StmtMem` carries a `Handle` (what it holds in breakers that can spill now,
//!   what it was asked, when and how often, whether it waits for memory itself), and each backend's Gravity interrupt
//!   byte is registered by its governor slot (`register_byte`).
//! - The decision is the policy's: `mempolicy::reclaim::revoke_asks` (who spills how much; pure). When to ask is the
//!   ladder's (C3: `decide` on Grow / Tick / Pressure), which calls `request_revoke`.
//! - The act: an ask stores the bytes and the next revoke generation on the victim's handle and sets `REVOKE_BIT` on its
//!   interrupt byte. Nothing else: it never blocks and never touches the victim's memory. The victim's kernels load that
//!   byte at every loop back-edge already, so a set byte reaches their poll (gravity_abi `ctx::poll`), which cuts the
//!   region each participant fills (gravity_driver `mem::grow::revoke_home`); each claim's start checks the generation
//!   too. The region's next room ask is refused and its breaker takes its existing spill road.
//!
//! Steady state (no ask): the kernels' byte stays 0 and the claim's check is one TLS read and two relaxed loads.
//!
//! REVOKESLA (coremem bar item 4, main 2026-10-12T22:13:47Z: the give-back SLAs): each ask's timeline is kept on the asked
//! statement's handle ([`Sla`]) and written as one `revoke-sla:` line on stderr (the server log, as cancelpoll's
//! `poll-gap:` lines) once the ask is settled, by the statement's next ask or by its end. Logging only: nothing in it
//! changes what is asked, cut or served.

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, Ordering::Relaxed};

use crate::governor::{StmtMem, SLOTS};

/// The interrupt byte's revoke bit, beside `ABORT_BIT` (1), `SERVICE_BIT` (2) and the poll-gap probe's `PROBE_BIT` (4,
/// cancelpoll) (init_small::globals and gravity_abi `ctx` hold the same value; exec's glue asserts it).
pub const REVOKE_BIT: u8 = 8;

static ON: AtomicBool = AtomicBool::new(false);

/// `pgrust.memory_revoke` (memruntime publishes it each tick): `request_revoke` asks nothing while it is off.
pub fn set_enabled(on: bool) {
    ON.store(on, Relaxed);
}

pub fn enabled() -> bool {
    ON.load(Relaxed)
}

/// Each backend's Gravity interrupt byte (init_small `abort_pending_flag`, a leaked 'static), by governor slot.
static BYTES: [AtomicPtr<AtomicU8>; SLOTS] = [const { AtomicPtr::new(core::ptr::null_mut()) }; SLOTS];

/// The next revoke generation: process-wide and never 0, so a participant's last serviced generation never equals an
/// ask of a later statement.
static NEXT_GEN: AtomicU32 = AtomicU32::new(1);

/// Asks made and their bytes, this process (the report line, tests).
pub static ASKS: AtomicU64 = AtomicU64::new(0);
pub static ASKED_BYTES: AtomicU64 = AtomicU64::new(0);

/// One statement's revoke state, on its `StmtMem` (reset at its start).
#[derive(Default, Debug)]
pub struct Handle {
    /// Bytes its participants fill in breaker regions with a spill road (`note_spillable`).
    spillable: AtomicU64,
    /// The bytes the last ask asked.
    asked: AtomicU64,
    /// The last ask's generation (0: none this statement).
    gen: AtomicU32,
    /// The registry clock (`now_ms`) at the last ask; 0 never.
    at_ms: AtomicU64,
    /// Asks this statement.
    count: AtomicU32,
    /// It waits for memory itself (C3's bounded wait): never a victim.
    waiting: AtomicBool,
    /// Ladder 1.3 (`fair_share`): its fair share at the ladder's last sample (0: none), its floor (its work_mem, set at
    /// its admission) and the generation of the ask whose cut was lifted (it came back within its share; 0: none).
    share: AtomicU64,
    floor: AtomicU64,
    lifted: AtomicU32,
    /// REVOKESLA: the last ask's timeline.
    sla: Sla,
}

/// REVOKESLA: one ask's timeline on the asked statement's handle, written as its `revoke-sla:` line when the ask is
/// settled ([`settle`]). Three times, each in ms after the ask (the broker's `request_revoke`, `Handle::at_ms`): the first
/// participant's cut ([`sla_cut`]: the region it fills refuses room from then on, so the statement stops growing there),
/// the last one's, and the last give-back ([`sla_given`]: bytes back to the kernel); beside them the breakers cut ([`Op`]),
/// who asked, and the bytes asked and given. Each word is `generation << 32 | value`, so a participant's late note of an
/// earlier ask never lands on a later one.
#[derive(Default, Debug)]
struct Sla {
    cut_first: AtomicU64,
    cut_last: AtomicU64,
    cuts: AtomicU64,
    /// The breakers the ask reached, one bit per `Op`.
    ops: AtomicU64,
    given_kb: AtomicU64,
    given_ms: AtomicU64,
    /// Who asked: a requester's governor slot + 1, 0 the pressure ladder (no requester), `SELF_ASK` the statement's own
    /// test lever.
    by: AtomicU64,
    /// The generation whose line is written (one line per ask).
    told: AtomicU32,
}

/// REVOKESLA: `Sla::by` for an ask the statement made of itself (`request_self`).
const SELF_ASK: u32 = u32::MAX;

/// REVOKESLA: the breaker an asked participant was filling (gravity_driver reads it off the table header of the region it
/// cut, `mem::grow::revoke_home`). `Roadless`: a region with no spill road, whose cut changes nothing: it goes on growing,
/// so it counts for no stop time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Op {
    HashAgg = 0,
    HashAggT1 = 1,
    HashBuild = 2,
    Sort = 3,
    Owned = 4,
    Other = 5,
    Roadless = 6,
}

const OP_NAMES: [&str; 7] = ["hashagg", "hashagg-t1", "hashbuild", "sort", "owned", "other", "roadless"];

impl Handle {
    pub(crate) fn reset(&self) {
        self.spillable.store(0, Relaxed);
        self.asked.store(0, Relaxed);
        self.gen.store(0, Relaxed);
        self.at_ms.store(0, Relaxed);
        self.count.store(0, Relaxed);
        self.waiting.store(false, Relaxed);
        self.share.store(0, Relaxed);
        self.floor.store(0, Relaxed);
        self.lifted.store(0, Relaxed);
        self.sla.told.store(0, Relaxed);
    }

    /// The bytes it holds in spillable breakers now.
    pub fn spillable(&self) -> u64 {
        self.spillable.load(Relaxed)
    }

    /// The last ask's bytes and generation (generation 0: none).
    pub fn asked(&self) -> (u64, u32) {
        (self.asked.load(Relaxed), self.gen.load(core::sync::atomic::Ordering::Acquire))
    }

    /// Asks this statement.
    pub fn count(&self) -> u32 {
        self.count.load(Relaxed)
    }
}

/// Register this backend's Gravity interrupt byte under its governor slot (`governor::register` first).
pub fn register_byte(byte: &'static AtomicU8) {
    if let Some(i) = crate::governor::my_slot_index() {
        BYTES[i].store(byte as *const AtomicU8 as *mut AtomicU8, Relaxed);
    }
}

/// The byte registered at slot `i`.
fn byte_at(i: usize) -> Option<&'static AtomicU8> {
    let p = BYTES.get(i)?.load(Relaxed);
    // SAFETY: only `register_byte` stores here, always a 'static byte (init_small leaks it, never freed)
    (!p.is_null()).then(|| unsafe { &*p })
}

/// This backend's revoke bit cleared (a statement's start and end: an ask that came after its last poll is the ended
/// statement's).
pub fn clear_own_bit() {
    if let Some(b) = crate::governor::my_slot_index().and_then(byte_at) {
        if b.load(Relaxed) & REVOKE_BIT != 0 {
            b.fetch_and(!REVOKE_BIT, Relaxed);
        }
    }
}

/// The registry's clock: milliseconds since its first read (never 0 once read).
#[cfg(any(feature = "std", test))]
fn now_ms() -> u64 {
    static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    T0.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + 1
}

/// Store an ask of `bytes` on `l` (`by`: who asks, `Sla::by`) and set its backend's revoke bit; the bytes asked (0 when it
/// has no backend byte) and the statement's earlier ask, settled by this one (REVOKESLA: its line, written by the caller
/// outside the registry's lock).
#[cfg(any(feature = "std", test))]
fn ask(l: &StmtMem, bytes: u64, now: u64, by: u32) -> (u64, Option<Settled>) {
    let Some(b) = l.slot_index().and_then(byte_at) else { return (0, None) };
    let h = &l.revoke;
    let prior = settle(l, now, false);
    let gen = loop {
        let g = NEXT_GEN.fetch_add(1, Relaxed);
        if g != 0 {
            break g;
        }
    };
    h.sla.by.store((gen as u64) << 32 | by as u64, Relaxed);
    h.asked.store(bytes, Relaxed);
    h.at_ms.store(now, Relaxed);
    h.count.fetch_add(1, Relaxed);
    // Release: a participant that reads this generation (Acquire) sees the asked bytes
    h.gen.store(gen, core::sync::atomic::Ordering::Release);
    b.fetch_or(REVOKE_BIT, core::sync::atomic::Ordering::AcqRel);
    ASKS.fetch_add(1, Relaxed);
    ASKED_BYTES.fetch_add(bytes, Relaxed);
    (bytes, prior)
}

/// The most candidates one `request_revoke` weighs (those with the most spillable bytes) and the most it asks.
const MAX_CANDS: usize = 128;
const MAX_ASKS: usize = 16;

/// C5's act hook, the one function the ladder calls (C3, membugs' interface, main 2026-10-11T06:16:58Z): ask running
/// statements to spill so `short` bytes come back. `share` = the fair share (B / holders); `requester` = the grower's
/// governor slot on Grow (never asked), `None` on Tick. Who and how much is `mempolicy::reclaim::revoke_asks`; each ask
/// sets the victim's `REVOKE_BIT` (`ask`). Returns the bytes asked: 0 when `pgrust.memory_revoke` is off, nothing is
/// short or no statement may be asked. Never blocks on a victim; one registry lock, no allocation. Ladder 1.3-1.4: under
/// `fair_share` the queries over their share first, then (its callers cancel next) the others for what is still short.
pub fn request_revoke(short: u64, share: u64, requester: Option<usize>) -> u64 {
    request_revoke_as(short, share, requester, true, true).unwrap_or(0)
}

/// Ladder 1.3-1.4: the ladder's revoke rung (R4b). As `request_revoke`, but under `fair_share` only the queries over their
/// own share are asked: a query within its share is never revoked so another can run. `request_revoke`'s callers cancel
/// next (the backstop, the projection's act): to avoid a kill they may ask the others too (`revoke_asks_fair_or_kill`).
pub fn request_revoke_within_shares(short: u64, share: u64, requester: Option<usize>) -> u64 {
    request_revoke_as(short, share, requester, false, true).unwrap_or(0)
}

/// D4812 (2): `request_revoke_within_shares` with the registry taken by try_lock at both its walks (the block gate's R4b,
/// inside an allocation: it never waits on the registry, and the walks read atomics into fixed arrays, so the lock is
/// never held across an allocation): `Err(Busy)` when it was held (no ask sent then, or none at the second walk).
pub fn try_request_revoke_within_shares(short: u64, share: u64, requester: Option<usize>) -> Result<u64, crate::governor::Busy> {
    request_revoke_as(short, share, requester, false, false)
}

/// The registry's live statements (`governor::for_each_live`), or by try_lock when `!wait`.
#[cfg(any(feature = "std", test))]
fn walk_live(wait: bool, f: &mut dyn FnMut(&StmtMem)) -> Result<(), crate::governor::Busy> {
    if wait {
        crate::governor::for_each_live(f);
        Ok(())
    } else {
        crate::governor::try_for_each_live(f)
    }
}

fn request_revoke_as(short: u64, share: u64, requester: Option<usize>, kill_avoid: bool, wait: bool) -> Result<u64, crate::governor::Busy> {
    #[cfg(any(feature = "std", test))]
    {
        if !enabled() || short == 0 {
            return Ok(0);
        }
        let now = now_ms();
        let mut cands = [mempolicy::reclaim::RevokeCand::default(); MAX_CANDS];
        let mut words = [0usize; MAX_CANDS];
        // LEDGER2 step 4: each candidate's bytes as the owner tree reads them (the shadow decision below)
        let mut tree_held = [0u64; MAX_CANDS];
        let mut n = 0;
        let mut asks = [mempolicy::reclaim::RevokeAsk::default(); MAX_ASKS];
        let mut total = 0;
        // REVOKESLA: the earlier asks these settle, written after the registry's lock
        let mut settled = [None; MAX_ASKS];
        let mut ns = 0;
        walk_live(wait, &mut |l| {
            let Some(slot) = l.slot_index() else { return };
            if requester == Some(slot) {
                return;
            }
            let h = &l.revoke;
            let c = mempolicy::reclaim::RevokeCand {
                slot,
                spillable: h.spillable.load(Relaxed),
                held: l.held(),
                progress: 0,
                revoked_ms: h.at_ms.load(Relaxed),
                revokes: h.count.load(Relaxed),
                waiting: h.waiting.load(Relaxed),
                share: h.share.load(Relaxed),
            };
            if c.spillable == 0 {
                return;
            }
            // full: the candidate holding the least spillable bytes gives way
            let at = if n < MAX_CANDS {
                n += 1;
                n - 1
            } else {
                match (0..MAX_CANDS).min_by_key(|&i| cands[i].spillable) {
                    Some(i) if cands[i].spillable < c.spillable => i,
                    _ => return,
                }
            };
            cands[at] = c;
            words[at] = l.statement_word();
            tree_held[at] = crate::ledger2::owned(Some(slot));
        })?;
        // ladder 1.3-1.4: under `fair_share` only the queries over their own share, each for bytes
        let fair = fair_on();
        let k = if fair && kill_avoid {
            mempolicy::reclaim::revoke_asks_fair_or_kill(short, share, now, &cands[..n], &mut asks)
        } else if fair {
            mempolicy::reclaim::revoke_asks_fair(short, now, &cands[..n], &mut asks)
        } else {
            mempolicy::reclaim::revoke_asks(short, share, now, &cands[..n], &mut asks)
        };
        // LEDGER2 step 4's shadow line, written once the asks are sent (D4812: act first, then log)
        let shadow = (!fair).then(|| shadow_revoke(short, share, now, &cands[..n], &tree_held[..n], &asks[..k]));
        let note = |shadow: Option<(crate::ledger2::Choice, crate::ledger2::Choice)>| {
            if let Some((today, by_tree)) = shadow {
                crate::ledger2::note(crate::ledger2::Decision::Revoke, today, by_tree, now);
            }
        };
        if k == 0 {
            note(shadow);
            return Ok(0);
        }
        walk_live(wait, &mut |l| {
            let Some(slot) = l.slot_index() else { return };
            let Some(a) = asks[..k].iter().find(|a| a.slot == slot) else { return };
            // the statement the candidate was read from, not one that started on the same backend since
            let Some(i) = cands[..n].iter().position(|c| c.slot == slot) else { return };
            if l.statement_word() == words[i] {
                let (bytes, prior) = ask(l, a.bytes, now, requester.map_or(0, |r| r as u32 + 1));
                total += bytes;
                if prior.is_some() && ns < MAX_ASKS {
                    settled[ns] = prior;
                    ns += 1;
                }
            }
        })?;
        note(shadow);
        for s in settled[..ns].iter().flatten() {
            write_line(s);
        }
        Ok(total)
    }
    #[cfg(not(any(feature = "std", test)))]
    {
        let _ = (short, share, requester, kill_avoid, wait);
        Ok(0)
    }
}

/// LEDGER2 step 4: revoke's first ask (`asks`, today's) beside the one the same rule makes over the owner tree's bytes
/// (`tree_held`, the candidates' in order): one shadow decision, for `ledger2::note` once the asks are sent. No
/// allocation.
#[cfg(any(feature = "std", test))]
fn shadow_revoke(
    short: u64,
    share: u64,
    now: u64,
    cands: &[mempolicy::reclaim::RevokeCand],
    tree_held: &[u64],
    asks: &[mempolicy::reclaim::RevokeAsk],
) -> (crate::ledger2::Choice, crate::ledger2::Choice) {
    let mut tree = [mempolicy::reclaim::RevokeCand::default(); MAX_CANDS];
    for (i, c) in cands.iter().enumerate() {
        tree[i] = mempolicy::reclaim::RevokeCand { held: tree_held[i], ..*c };
    }
    let mut tree_asks = [mempolicy::reclaim::RevokeAsk::default(); MAX_ASKS];
    let k = mempolicy::reclaim::revoke_asks(short, share, now, &tree[..cands.len()], &mut tree_asks);
    let first = |a: Option<&mempolicy::reclaim::RevokeAsk>, held: &dyn Fn(usize) -> u64| {
        a.map_or((None, 0), |a| (Some(a.slot), cands.iter().position(|c| c.slot == a.slot).map_or(0, held)))
    };
    let today = first(asks.first(), &|i| cands[i].held);
    let by_tree = first(tree_asks[..k].first(), &|i| tree_held[i]);
    (today, by_tree)
}

/// D4324 (the ladder's R4b input): the spillable bytes of every running statement revoke may ask now, summed; 0 while
/// `pgrust.memory_revoke` is off (no ask would be sent). One registry walk, no allocation.
pub fn spillable_total() -> u64 {
    spillable_total_as(true).unwrap_or(0)
}

/// D4812 (2): `spillable_total` with the registry taken by try_lock (the block gate's R4b input): `Err(Busy)` when held.
pub fn try_spillable_total() -> Result<u64, crate::governor::Busy> {
    spillable_total_as(false)
}

fn spillable_total_as(wait: bool) -> Result<u64, crate::governor::Busy> {
    #[cfg(any(feature = "std", test))]
    {
        if !enabled() {
            return Ok(0);
        }
        let mut total = 0u64;
        walk_live(wait, &mut |l| total = total.saturating_add(l.revoke.spillable.load(Relaxed)))?;
        Ok(total)
    }
    #[cfg(not(any(feature = "std", test)))]
    {
        let _ = wait;
        Ok(0)
    }
}

/// C5's revoke test lever (`pgrust.gravity_test_revoke_every`, driver `grow::Fault`): ask this thread's statement to spill
/// half its spillable bytes (at least one), as another statement's draw would, whether or not `pgrust.memory_revoke` is on.
pub fn request_self() {
    #[cfg(any(feature = "std", test))]
    {
        let Some(l) = crate::governor::current() else { return };
        let bytes = (l.revoke.spillable.load(Relaxed) / 2).max(1);
        if let (_, Some(s)) = ask(&l, bytes, now_ms(), SELF_ASK) {
            write_line(&s);
        }
    }
}

/// D4324: `l`'s spillable bytes now (the ladder's asker read).
pub(crate) fn spillable_of(l: &crate::governor::StmtMem) -> u64 {
    l.revoke.spillable.load(Relaxed)
}

/// D4324 (the ladder's R4a): ask this thread's statement to spill `bytes` of its own (at least one), as `request_self`
/// asks, whether or not `pgrust.memory_revoke` is on: the asker's own memory, past its fair share.
pub fn request_own(bytes: u64) {
    #[cfg(any(feature = "std", test))]
    {
        if bytes == 0 {
            return;
        }
        let Some(l) = crate::governor::current() else { return };
        if let (_, Some(s)) = ask(&l, bytes, now_ms(), SELF_ASK) {
            write_line(&s);
        }
    }
    #[cfg(not(any(feature = "std", test)))]
    {
        let _ = bytes;
    }
}

/// The last revoke generation of this thread's statement (a claim's: its leader's), 0 when none was asked: what a
/// participant compares with the generation it last serviced (gravity_driver `mem::grow::revoke_home`).
#[inline]
pub fn generation() -> u32 {
    crate::governor::with_current(|l| l.revoke.gen.load(core::sync::atomic::Ordering::Acquire)).unwrap_or(0)
}

/// `delta` bytes more (or fewer) in breaker regions with a spill road this thread's statement fills now (the driver
/// refreshes its participants' share at claims, grows and pipeline changes).
pub fn note_spillable(delta: i64) {
    if delta == 0 {
        return;
    }
    let _ = crate::governor::with_current(|l| {
        if delta > 0 {
            l.revoke.spillable.fetch_add(delta as u64, Relaxed);
        } else {
            let d = delta.unsigned_abs();
            let _ = l.revoke.spillable.fetch_update(Relaxed, Relaxed, |s| Some(s.saturating_sub(d)));
        }
    });
}

/// The statement word (its generation) of this thread's statement: a participant's spillable share is counted against
/// the statement it was noted for (0: none running).
pub fn statement() -> usize {
    crate::governor::with_current(|l| l.statement_word()).unwrap_or(0)
}

/// The governor slot of this thread's statement's backend (a pool worker's claim: its leader's), the `requester` a Grow
/// passes to `request_revoke`; `None` when no statement runs here.
pub fn requester() -> Option<usize> {
    crate::governor::with_current(|l| l.slot_index()).flatten()
}

/// C3's bounded wait marks its statement: a waiter is never a victim (no cycles, 2c §2.4).
pub fn set_waiting(on: bool) {
    let _ = crate::governor::with_current(|l| l.revoke.waiting.store(on, Relaxed));
}

/// Ladder 1.3: the `fair_share` knob is set (spillable bytes are noted whatever `pgrust.memory_revoke` says).
#[inline]
pub fn fair_knob() -> bool {
    mempolicy::consts::get(mempolicy::consts::K::FairShare) == 1
}

/// Ladder 1.3-1.4: fair share acts (`fair_share` with `pgrust.memory_revoke` on).
#[inline]
pub fn fair_on() -> bool {
    enabled() && fair_knob()
}

/// Ladder 1.3: this thread's statement's floor (its work_mem), never a fair share under it (its admission sets it).
pub fn set_floor(bytes: u64) {
    let _ = crate::governor::with_current(|l| l.revoke.floor.store(bytes, Relaxed));
}

/// Ladder 1.4: the generation of this thread's statement's ask whose cut was lifted (0: none): a participant that cut its
/// region for that ask lifts the cut (gravity_driver `mem::grow::revoke_home`).
#[inline]
pub fn lifted() -> u32 {
    crate::governor::with_current(|l| l.revoke.lifted.load(core::sync::atomic::Ordering::Acquire)).unwrap_or(0)
}

/// Ladder 1.3-1.4 counters (the view): samples that set shares, the level last set (`u64::MAX`: all fit), the queries
/// over their share then, and the cuts lifted (asked and lifted by the sample; lifted by a participant).
pub static FAIR: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
pub const FAIR_TICKS: usize = 0;
pub const FAIR_LEVEL: usize = 1;
pub const FAIR_OVER: usize = 2;
pub const FAIR_LIFTS: usize = 3;
pub const FAIR_UNCUTS: usize = 4;

/// The most queries one `fair_tick` levels exactly; past it the smallest are counted as keeping what they hold.
const FAIR_MAX: usize = 1024;

/// Ladder 1.3 (`fair_share`, the user's approval 10-16), at each ladder sample: every running query's fair share, the
/// max-min level over the running queries' held bytes in `total` (`mempolicy::reclaim::fair_level`), never under its
/// floor; then 1.4's lift: a query asked before, quiet since its ask (`revoke_quiet_ms`) and back within its share gets
/// its cut lifted (the ask's generation stored in `lifted`, its revoke bit set so its participants' next poll lifts the
/// cuts). Two registry walks, no allocation; nothing while `fair_on` is false.
pub fn fair_tick(total: u64) {
    #[cfg(any(feature = "std", test))]
    {
        if !fair_on() {
            return;
        }
        let mut held = [0u64; FAIR_MAX];
        let (mut n, mut small) = (0usize, 0u64);
        crate::governor::for_each_live(&mut |l| {
            if l.slot_index().is_none() {
                return;
            }
            let h = l.held();
            if n < FAIR_MAX {
                held[n] = h;
                n += 1;
                return;
            }
            // full: the smallest is counted as keeping what it holds
            let (i, m) = held.iter().enumerate().min_by_key(|e| *e.1).map(|(i, &v)| (i, v)).unwrap_or((0, 0));
            if m < h {
                small = small.saturating_add(m);
                held[i] = h;
            } else {
                small = small.saturating_add(h);
            }
        });
        let level = mempolicy::reclaim::fair_level(&mut held[..n], total.saturating_sub(small));
        let (now, quiet) = (now_ms(), mempolicy::consts::get(mempolicy::consts::K::RevokeQuietMs));
        let (mut over, mut lifts) = (0u64, 0u64);
        crate::governor::for_each_live(&mut |l| {
            let Some(slot) = l.slot_index() else { return };
            let h = &l.revoke;
            let share = mempolicy::reclaim::fair_share(level, h.floor.load(Relaxed));
            h.share.store(share, Relaxed);
            let held = l.held();
            if held > share {
                over += 1;
                return;
            }
            let gen = h.gen.load(core::sync::atomic::Ordering::Acquire);
            if gen != 0 && h.lifted.load(Relaxed) != gen && now.saturating_sub(h.at_ms.load(Relaxed)) >= quiet {
                h.lifted.store(gen, core::sync::atomic::Ordering::Release);
                if let Some(b) = byte_at(slot) {
                    b.fetch_or(REVOKE_BIT, core::sync::atomic::Ordering::AcqRel);
                }
                lifts += 1;
            }
        });
        FAIR[FAIR_TICKS].fetch_add(1, Relaxed);
        FAIR[FAIR_LEVEL].store(level, Relaxed);
        FAIR[FAIR_OVER].store(over, Relaxed);
        FAIR[FAIR_LIFTS].fetch_add(lifts, Relaxed);
    }
    #[cfg(not(any(feature = "std", test)))]
    {
        let _ = total;
    }
}

/// Ladder 1.4: a participant lifted its region's cut (`fair_tick`'s lift reached it).
pub fn note_uncut() {
    FAIR[FAIR_UNCUTS].fetch_add(1, Relaxed);
}

/// Ladder 1.3: `l`'s fair share at the ladder's last sample (0: none set).
pub(crate) fn share_of(l: &crate::governor::StmtMem) -> u64 {
    l.revoke.share.load(Relaxed)
}

/// REVOKESLA: `w` holds `v` for ask `gen`, combined with what it held for the same ask by `keep` (a word of an earlier ask
/// is replaced: the first note of this one).
#[cfg(any(feature = "std", test))]
fn tag(w: &AtomicU64, gen: u32, v: u32, keep: fn(u32, u32) -> u32) {
    let _ = w.fetch_update(Relaxed, Relaxed, |x| {
        let v = if (x >> 32) as u32 == gen { keep(x as u32, v) } else { v };
        Some((gen as u64) << 32 | v as u64)
    });
}

/// REVOKESLA: what `w` holds for ask `gen` (`None`: nothing noted for it).
#[cfg(any(feature = "std", test))]
fn tagged(w: &AtomicU64, gen: u32) -> Option<u32> {
    let x = w.load(Relaxed);
    ((x >> 32) as u32 == gen).then_some(x as u32)
}

/// REVOKESLA: ms since ask `gen` of `h`, or `None` when a later ask has replaced it (the note would be of a settled ask).
#[cfg(any(feature = "std", test))]
fn since_ask(h: &Handle, gen: u32, now: u64) -> Option<u32> {
    if gen == 0 || h.gen.load(core::sync::atomic::Ordering::Acquire) != gen {
        return None;
    }
    Some(now.saturating_sub(h.at_ms.load(Relaxed)).min(u32::MAX as u64) as u32)
}

/// REVOKESLA: a participant of this thread's statement serviced ask `gen` (gravity_driver `mem::grow::revoke_home`): it
/// cut the region it fills, a breaker `op`; the statement stops growing there (but at a `Roadless` one).
pub fn sla_cut(gen: u32, op: Op) {
    #[cfg(any(feature = "std", test))]
    let _ = crate::governor::with_current(|l| {
        let h = &l.revoke;
        let Some(ms) = since_ask(h, gen, now_ms()) else { return };
        tag(&h.sla.ops, gen, 1 << op as u32, |a, b| a | b);
        if op != Op::Roadless {
            tag(&h.sla.cut_first, gen, ms, u32::min);
            tag(&h.sla.cut_last, gen, ms, u32::max);
            tag(&h.sla.cuts, gen, 1, u32::saturating_add);
        }
    });
    #[cfg(not(any(feature = "std", test)))]
    let _ = (gen, op);
}

/// REVOKESLA: a participant of this thread's statement gave `bytes` back to the kernel for ask `gen` (gravity_driver
/// `mem::grow::give_back_scattered`).
pub fn sla_given(gen: u32, bytes: u64) {
    #[cfg(any(feature = "std", test))]
    let _ = crate::governor::with_current(|l| {
        let h = &l.revoke;
        let Some(ms) = since_ask(h, gen, now_ms()) else { return };
        crate::ladder_stats::note_given(bytes);
        tag(&h.sla.given_kb, gen, (bytes >> 10).min(u32::MAX as u64) as u32, u32::saturating_add);
        tag(&h.sla.given_ms, gen, ms, u32::max);
    });
    #[cfg(not(any(feature = "std", test)))]
    let _ = (gen, bytes);
}

/// REVOKESLA: one settled ask, read off its statement's handle for its line.
#[cfg(any(feature = "std", test))]
#[derive(Clone, Copy, Debug)]
struct Settled {
    slot: usize,
    /// Its place among its statement's asks (1 = the first).
    n: u32,
    by: Option<u32>,
    asked: u64,
    /// ms after the ask it was settled, and whether by the statement's end (else by its next ask).
    at: u32,
    end: bool,
    cut_first: Option<u32>,
    cut_last: Option<u32>,
    cuts: u32,
    ops: u32,
    given_kb: u32,
    given_ms: Option<u32>,
}

/// REVOKESLA: `l`'s last ask, read for its line once (`None`: no ask, or its line is written), settled at `now` by the
/// statement's end (`end`) or by its next ask.
#[cfg(any(feature = "std", test))]
fn settle(l: &StmtMem, now: u64, end: bool) -> Option<Settled> {
    let h = &l.revoke;
    let gen = h.gen.load(core::sync::atomic::Ordering::Acquire);
    if gen == 0 || h.sla.told.swap(gen, Relaxed) == gen {
        return None;
    }
    let s = &h.sla;
    Some(Settled {
        slot: l.slot_index().unwrap_or(usize::MAX),
        n: h.count.load(Relaxed),
        by: tagged(&s.by, gen),
        asked: h.asked.load(Relaxed),
        at: now.saturating_sub(h.at_ms.load(Relaxed)).min(u32::MAX as u64) as u32,
        end,
        cut_first: tagged(&s.cut_first, gen),
        cut_last: tagged(&s.cut_last, gen),
        cuts: tagged(&s.cuts, gen).unwrap_or(0),
        ops: tagged(&s.ops, gen).unwrap_or(0),
        given_kb: tagged(&s.given_kb, gen).unwrap_or(0),
        given_ms: tagged(&s.given_ms, gen),
    })
}

/// REVOKESLA: the statement `l` ends (`governor::statement_end`, on its backend's thread): its last ask's line, unless a
/// later ask already wrote it.
pub(crate) fn settle_end(l: &StmtMem) {
    #[cfg(any(feature = "std", test))]
    if l.revoke.gen.load(Relaxed) != 0 {
        if let Some(s) = settle(l, now_ms(), true) {
            write_line(&s);
        }
    }
    #[cfg(not(any(feature = "std", test)))]
    let _ = l;
}

/// REVOKESLA: a line's bytes on the stack (no allocation), written to stderr in one call so another thread's log write
/// never lands inside it; a line past its length is cut.
#[cfg(any(feature = "std", test))]
struct LineBuf {
    b: [u8; 512],
    n: usize,
}

#[cfg(any(feature = "std", test))]
impl core::fmt::Write for LineBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let k = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
        self.n += k;
        Ok(())
    }
}

/// REVOKESLA: one `revoke-sla:` line on stderr (the server log), as cancelpoll writes its `poll-gap:` lines: the bytes
/// asked and of whom, the breakers the ask reached, the three times after the ask (stopped growing at the first and the
/// last cut, the bytes given back) and what settled it. No allocation.
#[cfg(any(feature = "std", test))]
#[cold]
fn write_line(s: &Settled) {
    crate::ladder_stats::note_settled(s.cut_last, s.given_ms);
    use core::fmt::Write as _;
    let mut e = LineBuf { b: [0; 512], n: 0 };
    let _ = write!(e, "revoke-sla: asked {} kB of slot {} (ask {} of its statement) by ", s.asked >> 10, s.slot, s.n);
    let _ = match s.by {
        Some(0) => write!(e, "the pressure ladder"),
        Some(SELF_ASK) => write!(e, "its own test lever"),
        Some(r) => write!(e, "a draw of slot {}", r - 1),
        None => write!(e, "?"),
    };
    let _ = write!(e, "; operator ");
    if s.ops == 0 {
        let _ = write!(e, "none");
    }
    let mut first = true;
    for (i, name) in OP_NAMES.iter().enumerate() {
        if s.ops & (1 << i) != 0 {
            let _ = write!(e, "{}{name}", if first { "" } else { "+" });
            first = false;
        }
    }
    let _ = match (s.cut_first, s.cut_last) {
        (Some(a), Some(b)) => write!(e, "; stopped growing {a} ms after the ask ({} region(s), the last {b} ms)", s.cuts),
        _ => write!(e, "; stopped growing: no region with a spill road cut"),
    };
    let _ = match s.given_ms {
        Some(ms) => write!(e, "; gave back {} kB {ms} ms after the ask", s.given_kb),
        None => write!(e, "; gave back nothing"),
    };
    let _ = write!(e, "; settled by {} {} ms after the ask", if s.end { "its statement's end" } else { "its next ask" }, s.at);
    e.n = e.n.min(e.b.len() - 1);
    e.b[e.n] = b'\n';
    // D4812: through the server's stderr writer (deferred on a memory safety thread), never a raw stderr write
    crate::server_log_line(&e.b[..e.n + 1]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests that switch `pgrust.memory_revoke` and call `request_revoke`, one at a time.
    static SWITCH: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// C5: an ask reaches the running statement's backend byte and its handle (bytes, generation), never the requester's
    /// and nothing while off; the victim's own lever asks again; its statement's end clears the bit.
    #[test]
    fn request_revoke_sets_the_victims_bit_and_generation() {
        let _one = SWITCH.lock().unwrap_or_else(|e| e.into_inner());
        static VICTIM: AtomicU8 = AtomicU8::new(0);
        const MIB: u64 = 1 << 20;
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let victim = std::thread::spawn(move || {
            crate::governor::register(7, 7007);
            register_byte(&VICTIM);
            crate::governor::statement_start(0, true);
            note_spillable((900 * MIB) as i64);
            assert_eq!(requester(), Some(7), "a Grow's requester: its statement's backend slot");
            ready_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            let asked = generation();
            let mine = crate::governor::current().map(|l| l.revoke().asked());
            VICTIM.fetch_and(!REVOKE_BIT, Relaxed);
            request_self();
            let again = generation();
            let bit = VICTIM.load(Relaxed) & REVOKE_BIT;
            crate::governor::statement_end();
            (asked, mine, again, bit)
        });
        ready_rx.recv().unwrap();
        set_enabled(false);
        assert_eq!(request_revoke(64 * MIB, 0, None), 0, "off: nobody asked");
        set_enabled(true);
        assert_eq!(request_revoke(0, 0, None), 0, "nothing short: nobody asked");
        assert_eq!(request_revoke(64 * MIB, 0, Some(7)), 0, "the requester is never asked");
        assert_eq!(VICTIM.load(Relaxed) & REVOKE_BIT, 0);
        assert_eq!(request_revoke(64 * MIB, 0, None), 64 * MIB, "what is short, under half its spillable bytes");
        assert_ne!(VICTIM.load(Relaxed) & REVOKE_BIT, 0, "the victim's byte carries the bit");
        set_enabled(false);
        go_tx.send(()).unwrap();
        let (asked, mine, again, bit) = victim.join().unwrap();
        assert_ne!(asked, 0, "the victim's statement reads the ask's generation");
        assert_eq!(mine, Some((64 * MIB, asked)));
        assert!(again != 0 && again != asked && bit != 0, "the lever asks the statement itself, a new generation");
        assert_eq!(VICTIM.load(Relaxed) & REVOKE_BIT, 0, "the statement's end clears the bit");
    }

    /// Ladder 1.3-1.4 (`fair_share`): each sample sets every running query's share (max-min over their held bytes, never
    /// under its work_mem floor); a revoke asks only a query over its share, for bytes up to its part over it, never a
    /// waiter nor a query within its share; a query asked and back within its share gets its cut lifted (its bit set,
    /// the ask's generation in `lifted`).
    #[test]
    fn ladder13_fair_shares_ask_only_over_share_queries_and_lift_their_cuts() {
        // its statements hold GiBs: never beside the governor tests (the watchdog's largest, the fits)
        let _gov = crate::tests::GOVERNOR_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let _one = SWITCH.lock().unwrap_or_else(|e| e.into_inner());
        use mempolicy::consts::{get, set, K};
        const MIB: u64 = 1 << 20;
        static BYTES: [AtomicU8; 3] = [const { AtomicU8::new(0) }; 3];
        // (slot, held MiB, spillable MiB, waits)
        let qs = [(31usize, 100u64, 50u64, false), (32, 900, 800, true), (33, 2000, 1500, false)];
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let mut gos = std::vec::Vec::new();
        let mut ts = std::vec::Vec::new();
        for (i, &(slot, held, spill, waits)) in qs.iter().enumerate() {
            let ready = ready_tx.clone();
            let (go_tx, go_rx) = std::sync::mpsc::channel::<u32>();
            gos.push(go_tx);
            ts.push(std::thread::spawn(move || {
                crate::governor::register(slot as i32, 9000 + slot as i32);
                register_byte(&BYTES[i]);
                crate::governor::statement_start(0, true);
                set_floor(64 * MIB);
                let c = crate::governor::Charge::new("fair", (held * MIB) as usize);
                note_spillable((spill * MIB) as i64);
                set_waiting(waits);
                ready.send(()).unwrap();
                // 1: the cut answered, its bytes given back (the charge dropped); 2: the end
                let mut c = Some(c);
                let mut seen = (0, 0);
                while let Ok(cmd) = go_rx.recv() {
                    if cmd == 1 {
                        c = None;
                    } else {
                        seen = (generation(), lifted());
                        break;
                    }
                }
                drop(c);
                crate::governor::statement_end();
                seen
            }));
        }
        for _ in 0..qs.len() {
            ready_rx.recv().unwrap();
        }
        let quiet = get(K::RevokeQuietMs);
        set(K::RevokeQuietMs, 0);
        set_enabled(true);
        set(K::FairShare, 0);
        fair_tick(1000 * MIB);
        // (share, generation, lifted) of the statement on `slot`
        let handle = |slot: usize| {
            let mut v = None;
            crate::governor::for_each_live(&mut |l| {
                if l.slot_index() == Some(slot) {
                    v = Some((l.revoke.share.load(Relaxed), l.revoke.gen.load(Relaxed), l.revoke.lifted.load(Relaxed)));
                }
            });
            v
        };
        let share = |slot: usize| handle(slot).map(|h| h.0);
        assert_eq!(share(33), Some(0), "fair_share off: no shares");
        set(K::FairShare, 1);
        let ticks = FAIR[FAIR_TICKS].load(Relaxed);
        // the three hold 3000 MiB of 1000 (and any other test's statements, none past a few MiB): 100 keeps its 100,
        // the other two split what is left
        fair_tick(1000 * MIB);
        assert_eq!(FAIR[FAIR_TICKS].load(Relaxed), ticks + 1);
        let level = FAIR[FAIR_LEVEL].load(Relaxed);
        assert!(level <= 450 * MIB && level > 400 * MIB, "the max-min level ({} MiB)", level / MIB);
        assert_eq!((share(31), share(32), share(33)), (Some(level), Some(level), Some(level)));
        // the ladder's rung: only 33 (32 waits, 31 is within its share), for what is short
        assert_eq!(request_revoke_within_shares(600 * MIB, 0, None), 600 * MIB, "33 asked for what is short, under its part over its share");
        assert_eq!((BYTES[0].load(Relaxed), BYTES[1].load(Relaxed)) , (0, 0), "within its share, and a waiter: never asked");
        assert_ne!(BYTES[2].load(Relaxed) & REVOKE_BIT, 0);
        BYTES[2].fetch_and(!REVOKE_BIT, Relaxed);
        // 33 answers (its bytes back): the next sample lifts its cut
        let (_, gen, lifted0) = handle(33).unwrap();
        assert!(gen != 0 && lifted0 == 0, "asked, not lifted: {:?}", handle(33));
        fair_tick(1000 * MIB);
        assert_eq!(handle(33).unwrap().2, 0, "still over its share: no lift");
        gos[2].send(1).unwrap();
        let lifts = FAIR[FAIR_LIFTS].load(Relaxed);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while handle(33).unwrap().2 == 0 && std::time::Instant::now() < deadline {
            fair_tick(1000 * MIB);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(handle(33).unwrap().2, gen, "back within its share: its ask's cut lifted");
        assert!(FAIR[FAIR_LIFTS].load(Relaxed) > lifts, "counted");
        assert_ne!(BYTES[2].load(Relaxed) & REVOKE_BIT, 0, "its bit set: its participants' next poll lifts the cuts");
        BYTES[2].fetch_and(!REVOKE_BIT, Relaxed);
        fair_tick(1000 * MIB);
        assert_eq!((handle(33).unwrap().2, BYTES[2].load(Relaxed) & REVOKE_BIT), (gen, 0), "once per ask");
        assert_eq!((handle(31).unwrap().2, handle(32).unwrap().2), (0, 0), "never asked: nothing to lift");
        for g in &gos {
            g.send(2).unwrap();
        }
        let seen: std::vec::Vec<(u32, u32)> = ts.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(seen[2], (gen, gen), "33 reads the lifted generation of its ask");
        assert_eq!((seen[0], seen[1]), ((0, 0), (0, 0)));
        set(K::FairShare, 0);
        set(K::RevokeQuietMs, quiet);
        set_enabled(false);
    }

    /// REVOKESLA (main 2026-10-12T22:13:47Z): an ask's timeline on its statement's handle: who asked, its participants'
    /// cuts (first and last; a roadless one names its breaker but stops nothing) and give-backs, each in ms after the ask;
    /// the next ask settles it (its line) and a late note of it is dropped; the statement's end settles the last one;
    /// each ask is settled once. The asks are `ask`'s own, not `request_revoke`'s: this statement notes no spillable
    /// bytes, so it is never a candidate of another test's `request_revoke`, and `pgrust.memory_revoke` stays untouched.
    #[test]
    fn revokesla_an_asks_timeline_is_noted_by_generation_and_settled_once() {
        static VICTIM: AtomicU8 = AtomicU8::new(0);
        const MIB: u64 = 1 << 20;
        let victim = std::thread::spawn(move || {
            crate::governor::register(9, 9009);
            register_byte(&VICTIM);
            crate::governor::statement_start(0, true);
            let l = crate::governor::current().unwrap();
            // the first ask (the pressure ladder's) reaches three participants; one gives 8 MiB back
            let (b1, none) = ask(&l, 64 * MIB, now_ms(), 0);
            let g1 = generation();
            sla_cut(g1, Op::HashAggT1);
            std::thread::sleep(std::time::Duration::from_millis(30));
            sla_cut(g1, Op::Sort);
            sla_cut(g1, Op::Roadless);
            sla_given(g1, 8 * MIB);
            // the second ask (a draw of slot 3's) settles the first; a late note of the first is dropped
            let (b2, first) = ask(&l, 32 * MIB, now_ms(), 4);
            let g2 = generation();
            sla_cut(g1, Op::Owned);
            sla_given(g1, 64 * MIB);
            sla_cut(g2, Op::HashBuild);
            let second = settle(&l, now_ms(), true);
            let again = settle(&l, now_ms(), true);
            VICTIM.fetch_and(!REVOKE_BIT, Relaxed);
            crate::governor::statement_end();
            (b1, b2, none, g1, g2, first, second, again)
        });
        let (b1, b2, none, g1, g2, first, second, again) = victim.join().unwrap();
        assert_eq!((b1, b2), (64 * MIB, 32 * MIB));
        assert!(none.is_none(), "the statement's first ask settles nothing: {none:?}");
        assert!(g1 != 0 && g2 != 0 && g1 != g2);
        let f = first.expect("the next ask settles the first");
        assert_eq!((f.slot, f.n, f.by, f.asked, f.end), (9, 1, Some(0), 64 * MIB, false), "{f:?}");
        assert_eq!(f.cuts, 2, "the roadless cut stops nothing: {f:?}");
        assert_eq!(f.ops, 1 << Op::HashAggT1 as u32 | 1 << Op::Sort as u32 | 1 << Op::Roadless as u32, "{f:?}");
        let (a, b) = (f.cut_first.unwrap(), f.cut_last.unwrap());
        assert!(b >= a + 25, "stopped growing at the first cut, the last 30 ms later: {f:?}");
        assert_eq!((f.given_kb, f.given_ms.is_some()), (8 << 10, true), "{f:?}");
        let s = second.expect("the statement's end settles the second");
        assert_eq!((s.n, s.by, s.asked, s.end, s.cuts), (2, Some(4), 32 * MIB, true, 1), "{s:?}");
        assert_eq!(s.ops, 1 << Op::HashBuild as u32, "the first ask's late notes are dropped: {s:?}");
        assert_eq!((s.given_kb, s.given_ms), (0, None), "{s:?}");
        assert!(again.is_none(), "an ask's line is written once: {again:?}");
    }
}

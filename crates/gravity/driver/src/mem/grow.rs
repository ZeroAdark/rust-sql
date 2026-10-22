//! D254 ("no runtime 'past its share' errors before M7's spill.breakers"): a breaker region that outgrows its share
//! at run time reopens larger instead of failing by name. `region_grow` (the IR's host, `spill(SPILL_REGION_GROW)`) reopens the
//! region the calling owner's pipeline produces (the leader's slot at W1, a worker's partial at W >= 2); `State::grow` reopens the
//! leader's slot under a merge (`merge_into`, `finish`, phase 2's `prepare_partitions_in`). Memory comes from W, then G, then pool
//! bytes reserved past both (D225's arena rule, `node::chunk_region`): the only failure is a real one (the pool refusing within its
//! wait, or the allocator), `Error::OutOfMemory`. Grows are counted (`Accounting::past_share`, `past_share_bytes`) and logged once
//! per statement on the leader: `gravity: <breaker> past its share: <bytes> of <share>`.
#![allow(unsafe_code)]

use gravity_abi::breaker::{BreakerKind, HashAgg};
use gravity_program::spill::NoSpill;

use crate::mem::grant::{Counters, Reservation};
use crate::node::{Inner, Node, RegionBuf, SlotRegion};
use crate::sink::{SinkRef, State};
use crate::slots::WorkerSlot;
use crate::{Error, Result};

/// A reopened region's base alignment.
pub const ALIGN: usize = 64;
/// The largest region: a region's length is a u32 everywhere (`SlotRegion.bytes`, a worker's `state_bytes`, the armed limit).
pub const MAX_REGION: u64 = (u32::MAX as u64) & !(crate::park::REGION_MIN as u64 - 1);

/// Where `region_grow` reopens the region the running pipeline produces.
#[derive(Clone, Copy)]
pub(crate) enum Home {
    None,
    /// The leader's slot, set when the pipeline opens (`Node::open_pipeline`); `Node.slots` lives in the Node's arena.
    Slot(*mut SlotRegion),
    /// A worker's partial, set at its section bind and cleared at detach (the slot vector does not move during a section).
    Worker(*mut WorkerSlot),
}

/// The grows an owner made since the leader last folded them: how many, the length before the first, the last length.
#[derive(Clone, Copy, Default)]
pub(crate) struct Grew {
    pub(crate) n: u32,
    pub(crate) share: u64,
    pub(crate) top: u64,
}

impl Grew {
    pub(crate) fn note(&mut self, from: u64, to: u64) {
        if self.n == 0 {
            self.share = from;
        }
        self.n += 1;
        self.top = to;
    }
}

/// The leader's slot behind a growable `State` (the Node is borrowed for the State's life).
#[derive(Clone, Copy)]
pub(crate) struct Leader {
    region: *mut SlotRegion,
    stmt: *const Reservation,
    counters: *const Counters,
    kind: Option<BreakerKind>,
    /// M7-SPILL: the slot's stream, for a merge that spills (`State::spill_write`, the leader's file, index 0).
    spill: *mut crate::mem::spill::SpillTable,
    slot: u8,
}

/// The reopened length: max(2 x len, head + tail + need), page-rounded, at most `MAX_REGION`; `None` when even the need is past
/// it, or when the region cannot grow (D1029: a region at `MAX_REGION` reopened at its own length freed no room, and the IR's
/// retry spun; the caller's named out-of-memory until M7's spill takes that region, SPILL-1).
pub fn grown_len(len: u64, need: u64, head: u64, tail: u64) -> Option<u64> {
    let want = head.checked_add(tail)?.checked_add(need)?;
    if want > MAX_REGION {
        return None;
    }
    let n = mempolicy::grow::grow_to(len, want, MAX_REGION);
    Some((crate::park::region_bytes(n as usize) as u64).min(MAX_REGION)).filter(|&new| new > len)
}

/// exactsize B1a: the reopened length where the need is counted (a hash build's end: its records, directory and images,
/// `State::grow_exact`): head + tail + need, page-rounded, never doubled (no later grow follows it); `None` as `grown_len`.
pub fn exact_len(len: u64, need: u64, head: u64, tail: u64) -> Option<u64> {
    let want = head.checked_add(tail)?.checked_add(need)?;
    if want > MAX_REGION {
        return None;
    }
    Some((crate::park::region_bytes(want as usize) as u64).min(MAX_REGION)).filter(|&new| new > len)
}

/// `exact_len` when `exact`, else `grown_len`.
fn next_len(len: u64, need: u64, head: u64, tail: u64, exact: bool) -> Option<u64> {
    if exact {
        exact_len(len, need, head, tail)
    } else {
        grown_len(len, need, head, tail)
    }
}

fn oom(bytes: u64) -> Error {
    let pool = &crate::mem::grant::POOL;
    Error::OutOfMemory { bytes, held: pool.held(), capacity: pool.capacity(), why: None }
}

/// `oom` for a region whose grown length would pass `MAX_REGION` (or that cannot grow): no pool asked (D4914's branch).
pub(crate) fn max_region(bytes: u64) -> Error {
    crate::mem::grant::note_refusal(mcx::ladder_stats::Why::MaxRegion, bytes);
    oom(bytes)
}

/// A refused grow or open of a region whose road (`road`: a spilling breaker's road that names its debt) or whose breaker
/// (`home`: `SlotRegion::debt`, `WorkerSlot::share_debt`) names a SPILL-DEBT row: an `OutOfMemory` names it, the road's
/// row first (as `counted_as` counts it), then the row it names already, then the home's. The client's 53200 is the same;
/// exec glue writes the row in the server log when the error is the statement's. Any other error is as it is.
pub(crate) fn debt_refused(e: Error, home: Option<NoSpill>, road: Option<NoSpill>) -> Error {
    match e {
        Error::OutOfMemory { bytes, held, capacity, why } => Error::OutOfMemory { bytes, held, capacity, why: road.or(why).or(home) },
        e => e,
    }
}

/// A fresh region of `len` bytes (W first, then G, then reserved past both), its base `ALIGN`-aligned, with the old region's
/// `[0, head)` at its start and the old `[old_len - tail, old_len)` at its end; the bytes between are unspecified, as every
/// region's are until its table lays them out.
/// # Safety — `old` holds `old_len` readable bytes.
pub(crate) unsafe fn reopen(old: *const u8, old_len: usize, head: usize, tail: usize, len: usize, w: &mut u64, stmt: Option<&Reservation>, worker: bool) -> Result<(RegionBuf, *mut u8)> {
    reopen_road(old, old_len, head, tail, len, w, stmt, worker, false)?.ok_or_else(|| oom(len as u64))
}

/// `reopen`; `now`: the bytes past W and G are the pool's room as it is now (`chunk_region_now`: no wait), `None` when it
/// has none (a breaker under its ceiling, which spills then: `region_grow_capped`).
/// # Safety — as `reopen`.
pub(crate) unsafe fn reopen_road(old: *const u8, old_len: usize, head: usize, tail: usize, len: usize, w: &mut u64, stmt: Option<&Reservation>, worker: bool, now: bool) -> Result<Option<(RegionBuf, *mut u8)>> {
    if head.checked_add(tail).is_none_or(|t| t > old_len) {
        return Err(Error::host(format!("gravity: region_grow: head {head} + tail {tail} past the region's {old_len} bytes"))); // alloc: Cold
    }
    if let Some(s) = stmt {
        s.region_draw()?;
    }
    let buf = if now { crate::node::chunk_region_now(len + ALIGN, Some(w), stmt, worker) } else { crate::node::chunk_region(len + ALIGN, Some(w), stmt, worker) };
    let Some(mut buf) = buf else { return Ok(None) };
    let p = buf.as_mut_ptr();
    let base = p.add(p.align_offset(ALIGN));
    core::ptr::copy_nonoverlapping(old, base, head);
    core::ptr::copy_nonoverlapping(old.add(old_len - tail), base.add(len - tail), tail);
    Ok(Some((buf, base)))
}

/// The breakers' grows refused at their ceiling (`capped_len`: the need past the breaker's work_mem budget), this process:
/// each one a spill PostgreSQL's node would take there too.
pub static AT_CEILING: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The most a breaker region reopens to under its ceiling `ceil` (`capped_len`): the ceiling's region bytes.
pub(crate) fn ceil_cap(ceil: u64) -> u64 {
    (crate::park::region_bytes(ceil.min(MAX_REGION) as usize) as u64).min(MAX_REGION)
}

/// The reopened length of a breaker region under its ceiling `ceil` (`grant::ceiling_of`; the leader's
/// `grant::leader_ceiling`): `grown_len`'s, at most the ceiling's region bytes; `None` when the need itself is past the
/// ceiling (the breaker spills: counted in `AT_CEILING`) or the region cannot grow.
pub fn capped_len(len: u64, need: u64, head: u64, tail: u64, ceil: u64, exact: bool) -> Option<u64> {
    let cap = ceil_cap(ceil);
    let want = head.checked_add(tail)?.checked_add(need)?;
    if mempolicy::grow::spills_at_ceiling(want, cap) {
        at_ceiling();
        return None;
    }
    next_len(len, need, head, tail, exact).map(|n| n.min(cap)).filter(|&n| n > len && n >= want)
}

/// A breaker's grow refused at its ceiling: counted (`AT_CEILING`) and fired.
fn at_ceiling() {
    AT_CEILING.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    fires::fire_shared("breaker.at_ceiling"); // shapecheck: a breaker's grow refused at its work_mem ceiling, so it spills as PostgreSQL's node does
}

/// The ceiling's road: past its reservation, a breaker region with a ceiling (`grant::ceiling_of`) reopens at most at it,
/// its bytes from W, then G, then the pool's room now (no wait); `None` (nothing drawn) past the ceiling or when the pool
/// has no room, so the breaker spills there, as PostgreSQL's node spills past work_mem. G's room past the ceiling no
/// longer grows it.
/// # Safety — as `region_grow`.
unsafe fn region_grow_capped(n: &mut Inner, len: u64, need: u64, head: u64, tail: u64, ceil: u64) -> Result<Option<u64>> {
    let Some(new) = capped_len(len, need, head, tail, ceil, false) else { return Ok(None) };
    match n.home {
        Home::Slot(r) => {
            let r = &mut *r;
            let stmt = n.stmt.as_ref();
            let Some((mut buf, base)) = reopen_road(r.ptr, len as usize, head as usize, tail as usize, new as usize, &mut n.w, stmt, false, true)? else { return Ok(None) };
            if let Some(o) = r.owner {
                buf.own_by(o); // LEDGER2 step 5: the reopened region is its slot owner's, as the one it replaces
            }
            HashAgg::arm(base, new as u32);
            crate::park::REGIONS.put_from_life(core::mem::replace(&mut r.buf, buf), stmt);
            r.ptr = base;
            r.bytes = new as u32;
            n.grew.note(len, new);
            (*n.partials).own = gravity_abi::word(base);
            Ok(Some(base as u64))
        }
        Home::Worker(s) => Ok(crate::slots::grow_partial_to(s, n, new, head, tail)?.map(|b| b as u64)),
        Home::None => Err(Error::host("gravity: region_grow from a pipeline that produces no region")),
    }
}

/// The gate on one breaker region's spill-capable room asks (`region_grow_in`, `region_grow_place`, a T1 floor's
/// `region_commit`, the leader merge's `grow_state_in`), set at the region's open beside its ceiling. A refused ask takes
/// the road the region's ceiling takes there: the hash build widens its spilled partitions, the hash aggregate's table
/// compacts and scatters, the sort puts its run. Three causes:
/// - the forced-spill test fault (`Grant::spill_every`, `pgrust.gravity_test_spill_every`): every `every`th ask refused. A
///   refusal between grants is a pool's room gone and back, a road production takes too;
/// - C5's revoke (`cut`, `revoke_home`): once another statement's draw or the pressure ladder asked this statement to
///   spill (`mcx::revoke`), every ask of the region it was filling is refused for the rest of its life, so it spills
///   from then on as a work_mem-sized breaker does (2c §4: it never regrows into what it gave);
/// - the revoke test lever (`Grant::revoke_every`, `pgrust.gravity_test_revoke_every`): every `revoke_every`th ask asks a
///   revoke of its own statement (`mcx::revoke::request_self`) and goes on, so the revoke road (the interrupt byte's bit,
///   the poll, the cut, the refused ask) runs inside one statement.
///
/// NR-6's debt lever (`debt`: `Grant::refuse_debt`, `pgrust.gravity_test_refuse_debt`) is no room ask's cause: it refuses
/// a draw of a SPILL-DEBT row's road past the region's cap (`debt_draw`), as the pool refuses one.
///
/// All off: one load and a branch on these cold roads.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Fault {
    every: u32,
    asks: u32,
    revoke_every: u32,
    cut: bool,
    debt: bool,
}

/// The room asks the forced-spill test fault refused (`Fault`), this process.
pub static FORCED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// C5: the breaker regions a revoke cut (`Fault::cut`) and the room asks refused at a cut, this process.
pub static REVOKE_CUTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static REVOKE_REFUSED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

impl Fault {
    pub(crate) const OFF: Fault = Fault { every: 0, asks: 0, revoke_every: 0, cut: false, debt: false };

    pub(crate) const fn every(every: u32) -> Fault {
        Fault { every, ..Fault::OFF }
    }

    /// `self` with the revoke test lever at every `n`th ask (0 = off).
    pub(crate) const fn with_revoke_every(self, n: u32) -> Fault {
        Fault { revoke_every: n, ..self }
    }

    /// `self` with NR-6's debt lever (`debt`; false = off).
    pub(crate) const fn with_refuse_debt(self, on: bool) -> Fault {
        Fault { debt: on, ..self }
    }

    /// Whether any cause is on: an ask may be refused.
    #[inline]
    pub(crate) fn armed(&self) -> bool {
        self.cut || (self.every | self.revoke_every) != 0
    }

    /// Whether this room ask is refused (counted in `FORCED` / `REVOKE_REFUSED`).
    #[inline]
    pub(crate) fn refuse(&mut self) -> bool {
        self.armed() && self.ask()
    }

    /// C5: a revoke cut the region (`cut`).
    pub(crate) fn is_cut(&self) -> bool {
        self.cut
    }

    /// C5: the region's room asks refused from now on (a revoke reached its filler).
    pub(crate) fn cut(&mut self) {
        if !self.cut {
            self.cut = true;
            REVOKE_CUTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Ladder 1.4 (`fair_share`): the cut lifted (its statement came back within its fair share): room asks served again.
    pub(crate) fn uncut(&mut self) -> bool {
        core::mem::replace(&mut self.cut, false)
    }

    #[cold]
    fn ask(&mut self) -> bool {
        if self.cut {
            REVOKE_REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return true;
        }
        self.asks = self.asks.wrapping_add(1);
        if self.revoke_every != 0 && self.asks % self.revoke_every == 0 {
            mcx::revoke::request_self();
        }
        if self.every == 0 || self.asks % self.every != 0 {
            return false;
        }
        FORCED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        true
    }
}

/// A leader slot's fault (`SlotRegion::fault`, a `Cell`: the owner phase asks it through a shared `Node`,
/// `Node::owner_refuses`) read, run through `f` and stored back.
#[inline]
pub(crate) fn with_fault<R>(c: &core::cell::Cell<Fault>, f: impl FnOnce(&mut Fault) -> R) -> R {
    let mut v = c.get();
    let r = f(&mut v);
    c.set(v);
    r
}

/// C5 (revoke, reports/MEMORY-STEP2C-REVOKE-DESIGN-20261010.md §2.3): the region this participant fills (its home: the
/// leader's slot, a worker's share) cut (`Fault::cut`) once per revoke its statement was asked (`mcx::revoke::generation`),
/// so its next room ask takes its existing spill road. Run at the kernels' poll on a set `REVOKE_BIT` and at each claim's
/// start, on the participant's own thread: no thread touches another's region. A region with no spill road never asks
/// its gate, so a cut there changes nothing. `true` when it cut.
/// # Safety — `n` is a bound owner on the thread running it.
pub(crate) unsafe fn revoke_home(n: &mut Inner) -> bool {
    let gen = mcx::revoke::generation();
    // ladder 1.4 (`fair_share`): the ask this participant cut for was lifted (its statement is back within its share)
    let lift = mcx::revoke::lifted();
    if lift != 0 && lift == n.revoke_seen && lift != n.lift_seen {
        n.lift_seen = lift;
        let lifted = match n.home {
            Home::Slot(r) => with_fault(&(*r).fault, Fault::uncut),
            Home::Worker(s) => (*s).share_fault.uncut(),
            Home::None => false,
        };
        if lifted {
            mcx::revoke::note_uncut();
        }
    }
    if gen == 0 || gen == n.revoke_seen {
        return false;
    }
    n.revoke_seen = gen;
    match n.home {
        Home::Slot(r) => with_fault(&(*r).fault, |f| f.cut()),
        Home::Worker(s) => (*s).share_fault.cut(),
        Home::None => return false,
    }
    // REVOKESLA: the ask reached this participant's breaker (its statement's `revoke-sla:` line)
    mcx::revoke::sla_cut(gen, home_op(n));
    true
}

/// REVOKESLA: the breaker of the region this participant fills, read off its table header's kind byte
/// (`gravity_abi::breaker::TABLE_KIND_OFF`); `Roadless` when the region has no spill road (no ceiling: a cut changes
/// nothing there), `Other` when it was never laid out or names no spillable table.
/// # Safety — as `revoke_home`.
unsafe fn home_op(n: &Inner) -> mcx::revoke::Op {
    use gravity_abi::breaker::{table_kind, TABLE_AGG, TABLE_BUILD, TABLE_KIND_OFF, TABLE_OWNED, TABLE_SORT, TABLE_T1, TABLE_V4BUILD};
    use mcx::revoke::Op;
    let (road, at, len) = match n.home {
        Home::Slot(r) => ((*r).ceil != 0, (*r).buf.as_ptr(), (*r).buf.len()),
        Home::Worker(s) => ((*s).share_ceil != 0, (*s).state_ptr as *const u8, (*s).state_bytes as usize),
        Home::None => return Op::Other,
    };
    if !road {
        return Op::Roadless;
    }
    if at.is_null() || len <= TABLE_KIND_OFF {
        return Op::Other;
    }
    match table_kind(at) {
        TABLE_AGG => Op::HashAgg,
        TABLE_T1 => Op::HashAggT1,
        TABLE_BUILD | TABLE_V4BUILD => Op::HashBuild,
        TABLE_SORT => Op::Sort,
        TABLE_OWNED => Op::Owned,
        _ => Op::Other,
    }
}

/// D4314 (b): the memory policy's name for the breaker a cut of this participant's home lands on (`home_op`'s, read off
/// the same kind byte): the hand kernels' build (`TABLE_BUILD`, no give-back road) reads `Other`, a lowered one
/// (`TABLE_V4BUILD`) `HashBuild`.
/// # Safety — as `revoke_home`.
unsafe fn cut_kind(n: &Inner) -> mempolicy::reclaim::CutKind {
    use mcx::revoke::Op;
    use mempolicy::reclaim::CutKind;
    match home_op(n) {
        Op::HashAggT1 => CutKind::HashAggT1,
        Op::HashAgg => CutKind::HashAgg,
        Op::HashBuild if home_table_kind(n) == gravity_abi::breaker::TABLE_V4BUILD => CutKind::HashBuild,
        Op::Sort => CutKind::Sort,
        Op::Owned => CutKind::Owned,
        _ => CutKind::Other,
    }
}

/// The kind byte of the table in this participant's home region (0 when none is laid out there).
/// # Safety — as `revoke_home`.
unsafe fn home_table_kind(n: &Inner) -> u8 {
    let (at, len) = match n.home {
        Home::Slot(r) => ((*r).buf.as_ptr(), (*r).buf.len()),
        Home::Worker(s) => ((*s).state_ptr as *const u8, (*s).state_bytes as usize),
        Home::None => return 0,
    };
    if at.is_null() || len <= gravity_abi::breaker::TABLE_KIND_OFF { 0 } else { gravity_abi::breaker::table_kind(at) }
}

/// C5: the region this participant fills was cut by a revoke (`revoke_home`).
/// # Safety — as `revoke_home`.
pub(crate) unsafe fn home_cut(n: &Inner) -> bool {
    match n.home {
        Home::Slot(r) => (*r).fault.get().is_cut(),
        Home::Worker(s) => (*s).share_fault.is_cut(),
        Home::None => false,
    }
}

/// C5: the bytes revoked breakers gave back after their scatter (`give_back_scattered`), this process.
pub static REVOKE_GIVEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// C5: this process's revoke counters, `[cuts, refused asks, bytes given back]` (the report line's, exec runner).
pub fn revoke_counts() -> [u64; 3] {
    use core::sync::atomic::Ordering::Relaxed;
    [REVOKE_CUTS.load(Relaxed), REVOKE_REFUSED.load(Relaxed), REVOKE_GIVEN.load(Relaxed)]
}

/// D4265: why a cut home's give-back gave what it gave, process-wide (`give_back_counts`): `[tried, given, no growth,
/// not reserved, does not fit, not home]`. `tried` counts each cut home once (its first scatter after the cut), and so does
/// its first attempt's reason when it gave nothing; every give-back counts under `given`. A cut home that never scattered
/// again before its region was parked is `REVOKE_CUTS` less `tried`. D4394: `not home` (the table is not the cut home's
/// buffer, `cut_buf`'s `None`) apart from `not reserved` (it is, but that buffer is no reservation).
pub static GIVE_BACK: [core::sync::atomic::AtomicU64; 6] = [const { core::sync::atomic::AtomicU64::new(0) }; 6];
const GIVE_TRIED: usize = 0;
const GIVE_GIVEN: usize = 1;
const GIVE_NO_GROWTH: usize = 2;
const GIVE_NOT_RESERVED: usize = 3;
const GIVE_NO_FIT: usize = 4;
const GIVE_NOT_HOME: usize = 5;

/// D4265: `GIVE_BACK` now (`[tried, given, no growth, not reserved, does not fit, not home]`).
pub fn give_back_counts() -> [u64; 6] {
    GIVE_BACK.each_ref().map(|c| c.load(core::sync::atomic::Ordering::Relaxed))
}

/// D4394: where a give-back attempt looked for its table, for its `revoke-giveback:` line: the participant's home (`slot`,
/// `worker`, `none`), why no cut home buffer is the table (`miss`: `uncut`, `noshare` = a worker partial that is not its share
/// buffer, `base` = the buffer starts elsewhere, `nohome`; empty when one is), the table's offset from that buffer, and the
/// buffer's length, reservation and committed bytes (`noshare`: the partial's offset and length, no buffer).
#[derive(Clone, Copy)]
struct GiveAt {
    home: &'static str,
    miss: &'static str,
    off: isize,
    len: usize,
    reserve: usize,
    committed: usize,
}

impl core::fmt::Display for GiveAt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "home={}", self.home)?;
        if !self.miss.is_empty() {
            write!(f, " miss={}", self.miss)?;
        }
        write!(f, " off={} len={} reserve={} committed={}", self.off, self.len, self.reserve, self.committed)
    }
}

/// D4394: `GiveAt` of the table at `region` for this participant (`cut_buf`'s tests, in its order).
/// # Safety — as `revoke_home`.
unsafe fn give_at(n: &Inner, region: *const u8) -> GiveAt {
    let at = |home, miss, b: &RegionBuf| GiveAt {
        home,
        miss,
        off: (region as isize).wrapping_sub(b.as_ptr() as isize),
        len: b.len(),
        reserve: b.reserve(),
        committed: b.committed(),
    };
    match n.home {
        Home::Slot(r) => {
            let b = &(*r).buf;
            at("slot", if !(*r).fault.get().is_cut() { "uncut" } else if b.as_ptr() != region { "base" } else { "" }, b)
        }
        Home::Worker(s) if !(*s).share_fault.is_cut() => GiveAt { home: "worker", miss: "uncut", off: 0, len: 0, reserve: 0, committed: 0 },
        Home::Worker(s) => match crate::slots::share_buf_of(s) {
            Some(b) => at("worker", if b.as_ptr() != region { "base" } else { "" }, &*b),
            None => GiveAt {
                home: "worker",
                miss: "noshare",
                off: (region as isize).wrapping_sub((*s).state_ptr as isize),
                len: (*s).state_bytes as usize,
                reserve: 0,
                committed: 0,
            },
        },
        Home::None => GiveAt { home: "none", miss: "nohome", off: 0, len: 0, reserve: 0, committed: 0 },
    }
}

/// D4265: a give-back attempt of a cut home counted under `why`, with one `revoke-giveback:` line on stderr (the server
/// log, beside the statement's `revoke-sla:` line): the table, the reason, the bytes and the process's counters. Only a
/// home's first attempt after its cut, and every attempt that gave, are counted and written: a cut table that already gave
/// its growth back scatters on at its shrunk length with nothing more to give (one line per flush would flood the log).
fn give_tried(n: &mut Inner, table: &str, why: usize, bytes: u64, at: GiveAt) {
    use core::sync::atomic::Ordering::Relaxed;
    let first = n.revoke_tried != n.revoke_seen;
    if !first && why != GIVE_GIVEN {
        return;
    }
    if first {
        n.revoke_tried = n.revoke_seen;
        GIVE_BACK[GIVE_TRIED].fetch_add(1, Relaxed);
    }
    GIVE_BACK[why].fetch_add(1, Relaxed);
    const WHY: [&str; 6] = ["tried", "given", "no-growth", "not-reserved", "no-fit", "not-home"];
    let c = give_back_counts();
    let cuts = REVOKE_CUTS.load(Relaxed);
    eprintln!("revoke-giveback: {table} {} bytes={bytes} {at} (process: cuts={cuts} tried={} given={} no_growth={} not_reserved={} no_fit={} not_home={})", WHY[why], c[0], c[1], c[2], c[3], c[4], c[5]); // alloc: Cold
}

/// D4265: the region buffer of this participant's CUT home whose table is at `region` (the leader's slot, a worker's share),
/// with the home's length word (`SlotRegion::bytes`, `WorkerSlot::state_bytes`); `None` when the home is not cut or
/// `region` is not its buffer's base (a reopened region's aligned base, an owner's cut of another region).
/// # Safety — as `revoke_home`.
unsafe fn cut_buf<'a>(n: &Inner, region: *const u8) -> Option<(&'a mut RegionBuf, *mut u32)> {
    match n.home {
        Home::Slot(r) if (*r).fault.get().is_cut() && (*r).buf.as_ptr() == region => Some((&mut (*r).buf, core::ptr::addr_of_mut!((*r).bytes))),
        Home::Worker(s) if (*s).share_fault.is_cut() => match crate::slots::share_buf_of(s) {
            Some(b) if b.as_ptr() == region => Some((b, core::ptr::addr_of_mut!((*s).state_bytes))),
            _ => None,
        },
        _ => None,
    }
}

/// C5 (revoke's give-back, 2c section 2.3 step 3): a revoked hash aggregate's phase-1 table (`T1_RESERVED`, the region
/// this participant fills), just scattered whole by its flush (`SPILL_SCATTER`: every record written, the pool empty),
/// gives back what its pool grew past the region's length: the table's pool shrinks by those blocks (`T1::shrink_pool`,
/// `grow_pool`'s reverse) and the region's pages past its length leave with their pool holds
/// (`RegionBuf::give_back_committed`). The cut never lifts, so the table spills at its shrunk pool from then on. Only when
/// every byte the table still reads (its header, chains, slots and kept blocks) lies within the region's length; else
/// nothing changes. The bytes given back. D4265: each attempt counted by its reason (`give_tried`).
/// # Safety — as `revoke_home`; `region` is the table the scatter just wrote, no appender open.
pub(crate) unsafe fn give_back_scattered(n: &mut Inner, region: *mut u8) -> u64 {
    use gravity_abi::breaker::{table_kind, T1, TABLE_T1};
    if table_kind(region) != TABLE_T1 {
        return 0;
    }
    let at = give_at(n, region);
    let Some((buf, _)) = cut_buf(n, region) else {
        give_tried(n, "hashagg-t1", GIVE_NOT_HOME, 0, at);
        return 0;
    };
    let (c, t) = (buf.committed(), region as *mut T1);
    if !buf.is_reserved() {
        give_tried(n, "hashagg-t1", GIVE_NOT_RESERVED, 0, at);
        return 0;
    }
    if c == 0 {
        give_tried(n, "hashagg-t1", GIVE_NO_GROWTH, 0, at);
        return 0;
    }
    let bb = (*t).block_bytes as usize;
    let kept = ((*t).nblocks as usize).saturating_sub(c / bb.max(1));
    if (*t).pool_off as usize + kept * bb > buf.len() || !T1::shrink_pool(t, c as u64) {
        give_tried(n, "hashagg-t1", GIVE_NO_FIT, 0, at);
        return 0;
    }
    let given = buf.give_back_committed() as u64;
    REVOKE_GIVEN.fetch_add(given, core::sync::atomic::Ordering::Relaxed);
    // REVOKESLA: the bytes the ask this participant serviced freed
    mcx::revoke::sla_given(n.revoke_seen, given);
    give_tried(n, "hashagg-t1", GIVE_GIVEN, given, at);
    note_spillable(n);
    given
}

/// D4265: the length a CUT kind-7 build at `region` (lowerv4's `TABLE_V4BUILD`, the region this participant fills) shrinks
/// to before it gives its growth back (`give_back_build`): its buffer's own length (the share or start it opened at, before
/// any commit in place). 0 when it cannot give back: its home is not cut (the caller's spill is as before), its region is
/// no reservation grown in place at that base, or nothing was committed past it (counted: `give_tried`).
/// # Safety — as `revoke_home`; `region` is the build the calling owner fills.
pub(crate) unsafe fn build_keep(n: &mut Inner, region: *const u8) -> u64 {
    if !home_cut(n) || gravity_abi::breaker::table_kind(region) != gravity_abi::breaker::TABLE_V4BUILD {
        return 0;
    }
    let at = give_at(n, region);
    let Some((buf, _)) = cut_buf(n, region) else {
        give_tried(n, "hashbuild", GIVE_NOT_HOME, 0, at);
        return 0;
    };
    if !buf.is_reserved() {
        give_tried(n, "hashbuild", GIVE_NOT_RESERVED, 0, at);
        return 0;
    }
    if buf.committed() == 0 {
        give_tried(n, "hashbuild", GIVE_NO_GROWTH, 0, at);
        return 0;
    }
    buf.len() as u64
}

/// D4265: a cut kind-7 build whose spill (lowerv4 `spill_grow`) left every kept record and image within `keep` bytes
/// (`build_keep`'s length; `fits`: false when even every partition out did not, counted) gives back what it committed past
/// it: the home's length and the region's armed limit (offset 8) become `keep`, and the pages past it leave with their
/// holds (`RegionBuf::give_back_committed`). The cut never lifts, so the build spills at that length from then on, as
/// PostgreSQL's hash join doubles its batches inside a fixed hash_mem. The bytes given back.
/// # Safety — as `revoke_home`; `region` is the build the calling owner fills, nothing of it is read past `keep` from now.
pub(crate) unsafe fn give_back_build(n: &mut Inner, region: *mut u8, keep: u64, fits: bool) -> u64 {
    let at = give_at(n, region);
    if !fits {
        give_tried(n, "hashbuild", GIVE_NO_FIT, 0, at);
        return 0;
    }
    let Some((buf, bytes)) = cut_buf(n, region) else { return 0 };
    if keep != buf.len() as u64 || !buf.is_reserved() {
        return 0;
    }
    *bytes = keep as u32;
    HashAgg::arm(region, keep as u32);
    let given = buf.give_back_committed() as u64;
    REVOKE_GIVEN.fetch_add(given, core::sync::atomic::Ordering::Relaxed);
    mcx::revoke::sla_given(n.revoke_seen, given);
    give_tried(n, "hashbuild", GIVE_GIVEN, given, at);
    note_spillable(n);
    given
}

/// Sort give-back (D4291, D4265's sort): the one step both askers take, a refused no-spill open (`Node::put_own_sorts`)
/// and a revoke (`Node::give_back_sorts`). A reserved region whose sealed sort run just went to its stream through the
/// sort's own put road (`SpillTable::run_put`), none of its readers begun, gives its bytes past `keep` back
/// (`RegionBuf::give_back_past`: the committed growth, a warm take's length); the caller lays the region again at `keep`.
/// `keep` is at least a run header and at most the region's length. The bytes given back.
/// A growing sort's region is not given back mid-section (a revoke cuts it: it stops growing and spills from then on; its
/// pages leave at its park, `RegionBuf::uncommit`): pages a pool worker's share holds past `keep` can still be read by a
/// later pipeline of its section (D4341: a PROT_NONE probe's core), so no point inside the section is known to be pointer-free.
pub(crate) fn give_back_sorted(buf: &mut RegionBuf, keep: usize) -> usize {
    let keep = keep.max(gravity_abi::breaker::SORTRUN_HDR).min(buf.len()) & !7;
    if !buf.is_reserved() || buf.past_bytes(keep) == 0 {
        return 0;
    }
    buf.give_back_past(keep)
}

/// C5: the bytes a cut of the region this participant fills gives back now, when its breaker has a spill road (a home with
/// a ceiling whose breaker spills past its grant: its refused ask spills), else 0. They weigh the victim choice and size
/// the ask; the cut itself is whole. A debt home (a ceiling, but no spill road) is no revoke target: asked, it gives
/// nothing back. D4314 (b): nor is a breaker whose cut keeps its pages (`mempolicy::reclaim::cut_gives_back`: a sort's
/// run, an owned table), nor a region whose table is not laid out yet: its bytes are noted once its kind byte names a
/// giving kind (the note after its next room ask). D4394: a giving kind's bytes are its reservation's committed growth
/// (what `give_back_scattered` / `give_back_build` return); a region at its share (an allocator region, or a reservation
/// that has not grown) offers none.
/// # Safety — as `revoke_home`.
unsafe fn home_spillable(n: &Inner) -> u64 {
    if !mempolicy::reclaim::cut_gives_back(cut_kind(n)) {
        return 0;
    }
    let grown = |b: &RegionBuf| if b.is_reserved() { b.committed() as u64 } else { 0 };
    match n.home {
        Home::Slot(r) if (*r).ceil != 0 && (*r).spills => grown(&(*r).buf),
        Home::Worker(s) if (*s).share_ceil != 0 && (*s).share_spills => crate::slots::share_buf_of(s).map_or(0, |b| grown(&*b)),
        _ => 0,
    }
}

/// C5: this participant's spillable bytes (`home_spillable`) noted on its statement's revoke handle
/// (`mcx::revoke::note_spillable`), as the change since its last note for the same statement. Run after its room asks, at
/// each claim's start and when a section unbinds its home; nothing while `pgrust.memory_revoke` is off.
/// # Safety — as `revoke_home`.
pub(crate) unsafe fn note_spillable(n: &mut Inner) {
    // ladder 1.3: under `fair_share` noted whatever `pgrust.memory_revoke` says
    if !mcx::revoke::enabled() && !mcx::revoke::fair_knob() {
        return;
    }
    let stmt = mcx::revoke::statement();
    let now = if stmt == 0 { 0 } else { home_spillable(n) };
    let (word, was) = n.spill_noted;
    let was = if word == stmt { was } else { 0 };
    n.spill_noted = (stmt, now);
    if stmt != 0 {
        mcx::revoke::note_spillable(now as i64 - was as i64);
    }
}

/// C5: the leader's spillable bytes noted (`note_spillable`) and its home cut once per revoke (`revoke_home`), between
/// calls (a claim's start).
pub(crate) fn revoke_leader(n: &mut Inner) -> bool {
    // SAFETY: the leader's own `Inner` (`Node::revoke_home`), on its thread with no call running: its home is one of its
    // own slots or none
    unsafe {
        note_spillable(n);
        revoke_home(n)
    }
}

/// C5: `revoke_home` for the kernels' poll on a set `REVOKE_BIT` (exec glue `poll_kernel`): the participant `ctx` is bound
/// to (the leader's own run or a pool worker's claim).
/// # Safety — `ctx` is bound, on the thread running its call.
pub unsafe fn revoke_poll(ctx: *mut gravity_abi::Ctx) -> bool {
    use gravity_abi::PtrWord;
    // a ctx no program block binds (a host loop's own), or a block with no participant, has no region to cut
    let block = (*ctx).program.ptr();
    if block.is_null() || (*block).env.ptr().is_null() {
        return false;
    }
    revoke_home(crate::hostimpl::inner_of(ctx))
}

/// `Fault::refuse` on the region the calling owner's pipeline produces (`home_ceil`'s).
/// # Safety — as `region_grow`.
unsafe fn home_refuses(n: &Inner) -> bool {
    match n.home {
        Home::Slot(r) => with_fault(&(*r).fault, |f| f.refuse()),
        Home::Worker(s) => (*s).share_fault.refuse(),
        Home::None => false,
    }
}

/// The ceiling of the region the calling owner's pipeline produces (`SlotRegion::ceil`, `WorkerSlot::share_ceil`); 0 = none.
/// # Safety — as `region_grow`.
unsafe fn home_ceil(n: &Inner) -> u64 {
    match n.home {
        Home::Slot(r) => (*r).ceil,
        Home::Worker(s) => (*s).share_ceil,
        Home::None => 0,
    }
}

/// D4080: the most a region reserved at `reserve` bytes grows to in place under its ceiling `ceil` (0 = none): the
/// reservation, held to the ceiling's region bytes (`ceil_cap`). A reservation can be a parked mapping a larger ask left
/// (`take_reserved` serves any at least the ask), and a breaker grows into it only up to its own ceiling, then spills.
pub(crate) fn in_place_cap(reserve: u64, ceil: u64) -> u64 {
    if ceil == 0 { reserve } else { reserve.min(ceil_cap(ceil)) }
}

/// D2894: the leader slot `r` (`len` bytes in use) grown in place within its reservation (`grow_in_place`): `grown_len`'s
/// length (`exact_len`'s when `exact`), at most the reservation when the need fits it (`in_place_cap` under the ceiling
/// `ceil`, 0 = none); `Some((true, new))` with `r.bytes` = new, `Some((false, new))` refused (nothing changed), `None` when
/// `r` is no reservation the need fits, or the need passes the ceiling's cap (the caller reopens, or takes its ceiling road).
/// # Safety — `r` is a live slot of the running statement, on the leader's thread.
unsafe fn slot_in_place(r: &mut SlotRegion, len: u64, need: u64, head: u64, tail: u64, w: Option<&mut u64>, stmt: Option<&Reservation>, spills: bool, exact: bool, ceil: u64) -> Option<(bool, u64)> {
    if !r.buf.is_reserved() || r.ptr != r.buf.as_mut_ptr() {
        return None;
    }
    let want = head.saturating_add(tail).saturating_add(need);
    let cap = in_place_cap(r.buf.reserve() as u64, ceil);
    if ceil != 0 && want > cap {
        return None;
    }
    let new = next_len(len, need, head, tail, exact)?.min(cap.max(want));
    let ok = grow_in_place(&mut r.buf, r.ptr, len, new, tail, w, stmt, false, spills)?;
    if ok {
        r.bytes = new as u32;
    }
    Some((ok, new))
}

/// The leader slot `r` reopened (`grown_len`, `exact_len` when `exact`); its old buffer back to the depot (its charge to the
/// pool: `put_from_life`). `arm`: the u32 limit at offset 8 re-armed to the new length (`HashAgg::arm`, what the IR's tables
/// read).
/// # Safety — `r` is a live slot of the running statement, on the leader's thread.
unsafe fn grow_slot(r: &mut SlotRegion, need: u64, head: u64, tail: u64, w: &mut u64, stmt: Option<&Reservation>, grew: &mut Grew, arm: bool, exact: bool) -> Result<*mut u8> {
    let len = r.bytes as u64;
    let new = next_len(len, need, head, tail, exact).ok_or_else(|| max_region(head.saturating_add(tail).saturating_add(need)))?;
    // D2894: a floor reservation (or a set operation side's) grows in place, no reopen, no copy past its tail
    match slot_in_place(r, len, need, head, tail, Some(&mut *w), stmt, false, exact, 0) {
        Some((true, new)) => {
            if arm {
                HashAgg::arm(r.ptr, new as u32);
            }
            grew.note(len, new);
            return Ok(r.ptr);
        }
        Some((false, new)) => return Err(oom(new - len)),
        None => {}
    }
    let (mut buf, base) = reopen(r.ptr, len as usize, head as usize, tail as usize, new as usize, w, stmt, false)?;
    if let Some(o) = r.owner {
        buf.own_by(o); // LEDGER2 step 5: as region_grow_capped's
    }
    if arm {
        HashAgg::arm(base, new as u32);
    }
    crate::park::REGIONS.put_from_life(core::mem::replace(&mut r.buf, buf), stmt);
    r.ptr = base;
    r.bytes = new as u32;
    grew.note(len, new);
    Ok(base)
}

/// C10: the grows of a spilling breaker's region (`SlotRegion::spills`, `WorkerSlot::share_spills`) on the uncapped road
/// (`SPILL_REGION_GROW`, `State::grow`) past its grant (its ceiling, else W and the leader's G), this process: `[0]` on a
/// road no lowering named (a bug key: the breaker declared it spills), `[1]` on a road a SPILL-DEBT row names
/// (`region_grow_debt`, `State::grow_debt`), or of a region whose breaker names one (NR-1: `SlotRegion::debt`,
/// `WorkerSlot::share_debt`). Exec's runner reports a statement's (`uncapped_line`).
pub static UNCAPPED: [core::sync::atomic::AtomicU64; 2] = [core::sync::atomic::AtomicU64::new(0), core::sync::atomic::AtomicU64::new(0)];
/// The last unnamed one's `[slot, bytes asked past the grant]`, for the report line.
pub static UNCAPPED_LAST: [core::sync::atomic::AtomicU64; 2] = [core::sync::atomic::AtomicU64::new(0), core::sync::atomic::AtomicU64::new(0)];
/// NR-1: `UNCAPPED[1]` by the SPILL-DEBT row that named the grow (`NoSpill::code`), this process.
pub static NAMED: [core::sync::atomic::AtomicU64; NoSpill::ALL.len()] = [const { core::sync::atomic::AtomicU64::new(0) }; NoSpill::ALL.len()];

/// C10: `UNCAPPED` now, a statement's start for `uncapped_line`.
pub fn uncapped_at() -> [u64; 2] {
    core::array::from_fn(|i| UNCAPPED[i].load(core::sync::atomic::Ordering::Relaxed))
}

/// C10: the uncapped grows past the grant since `at` (`uncapped_at` at a statement's start), for its one report line (exec
/// execute.rs, under PGRUST_GRAVITY_REPORT): `[unnamed, named, the last unnamed one's slot, its bytes past the grant]`.
pub fn uncapped_since(at: [u64; 2]) -> [u64; 4] {
    use core::sync::atomic::Ordering::Relaxed;
    let now = uncapped_at();
    [now[0].saturating_sub(at[0]), now[1].saturating_sub(at[1]), UNCAPPED_LAST[0].load(Relaxed), UNCAPPED_LAST[1].load(Relaxed)]
}

/// NR-1: `NAMED` now, a statement's start for `named_since`.
pub fn named_at() -> [u64; NoSpill::ALL.len()] {
    core::array::from_fn(|i| NAMED[i].load(core::sync::atomic::Ordering::Relaxed))
}

/// NR-1: the named uncapped grows since `at` (`named_at` at a statement's start) by row, the report line's `named=`:
/// `<row>:<n>` comma-separated in code order, `0` for none.
pub fn named_since(at: [u64; NoSpill::ALL.len()]) -> String {
    named_field(named_at(), at)
}

fn named_field(now: [u64; NoSpill::ALL.len()], at: [u64; NoSpill::ALL.len()]) -> String {
    use core::fmt::Write;
    let mut out = String::new();
    for ((r, now), at) in NoSpill::ALL.into_iter().zip(now).zip(at) {
        if now > at {
            let _ = write!(out, "{}{r:?}:{}", if out.is_empty() { "" } else { "," }, now - at); // alloc: Cold
        }
    }
    if out.is_empty() {
        out.push('0'); // alloc: Cold
    }
    out
}

/// C10: a spilling (NR-1: or debt-naming) breaker's region of slot `slot` grown to `new` bytes on the uncapped road, its
/// ceiling `ceil` (0: none), its share `w` and, on the leader, the statement's G left (`left`): past them it is counted,
/// named by `debt` (by row in `NAMED`) or not. An unnamed one fires `breaker.spills_uncapped`, and is a Bug in a debug
/// build (the lowering named no such road). NR-6: the row a named one is counted as (`debt_draw`), else `None`.
fn note_uncapped(slot: u8, new: u64, ceil: u64, w: u64, left: Option<u64>, debt: Option<NoSpill>) -> Option<NoSpill> {
    use core::sync::atomic::Ordering::Relaxed;
    let cap = uncapped_cap(ceil, w, left);
    if new <= cap {
        return None;
    }
    if let Some(d) = debt {
        UNCAPPED[1].fetch_add(1, Relaxed);
        NAMED[d.code() as usize].fetch_add(1, Relaxed);
        return Some(d);
    }
    UNCAPPED[0].fetch_add(1, Relaxed);
    UNCAPPED_LAST[0].store(slot as u64, Relaxed);
    UNCAPPED_LAST[1].store(new - cap, Relaxed);
    fires::fire_shared("breaker.spills_uncapped");
    debug_assert!(false, "gravity: slot {slot}'s spilling breaker grew to {new} bytes past its grant ({cap}) on an unnamed uncapped road (C10)");
    None
}

/// `note_uncapped`'s cap: the ceiling's region bytes (`ceil` 0: none), else the larger of the share `w` and the G left.
fn uncapped_cap(ceil: u64, w: u64, left: Option<u64>) -> u64 {
    if ceil != 0 { ceil_cap(ceil) } else { w.max(left.unwrap_or(0)) }
}

/// NR-6: a draw of `bytes` counted past its region's cap as the SPILL-DEBT row `named` (`note_uncapped`), refused when
/// the region's fault holds the debt lever (`refuses`: `Fault::debt`) as the pool refuses one: the statement's out of
/// memory, its row named (`debt_refused`).
fn debt_draw(named: Option<NoSpill>, refuses: bool, bytes: u64) -> Result<()> {
    match named {
        Some(row) if refuses => Err(debt_refused(oom(bytes), None, Some(row))),
        _ => Ok(()),
    }
}

/// NR-6: whether the region the calling owner's pipeline produces (`home_ceil`'s) holds the debt lever (`Fault::debt`).
/// # Safety — as `region_grow`.
unsafe fn home_debt_lever(n: &Inner) -> bool {
    match n.home {
        Home::Slot(r) => (*r).fault.get().debt,
        Home::Worker(s) => (*s).share_fault.debt,
        Home::None => false,
    }
}

/// NR-1: how an uncapped grow of a home is counted: a spilling home's as its road names it (`road`; none: unnamed), a home
/// whose breaker names a SPILL-DEBT row (`home`) as the road's row, else its own; `None` (not counted) for a home that
/// neither spills nor names a row (a program constant, a form PostgreSQL holds too: no road of either grows past a grant).
fn counted_as(spills: bool, home: Option<NoSpill>, road: Option<NoSpill>) -> Option<Option<NoSpill>> {
    match home {
        _ if spills => Some(road),
        Some(h) => Some(Some(road.unwrap_or(h))),
        None => None,
    }
}

/// C10: `note_uncapped` for the calling owner's produced region asked `need` more (`region_grow`'s arguments) on the
/// uncapped road, `debt` the road's named debt (`SPILL_REGION_GROW_DEBT`); nothing for a region `counted_as` skips. NR-6:
/// a named one the debt lever refuses (`debt_draw`) is the grow's error, before it draws.
/// # Safety — as `region_grow`.
pub(crate) unsafe fn uncapped_op(n: &Inner, need: u64, head: u64, tail: u64, debt: Option<NoSpill>) -> Result<()> {
    let (spills, home, len, leader) = match n.home {
        Home::Slot(r) => ((*r).spills, (*r).debt, (*r).bytes as u64, true),
        Home::Worker(s) => ((*s).share_spills, (*s).share_debt, (*s).state_bytes as u64, false),
        Home::None => return Ok(()),
    };
    let Some(debt) = counted_as(spills, home, debt) else { return Ok(()) };
    let Some(new) = grown_len(len, need, head, tail) else { return Ok(()) };
    let left = if leader { n.stmt.as_ref().map(|s| s.left()) } else { None };
    debt_draw(note_uncapped(n.own_slot, new, home_ceil(n), n.w, left, debt), home_debt_lever(n), new)
}

/// C10: a pinned region (`hostimpl::arena` pin: a fresh pool chunk of `bytes`) drawn when
/// W and the leader's G held `held`, counted as a grow is past them (`note_uncapped`): any region pinned past W and G
/// counts, grown or pinned. `road` names it (an owner-phase breaker's owner pins, `arena::pin_owner`: `AggOwnerAll`);
/// an unnamed one counts for a spilling home only (the calling owner's produced region, `uncapped_op`'s).
/// # Safety — `n` is a bound owner, on the thread running it.
pub(crate) unsafe fn uncapped_pin(n: &Inner, bytes: u64, held: (u64, Option<u64>), road: Option<NoSpill>) {
    let spills = match n.home {
        _ if road.is_some() => true,
        Home::Slot(r) => (*r).spills,
        Home::Worker(s) => (*s).share_spills,
        Home::None => false,
    };
    if spills {
        note_uncapped(n.own_slot, bytes, 0, held.0, held.1, road);
    }
}

/// NR-6: whether the debt lever refuses a pin of `bytes` that `uncapped_pin` would count past W and G (`held`) as its
/// named road `road`: read before the draw, so the pin is refused as the pool refuses one. The lever is the home's
/// (`Fault::debt`) or, on the leader, its statement's (`Grant::refuse_debt`: an owner phase pins outside any home's
/// fault).
/// # Safety — as `uncapped_pin`.
pub(crate) unsafe fn debt_pin_refused(n: &Inner, bytes: u64, held: (u64, Option<u64>), road: Option<NoSpill>) -> bool {
    road.is_some() && (home_debt_lever(n) || n.stmt.as_ref().is_some_and(|s| s.grant.refuse_debt)) && bytes > uncapped_cap(0, held.0, held.1)
}

/// C10: `note_uncapped` for the leader slot behind `s` grown to `bytes` on the uncapped road (`State::grow`,
/// `State::grow_debt`). NR-6: a named one the debt lever refuses (`debt_draw`) is the grow's error, before it draws.
fn uncapped_state(s: &State<'_>, bytes: usize, debt: Option<NoSpill>) -> Result<()> {
    let Some(h) = s.home else { return Ok(()) };
    // SAFETY: the slot and the reservation belong to the Node this State borrows, read on the leader's thread.
    let (spills, home, len, ceil, left, lever) = unsafe { ((*h.region).spills, (*h.region).debt, (*h.region).bytes as u64, (*h.region).ceil, h.stmt.as_ref().map(|r| r.left()), (*h.region).fault.get().debt) };
    let Some(debt) = counted_as(spills, home, debt) else { return Ok(()) };
    if bytes as u64 <= len {
        return Ok(());
    }
    let new = grown_len(len, bytes as u64 - len, len, 0).unwrap_or(bytes as u64);
    debt_draw(note_uncapped(h.slot, new, ceil, 0, left, debt), lever, new)
}

/// `Program.spill(ctx, SPILL_REGION_GROW, &[need, head, tail])`: the calling owner's produced region reopened; its new base.
/// # Safety — `n` is a bound owner inside a Call, on the thread running it.
pub(crate) unsafe fn region_grow(n: &mut Inner, need: u64, head: u64, tail: u64) -> Result<u64> {
    match n.home {
        Home::Slot(r) => {
            let stmt = n.stmt.as_ref();
            let base = grow_slot(&mut *r, need, head, tail, &mut n.w, stmt, &mut n.grew, true, false).map_err(|e| debt_refused(e, (*r).debt, None))?;
            (*n.partials).own = gravity_abi::word(base);
            Ok(base as u64)
        }
        Home::Worker(s) => crate::slots::grow_partial(s, n, need, head, tail).map(|b| b as u64).map_err(|e| debt_refused(e, (*s).share_debt, None)),
        Home::None => Err(Error::host("gravity: region_grow from a pipeline that produces no region")),
    }
}

/// The setop posture (option c'): the reservation a `Budget::Pool` breaker's region of `share` bytes binds in: the share plus
/// the side's rows bound (every row it receives its own group: `cost::breaker::rows_bytes` of the planner's rows and the
/// record), at most `MAX_REGION`. Address space only: the allocator's pages cost nothing until the fold commits and
/// touches them. 0 for any other breaker, or when the bound fits the share (the region is the share, as today).
pub(crate) fn pool_reserve(pipe: &gravity_program::Pipeline, share: u64) -> u64 {
    if !pipe.breaker().is_some_and(|b| b.budget == gravity_abi::breaker::Budget::Pool) {
        return 0;
    }
    let Some(c) = gravity_program::cost::breaker::of(pipe) else { return 0 };
    let bound = gravity_program::cost::breaker::rows_bytes(c.rows, c.record);
    if bound <= share {
        return 0;
    }
    (crate::park::region_bytes(share.saturating_add(bound).min(MAX_REGION) as usize) as u64).min(MAX_REGION)
}

/// `Program.spill(ctx, SPILL_REGION_COMMIT, need)` (option c'): the calling owner's produced region's reservation (`need` 0;
/// 0 = the region is not reserved; a share opened smaller adds its section's share word, D4033), or `need` more bytes of
/// it committed in place, drawn as past-share growth (W, then G, then pool bytes past both: `road_held`) and counted as a
/// grow; 0 past the reservation or when the draw is refused.
/// # Safety — `n` is a bound owner inside a Call, on the thread running it.
pub(crate) unsafe fn region_commit(n: &mut Inner, need: u64) -> Result<u64> {
    let (buf, worker, cut): (&mut crate::node::RegionBuf, bool, u64) = match n.home {
        Home::Slot(r) if (*r).ptr == (*r).buf.as_mut_ptr() => (&mut (*r).buf, false, 0),
        Home::Worker(s) => match crate::slots::share_buf_of(s) {
            Some(b) => (b, n.w_only, (*s).share_cut as u64),
            None => return Ok(0),
        },
        _ => return Ok(0),
    };
    if !buf.is_reserved() {
        // D4033: a worker share the pool's room opened smaller carries its section's share (no reservation bytes), so
        // kernels `fold_bits` cut its partitions as the other workers' are: the owner phase reads all at one count
        return Ok(if need == 0 { gravity_abi::program::reserve_share_word(cut) } else { 0 });
    }
    if need == 0 {
        // D2894: a floor's answer carries `RESERVE_FLOOR` (its T1 commits before it compacts)
        // D3809: a laid region's answer carries the share its partitions are cut by (kernels `fold_bits`); D4033: so does
        // a worker share the pool's room opened smaller (`WorkerSlot::share_cut`)
        let share = if buf.is_laid() { buf.lay_share() as u64 } else { cut };
        return Ok(buf.lay_reserve() as u64 | if buf.is_floor() { gravity_abi::program::RESERVE_FLOOR } else { 0 } | if buf.is_laid() { gravity_abi::program::RESERVE_LAYOUT } else { 0 } | gravity_abi::program::reserve_share_word(share));
    }
    // the forced-spill test fault: a floor's commit refused as at its ceiling (never a set operation side's: its refusal
    // is 53200)
    if buf.is_floor() && home_refuses(n) {
        return Ok(0);
    }
    let from = (buf.len() + buf.committed()) as u64;
    if from.saturating_add(need) > buf.lay_reserve() as u64 {
        // a ceiling's reservation (`floor_reserve`) full: the breaker spills at its work_mem budget
        if buf.is_floor() && home_ceil(n) != 0 {
            at_ceiling();
        }
        return Ok(0);
    }
    let stmt = n.stmt.as_ref();
    if stmt.is_some_and(|s| s.region_draw().is_err()) {
        return Ok(0);
    }
    // D2894: a floor's pool (a hash aggregate's T1: a spilling kind) takes pool bytes only if the pool has them now; a
    // refusal compacts, then spills (`road_held_now`). A set operation side's waits as before (its refusal is 53200)
    let held = if buf.is_floor() { crate::node::road_held_now(need as usize, Some(&mut n.w), stmt, worker) } else { crate::node::road_held(need as usize, Some(&mut n.w), stmt, worker) };
    let Some(held) = held else { return Ok(0) };
    if !buf.commit(need as usize, held) {
        return Ok(0);
    }
    n.grew.note(from, from + need);
    GROWN_DRAWS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    fire_commit(buf);
    Ok(need)
}

/// The shapecheck fire of a commit in place: a set operation side's (option c') or a floor reservation's (D2894).
fn fire_commit(buf: &crate::node::RegionBuf) {
    if buf.is_floor() {
        fires::fire_shared("breaker.grow_in_place"); // shapecheck: a breaker past its modeled share committed more of its floor in place (D2894)
    } else {
        fires::fire_shared("setop.grow_in_place"); // shapecheck: the side's fold committed more of its reservation in place (option c')
    }
}

/// D2894: the reservation a floor-carrying breaker's region of `share` bytes binds in: its kind's PostgreSQL budget
/// (D2587's floor held to a row top's bound, `grant::floor_for`) when that is past the share, at most `MAX_REGION`; 0
/// otherwise. The share is what the one divisor charged (the floor up to the breaker's modeled need, `grant::share_with`);
/// the rest of the floor is address space the region commits in place as it fills (`grow_in_place`), as PostgreSQL takes
/// work_mem / hash_mem as it fills. A breaker with a ceiling (`grant::ceiling_of`) binds in a reservation of its ceiling,
/// whatever its grant's floors (a trimmed grant has none): what it may hold before it spills, a worker its own, the leader
/// (`leader`) its node's at `width` (`grant::leader_ceiling`). Without one: `width` is the leader's 1, a
/// worker's its section's width (exactsize D3215: a partitioned record table's worker reserves its cut of the floor,
/// `grant::worker_floor`).
pub(crate) fn floor_reserve(pipe: &gravity_program::Pipeline, grant: Option<crate::mem::grant::Grant>, share: u64, width: u16, leader: bool) -> u64 {
    let (Some(_), Some(g)) = (pipe.breaker(), grant) else { return 0 };
    let floor = match crate::mem::grant::ceiling_of(g, pipe) {
        0 => crate::mem::grant::worker_floor(pipe, crate::mem::grant::floor_for(g, pipe), if leader { 1 } else { width }),
        c if leader => pipe.breaker().map_or(c, |b| crate::mem::grant::leader_ceiling(c, width, b.kind)),
        c => c,
    }
    .min(MAX_REGION);
    if floor <= share {
        return 0;
    }
    (crate::park::region_bytes(floor as usize) as u64).min(MAX_REGION)
}

/// The reservation a started-small region (`grant::starts_small`) of `share` bytes binds in: the share itself, its layout,
/// or its kind's PostgreSQL floor where that is more (`floor_reserve` at `width`), at most `MAX_REGION`. Its region commits
/// `grant::start_bytes` of it and grows in place.
pub(crate) fn start_reserve(pipe: &gravity_program::Pipeline, grant: Option<crate::mem::grant::Grant>, share: u64, width: u16, leader: bool) -> u64 {
    let lay = (crate::park::region_bytes(share.min(MAX_REGION) as usize) as u64).min(MAX_REGION);
    lay.max(floor_reserve(pipe, grant, share, width, leader))
}

/// The reservation a shared region of `share` bytes binds in (0 = none): a set operation side's rows bound (option c',
/// `pool_reserve`) or a breaker's PostgreSQL floor or ceiling (D2894, `floor_reserve` at `width`, the leader's when
/// `leader`), the larger; and whether it is the floor.
pub(crate) fn share_reserve(pipe: &gravity_program::Pipeline, grant: Option<crate::mem::grant::Grant>, share: u64, width: u16, leader: bool) -> (u64, bool) {
    let (pool, floor) = (pool_reserve(pipe, share), floor_reserve(pipe, grant, share, width, leader));
    if floor > pool { (floor, true) } else { (pool, false) }
}

/// D2894: a reserved region (`buf`, its bytes at `base`, `len` of them in use) grown to `new` IN PLACE: the bytes past what
/// it has committed drawn as past-share growth (W, then G, then pool bytes past both: `road_held`, or `road_held_now`
/// when `spills`: the caller spills on a refusal), its last `tail` bytes moved to the new end, the base unchanged. `None`
/// when the region is no reservation of that base or `new` passes it (the caller reopens as before); `Some(false)` when
/// the draw (or the kernel's protection) is refused, nothing changed.
/// # Safety — `base` holds `len` live bytes of `buf`, on the thread that owns it.
pub(crate) unsafe fn grow_in_place(buf: &mut crate::node::RegionBuf, base: *mut u8, len: u64, new: u64, tail: u64, w: Option<&mut u64>, stmt: Option<&Reservation>, worker: bool, spills: bool) -> Option<bool> {
    if !buf.is_reserved() || base != buf.as_mut_ptr() || new > buf.reserve() as u64 || new <= len || tail > len {
        return None;
    }
    // D4373 (c): a canceled statement grows nothing: refused, the caller's road runs (a sort puts its run, whose put polls
    // the cancel and stops), never another doubling of pages the scan then fills before its next poll
    if cancelpoll::step("grow in place: a region's doubling").tick().is_err() {
        return Some(false);
    }
    let have = (buf.len() + buf.committed()) as u64;
    if new > have {
        let need = (new - have) as usize;
        if stmt.is_some_and(|s| s.region_draw().is_err()) {
            return Some(false);
        }
        let held = if spills { crate::node::road_held_now(need, w, stmt, worker) } else { crate::node::road_held(need, w, stmt, worker) };
        let Some(held) = held else { return Some(false) };
        if !buf.commit(need, held) {
            return Some(false);
        }
        GROWN_DRAWS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        fire_commit(buf);
    }
    if tail != 0 {
        core::ptr::copy(base.add((len - tail) as usize), base.add((new - tail) as usize), tail as usize);
    }
    Some(true)
}

/// The owner phase's draws, process-wide, for the report (`draws`): exact (`commit_exact`, one counted draw
/// taken once) and grown (a commit in place on demand: `region_commit`, `grow_in_place`).
pub static EXACT_DRAWS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static GROWN_DRAWS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// (exact, grown) draws so far (`EXACT_DRAWS`, `GROWN_DRAWS`).
pub fn draws() -> (u64, u64) {
    (EXACT_DRAWS.load(core::sync::atomic::Ordering::Relaxed), GROWN_DRAWS.load(core::sync::atomic::Ordering::Relaxed))
}

/// A reserved region (`buf`, its bytes at `base`) committed in place up to `new` in ONE exact draw
/// (`road_held_exact`: W, G, else the pool directly, waiting for its room), never through the past-G grow road. False
/// (nothing changed) when it is no reservation of that base, `new` passes it, or the pool stayed short.
/// # Safety — `base` is `buf`'s, on the thread that owns it.
pub(crate) unsafe fn commit_exact(buf: &mut crate::node::RegionBuf, base: *mut u8, new: u64, w: Option<&mut u64>, stmt: Option<&Reservation>) -> bool {
    if !buf.is_reserved() || base != buf.as_mut_ptr() || new > buf.reserve() as u64 {
        return false;
    }
    let have = (buf.len() + buf.committed()) as u64;
    if new <= have {
        return true;
    }
    let need = (new - have) as usize;
    if stmt.is_some_and(|s| s.region_draw().is_err()) {
        return false;
    }
    let Some(held) = crate::node::road_held_exact(need, w, stmt) else { return false };
    if !buf.commit(need, held) {
        return false;
    }
    EXACT_DRAWS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    fire_commit(buf);
    true
}

/// The exact draw's witness: a laid reservation of `reserve` bytes holding `start` under `stmt`, committed in one exact
/// draw to `new` (`commit_exact`, the owner phase's `owners_in_place`); whether it committed. Refused, nothing is drawn.
#[doc(hidden)]
pub fn exact_commit_witness(start: usize, reserve: usize, new: u64, stmt: &Reservation) -> bool {
    let mut buf = crate::mem::region::laid_region(start, reserve, start, None, Some(stmt)).expect("a laid region");
    let base = buf.as_mut_ptr();
    // SAFETY: `buf` is this thread's own reservation and `base` its base; nothing else reads it
    unsafe { commit_exact(&mut buf, base, new, None, Some(stmt)) }
}

/// `Program.spill(ctx, SPILL_REGION_PLACE, &[need, head])`: the calling owner's produced region, when it started small
/// (`RegionBuf::is_laid`: `grant::starts_small`), grown IN PLACE (`grow_in_place`: W, then G, then the pool's room now, no
/// wait) to `place_len` and re-armed at offset 8: its new length. 0 for any other region, a need past the reservation
/// (counted at the breaker's ceiling: `at_ceiling`), or no room now: nothing drawn, and the sort run that asked is put as
/// before (PostgreSQL's Sort spills past work_mem). Never moves the base: the IR holds the run's address.
/// # Safety — `n` is a bound owner inside a Call, on the thread running it.
pub(crate) unsafe fn region_grow_place(n: &mut Inner, need: u64, head: u64) -> u64 {
    // D4373 (c): a canceled statement's run puts (its put polls and stops) instead of doubling in place
    if home_refuses(n) || cancelpoll::step("spill region place: a sort run's growth").tick().is_err() {
        return 0;
    }
    let ceil = home_ceil(n);
    let (buf, base, len): (&mut RegionBuf, *mut u8, u64) = match n.home {
        Home::Slot(r) => (&mut (*r).buf, (*r).ptr, (*r).bytes as u64),
        Home::Worker(s) => match crate::slots::share_buf_of(s) {
            Some(b) => (b, (*s).state_ptr, (*s).state_bytes as u64),
            None => return 0,
        },
        Home::None => return 0,
    };
    let Some(new) = place_len(buf, len, need, head) else {
        if buf.is_laid() && ceil != 0 {
            at_ceiling();
        }
        return 0;
    };
    let worker = matches!(n.home, Home::Worker(_)) && n.w_only;
    if grow_in_place(buf, base, len, new, 0, Some(&mut n.w), n.stmt.as_ref(), worker, true) != Some(true) {
        return 0;
    }
    HashAgg::arm(base, new as u32);
    match n.home {
        Home::Slot(r) => (*r).bytes = new as u32,
        Home::Worker(s) => (*s).state_bytes = new as u32,
        Home::None => {}
    }
    n.grew.note(len, new);
    fires::fire_shared("sort.grow_in_place"); // shapecheck: a sort run's started-small region grew in place at its full instead of putting the run
    new
}

/// The length a started-small region of `len` bytes grows to in place for `head + need` bytes: `grown_len`'s (twice its
/// length at least), at most the reservation its taker laid (`lay_reserve`: the breaker's ceiling, never a parked
/// mapping's larger one); `None` for a region that did not start small or when `head + need` passes the reservation.
fn place_len(buf: &RegionBuf, len: u64, need: u64, head: u64) -> Option<u64> {
    let lay = buf.lay_reserve() as u64;
    if !buf.is_laid() || head.checked_add(need)? > lay {
        return None;
    }
    grown_len(len, need, head, 0).map(|n| n.min(lay)).filter(|&n| n > len)
}

/// M7-SPILL (V4R-359, decision 5 (a)): `region_grow` only within the owner's limit: its share W, or for the leader the
/// statement's G; `None` (nothing drawn, no error) when the grown length fits neither, so a kind-7 build spills instead.
/// # Safety — as `region_grow`.
pub(crate) unsafe fn region_grow_in(n: &mut Inner, need: u64, head: u64, tail: u64) -> Result<Option<u64>> {
    let (len, leader) = match n.home {
        Home::Slot(r) => ((*r).bytes as u64, true),
        Home::Worker(s) => ((*s).state_bytes as u64, false),
        Home::None => return Err(Error::host("gravity: region_grow from a pipeline that produces no region")),
    };
    if home_refuses(n) {
        return Ok(None);
    }
    let ceil = home_ceil(n);
    // D2894: a reserved region (a breaker's floor) commits in place up to its reservation before it spills, pool bytes past
    // W and G only if the pool has them now: PostgreSQL's hash join fills hash_mem before it batches
    match n.home {
        Home::Slot(r) => match slot_in_place(&mut *r, len, need, head, tail, Some(&mut n.w), n.stmt.as_ref(), true, false, ceil) {
            Some((true, new)) => {
                HashAgg::arm((*r).ptr, new as u32);
                (*n.partials).own = gravity_abi::word((*r).ptr);
                n.grew.note(len, new);
                return Ok(Some((*r).ptr as u64));
            }
            Some((false, _)) => return Ok(None),
            None => {}
        },
        Home::Worker(s) => match crate::slots::grow_partial_in_place(s, n, need, head, tail, true, ceil) {
            Some(Some(base)) => return Ok(Some(base as u64)),
            Some(None) => return Ok(None),
            None => {}
        },
        Home::None => {}
    }
    if ceil != 0 {
        return region_grow_capped(n, len, need, head, tail, ceil);
    }
    let Some(new) = grown_len(len, need, head, tail) else { return Ok(None) };
    let g = leader && n.stmt.as_ref().is_some_and(|s| s.left() >= new);
    if n.w < new && !g {
        return Ok(None);
    }
    region_grow(n, need, head, tail).map(Some)
}

/// D3105 (b) (`SPILL_REGION_GROW_IN`): `region_grow_in` never past both W and G, a reserved region's commit in place (D2894:
/// pool bytes past both when the pool has them now) too: its draw only within W, or the leader's G, else `None` (nothing
/// drawn): the merge join's Sort inner puts its run, as PostgreSQL's Sort spills past work_mem.
/// # Safety — as `region_grow`.
pub(crate) unsafe fn region_grow_within(n: &mut Inner, need: u64, head: u64, tail: u64) -> Result<Option<u64>> {
    let (buf, base, len, leader): (Option<&RegionBuf>, *const u8, u64, bool) = match n.home {
        Home::Slot(r) => (Some(&(*r).buf), (*r).ptr, (*r).bytes as u64, true),
        Home::Worker(s) => (crate::slots::share_buf_of(s).map(|b| &*b), (*s).state_ptr, (*s).state_bytes as u64, false),
        Home::None => return Err(Error::host("gravity: region_grow from a pipeline that produces no region")),
    };
    // under a ceiling the reserved region commits up to it from the pool's room now (`region_grow_in`), as a Sort fills
    // work_mem before it puts a run
    let capped = home_ceil(n) != 0;
    if let Some(buf) = buf.filter(|b| !capped && b.is_reserved() && core::ptr::eq(b.as_ptr(), base)) {
        let Some(g) = grown_len(len, need, head, tail) else { return Ok(None) };
        let new = g.min((buf.reserve() as u64).max(head.saturating_add(tail).saturating_add(need)));
        let draw = new.saturating_sub((buf.len() + buf.committed()) as u64);
        let in_g = leader && n.stmt.as_ref().is_some_and(|s| s.left() >= draw);
        if draw > 0 && n.w < draw && !in_g {
            return Ok(None);
        }
    }
    region_grow_in(n, need, head, tail)
}

/// An owner's grows counted in the Node's census; the statement's first logged (leader thread).
pub(crate) fn fold(g: &mut Grew, counters: &Counters, kind: Option<BreakerKind>) {
    let g = core::mem::take(g);
    if g.n == 0 {
        return;
    }
    counters.past_share.set(counters.past_share.get() + g.n as u64);
    counters.past_share_bytes.set(counters.past_share_bytes.get() + g.top.saturating_sub(g.share));
    if !counters.past_share_logged.replace(true) {
        log_past_share(kind, g.top, g.share);
    }
}

#[cold]
fn log_past_share(kind: Option<BreakerKind>, bytes: u64, share: u64) {
    match kind {
        Some(k) => eprintln!("gravity: {k:?} past its share: {bytes} of {share}"), // alloc: Cold
        None => eprintln!("gravity: region past its share: {bytes} of {share}"), // alloc: Cold
    }
}

/// The breaker kind of the pipeline that produces `slot` (the log line's name).
pub(crate) fn kind_of(program: &gravity_program::Program, slot: u8) -> Option<BreakerKind> {
    program.pipelines.iter().find_map(|p| p.breaker().filter(|b| b.slot == slot)).map(|b| b.kind)
}

impl Node {
    /// The leader's slot `slot` as a merge's `State` that can grow (`State::grow`): the width-1 arm's `finish`.
    pub fn leader_state<'a>(&'a mut self, slot: u8, sink: SinkRef<'a>) -> State<'a> {
        let home = self.leader_home(slot);
        State::growable(self.slot_region(slot), sink, home)
    }

    pub(crate) fn leader_home(&mut self, slot: u8) -> Leader {
        let kind = kind_of(self.program(), slot);
        Leader { region: &mut self.slots[slot as usize] as *mut SlotRegion, stmt: self.stmt_ptr(), counters: &self.counters as *const Counters, kind, spill: &mut *self.spill as *mut crate::mem::spill::SpillTable, slot }
    }
}

/// D3625: `State::hash_ceiling`: the statement's grant's hash ceiling (0 = none).
pub(crate) fn hash_ceiling(s: &State<'_>) -> u64 {
    // SAFETY: as `exact_build`'s: the reservation belongs to the Node this State borrows, read on the leader's thread.
    s.home.and_then(|h| unsafe { h.stmt.as_ref() }.map(|r| r.grant.hash_ceil)).unwrap_or(0)
}

/// CONVERT1: `State::op_owner`: the leader slot's ledger owner (`SlotRegion::owner`).
pub(crate) fn op_owner(s: &State<'_>) -> Option<memledger::Owner> {
    // SAFETY: as `hash_ceiling`'s: the slot belongs to the Node this State borrows, read on the leader's thread.
    s.home.and_then(|h| unsafe { h.region.as_ref() }.and_then(|r| r.owner))
}

/// `State::grow`: the leader's slot reopened at max(2 x len, bytes) when it is shorter than `bytes`, the whole old region copied,
/// the header untouched (the merge owns it); `state.bytes` the new slice.
pub(crate) fn grow_state(s: &mut State<'_>, bytes: usize) -> Result<()> {
    uncapped_state(s, bytes, None)?;
    grow_state_at(s, bytes, false)
}

/// C10: `State::grow_debt`: `grow_state` (`grow_exact`'s when `counted`) on a road the SPILL-DEBT row `why` names.
pub(crate) fn grow_state_debt(s: &mut State<'_>, bytes: usize, counted: Option<u64>, why: NoSpill) -> Result<()> {
    uncapped_state(s, bytes, Some(why))?;
    match counted {
        Some(n) => grow_exact_named(s, bytes, n),
        None => grow_state_at(s, bytes, false),
    }
    .map_err(|e| debt_refused(e, None, Some(why)))
}

/// exactsize B1a: whether the statement behind `s` sizes its hash builds from their count (`Grant::exact_build`).
pub(crate) fn exact_build(s: &State<'_>) -> bool {
    // SAFETY: the reservation belongs to the Node this State borrows (it outlives the State), read on the leader's thread.
    s.home.is_some_and(|h| unsafe { h.stmt.as_ref() }.is_some_and(|r| r.grant.exact_build()))
}

/// exactsize B1a: `State::grow_exact` / `grow_in_exact`: the region grown to the counted `bytes` (`exact_len`) when the
/// statement sizes from counts (`exact_build`), else `grow` / `grow_in`. One PHASES line per boundary under
/// PGRUST_GRAVITY_PHASES (`EXACT kind=build slot=<s> counted=<rows> need=<bytes> grown=<len> road=<...>`) and the shapecheck
/// fire `exact.build_boundary`; `within`: `grow_in`'s G-only draw (false = refused, nothing drawn).
pub(crate) fn grow_exact(s: &mut State<'_>, bytes: usize, counted: u64, within: bool) -> Result<bool> {
    if !within {
        uncapped_state(s, bytes, None)?;
    }
    grow_exact_at(s, bytes, counted, within)
}

/// C10: `grow_exact`'s uncapped form on a road a SPILL-DEBT row names (`grow_state_debt`, counted already).
fn grow_exact_named(s: &mut State<'_>, bytes: usize, counted: u64) -> Result<()> {
    grow_exact_at(s, bytes, counted, false).map(|_| ())
}

/// `grow_exact` past its uncapped check.
fn grow_exact_at(s: &mut State<'_>, bytes: usize, counted: u64, within: bool) -> Result<bool> {
    if !exact_build(s) {
        return if within { grow_state_in(s, bytes) } else { grow_state_at(s, bytes, false).map(|()| true) };
    }
    let (len, base) = (s.bytes.len(), s.bytes.as_ptr());
    let ok = if within { grow_state_in_at(s, bytes, true)? } else { grow_state_at(s, bytes, true).map(|()| true)? };
    fires::fire_shared("exact.build_boundary");
    if crate::env::set("PGRUST_GRAVITY_PHASES") {
        let road = match ok {
            false => "refused",
            true if s.bytes.len() == len => "fits",
            true if s.bytes.as_ptr() == base => "in-place",
            true => "reopen",
        };
        let slot = s.home.map_or(0, |h| h.slot);
        eprintln!("EXACT kind=build slot={slot} counted={counted} need={bytes} len={len} grown={} road={road}", s.bytes.len());
    }
    Ok(ok)
}

/// exactsize B1d: `State::room_in`.
pub(crate) fn room_in(s: &State<'_>) -> u64 {
    let Some(h) = s.home else { return s.bytes.len() as u64 };
    // SAFETY: the slot and the reservation belong to the Node this State borrows, read on the leader's thread.
    unsafe {
        let r = &*h.region;
        let reserved = if r.buf.is_reserved() && r.ptr as *const u8 == r.buf.as_ptr() { r.buf.reserve() as u64 } else { 0 };
        // under a ceiling `grow_state_in` reopens the slot at most at it (`grow_state_capped`), whatever G has left
        if r.ceil != 0 {
            return reserved.max(ceil_cap(r.ceil));
        }
        match h.stmt.as_ref() {
            Some(st) if st.admitted() => reserved.max(st.left()),
            _ => u64::MAX,
        }
    }
}

/// `grow_state`, its length `exact_len`'s when `exact` (exactsize B1a), else `grown_len`'s.
fn grow_state_at(s: &mut State<'_>, bytes: usize, exact: bool) -> Result<()> {
    let Some(h) = s.home else { return Err(Error::host("gravity: State::grow on a fixed view (no leader slot behind it)")) };
    // SAFETY: `h` was taken from the Node this State borrows, on the leader's thread; the slot is the one `s.bytes` views.
    unsafe {
        let r = &mut *h.region;
        // D292: an unopened slot (null: never opened this statement, or settled; `bytes` then keeps a fixed slot's size) is no
        // region to grow: a named ERROR, never a write through the empty view
        if r.ptr.is_null() {
            return Err(Error::host("gravity: State::grow on a leader slot not opened for its pipeline (D292)"));
        }
        let len = r.bytes as usize;
        if bytes <= len {
            return Ok(());
        }
        let mut g = Grew::default();
        let mut w = 0;
        let base = grow_slot(r, (bytes - len) as u64, len as u64, 0, &mut w, h.stmt.as_ref(), &mut g, false, exact).map_err(|e| debt_refused(e, r.debt, None))?;
        s.bytes = core::slice::from_raw_parts_mut(base, r.bytes as usize);
        fold(&mut g, &*h.counters, h.kind);
    }
    Ok(())
}

/// M7-SPILL (V4R-359): `State::grow_in`: `grow_state` only within the statement's G (the merge draws no W); `false` (nothing
/// drawn) past it, so the merge of a kind-7 build spills instead. A statement with no G grows as `grow_state` does.
pub(crate) fn grow_state_in(s: &mut State<'_>, bytes: usize) -> Result<bool> {
    grow_state_in_at(s, bytes, false)
}

/// `grow_state_in`, its length `exact_len`'s when `exact` (exactsize B1a).
fn grow_state_in_at(s: &mut State<'_>, bytes: usize, exact: bool) -> Result<bool> {
    let Some(h) = s.home else { return Err(Error::host("gravity: State::grow_in on a fixed view (no leader slot behind it)")) };
    // SAFETY: as `grow_state`: the slot and the reservation belong to the Node this State borrows, on the leader's thread.
    let (len, stmt) = unsafe { ((*h.region).bytes as u64, h.stmt.as_ref()) };
    if bytes as u64 <= len {
        return Ok(true);
    }
    // SAFETY: as above
    if unsafe { with_fault(&(*h.region).fault, |f| f.refuse()) } {
        return Ok(false);
    }
    // D2894: a reserved leader slot commits in place before the merge spills (pool bytes past G only if the pool has them now)
    // SAFETY: as above; `s.bytes` views the slot, re-viewed at its new length when it grew
    unsafe {
        let r = &mut *h.region;
        let ceil = r.ceil;
        if let Some((ok, _)) = slot_in_place(r, len, bytes as u64 - len, len, 0, None, stmt, true, exact, ceil) {
            if ok {
                s.bytes = core::slice::from_raw_parts_mut(r.ptr, r.bytes as usize);
                let mut g = Grew::default();
                g.note(len, r.bytes as u64);
                fold(&mut g, &*h.counters, h.kind);
            }
            return Ok(ok);
        }
    }
    // SAFETY: as above
    let ceil = unsafe { (*h.region).ceil };
    if ceil != 0 {
        return grow_state_capped(s, bytes, ceil, exact);
    }
    let new = next_len(len, bytes as u64 - len, len, 0, exact);
    if stmt.is_some_and(|r| r.admitted() && new.is_none_or(|n| r.left() < n)) {
        return Ok(false);
    }
    grow_state_at(s, bytes, exact).map(|()| true)
}

/// `grow_state_in` under the leader slot's ceiling (`SlotRegion::ceil`): the slot reopened at most at it (`capped_len`), the
/// whole old region copied, the bytes past G the pool's room now; `false` (nothing drawn) past the ceiling or when the
/// pool has no room, so the merge spills.
fn grow_state_capped(s: &mut State<'_>, bytes: usize, ceil: u64, exact: bool) -> Result<bool> {
    let Some(h) = s.home else { return Err(Error::host("gravity: State::grow_in on a fixed view (no leader slot behind it)")) };
    // SAFETY: as `grow_state`: the slot and the reservation belong to the Node this State borrows, on the leader's thread
    unsafe {
        let r = &mut *h.region;
        if r.ptr.is_null() {
            return Err(Error::host("gravity: State::grow on a leader slot not opened for its pipeline (D292)"));
        }
        let len = r.bytes as u64;
        let Some(new) = capped_len(len, bytes as u64 - len, len, 0, ceil, exact) else { return Ok(false) };
        let stmt = h.stmt.as_ref();
        let mut w = 0;
        let Some((mut buf, base)) = reopen_road(r.ptr, len as usize, len as usize, 0, new as usize, &mut w, stmt, false, true)? else { return Ok(false) };
        if let Some(o) = r.owner {
            buf.own_by(o); // LEDGER2 step 5: as region_grow_capped's
        }
        crate::park::REGIONS.put_from_life(core::mem::replace(&mut r.buf, buf), stmt);
        r.ptr = base;
        r.bytes = new as u32;
        s.bytes = core::slice::from_raw_parts_mut(base, r.bytes as usize);
        let mut g = Grew::default();
        g.note(len, new);
        fold(&mut g, &*h.counters, h.kind);
    }
    Ok(true)
}

/// M7-SPILL (V4R-359): `State::spill_write`: one epoch of the leader's (index 0) stored file of the State's slot.
pub(crate) fn spill_write(s: &mut State<'_>, bits: u8, parts: &[(u32, &[u8])]) -> Result<()> {
    let Some(h) = s.home else { return Err(Error::host("gravity: State::spill_write on a fixed view (no leader slot behind it)")) };
    // SAFETY: the Node's spill table (heap-stable), on the leader's thread after the section: no owner writes index 0 now.
    unsafe { (*h.spill).file_stored(h.slot, 0, bits, true)?.write_epoch(parts) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Grow mode (no work_mem ceiling with work_mem unset): a breaker region under the region wall's ceiling grows where the
    /// same need passes an 8 MiB hash_mem ceiling and spills there (`capped_len` None, counted at the ceiling); past the
    /// wall itself it still spills.
    #[test]
    fn under_grow_modes_ceiling_a_region_grows_past_work_mem() {
        const MIB: u64 = 1 << 20;
        let wall = mempolicy::grow::grow_ceiling(0, MAX_REGION);
        assert_eq!(capped_len(8 * MIB, 56 * MIB, 64, 0, 8 * MIB, false), None, "today: past hash_mem, the breaker spills");
        let grown = capped_len(8 * MIB, 56 * MIB, 64, 0, wall, false).expect("grow mode: it grows");
        assert!(grown >= 56 * MIB + 64 && grown <= wall, "grown to {grown}");
        assert_eq!(capped_len(8 * MIB, wall, 64, 0, wall, false), None, "past the region wall: it spills");
        assert_eq!(in_place_cap(1 << 30, wall), 1 << 30, "in place up to its whole reservation");
    }

    /// C10: a spilling breaker's uncapped grow counts only past its grant (its ceiling's region bytes, else W and the
    /// leader's G left), as its named road's when it names one.
    #[test]
    fn an_uncapped_grow_counts_past_the_grant_as_its_named_road() {
        use core::sync::atomic::Ordering::Relaxed;
        let n0 = UNCAPPED[1].load(Relaxed);
        note_uncapped(3, 1 << 20, 0, 1 << 20, None, Some(NoSpill::BuildLeaderMerge));
        note_uncapped(3, 1 << 20, 0, 1 << 19, Some(1 << 20), Some(NoSpill::BuildLeaderMerge));
        note_uncapped(3, ceil_cap(1 << 20), 1 << 20, 0, None, None);
        assert_eq!(UNCAPPED[1].load(Relaxed), n0, "within W, G or the ceiling's bytes is within the grant");
        note_uncapped(3, 64 << 20, 1 << 20, 8 << 20, Some(8 << 20), Some(NoSpill::BuildLeaderMerge));
        assert_eq!(UNCAPPED[1].load(Relaxed), n0 + 1, "a ceiling bounds the grow whatever W and G hold");
    }

    /// NR-1: a region whose breaker names a SPILL-DEBT row and builds no spill road (a nest inner's build) grown past the
    /// leader's G is counted as that row's (`UNCAPPED[1]`, its `NAMED`), never as an unnamed road. Before NR-1 its grows
    /// were skipped as a region that does not spill.
    #[test]
    fn a_debt_regions_grow_past_g_counts_as_its_row() {
        use core::sync::atomic::Ordering::Relaxed;
        use gravity_program::spill::{Holds, Proof};
        let held = Holds::Bounded(Proof::Estimate(NoSpill::BuildNestInner));
        let row = NoSpill::BuildNestInner.code() as usize;
        let (n0, r0) = (UNCAPPED[1].load(Relaxed), NAMED[row].load(Relaxed));
        // as `uncapped_op` / `uncapped_state` count it: the home as the slot's open sets it, a plain `region_grow` road
        if let Some(debt) = counted_as(held.spills(), held.debt(), None) {
            note_uncapped(2, 4 << 20, 0, 0, Some(1 << 20), debt);
        }
        assert_eq!(UNCAPPED[1].load(Relaxed), n0 + 1, "a debt region past G counts as named");
        assert_eq!(NAMED[row].load(Relaxed), r0 + 1, "as its own row's");
        assert_eq!(counted_as(held.spills(), held.debt(), Some(NoSpill::BuildLeaderMerge)), Some(Some(NoSpill::BuildLeaderMerge)), "a road that names its row counts as the road's");
        assert_eq!(counted_as(true, None, None), Some(None), "a spilling region's unnamed road stays a bug key");
    }

    /// NR-1: a region whose breaker neither spills nor names a row (a form PostgreSQL holds too, a program constant) is
    /// not counted on the uncapped road, so its grow past G meets no `note_uncapped` (whose debug assert would fire).
    #[test]
    fn a_pg_holds_too_regions_grow_past_g_fires_no_assert() {
        use gravity_program::spill::{Holds, PgHolds, Proof};
        for held in [Holds::Bounded(Proof::PgHoldsToo(PgHolds::SetOpSide)), Holds::Bounded(Proof::Constant(64))] {
            let counted = counted_as(held.spills(), held.debt(), None);
            assert_eq!(counted, None, "{held:?} is no road's");
            if let Some(debt) = counted {
                note_uncapped(2, 64 << 20, 1 << 20, 0, Some(1 << 20), debt);
            }
        }
    }

    /// NR-1: the report line's `named=` names each row's grows since the statement's start, in code order, 0 for none.
    #[test]
    fn the_named_field_names_each_rows_grows() {
        let mut now = [0u64; NoSpill::ALL.len()];
        let at = now;
        assert_eq!(named_field(now, at), "0");
        now[NoSpill::BuildNestInner.code() as usize] = 1;
        now[NoSpill::ConstantPastCap.code() as usize] = 3;
        let mut later = at;
        later[NoSpill::ConstantPastCap.code() as usize] = 1;
        assert_eq!(named_field(now, later), "BuildNestInner:1,ConstantPastCap:2");
    }

    /// NR-6: the debt lever (`pgrust.gravity_test_refuse_debt`) off is today's road: a grant has it off unless set (a trim
    /// keeps it), a named draw past the cap is counted as before and drawn, and the lever is no room ask's cause (`armed`,
    /// the forced-spill and revoke counts unchanged). On, the same draw is the pool's out of memory; a draw within the cap
    /// is never refused.
    #[test]
    fn the_debt_lever_off_is_todays_road() {
        use crate::mem::grant::Grant;
        use core::sync::atomic::Ordering::Relaxed;
        assert!(!Grant::of(1 << 20).refuse_debt && !Grant::of(1 << 20).trimmed(1 << 19).refuse_debt, "off unless set");
        assert!(Grant::of(1 << 20).with_refuse_debt(true).trimmed(1 << 19).refuse_debt, "a trim keeps it");
        let row = NoSpill::MergeRunPulled;
        let r0 = NAMED[row.code() as usize].load(Relaxed);
        let named = note_uncapped(4, 4 << 20, 0, 0, Some(1 << 20), Some(row));
        assert_eq!(named, Some(row), "past the cap: counted as its row");
        assert!(NAMED[row.code() as usize].load(Relaxed) > r0);
        assert!(debt_draw(named, false, 4 << 20).is_ok(), "off: drawn, as before the lever");
        assert!(matches!(debt_draw(named, true, 4 << 20), Err(Error::OutOfMemory { why: Some(r), .. }) if r == row), "on: the pool's out of memory, its row named");
        assert_eq!(note_uncapped(4, 1 << 20, 0, 0, Some(1 << 20), Some(row)), None, "within the cap: not counted");
        assert!(debt_draw(None, true, 1 << 20).is_ok(), "within the cap: never refused");
        for f in [Fault::OFF, Fault::every(3), Fault::every(0).with_revoke_every(2), Fault::every(5).with_revoke_every(7)] {
            assert!(!f.debt, "every fault is built with the lever off");
            for on in [false, true] {
                let (mut a, mut b) = (f, f.with_refuse_debt(on));
                assert_eq!(a.armed(), b.armed(), "the lever is no room ask's cause");
                for _ in 0..16 {
                    assert_eq!(a.refuse(), b.refuse(), "nor changes which ask is refused");
                }
                assert_eq!((a.every, a.asks, a.revoke_every, a.cut), (b.every, b.asks, b.revoke_every, b.cut));
            }
        }
    }

    /// C10: an uncapped grow past the grant on a road no lowering named is a Bug in a debug build.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "unnamed uncapped road")]
    fn an_unnamed_uncapped_grow_past_the_grant_is_a_bug_in_debug() {
        note_uncapped(5, 64 << 20, 1 << 20, 0, None, None);
    }

    /// D1029: a region at `MAX_REGION` cannot grow, whatever the need (a same-length reopen freed no room and the IR's retry
    /// spun); under it the region at least doubles, capped at `MAX_REGION`, and a need past `MAX_REGION` is refused as before.
    #[test]
    fn a_region_at_max_region_cannot_grow() {
        assert_eq!(grown_len(MAX_REGION, 16, MAX_REGION - 4096, 0), None);
        assert_eq!(grown_len(MAX_REGION, 16, 64, 64), None);
        assert_eq!(grown_len(MAX_REGION / 2 + 4096, 16, 64, 64), Some(MAX_REGION));
        assert!(grown_len(1 << 20, 16, 1 << 19, 1 << 19).is_some_and(|n| n >= 2 << 20));
        assert_eq!(grown_len(1 << 20, MAX_REGION, 64, 0), None);
    }

    /// Under a ceiling a region doubles up to it, never past it: a need that fits the ceiling grows to at most the ceiling's
    /// region bytes; a need past it is refused (the breaker spills) and counted.
    #[test]
    fn a_region_grows_up_to_its_ceiling_and_never_past_it() {
        const MIB: u64 = 1 << 20;
        assert_eq!(capped_len(MIB, 16, MIB / 2, MIB / 2, 4 * MIB, false), Some(2 * MIB), "doubles under the ceiling");
        assert_eq!(capped_len(3 * MIB, 16, 3 * MIB / 2, 3 * MIB / 2, 4 * MIB, false), Some(4 * MIB), "held to the ceiling");
        let before = AT_CEILING.load(core::sync::atomic::Ordering::Relaxed);
        assert_eq!(capped_len(4 * MIB, 16, 2 * MIB, 2 * MIB, 4 * MIB, false), None, "past the ceiling: spill");
        assert!(AT_CEILING.load(core::sync::atomic::Ordering::Relaxed) > before, "counted");
        assert_eq!(capped_len(MIB, 3 * MIB, MIB / 2, 0, 4 * MIB, true), Some(crate::park::region_bytes((3 * MIB + MIB / 2) as usize) as u64), "exact: the need, not a doubling");
    }
}

//! The residency registry: ranges charged to an account at what they asked for (Gravity regions, the buffer pool's
//! block array), read by the residency pass off the query path. Each account's not-resident bytes are its ranges'
//! charge less what the pass found resident. D4059 / D4812: a statement a cancel's pick found stale is handed to the
//! residency thread (`hint`), which reads its ranges again (`refresh_acct`), so a range that filled since its last read
//! does not hide the statement from the next pick.

use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use pgsync::Mutex;

use crate::charge::NrHome;
use crate::{note_not_resident, Acct};

/// A range the registry does not list.
pub const NONE: u32 = u32::MAX;

/// mincore's scratch: one byte a page, this many pages a call.
const STEP_PAGES: usize = 1 << 16;

/// Every this many passes, one reads every entry (a page the kernel took without an event of ours: swap).
const FULL_EVERY: u64 = 10;

#[derive(Clone, Copy)]
struct Entry {
    lo: usize,
    span: usize,
    charged: usize,
    acct: Acct,
    mapped: bool,
    nr: i64,
    /// LEDGER2 design A: the owner cell its not-resident bytes book on too (its charge's; `NrHome::NONE`: its account's
    /// alone).
    home: NrHome,
    /// A pass applies what it read only to the entry it read (`born` unchanged), moved or resized since or not.
    born: u64,
    gen: u64,
    /// An entry read wholly resident at `gen` is skipped until it changes or a full pass: its pages leave only at the
    /// events that change it (a give-back, an uncommit, an unmap).
    seen: u64,
    /// Unchanged, it is read again from this pass on: one budget's worth of its span a pass (the buffer pool is one
    /// entry of shared_buffers' size).
    due: u64,
    /// D4059: the pass interval of its last read (the count of passes run then; `u64::MAX` before any): a refresh reads
    /// it at most once an interval, so the watchdog's reads cost no more than the passes' own.
    read_at: u64,
}

struct Registry {
    slots: Vec<Option<Entry>>,
    /// Its capacity is kept at least the slots' count, so an unregister never allocates.
    free: Vec<u32>,
    gen: u64,
}

static REG: Mutex<Registry> = Mutex::new(Registry { slots: Vec::new(), free: Vec::new(), gen: 0 }); // alloc: Cold

struct Scratch {
    /// (slot, born, gen, lo, span, resident bytes; `usize::MAX` = not read)
    snap: Vec<(u32, u64, u64, usize, usize, usize)>,
    buf: Vec<u8>,
    passes: u64,
    /// The slot the next budgeted read starts at.
    cursor: usize,
}

static PASS: Mutex<Scratch> = Mutex::new(Scratch { snap: Vec::new(), buf: Vec::new(), passes: 0, cursor: 0 }); // alloc: Cold

/// A residency reader: `read(lo, hi, scratch)` is the bytes of [lo, hi) resident now (mincore in the server), `None` for a
/// range unmapped meanwhile.
pub type ReadFn = fn(usize, usize, &mut [u8]) -> Option<usize>;

/// D4059: the reader the last pass ran with, for `refresh_acct` (the watchdog's crate has no reader of its own).
static READER: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// D4059: at most this many of one account's ranges are read by one `refresh_acct` (a fixed snapshot: no allocation).
const REFRESH_ENTRIES: usize = 32;

/// What one pass read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PassRead {
    pub regions: u64,
    pub bytes: u64,
}

/// `fresh`: none of its pages resident yet (a mapping just made); otherwise resident until a pass reads it. `NONE` when
/// the registry cannot grow: its account then counts it whole.
pub fn register(lo: usize, span: usize, charged: usize, acct: Acct, mapped: bool, fresh: bool) -> u32 {
    register_at(lo, span, charged, acct, mapped, fresh, NrHome::NONE)
}

/// LEDGER2 design A: `register` for a range its charge owns: its not-resident bytes book on `home` (`Charge::nr_home`)
/// as well as on its account.
pub fn register_at(lo: usize, span: usize, charged: usize, acct: Acct, mapped: bool, fresh: bool, mut home: NrHome) -> u32 {
    let mut r = REG.lock().unwrap_or_else(|e| e.into_inner());
    let slot = match r.free.pop() {
        Some(i) => i,
        None => {
            let n = r.slots.len();
            let free_more = (n + 1).saturating_sub(r.free.len());
            if n >= NONE as usize || r.slots.try_reserve(1).is_err() || r.free.try_reserve(free_more).is_err() {
                return NONE;
            }
            r.slots.push(None);
            n as u32
        }
    };
    r.gen += 1;
    let nr = if fresh { charged as i64 } else { 0 };
    note_not_resident(acct, nr, mapped);
    home.post(nr);
    r.slots[slot as usize] = Some(Entry { lo, span, charged, acct, mapped, nr, home, born: r.gen, gen: r.gen, seen: 0, due: 0, read_at: u64::MAX });
    slot
}

/// The bytes it grew by count as not resident (pages just opened or taken back) until the next pass reads it.
pub fn update(id: u32, lo: usize, span: usize, charged: usize, acct: Acct) {
    update_with(id, lo, span, charged, acct, None)
}

/// LEDGER2 design A: `update` for a range whose charge may have moved: its not-resident bytes leave its last home for
/// `home` (`Charge::nr_home`, read after the move).
pub fn update_at(id: u32, lo: usize, span: usize, charged: usize, acct: Acct, home: NrHome) {
    update_with(id, lo, span, charged, acct, Some(home))
}

fn update_with(id: u32, lo: usize, span: usize, charged: usize, acct: Acct, home: Option<NrHome>) {
    if id == NONE {
        return;
    }
    let mut r = REG.lock().unwrap_or_else(|e| e.into_inner());
    r.gen += 1;
    let gen = r.gen;
    let Some(Some(e)) = r.slots.get_mut(id as usize) else { return };
    let nr = e.nr.saturating_add(charged.saturating_sub(e.charged) as i64).min(charged as i64);
    note_not_resident(e.acct, -e.nr, e.mapped);
    note_not_resident(acct, nr, e.mapped);
    let mut to = home.unwrap_or(e.home);
    // the new home's first, so an ended operator's cell that only these bytes keep frees after its last one leaves
    to.post(nr);
    e.home.post(-e.nr);
    *e = Entry { lo, span, charged, acct, mapped: e.mapped, nr, home: to, born: e.born, gen, seen: 0, due: e.due, read_at: e.read_at };
}

/// D4404: entry `id`'s resident bytes at its last read (its charged bytes less the ones not resident then; a fresh mapping
/// no pass has read yet: none), `None` when not listed. One lock, no read of the range.
pub fn resident_of(id: u32) -> Option<usize> {
    if id == NONE {
        return None;
    }
    let r = REG.lock().unwrap_or_else(|e| e.into_inner());
    let e = r.slots.get(id as usize)?.as_ref()?;
    Some(e.charged.saturating_sub(e.nr.max(0) as usize))
}

/// Entry `id`'s account (the charge's owner it books on), `None` when not listed: a witness's read of where a region's
/// residency moved with its charge.
pub fn acct_of(id: u32) -> Option<Acct> {
    if id == NONE {
        return None;
    }
    let r = REG.lock().unwrap_or_else(|e| e.into_inner());
    Some(r.slots.get(id as usize)?.as_ref()?.acct)
}

pub fn unregister(id: u32) {
    if id == NONE {
        return;
    }
    let mut r = REG.lock().unwrap_or_else(|e| e.into_inner());
    let Some(slot) = r.slots.get_mut(id as usize) else { return };
    if let Some(mut e) = slot.take() {
        note_not_resident(e.acct, -e.nr, e.mapped);
        e.home.post(-e.nr);
        r.free.push(id);
    }
}

/// `read(lo, hi, scratch)` runs outside the registry's lock; `None` from it (a range unmapped meanwhile) keeps the
/// entry's last numbers, never 0. An entry changed since its last read is always read; the partly resident rest (every
/// entry on a full pass) round-robin until `budget` bytes are read, each again only `span / budget` passes after its
/// last read, so a large range warming up (the buffer pool) costs a pass about `budget` bytes on average. An unbounded
/// pass (`usize::MAX`, the view's) reads every such entry whenever a bounded one last read it: the bounded pass's wait is
/// its own cost bound, and the view would otherwise show that older read, a pool the watchdog read before its first page
/// was touched as wholly not resident for `span / budget` passes. `None` when the scratch could not be had.
pub fn pass(read: ReadFn, budget: usize) -> Option<PassRead> {
    READER.store(read as *mut (), Ordering::Release);
    let mut guard = PASS.lock().unwrap_or_else(|e| e.into_inner());
    let Scratch { snap, buf, passes, cursor } = &mut *guard;
    if buf.is_empty() {
        if buf.try_reserve_exact(STEP_PAGES).is_err() {
            return None;
        }
        buf.resize(STEP_PAGES, 0);
    }
    snap.clear();
    let full = *passes % FULL_EVERY == 0;
    let all = budget == usize::MAX;
    {
        let r = REG.lock().unwrap_or_else(|e| e.into_inner());
        let n = r.slots.len() - r.free.len();
        if snap.try_reserve(n).is_err() {
            return None;
        }
        for (i, s) in r.slots.iter().enumerate() {
            if let Some(e) = s.filter(|e| e.seen != e.gen) {
                snap.push((i as u32, e.born, e.gen, e.lo, e.span, usize::MAX));
            }
        }
        let (n, mut spent, mut next) = (r.slots.len(), 0usize, 0);
        for k in 0..n {
            let i = (*cursor + k) % n;
            let Some(e) = r.slots[i].filter(|e| e.seen == e.gen && (full || e.nr != 0) && (all || *passes >= e.due)) else {
                continue;
            };
            if spent >= budget {
                next = i;
                break;
            }
            spent = spent.saturating_add(e.span);
            snap.push((i as u32, e.born, e.gen, e.lo, e.span, usize::MAX));
        }
        *cursor = next;
    }
    let mut bytes = 0u64;
    for item in snap.iter_mut() {
        item.5 = read(item.3, item.3.saturating_add(item.4), buf).unwrap_or(usize::MAX);
        bytes += item.4 as u64;
    }
    apply(&mut REG.lock().unwrap_or_else(|e| e.into_inner()), snap, *passes, Some(budget));
    *passes += 1;
    Some(PassRead { regions: snap.len() as u64, bytes })
}

/// What a read found, onto the entries it read (`born` unchanged; an unread one keeps its last numbers) in pass interval
/// `at`. `budget`: a pass's, which schedules an entry's next bounded read; `None` (a refresh) leaves the schedule as it was.
fn apply(r: &mut Registry, snap: &[(u32, u64, u64, usize, usize, usize)], at: u64, budget: Option<usize>) {
    for &(id, born, gen, _, _, resident) in snap.iter() {
        let Some(Some(e)) = r.slots.get_mut(id as usize) else { continue };
        if e.born != born || resident == usize::MAX {
            continue;
        }
        // pages a commit opened since the read are not resident yet; the next pass reads it again
        let nr = e.charged.saturating_sub(resident.min(e.charged)) as i64;
        note_not_resident(e.acct, nr - e.nr, e.mapped);
        e.home.post(nr - e.nr);
        e.nr = nr;
        e.read_at = at;
        if let Some(budget) = budget {
            e.due = at + e.span.div_ceil(budget.max(1)) as u64;
        }
        if e.gen == gen {
            e.seen = gen;
        }
    }
}

/// D4059: account `a`'s ranges that were not wholly resident at their last read, read again now with the last pass's
/// reader (at most `REFRESH_ENTRIES` of them, in registry order; each at most once a pass interval): a range that filled
/// with no event of ours (a region's pages written after its commit) counts as resident at once, not `span / budget`
/// passes later. The bytes read; `None`
/// when no pass has run yet (no reader, no scratch), or a pass holds the scratch or another thread the registry now. No
/// allocation and no wait: the residency thread calls it for a cancel's hints (`refresh_hinted`).
pub fn refresh_acct(a: Acct) -> Option<u64> {
    let p = READER.load(Ordering::Acquire);
    if p.is_null() {
        return None;
    }
    // SAFETY: only `pass` stores here, always from a `ReadFn`.
    let read: ReadFn = unsafe { core::mem::transmute::<*mut (), ReadFn>(p) };
    let mut guard = PASS.try_lock().ok()?;
    let Scratch { buf, passes, .. } = &mut *guard;
    let at = *passes;
    if buf.is_empty() {
        return None;
    }
    let mut snap = [(0u32, 0u64, 0u64, 0usize, 0usize, usize::MAX); REFRESH_ENTRIES];
    let mut n = 0;
    {
        let r = REG.try_lock().ok()?;
        for (i, s) in r.slots.iter().enumerate() {
            if n == REFRESH_ENTRIES {
                break;
            }
            if let Some(e) = s.filter(|e| e.acct == a && e.nr > 0 && e.read_at != at) {
                snap[n] = (i as u32, e.born, e.gen, e.lo, e.span, usize::MAX);
                n += 1;
            }
        }
    }
    let mut bytes = 0u64;
    for item in snap[..n].iter_mut() {
        item.5 = read(item.3, item.3.saturating_add(item.4), buf).unwrap_or(usize::MAX);
        bytes += item.4 as u64;
    }
    apply(&mut *REG.try_lock().ok()?, &snap[..n], at, None);
    Some(bytes)
}

/// D4812: the accounts a cancel's pick found stale (`hint`), for the residency thread to read again next
/// (`refresh_hinted`): an account's id plus one, 0 = an empty slot.
static HINTS: [AtomicU64; HINT_SLOTS] = [const { AtomicU64::new(0) }; HINT_SLOTS];
/// The slots: twice the statements one pick hints (`mempolicy::watchdog::STALE_REFRESH_MAX`). CHOSEN, not tuned.
const HINT_SLOTS: usize = 8;
/// D4812: the residency thread's wake (`set_hint_wake`), which `hint` calls.
static HINT_WAKE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
/// D4812: the hints posted, the hints dropped (every slot taken), the accounts `refresh_hinted` read again.
static HINTED: AtomicU64 = AtomicU64::new(0);
static HINTS_DROPPED: AtomicU64 = AtomicU64::new(0);
static HINTS_READ: AtomicU64 = AtomicU64::new(0);

/// D4812: the residency thread's wake, called by each `hint` (the memory runtime's, at its start).
pub fn set_hint_wake(wake: fn()) {
    HINT_WAKE.store(wake as *mut (), Ordering::Release);
}

/// D4812: account `a`'s ranges are read again by the residency thread's next step (`refresh_hinted`), not on the
/// caller's thread: the memory watchdog's cancel pick calls it where D4059 read them itself (each mincore allocates a
/// kernel page, which under host memory pressure waited in direct reclaim for seconds). Stores and a wake: no
/// allocation, no lock, no wait. An account already hinted keeps its one slot; with every slot taken the hint is dropped
/// and counted (the thread's passes read the account on their schedule).
pub fn hint(a: Acct) {
    let v = u64::from(a.0) + 1;
    let posted = HINTS.iter().any(|h| h.load(Ordering::Relaxed) == v)
        || HINTS.iter().any(|h| h.compare_exchange(0, v, Ordering::AcqRel, Ordering::Relaxed).is_ok());
    if !posted {
        HINTS_DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    HINTED.fetch_add(1, Ordering::Relaxed);
    let p = HINT_WAKE.load(Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: only `set_hint_wake` stores here, always from a `fn()`.
        let wake: fn() = unsafe { core::mem::transmute::<*mut (), fn()>(p) };
        wake();
    }
}

/// D4812: on the residency thread, each hinted account read again (`refresh_acct`) and its slot freed. The accounts read
/// (one a pass holds the scratch for, or whose ranges were all read this pass interval, is not).
pub fn refresh_hinted() -> u64 {
    let mut read = 0;
    for h in HINTS.iter() {
        let v = h.swap(0, Ordering::AcqRel);
        if v != 0 && refresh_acct(Acct((v - 1) as u32)).is_some() {
            read += 1;
        }
    }
    HINTS_READ.fetch_add(read, Ordering::Relaxed);
    read
}

/// D4812: `(hints posted, hints dropped with every slot taken, accounts the residency thread read again for them)`.
pub fn hint_counts() -> (u64, u64, u64) {
    (HINTED.load(Ordering::Relaxed), HINTS_DROPPED.load(Ordering::Relaxed), HINTS_READ.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};

    const MIB: usize = 1 << 20;

    static RESIDENT: AtomicUsize = AtomicUsize::new(0);
    static READS: AtomicUsize = AtomicUsize::new(0);

    fn fake(_lo: usize, _hi: usize, _buf: &mut [u8]) -> Option<usize> {
        READS.fetch_add(1, Ordering::Relaxed);
        match RESIDENT.load(Ordering::Relaxed) {
            usize::MAX => None,
            n => Some(n),
        }
    }

    fn nr(a: Acct) -> (i64, i64) {
        let r = crate::account_row(a);
        (r.not_resident_bytes, r.not_resident_mapped_bytes)
    }

    /// Every test here runs `pass`, which reads every listed range with its fake reader: one at a time.
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn turn() -> std::sync::MutexGuard<'static, ()> {
        TURN.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn an_accounts_not_resident_bytes_follow_its_ranges() {
        let _turn = turn();
        let a = Acct::backend(4050);
        let b = Acct::backend(4051);
        let id = register(0x1000_0000, 64 * MIB, 64 * MIB, a, true, true);
        assert_ne!(id, NONE);
        assert_eq!(nr(a), (64 * MIB as i64, 64 * MIB as i64), "a fresh mapping: nothing resident");

        RESIDENT.store(16 * MIB, Ordering::Relaxed);
        assert!(pass(fake, usize::MAX).is_some());
        assert_eq!(nr(a).1, 48 * MIB as i64);

        RESIDENT.store(usize::MAX, Ordering::Relaxed);
        assert!(pass(fake, usize::MAX).is_some());
        assert_eq!(nr(a).1, 48 * MIB as i64, "an unread range keeps its last numbers");

        update(id, 0x1000_0000, 96 * MIB, 96 * MIB, b);
        assert_eq!((nr(a).1, nr(b).1), (0, 80 * MIB as i64), "moved with its growth not resident");

        RESIDENT.store(96 * MIB, Ordering::Relaxed);
        assert!(pass(fake, usize::MAX).is_some());
        assert_eq!(nr(b).1, 0);
        let reads = READS.load(Ordering::Relaxed);
        for _ in 0..FULL_EVERY {
            assert!(pass(fake, usize::MAX).is_some());
        }
        assert_eq!(READS.load(Ordering::Relaxed) - reads, 1, "wholly resident and unchanged: read at the full pass only");

        unregister(id);
        assert_eq!((nr(a), nr(b)), ((0, 0), (0, 0)));

        // a budget of 4 MiB: changed entries are all read, the partly resident rest in turn up to the budget
        let x = register(0x2000_0000, 4 * MIB, 4 * MIB, a, true, true);
        let y = register(0x3000_0000, 4 * MIB, 4 * MIB, b, true, true);
        RESIDENT.store(MIB, Ordering::Relaxed);
        let reads = READS.load(Ordering::Relaxed);
        assert!(pass(fake, 4 * MIB).is_some());
        assert_eq!((READS.load(Ordering::Relaxed) - reads, nr(a).1, nr(b).1), (2, 3 * MIB as i64, 3 * MIB as i64));
        RESIDENT.store(2 * MIB, Ordering::Relaxed);
        assert!(pass(fake, 4 * MIB).is_some());
        assert_eq!(READS.load(Ordering::Relaxed) - reads, 3);
        assert_eq!(nr(a).1 + nr(b).1, 5 * MIB as i64, "one of the two read");
        assert!(pass(fake, 4 * MIB).is_some());
        assert_eq!((READS.load(Ordering::Relaxed) - reads, nr(a).1, nr(b).1), (4, 2 * MIB as i64, 2 * MIB as i64), "the other next");
        unregister(x);
        unregister(y);
        assert_eq!((nr(a), nr(b)), ((0, 0), (0, 0)));

        // an entry twice the budget is read again only every second pass
        let z = register(0x4000_0000, 8 * MIB, 8 * MIB, a, true, true);
        assert!(pass(fake, 4 * MIB).is_some());
        assert_eq!(nr(a).1, 6 * MIB as i64);
        RESIDENT.store(3 * MIB, Ordering::Relaxed);
        assert!(pass(fake, 4 * MIB).is_some());
        assert_eq!(nr(a).1, 6 * MIB as i64, "not due yet");
        assert!(pass(fake, 4 * MIB).is_some());
        assert_eq!(nr(a).1, 5 * MIB as i64, "due");
        unregister(z);
        assert_eq!(nr(a), (0, 0));

        // the view's pass reads an entry a bounded pass read before its first page was touched, not due or due
        let v = register(0x5000_0000, 8 * MIB, 8 * MIB, a, true, true);
        RESIDENT.store(0, Ordering::Relaxed);
        assert!(pass(fake, 4 * MIB).is_some());
        assert_eq!(nr(a).1, 8 * MIB as i64, "nothing touched yet");
        RESIDENT.store(3 * MIB, Ordering::Relaxed);
        assert!(pass(fake, usize::MAX).is_some());
        assert_eq!(nr(a).1, 5 * MIB as i64, "the view's pass reads it at once");
        RESIDENT.store(4 * MIB, Ordering::Relaxed);
        assert!(pass(fake, usize::MAX).is_some());
        assert_eq!(nr(a).1, 4 * MIB as i64, "and every time");
        unregister(v);
        assert_eq!(nr(a), (0, 0));

        // D4059: a refresh reads one account's not-wholly-resident ranges now, whatever their schedule, and no other's
        let c = Acct::backend(4052);
        let w = register(0x6000_0000, 8 * MIB, 8 * MIB, c, true, true);
        let u = register(0x7000_0000, 8 * MIB, 8 * MIB, b, true, true);
        RESIDENT.store(0, Ordering::Relaxed);
        assert!(pass(fake, 4 * MIB).is_some());
        assert_eq!((nr(c).1, nr(b).1), (8 * MIB as i64, 8 * MIB as i64), "read untouched");
        RESIDENT.store(8 * MIB, Ordering::Relaxed);
        let reads = READS.load(Ordering::Relaxed);
        assert_eq!(refresh_acct(c), Some(8 * MIB as u64));
        assert_eq!((READS.load(Ordering::Relaxed) - reads, nr(c).1, nr(b).1), (1, 0, 8 * MIB as i64), "filled: read at once");
        assert_eq!(refresh_acct(c), Some(0), "wholly resident: nothing to read again");
        // a range still partly resident is read again at most once a pass interval
        RESIDENT.store(2 * MIB, Ordering::Relaxed);
        update(w, 0x6000_0000, 8 * MIB, 8 * MIB, c);
        assert!(pass(fake, 4 * MIB).is_some());
        assert_eq!(nr(c).1, 6 * MIB as i64, "read by the pass: changed");
        RESIDENT.store(4 * MIB, Ordering::Relaxed);
        assert_eq!(refresh_acct(c), Some(8 * MIB as u64), "the first refresh after a pass reads it");
        assert_eq!(nr(c).1, 4 * MIB as i64);
        assert_eq!(refresh_acct(c), Some(0), "not again before the next pass");
        unregister(w);
        unregister(u);
        assert_eq!((nr(b), nr(c)), ((0, 0), (0, 0)));
    }

    /// LEDGER2 design A (main 09:50:42Z): a listed range's not-resident bytes book on its charge's owner as on its account;
    /// the owner's resident bytes are its held less them (`Usage::subtree_resident`); they follow the charge to another
    /// owner when the range is listed again with the charge's home (a region's `move_to` / `own_by`), sum into the parent's
    /// subtree, keep an ended operator keyed while they are on it, move with its residue to the session at the statement's
    /// end, and leave with the range.
    #[test]
    fn ledger2_an_owners_not_resident_bytes_follow_its_charge_and_resident_is_held_less_them() {
        use crate::charge::{context, op_begin, op_end, usage, Door, Owner, Release};
        use crate::{Charge, Kind};
        let _turn = turn();
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        std::thread::spawn(|| {
            const M: i64 = MIB as i64;
            let a = Acct::backend(4053);
            crate::bind_backend(4053, 9053);
            crate::statement_begin();
            let st = context().0;
            let op = op_begin(st, Kind::Sort as u32, 64 * MIB as u64);
            assert!(matches!(op, Owner::Op { .. }), "an operator cell of its own");
            // a mapping of 32 MiB on the statement, just made: none of it resident
            let mut c = Charge::open(st, Release::Statement, Kind::Sort, Door::Mapped, 32 * MIB as u64);
            let id = register_at(0x5100_0000, 32 * MIB, 32 * MIB, c.acct(), true, true, c.nr_home());
            assert_ne!(id, NONE);
            let u = usage(st);
            assert_eq!((u.held, u.not_resident, u.subtree_resident()), (32 * M, 32 * M, 0), "a fresh mapping: nothing in memory");
            // the pass finds 8 MiB resident: the owner and its account agree
            RESIDENT.store(8 * MIB, Ordering::Relaxed);
            assert!(pass(fake, usize::MAX).is_some());
            let u = usage(st);
            assert_eq!((u.not_resident, u.subtree_resident()), (24 * M, 8 * M), "resident = held - not resident");
            assert_eq!(nr(a).1, 24 * M, "its account's not-resident bytes, the same");
            // the charge moves to the operator and the range is listed again with its home (`RegionBuf::move_to`)
            c.transfer(op, Release::Statement);
            update_at(id, 0x5100_0000, 32 * MIB, 32 * MIB, c.acct(), c.nr_home());
            let (us, uo) = (usage(st), usage(op));
            assert_eq!((uo.held, uo.not_resident, uo.subtree_resident()), (32 * M, 24 * M, 8 * M), "on the operator now");
            assert_eq!((us.held, us.not_resident), (0, 0), "off the statement's own");
            assert_eq!((us.subtree, us.subtree_not_resident, us.subtree_resident()), (32 * M, 24 * M, 8 * M), "in its subtree");
            // a growth of the listed range counts as not resident until a pass reads it
            c.commit(16 * MIB as u64);
            update_at(id, 0x5100_0000, 48 * MIB, 48 * MIB, c.acct(), c.nr_home());
            assert_eq!((usage(op).held, usage(op).not_resident), (48 * M, 40 * M));
            RESIDENT.store(12 * MIB, Ordering::Relaxed);
            assert!(pass(fake, usize::MAX).is_some());
            assert_eq!((usage(op).not_resident, usage(op).subtree_resident()), (36 * M, 12 * M), "read again: 12 MiB in memory");
            // an ended operator stays keyed and in its statement's subtree while its bytes are on it
            op_end(op);
            assert_eq!((usage(op).not_resident, usage(st).subtree_not_resident), (36 * M, 36 * M), "ended, still counted");
            // the statement ends: the operator's numbers, its not-resident bytes with them, go where its charges go
            crate::statement_end();
            let ses = usage(Owner::Session(a));
            assert_eq!((usage(op), usage(st)), (crate::charge::Usage::default(), crate::charge::Usage::default()), "both ended");
            assert_eq!((ses.held, ses.not_resident, ses.subtree_resident()), (48 * M, 36 * M, 12 * M), "on the session");
            // the range leaves (its region's drop: off the registry first), then its charge
            unregister(id);
            assert_eq!((usage(Owner::Session(a)).not_resident, nr(a)), (0, (0, 0)), "nothing left behind");
            drop(c);
            assert_eq!(usage(Owner::Session(a)).held, 0);
            crate::unbind_backend();
        })
        .join()
        .unwrap();
    }

    /// LEDGER2 design A: an ended operator whose charge already left (a mapping handed to the reaper: its charge dropped,
    /// the range not yet moved) is kept by its not-resident bytes alone, and frees when they move to the new home.
    #[test]
    fn ledger2_an_ended_operator_frees_when_its_last_not_resident_bytes_move() {
        use crate::charge::{context, op_begin, op_end, usage, Door, Holder, Owner, Release};
        use crate::{Charge, Kind};
        let _turn = turn();
        let _unbound = crate::unbound_shared(); // D4356: this test binds a backend, so it may move UNBOUND's books
        std::thread::spawn(|| {
            const M: i64 = MIB as i64;
            crate::bind_backend(4054, 9054);
            crate::statement_begin();
            let st = context().0;
            let op = op_begin(st, Kind::HashAgg as u32, 0);
            let c = Charge::open(op, Release::Statement, Kind::HashAgg, Door::Mapped, 8 * MIB as u64);
            let id = register_at(0x5200_0000, 8 * MIB, 8 * MIB, c.acct(), true, true, c.nr_home());
            op_end(op);
            drop(c);
            assert_eq!((usage(op).held, usage(op).not_resident), (0, 8 * M), "its not-resident bytes keep it keyed");
            // the reaper's charge takes the range (`reap::defer`)
            let r = Charge::open(Owner::Cache(Holder::Reaping), Release::Evict, Kind::Mapped, Door::Mapped, 8 * MIB as u64);
            update_at(id, 0x5200_0000, 8 * MIB, 8 * MIB, Acct::REAPING, r.nr_home());
            assert_eq!(usage(op), crate::charge::Usage::default(), "freed with its last byte");
            assert_eq!(usage(st).subtree_not_resident, 0);
            unregister(id);
            drop(r);
            crate::statement_end();
            crate::unbind_backend();
        })
        .join()
        .unwrap();
    }
}

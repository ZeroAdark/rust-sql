//! The once-cell per (part, dictionary epoch, expression fingerprint): cells in the part-resident
//! region, the first claimer builds, concurrent claimers wait, a new query generation frees
//! every cell (ruling: tables never persist across queries). A cell records its builder's thread:
//! the thread that holds a key's build never waits for itself (D319), and a wait for another
//! thread's build spins briefly, then takes the caller's wait step (its sanctioned sleep and
//! interrupt check, `claim_while`): this crate never sleeps or parks a thread itself.
extern crate std;

use gravity_abi::{word, PtrWord, Word};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::table::Header;
use crate::{BuildError, Region};

pub const FREE: u32 = 0;
pub const CLAIMING: u32 = 1;
pub const BUILDING: u32 = 2;
pub const READY: u32 = 3;
pub const FAILED: u32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub part: u32,
    pub epoch: u32,
    pub fingerprint: u64,
}

#[repr(C)]
pub struct Cell {
    state: AtomicU32,
    part: u32,
    epoch: u32,
    pad: u32,
    fingerprint: u64,
    generation: u64,
    table: Word<*const Header>,
    /// The builder's thread (`token`), written before `BUILDING` is published.
    owner: u64,
}

#[repr(C)]
struct Head {
    generation: AtomicU64,
    slots: u32,
    pad: u32,
}

#[derive(Clone, Copy)]
pub struct OnceTable {
    head: *const Head,
}

unsafe impl Send for OnceTable {}
unsafe impl Sync for OnceTable {}

pub enum Claim {
    /// Built by an earlier claimer of this generation.
    Ready(*const Header),
    /// This claimer builds; `publish` or drop (a failed build, retried by the next claimer).
    Build(Builder),
    /// Every cell holds a live table of this generation.
    Full,
    /// This thread holds the key's build (its `Builder` neither published nor dropped): a second
    /// claim of the key before the first ends would wait for itself forever (D319: a program's
    /// table left building until its `LANE_DONE`, claimed again by a second lane of the same key).
    /// The claimer's bug, never a wait.
    Held,
    /// `claim_while`'s wait step ended the wait for another thread's claim or build (its interrupt check).
    Cancelled,
}

/// The calling thread's token (nonzero, one per thread for the process's life).
fn token() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    std::thread_local! {
        static TOKEN: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
    }
    TOKEN.with(|t| {
        if t.get() == 0 {
            t.set(NEXT.fetch_add(1, Ordering::Relaxed));
        }
        t.get()
    })
}

/// A wait for another thread's claim or build: 64 spins, then the caller's `wait` step each round
/// (false: the wait ends).
struct Backoff(u32);

impl Backoff {
    fn snooze(&mut self, wait: &mut dyn FnMut() -> bool) -> bool {
        self.0 = self.0.saturating_add(1);
        if self.0 <= 64 {
            core::hint::spin_loop();
            return true;
        }
        wait()
    }
}

pub struct Builder {
    cell: *mut Cell,
    published: bool,
}

impl OnceTable {
    /// The region bytes `in_region` takes for `slots` cells (D1747: a source sizes its cells to its table lanes).
    pub const fn region_bytes(slots: u32) -> usize {
        core::mem::size_of::<Head>() + slots as usize * core::mem::size_of::<Cell>()
    }

    pub fn in_region(region: &mut Region, slots: u32) -> Result<OnceTable, BuildError> {
        let head = region.alloc_zeroed(Self::region_bytes(slots), 8)? as *mut Head;
        // SAFETY: a fresh zeroed block; zero is `FREE` / generation 0 / null.
        unsafe { (*head).slots = slots };
        Ok(OnceTable { head })
    }

    fn cells(&self) -> (*mut Cell, usize) {
        // SAFETY: `head` is followed by `slots` cells.
        unsafe { ((self.head as *mut u8).add(core::mem::size_of::<Head>()) as *mut Cell, (*self.head).slots as usize) }
    }

    pub fn slots(&self) -> u32 {
        // SAFETY: `head` is live.
        unsafe { (*self.head).slots }
    }

    pub fn generation(&self) -> u64 {
        // SAFETY: `head` is live.
        unsafe { (*self.head).generation.load(Ordering::Acquire) }
    }

    /// A statement boundary: cells of every earlier generation are free. No claim is in flight.
    pub fn begin_query(&self, generation: u64) {
        // SAFETY: `head` is live.
        unsafe { (*self.head).generation.store(generation, Ordering::Release) };
    }

    /// Tables of the current generation that are ready.
    pub fn live(&self) -> u32 {
        let (cells, n) = self.cells();
        let gen = self.generation();
        let mut live = 0;
        for i in 0..n {
            // SAFETY: `i < slots`.
            let c = unsafe { &*cells.add(i) };
            if c.state.load(Ordering::Acquire) == READY && c.generation == gen {
                live += 1;
            }
        }
        live
    }

    /// `claim_while` whose wait step yields the thread and never ends the wait: for cells no other
    /// thread builds (a source's own table cells), where the wait cannot arise.
    pub fn claim(&self, key: Key) -> Claim {
        self.claim_while(key, &mut || {
            std::thread::yield_now();
            true
        })
    }

    /// The key's table: `Ready`, or `Build` for this claimer, or `Full`; `Held` when this thread
    /// already holds the key's build (D319). A wait for another thread's claim or build spins
    /// briefly, then calls `wait` each round: one step of the caller's backoff (its sanctioned
    /// sleep, `waiter::sleep`) and its interrupt check; `wait` returning false ends the wait with
    /// `Cancelled` (a cancel, a statement_timeout).
    pub fn claim_while(&self, key: Key, wait: &mut dyn FnMut() -> bool) -> Claim {
        let (cells, n) = self.cells();
        let gen = self.generation();
        let mut backoff = Backoff(0);
        'scan: loop {
            let mut free: Option<(usize, u32)> = None;
            for i in 0..n {
                // SAFETY: `i < slots`; fields other than `state` are read only in states past `CLAIMING`.
                let c = unsafe { &*cells.add(i) };
                let st = c.state.load(Ordering::Acquire);
                if st == CLAIMING {
                    if !backoff.snooze(wait) {
                        return Claim::Cancelled;
                    }
                    continue 'scan;
                }
                if st == FREE || c.generation != gen {
                    if free.is_none() {
                        free = Some((i, st));
                    }
                    continue;
                }
                if (c.part, c.epoch, c.fingerprint) != (key.part, key.epoch, key.fingerprint) {
                    continue;
                }
                match st {
                    READY => return Claim::Ready(c.table.ptr()),
                    BUILDING => {
                        // `owner` was written before the `Release` of `BUILDING` this load acquired
                        if c.owner == token() {
                            return Claim::Held;
                        }
                        while c.state.load(Ordering::Acquire) == BUILDING {
                            if !backoff.snooze(wait) {
                                return Claim::Cancelled;
                            }
                        }
                        continue 'scan;
                    }
                    _ => {
                        if c.state.compare_exchange(FAILED, CLAIMING, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                            return self.own(cells, i, key, gen);
                        }
                        continue 'scan;
                    }
                }
            }
            let Some((i, st)) = free else { return Claim::Full };
            // SAFETY: `i < slots`.
            let c = unsafe { &*cells.add(i) };
            if c.state.compare_exchange(st, CLAIMING, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return self.own(cells, i, key, gen);
            }
        }
    }

    fn own(&self, cells: *mut Cell, i: usize, key: Key, gen: u64) -> Claim {
        // SAFETY: this thread holds the cell in `CLAIMING`; nobody else reads the fields until `BUILDING`.
        unsafe {
            let c = cells.add(i);
            (*c).part = key.part;
            (*c).epoch = key.epoch;
            (*c).fingerprint = key.fingerprint;
            (*c).generation = gen;
            (*c).table = word(core::ptr::null());
            (*c).owner = token();
            (*c).state.store(BUILDING, Ordering::Release);
            Claim::Build(Builder { cell: c, published: false })
        }
    }
}

impl Builder {
    pub fn key(&self) -> Key {
        // SAFETY: the builder owns the cell.
        unsafe { Key { part: (*self.cell).part, epoch: (*self.cell).epoch, fingerprint: (*self.cell).fingerprint } }
    }

    pub fn publish(mut self, table: *const Header) {
        self.published = true;
        // SAFETY: the builder owns the cell; the table is written before the `Release`.
        unsafe {
            (*self.cell).table = word(table);
            (*self.cell).state.store(READY, Ordering::Release);
        }
    }
}

impl Drop for Builder {
    fn drop(&mut self) {
        if !self.published {
            // SAFETY: the builder owns the cell.
            unsafe { (*self.cell).state.store(FAILED, Ordering::Release) };
        }
    }
}

//! CONVERT wave 3 (W3-2b, rulings R1 (ii)): a door-1 memory-ledger charge that follows the heap bytes a load structure
//! holds, so a load's buffers sit on their statement's owner row by construction instead of the account's unattributed
//! row.
//!
//! A [`ColBuffer`](crate::ingest::ColBuffer), a [`RowChunk`](crate::par::RowChunk) or a
//! [`DictBuilder`](crate::dict::DictBuilder) holds several `Vec`s (some filled through unsafe fill faces or iterator
//! extends), so `memledger::ChargedVec` (one `Vec`) does not fit them: each keeps ONE `HeldCharge` instead, opened on
//! the owner the load passed it (read once on the COPY leader and handed down by value; never read on a worker) and
//! brought to the structure's held bytes (its blocks' capacity) by [`HeldCharge::sync`] on the thread that grew them.
//! The structure declares it FIRST so it drops first: its drop moves the bytes to the freeing thread's account, where
//! the blocks' frees land next (`memledger::Charge`'s drop). With no owner it is inert and the bytes stay where the
//! allocator put them (the account's unattributed row), exactly as before.

use memledger::charge::Door;
use memledger::{Charge, Kind, Owner, Release};

/// One structure's charge on its owner (`None`: none, and every call is a no-op).
#[derive(Debug)]
pub struct HeldCharge {
    charge: Charge,
    on: bool,
}

/// The release point a charge of `o` states (as `memledger::ChargedVec` states it).
fn release_of(o: Owner) -> Release {
    match o {
        Owner::Portal { .. } => Release::Portal,
        Owner::Session(_) => Release::Session,
        Owner::Cache(_) => Release::Evict,
        Owner::Statement { .. } | Owner::WorkerSet(_) | Owner::Op { .. } => Release::Statement,
    }
}

impl HeldCharge {
    /// No owner: nothing is charged.
    pub const fn none() -> HeldCharge {
        HeldCharge { charge: Charge::NONE, on: false }
    }

    /// A charge on `owner` (kind lane `Region`: a statement's load data, not a breaker's), 0 bytes until the first
    /// [`HeldCharge::sync`]; `None`: [`HeldCharge::none`].
    pub fn new(owner: Option<Owner>) -> HeldCharge {
        match owner {
            Some(o) => HeldCharge { charge: Charge::open(o, release_of(o), Kind::Region, Door::Heap, 0), on: true },
            None => HeldCharge::none(),
        }
    }

    /// The charge brought to `held` bytes on this thread: a commit of what the structure grew (blocks this thread
    /// allocated), a free of what it gave back. Call it on the thread that grew or shrank the blocks, right after.
    #[inline]
    pub fn sync(&mut self, held: u64) {
        if self.on && held != self.charge.committed() {
            self.charge.fit_committed(held);
        }
    }

    /// The owner the bytes are charged to (`None`: uncharged).
    pub fn owner(&self) -> Option<Owner> {
        self.on.then(|| self.charge.owner())
    }

    /// The bytes the charge holds now (0 uncharged).
    pub fn charged(&self) -> u64 {
        if self.on { self.charge.committed() } else { 0 }
    }
}

/// The bytes a block of `cap` elements of `v`'s element type holds (a `Vec`'s capacity times its element size), for
/// the held-bytes sums, without naming the container type.
#[inline]
pub fn cap_bytes<T>(cap: usize, _v: &[T]) -> u64 {
    (cap * core::mem::size_of::<T>()) as u64
}

impl Default for HeldCharge {
    fn default() -> HeldCharge {
        HeldCharge::none()
    }
}

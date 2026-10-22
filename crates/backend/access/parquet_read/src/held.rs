//! CONVERT wave 3 (cvwrite job 2; rulings R1 (ii)): a door-1 memory-ledger charge that follows the heap bytes a decode
//! structure holds, so a parquet load's decoder buffers (the row group's column chunks, a column's decompressed page,
//! levels and dictionary) sit on the loading statement's owner row by construction, not on the account's
//! unattributed row.
//!
//! The same shape as pgrc_write's `held::HeldCharge` (this crate cannot depend on pgrc_write): a structure that holds
//! several `Vec`s keeps ONE charge, opened on the owner its caller passed (read once on the COPY leader, handed down by
//! value) and brought to the structure's held bytes (its blocks' capacity) by [`HeldCharge::sync`] on the thread that
//! grew them. The structure declares it FIRST so it drops first: its drop moves the bytes to the freeing thread's
//! account, where the blocks' frees land next. With no owner it is inert (the bytes stay where the allocator put them).

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

    /// A charge on `owner` (kind lane `Region`: a statement's load data), 0 bytes until the first
    /// [`HeldCharge::sync`]; `None`: [`HeldCharge::none`].
    pub fn new(owner: Option<Owner>) -> HeldCharge {
        match owner {
            Some(o) => HeldCharge { charge: Charge::open(o, release_of(o), Kind::Region, Door::Heap, 0), on: true },
            None => HeldCharge::none(),
        }
    }

    /// The charge brought to `held` bytes on this thread. Call it on the thread that grew or shrank the blocks.
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

impl Default for HeldCharge {
    fn default() -> HeldCharge {
        HeldCharge::none()
    }
}

/// The bytes a block of `cap` elements of `v`'s element type holds (a `Vec`'s capacity times its element size).
#[inline]
pub fn cap_bytes<T>(cap: usize, _v: &[T]) -> u64 {
    (cap * core::mem::size_of::<T>()) as u64
}

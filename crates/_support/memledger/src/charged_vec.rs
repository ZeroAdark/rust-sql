//! Wave 2 R1: `ChargedVec<T>`, a `Vec` whose heap block is charged to an explicit owner by construction (door 1). The
//! block's CAPACITY is the charge's committed bytes, re-synced at every growth, and given back with the data. Unlike an
//! operator arena's carve it is Send / Sync as a `Vec` is: filled on one thread (a worker serving its leader) and freed
//! on another (the leader). Its bytes leave the filling thread's account at each commit and reach the freeing thread's
//! at the drop (`Charge::commit`, `Charge`'s drop), where the allocator counts the free. With no owner it is a plain
//! `Vec`: its bytes stay on the allocating thread's account (the account's unattributed row), as before.

use std::collections::TryReserveError;

use crate::charge::{release_of, Charge, Door, Owner};
use crate::Kind;

/// Class E (one value's copy, charged to its operator's owner): a copy below this many bytes is not charged, as the account
/// does not see it yet either (door 1 folds a thread's pending bytes onto its account in steps of `DOOR_FOLD_BYTES`).
pub const VALUE_FLOOR: usize = crate::DOOR_FOLD_BYTES as usize;

#[derive(Debug)]
pub struct ChargedVec<T> {
    /// Declared first, so dropped first: its door-1 drop moves the block's bytes to the freeing thread's account, where
    /// `v`'s free lands next.
    charge: Charge,
    /// `charge` is open on an owner (`Charge::NONE` takes no commit).
    on: bool,
    /// What `charge` holds committed: `v`'s capacity in bytes at the last re-sync.
    charged: u64,
    v: Vec<T>,
}

/// The release point a charge of `o` states (read back only, `Charge::release`): its own end for a statement, a portal
/// or a session; a cache's eviction; a worker set's or an operator's statement.

impl<T> ChargedVec<T> {
    /// Empty, charged to `owner` on kind lane `kind` as it grows; `None`: uncharged.
    pub fn new(owner: Option<Owner>, kind: Kind) -> ChargedVec<T> {
        match owner {
            Some(o) => ChargedVec { charge: Charge::open(o, release_of(o), kind, Door::Heap, 0), on: true, charged: 0, v: Vec::new() },
            None => ChargedVec::none(),
        }
    }

    /// Empty and uncharged (`Default`; usable in a `const fn`).
    pub const fn none() -> ChargedVec<T> {
        ChargedVec { charge: Charge::NONE, on: false, charged: 0, v: Vec::new() }
    }

    /// `it`'s items in a vec charged to `owner` on `kind` (`None`: uncharged): one reservation for its lower size hint, then a
    /// fallible push per item; a refusal (capacity overflow, the allocator's) drops what was built and returns it.
    pub fn try_from_iter(owner: Option<Owner>, kind: Kind, it: impl IntoIterator<Item = T>) -> Result<ChargedVec<T>, TryReserveError> {
        let it = it.into_iter();
        let mut v = ChargedVec::new(owner, kind);
        v.try_reserve(it.size_hint().0)?;
        for x in it {
            v.push(x)?;
        }
        Ok(v)
    }

    /// Room for `additional` more elements; a refusal (capacity overflow, the allocator's) leaves it as it was.
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), TryReserveError> {
        if self.v.capacity() - self.v.len() >= additional {
            return Ok(());
        }
        let r = self.v.try_reserve(additional); // alloc: charged self.charge.owner()
        self.recharge();
        r
    }

    pub fn push(&mut self, x: T) -> Result<(), TryReserveError> {
        self.try_reserve(1)?;
        self.v.push(x); // alloc: charged self.charge.owner()
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.v.len()
    }

    pub fn is_empty(&self) -> bool {
        self.v.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.v.capacity()
    }

    /// No elements; the block (and its charge) stays for the next fill.
    pub fn clear(&mut self) {
        self.v.clear();
    }

    pub fn as_slice(&self) -> &[T] {
        &self.v
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.v
    }

    /// The charge re-synced to the block's capacity now: a commit of what it grew (allocated on this thread), a free of
    /// what it shrank. `with_vec` calls it; a caller that grows the block another way calls it after.
    pub fn recharge(&mut self) {
        if !self.on {
            return;
        }
        let now = (self.v.capacity() as u64).saturating_mul(core::mem::size_of::<T>() as u64);
        if now > self.charged {
            self.charge.commit(now - self.charged);
        } else if now < self.charged {
            self.charge.free(self.charged - now);
        }
        self.charged = now;
    }

    /// `f` on the inner `Vec` (a writer that takes a `&mut Vec<T>`, e.g. a framer), the charge re-synced after it.
    pub fn with_vec<R>(&mut self, f: impl FnOnce(&mut Vec<T>) -> R) -> R {
        let r = f(&mut self.v); // alloc: charged self.charge.owner()
        self.recharge();
        r
    }

    /// The inner `Vec` behind a guard, for a holder that grows it across many calls (a section writer that hands each
    /// codec call a `&mut Vec<u8>`): the guard derefs to it, `sync` fits the charge to its capacity, and its drop does too
    /// (an error path included). `with_vec` is the closure form.
    ///
    /// The lag bound: what the block grew since the last `sync` is on the growing thread's account until the next one,
    /// so the holder calls `sync` after each call that may grow it (each codec call, or each section): a sample never
    /// sees more than one call's growth unattributed. A realloc in between nets out at the sync (new capacity minus the
    /// charged one); nothing double counts as long as each growth is synced before the next. The guard is neither Send
    /// nor Sync: its syncs run on the thread whose growth door 1 counted.
    pub fn vec_mut(&mut self) -> ChargedVecMut<'_, T> {
        ChargedVecMut { cv: self, _one_thread: core::marker::PhantomData }
    }

    /// The owner the block is charged to (`None`: uncharged).
    pub fn owner(&self) -> Option<Owner> {
        self.on.then(|| self.charge.owner())
    }

    /// The bytes the charge holds committed (the block's capacity at the last re-sync; 0 uncharged).
    pub fn charged_bytes(&self) -> u64 {
        if self.on { self.charged } else { 0 }
    }
}

impl<T: Copy> ChargedVec<T> {
    pub fn extend_from_slice(&mut self, s: &[T]) -> Result<(), TryReserveError> {
        self.try_reserve(s.len())?;
        self.v.extend_from_slice(s); // alloc: charged self.charge.owner()
        Ok(())
    }

    /// `n` elements: the first `len` kept, the rest `x` (`Vec::resize`; a shorter `n` truncates, the block kept).
    pub fn resize(&mut self, n: usize, x: T) -> Result<(), TryReserveError> {
        if n > self.v.len() {
            self.try_reserve(n - self.v.len())?;
        }
        self.v.resize(n, x); // alloc: charged self.charge.owner()
        Ok(())
    }
}

/// `ChargedVec::vec_mut`'s guard: the inner `Vec` for in-place growth, its charge fitted to the capacity at `sync` and at
/// the drop. On one thread (not Send / Sync), the one that grows the block.
#[derive(Debug)]
pub struct ChargedVecMut<'a, T> {
    cv: &'a mut ChargedVec<T>,
    _one_thread: core::marker::PhantomData<*const ()>,
}

impl<T> ChargedVecMut<'_, T> {
    /// The charge fitted to the block's capacity (not its length) now (`ChargedVec::recharge`): a commit of what it grew,
    /// a free of what it shrank. Call it after each call that may have grown the block (the lag bound, `vec_mut`).
    pub fn sync(&mut self) {
        self.cv.recharge();
    }

    /// The owner the block is charged to (`None`: uncharged).
    pub fn owner(&self) -> Option<Owner> {
        self.cv.owner()
    }
}

impl<T> core::ops::Deref for ChargedVecMut<'_, T> {
    type Target = Vec<T>;
    fn deref(&self) -> &Vec<T> {
        &self.cv.v
    }
}

impl<T> core::ops::DerefMut for ChargedVecMut<'_, T> {
    fn deref_mut(&mut self) -> &mut Vec<T> {
        &mut self.cv.v
    }
}

impl<T> Drop for ChargedVecMut<'_, T> {
    fn drop(&mut self) {
        self.cv.recharge();
    }
}

impl<T> Default for ChargedVec<T> {
    fn default() -> ChargedVec<T> {
        ChargedVec::none()
    }
}

impl<T> core::ops::Deref for ChargedVec<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.v
    }
}

impl<T> core::ops::DerefMut for ChargedVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.v
    }
}

//! LEDGER2 step 2: a context's owner in the memory ledger (`memledger::charge::CtxOwner`), on its `Acct`. Its door-1 block
//! bytes follow the context's footprint (`sync`, at every footprint update), so a block's owner is its context's, never the
//! thread's that happens to allocate or free it; a mapped block (door 2) maps for the same owner (`heap::allocate_for`),
//! and a block source's block posts to it at its take and give (`pooled_in` / `pooled_out`), on no thread's account.
//! The owner is posted to in server (std) builds only.

use core::alloc::Layout;
use core::cell::Cell;

pub(crate) struct Led {
    #[cfg(feature = "std")]
    owner: Cell<memledger::charge::CtxOwner>,
    // door-1 bytes on the owner now: what a re-own moves and the context's drop takes back
    #[cfg(feature = "std")]
    posted: Cell<i64>,
    // of the footprint, the blocks of heap::MMAP_THRESHOLD and more: door 2's, on the owner from their map
    mapped: Cell<usize>,
    // of the footprint, the block source's blocks: on the owner from their take (`CtxOwner::post_pool`)
    pooled: Cell<usize>,
    // the class every pooled block of the context was taken in (`heap::Lifetime as u8`) while they agree, MIXED once two
    // differ, until none is left: a give names it to the source (`pooled_life`)
    pooled_life: Cell<u8>,
}

/// `Led::pooled_life`'s byte once a context's pooled blocks were taken in more than one class.
const MIXED: u8 = u8::MAX;

impl Led {
    #[allow(clippy::declare_interior_mutable_const)]
    pub(crate) const NEW: Led = Led {
        #[cfg(feature = "std")]
        owner: Cell::new(memledger::charge::CtxOwner::NONE),
        #[cfg(feature = "std")]
        posted: Cell::new(0),
        mapped: Cell::new(0),
        pooled: Cell::new(0),
        pooled_life: Cell::new(MIXED),
    };

    /// The context's footprint is `footprint` bytes now: its door-1 part posts the difference to the owner.
    #[inline]
    pub(crate) fn sync(&self, footprint: usize) {
        #[cfg(feature = "std")]
        {
            let d = (footprint.saturating_sub(self.mapped.get().saturating_add(self.pooled.get())) as i64).wrapping_sub(self.posted.get());
            if d != 0 {
                self.post(d);
            }
        }
        #[cfg(not(feature = "std"))]
        let _ = footprint;
    }

    #[cfg(feature = "std")]
    #[cold]
    #[inline(never)]
    fn post(&self, d: i64) {
        let mut o = self.owner.get();
        o.post(d);
        self.owner.set(o);
        self.posted.set(self.posted.get().wrapping_add(d));
    }

    /// A block of `layout` mapped for the context (`heap::allocate_for`), or freed (`unmapped`).
    #[inline]
    pub(crate) fn mapped_in(&self, layout: Layout) {
        if crate::heap::mapped(layout) {
            self.mapped.set(self.mapped.get() + layout.size());
        }
    }

    #[inline]
    pub(crate) fn unmapped(&self, layout: Layout) {
        if crate::heap::mapped(layout) {
            self.mapped.set(self.mapped.get().saturating_sub(layout.size()));
        }
    }

    /// A reset left at most one block (its keeper, `footprint` bytes): mapped only at the threshold. The blocks a reset
    /// keeps never come from the block source (`heap::allocate_kept_for`) but an AllocSet's (`reset_blocks`).
    #[inline]
    pub(crate) fn reset_mapped(&self, footprint: usize) {
        self.reset_blocks(footprint, 0);
    }

    /// `reset_mapped` where `pooled` bytes of the blocks kept are the source's.
    #[inline]
    pub(crate) fn reset_blocks(&self, footprint: usize, pooled: usize) {
        let own = footprint.saturating_sub(pooled);
        self.mapped.set(if own >= crate::heap::MMAP_THRESHOLD { own } else { 0 });
        let was = self.pooled.replace(pooled);
        if was != pooled {
            self.post_pool(pooled as i64 - was as i64);
        }
    }

    /// A block of `bytes` from the block source (`heap::allocate_for`), taken in `life`'s class, or given back
    /// (`pooled_out`), or resized there (in its class).
    #[inline]
    pub(crate) fn pooled_in(&self, bytes: usize, life: crate::heap::Lifetime) {
        let l = life as u8;
        if self.pooled.get() == 0 {
            self.pooled_life.set(l);
        } else if self.pooled_life.get() != l {
            self.pooled_life.set(MIXED);
        }
        self.pooled.set(self.pooled.get() + bytes);
        self.post_pool(bytes as i64);
    }

    /// The class every pooled block of the context was taken in, when one class holds them all (a give names it to the
    /// source: no lookup there); None once they mixed (until none is left) or before the first.
    #[inline]
    pub(crate) fn pooled_life(&self) -> Option<crate::heap::Lifetime> {
        crate::heap::Lifetime::from_u8(self.pooled_life.get())
    }

    #[inline]
    pub(crate) fn pooled_out(&self, bytes: usize) {
        debug_assert!(self.pooled.get() >= bytes, "mcx: a pooled block given back past its context's pooled bytes");
        self.pooled.set(self.pooled.get().saturating_sub(bytes));
        self.post_pool(-(bytes as i64));
    }

    #[inline]
    pub(crate) fn pooled_resized(&self, old: usize, new: usize) {
        debug_assert!(self.pooled.get() >= old);
        self.pooled.set(self.pooled.get().saturating_sub(old) + new);
        self.post_pool(new as i64 - old as i64);
    }

    #[cfg_attr(not(feature = "std"), allow(unused_variables))]
    #[cold]
    #[inline(never)]
    fn post_pool(&self, d: i64) {
        #[cfg(feature = "std")]
        {
            let mut o = self.owner.get();
            o.post_pool(d);
            self.owner.set(o);
        }
    }

    /// The class a block taken now lives in: a cache scope's, else the owner's (read at the take, off the hot paths).
    pub(crate) fn lifetime(&self) -> crate::heap::Lifetime {
        #[cfg(feature = "std")]
        {
            if memledger::arena::in_cache_scope() {
                return crate::heap::Lifetime::Cache;
            }
            // an owner kept past its end reads its session's (`CtxOwner::lifetime` follows first, as a post does)
            let mut o = self.owner.get();
            let life = life_of(&mut o);
            self.owner.set(o);
            life
        }
        #[cfg(not(feature = "std"))]
        crate::heap::Lifetime::Session
    }

    /// The new context's owner (its parent's, else the root's): its footprint so far (a recycled keeper) posts to it.
    #[cfg(feature = "std")]
    pub(crate) fn attach(&self, o: memledger::charge::CtxOwner, footprint: usize) {
        self.owner.set(o);
        self.sync(footprint);
    }

    #[cfg(feature = "std")]
    pub(crate) fn owner(&self) -> memledger::charge::CtxOwner {
        self.owner.get()
    }

    #[cfg(feature = "std")]
    pub(crate) fn set_owner(&self, o: memledger::charge::CtxOwner) {
        self.owner.set(o);
    }

    /// A new owner for the context (`MemoryContext::set_owner`): its door-1 bytes move with it in one step.
    #[cfg(feature = "std")]
    pub(crate) fn retag(&self, to: memledger::charge::CtxOwner) {
        let mut o = self.owner.get();
        let p = self.pooled.get() as i64;
        if p != 0 {
            o.post_pool(-p);
        }
        o.transfer(to, self.posted.get());
        if p != 0 {
            o.post_pool(p);
        }
        self.owner.set(o);
    }
}

#[cfg(feature = "std")]
fn life_of(o: &mut memledger::charge::CtxOwner) -> crate::heap::Lifetime {
    use crate::heap::Lifetime;
    use memledger::OwnerLife;
    match o.lifetime() {
        OwnerLife::Statement => Lifetime::Task,
        OwnerLife::Session => Lifetime::Session,
        OwnerLife::Cache => Lifetime::Cache,
        OwnerLife::Process => Lifetime::Process,
    }
}

#[cfg(test)]
mod tests {
    use super::Led;
    use crate::heap::Lifetime;

    const K64: usize = 64 << 10;

    /// pagepool P2 (E3b): a give names the class its context's pooled blocks were taken in only while one class holds
    /// them all; mixed until none is left, then the next take's.
    #[test]
    fn a_led_names_its_pooled_class_while_one_class_holds_them_all() {
        let led = Led::NEW;
        assert_eq!(led.pooled_life(), None, "no pooled block yet");
        led.pooled_in(K64, Lifetime::Task);
        led.pooled_in(2 * K64, Lifetime::Task);
        assert_eq!(led.pooled_life(), Some(Lifetime::Task));
        led.pooled_in(K64, Lifetime::Session);
        assert_eq!(led.pooled_life(), None, "two classes: no name");
        led.pooled_out(K64);
        led.pooled_out(2 * K64);
        assert_eq!(led.pooled_life(), None, "mixed until none is left");
        led.pooled_out(K64);
        led.pooled_in(K64, Lifetime::Cache);
        assert_eq!(led.pooled_life(), Some(Lifetime::Cache), "none left: the next take's class");
        led.pooled_in(K64, Lifetime::Cache);
        led.reset_blocks(0, 0);
        led.pooled_in(K64, Lifetime::Session);
        assert_eq!(led.pooled_life(), Some(Lifetime::Session), "a reset that kept no pooled block: the next take's");
        led.reset_blocks(K64, K64);
        led.pooled_in(K64, Lifetime::Process);
        assert_eq!(led.pooled_life(), None, "a kept pooled block of another class: mixed");
    }
}

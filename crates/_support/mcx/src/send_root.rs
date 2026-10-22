
use core::alloc::Layout;
use core::ptr::NonNull;

use allocator_api2::alloc::{AllocError, Allocator};

use crate::{Mcx, MemoryContext};

/// A product's root context: it moves between threads with what it holds and is shared read-only once built. Unobserved,
/// parentless, and it refuses children and reset callbacks; every write takes `&mut self`.
pub struct SendRoot(NonNull<MemoryContext>);

// SAFETY: the context's state (its `Cell`s, its non-atomic `AcctRc`) is reached only through this value: it has no parent
// and no observer entry (`new_unobserved`), it refuses children and reset callbacks, and every `Mcx` into it borrows the
// root (`mcx` takes `&mut self`), so it moves only when nothing on the old thread can touch it. A `RootA` is neither `Send`
// nor `Sync`: it moves with the root only inside a holder whose own `Send` answers for both (`handle`). The ledger posts a
// block's bytes across accounts by atomics (`CtxOwner::post`); the block pools it frees into are per thread or locked. The
// box is raw (as `PinnedContext`): a move of the root retags nothing a `RootA` points through.
unsafe impl Send for SendRoot {}
// SAFETY: through `&SendRoot` only `used` and `allocated` read the context; every write (an allocation, a free, a re-own)
// needs `&mut SendRoot`, which no other reference can share.
unsafe impl Sync for SendRoot {}

impl SendRoot {
    pub const HEADER_BYTES: u64 = core::mem::size_of::<MemoryContext>() as u64;

    /// For a root that dies inside its creating statement; one that outlives it gets its owner at birth (`new_owned`).
    pub fn new(name: &'static str) -> SendRoot {
        SendRoot::of(MemoryContext::new_unobserved(name))
    }

    /// `new` over a 1 KiB keeper (`MemoryContext::new_unobserved_small`), for a product of a few small lists.
    pub fn new_small(name: &'static str) -> SendRoot {
        SendRoot::of(MemoryContext::new_unobserved_small(name))
    }

    fn of(ctx: MemoryContext) -> SendRoot {
        ctx.acct.send_root.set(true);
        SendRoot(NonNull::from(alloc::boxed::Box::leak(alloc::boxed::Box::new(ctx))))
    }

    #[cfg(feature = "std")]
    pub fn new_owned(name: &'static str, o: memledger::Owner) -> SendRoot {
        let mut r = SendRoot::new(name);
        r.set_owner(o);
        r
    }

    /// `new_owned` that takes nothing from the creating thread's pools and gives nothing to the dropping thread's
    /// (`MemoryContext::new_unobserved_unpooled`): a product built in an account's home and dropped in that home on another
    /// thread (a code cache entry), so every block of it is allocated and freed on that account.
    #[cfg(feature = "std")]
    pub fn new_owned_unpooled(name: &'static str, o: memledger::Owner) -> SendRoot {
        let mut r = SendRoot::of(MemoryContext::new_unobserved_unpooled(name));
        r.set_owner(o);
        r
    }

    #[cfg(feature = "std")]
    pub fn new_owned_small(name: &'static str, o: memledger::Owner) -> SendRoot {
        let mut r = SendRoot::new_small(name);
        r.set_owner(o);
        r
    }

    fn ctx(&self) -> &MemoryContext {
        // SAFETY: the box is this value's until its drop.
        unsafe { self.0.as_ref() }
    }

    pub fn mcx(&mut self) -> Mcx<'_> {
        self.ctx().mcx()
    }

    #[cfg(feature = "std")]
    pub fn set_owner(&mut self, o: memledger::Owner) {
        self.ctx().set_owner(o);
    }

    pub fn used(&self) -> usize {
        self.ctx().used()
    }

    pub fn allocated(&self) -> usize {
        self.ctx().mem_allocated()
    }

    /// # Safety
    /// Every block allocated through the handle is freed before this root drops; the handle allocates, grows, shrinks or
    /// frees only under `&mut` of the value that holds both it and this root; and that holder moves or is shared across
    /// threads only whole, root and handles together, with nothing allocating through a shared reference.
    pub unsafe fn handle(&mut self) -> RootA {
        RootA(self.0)
    }

    pub fn holds(&self, a: &RootA) -> bool {
        a.0 == self.0
    }
}

impl Drop for SendRoot {
    fn drop(&mut self) {
        let live = self.used();
        if live != 0 {
            // A block outlived its holder (`handle`'s contract broken): leak the context (its owner still shows it).
            debug_assert!(false, "mcx: SendRoot {:?} dropped with {live} bytes live", self.ctx().name());
            return;
        }
        // SAFETY: made by `Box::leak` in `new` and taken back once; nothing borrows it past the root (`mcx` borrows it,
        // and no block is live).
        drop(unsafe { alloc::boxed::Box::from_raw(self.0.as_ptr()) });
    }
}

impl core::fmt::Debug for SendRoot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SendRoot").field("name", &self.ctx().name()).field("used", &self.used()).finish()
    }
}

/// A lifetime-free allocator into a `SendRoot` (`handle`), for `Vec<T, RootA>` held beside the root. Not `Clone` or
/// `Default`, so no collection allocates through a shared reference; not `Send` or `Sync`, so two parts of one holder
/// never grow on two threads (the holder alone may be `Send` / `Sync`).
#[doc = "```compile_fail"]
#[doc = "fn send<T: Send>() {}"]
#[doc = "send::<mcx::RootA>();"]
#[doc = "```"]
pub struct RootA(NonNull<MemoryContext>);

impl RootA {
    /// # Safety
    /// As `SendRoot::handle`'s: the new handle is held beside the same root, by the same holder.
    pub unsafe fn again(&self) -> RootA {
        RootA(self.0)
    }

    #[inline(always)]
    fn mcx(&self) -> Mcx<'_> {
        // SAFETY: `SendRoot::handle`'s contract keeps the root's context alive while its blocks are live. Through
        // `MemoryContext::mcx`, so debug builds record the root's (boxed, never moving) address for `check_home`.
        unsafe { self.0.as_ref() }.mcx()
    }
}

impl core::fmt::Debug for RootA {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("RootA")
    }
}

// SAFETY (trait contract): delegates to the root's `Mcx`, whose blocks stay valid while the root lives (`handle`).
unsafe impl Allocator for RootA {
    #[inline(always)]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        Allocator::allocate(&self.mcx(), layout)
    }

    #[inline]
    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        Allocator::allocate_zeroed(&self.mcx(), layout)
    }

    #[inline]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { Allocator::deallocate(&self.mcx(), ptr, layout) }
    }

    #[inline]
    unsafe fn grow(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's contract, passed on.
        unsafe { Allocator::grow(&self.mcx(), ptr, old, new) }
    }

    #[inline]
    unsafe fn grow_zeroed(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's contract, passed on.
        unsafe { Allocator::grow_zeroed(&self.mcx(), ptr, old, new) }
    }

    #[inline]
    unsafe fn shrink(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // SAFETY: the caller's contract, passed on.
        unsafe { Allocator::shrink(&self.mcx(), ptr, old, new) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_root_moves_to_another_thread_with_what_it_holds() {
        let mut root = SendRoot::new("SendRootMove");
        let n = {
            let mut v = crate::PgVec::<u64>::new_in(root.mcx());
            v.extend(0..1000);
            v.iter().sum::<u64>()
        };
        assert_eq!(n, 499_500);
        let used = std::thread::spawn(move || {
            let held = {
                let m = root.mcx();
                let mut v = crate::PgVec::<u8>::with_capacity_in(4096, m);
                v.resize(4096, 7);
                m.context().used()
            };
            assert!(root.used() < held);
            held
        })
        .join()
        .unwrap();
        assert!(used >= 4096);
    }

    struct Product {
        data: allocator_api2::vec::Vec<u32, RootA>,
        root: SendRoot,
    }

    // SAFETY: `data` is the root's only handle and moves with it; `&Product` reads only.
    unsafe impl Send for Product {}
    // SAFETY: as `Send`.
    unsafe impl Sync for Product {}

    impl Product {
        fn new(n: u32) -> Product {
            let mut root = SendRoot::new("SendRootProduct");
            // SAFETY: `data` is the only holder and drops before `root` (field order).
            let mut data = allocator_api2::vec::Vec::new_in(unsafe { root.handle() });
            data.extend(0..n);
            Product { data, root }
        }
    }

    #[test]
    fn a_frozen_product_is_read_on_many_threads_and_dropped_on_another() {
        let p = Arc::new(Product::new(10_000));
        assert!(p.root.holds(p.data.allocator()));
        assert!(p.root.used() >= 40_000);
        let sums: std::vec::Vec<u64> = (0..4)
            .map(|_| {
                let p = p.clone();
                std::thread::spawn(move || p.data.iter().map(|&x| x as u64).sum::<u64>())
            })
            .collect::<std::vec::Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect();
        assert!(sums.iter().all(|&s| s == 49_995_000));
        std::thread::spawn(move || drop(p)).join().unwrap();
    }

    #[test]
    #[should_panic(expected = "SendRoot")]
    fn a_root_refuses_a_child() {
        let mut root = SendRoot::new("SendRootChild");
        let _child = root.mcx().context().new_child("child");
    }

    #[test]
    #[should_panic(expected = "SendRoot")]
    fn a_root_refuses_a_reset_callback() {
        let mut root = SendRoot::new("SendRootCallback");
        root.mcx().context().register_reset_callback(|| {});
    }

    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "bytes live"))]
    fn a_root_dropped_under_a_live_block_is_kept() {
        let mut root = SendRoot::new("SendRootLive");
        // SAFETY: broken on purpose (the block outlives the root); the root must keep its context for `v`.
        let mut v = allocator_api2::vec::Vec::<u64, RootA>::new_in(unsafe { root.handle() });
        v.extend(0..64);
        drop(root);
        assert_eq!(v.iter().sum::<u64>(), 2016);
    }

    /// A small root's first block is the 1 KiB keeper class, the default's 8 KiB, and each is its owner's from its birth.
    #[cfg(feature = "std")]
    #[test]
    fn a_small_roots_keeper_is_one_kilobyte_and_its_owners() {
        std::thread::spawn(|| {
            memledger::bind_backend(4071, 9071);
            let s = memledger::Owner::Session(memledger::Acct::backend(4071));
            let s0 = memledger::charge::owner_ctx(s).0;
            let mut small = SendRoot::new_owned_small("SendRootSmall", s);
            let mut big = SendRoot::new_owned("SendRootDefault", s);
            for r in [&mut small, &mut big] {
                let mut v = crate::PgVec::<u64>::new_in(r.mcx());
                v.push(7);
            }
            assert_eq!((small.allocated(), big.allocated()), (crate::ALLOCSET_SMALL_INITSIZE, crate::aset::INIT_BLOCK_SIZE));
            let held = memledger::charge::owner_ctx(s).0 - s0;
            std::eprintln!("small keeper witness: a small root {} B, a default root {} B, on their owner {held} B", small.allocated(), big.allocated());
            assert_eq!(held, (small.allocated() + big.allocated()) as i64, "the keepers are the owner's, not unattributed");
            drop((small, big));
            assert_eq!(memledger::charge::owner_ctx(s).0, s0);
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_root_is_send_and_sync() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<SendRoot>();
        send_sync::<Product>();
    }
}

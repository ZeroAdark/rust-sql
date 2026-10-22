// A handle (Mcx) is the context's address. Safe code cannot move a context while a handle borrows it, but unsafe
// code that lifetime-extends a handle over a context held by value, then moves the context (into an Rc, a Box, a Vec),
// leaves the handle at the old address. Debug builds name such a handle at its next allocation (`check_home`) instead
// of reading the old bytes as a context (the relcache defaults case: "memory allocation of 128 bytes failed").
// A lifetime-extended handle comes from an address-stable owner: PinnedContext (made with `new_child*`, or the context
// pinned before any handle is taken), McxOwned, SendRoot.
//
// Run: cargo test -p mcx --test moved_context (a debug build; release builds carry no check)

use core::alloc::Layout;
use mcx::{Allocator, Mcx, MemoryContext, PinnedContext};

/// A handle extended to 'static the way the defaults cache did it (deliberately unsound: the case under test).
fn extended(ctx: &MemoryContext) -> Mcx<'static> {
    // SAFETY: none. The tests below move the context out from under this handle on purpose.
    unsafe { core::mem::transmute::<Mcx<'_>, Mcx<'static>>(ctx.mcx()) }
}

/// The moved copy hands out a handle at its new address, and the old bytes are still the context's stale copy.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "mcx: a memory context moved after a handle to it was taken: an Mcx still points at its old address (context \"moved after its handle\")")]
fn a_handle_left_at_a_moved_contexts_old_address_is_named() {
    let ctx = MemoryContext::new("moved after its handle");
    let stale = extended(&ctx);
    let moved = Box::new(ctx);
    let _fresh = moved.mcx().alloc_bytes(64).unwrap();
    let _ = stale.alloc_bytes(64);
}

/// The old address has been reused (the defaults cache's stack slot): the handle's own record no longer names it.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "mcx: a memory context moved after a handle to it was taken: an Mcx still points at its old address (context (its old address now holds other bytes))")]
fn a_handle_left_at_a_reused_address_is_named() {
    let mut slot = core::mem::MaybeUninit::new(MemoryContext::new("moved, then its slot reused"));
    // SAFETY: `slot` holds an initialized context until it is read out below.
    let stale = extended(unsafe { slot.assume_init_ref() });
    // SAFETY: read out once; the slot is not read as a context again.
    let moved = Box::new(unsafe { slot.assume_init_read() });
    let _fresh = moved.mcx().alloc_bytes(64).unwrap();
    // SAFETY: the slot is plain bytes now (its context moved out above).
    unsafe { core::ptr::write_bytes(slot.as_mut_ptr().cast::<u8>(), 0xA5, core::mem::size_of::<MemoryContext>()) };
    let _ = stale.alloc_bytes(64);
}

/// D4483: the check also runs on the pool hit that Mcx::alloc_fast inlines at every allocation site, not only on the
/// slow path. The context's active block has room when its handle goes stale (the twin test below shows the same
/// request is a hit), so without the check the stale handle's allocation would be served from the old bytes' pool.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "mcx: a memory context moved after a handle to it was taken: an Mcx still points at its old address (context \"moved after a pool hit\")")]
fn a_pool_hit_through_a_handle_left_at_a_moved_contexts_old_address_is_named() {
    let ctx = MemoryContext::new("moved after a pool hit");
    ctx.mcx().allocate(Layout::from_size_align(64, 8).unwrap()).unwrap();
    let stale = extended(&ctx);
    let moved = Box::new(ctx);
    let _fresh = moved.mcx().allocate(Layout::from_size_align(64, 8).unwrap()).unwrap();
    let _ = stale.allocate(Layout::from_size_align(64, 8).unwrap());
}

/// The twin of the test above, not moved: its second 64-byte request opens no block, i.e. it is a pool hit.
#[test]
fn the_second_small_allocation_of_a_context_is_a_pool_hit() {
    let ctx = MemoryContext::new("pool hit twin");
    ctx.mcx().allocate(Layout::from_size_align(64, 8).unwrap()).unwrap();
    let footprint = ctx.mem_allocated();
    ctx.mcx().allocate(Layout::from_size_align(64, 8).unwrap()).unwrap();
    assert_eq!(ctx.mem_allocated(), footprint);
}

/// No false alarm: a context moved after its handles ended hands out new ones at its new address, and a pinned child's
/// handle outlives a move of its owner (the context stays where it was boxed).
#[test]
fn a_context_moved_after_its_handles_end_and_a_pinned_child_are_not_flagged() {
    let ctx = MemoryContext::new("moved after its handles ended");
    {
        let m = ctx.mcx();
        m.alloc_bytes(64).unwrap();
        let mut v = mcx::PgVec::<u64>::new_in(m);
        v.push(1);
    }
    let moved = Box::new(ctx);
    moved.mcx().alloc_bytes(64).unwrap();
    let mut v = mcx::PgVec::<u64>::new_in(moved.mcx());
    v.extend([1, 2, 3, 4, 5, 6, 7, 8, 9]);
    drop(v);

    let parent = MemoryContext::new("pinned parent");
    let pinned = PinnedContext::new_child_small(&parent, "pinned child");
    // SAFETY: the handle and everything it allocates die before `owner` drops below.
    let h = unsafe { pinned.handle() };
    let owner = vec![pinned];
    let mut v = mcx::PgVec::<u64>::new_in(h);
    v.extend([1, 2, 3, 4, 5, 6, 7, 8, 9]);
    h.alloc_bytes(64).unwrap();
    drop(v);
    drop(owner);

    let pinned = PinnedContext::new_child(&parent, "pinned child, default size");
    // SAFETY: as above.
    let h = unsafe { pinned.handle() };
    let owner = Box::new(pinned);
    h.alloc_bytes(64).unwrap();
    drop(owner);
}

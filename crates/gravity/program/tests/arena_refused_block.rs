//! D3957: an arena under the mapped threshold is the global allocator's block (decompose normalize's 64 KiB); a refused
//! allocation there is the statement's ERROR 53200 from `Arena::new` and `try_new`'s `None`, never `handle_alloc_error`'s
//! abort. Its own test binary: this allocator refuses the next zeroed request of at least the armed size on this thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use gravity_program::Arena;

std::thread_local! {
    static ARMED: Cell<usize> = const { Cell::new(0) };
}

struct Refusing;

// SAFETY: `System`'s, but for the one armed zeroed request, refused (null) as any allocator may refuse a request.
unsafe impl GlobalAlloc for Refusing {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        unsafe { System.alloc(l) }
    }

    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let armed = ARMED.with(Cell::get);
        if armed != 0 && l.size() >= armed {
            ARMED.with(|a| a.set(0));
            return core::ptr::null_mut();
        }
        unsafe { System.alloc_zeroed(l) }
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static REFUSING: Refusing = Refusing;

const NORMALIZE: usize = 64 << 10;

#[test]
fn a_refused_boxed_block_raises_53200_never_the_abort() {
    ARMED.with(|a| a.set(NORMALIZE));
    let payload = std::panic::catch_unwind(|| drop(Arena::new(NORMALIZE))).expect_err("the refused block raises");
    let e = payload.downcast_ref::<types_error::PgError>().expect("a PgError payload (an ERROR), not a plain panic");
    assert_eq!(e.sqlstate(), types_error::ERRCODE_OUT_OF_MEMORY);
    assert_eq!(e.message(), "out of memory");
    ARMED.with(|a| a.set(NORMALIZE));
    assert!(Arena::try_new(NORMALIZE).is_none(), "try_new: the refusal as None");
    assert!(Arena::try_new(NORMALIZE).is_some(), "disarmed: the allocator's block");
}

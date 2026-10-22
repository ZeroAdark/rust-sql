// Out-of-memory reporting must not need memory (C: ErrorContext's reserved
// space). This binary's global allocator refuses every request on a thread
// while that thread's FAIL flag is set, so the arena's block request, the
// error's Box and its detail String all fail together.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use mcx::MemoryContext;
use types_error::ERRCODE_OUT_OF_MEMORY;

thread_local! {
    static FAIL: Cell<bool> = const { Cell::new(false) };
}

struct Refusing;

unsafe impl GlobalAlloc for Refusing {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if FAIL.with(Cell::get) {
            return std::ptr::null_mut();
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Refusing = Refusing;

#[test]
fn exhausted_allocator_still_reports_53200() {
    let ctx = MemoryContext::new("OomReserveTest");
    let mut errors = Vec::with_capacity(3);
    for _ in 0..3 {
        FAIL.with(|f| f.set(true));
        let r = mcx::vec_with_capacity_in::<u64>(ctx.mcx(), 1 << 20);
        FAIL.with(|f| f.set(false));
        errors.push(r.expect_err("allocation must fail while the allocator refuses"));
    }
    for e in &errors {
        assert_eq!(e.sqlstate(), ERRCODE_OUT_OF_MEMORY);
        assert_eq!(e.message(), "out of memory");
        assert_eq!(
            e.detail.as_deref(),
            Some("Failed on request of size 8388608 in memory context \"OomReserveTest\".")
        );
    }
    drop(errors);

    let limited = MemoryContext::new("Limited").with_limit(64);
    let e = mcx::vec_with_capacity_in::<u64>(limited.mcx(), 1 << 10).unwrap_err();
    assert_eq!(e.sqlstate(), ERRCODE_OUT_OF_MEMORY);
    assert_eq!(
        e.detail.as_deref(),
        Some("Failed on request of size 8192 in memory context \"Limited\".")
    );
}

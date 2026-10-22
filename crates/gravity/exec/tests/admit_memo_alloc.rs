//! A counting global allocator (its own binary, so no other test's
//! allocations land in the count): the memo's hits, refusals and inserts
//! allocate nothing.

use std::alloc::{GlobalAlloc, Layout, System};

use gravity_exec::admit::memo::{PlanMemo, Verdict, CAPACITY};
use gravity_program::{Generation, Handle, PlanKey};

struct Counting;

thread_local! {
    /// Allocations made by THIS thread. A process-wide counter charged any other thread's allocations during the measured
    /// calls to them, most likely libtest's main thread first parking on its result channel late on a loaded gate box
    /// (the flake class of 27e1f36eee1 / d07265c7bcc). It counts only the calling thread: a measured call that handed work
    /// to another thread would not be covered, and every PlanMemo call here runs on the test's own thread.
    static ALLOCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// This thread's allocation count so far (see ALLOCS).
fn allocs_here() -> usize {
    ALLOCS.with(|c| c.get())
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1)); // try_with: a thread tearing down its TLS still allocates
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn key(handle: u64, stmt: u32) -> PlanKey {
    PlanKey { handle: Handle(handle), generation: Generation(7 | stmt << 24) }
}

#[test]
fn hits_and_refusals_allocate_nothing() {
    let mut memo = PlanMemo::new();
    let k = key(0xabc0, 0);
    let before = allocs_here();
    assert_eq!(memo.probe(k), None);
    assert_eq!(memo.insert(k, Verdict::Refused), None);
    for _ in 0..1000 {
        assert_eq!(memo.probe(k), Some(Verdict::Refused));
    }
    for i in 1..CAPACITY as u64 {
        let k = key(0xabc0 + i * 0x38, (i % 3) as u32);
        memo.probe(k);
        memo.insert(k, if i % 7 == 0 { Verdict::Served(i as u16) } else { Verdict::Refused });
        assert!(memo.probe(k).is_some());
    }
    memo.drop_for(Handle(0xabc0));
    memo.remove(key(0xabc0 + 0x38, 1));
    memo.clear();
    assert_eq!(allocs_here(), before, "the memo's steady state allocates");
}

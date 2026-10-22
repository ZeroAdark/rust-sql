//! D4633 (charge first): a `RangeCursor`'s charge counts its vectors (`heap_bytes`) on the owner `charge_to` names. Fields drop
//! in declaration order, so a charge declared after the vectors leaves a thread that drops the cursor with the vectors' free
//! first: its account dips by their bytes and only then gets them back from the owner. On a lone thread near zero that dip
//! folds below zero, and `ledger_floor` credits it to UNBOUND for good. With the charge FIRST its drop moves the bytes to the
//! dropping thread's account before the free lands: no dip, nothing floored.
use std::alloc::{GlobalAlloc, Layout, System};

use gravity_parallel::{ClaimSize, RangeCursor};

/// Door 1 as the server's allocator counts it (init_small's `oom::Rescue`): every block in and out on the calling thread.
struct Door1;

unsafe impl GlobalAlloc for Door1 {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            memledger::door_heap(l.size() as isize);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l);
        memledger::door_heap(-(l.size() as isize));
    }
}

#[global_allocator]
static A: Door1 = Door1;

/// The cursor is cut and charged by one backend's statement (thread A) and dropped by another backend's lone thread (B), as
/// a cursor whose last holder goes on another thread: B's account must never fold below zero on the drop.
#[test]
fn dropping_a_charged_cursor_on_another_lone_thread_never_floors_the_account() {
    memledger::set_floor(true);
    let floored = std::thread::spawn(|| {
        memledger::bind_backend(633, 9_633);
        memledger::statement_begin();
        let n = 1usize << 19; // 2 MiB of order + 4 MiB of mins, far past a fold step
        let order: Vec<u32> = (0..n as u32).collect();
        let mins: Vec<i64> = (0..n as i64).collect();
        let cursor = RangeCursor::ordered(&order, &mins, |_| 0, ClaimSize::Fixed(1));
        drop((order, mins));
        let (o, _) = memledger::charge::context();
        cursor.charge_to(Some(o));
        memledger::fold();
        let floored = std::thread::scope(|s| {
            s.spawn(move || {
                memledger::bind_backend(634, 9_634);
                // a little real heap of this thread's own: its account sits near zero (not below it) before the drop
                let ballast: Vec<u8> = Vec::with_capacity(1 << 20);
                std::hint::black_box(&ballast);
                memledger::fold();
                let (before, _) = memledger::floored();
                drop(cursor);
                memledger::fold();
                let (after, _) = memledger::floored();
                drop(ballast);
                memledger::fold();
                memledger::unbind_backend();
                after - before
            })
            .join()
            .unwrap()
        });
        memledger::statement_end();
        memledger::unbind_backend();
        floored
    })
    .join()
    .unwrap();
    memledger::set_floor(false);
    assert_eq!(floored, 0, "the cursor's drop on a lone thread must not dip its account below zero (the charge declared FIRST)");
}

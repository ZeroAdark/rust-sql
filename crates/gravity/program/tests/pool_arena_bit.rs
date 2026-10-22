//! PGRUST_GRAVITY_POOLED_BIT for the program arena, with `PoolAlloc` as the global allocator, which serves from the page
//! pool's range too (D4720): an arena records whether its block is the pool's from the road that served it (a pool block,
//! a pool span, a lazy span: the pool's), a refused pool take (`try_new_off_pool`) records the allocator's block, in the
//! pool's range but not the pool's, or a mapping as not, and its resets and lazy charge steps read the record (debug
//! builds check it against `pagepool::taken` at each use). Off: nothing is recorded or read.
#![cfg(target_os = "linux")]

use std::sync::{Mutex, MutexGuard, Once};

use gravity_program::arena::{set_pooled_bit, KEEP_BYTES};
use gravity_program::Arena;
use pagepool::PoolAlloc;

#[global_allocator]
static A: PoolAlloc = PoolAlloc;

const KIB: usize = 1 << 10;
const MIB: usize = 1 << 20;

/// The pool started by the global allocator, with the Gravity switch on (its default), and this test alone on it (the bit
/// and the pool's span sizes are the process's).
fn serial() -> MutexGuard<'static, ()> {
    static ONCE: Once = Once::new();
    static TURN: Mutex<()> = Mutex::new(());
    ONCE.call_once(|| {
        let probe = vec![1u8; 8 * KIB];
        assert!(pagepool::global_on() && pagepool::owns(probe.as_ptr()), "the global allocator serves from the pool's range");
        assert!(pagepool::switches().gravity, "arenas take from the pool");
    });
    TURN.lock().unwrap_or_else(|e| e.into_inner())
}

fn on(procno: u32, f: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        memledger::bind_backend(procno, 9000 + procno);
        f();
        set_pooled_bit(true);
        pagepool::flush_shelf();
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}

/// Resets below and past what was used (`n` bytes a round), each round's memory reading zero again.
fn exercise(arena: &Arena, n: usize) {
    let a = arena.handle();
    let m = a.mark();
    for _ in 0..3 {
        let p = a.alloc(n);
        let s = unsafe { core::slice::from_raw_parts_mut(p, n) };
        assert!(s.iter().all(|&b| b == 0), "the arena's unused memory reads zero");
        s.fill(0x5a);
        let inner = a.mark();
        let _ = a.alloc(64);
        a.reset_to(inner);
        a.reset_to(m);
    }
}

#[test]
fn an_arena_from_the_pool_records_it_in_each_form() {
    let _t = serial();
    on(4810, || {
        for (bytes, n) in [(64 * KIB, 32 * KIB), (4 * MIB, 3 * MIB), (KEEP_BYTES + 32 * MIB, KEEP_BYTES + 16 * MIB)] {
            let arena = Arena::new(bytes);
            let a = arena.handle();
            let p = a.alloc(8);
            assert!(pagepool::taken(p), "{bytes}: the pool's block");
            assert_eq!(a.recorded_pool(), Some(true), "{bytes}: recorded as the pool's");
            a.reset_to(a.mark());
            exercise(&arena, n);
        }
    });
}

#[test]
fn a_refused_pool_take_records_the_allocators_block_in_the_pools_range_and_a_mapping() {
    let _t = serial();
    on(4811, || {
        let arena = Arena::try_new_off_pool(64 * KIB).expect("the allocator's road");
        let a = arena.handle();
        let p = a.alloc(8);
        assert!(pagepool::owns(p) && !pagepool::taken(p), "the global allocator's block, in the pool's range");
        assert_eq!(a.recorded_pool(), Some(false), "recorded as not the pool's");
        exercise(&arena, 32 * KIB);
        drop(arena);
        let mapped = Arena::try_new_off_pool(4 * MIB).expect("a mapping");
        let a = mapped.handle();
        assert!(!pagepool::owns(a.alloc(8)), "its own mapping");
        assert_eq!(a.recorded_pool(), Some(false));
        exercise(&mapped, 3 * MIB);
    });
}

#[test]
fn with_the_bit_off_nothing_is_recorded_or_read() {
    let _t = serial();
    on(4812, || {
        let made_on = Arena::new(64 * KIB);
        set_pooled_bit(false);
        assert_eq!(made_on.handle().recorded_pool(), None, "a record is not read while off");
        let made_off = Arena::new(64 * KIB);
        let off_pool = Arena::try_new_off_pool(64 * KIB).expect("the allocator's road");
        for arena in [&made_on, &made_off, &off_pool] {
            assert_eq!(arena.handle().recorded_pool(), None);
            exercise(arena, 32 * KIB);
        }
        set_pooled_bit(true);
        assert_eq!(made_off.handle().recorded_pool(), None, "made while off: nothing recorded");
        assert_eq!(made_on.handle().recorded_pool(), Some(true));
        exercise(&made_off, 32 * KIB);
    });
}

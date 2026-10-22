//! Broker (b): the tail refresh each broker plan runs on the memory watchdog's thread (`membroker::publish_tails`, under
//! `PGRUST_MEMPOLICY=broker_value=1`) allocates nothing and never waits, with the part cache, the condition cache and the
//! build pool holding entries: the thread that frees memory under pressure must not stall on an allocation or a lock.

use std::alloc::{GlobalAlloc, Layout, System};
use std::any::Any;
use std::cell::Cell;
use std::sync::Arc;

#[path = "common/granule_fixtures.rs"]
#[allow(dead_code)]
mod granule_fixtures;

use gravity_hosts::dict::text_hosts::reuse;
use gravity_hosts::ops::scan::pgrc::cond_cache;
use memledger::{value, Consumer};
use pgrc_format::dirlayout::part_file_name;
use pgrc_read::io::{MemPartIo, PartIo};
use pgrc_read::PartExpect;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

/// Counts the allocations a thread makes while its COUNTING is set.
struct Counting;

// SAFETY: a pass-through to System plus a thread-local counter.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.with(|a| a.set(a.get() + 1));
        }
        // SAFETY: forwarded verbatim.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded verbatim.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn the_tail_refresh_allocates_nothing_with_every_cache_holding_entries() {
    value::set_ext(true);
    // the part cache: a fixture's three parts open in the process registry, every section faulted in
    let bf = granule_fixtures::sorted_fixture();
    let reg = pgrc_am::inval::registry();
    let pins: Vec<_> = bf
        .manifest
        .parts
        .iter()
        .enumerate()
        .map(|(i, rec)| {
            let bytes = bf.files.get(&part_file_name(rec.part_no)).expect("part file").clone();
            let p = reg.open_pinned(&PartExpect::none(), move || Ok(Box::new(MemPartIo::new(bytes, 31, 3100 + i as u64)) as Box<dyn PartIo>)).expect("open");
            p.part().prefault_all_sections().expect("fault");
            p
        })
        .collect();
    // the condition cache's entries and the build pool's kept buffers
    let owner: Arc<dyn Any + Send + Sync> = Arc::new(0u64);
    let entries: Vec<_> = (0..64u64).map(|i| cond_cache::entry_for([0xb1, i], [0, 0], &owner, 8, 64)).collect();
    for _ in 0..3 {
        reuse::give(vec![0u64; gravity_program::cost::DICT_POOL_MIN_WORDS * 2]);
    }

    COUNTING.with(|c| c.set(true));
    gravity_hosts::membroker::publish_tails();
    COUNTING.with(|c| c.set(false));
    assert_eq!(ALLOCS.with(Cell::get), 0, "the tail refresh allocated");
    for c in [Consumer::PartCache, Consumer::CondCache, Consumer::BuildPool] {
        assert!(value::ext_stats(c).tail_bytes > 0, "{c:?}: its tail published");
    }
    drop((entries, pins));
}

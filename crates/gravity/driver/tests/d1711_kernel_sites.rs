//! D1711 (Miri, Tree Borrows; D1705's class in these tests' own kernels): the common kernels took `site_of`'s `&mut Site`
//! before `range_seek`, whose seek rewrites the block, and wrote through it after the window (k_range_grow's `cursor`,
//! region_grow.rs under Miri; k_range_count's `state` at the end). Serial runs small enough for Miri: plain cargo passes,
//! Miri TB stopped at the parent.

use gravity_driver_rig as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use common::*;
use gravity_driver::{Accounting, Call, Entry, Grant, Step, SHARE_FLOOR};
use gravity_program::Program;

const NULLS: [u8; 1] = [0];
const PER_BLOCK: u64 = 64;

/// D1704 (its own row, in flight): a driver region is a `Vec<u8>` (alignment 1) under 8-aligned table views; Miri places a
/// block at exactly the alignment asked and stops at the first view. Every block asks 16 here, as the server's allocator
/// gives, so Miri judges D1711's site accesses past D1704 (batch 25's witnesses carried the same shim until D1704).
struct Min16;

// SAFETY: the system allocator with every layout's alignment raised to 16, the same raise on free.
unsafe impl std::alloc::GlobalAlloc for Min16 {
    unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
        std::alloc::System.alloc(Self::up(l))
    }
    unsafe fn alloc_zeroed(&self, l: std::alloc::Layout) -> *mut u8 {
        std::alloc::System.alloc_zeroed(Self::up(l))
    }
    unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
        std::alloc::System.dealloc(p, Self::up(l))
    }
    unsafe fn realloc(&self, p: *mut u8, l: std::alloc::Layout, n: usize) -> *mut u8 {
        std::alloc::System.realloc(p, Self::up(l), n)
    }
}

impl Min16 {
    fn up(l: std::alloc::Layout) -> std::alloc::Layout {
        l.align_to(16).expect("layout")
    }
}

#[global_allocator]
static A: Min16 = Min16;

/// One serial run of `program` over `blocks` x `PER_BLOCK` rows with parameter 0 `param`: the emitted rows and the Node's accounting.
fn serial(program: Program, blocks: u64, param: u64, grant: Grant) -> (Vec<Vec<u64>>, Accounting) {
    let program = Arc::new(program);
    let mut derive = FakeDerive::ranges(RangeTable::new(blocks, PER_BLOCK));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let params = [param];
    let mut b = binding(&params, &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(grant)).unwrap();
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    let acc = exec.node().accounting();
    entry.end(exec, true);
    (sink.sorted(), acc)
}

/// k_range_grow over twice its share: the leader's region grows mid-window (`region_grow`) and every row lands.
#[test]
fn the_grow_kernel_writes_its_cursor_after_the_window_and_the_grows() {
    let blocks = 2 * SHARE_FLOOR as u64 / 16 / PER_BLOCK;
    let rows = blocks * PER_BLOCK;
    let sum = rows * (rows - 1) / 2;
    let (got, acc) = serial(p_grow_share(4), blocks, 0, Grant::of(1));
    assert_eq!(got, vec![vec![rows, sum, sum, sum]]);
    assert!(acc.past_share > 0, "the region grew ({acc:?})");
}

/// k_range_count (the row protocol) and k_range_count_w (windows): the count below the threshold, the site at `EOF` after.
#[test]
fn the_count_kernels_write_the_site_after_the_seek_and_the_windows() {
    for (program, what) in [(p_count(4), "rows"), (p_count_w(4), "windows")] {
        let (got, _) = serial(program, 4, 100, Grant::of(8 << 20));
        assert_eq!(got, vec![vec![100, 99 * 100 / 2]], "{what}");
    }
}

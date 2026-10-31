//! Requested live allocator bytes for cache metadata, excluding prebuilt programs and allocator overhead.
use gravity_abi::Ctx;
use gravity_program::{Code, Layout, Program, ProgramCache, WritePlan};
use std::{alloc::{GlobalAlloc, Layout as AllocLayout, System}, sync::{Arc, atomic::{AtomicIsize, Ordering::Relaxed}}};

struct Count;
static LIVE: AtomicIsize = AtomicIsize::new(0);
unsafe impl GlobalAlloc for Count {
    unsafe fn alloc(&self, l: AllocLayout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() { LIVE.fetch_add(l.size() as isize, Relaxed); }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: AllocLayout) {
        LIVE.fetch_sub(l.size() as isize, Relaxed);
        unsafe { System.dealloc(p, l) };
    }
    unsafe fn realloc(&self, p: *mut u8, l: AllocLayout, n: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, n) };
        if !q.is_null() { LIVE.fetch_add(n as isize - l.size() as isize, Relaxed); }
        q
    }
}
#[global_allocator]
static ALLOCATOR: Count = Count;
unsafe extern "C-unwind" fn entry(_: *mut Ctx) -> i64 { 0 }

fn main() {
    println!("entries,cache_metadata_allocated_bytes");
    for n in [1, 128, 512, 2048, 8192] {
        let programs: Vec<_> = (0..n).map(|i| Arc::new(Program::new(
            Layout { nsites: 0, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 },
            &[], &[], WritePlan::SELECT, Code::hand(entry).with_identity(i as u128 + 1),
        ))).collect();
        let before = LIVE.load(Relaxed);
        let mut cache = ProgramCache::new(usize::MAX, n);
        for p in &programs { cache.insert(None, p.clone()); }
        let allocated = LIVE.load(Relaxed) - before;
        println!("{n},{allocated}");
        assert_eq!(cache.l2_len(), n);
        std::hint::black_box(cache);
    }
}

//! A per-window fmgr site (`bind_under_window`) allocates nothing in steady state: `lower(x) LIKE '%ab%'` over 1,000
//! windows of 1,024 rows, sized and undersized keepers, 0 allocations over windows 10..1,000.
mod common;

use std::alloc::{GlobalAlloc, Layout, System};

use datum::Datum;
use gravity_fn::FmgrCall;
use mcx::MemoryContext;

struct Counting;

thread_local! {
    /// Allocations made by THIS thread. A process-wide counter charged any other thread's allocations during the measured
    /// calls to them, most likely libtest's main thread first parking on its result channel late on a loaded gate box
    /// (the flake class of 27e1f36eee1 / d07265c7bcc). It counts only the calling thread: a measured call that handed work
    /// to another thread would not be covered, and every fmgr site call here runs on the test's own thread.
    static ALLOCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// This thread's allocation count so far (see ALLOCS).
fn allocs_here() -> usize {
    ALLOCS.with(|c| c.get())
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1)); // try_with: a thread tearing down its TLS still allocates
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
}

#[global_allocator]
static A: Counting = Counting;

const F_LOWER: u32 = 870;
const F_TEXTLIKE: u32 = 850;
const C_COLLATION: u32 = 950;
const ROWS: usize = 1024;
const WINDOWS: usize = 1000;

fn text_image(s: &str) -> Vec<u8> {
    let mut v = ((4 + s.len()) as u32).wrapping_shl(2).to_le_bytes().to_vec();
    v.extend_from_slice(s.as_bytes());
    v
}

/// Allocations over windows 10..WINDOWS of the filter with the lower() site sized for (rows, width).
fn filter_allocs(rows_hint: usize, width_hint: usize) -> (usize, usize) {
    common::install_seams();
    let parent = MemoryContext::new("statement");
    let mut lower = FmgrCall::bind_under_window(F_LOWER, 1, C_COLLATION, &parent, -1, rows_hint, width_hint).unwrap();
    let mut like = FmgrCall::bind_under_window(F_TEXTLIKE, 2, C_COLLATION, &parent, 1, rows_hint, 8).unwrap();
    let rows: Vec<Vec<u8>> = (0..ROWS).map(|i| text_image(&format!("Row {i:05} OF THE WINDOW, MIXED Case AB text {}", i * 7919))).collect();
    let pat = text_image("%ab%");
    let mut hits = 0usize;
    let mut before = 0;
    for w in 0..WINDOWS {
        if w == 10 {
            before = allocs_here();
        }
        for r in &rows {
            let l = lower.call_site(&[Datum::from_usize(r.as_ptr() as usize)], &[false]).unwrap();
            let m = like.call_site(&[l.value, Datum::from_usize(pat.as_ptr() as usize)], &[false, false]).unwrap();
            hits += m.value.as_bool() as usize;
        }
        lower.reset_window();
        like.reset_window();
    }
    (allocs_here() - before, hits)
}

#[test]
fn a_window_site_allocates_nothing_in_steady_state() {
    let (sized, hits) = filter_allocs(ROWS, 64);
    let (kept, hits2) = filter_allocs(1, 1);
    eprintln!("allocations over {} windows of {ROWS} rows: sized keeper {sized}, undersized keeper + kept blocks {kept}; hits {hits}", WINDOWS - 10);
    assert_eq!(hits, hits2);
    assert_eq!(hits, (WINDOWS * ROWS));
    assert_eq!(sized, 0, "a sized window site mallocs in steady state");
    assert_eq!(kept, 0, "an undersized window site mallocs in steady state despite its kept blocks");
}

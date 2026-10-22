//! A 10,000-alternative pattern's search dfa has a ~400 MB state area that C
//! MALLOCs untouched; pg_regexec must not allocate and fill it per call.

use regex::{RegcompResult, RegexecResult};
use regex_core::regex_export_free_error::{seam_pg_regcomp, seam_pg_regexec};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use types_core::C_COLLATION_OID;

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(l.size(), Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(l.size(), Relaxed);
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCATED.fetch_add(n, Relaxed);
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn wchars(s: &str) -> Vec<u32> {
    s.chars().map(|c| c as u32).collect()
}

#[test]
fn large_dfa_exec_does_not_allocate_its_state_area_per_call() {
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(|| {
            stack_depth::set_stack_base();
            let alts: Vec<String> = (1..=10_000).map(|i| format!("w{i}")).collect();
            let pat = wchars(&format!("({})", alts.join("|")));
            let re = match seam_pg_regcomp(&pat, 0o3, C_COLLATION_OID).unwrap() {
                RegcompResult::Compiled(r) => r,
                RegcompResult::Failed(f) => panic!("compile failed: {}", f.message),
            };
            for (i, (subject, hit)) in [
                ("k12-c20ad4d76fe97759aa27a0c99bff6710", false),
                ("k7-w9999x", true),
                ("k12-c20ad4d76fe97759aa27a0c99bff6710", false),
            ]
            .into_iter()
            .enumerate()
            {
                let before = ALLOCATED.load(Relaxed);
                let r = seam_pg_regexec(&re, &wchars(subject), 0, &mut []).unwrap();
                let bytes = ALLOCATED.load(Relaxed) - before;
                assert_eq!(matches!(r, RegexecResult::Matched), hit, "{subject}");
                if i > 0 {
                    assert!(bytes < (1 << 20), "call {i}: exec allocated {bytes} bytes");
                }
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

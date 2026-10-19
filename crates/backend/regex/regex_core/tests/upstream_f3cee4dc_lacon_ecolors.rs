//! upstream f3cee4dc4330 (18.4): Harden our regex engine against integer
//! overflow in size calculations. Two edge cases of that commit.
//!
//! newdfa bounds the DFA arrays to INT_MAX members (REG_ETOOBIG) before any
//! allocation: `wordsper >= INT_MAX / (nss + WORK) || ncolors >= INT_MAX /
//! nss`. pgrust's HeapSpace::for_cnfa only did usize checked_mul, which never
//! fires on 64-bit for the sizes C rejects, and went on to allocate. K
//! distinct chrs above MAX_SIMPLE_CHR concatenated twice give nstates ~ 2K,
//! nss = 2 * nstates, ncolors = K + 5 (WHITE + 4 pseudocolors), so
//! nss * ncolors crosses INT_MAX near K = 23170: K = 23200 is the witness,
//! K = 1000 the control.
//!
//! The LACON case: compact() packs lookaround arcs as color = ncolors + index
//! into the i16 color type, and when ncolors is within a few of MAX_COLOR
//! that sum no longer fits. C 18.4+ rejects the compile with REG_ECOLORS ("too
//! many colors"); pgrust guarded the same predicate but reported REG_ETOOBIG
//! ("regular expression is too complex"). ncolors = K distinct literal chrs +
//! 'A' + 'B' + WHITE + 4 pseudocolors (BOS/BOL/EOS/EOL) = K + 7. Lookaround
//! #1 needs ncolors + 1 <= MAX_COLOR (32767), so K = 32760 (ncolors 32767)
//! and K = 32761 (ncolors 32768, the colormap's own maximum) are exactly the
//! counts where compact()'s check fires; K = 32762 overflows the colormap
//! itself (newcolor's REG_ECOLORS, the control). A single-chr lookahead such
//! as (?=A) is folded by processlacon() into a plain AHEAD arc and never
//! creates a LACON, hence (?=AB). Measured against live C 18.6
//! (scripts/upstream-f3cee4dc4330-e2e.sh).

use std::alloc::{GlobalAlloc, Layout, System};

use regex::{RegMatch, RegcompResult, RegexecResult};
use regex_core::regex_export_free_error::{seam_pg_regcomp, seam_pg_regexec};
use types_core::C_COLLATION_OID;

/// C's MaxAllocSize: this binary refuses any single allocation palloc would
/// refuse, so on pre-fix code the over-INT_MAX shape aborts on its first DFA
/// array (8.6 GB of outs, then 17 GB of incarea) instead of filling the host.
const MAX_ALLOC_SIZE: usize = 0x3fff_ffff;

struct CappedAlloc;

unsafe impl GlobalAlloc for CappedAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if l.size() > MAX_ALLOC_SIZE {
            return core::ptr::null_mut();
        }
        System.alloc(l)
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        if l.size() > MAX_ALLOC_SIZE {
            return core::ptr::null_mut();
        }
        System.alloc_zeroed(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        if new_size > MAX_ALLOC_SIZE {
            return core::ptr::null_mut();
        }
        System.realloc(p, l, new_size)
    }
}

#[global_allocator]
static ALLOC: CappedAlloc = CappedAlloc;

/// K distinct chrs above MAX_SIMPLE_CHR (each its own color).
fn distinct_chrs(k: u32) -> Vec<u32> {
    (2048..2048 + k).collect()
}

/// The LACON shape: K distinct chrs then `(?=AB)`.
fn pattern(k: u32) -> Vec<u32> {
    let mut p = distinct_chrs(k);
    p.extend("(?=AB)".chars().map(|c| c as u32));
    p
}

/// The newdfa shape: K distinct chrs concatenated twice.
fn doubled_pattern(k: u32) -> Vec<u32> {
    let body = distinct_chrs(k);
    [body.clone(), body].concat()
}

fn on_big_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            stack_depth::set_stack_base();
            stack_depth::set_max_stack_depth(30 * 1024);
            stack_depth::assign_max_stack_depth(30 * 1024);
            f()
        })
        .unwrap()
        .join()
        .unwrap()
}

fn compile_msg(pat: Vec<u32>) -> Option<String> {
    on_big_stack(move || match seam_pg_regcomp(&pat, 0o3, C_COLLATION_OID).unwrap() {
        RegcompResult::Compiled(_) => None,
        RegcompResult::Failed(f) => Some(f.message),
    })
}

/// Compile, then execute against a one-chr subject: the DFA is sized (newdfa)
/// before any matching, so its bound fires regardless of the data.
fn exec_msg(pat: Vec<u32>) -> Option<String> {
    on_big_stack(move || {
        let re = match seam_pg_regcomp(&pat, 0o3, C_COLLATION_OID).unwrap() {
            RegcompResult::Compiled(re) => re,
            RegcompResult::Failed(f) => panic!("compile failed: {}", f.message),
        };
        let mut pmatch = [RegMatch::UNSET; 1];
        match seam_pg_regexec(&re, &['x' as u32], 0, &mut pmatch).unwrap() {
            RegexecResult::Failed(f) => Some(f.message),
            RegexecResult::Matched => panic!("matched"),
            RegexecResult::NoMatch => None,
        }
    })
}

/// Compile, then execute; distinguishes the engine's own failure codes from
/// the ereport C's palloc raises out of the engine.
fn exec_outcome(pat: Vec<u32>, subject: Vec<u32>) -> String {
    on_big_stack(move || {
        let re = match seam_pg_regcomp(&pat, 0o3, C_COLLATION_OID).unwrap() {
            RegcompResult::Compiled(re) => re,
            RegcompResult::Failed(f) => return format!("compile failed: {}", f.message),
        };
        let mut pmatch = [RegMatch::UNSET; 1];
        match seam_pg_regexec(&re, &subject, 0, &mut pmatch) {
            Ok(RegexecResult::Failed(f)) => format!("failed: {}", f.message),
            Ok(RegexecResult::Matched) => "matched".to_string(),
            Ok(RegexecResult::NoMatch) => "nomatch".to_string(),
            Err(e) => format!(
                "error {}: {}",
                String::from_utf8_lossy(&types_error::unpack_sqlstate(e.sqlstate())),
                e.message()
            ),
        }
    })
}

/// nss * ncolors below INT_MAX members but the outsarea request
/// (nss * ncolors * sizeof(struct sset *)) above MaxAllocSize: C's newdfa
/// bound passes and MALLOC_ARRAY raises mcxt.c's alloc-size ereport, not a
/// regex error code. K = 10000 distinct chrs: nss = 20002, ncolors = 10007,
/// 20002 * 10007 * 8 = 1601280240 (measured against live C 18.6).
#[test]
fn dfa_arrays_over_max_alloc_size_raise_c_alloc_error() {
    assert_eq!(
        exec_outcome(distinct_chrs(10_000), vec!['x' as u32]),
        "error XX000: invalid memory alloc request size 1601280240"
    );
}

/// C's newlacon has no lookaround-count cap; the MAX_COLOR fit applies only
/// to LACON arcs that survive to compact(). A lookaround inside a `{0}`
/// group has its arcs deleted first, so 32768 of them compile and the empty
/// alternative matches (C 18.6: true).
#[test]
fn deleted_lacon_arcs_are_not_color_bounded() {
    let pat: Vec<u32> = "(?:(?=ab)){0}|".repeat(32_768).chars().map(|c| c as u32).collect();
    assert_eq!(exec_outcome(pat, Vec::new()), "matched");
}

/// nss * ncolors >= INT_MAX: C 18.4+ newdfa refuses before allocating.
#[test]
fn dfa_arrays_over_int_max_are_too_complex() {
    assert_eq!(
        exec_msg(doubled_pattern(23_200)).as_deref(),
        Some("regular expression is too complex")
    );
}

/// Well below the bound the doubled pattern sizes a ~4M-cell DFA and runs.
#[test]
fn dfa_arrays_under_the_bound_execute() {
    assert_eq!(exec_msg(doubled_pattern(1_000)), None);
}

/// The two counts where the LACON color (ncolors + 1) overflows MAX_COLOR
/// while the colormap itself still fits: C 18.4+ REG_ECOLORS.
#[test]
fn lacon_color_overflow_is_too_many_colors() {
    for k in [32760u32, 32761] {
        assert_eq!(
            compile_msg(pattern(k)).as_deref(),
            Some("too many colors"),
            "K={k}: compact()'s LACON check must report REG_ECOLORS"
        );
    }
}

/// One more color: the colormap overflows first (same message, other site).
#[test]
fn colormap_overflow_is_too_many_colors() {
    assert_eq!(compile_msg(pattern(32762)).as_deref(), Some("too many colors"));
}

/// Well below the window the two-chr lookahead compiles.
#[test]
fn small_lookahead_compiles() {
    assert_eq!(compile_msg(pattern(100)), None);
}

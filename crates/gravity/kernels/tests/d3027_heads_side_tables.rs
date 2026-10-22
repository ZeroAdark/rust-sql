//! D3027: `code.heads`' count over a part's table, built on the part's second entry: a skewed part (rows repeating the row
//! before) counts flat windows into four interleaved tables folded into its table, a spread one into its table alone, a
//! selected window row by row; every later entry is a lookup and each count is the part's rows of its code. The side
//! tables are the statement's, zeroed for each part that takes them.
use gravity_abi::ctx::{ctx_size, lane, site_off, site_ptr};
use gravity_abi::program::PIN_STATEMENT;
use gravity_abi::site::site::{STATE_CLOSED, STATE_OPEN};
use gravity_abi::{Ctx, Lane, ProgramBlock, Site};
use gravity_kernels::dict::strategies::heads::{heads_count, heads_tally, NULL_CODE};

const SKEWED: [u32; 10] = [7, 7, 7, 7, 9, 7, 7, 2, 7, 7];
const SPREAD: [u32; 10] = [7, 9, 2, 0, 7, 5, 3, 7, 1, 4];
const SEL: [u32; 3] = [0, 2, 4];
const WINDOWS: u32 = 2;

/// The source behind `Site.src`: the lane every window serves, whether windows carry `SEL`, the lane's code count, windows
/// left of the current seek and served in all.
struct Src {
    codes: &'static [u32; 10],
    sel: bool,
    ncodes: u32,
    left: u32,
    served: u32,
}

/// `WINDOWS` windows of the lane in part 3 (4-byte codes), re-armed at each seek.
unsafe extern "C" fn next(s: *mut Site) -> i64 {
    let st = &mut *s;
    let src = &mut *(st.src as *mut Src);
    if st.state == STATE_CLOSED {
        src.left = WINDOWS;
    }
    st.state = STATE_OPEN;
    (st.cursor, st.sel, st.sel_n, st.nlanes) = (0, core::ptr::null(), 0, 0);
    if src.left == 0 {
        st.rows = 0;
        return 0;
    }
    src.left -= 1;
    src.served += 1;
    let mut l = Lane::byval(src.codes.as_ptr() as *const u8, core::ptr::null(), 4, lane::REGION_ARENA, 0);
    l.ncodes_or_scale = src.ncodes;
    *st.lanes = l;
    (st.rows, st.nlanes, st.part_id) = (src.codes.len() as u32, 1, 3);
    if src.sel {
        (st.sel, st.sel_n) = (SEL.as_ptr(), SEL.len() as u32);
    }
    src.codes.len() as i64
}

/// The test's `Program.pin`: a zeroed region of its own (the statement bit dropped), leaked: kept to the process's end.
unsafe extern "C" fn test_pin(_ctx: *mut Ctx, bytes: u64) -> *mut u8 {
    vec![0u64; ((bytes & !PIN_STATEMENT) as usize).div_ceil(8)].leak().as_mut_ptr() as *mut u8
}

/// Each `(first granule, code, rows)` counted in turn through one memo word; the windows served after each.
fn counts(codes: &'static [u32; 10], sel: bool, ncodes: u32, asks: &[(u64, u32, i64)]) -> Vec<u32> {
    counts_after_pass(codes, sel, ncodes, &mut [], asks)
}

/// `counts` behind D3027 (B)'s count pass: a non-empty `dir` (`[n, (first granule, slots, table) x n]`) first takes every
/// window of the part (part 3 of the list) through `heads_tally`, and each entry's `heads_count` reads it; the windows
/// served after each count, the pass's first.
fn counts_after_pass(codes: &'static [u32; 10], sel: bool, ncodes: u32, dir: &mut [u64], asks: &[(u64, u32, i64)]) -> Vec<u32> {
    let mut buf = vec![0u64; ctx_size(1) / 8];
    let mut program = Box::new(ProgramBlock::NONE);
    program.pin = Some(test_pin);
    let mut src = Src { codes, sel, ncodes, left: 0, served: 0 };
    let (mut lanes, mut keys, mut keys_null) = ([Lane::NONE; 1], [0u64; 2], [0u8; 2]);
    let mut memo = 0u64;
    let mut served = Vec::new();
    // SAFETY: the ctx is bound over locals that outlive it, as the driver binds one site inline behind the header.
    unsafe {
        let ctx = buf.as_mut_ptr() as *mut Ctx;
        (*ctx).program = &mut *program;
        (*ctx).sites = (ctx as *mut u8).add(site_off(0)) as *mut Site;
        (*ctx).nsites = 1;
        let s = site_ptr(ctx, 0);
        (*s).next = Some(next);
        (*s).src = &mut src as *mut Src as *mut u8;
        (*s).lanes = lanes.as_mut_ptr();
        (*s).lanes_cap = 1;
        (*s).keys = keys.as_mut_ptr();
        (*s).keys_null = keys_null.as_mut_ptr();
        (*s).nkeys = 2;
        let d = if dir.is_empty() { 0 } else { dir.as_mut_ptr() as u64 };
        if d != 0 {
            (*s).state = STATE_CLOSED;
            while next(s) > 0 {
                assert_eq!(heads_tally(ctx as u64, 0, d), 0, "a window tallied");
            }
            (*s).state = STATE_CLOSED;
            served.push((*(*s).src.cast::<Src>()).served);
        }
        for &(start, code, rows) in asks {
            let c = heads_count(ctx as u64, 0, 1 << 63 | start << 32 | code as u64, &mut memo as *mut u64 as u64, d);
            assert_eq!(c, rows, "part {start} code {code} (sel {sel}, ncodes {ncodes})");
            served.push((*(*s).src.cast::<Src>()).served);
        }
    }
    served
}

/// Two parts in turn (first granules 5 and 6, the same rows): the second reuses the first's side tables, zeroed again.
#[test]
fn a_skewed_part_counts_into_side_tables() {
    let asks = [(5, 7, 16), (5, 9, 2), (5, 2, 2), (5, 7, 16), (5, 5, 0), (6, 9, 2), (6, 7, 16), (6, 2, 2), (5, 9, 2)];
    assert_eq!(counts(&SKEWED, false, 10, &asks), [2, 4, 4, 4, 4, 6, 8, 8, 8]);
}

#[test]
fn a_spread_part_counts_into_its_table() {
    let asks = [(5, 7, 6), (5, 9, 2), (5, 2, 2), (5, 0, 2), (5, 5, 2), (5, 3, 2), (5, 1, 2), (5, 4, 2), (5, 6, 0), (5, 8, 0)];
    assert_eq!(counts(&SPREAD, false, 10, &asks), [2, 4, 4, 4, 4, 4, 4, 4, 4, 4]);
}

#[test]
fn a_selected_window_counts_row_by_row() {
    let asks = [(5, 7, 4), (5, 9, 2), (5, 2, 0), (5, 7, 4)];
    assert_eq!(counts(&SKEWED, true, 10, &asks), [2, 4, 4, 4]);
}

/// A code past the lane's count gives the part no table: the second entry's pass stops at the first window and counts
/// its code by a pass of its own; every later entry is a single-code pass.
#[test]
fn a_code_past_the_lane_count_keeps_the_part_on_single_passes() {
    let asks = [(5, 7, 16), (5, 9, 2), (5, 7, 16)];
    assert_eq!(counts(&SKEWED, false, 6, &asks), [2, 5, 7]);
}

/// D3027 (B): the count pass tallies part 3's two windows (first granule 5) into its directory entry; each of the part's
/// entries then reads that table and serves no window (NULL_CODE: the part's NULL rows, none). A part the pass left
/// (`slots` 0, first granule 2) or gave no table (`usize::MAX`, first granule 4) counts by its own pass.
#[test]
fn a_part_the_count_pass_tallied_reads_its_directory() {
    for (codes, sel, own) in [(&SKEWED, false, 16), (&SPREAD, false, 6), (&SKEWED, true, 4)] {
        let mut dir = [4, 1, 0, 0, 2, 0, 0, 4, u64::MAX, 0, 5, 0, 0];
        let asks = [(5, 7, own), (5, 9, 2), (5, 2, if sel { 0 } else { 2 }), (5, NULL_CODE, 0), (5, 8, 0), (2, 7, own), (4, 7, own)];
        assert_eq!(counts_after_pass(codes, sel, 10, &mut dir, &asks), [2, 2, 2, 2, 2, 2, 4, 6], "sel {sel}");
        assert_eq!(dir[11], 11, "part 3's table: ten codes and the NULL slot");
    }
}

/// A code past the lane's count gives the part no table in the pass's directory: its entries count by their own passes.
#[test]
fn a_code_past_the_lane_count_leaves_the_pass_no_table() {
    let mut dir = [4, 1, 0, 0, 2, 0, 0, 4, 0, 0, 5, 0, 0];
    let asks = [(5, 7, 16), (5, 9, 2)];
    assert_eq!(counts_after_pass(&SKEWED, false, 6, &mut dir, &asks), [2, 4, 7]);
    assert_eq!(dir[11], u64::MAX, "NO_TABLE");
}

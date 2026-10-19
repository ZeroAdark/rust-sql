//! regc_color.c newhicolorcols()/newhicolorrow() REALLOC_ARRAY the high
//! colormap at its power-of-two row capacity (maxarrayrows); the request is
//! what palloc admits, so the MaxAllocSize refusal is the mcxt.c ereport with
//! C's byte count, not a regex error code. 524288 rows x 1024 columns x
//! sizeof(color) = 1073741824 (measured against live C 18.6: a bracket of the
//! ten locale classes plus 262145 distinct chrs above U+FFFF).

use ::mcx::MemoryContext;
use regex_core::regex_consts::{NUM_CCLASSES, REG_CANCEL};
use regex_core::regex_error::take_cancel_error;
use regex_core::regex_foundation::{newhicolorcols, newhicolorrow};
use regex_core::regguts::{ColorMap, WHITE};

fn colormap(rows: i32, cols: i32, maxarrayrows: i32) -> ColorMap {
    ColorMap {
        cd: Vec::new(),
        max: 0,
        free: 0,
        locolormap: Vec::new(),
        classbits: [0; NUM_CCLASSES as usize],
        cmranges: Vec::new(),
        hicolormap: vec![WHITE; (rows * cols) as usize],
        hiarrayrows: rows,
        hiarraycols: cols,
        maxarrayrows,
    }
}

#[test]
fn column_doubling_over_max_alloc_size_raises_c_alloc_error() {
    let ctx = MemoryContext::new("t");
    let mut cm = colormap(1, 512, 524_288);
    let e = newhicolorcols(ctx.mcx(), &mut cm).unwrap_err();
    assert_eq!(e.code(), REG_CANCEL);
    assert_eq!(take_cancel_error().message(), "invalid memory alloc request size 1073741824");
}

#[test]
fn row_doubling_over_max_alloc_size_raises_c_alloc_error() {
    let ctx = MemoryContext::new("t");
    let mut cm = colormap(4, 1024, 4);
    cm.maxarrayrows = 262_144;
    cm.hiarrayrows = 262_144;
    cm.hicolormap = vec![WHITE; 1024];
    let e = newhicolorrow(ctx.mcx(), &mut cm, 0).unwrap_err();
    assert_eq!(e.code(), REG_CANCEL);
    assert_eq!(take_cancel_error().message(), "invalid memory alloc request size 1073741824");
}

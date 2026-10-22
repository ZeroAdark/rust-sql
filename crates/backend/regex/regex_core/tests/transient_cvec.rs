//! regc_cvec.c getcvec recycles the per-compile transient cvec only while a
//! request fits; otherwise it is replaced by one of exactly the requested
//! size, so a later range() can see fewer than 100 chr slots.

use regex_core::regex_foundation::{reset_transient_cvec, transient_getcvec};

#[test]
fn transient_cvec_capacity_follows_c_getcvec() {
    let cx = mcx::MemoryContext::new("t");
    reset_transient_cvec();
    assert_eq!(transient_getcvec(cx.mcx(), 1, 1).unwrap().1, 100);
    // [a-ž] under REG_ICASE: 286 chrs replace the (100, 20) cvec.
    assert_eq!(transient_getcvec(cx.mcx(), 286, 1).unwrap().1, 286);
    // [[:cntrl:]] needs 2 ranges: replaced by exactly (0, 2).
    assert_eq!(transient_getcvec(cx.mcx(), 0, 2).unwrap().1, 0);
    // [ǅ-ǅ] then gets a 1-slot cvec, too small for its two case variants.
    assert_eq!(transient_getcvec(cx.mcx(), 1, 1).unwrap().1, 1);
    reset_transient_cvec();
    assert_eq!(transient_getcvec(cx.mcx(), 1, 1).unwrap().1, 100);
}

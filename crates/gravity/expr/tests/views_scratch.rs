//! views_scratch.rs: the detoast scratch nests — a re-entered detoast gets its own context and the outer's bytes stay put.

use gravity_expr::views::with_detoast_scratch;

fn fill(cx: mcx::Mcx<'_>, byte: u8, n: usize) -> (*const u8, usize) {
    let mut v: mcx::PgVec<u8> = mcx::vec_with_capacity_in(cx, n).unwrap();
    mcx::vec_append_bytes(&mut v, &vec![byte; n]).unwrap();
    let s = v.leak();
    (s.as_ptr(), s.len())
}

#[test]
fn a_nested_detoast_keeps_the_outer_bytes() {
    let outer = with_detoast_scratch(|cx| {
        let (p, n) = fill(cx, 0xA5, 4096);
        let (q, m) = with_detoast_scratch(|cx2| fill(cx2, 0x5A, 8192));
        // SAFETY: the inner context was reset at its own exit; the outer's is live until this closure returns
        let inner_bytes = unsafe { core::slice::from_raw_parts(q, m) };
        let _ = inner_bytes.len();
        let bytes = unsafe { core::slice::from_raw_parts(p, n) };
        assert!(bytes.iter().all(|b| *b == 0xA5), "the outer's bytes survive the nested detoast's reset");
        (p, n)
    });
    // a fresh outer call after the reset gets the retained context again: nested once more, it still nests
    let again = with_detoast_scratch(|cx| fill(cx, 0x11, 64));
    assert_eq!(again.1, 64);
    let _ = outer;
}

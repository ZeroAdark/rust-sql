//! D4322: an image the allocator refuses is a typed out-of-memory read error (53200 at the client through
//! `pgrc_am::read_error`), never a panic: a panic here aborts the server and every session with it.

use crate::testpart::{build_part, seq_i64_col, PartSpec};
use crate::ReadError;

/// A listed section's image copied out of a run (`SegBuf::from_bytes`) with its allocation refused.
#[test]
fn a_refused_section_image_is_an_out_of_memory_error_not_a_panic() {
    let built = build_part(&PartSpec::new(6_000, vec![seq_i64_col(1, 6_000), seq_i64_col(2, 6_000)]));
    let part = super::open_built(&built, 1, 1);
    let sections = part.sections();
    let (idx, len) = sections.iter().enumerate().map(|(i, e)| (i, e.len)).max_by_key(|&(_, l)| l).expect("sections");
    // the image's 8-aligned buffer, and nothing smaller (the run itself is a byte buffer)
    let from = (len as usize).div_ceil(8) * 8;
    let before = part.resident();
    let got = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        super::refusing(from, || part.section_bytes_batch(&[idx], 0, &|_| true).map(|v| v.len()))
    }));
    match got {
        Err(_) => panic!("section {idx} ({len} bytes): a refused image allocation panicked (the server aborts)"),
        Ok(Ok(n)) => panic!("section {idx} ({len} bytes): served {n} images with the allocation refused"),
        Ok(Err(e)) => assert!(matches!(e, ReadError::Io { errno: libc::ENOMEM, .. }), "out of memory, got {e:?}"),
    }
    assert_eq!(part.resident(), before, "nothing became resident");
    // the allocator gives again: the same read is served
    let v = part.section_bytes_batch(&[idx], 0, &|_| true).expect("served");
    assert_eq!(v[0].len() as u64, len);
}

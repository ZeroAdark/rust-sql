//! D4409 (b): a band kept wrapped comes back, in a stream that does not wrap, as exactly the section its writer closed
//! out; the rebuild is checked against the writer's CRC, and a mismatch is a typed refusal.

use crate::seal::unwrap_kept;
use crate::WriteError;
use pgrc_format::enc::{EncodingId, Wrapper};
use pgrc_format::part::StreamSectionWriter;

/// An unwrapped one-granule section of `n` packing bytes, its writer's CRC and its granule payload ends.
fn section(n: usize) -> (Vec<u8>, u32, Vec<u32>) {
    let mut sec: Vec<u8> = Vec::new();
    let mut w = StreamSectionWriter::begin(&mut sec, EncodingId::Verbatim.as_u16(), 0, Wrapper::None).expect("begin");
    w.payload().extend((0..n).map(|i| b'p' + (i % 7) as u8));
    let end = w.payload_off();
    w.end_granule(1);
    let closeout = w.finish(false).expect("finish");
    (sec, closeout.crc, vec![end])
}

#[test]
fn a_kept_image_unwraps_to_its_writers_section() {
    let (sec, crc, ends) = section(64 << 10);
    let mut img: Vec<u8> = Vec::new();
    pgrc_codec::wrapper::wrap_section(&sec, &ends, Wrapper::Zstd, &mut img).expect("wrap");
    assert!(img.len() < sec.len(), "the image packs");
    assert_eq!(unwrap_kept(&img, crc).expect("unwrap"), sec);
}

#[test]
fn an_unwrap_that_misses_the_writers_crc_is_refused() {
    let (sec, crc, ends) = section(64 << 10);
    let mut img: Vec<u8> = Vec::new();
    pgrc_codec::wrapper::wrap_section(&sec, &ends, Wrapper::Zstd, &mut img).expect("wrap");
    match unwrap_kept(&img, crc ^ 1) {
        Err(WriteError::Refused { what }) => assert!(what.contains("did not unwrap"), "{what}"),
        other => panic!("a CRC mismatch must refuse, got {other:?}"),
    }
}

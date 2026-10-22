//! `OpenPart::section_bytes_batch`: the coalesced multi-section read — bytes
//! equal to the single-section face, fewer positioned reads than sections,
//! kept sections resident and unkept ones caller-scoped.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::io::PartIo;
use crate::openpart::{OpenPart, PartExpect};
use crate::testpart::{build_part, seq_i64_col, PartSpec};
use crate::ReadResult;

struct CountingIo {
    inner: crate::io::MemPartIo,
    reads: Arc<AtomicU64>,
}

impl PartIo for CountingIo {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn dev_ino(&self) -> (u64, u64) {
        self.inner.dev_ino()
    }
    fn pread_exact(&self, off: u64, buf: &mut [u8], at: &'static str) -> ReadResult<()> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.pread_exact(off, buf, at)
    }
}

#[test]
fn batch_reads_equal_bytes_with_fewer_reads_and_keeps_only_what_is_asked() {
    let built = build_part(&PartSpec::new(
        6_000,
        vec![seq_i64_col(1, 6_000), seq_i64_col(2, 6_000), seq_i64_col(3, 6_000)],
    ));
    let reads = Arc::new(AtomicU64::new(0));
    let io = CountingIo { inner: built.mem_io(1, 1), reads: Arc::clone(&reads) };
    let part = OpenPart::open(Box::new(io), &PartExpect::none()).expect("open");
    let reference = OpenPart::open(Box::new(built.mem_io(1, 1)), &PartExpect::none()).expect("open");
    let n = part.sections().len();
    assert!(n >= 4, "the fixture lists {n} sections");
    let idxs: Vec<usize> = (0..n).collect();
    let keep = |idx: usize| idx % 2 == 0;
    let before = reads.load(Ordering::Relaxed);
    let got = part.section_bytes_batch(&idxs, 1 << 20, &keep).expect("batch");
    let issued = reads.load(Ordering::Relaxed) - before;
    assert!(issued < n as u64, "{issued} reads for {n} sections");
    assert_eq!(issued, 1, "a 1 MiB gap covers this small part in one read");
    let mut kept_bytes = 0u64;
    for (idx, buf) in idxs.iter().zip(&got) {
        assert_eq!(buf.bytes(), reference.section_bytes(*idx).expect("single").bytes(), "section {idx}");
        if keep(*idx) {
            kept_bytes += buf.len() as u64;
        }
    }
    assert_eq!(part.resident(), kept_bytes, "only the kept sections became resident");
    // A second batch: the kept ones are cache hits, the others are read again (never cached).
    let before = reads.load(Ordering::Relaxed);
    let again = part.section_bytes_batch(&idxs, 0, &keep).expect("batch");
    let unkept = idxs.iter().filter(|&&i| !keep(i)).count() as u64;
    assert!(reads.load(Ordering::Relaxed) - before <= unkept, "no read for a resident section");
    assert_eq!(again.len(), got.len());
    // A zero gap still coalesces adjacent sections into runs and never over-reads past the file.
    let all_kept = |_: usize| true;
    let once_more = part.section_bytes_batch(&idxs, 0, &all_kept).expect("batch");
    assert_eq!(once_more.len(), n);
    assert!(part.resident() >= kept_bytes);
    // Out-of-range index refuses typed.
    assert!(part.section_bytes_batch(&[n], 0, &all_kept).is_err());
}

/// D3973: a whole-extent read goes a piece a read with the statement's cancel polled before each: canceled, it stops
/// before its first read; outside a kernel call it reads the same bytes in pieces.
#[test]
fn a_canceled_whole_extent_read_stops_before_its_next_piece() {
    use crate::openpart::pread_pieces;
    const PIECE: usize = 4096;
    let bytes: Vec<u8> = (0..3 * PIECE + 17).map(|i| (i * 31 % 251) as u8).collect();
    let reads = Arc::new(AtomicU64::new(0));
    let io = CountingIo { inner: crate::io::MemPartIo::new(bytes.clone(), 1, 1), reads: Arc::clone(&reads) };
    let mut buf = vec![0u8; bytes.len() - 5];
    let byte = core::sync::atomic::AtomicU8::new(cancelpoll::ABORT_BIT);
    let got = cancelpoll::scoped(&byte, || pread_pieces(&mut cancelpoll::step("test extent"), &io, 5, &mut buf, "test extent", PIECE));
    assert!(matches!(got, Err(crate::ReadError::Canceled)), "a canceled read stops");
    assert_eq!(reads.load(Ordering::Relaxed), 0, "before its first piece");
    pread_pieces(&mut cancelpoll::step("test extent"), &io, 5, &mut buf, "test extent", PIECE).expect("no cancel outside a kernel call");
    assert!(buf[..] == bytes[5..], "the same bytes");
    assert_eq!(reads.load(Ordering::Relaxed), 4, "one read a piece");
}

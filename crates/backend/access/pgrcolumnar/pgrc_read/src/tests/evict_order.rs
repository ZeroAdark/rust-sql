//! D4595: a part-cache eviction gives the evicted images' bytes to the evicting thread before their frees land on it
//! (`OpenPart::evict_extent_images`, as `evict_passed_tier` and `evict` do). The cache's charge holds a cached image's
//! bytes, not the image, so a free that came first took the evicting row below zero at its fold (each image here is past
//! `DOOR_FOLD_BYTES`), and with the ledger's floor on that dip became bytes left on the evicting row.

use std::sync::Arc;

use pgrc_format::part::StreamRole;

use super::door1;
use crate::cursor::{reference_binding_leaked, StreamCursor};
use crate::openpart::{OpenPart, PartExpect};
use crate::testpart::{build_part, seq_i64_col, PartSpec};

/// What the runtime around a spawn moves on a thread's account: far below the images evicted here.
const SLACK: i64 = 64 << 10;

/// One backend faults a part's four extent images into the part cache (a prefetch), another, whose row holds none of
/// them, evicts them, both on door 1: the evicting row never reads below where it started after a free, and nets zero.
#[test]
fn an_eviction_gives_the_images_bytes_to_the_evicting_thread_before_their_frees() {
    // 32 granules of 8,192 rows of 8-byte words (verbatim), cut into four extents of 8 granules (512 KiB) each
    let rows = 32 * 8_192u64;
    let mut col = seq_i64_col(1, rows);
    col.extent_cuts = vec![8, 16, 24];
    let b = build_part(&PartSpec::new(rows, vec![col]));
    let part = Arc::new(OpenPart::open(Box::new(b.mem_io(1, 4595)), &PartExpect::none()).expect("open"));
    let dir = part.stream_directory().expect("dir");
    let ps = dir.lookup(1, 0, StreamRole::Values).expect("values stream");
    let keys: Vec<(u64, u64)> = ps.extents.iter().map(|r| (r.file_off, r.len)).collect();
    assert_eq!(keys.len(), 4);
    assert!(keys.iter().all(|k| k.1 > memledger::DOOR_FOLD_BYTES as u64), "each image's free folds at once: {keys:?}");

    let p = Arc::clone(&part);
    std::thread::spawn(move || {
        memledger::bind_backend(2595, 9000 + 2595);
        let (r, _) = door1(|| {
            let cur = StreamCursor::open(Arc::clone(&p), reference_binding_leaked(), 1, 0).expect("cursor");
            cur.prefetch_granules(0, cur.granule_count(), 8 << 20, 256 << 10)
        });
        assert_eq!(r[0], 4, "the four extents cached");
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
    assert!(part.resident() > 4 * memledger::DOOR_FOLD_BYTES as u64);

    let p = Arc::clone(&part);
    std::thread::spawn(move || {
        memledger::bind_backend(2596, 9000 + 2596);
        let a = memledger::Acct::backend(2596);
        assert!(a.is_backend());
        memledger::fold();
        let (before, resident) = (memledger::account_row(a).account_bytes, p.resident());
        let ((n, bytes), low) = door1(|| p.evict_extent_images(&keys));
        memledger::fold();
        let after = memledger::account_row(a).account_bytes;
        assert_eq!((n, resident - p.resident()), (4, bytes), "the four images evicted");
        assert!(low >= before - SLACK, "after a free the evicting row read {} B below its start ({bytes} B evicted)", before - low);
        assert!((after - before).abs() < SLACK, "the evicting row nets zero: {} B", after - before);
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}

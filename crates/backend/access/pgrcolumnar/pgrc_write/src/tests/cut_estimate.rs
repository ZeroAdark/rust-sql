//! K1 L7: compaction's estimate of the writer's part-cut measure (`facts::cut_bytes` over `facts::part_heap_stats`)
//! reads a part the byte budget closed at the budget (it closes at the first cut granule past it), and the tail
//! part under it.

use super::*;
use pgrc_read::{MemPartIo, OpenPart, PartExpect};

#[test]
fn a_part_the_byte_budget_closed_estimates_at_the_budget() {
    let policy = PartCutPolicy { max_rows: u64::MAX, max_bytes: 200_000, cut_granule_rows: 64 };
    let parts = serial_mixed_tmp_bytes(30_000, 77, policy);
    assert!(parts.len() >= 4, "{} parts", parts.len());
    for (seq, bytes) in parts.iter().enumerate() {
        let part = OpenPart::open(Box::new(MemPartIo::new(bytes.clone(), 7, seq as u64 + 1)), &PartExpect::none()).expect("open");
        let cols = crate::facts::part_heap_stats(&part).expect("stats");
        assert_eq!(cols.iter().map(|c| (c.attno, c.heap(), c.stats.is_some())).collect::<Vec<_>>(), [(1, false, false), (2, true, true)]);
        let est = crate::facts::cut_bytes(part.rows(), &cols);
        // mixed_row's text is "value-<i % 977>" (7-9 B, a 16-B heap entry, where the estimate takes 7.5 B of header
        // and padding over the payload): within 3 % of the true measure, plus one granule's overshoot (64 rows of
        // ~31 B).
        if seq + 1 < parts.len() {
            assert!(est * 100 >= policy.max_bytes * 97 && est <= policy.max_bytes * 103 / 100 + 64 * 32, "part {seq}: {est} B");
        } else {
            assert!(est < policy.max_bytes, "the tail part: {est} B");
        }
    }
}

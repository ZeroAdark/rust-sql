//! Verdict-aware coalesced claim reads (H03b, cross-crate: hosts `granule_source::read_claim_run`): a pgrc claim reads
//! only the runs of granules its verdicts admit (zone, bloom, an empty PSMA window, the condition cache's `NoMatch`),
//! coalesced up to `pgrust.gravity_read_run_kb` across holes of at most `pgrust.gravity_read_hole_kb`, instead of every granule of the
//! claim's range. The pgrc source asks for it in its read plan and in the `src.verdicts` enables (`VERDICT_CLAIM`)
//! whenever this road is on. Registers road `read.claim`. Owner M1-SOURCE.
use crate::ops::Table;

crate::strategies! {
    /// Verdict-aware coalesced claim reads (hosts: granule_source).
    READ_CLAIM = 17, "read.claim", Pgrc;
}

pub(crate) const ROWS: Table = Table { strategies: &[(READ_CLAIM, &())], ..crate::ops::NONE };

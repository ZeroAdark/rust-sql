//! Road `heap.stream` (H03a, R1-L06 D143; cards/M2.md L4, `Xc`): the heap source's read-stream look-ahead. The advice is
//! the host's: hosts v4/m2_heap.rs `ReadAhead` (the kis kernel's `read_ahead`, fb2930c8604) over STORAGE's seam
//! `prefetch_range_without_relcache` (e69df098850), which applies PostgreSQL's rule itself (Decision 15): after a combined
//! read that missed shared buffers, advise the window past it, one window per relation per statement shared by the
//! workers, refilled a quarter at a time, in calls of the device's I/O size; a shared-buffer hit advises nothing. The
//! lowerer's part is the switch: source/heap.rs declares the site's `src.verdicts` with `VERDICT_STREAM` when the road is
//! on (V4R-32; exec sets the recipe's flag at prepare), so `gravity_road_off=heap.stream` is the forced-off twin. Owner
//! M2-HEAP.
use crate::ops::Table;

crate::strategies! {
    /// Read-stream look-ahead (hosts / bufmgr).
    HEAP_STREAM = 57, "heap.stream", Heap;
}

pub(crate) const ROWS: Table = Table { strategies: &[(HEAP_STREAM, &())], ..crate::ops::NONE };

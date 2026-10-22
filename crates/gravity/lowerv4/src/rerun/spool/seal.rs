//! D4628 (road `spool.seal`, default off): an owned spool (D3040, `Rec::owned`: a CTE store, a spooled drain's probe, a
//! grouping-set spool filled at width) sealed off the leader's copy. Off, the leader alone copies every owner's records
//! into one merged region after the fill, no worker running (exec ops/material/raw.rs `seal`): a store of tens of millions
//! of records waits hundreds of ms for it at every width. Under the road the proto carries `SPOOL_SEAL_WIDE` and exec seals it
//! with no leader copy: one owner holding every record (width 1, or one worker's fill) lends its region to the merged cell;
//! the records of several owners are copied into the merged region by the pool's threads, a disjoint byte range each. The
//! records, their order (owner ordinal, then append order), the count and the readers are unchanged.
//!
//! Off (`SET pgrust.gravity_road_off` without `+spool.seal`): the proto is today's and the leader copies, as before.
use gravity_kernels::ops::shared::object::entries as m8;

crate::strategies! {
    /// An owned spool sealed off the leader's copy: one owner's region lent, several owners' records copied by the pool.
    SPOOL_SEAL = 155, "spool.seal", Cte;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(SPOOL_SEAL, &())], ..crate::ops::NONE };

/// The bits road `spool.seal` adds to an owned spool's proto `arg`: `SPOOL_SEAL_WIDE` while the road is on, else none.
pub(crate) fn bits() -> u64 {
    if !crate::face::end::Parallel::SpoolSeal.on() {
        return 0;
    }
    fires::fire("lower.spool.seal"); // shapecheck fire signal (D4628): an owned spool sealed off the leader's copy
    m8::SPOOL_SEAL_WIDE
}

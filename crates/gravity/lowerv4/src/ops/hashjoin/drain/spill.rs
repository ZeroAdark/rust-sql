//! The drain parts of a spilled RIGHT / FULL / RIGHT SEMI / RIGHT ANTI build (M7-SPILL B4c): each partition's U(p),
//! written at its pair claim's close, and the drain's part numbering; the loader's host functions sit in drain/raw.rs.
// the drain parts' host functions live in drain/raw.rs (R6); their callers keep these paths (ops/hashjoin/spill/load.rs and
// raw.rs; `drain_records` only for load.rs's unit tests)
pub(crate) use crate::ops::hashjoin::drain::raw::{drain_bytes, drain_each, write_drain};
#[cfg(test)]
pub(crate) use crate::ops::hashjoin::drain::raw::{drain_records, drain_set};

/// A drain part's header (M7-SPILL B4c): `[MAGIC_U | u32 records]`, written first by the one owner whose claim opened the
/// partition, even with no record to send.
pub(crate) const MAGIC_U: [u8; 4] = *b"M7U1";

/// The header of a drain part of `n` records.
pub(crate) const fn drain_head(n: u32) -> [u8; 8] {
    let c = n.to_le_bytes();
    [MAGIC_U[0], MAGIC_U[1], MAGIC_U[2], MAGIC_U[3], c[0], c[1], c[2], c[3]]
}

/// The stream of the build slot the running pipeline consumes as its region `consumed`: the build's parts, its kept rows
/// and its drain parts (B4c-2: one definition; `Kept::stream` and the drain source read it). D3590: named by the consumed
/// index (D1324 `STREAM_CONSUMED`), which the driver resolves through the running pipeline's `consumes`, never by the
/// slot's number baked into the code: lowerv4 renumbers a deferred pipeline's slot after the code is built (frame.rs
/// `renumber_deferred`: an InitPlan's members), and a baked `slot + 1` then named another life's stream (the fold the
/// spill pair pass feeds, which then took the kept rows for its own spill: kind 2; or a slot outside its life).
pub(crate) const fn stream_of(consumed: usize) -> u64 {
    gravity_abi::program::STREAM_CONSUMED | consumed as u64
}

/// Partition `p`'s drain part: the records the drain of a RIGHT / FULL / RIGHT SEMI / RIGHT ANTI build sends for it.
/// Build parts are `0..2^bits`, kept probe rows `2^bits + p`.
pub(crate) const fn u_part(bits: u8, p: u32) -> u32 {
    (2u32 << bits) + p
}

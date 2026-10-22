//! The spill file format's limits (driver mem/spill.rs writes and reads the files): the parts a file holds, the runs a
//! run token can index, the partitions a stored split scatters into, and the merge passes a merged run's file lineage
//! has bits for. Each is a bound of the format, not a tuned size.

/// Runs per file: a run is one partition of the owner's run file. D1493: an owner's runs past it go on in a chained file
/// (`Chain`), as a merge pass's runs do: a file keeps at most this many parts, the owner's runs have no bound of their own.
pub const MAX_RUNS: u32 = 4096;
/// D1493: the runs one owner puts, or one merge pass writes (a merged run token's index takes 20 bits); past it a spill is
/// `UNSUPPORTED_SPILL` (at 64 kB a run, 64 GB an owner).
pub const MAX_CHAIN_RUNS: u32 = 1 << 20;
/// The partitions a stored split scatters into at most: generation 0's widest fan-out, whose bits the re-split depth starts
/// from (DR-17); a split's partition table and its epoch list are arrays of this many entries.
pub const MAX_PARTS: usize = 2048;
/// D1327: a merge's passes (a fan-in of 2 halves the runs each pass: 4096 runs need 12). Below 64, so a pass's chained files
/// fit the merged runs' file lineage below the pass bits.
pub const MAX_MERGE_PASS: usize = 63;

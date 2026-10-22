//! The spill face (plan §3.1; H24, MEMORY S5). A breaker past its grant writes partitioned streams, each named by
//! (slot, life): hash-agg groups, sort runs, a build with its kept outer rows, partition pairs. It reads them back one
//! partition at a time. A partition that still does not fit is re-split by the next route bits of the one KeyHash
//! (`KeyHash::split` at the partition's `Depth`), so no partition hashes its keys twice. A build is sized against the whole
//! grant. `spill.breakers` (M7) builds every form; the result is PG's for any `work_mem` (a spill changes cost, never rows).
use gravity_abi::hash::Depth;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;
use gravity_program::LifeId;

use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::frame::stop::Exits;

crate::hook_variants! {
    /// What a breaker spills (H24).
    "H24" pub enum SpillForm {
        /// A hash aggregate's groups past the grant: their partial states, and the rows whose groups did not fit.
        HashAgg = Kind(SpillBreakers),
        /// A sort's runs.
        SortRuns = Kind(SpillBreakers),
        /// A hash build's partitions.
        Build = Kind(SpillBreakers),
        /// The outer rows kept for a spilled build's partitions.
        KeptOuter = Kind(SpillBreakers),
        /// Partition pairs (build and probe) joined pair by pair.
        Pairs = Kind(SpillBreakers),
    }
}

/// A spill stream's name: the breaker's slot, the life that owns the stream, and its partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamName {
    pub slot: u8,
    pub life: LifeId,
    pub part: u32,
}

/// How a breaker spills: its form, the route bits its partitions have consumed so far, and its grant in bytes.
#[derive(Clone, Copy, Debug)]
pub struct SpillDecl {
    pub form: SpillForm,
    pub depth: Depth,
    pub grant: u64,
}

/// A spill form's mechanism (registered as `SpillImpl` beside `spill.breakers`).
pub trait SpillMech: Sync {
    /// The exit an insert takes when it finds no room. The row at `row` (`bytes` long) goes to its partition's stream,
    /// picked from the row's KeyHash word `h` at `d.depth`, and the breaker continues.
    fn spill_row(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, d: &SpillDecl, h: Value, row: Value, bytes: Value);
    /// After the input ends, the partition streams are read back into the breaker, one partition at a time.
    fn reload(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, d: &SpillDecl, x: &Exits);
    /// M7-SPILL (V4R-359): the partition bits hash build `k` spills at, when its probe (op `j` of pipeline `q`) and the
    /// program let it spill; `None`: it grows by name past its grant (D254).
    fn build_bits(&self, _px: &Px<'_, '_>, _k: usize, _q: usize, _j: usize) -> Option<u8> {
        None
    }
}

pub struct SpillImpl(pub &'static dyn SpillMech);

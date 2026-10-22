//! M3's sort run hosts (kernels host_abi/v4/m3.rs's sub-file, the exec v4/m3_sort.rs pattern): the resident seal, the
//! var table copy, the sealed runs' merge (`RunShape`), road sort.keylane's radix and sort.natural's skip. Owner M3-HOSTS.
use crate::host_abi::SEALED;

pub use super::raw::runs::{describe, sort_full_res, sort_full_res_var, sort_last_res, sort_last_res_var, sort_open_res, sort_open_res_top, sort_open_res_var, sort_open_res_var_top};
pub(crate) use super::raw::runs::index;

/// The `pad[0]` of a run sealed in its region (`sort_last`'s, `sort_open_res`'s): exec v4/m3_sort.rs merges only these.
pub const RUN_SEALED: u8 = SEALED;
/// The bytes the leader's merged run may take: a slot region's byte count is a u32 (driver grow.rs `MAX_REGION`, rounded
/// down to its 4 KiB region granule), less a MiB for the run's header and trailer.
pub const RESIDENT_MAX: u64 = u32::MAX as u64 - (1 << 20);

/// `RUN_KEYLANE` in a run's `pad[2]` (road `sort.keylane`, the IR's): key 0's `(flags, word)` pair orders its rows by an
/// LSD radix; ties on the pair take the full row compare.
pub const RUN_KEYLANE: u8 = 1;
/// `RUN_ORDERED` in `pad[2]` (road `sort.natural`, the IR's): set at the run's first row, cleared at the first row out of
/// order; a run that keeps it is not sorted.
pub const RUN_ORDERED: u8 = 2;
/// `RUN_CUT` in `pad[2]` (D508, the row walk's bound): a cut (m3_cut.rs) left the run's first `n` rows in key order, row
/// `n - 1` the bound exec v4/m3_sort.rs `run_bound` reads; cleared when the run is emptied (a put, an open's empty run).
pub const RUN_CUT: u8 = 4;
/// `RUN_HEAP` in `pad[2]` (USEROP K7, raw.rs `SortHeapStep`): a heap top's run (an exact top's, or ROW-TOPN's: a first key
/// ordered by its comparator under the constant word) holds PostgreSQL's bounded heap (its row indexes in cut.rs `HEAPS`);
/// cleared when the run is emptied or its heap is ordered.
pub const RUN_HEAP: u8 = 8;
/// `RUN_HELD` in `pad[2]` (D1236's held inner, op 14 for runs.sort): the fill raised and the driver holds its error for the
/// reader's first fetch (exec ops/sort/held.rs stamps it, `len` 0): the run reads empty, and a merge join's first inner
/// call (or a pull merge's first read of the run's end, D449) raises the held error (`m11_run_held`), as PG's ExecSort
/// raises at its first call.
pub const RUN_HELD: u8 = 0x80;
/// `RUN_HELD_AT` in `pad[2]` beside `RUN_HELD` (D2316, op 14 "held at len" for runs.incremental; exec ops/sort/held.rs
/// `stamp_at` sets both, `len` kept): an Incremental Sort's fill raised and its rows stay as the fill left them, every
/// closed prefix group in order. The emit's open seals the run in its region (rowsort `begin_in`), its reader reads the
/// rows PostgreSQL's batches handed on before the raising row and raises the held error past them (`sort_held_end`).
pub const RUN_HELD_AT: u8 = 0x40;

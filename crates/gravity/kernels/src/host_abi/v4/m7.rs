//! M7's host entries (M7-plan.md §6, cards/M7.md L1 / L2 hosts): the C-ABI functions M7's lowered IR calls for the joins
//! breadth: `m7_absorb(ctx, from_slot, to_slot) -> i64`, the drain end's merge taking the probe pipeline's seeded `m` (a
//! partial table, a run set through `adopt_runs`, a build) as one more partial (M7-plan §3.6). The marks, the walk and the
//! drain source are IR over the build region (no host per row). An L1 stub: each entry lands with its first reader and is
//! listed in the `hosts:` rows of the lowerv4 file that calls it; every entry runs its body under `guard!` (V4R-16's rule),
//! stashing PG's error and returning the error code. The spill entries are M7-SPILL's (v4/m7_spill.rs). Created by M7-DRAIN.

//! The keyed owner's table sizes (exec table/owner.rs, the `count(DISTINCT x)` two-round owner and its dense round): the
//! bounds its region and hot tables are laid out by. Each is a size bound, not an answer: past it the owner spills, folds
//! in a smaller table or takes the pair path, and every statement returns the same rows.

/// The round-2 hot table's ceiling: two slots per estimated group, clamped to [the swept 256 KiB floor, this] (owner.rs
/// `distinct_open`; the table is also held to a quarter of its region). CHOSEN, not measured: it bounds the table a group
/// over-estimate opens; a larger group set folds through the spill as before.
pub const ROUND2_T1_MAX: u64 = 8 << 20;
/// The runs form's floor bound in the least region the owner asks (owner.rs `distinct_min`: four times the estimated groups'
/// accumulators, at most this). CHOSEN, not measured: it bounds the region a large group estimate asks for; a region below
/// the table's need spills, so it moves no answer.
pub const RUNS_FLOOR_MAX: u64 = 16 << 20;

/// B04 / CAP-21 (agg's EX-05): whether the distinct owner's dense round 1 pays against its pair path, a pure reading of
/// its integers (exec table/raw/owner.rs reads it at the bind and for its region floor; agg's table.small can read it at
/// its open with its own). The area is a `u32` slot per key of the group key's `span` and an accumulator of `slot_bytes`
/// a key. Against the pair path it saves round 2's re-sink: each partition's table folds its own accumulator for every
/// group it sees, about one a deduped pair over a wide group set, where the area folds each group once (a 2M-row
/// count(*) + count(DISTINCT x) top over 64k keys spilled 92 MB on the pair path against the area's 47 MB and ran 2.7x as
/// long). It costs its open and walk, a slot a key of the span whatever the rows. So it pays where the `rows` the owner
/// folds (an upper bound of its pairs: the census's surviving rows; `u64::MAX` unknown) fill the span to at least one row in
/// SPAN_PER_ROW keys, and where the area fits `grant` (the floor exec's owner grows for it reads cost runtime `GRANT_FLOOR`,
/// the least grant admission hands out; at the bind `u64::MAX`, since the run time lays the area only in a region that
/// holds it, else the pair path).
/// FITTED on DEV data only (topn G1 perf-b04r: 2M rows in row-number order, the key a hash of it over spans 64k / 256k /
/// 1M, cut by the row number to 1 / 2 / 16 / 123 / 245 granules; quiet box 82, A B A B, --perf 5, the area against the pair
/// path): where the census rows filled the span to one row in 64 keys or more the area won 13 of 14 cells at W0 (x1.03-2.97;
/// the one loss 0.15 ms) and 9 of 14 at W8 (x1.11-5.3; the pair path won the 16-granule cut at every span, x0.26-0.54, a
/// width effect the rows do not read, and two cells under 4 ms; all kept on the area as before); at one row in 128 keys
/// (1M over one granule) the pair path won 2.4-2.6x at both widths. SPAN_PER_ROW is 64, the largest power of two the area kept every W0 win at.
/// The span's own ceiling is the dense word's (kernels t3_dense `SPAN_MAX`, 2^23 keys).
pub const fn dense_pays(span: u64, slot_bytes: u64, rows: u64, grant: u64) -> bool {
    const SPAN_PER_ROW: u64 = 64;
    span <= rows.saturating_mul(SPAN_PER_ROW) && span.saturating_mul(4 + slot_bytes) <= grant
}

/// decompose ops/agg/mod.rs `multi_counts` (PS1-32): the largest share of its relation's rows a text argument's distinct
/// values may be (the planner's ndistinct, `Facts::att_ndistinct`) for PSP2-34's one-pass count to key it. The one-pass
/// table holds an entry per distinct value, the host-sorted form (D1210) sorts every row. CHOSEN on DEV data (box 89, c8g,
/// W0; 2M-row pgrcolumnar filtered to 1M and its heap copy, text columns of 100 to 1.8M values; held out a 500k-row
/// pair): every cell whose estimated share was at most 0.80 ran one-pass in 0.04-0.92 of host-sorted, every near-unique
/// one (0.958-1.0) lost 1.02-1.32; 0.75 is the share where the held-out heap's line between its last win (0.57, 0.84)
/// and its loss (1.0, 1.32) still reads at most 1.05.
pub const TEXT_COUNTS_DISTINCT_SHARE: f64 = 0.75;

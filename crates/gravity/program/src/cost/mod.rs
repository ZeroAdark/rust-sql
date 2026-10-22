//! The cost seed (one home for the sizes a program is planned and granted by): the bytes a program's share-budgeted
//! breakers fill at a width (breakers.md "Grant admission by estimate"; the ask of `gravity_grant_mode=estimate` and the
//! class test of `=lanes`), and the partition bits a breaker spills by. The plan's row and width estimate (`of`) comes
//! with the stats lane's `Stats`; until then each caller reads the plan's rows itself.
//!
//! Its parts (gap-optoverfit §3.6, mechanism C): `breaker`, the one breaker-cost function (a share breaker's record, rows,
//! table and held bytes; the grant ask sums it); `machine`, the machine facts a size or a span derives from; `scan`, what a substring scan's rejected candidate and a counted byte cost; `fit`, the
//! break-even rule and the unit costs it is fitted with on dev data (both from `_support/cost_profile`, which the backend
//! crates read too). A family's threshold or size that cites the cost home is an expression over these, never a constant
//! of its own.
use crate::Program;

pub mod breaker;
pub mod codeif;
pub mod compile;
pub mod dict_front;
pub mod drain_set;
pub mod entry;
pub mod expr;
pub mod forms;
#[cfg(test)]
mod host_census;
pub mod num_front;
pub mod num_pair;
pub mod octet;
pub mod owner;
pub mod payload_plain;
pub mod pgrc;
pub mod runtime;
pub use cost_profile::{anchor, census, fit, hash, machine, scan, walk};

pub use breaker::BreakerCost;
pub use codeif::codeif_keys_by_image;
pub use fit::{break_even, break_even_div, fraction};
pub use machine::Machine;

/// A share breaker's record bytes when it has no owner layout to read them from (a cache line): the floor of a
/// layout-less record, which the planner's width of its rows raises (`breaker::record`, DR-1); only the ask's size moves
/// with it, never an answer; a breaker with a layout asks its own size (the width its floor too).
pub const FALLBACK_RECORD: u64 = 64;
/// One partition's groups' bytes: derived, the slice the kernels split a partitioned table into (their bits are
/// `groups x record / 512 KiB`, kernels table/params.rs), so `PARTITION_BYTES << bits` is the planner's groups' bytes (D365's
/// held bound); the pool a share opens for them is `breaker::POOL_PART_BYTES` a partition (DR-6 (B)).
/// MEASURED-HELDOUT (DR-2; shapecheck CONSTANT-SWEEP r5, box 62, the grouping corpus at W8, arms 128 KiB to 2 MiB, one take
/// each, the same answers every arm): the held-out set ran 126.8-131.0 s, within 3.3 % and not monotonic (128 KiB fastest,
/// 256 KiB and 2 MiB slowest, 512 KiB inside the spread), the bench set 18.5-19.5 s; the value stays the kernels' slice,
/// not a tuned speed.
pub const PARTITION_BYTES: u64 = gravity_abi::breaker::PARTITION_SLICE;
/// D365: a partitioned record table's held records are asked up to this many of its bits' tables (the planner's groups off 16x).
/// MEASURED-HELDOUT (W8 sweep 4 / 8 / 16 / 32 / 64 / unbounded over the held-out grouping set and the bench set, the
/// same answers every arm): time is flat within take-to-take noise (held-out 67.5-68.4 s, bench 18.0-18.5 s, unbounded
/// included), so the value stays as the bound on the ask, not a tuned speed.
pub const HELD_TABLES: u64 = 16;
/// A FunctionScan store window's rows at most (a narrower row's window holds fewer, near the window's bytes; a multiple of 64,
/// the validity words' rows). Chosen capacity, not measured: it bounds one window's rows; past it the next window holds
/// the rest, so it moves no answer.
pub const STORE_WINDOW_ROWS_MAX: u32 = 256;
/// C1 part 2 (NEW-43 (B), lowerv4 ops/sort/inc_stream.rs `cursor_pull`): an INCSTREAM cursor reader's first batch, the
/// rows its stream pulls before it answers the first FETCH (each later batch twice the last, at most half the run's room).
/// PostgreSQL's, not measured: nodeIncrementalSort.c DEFAULT_MIN_GROUP_SIZE, the rows an unbounded Incremental Sort loads
/// before it looks for its batch's prefix group close. It moves no answer (a batch past the room takes the fallback).
pub const INC_CURSOR_BATCH_FIRST: i64 = 32;
/// decompose ops/append.rs copies(): the arms from which a top with no presorted key streams into its union rather than keep
/// the Materialize form (no R-KEEP regression of the small union tops). MEASURED: the two forms' costs
/// per execution, box 32 (c8g, quiet):
/// the Materialize form's driver walk grows with the cube of its feeds (Node::free_after: every earlier producer per
/// pipeline, each scanning every proto): 27.8 s at 4,100 feeds (e2e v4/m4_l20), ~0.4 ns x P^3; the arms form adds ~1 ms
/// a statement at width 2 or more (a worker's 32 MiB Sort share reserved past W and G, zeroed) and ~10 us an arm (exec
/// ArmSort's hold): app-0156's 5 arms W8 0.76 -> 1.68 ms, a 40-partition top W0 1.25 -> 1.67 ms, 4,100 arms 27.8 s ->
/// 0.65 s. They meet near 200 arms; 256 keeps every measured small top on the Materialize form. A measured crossover of
/// two costs that are FOR MAIN items (contract-runtime: free_after, the past-G share): revisit when either lands.
pub const UNION_TOP_ARMS_MIN: usize = 256;
/// The per-code front's run-time decline (kernels dict/strategies/percode.rs `declines`, D2804; the lowerer passes the word
/// to the window's host): a serial claim whose sample of folds costs the front at least what the per-row path pays for
/// its rows turns the front off for the rest of the claim, its codes and its opened map records weighed by the front's
/// fitted costs against the per-row path's (cost_profile front.rs `PERCODE_CODE`, `PERCODE_OPENED`, `PERCODE_ROW`;
/// `percode_declines`). A sample is a full fold, or folds of fewer rows than a full list (a part's tail, a window's rest
/// after a full list, a filtered window) added up until they hold as many: a few rows never decide for the rest of the
/// claim. FITTED (D-04's sweep, generated keys over two 1M-row parts at W0): a fresh code and a fresh record every row
/// costs the front 1.6 - 2.2x the per-row path, a code and a record in two rows 1.02x, fresh codes whose record repeats
/// every 4 rows 1.10x, codes and records repeating every 4 - 16 rows 0.5 - 0.75x; six derived keys of real-shaped text
/// columns decide as before, within 0.95 - 1.03x of either form. Long key runs over many codes a key keep the front where
/// declining wins, as the 50 % opened rule before it did (the fit's scope: cost_profile front.rs `PERCODE_CORPUS`). A
/// parallel claim never declines: its front cuts the records every worker writes for the owner merge. Word: the code
/// weight << 32 | the opened record's (0: never).
pub const PERCODE_DECLINE: u64 = cost_profile::front::PERCODE_CODE_W << 32 | cost_profile::front::PERCODE_OPENED_W;
/// PS1-29: a per-code `PROGRAM` body over a code column with at least `PERCODE_DATUM_PCT` % as many entries as the
/// relation has rows (each entry read about once) reads the column's datum per row instead (lowerv4 dict/percode.rs
/// `attach`); under `PERCODE_DATUM_ROWS_MIN` rows the table stays (the forms differ by less than the bar's +0.2 ms).
/// MEASURED (box 51 held whole, c8g 16 vCPU, 1M-row pgrc columns stored verbatim, fallback off, condition cache off,
/// medians of 10 hot runs over two postmasters, W0 = W8): a bpchar BETWEEN over a unique column reads its table in 106 ms,
/// its datum in 68 (row executor 59); md5(x) < c at 1.0 entries a row: table 446, datum 424; at 0.91: table 404, datum
/// 423; at 0.75: table 340 (row executor 408 throughout). A costly body crosses near 0.95 (the table pays it once an
/// entry), a cheap one near 0.6 (the table's fill dominates): 95 % is where the datum wins for any body.
/// R5-B: `forms::percode_datum` decides from the measured rows where the datum or the table wins at both ends (W0 and the
/// server's widest width); these two keep the rule of before where the ends disagree (and under KEYED-31's image guard).
pub const PERCODE_DATUM_PCT: u64 = 95;
pub const PERCODE_DATUM_ROWS_MIN: u64 = 4096;

/// PS1-29's rule over a relation of `rows` rows whose column holds `entries` per-code entries: nearly an entry a row
/// (`PERCODE_DATUM_PCT`), at least `PERCODE_DATUM_ROWS_MIN` rows. lowerv4 dict/percode.rs `reads_datum` (a body reads the
/// datum, where `forms::percode_datum`'s ends disagree) and decompose ops/scan `datum_text_lanes` (D3016: a column read
/// only through calls is a by-reference lane).
pub const fn near_unique(rows: u64, entries: u64) -> bool {
    rows >= PERCODE_DATUM_ROWS_MIN && entries.saturating_mul(100) >= rows.saturating_mul(PERCODE_DATUM_PCT)
}

/// D3016: the widest average datum (bytes) a near-unique text column read only through calls is laid down by reference
/// at (decompose ops/scan `datum_text_lanes`). A by-reference lane's arena holds its granule's images, up to 8192 rows,
/// in every source that reads it (a worker each), where the dictionary's render table is one resident copy shared by
/// them: 1 KiB keeps a granule's images under 8 MiB a source. CHOSEN, not swept: the near-unique columns the per-row
/// render cost (phone numbers, names, comments, URLs) average well under 200 bytes; a column of 67-136 KB documents
/// (e2e scan_pgrc_d2582) held 1.1 GB a granule and took the server past its memory bound.
pub const DATUM_TEXT_WIDTH_MAX: u32 = 1024;
/// H08: road code.sparse's table form (hosts granule_source/fetch.rs `sparse_stage`) walks a granule's packed codes for
/// its non-base rows in place of the whole decode, keeping only the rows whose code the table may pass, so the base
/// rows are never decoded nor tested and the lanes gathered beside it are read for the survivors alone. A part whose
/// dictionary holds more values than this share of its rows is not walked at all (no value can cover the rest), and a
/// part whose walked granule held more than this share of non-base rows decodes its later granules whole. FITTED: the
/// non-base share where the walk's and the whole decode's lines cross on this host (cost_profile sparse.rs `dense_pct`;
/// 70 % at the reference and, its prices sharing one anchor, on every host, `DENSE_PCT`: D-04's sweep over 1 .. 95 %
/// non-base shares, the walk 0.23 - 0.26x of the whole decode at 1 %, 0.5x at 10 %, 1.16 - 1.32x at 95 % (W0);
/// real-shaped text columns agree: one 13 % off its most common value walked at 0.73x, one 88 % off it decoded whole
/// where the walk costs 1.33x).
pub fn code_sparse_dense_pct() -> u64 {
    cost_profile::sparse::dense_pct()
}
/// The dictionary build-buffer pool's smallest kept buffer in words (hosts dict/text_hosts.rs `reuse`): a buffer under it
/// is freed, not kept for the next part's build. It moves no answer (a build reads only the words it wrote). CHOSEN, not
/// measured: 64 KiB, below which a fresh zeroed build costs less than the pool's bookkeeping and the memory it pins.
pub const DICT_POOL_MIN_WORDS: usize = 8 << 10;
/// The memory policy's sizes (B, its reserves, every cache's cap, the grant pool's capacity) live in `mempolicy::sizes`;
/// their names stay here for every caller.
pub use mempolicy::sizes::{
    budget_auto, budget_of, build_pool_cap, condition_cache_cap, derived_pool_cap, os_reserve, page_cache_floor_default, part_cache_cap, process_caches_cap, reserve_high, reserve_low,
    resident_tables_cap, zone_store_cap, Budget, BUILD_POOL_RAM_64THS, CONDITION_CACHE_RAM_64THS, RECLAIM_INLINE_MAX, RECLAIM_TICK_MS, TABLES_RAM_64THS, WORK_POOL_MIN,
    ZONE_STORE_RAM_64THS,
};
/// The code-range verdict (lowerv4 dict/strategies/verdict_coderange.rs): past this many table words (64 codes each) from
/// a window's lowest code to its highest, the range goes unread (`Maybe`) and the window is read as without the verdict.
/// It moves no answer. CHOSEN, not measured: it bounds the window open's range read to 1 KiB of the table (8 Ki codes).
pub const CODERANGE_WORDS_MAX: i64 = 128;
/// The largest record the light code front takes (lowerv4 dict/strategies/codefront_light.rs: a count top's record, an
/// Fp128 image, a flags word, a count and a representative); a wider record runs on the hashed table. It moves no answer.
/// CHOSEN, not measured: the text front's rung value, carried unchanged.
pub const CODE_FRONT_MAX_RECORD: u32 = 256;
/// A VALUES list's rows that need code of their own (every row of a list without the literal table, else the rows
/// holding an expression: the bisection's leaves). Chosen capacity, not measured: it bounds the per-row code one VALUES
/// source lowers where the list cannot stream (a cell reading an executor parameter or a subplan). CAP-29: past it a list
/// with the table streams those rows through PostgreSQL's evaluator at each row's turn (write.literals' stream form,
/// protos `LiteralsStream`), no code per row; one that cannot stream refuses by shape.
pub const VALUES_CODED_ROWS_MAX: usize = 1 << 16;
/// D2907: the cells that are not a Const in a VALUES list's coded rows past which a list that can stream streams them
/// (`VALUES_CODED_ROWS_MAX`'s stream form) instead of compiling a leaf of code per row. MEASURED (box 69 held whole, c8g
/// 16 vCPU, W0, 5 servers an arm, one binary running both forms; the v5 plan's lanes/result/work/cap29/cut2/m.tsv): the
/// first execution compiled / streamed, rows of one coded cell `(i, length(current_user))`: 1 cell 0.82 / 0.81 ms, 2
/// 0.90 / 0.84, 4 0.94 / 0.76, 8 1.06 / 0.81, 16 1.22 / 0.85, 64 2.61 / 1.16, 128 4.20 / 1.56 (now() rows alike; four
/// coded cells a row x1.50 at 16 cells, x2.59 at 128, x3.75 at 512; the compiled form's compile grows ~N^2..N^3, 30+ s at
/// 4096 rows), every later execution a tie (x0.90-1.20). The fit: 4 is the largest count whose compiled first execution
/// stays within the bar (max(1.2x, +0.2 ms)) of the stream's; from 8 it is past both. R5-B: `forms::values_coded_cells_max`
/// reads the cut from the class table's rows (the largest count whose compiled first execution is at most the stream's);
/// this is the rule of before, kept only for a table without those rows.
pub const VALUES_CODED_CELLS_MAX: usize = 4;
/// The literal words a VALUES list's coded rows may take compiled before a list that can stream streams them instead
/// (lowerv4 ops/values/lower.rs `make`). CHOSEN, not measured: the word plan's bound of before (`limits::PARAM_WORDS` was
/// `1 << 14`), kept when the bound grew so no list changes form; a list that cannot stream compiles up to the bound itself.
pub const VALUES_CODED_WORDS_MAX: usize = 1 << 14;
/// hosts ops/scan/page_hints.rs: every relation file's block -> buffer hint array together (4 B an entry), past which the
/// table starts over (a lost hint is one full lookup). CHOSEN, not measured: a memory bound, not a speed knob; 8 Mi
/// entries = 32 MiB of process memory and covers 64 GiB of 8 KiB pages (BLCKSZ); an array is already capped at the
/// pool's size (NBuffers). The floor: page_hints grows the budget to the pool's size past it (H42), so no pool loses hints.
pub const PAGE_HINTS_BUDGET: usize = 8 << 20;
/// A hashed SubPlan's key set's first table (exec ops/subplan/keyset.rs): its slots before the first doubling; the load
/// stays at or under one half, so a probe always meets an empty tag. CHOSEN, not measured: a small power of two, so a set
/// of a few keys allocates little; the set doubles past half load, so the value moves no answer.
pub const KEYSET_MIN_SLOTS: usize = 16;
/// The largest `n` a text-first top's part cut keeps codes for (lowerv4 dict strategies/code_cut.rs: the entry is the cut,
/// the owner, the length, the least code, then `n` codes in frame words under a u8 index, so 4 + n <= 255). B3-12: a
/// bound of that structure, not a speed knob: the entry is a max-heap (kernels `topk_cut`, log2(n) moves a replacement,
/// resting while its part's rows descend), a top of up to this many rows is cut from its Demand, and past it the top
/// runs unfiltered.
pub const TOPK_CUT_MAX_N: u32 = 251;
/// USEROP (lowerv4 ops/shared/one_batch.rs): the bytes a PG 18.6 hash table may account (ExecHashTableInsert's spaceUsed
/// plus its bucket array) and still hold one batch under any setting: the least hash_mem (work_mem's floor 64 kB,
/// guc_tables.c, times hash_mem_multiplier's floor 1.0) less 4 KiB for the skew table ExecChooseHashTableSize may carve
/// from it (SKEW_HASH_MEM_PERCENT, 2 %, and its bucket array).
pub const PG_HASH_ONE_BATCH_BYTES: u64 = 60 << 10;
/// ExecChooseHashTableSize's least bucket count (nodeHash.c).
pub const PG_HASH_MIN_BUCKETS: u64 = 1024;

/// twopass.having's per-execution choice between one pass and two (exec ops/sort/topn/proto.rs `having_cost`), per input
/// row in units of one scan read: the count fold and pass 2's admission probe; two passes pay when (count + folds) x the
/// share of rows the floor drops > 1 + count + probe. Their sum with the fold weights is what the choice reads (lowerv4
/// strategies/twopass.rs `fold_eighths` carries the weights). MEASURED (box 86, a guest slot, a 2M-row pgrcolumnar table,
/// 33k groups of mean 60 under floors around the mean, forced one / two arms interleaved, 2 rounds, work_mem 4MB): text
/// min + max two / one 1.41 with no row dropped to 0.19 with 89 % dropped, crossing near a dropped share of 0.28 at W0
/// and 0.4 at W8; text min alone near 0.15 at W0 and 0.5 at W8.
pub const HAVING_COUNT: f64 = 0.25;

/// The estimate at which a bitmap heap scan takes the morsel form (decompose ops/scan/closure.rs `morsel_form`); a smaller
/// bitmap keeps the serial bitmap source. MEASURED (box 18, pgbench 1 client, 8 s per cell, a 2M-row heap with a btree
/// key of 20 rows per value, each row on its own page; serial source vs morsel form, ms at W0 / W8): 160 rows .188/.188
/// vs .220/.258, 300 rows .258/.259 vs .283/.287, 600 rows .390/.388 vs .401/.344, 1000 rows .567/.568 vs .569/.403,
/// 2000 rows 1.035/1.020 vs 1.006/.542: the form is no slower at W0 from about 1000 rows and wins at W8 from about 500.
pub const BITMAP_MORSEL_MIN_ROWS: f64 = 1024.0;

/// The estimate at which an order-free btree index scan takes the bitmap morsel form (decompose ops/scan/closure.rs
/// `index_morsel`); a smaller scan keeps the serial index source, whose page-at-a-time fetch with no bitmap to build is
/// cheaper at W0. MEASURED (box 18, the same table and pgbench cells as BITMAP_MORSEL_MIN_ROWS, bitmap scans off; serial
/// index source vs morsel form, ms at W0 / W8): 2000 rows .817/.803 vs 1.021/.543, 5000 rows 1.90/1.84 vs 2.50/1.02,
/// 10000 rows 4.29/4.70 vs 4.95/1.81, 20000 rows 9.53/9.26 vs 9.16/3.59, 40000 rows 19.0/19.4 vs 13.2/5.10: the form is
/// no slower at W0 from about 20000 rows. This is the threshold of a serial session (`Facts::serial`, D3047's class:
/// its statements run on the leader alone); a session with workers takes INDEX_MORSEL_MIN_ROWS_WIDE.
pub const INDEX_MORSEL_MIN_ROWS: f64 = 20000.0;

/// INDEX_MORSEL_MIN_ROWS in a session with workers, where the pool walks the heap pages. MEASURED (box 39, the same
/// cells, W8, serial index source vs this threshold): 2000 rows .813 vs .545 ms, 5000 2.41 vs 1.03, 10000 4.86 vs 1.97,
/// 20000 9.54 vs 3.48; at 1000 rows and below both take the serial source (160-1000 rows within 2 %), as the bitmap heap
/// scan's BITMAP_MORSEL_MIN_ROWS.
pub const INDEX_MORSEL_MIN_ROWS_WIDE: f64 = 1024.0;

/// A2 entry 2: the estimated index rows per pool thread of a btree's parallel leaf walk (hosts bitmap_image.rs
/// `getbitmap_wide`): the morsel leader's build walks the leaves on min(width, rows / this) threads, the leader alone under
/// two. ARBITRARY until the share-e2 cells (200k-1M-row ranges, W8) measure where the fan-out's start (one image view and
/// scan per thread, the bitmaps' union) pays; a btree leaf holds a few hundred TIDs, so this is a few dozen leaves a thread.
pub const INDEX_LEAF_ROWS_PER_WORKER: u64 = 16384;

/// D3190 (merge.cosplit, exec ops/sort/cosplit.rs): the parts a co-split merge join's outer is cut in per pool thread, so
/// a skewed key range evens out over the claims after it (the pool's sizer may hand several consecutive parts to one
/// claim). ARBITRARY until timed takes of a merge over two sorts (W8 / W16) measure the claims' tail; at most
/// `COSPLIT_MAX_PARTS` (gravity_abi).
pub const COSPLIT_PARTS_PER_WORKER: usize = 4;

/// D3190: the fewest outer rows a co-split part is worth (a claim's open, its two views, its machine's reset and, over put
/// runs, a fence seek per run). ARBITRARY as `COSPLIT_PARTS_PER_WORKER`: an outer under two such parts runs as one claim.
pub const COSPLIT_PART_ROWS_MIN: u64 = 4096;

/// D3336 (win.partruns, exec ops/window/proto.rs `parted`): the window objects a window read by claims gets per thread
/// of its reader. The serial fill hands whole partitions to the objects in turn, so with more objects than threads a
/// claim of a big partition's object leaves the others to the rest of the pool. ARBITRARY (each object is a WinObj with
/// its own contexts, a few KiB) until timed W8 takes of the TPC-DS window statements measure the claims' tail; at most
/// `PARTRUNS_OBJECTS_MAX`.
pub const PARTRUNS_OBJECTS_PER_WORKER: usize = 2;

/// D3336: the most objects a window read by claims gets.
pub const PARTRUNS_OBJECTS_MAX: usize = 128;

/// 5c-F6 (exec ops/sort/topn/proto.rs `tie_bail`): a top-count two-pass split (twopass.count, led by a count or a sum)
/// whose pass 1 admits more than this percentage of its records finishes as one pass (pass 2 folds every key).
/// MEASURED (box gensum, c8g.4xlarge, Neoverse V2, 16 vCPU; corpus: a 4M-row
/// pgrcolumnar table of 50k groups of about 80 rows with two text extremes a group, the admitted share swept by an
/// ascending sum of a 0/1 expression whose n-th best is 0; one take per arm): two passes over one at shares 0.02 / 0.14
/// / 0.30 / 0.45 / 0.67 / 0.82 / 0.96 cost 0.12 / 0.18 / 0.43 / 0.63 / 0.97 / 1.18 / 1.38 x at W0 and 0.15 / 0.28 /
/// 0.46 / 0.65 / 0.93 / 1.10 / 1.25 x at W8, even near 0.69 (W0) and 0.73 (W8).
pub const TWOPASS_TOP_ADMIT_CUT_PCT: u64 = 70;

/// D3157 (twopass.floor, exec ops/sort/topn/proto.rs): the largest top whose bare pass 1 shares one floor of its best
/// keys across its claims (`16 n + 24` bytes laid per execution); a larger top keeps each claim's own floor alone.
/// ARBITRARY: each claim's own floor already takes `8 n` bytes a thread, and 2^16 keys (1 MiB) is past every measured
/// top measured (LIMIT 10 to 1000).
pub const TWOPASS_SHARED_KEYS_MAX: u64 = 1 << 16;

/// An ordered btree scan read by the pool in key ranges (hosts index_ranges.rs): the ranges cut per pool thread, so a
/// claim is a small share of the scan and a slow range (a dense key, a page not cached) leaves the others work. ARBITRARY
/// until timing cells measure the cut's cost (one descent per range) against the skew it absorbs.
pub const INDEX_RANGES_PER_WORKER: usize = 8;

/// Pass 2's admission probe in the having split's choice, per input row in units of one scan read. Fitted, MEASURED as
/// `HAVING_COUNT`.
pub const HAVING_PROBE: f64 = 0.25;
/// The having split's floor: the second read plus the probe, `8 x (1 + HAVING_PROBE)` eighths of a read; a split whose
/// folds weigh no more never pays (lowerv4 strategies/twopass.rs `having_split`). Derived.
pub const HAVING_SPLIT_FLOOR_EIGHTHS: u64 = 10;
/// One min / max fold over text codes in the having split's choice, in eighths of a read: the split was taken for it and
/// ran two passes always before, so its weight puts the crossover below every measured one (one pass only under a floor
/// that keeps nearly every row, where one pass was faster at W0 and W8 alike). Fitted, MEASURED as `HAVING_COUNT`.
pub const HAVING_CODE_FOLD_EIGHTHS: u64 = 240;
/// One min / max fold over a Var slot (an unscaled numeric, text under a collation that does not order by bytes): one
/// pass before, so its weight puts the crossover above every measured one (two passes only under a floor that drops
/// most rows); one such fold alone stays under the split's floor. Fitted, MEASURED as `HAVING_COUNT`.
pub const HAVING_VAR_FOLD_EIGHTHS: u64 = 9;

/// Per share breaker (`breaker::of`): every worker's share (`BreakerCost::share_at`: a partitioned record table's block pool at
/// the bits it opens, DR-6 (B); else its table, or unpartitioned the rows × record) plus the owner's fold of the same, and the
/// T1 a worker opens (`BreakerCost::t1`: a partitioned record table's only, DR-6 (1)); `None` without a share breaker. At
/// least the largest share at each of `max_live` shares (`Grant::split` divides by it, D149); a partitioned record table's
/// largest is the records its workers hold (D365).
pub fn grant_need(program: &Program, width: u16) -> Option<u64> {
    grant_need_of(&program.pipelines, program.slots.max_live, width)
}

/// `grant_need` over `pipelines` (a program's, or the same with a statement's estimates bound: KEY-PREDICTED-ROWS road (b),
/// exec admit `bound_all`) under its `max_live`.
pub fn grant_need_of(pipelines: &[crate::Pipeline], max_live: u16, width: u16) -> Option<u64> {
    grant_need_of_at(pipelines, max_live, width, false, false)
}

/// `grant_need_of` under `pgrust.gravity_exact_size` (`breaker::of_at`): exactsize B1c, `exact_build` asks a hash build's
/// records alone; D3205, `end_rows` asks a serial pipeline's breaker by the planner's rows at its end, not 0. `false,
/// false` is `grant_need_of`, byte for byte.
pub fn grant_need_of_at(pipelines: &[crate::Pipeline], max_live: u16, width: u16, exact_build: bool, end_rows: bool) -> Option<u64> {
    let w = width.max(1) as u64;
    let mut sum = 0u64;
    let mut any = false;
    let mut most = 0u64;
    for p in pipelines.iter() {
        let Some(c) = breaker::of_at(p, exact_build, end_rows) else { continue };
        any = true;
        let share = c.share_at(width);
        most = most.max(share).max(c.held);
        sum = sum.saturating_add(share.saturating_mul(w + 1)).saturating_add(w.saturating_mul(c.t1()));
    }
    any.then_some(sum.max(most.saturating_mul(max_live.max(1) as u64)))
}

/// A breaker's partition bits when the planner gave no rows.
pub const DEFAULT_BITS: u8 = gravity_abi::breaker::PARTITION_DEFAULT_BITS;
/// The bytes a stored split writes per call (driver mem/spill.rs `scatter_build`): a partition's rows go out in runs of
/// about this many bytes. Chosen capacity, not measured: it bounds the split's one write buffer / a partition's rows past
/// it go out in another run of the same file.
pub const SPILL_SCATTER_CHUNK: usize = 256 << 10;

/// The bytes a hash join build's spill holds per partition before it writes them as one epoch of the leader's file
/// (lowerv4 ops/hashjoin spill/raw/region.rs). CHOSEN, not measured: it bounds a partition's write buffer / a partition
/// past it writes several epochs, and its rest at the end.
pub const JOIN_SPILL_EPOCH: usize = 64 << 10;

/// The sort runs' parts a phase-2 cut reads and sums per lock (exec ops/sort/merge.rs `prepare_partial`: positions and
/// sums pass through two stack arrays of this many entries). CHOSEN, not measured: it bounds the stack arrays (about 3 KiB)
/// and how many parts one lock covers.
pub const SORT_PREP_BATCH: usize = 256;

/// The bytes a hash join drain part gathers on the stack before each spill put (lowerv4 ops/hashjoin drain/raw.rs
/// `write_drain`); a piece past it goes in its own pieces. CHOSEN, not measured: it bounds the stack buffer.
pub const DRAIN_PUT_BATCH: usize = 8 << 10;

/// The least cap on the build records whose keys a run-time filter exports one by one, as a key set (lowerv4
/// ops/hashjoin strategies/rtfilter.rs, `rtfilter_set_max`); a larger build exports its key range alone. CHOSEN, not
/// measured: a set this small costs about a microsecond, whatever the probe.
pub const RTFILTER_SET_MAX: usize = 64;

/// D3148 (lowerv4 ops/hashjoin strategies/domain.rs, road join.domain): the most build records whose part-0 keys the
/// runner lays as a keyed hash end's admitted set (about 32 bytes a key with its prefilter, charged to the statement's
/// grant); a larger build admits every key. MEASURED (box 10, D3148): the leader lays about 21 ns a key (20.8 ms for
/// 1M keys), so 2^17 keys take about 2.7 ms, under the 5 ms bound of a leader-only serial site, and the set (about
/// 4 MiB) stays cache-resident for the probe (a 1M-key set probed without prefetch ran x2.97 at W0 on an unselective
/// build). Laying from the workers' build partitions (D3172) would lift it.
pub const DOMAIN_SET_MAX: u64 = 1 << 17;

/// D3148: join.domain takes a build of at most `groups / DOMAIN_RATIO` rows, by the planner's build rows and group
/// estimate (and lays at most that many keys at run time): past it the set would cut too few rows to pay its probe and
/// the fronts a plain hash end keeps. CHOSEN, not measured.
pub const DOMAIN_RATIO: u64 = 4;

/// D3194 (lowerv4 ops/hashjoin strategies/rowfilter.rs, road join.rowfilter): the build's Bloom words take at least this
/// many bits a key, rounded up to a power of two of 64-bit words (so 8 to 16 bits a key), four bits a key in one word: a
/// false-positive share of about 3.3 % at 8 bits and 0.5 % at 16 (the blocked-Bloom formula; the unit test measures it).
/// CHOSEN, not measured: the words of a 1.46 M-key build then fit a 2 MiB L2.
pub const ROWFILTER_BITS_PER_KEY: u64 = 8;

/// D3194: the most Bloom words a build lays, as log2: half one core's L2 share of `l2_share` bytes (2^17 words, 1 MiB, on
/// a 2 MiB L2), so the words stay in L2 beside the probe's own rows; a build of more keys than `ROWFILTER_BITS_PER_KEY`
/// fills takes fewer bits a key in them, down to `ROWFILTER_MIN_BITS_PER_KEY`, and one past that lays none. Words past
/// the L2 miss as the directory's head does, so the test saves no miss and adds its work, and the lay pays a missed write
/// a key (seen on a 16-vCPU dev box, shared, W0: two builds of about 3.2 M keys with 4 MiB of words each cost their query
/// +6 %, the lay +60 ms and the probes +240 ms; a 1.46 M-key build with 2 MiB of words +10 % on a probe with one late
/// column, 0.83x on one with two numeric ones). DERIVED from the machine.
pub fn rowfilter_max_bits(l2_share: u64) -> u32 {
    (l2_share / 16).max(8).ilog2()
}

/// D3194: the fewest bits a key a build's Bloom words may take when `rowfilter_max_bits` caps them (four bits of one word
/// a key: a false-positive share of about 18 % at 4 bits, 8 % at 5.7). CHOSEN, not measured.
pub const ROWFILTER_MIN_BITS_PER_KEY: u64 = 4;

/// D3194: the probed rows the sample sees before the pass rule may switch a build's filter off (`ROWFILTER_PASS_SHIFT`).
/// CHOSEN, not measured.
pub const ROWFILTER_SAMPLE: u64 = 4096;

/// D3194: the filter goes off once more than 1/2^`ROWFILTER_PASS_SHIFT` of the sampled rows pass (1/4: the rule a
/// vectorized engine's probe-side Bloom filters follow). CHOSEN, not measured.
pub const ROWFILTER_PASS_SHIFT: u32 = 2;

/// D3323 (lowerv4 ops/hashjoin strategies/rowfilter.rs `window`, road join.rowfilter.cost): what a probe row that passes
/// its build's filter pays for the test, in ns on the reference machine (Graviton4): the block test's image, hash and word
/// read, then the walk's own hash. MEASURED (box 45, c8g.4xlarge, one core): a 1.5 M-key HASH build (its directory past
/// L2, its words at the cap) probed by 6 M rows that all match, the filter forced on vs road join.rowfilter off: 1,213 vs
/// 1,165 ms (min of 5), +48 ms = 8.0 ns a passing row (+2.28 G instructions).
pub const ROWFILTER_TEST_NS: u64 = 8;

/// D3323 (road join.rowfilter.cost): what a probe row the filter rejects saves, the directory head it no longer reads (a
/// directory past L2, a miss inside the probe pipeline's dependent chain, D3168), in ns on the reference machine.
/// MEASURED (boxes 50 and 73, c8g.4xlarge, profiles of date-clustered probes over 1.5 M- and 3 M-key builds whose
/// directories are past L2): 744 ms of directory head loads over 32 M probe rows (23 ns) and 1,186 ms over 60 M (20 ns);
/// at 8 workers the first probe's filter on vs off spent 609 vs 1,049 ms of CPU over the same 32 M rows at about 92 %
/// rejected (13.8 ns net a row, 23 ns a rejected row once the test's ROWFILTER_TEST_NS is added back). A probe whose
/// head reads overlap freely misses less (a bare count over 6 M rejected rows saved 3 ns a row): there the filter stays
/// on up to a pass share it does not pay at (+2 to +4 % of that statement's time at 31 to 100 % passing).
pub const ROWFILTER_HEAD_NS: u64 = 23;

/// D3323 (road join.rowfilter.cost): once a probe's filter has its first verdict, each participant samples it again on one
/// window in this many of its own (a power of two), running the filter on that window even while it is off: a filter the
/// first rows turned off (a probe table clustered on the join's bound, D3323) arms again when later rows stop matching,
/// and one that stopped paying goes off. CHOSEN, not measured: an off filter's re-sample costs at most
/// ROWFILTER_TEST_NS / ROWFILTER_RESAMPLE = 0.25 ns a probe row.
pub const ROWFILTER_RESAMPLE: u64 = 32;

/// JOIN-09: join.buildside swaps an INNER pair (builds the probe side, probes with the build side) when the build side's
/// records hold more than `BUILD_SIDE_BYTES` x the probe side's bytes (rows x record words: key images, the link, the
/// payload the probe reads), or more than `BUILD_SIDE_ROWS` x its rows. The program serves every width and the width is
/// the run's. Fitted on dev builds (box 46, a 200,000-row side of 1 / 3 / 6 payload columns, unique keys or 200 rows a
/// key, against 300,000 to 2,400,000 rows of 1 or 4 columns): at W0 the swap pays past a bytes ratio of 1.1 to 2.4 (2.4
/// the largest, taken here); at W8 the swapped form was faster at every ratio from 1.5 (2x to 6x), so the rows bound
/// keeps every swap made before (dropping one costs 2x to 3x at W8) and the bytes bound adds those that also pay at W0.
pub const BUILD_SIDE_BYTES: (u64, u64) = (12, 5);
pub const BUILD_SIDE_ROWS: u64 = 4;

pub const fn build_side_swaps(build_rows: u64, probe_rows: u64, build_words: u64, probe_words: u64) -> bool {
    let (num, den) = BUILD_SIDE_BYTES;
    build_rows > probe_rows.saturating_mul(BUILD_SIDE_ROWS)
        || build_rows.saturating_mul(build_words).saturating_mul(den) > probe_rows.saturating_mul(probe_words).saturating_mul(num)
}

/// The rows of one pgrc granule, the unit a zone summary covers: pgrc_format geom.rs `GRANULE_ROWS`, the writer's
/// default cut (hosts tests/cost_pgrc_granule.rs holds the two equal). A part cut otherwise only moves an estimate.
pub const PGRC_GRANULE_ROWS: u64 = 8192;

/// D3245 (road join.rtfilter.multi): the most build records whose keys a run-time filter reads to fold into runs of
/// consecutive keys when the build is past its set cap (`rtfilter_set_max`): the runner sorts them on the leader, about 22
/// ns a key (JOIN-07's measure), so 2^15 keys cost about 0.7 ms; a set of 200 year-months of days (about 6,000 keys, 200
/// runs) folds within it. CHOSEN, not measured.
pub const RTFILTER_RUNS_READ: usize = 1 << 15;

/// JOIN-07: the most build records whose keys a run-time filter over a pgrc probe of `probe_rows` rows exports as a key
/// set: the probe's granules (`PGRC_GRANULE_ROWS` rows each), at least `RTFILTER_SET_MAX`. n keys skip every granule
/// that holds none of them, so a set of fewer keys than granules can cut granules wherever the keys fall; past that a
/// spread set cuts none and its cost (about 22 ns a key, serial on the leader, against about 5.7 ns a probe row) is
/// all that is left. Fitted on dev builds (box 46, a sorted 8,388,608-row probe of 1,024 granules): spread keys W0 / W8
/// 45.8 / 6.6 ms without the set against 12.3 / 2.4 with it at 256 keys, equal at 1,024, 1.19x / 1.98x slower at
/// 262,144; clustered keys gain at every count. `None` rows (not a pgrc relation): the floor.
pub const fn rtfilter_set_max(probe_rows: Option<u64>) -> usize {
    let granules = match probe_rows {
        Some(r) => r.div_ceil(PGRC_GRANULE_ROWS),
        None => 0,
    };
    if granules > RTFILTER_SET_MAX as u64 {
        if granules > u32::MAX as u64 { u32::MAX as usize } else { granules as usize }
    } else {
        RTFILTER_SET_MAX
    }
}

/// Decision 3's partition count: `rows` x `record` over `PARTITION_BYTES` partitions, a power of two, 3..=11 bits
/// (`DEFAULT_BITS` with no rows): the one rule (DR-6, `gravity_abi::breaker::partition_bits`) over the reference slice,
/// so a lowered program is the same on every machine.
pub const fn spill_bits(rows: u64, record: u32) -> u8 {
    gravity_abi::breaker::partition_bits(rows, record as u64, PARTITION_BYTES)
}

/// hosts ops/window: the stored rows one chunk of a window object's row store holds (a chunk never moves, so a stored
/// row's address lives until its partition is released, the fill's next open or the execution's end). A chosen
/// allocation granularity, not measured and not a PostgreSQL limit: it decides only how the rows are allocated, never an
/// answer.
pub const WINDOW_CHUNK_ROWS: usize = 256;

/// hosts ops/window (D3345): a window object's row store carves its chunks and by-reference copies in order from heap
/// segments of half its live heap bytes within these bounds (past work_mem, from mapped temp files). Chosen allocation
/// granularities, not measured: they decide only how the rows are allocated, never an answer.
pub const WINDOW_SEG_MIN_BYTES: usize = 64 << 10;
pub const WINDOW_SEG_MAX_BYTES: usize = 4 << 20;

/// hosts ops/window (D3345): a streaming window fill's largest pull (W10's budget doubles from one row up to it), so the
/// rows stored ahead of the object's walk stay bounded, as PG's spool reads only the rows a position needs. Chosen, not
/// measured: a pull costs about 0.25 us (lowerv4 ops/window/lower.rs `exact`), so past the doublings a whole read pays
/// one per 8,192 rows. It decides only how far a fill reads ahead, never an answer.
pub const WINDOW_PULL_MAX_ROWS: u64 = 8192;

/// hosts ops/window (D3584): the size past which the row store keeps a computed by-reference window argument's value as
/// a cell the object evaluates again at each read, instead of a copy per input row (PG's spool keeps only the input row).
/// A chosen bound, not measured: below it a stored copy costs about what one evaluation does; above it a million rows of
/// copies stop being cheap. It decides only where a value is evaluated, never an answer.
pub const WINDOW_DEFER_BYTES: usize = 256;

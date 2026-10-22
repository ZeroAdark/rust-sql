//! The tier-3 group plan's bounds (contract::layout): the params words the lowerer writes and kernels
//! ops/sort/topn/t3.rs reads back (`Plan::from_words`, which refuses a plan past them), and the round-2 table its
//! distinct owner sizes. Each bound is derived from the word layout, MEASURED, or marked CHOSEN.

/// Group keys per plan. The `P_KEYS` word holds a key in two bytes, its lane and kind in the low half and its `Fp128`
/// table lane in the high half, so one word carries 4; a text key takes two record key words, so 4 text keys fill
/// `breaker::MAX_RECORD_KEYS` (8).
pub const MAX_KEYS: usize = 4;

/// Plain slots per plan. The distinct family's accumulator adds a pair count and one representative per text key, so
/// 1 + 3 + `MAX_KEYS` is `breaker::MAX_RECORD_SLOTS` (8).
pub const MAX_SLOTS: usize = 3;

/// Output columns per plan: one byte each in the `P_OUT` and `P_OUTVAL` words, so 8.
pub const MAX_OUT: usize = 8;

/// The most groups an emit keeps (`LIMIT + OFFSET`); the out block holds them, so no push
/// happens before the last row and the emit never suspends. CHOSEN, not measured: it bounds the kept results each
/// owner region and the out block hold whole; a larger top is refused when the plan is read.
pub const MAX_TOPK: usize = 1024;

/// The most verdict quals a plan carries (`P_SLOTS` bytes 4 and 5: `(table lane + 1) | code lane << 5 | negated << 7`).
pub const MAX_QUALS: usize = 2;

/// The round-2 hot table's floor (two slots per estimated group, clamped to [floor, `ROUND2_T1_MAX`]). MEASURED, swept
/// flat on c8g: a held-out dev table of 4,000,000 rows from `generate_series`, 64 to 4096 groups and an expression key
/// PostgreSQL estimates at 200 groups that holds 65,536; same binary, floors 16 KiB, 64 KiB, 1 MiB and 4 MiB against
/// 256 KiB, W0 and W8, arms interleaved: every statement within 0.98..1.02 of 256 KiB, rows identical, so the value is
/// not tuned to a benchmark. Swept on the exec owner's copy (exec table/owner.rs `distinct_open`); the hand arm t3.rs
/// mirrors it.
pub const ROUND2_T1_BYTES: u32 = 256 << 10;

/// The round-2 hot table's ceiling. CHOSEN, not measured: it caps the table a large group estimate asks for (the table
/// is also held to a quarter of the round-2 region).
pub const ROUND2_T1_MAX: u32 = 8 << 20;

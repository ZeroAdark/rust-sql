use crate::host_abi::*;

pub use super::raw::topk::{topk_cut, topk_cut_open, topk_insert, topk_insert_text, topk_open, topk_order, topk_order_text, topk_run, topk_text_word};
pub(crate) use super::raw::topk::code_bytes;

/// The spec words of `topk_insert_text`: `spec1 = site | key col << 8 | key lane << 24` (a text first key's code column, `TEXT_NONE` when the first key is a word), `spec2 = kind | col << 8 | lane << 24 | at << 40` for the second key (kind 0 none, 1 a word at run column `at`, 2 a pair at `at`, 3 the text of code column `col`).
pub const TEXT_NONE: u64 = 0xffff;
/// `spec1` bit 63: FETCH FIRST WITH TIES over the text key; the run's `n` rides bits 40..56 (the emit passes the capacity under ties).
pub const TEXT_TIES: u64 = 1 << 63;
pub const TEXT_N_SHIFT: u64 = 40;

/// The part cut of a text-first top-k (M4-L3T (2), lower.md §15 "a text-first TopK cuts by the part's code order"): a part's dictionary is byte-rank sorted, so once `n` rows of a part are seen every later row of that part whose code lies strictly past the part's n-th smallest can never be among the run's `n` (that claim is the worker's own and no other part changes it). The table follows the run in the partial region: `CUT_SLOTS` direct-mapped entries `[cut, owner, len, min, codes[n]]` keyed by `part & (CUT_SLOTS - 1)`; the owner tag is `part << 32 | 1`, so a zero page owns nothing; a lost entry costs renders, never rows.
pub const CUT_SLOTS: u64 = 1024;
pub const CUT_OFF_CUT: u64 = 0;
pub const CUT_OFF_OWNER: u64 = 8;
pub const CUT_OFF_LEN: u64 = 16;
/// B3-12: the least code the entry has taken since its claim (`topk_cut`'s order check).
pub(crate) const CUT_OFF_LEAST: u64 = 24;
pub const CUT_OFF_CODES: u64 = 32;
/// B3-12: the length word's fields: the codes held (bits 0..16), the run of rests (16..32: how many replacements in a row
/// have taken a new least code, each starting a rest; any other replacement sets it to 0) and the calls left while the
/// entry rests (from `CUT_REST_SHIFT`; nonzero only in a full entry). A rest's length is cost_profile's `cut::rest_calls`
/// of `n` and the run (`cut_rest_calls`; lane catalog row OF-TG2-2: derived from what a rested call saves and risks, cost_profile cut.rs).
pub(crate) const CUT_LEN_MASK: u64 = 0xffff;
pub(crate) const CUT_ROUND_SHIFT: u32 = 16;
pub(crate) const CUT_ROUND_MASK: u64 = 0xffff;
pub const CUT_REST_SHIFT: u32 = 32;
/// NEW-45: `topk_cut`'s `seed` when the claiming part's cut is not known before its rows: the entry starts empty.
pub const CUT_NO_SEED: u64 = u64::MAX;
/// NEW-45: the length word's mark of a seeded entry (in the codes field, past any `n` an entry holds). A part whose
/// dictionary's every code stands for one of the rows the scan delivers (the window's `site::PART_ENTRIES_SAFE`) has its
/// `n` least codes held by at least `n` of those rows, so its claim takes the `n`-th least code (code_cut computes it from
/// the lane's code count, either direction) as the cut at once: the entry holds no codes, takes none (the rows that would
/// be taken are those very rows, not more of them) and rests for good.
pub(crate) const CUT_LEN_SEEDED: u64 = 1 << 15;
/// The rest field's bound: a seeded entry's rest, renewed whenever it runs out.
pub(crate) const CUT_REST_MAX: u64 = u32::MAX as u64;
/// OF-TG2-2: `q` (what a replacing call costs over what a row past the cut costs) per heap depth in 1/65536ths:
/// cost_profile cut.rs `REST_Q16`, the fitted rule's table. The kernels stay inside their boundary (abi, dict, fn), so
/// this is its copy; cut_tests hold the two equal.
pub(crate) const CUT_REST_Q16: [u64; 8] = [12672, 12921, 13194, 16055, 24798, 38605, 46416, 58474];

/// The calls the `round`-th rest of a run lets through at `n` codes: `q (round + 1) / 2`, rounded up, at least one, at
/// most the rest field's 32 bits (cost_profile cut.rs `rest_calls`, which cut_tests hold equal).
pub(crate) const fn cut_rest_calls(n: u64, round: u64) -> u64 {
    let d = if n <= 1 { 0 } else { 63 - n.leading_zeros() as usize };
    let q = CUT_REST_Q16[if d < CUT_REST_Q16.len() { d } else { CUT_REST_Q16.len() - 1 }];
    let r = q.saturating_mul(round.saturating_add(1)).saturating_add((1 << 17) - 1) >> 17;
    if r == 0 {
        1
    } else if r > CUT_REST_MAX {
        CUT_REST_MAX
    } else {
        r
    }
}

pub const fn cut_entry_bytes(n: u32) -> u64 {
    CUT_OFF_CODES + 8 * n as u64
}

pub const fn cut_bytes(n: u32) -> u32 {
    (CUT_SLOTS * cut_entry_bytes(n)) as u32
}

/// The table's offset in the region: the run's bytes rounded up to eight.
pub const fn cut_base(cap: u32, natts: u32) -> u32 {
    (TopK::bytes_cap(cap, natts) + 7) & !7
}

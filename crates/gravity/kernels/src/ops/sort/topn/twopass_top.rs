//! keyed.twopass's pass-1 export owner under the top rule (V4R-192; the ladder's `EXPORT_DUPS`, kernels cb/t3.rs
//! `export_reduce` at ec2df04e1fc): each owner keeps, of its partitions' keys, every key whose records sum to two or more
//! (all of its part records) and its `n` best keys seen once in the top's tie order; lowerv4 keyed/twopass.rs `admitted`
//! then reads only these. The record is `partitioned_by` its image words (a marked record's part word after them), so a
//! key's parts meet in one partition: the sum is the key's whole count, and the global `n` best singles lie among the
//! owners' `n` best. Before this every pass-1 record was kept and `admitted` walked (and sorted) them all serially.
//!
//! Region: `[OwnerTop][run directory][the singles' heap: n records][the kept run ...][table]`; run 0 the kept keys' records
//! (grown in place across the partitions), run 1 the heap's (at `end_round`). `OwnerTop.resume` the heap's length. Under an
//! extreme top the heap holds the owner's n best key extremes and run 1 is empty: each of them is in the kept run.
use core::mem::size_of;

use gravity_abi::breaker::{OwnerFns, RecordLayout, ORDER_NONE, OWNER_PARAMS};

use super::raw::twopass_top::{owner_end, owner_min, owner_open, owner_reduce};

/// The export: the record (`partitioned_by` its image words), its count slot, the image's exported words (1 or 2) and the
/// top's `n` (LIMIT + OFFSET) with its tie order on the key among the keys seen once (lowerv4 twopass.rs `Single`). An
/// extreme top (`ext`): the slots after the count are the key's extreme and, with `ext_nn`, its non-NULL rows; `desc` /
/// `nulls_first` are then the extreme's order, and each owner keeps every key whose best possible group ranks at its own
/// n-th best key extreme or before (lowerv4 raw.rs `ext_admitted` then ranks them against the global n-th).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TopExportSpec {
    pub layout: RecordLayout,
    pub count_slot: u8,
    pub img_words: u8,
    pub n: u32,
    pub desc: bool,
    pub nulls_first: bool,
    pub nullable: bool,
    pub ext: bool,
    pub ext_nn: bool,
    /// gtopn GT-1: the slot after the extreme (and its non-NULL rows) is the key's best, apart from its witness extreme.
    pub spread: bool,
}

pub(super) const P_HEAD: usize = 0;
pub(super) const P_N: usize = 1;
pub(super) const P_LAYOUT: usize = 2;
pub(super) const LAYOUT_WORDS: usize = size_of::<RecordLayout>() / 8;
const _: () = assert!(P_LAYOUT + LAYOUT_WORDS <= OWNER_PARAMS, "the spec fits the owner's words");

pub fn owner_fns(spec: &TopExportSpec) -> OwnerFns {
    let mut p = [0u64; OWNER_PARAMS];
    let flags = spec.desc as u64 | (spec.nulls_first as u64) << 1 | (spec.nullable as u64) << 2 | (spec.ext as u64) << 3 | (spec.ext_nn as u64) << 4 | (spec.spread as u64) << 5;
    p[P_HEAD] = spec.count_slot as u64 | (spec.img_words as u64) << 8 | flags << 16;
    p[P_N] = spec.n as u64;
    let words: [u64; LAYOUT_WORDS] = spec.layout.words();
    p[P_LAYOUT..P_LAYOUT + LAYOUT_WORDS].copy_from_slice(&words);
    OwnerFns { params: p, rounds: 1, order_slot: ORDER_NONE, order_slot2: ORDER_NONE, min_bytes: owner_min, open: owner_open, reduce: owner_reduce, end_round: owner_end, reduce2: owner_reduce, fold: Some(crate::table::owner_fold::owner_fold) }
}

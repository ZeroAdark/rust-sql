//! One-round plain distinct total. Hooks / levers: H12 R3-L06. Budget 160 lines (files.tsv, +25 % gate). Owner
//! M1-TABLES.
//!
//! `count(DISTINCT x)` with no group, whatever the argument's type (an integer word or a text's image): the argument's
//! image is the whole pair record, slot-less, keyed by every word. Each owner partition dedups its pairs and counts
//! them into `OwnerTop.total` (hosts distinct_fold.rs, the plain form); the emit gathers ONE accumulator record
//! `[sentinel = 1, pairs = the owners' totals summed]` through `pairs_total` where a group emit gathers through
//! `agg_gather`, and reads it like any group's (`ACC_PAIRS`).
//!
//! Registers: road `distinct.total` (`DISTINCT_TOTAL`).
use gravity_abi::breaker::{KeyKind, OwnerFns, RecordLayout, LAYOUT_IMPLICIT_COUNT, ORDER_NONE};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, HostFn, Type};
use gravity_program::{OwnerDesc, MAX_REPS, REP_NONE};


crate::strategies! {
    /// One-round plain distinct total.
    DISTINCT_TOTAL = 41, "distinct.total", DistinctPlain;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(DISTINCT_TOTAL, &())], ..crate::ops::NONE };

/// The plain distinct owner over `pair` (`coderep::image_layout` of the argument, no slots but the implicit sentinel): the
/// distinct form with no group words, so each owner's deduped length is its `total`. Pairs route by every word.
/// V4R-26 item 3: a one-word pair (an integer / date / timestamp image: `[x, n]`, 16 bytes) is owned by the insert-count
/// (host_abi v4 m1 `distinct_count_fns`: the key's `KeyHash::word` in 8-byte slots) through `ctor_desc`.
pub(crate) fn owner_desc(pair: RecordLayout) -> OwnerDesc {
    debug_assert!(pair.nslots() == 1 && pair.flags & LAYOUT_IMPLICIT_COUNT != 0, "the plain distinct pair is slot-less (riders are the pair form's)");
    let words = pair.nkeys() as u8;
    let d = OwnerDesc::distinct(pair, OwnerDesc::accumulator_of(&pair, words), words, 0, ORDER_NONE, 0, [(REP_NONE, 0); MAX_REPS]);
    let one_word = words == 1 && pair.size() == 16 && pair.sentinel_off() == 8 && !pair.has_validity() && pair.key_kinds()[0] != KeyKind::Fp128;
    if one_word { OwnerDesc { ctor_desc: Some(count_owner), ..d } } else { d }
}

fn count_owner(_d: &OwnerDesc, _words: &[u64]) -> OwnerFns {
    gravity_kernels::ops::group::entries_distinct::distinct_count_fns()
}

/// The plain form's gather, called `(ctx, desc) -> state` exactly where the group emit calls `agg_gather`: a fresh call
/// files the one accumulator record of `desc`'s layout (`owner_desc(..).acc`) behind an `EmitState`, a resumed call
/// returns it, null when the consumed region is not a finished owner phase (the emit raises it, as for `agg_gather`).
pub(crate) fn gather(b: &mut FunctionBuilder<'_>) -> HostFn {
    let addr = gravity_abi::host_words!(gravity_kernels::host_abi::pairs_total(ctx, desc));
    b.declare_host_once("pairs_total", &[Type::Ptr, Type::Ptr], Some(Type::Ptr), addr, HostFlags { touches_memory: true, ..HostFlags::default() })
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::{KeyKind, RecordLayout, ACC_PAIRS};

    #[test]
    fn the_plain_owner_counts_pairs_of_every_word_into_a_keyless_accumulator() {
        let pair = RecordLayout::with_sentinel(&[KeyKind::I64, KeyKind::I64], &[], false, None);
        let d = super::owner_desc(pair);
        assert_eq!((d.pair, d.acc.nkeys()), (2, 0));
        assert!((ACC_PAIRS as usize) < d.acc.nslots());
        assert!(d.ctor_desc.is_none(), "a two-word pair keeps the generic plain owner");
    }

    /// V4R-26 item 3: a one-word pair is owned by the insert-count (`distinct_count_fns`), an Fp128 image is not.
    #[test]
    fn a_one_word_pair_is_owned_by_the_insert_count() {
        let d = super::owner_desc(RecordLayout::with_sentinel(&[KeyKind::I64], &[], false, None));
        assert!(d.ctor_desc.is_some() && d.pair == 1);
        let fns = (d.ctor_desc.unwrap())(&d, &[]);
        // by behaviour, not by fn-pointer identity (unpredictable_function_pointer_comparisons: box 42's fast-profile, 16
        // codegen units, gave the one fn two addresses)
        let want = gravity_kernels::ops::group::entries_distinct::distinct_count_fns();
        assert!(fns.rounds == 1 && fns.fold.is_some() && (fns.min_bytes)(core::ptr::null(), 1) == (want.min_bytes)(core::ptr::null(), 1), "the insert-count's one round and floor");
        assert!(super::owner_desc(RecordLayout::with_sentinel(&[KeyKind::Fp128], &[], false, None)).ctor_desc.is_none());
    }
}

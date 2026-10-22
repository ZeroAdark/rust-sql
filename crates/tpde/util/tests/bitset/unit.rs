// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::SmallBitSet;
use mcx::MemoryContext;

#[test]
fn empty_inline_forms_and_no_drop() {
    let context = MemoryContext::new_bump("bitset");
    for bits in [
        SmallBitSet::<0>::new(context.mcx()).bit_size(),
        SmallBitSet::<1>::new(context.mcx()).bit_size(),
        SmallBitSet::<4>::new(context.mcx()).bit_size(),
    ] {
        assert_eq!(bits, 0);
    }
    assert!(!core::mem::needs_drop::<SmallBitSet<'_, 4>>());
    assert!(core::mem::size_of::<SmallBitSet<'_, 4>>() <= 128);
}

#[test]
fn push_all_word_boundaries_and_overwrite_padding() {
    let context = MemoryContext::new_bump("bitset");
    let mut bits = SmallBitSet::<1>::new(context.mcx());
    for i in 0..513 {
        bits.push_back(i % 3 == 0).unwrap();
        assert_eq!(bits.bit_size(), i + 1);
        assert_eq!(bits.is_set(i).unwrap(), i % 3 == 0);
    }
    for i in 0..513 {
        assert_eq!(bits.is_set(i).unwrap(), i % 3 == 0);
    }
    bits.resize(1).unwrap();
    bits.mark_set(1).unwrap();
    bits.push_back(false).unwrap();
    assert!(!bits.is_set(1).unwrap());
    bits.push_back(true).unwrap();
    assert!(bits.is_set(2).unwrap());
}

#[test]
fn aligned_growth_and_shrink_keep_upstream_masking() {
    let context = MemoryContext::new_bump("bitset");
    let mut bits = SmallBitSet::<1>::new(context.mcx());
    bits.resize(64).unwrap();
    bits.words_mut()[0] = u64::MAX;
    bits.resize(65).unwrap();
    assert_eq!(bits.words(), [0, 0]);
    bits.words_mut().fill(u64::MAX);
    bits.resize(1).unwrap();
    assert_eq!(bits.words(), [u64::MAX]);
    assert!(bits.is_set(63).unwrap());
    bits.resize(2).unwrap();
    assert_eq!(bits.words(), [1]);
    bits.resize(0).unwrap();
    bits.resize(65).unwrap();
    assert_eq!(bits.words(), [0, 0]);
}

#[test]
fn first_set_keeps_upstream_word_index_addition() {
    let context = MemoryContext::new_bump("bitset");
    let mut bits = SmallBitSet::<4>::new(context.mcx());
    bits.resize(257).unwrap();
    assert_eq!(bits.first_set(), None);
    for word in 0..5 {
        for offset in [0, 1, 31, 63] {
            bits.zero();
            bits.mark_set(word * 64 + offset).unwrap();
            assert_eq!(bits.first_set(), Some(word + offset));
            bits.mark_unset(word * 64 + offset).unwrap();
            assert_eq!(bits.first_set(), None);
        }
    }
}

fn moved_source<const WORDS: usize>(size: u32) {
    let context = MemoryContext::new_bump("bitset");
    let mut source = SmallBitSet::<WORDS>::new(context.mcx());
    source.resize(size).unwrap();
    source.mark_set(size - 1).unwrap();
    let moved = SmallBitSet::move_from(&mut source);
    assert_eq!(moved.bit_size(), size);
    assert!(moved.is_set(size - 1).unwrap());
    assert_eq!(source.bit_size(), size);
    assert_eq!(source.words(), []);
    assert_eq!(source.first_set(), None);
    source.zero();
    assert!(source.resize(size + 1).is_err());
    source.resize(size).unwrap();
    assert_eq!(source.bit_size(), size);
    assert!(source.words().iter().all(|v| *v == 0));
    source.clear();
    source.push_back(true).unwrap();
    assert!(source.is_set(0).unwrap());
}

#[test]
fn move_constructor_preserves_source_length_inline_and_spilled() {
    moved_source::<0>(1);
    moved_source::<1>(1);
    moved_source::<1>(129);
    moved_source::<4>(256);
    moved_source::<4>(257);
}

#[test]
fn move_assignment_clears_destination_and_leaves_source() {
    let context = MemoryContext::new_bump("bitset");
    for size in [0, 1, 64, 65, 256, 257] {
        let mut source = SmallBitSet::<1>::new(context.mcx());
        source.resize(size).unwrap();
        source.words_mut().fill(0xff);
        let mut dest = SmallBitSet::<1>::new(context.mcx());
        dest.resize(257).unwrap();
        dest.move_assign(&source);
        assert_eq!(dest.bit_size(), size);
        assert!(dest.words().is_empty());
        assert_eq!(source.bit_size(), size);
        assert!(source.words().iter().all(|v| *v == 0xff));
        dest.resize(size).unwrap();
        assert!(dest.words().iter().all(|v| *v == 0));
        source = source;
        assert_eq!(source.bit_size(), size);
        assert!(source.words().iter().all(|v| *v == 0xff));
    }
}

#[test]
fn moved_from_boundary_false_push_has_defined_partial_backing() {
    let context = MemoryContext::new_bump("bitset");
    let mut source = SmallBitSet::<1>::new(context.mcx());
    source.resize(64).unwrap();
    let _moved = SmallBitSet::move_from(&mut source);
    assert!(source.push_back(true).is_err());
    assert!(source.words().is_empty());
    source.push_back(false).unwrap();
    assert_eq!(source.bit_size(), 65);
    assert_eq!(source.words(), [0]);
    assert!(source.is_set(63).is_ok());
    assert!(source.is_set(64).is_err());
    source.resize(66).unwrap();
    assert_eq!(source.words(), [0, 0]);
}

#[test]
fn invalid_indices_do_not_change_storage() {
    let context = MemoryContext::new_bump("bitset");
    let mut bits = SmallBitSet::<1>::new(context.mcx());
    assert!(bits.is_set(0).is_err());
    assert!(bits.mark_set(0).is_err());
    assert!(bits.mark_unset(0).is_err());
    bits.resize(1).unwrap();
    bits.mark_set(63).unwrap();
    for index in [64, 65, u32::MAX] {
        assert!(bits.is_set(index).is_err());
        assert!(bits.mark_set(index).is_err());
        assert!(bits.mark_unset(index).is_err());
    }
    assert_eq!(bits.words(), [1 << 63]);
    assert_eq!(bits.bit_size(), 1);
}

#[test]
fn resize_rounding_uses_unsigned_wrap_at_u32_limit() {
    let context = MemoryContext::new_bump("bitset");
    let mut bits = SmallBitSet::<1>::new(context.mcx());
    for size in u32::MAX - 62..=u32::MAX {
        bits.clear();
        bits.resize(64).unwrap();
        bits.words_mut()[0] = u64::MAX;
        bits.resize(size).unwrap();
        assert_eq!(bits.bit_size(), size);
        assert!(bits.words().is_empty());
        assert_eq!(bits.first_set(), None);
        bits.resize(65).unwrap();
        assert_eq!(bits.words(), [0, 0]);
    }
}

#[test]
fn budget_failure_precedes_masking_and_allows_reuse() {
    let context = MemoryContext::new("bitset").with_limit(8192);
    let mut bits = SmallBitSet::<1>::new(context.mcx());
    bits.resize(64).unwrap();
    bits.words_mut()[0] = u64::MAX;
    assert!(bits.resize(1_000_000).is_err());
    assert_eq!(bits.bit_size(), 64);
    assert_eq!(bits.words(), [u64::MAX]);
    bits.push_back(true).unwrap();
    assert_eq!(bits.words(), [u64::MAX, 1]);
    bits.clear();
    bits.resize(65).unwrap();
    assert_eq!(bits.words(), [0, 0]);
}

#[test]
fn clear_and_reuse_retains_budgeted_storage() {
    let context = MemoryContext::new("bitset").with_limit(8192);
    let mut bits = SmallBitSet::<0>::new(context.mcx());
    bits.resize(4096).unwrap();
    let ptr = bits.words().as_ptr();
    for _ in 0..32 {
        bits.clear();
        bits.resize(4096).unwrap();
        assert_eq!(bits.words().as_ptr(), ptr);
        assert!(bits.words().iter().all(|v| *v == 0));
    }
}

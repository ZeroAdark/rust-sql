// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use super::*;
use mcx::MemoryContext;
use std::vec;

#[test]
fn empty_operations_have_distinct_offsets() {
    let context = MemoryContext::new_bump("tpde-test");
    let mut table = StringTable::new(context.mcx());
    assert_eq!(table.size(), 1);
    assert_eq!(table.data(), &[0]);
    assert_eq!(table.add(b"").unwrap(), 0);
    assert_eq!(table.data(), &[0]);
    assert_eq!(table.add_prefix(b"", b"").unwrap(), 1);
    assert_eq!(table.data(), &[0, 0]);
    assert_eq!(table.add(b"").unwrap(), 0);
}

#[test]
fn repeated_binary_strings_are_not_deduplicated() {
    let context = MemoryContext::new_bump("tpde-test");
    let mut table = StringTable::new(context.mcx());
    assert_eq!(table.add(b"a\0\xff").unwrap(), 1);
    assert_eq!(table.add(b"a\0\xff").unwrap(), 5);
    assert_eq!(table.add_prefix(b"\xfe\0", b"b\0").unwrap(), 9);
    assert_eq!(table.data(), b"\0a\0\xff\0a\0\xff\0\xfe\0b\0\0");
}

#[test]
fn inline_boundary_and_growth_match_upstream_capacity() {
    let context = MemoryContext::new("tpde-test");
    let mut table = StringTable::new(context.mcx());
    table.add(&[7; 22]).unwrap();
    assert_eq!(table.size(), 24);
    assert_eq!(context.used(), 0);
    table.add_prefix(b"", b"").unwrap();
    assert_eq!(table.size(), 25);
    assert_eq!(table.spill.capacity(), 49);
    assert_eq!(context.used(), 49);
    table.add(&[8; 23]).unwrap();
    assert_eq!(table.size(), 49);
    assert_eq!(table.spill.capacity(), 49);
    table.add(b"z").unwrap();
    assert_eq!(table.spill.capacity(), 99);
    assert_eq!(table.data()[1..23], [7; 22]);
}

#[test]
fn large_first_append_reserves_the_requested_size() {
    let context = MemoryContext::new_bump("tpde-test");
    let mut table = StringTable::new(context.mcx());
    let bytes = vec![0xa5; 1000];
    assert_eq!(table.add_prefix(b"prefix", &bytes).unwrap(), 1);
    assert_eq!(table.spill.capacity(), 1008);
    assert_eq!(&table.data()[1..7], b"prefix");
    assert_eq!(&table.data()[7..1007], &bytes);
    assert_eq!(table.data()[1007], 0);
}

#[test]
fn failed_inline_growth_is_atomic_and_reusable() {
    let context = MemoryContext::new("tpde-limit").with_limit(48);
    let mut table = StringTable::new(context.mcx());
    table.add(b"kept").unwrap();
    let before = table.data().to_vec();
    assert!(table.add_prefix(b"prefix", &[1; 24]).is_err());
    assert_eq!(table.data(), before);
    assert_eq!(table.spill.capacity(), 0);
    assert_eq!(context.used(), 0);
    assert_eq!(table.add(b"ok").unwrap(), before.len());
}

#[test]
fn failed_spill_growth_is_atomic_and_reusable() {
    let context = MemoryContext::new("tpde-limit").with_limit(49);
    let mut table = StringTable::new(context.mcx());
    table.add(&[3; 24]).unwrap();
    let before = table.data().to_vec();
    assert!(table.add_prefix(b"prefix", &[9; 24]).is_err());
    assert_eq!(table.data(), before);
    assert_eq!(table.spill.capacity(), 49);
    assert_eq!(context.used(), 49);
    assert_eq!(table.add(b"ok").unwrap(), before.len());
}

#[test]
fn moves_preserve_inline_and_spilled_storage() {
    fn moved(table: StringTable<'_>) -> StringTable<'_> {
        table
    }
    let context = MemoryContext::new_bump("tpde-test");
    let mut table = StringTable::new(context.mcx());
    table.add(b"in").unwrap();
    let mut table = moved(table);
    assert_eq!(table.data(), b"\0in\0");
    table.add(&[2; 100]).unwrap();
    let expected = table.data().to_vec();
    assert_eq!(moved(table).data(), expected);
}

#[test]
fn checked_lengths_reject_every_overflow_position() {
    assert_eq!(appended_size(usize::MAX, 0, 0), None);
    assert_eq!(appended_size(1, usize::MAX, 0), None);
    assert_eq!(appended_size(1, 0, usize::MAX), None);
    assert_eq!(appended_size(1, 2, 3), Some(7));
}

#[test]
fn append_sequences_match_an_independent_byte_model() {
    let context = MemoryContext::new_bump("tpde-test");
    let mut table = StringTable::new(context.mcx());
    let mut expected = vec![0];
    for n in 0..256usize {
        let bytes: std::vec::Vec<u8> = (0..n % 65).map(|i| (n + i) as u8).collect();
        let prefix = [n as u8, 0];
        let offset = expected.len();
        if n % 2 == 0 {
            assert_eq!(table.add_prefix(&prefix, &bytes).unwrap(), offset);
            expected.extend_from_slice(&prefix);
            expected.extend_from_slice(&bytes);
            expected.push(0);
        } else if bytes.is_empty() {
            assert_eq!(table.add(&bytes).unwrap(), 0);
        } else {
            assert_eq!(table.add(&bytes).unwrap(), offset);
            expected.extend_from_slice(&bytes);
            expected.push(0);
        }
        assert_eq!(table.size(), expected.len());
        assert_eq!(table.data(), expected);
    }
}

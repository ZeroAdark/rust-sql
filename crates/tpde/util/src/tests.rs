// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{misc::*, *};
use mcx::MemoryContext;
use std::vec::Vec;

#[test]
fn checked_matches_the_build_stage() {
    assert_eq!(
        CHECKED,
        cfg!(debug_assertions) || cfg!(feature = "checked") || cfg!(tpde_address_sanitizer)
    );
    let panicked = std::panic::catch_unwind(|| tpde_assert!(false, "contract")).is_err();
    assert_eq!(panicked, CHECKED);
    assert_eq!(std::panic::catch_unwind(|| tpde_assert_eq!(1, 2)).is_err(), CHECKED);
    assert_eq!(std::panic::catch_unwind(|| tpde_assert_ne!(1, 1)).is_err(), CHECKED);
    tpde_assert!(true);
    tpde_assert_eq!(1, 1);
    tpde_assert_ne!(1, 2);
}
#[cfg(tpde_address_sanitizer)]
#[test]
fn sanitizer_builds_keep_every_check() {
    assert!(CHECKED, "address-sanitizer stage must keep TPDE_ASSERT checks");
}
#[test]
fn alignment_and_overflow() {
    assert_eq!(align_down(19u64, 16).unwrap(), 16);
    assert_eq!(align_up(19u64, 16).unwrap(), 32);
    assert_eq!(align_down(-17i64, 16).unwrap(), -32);
    assert_eq!(align_up(-17i64, 16).unwrap(), -16);
    assert!(align_up(u64::MAX, 2).is_err());
    assert!(align_up(i64::MAX, 2).is_err());
    assert!(align_down(4u8, 0).is_err());
    assert!(align_up(4u16, 3).is_err());
    assert!(align_up(4i8, -128).is_err());
}
#[test]
fn bit_counts_preserve_widths() {
    for i in 0..64 {
        assert_eq!(cnt_tz(1u64 << i).unwrap(), i);
        assert_eq!(cnt_lz(1u64 << i).unwrap(), 63 - i);
    }
    assert_eq!(cnt_lz(1u8).unwrap(), 7);
    assert_eq!(cnt_lz(1u16).unwrap(), 15);
    assert_eq!(cnt_lz(1u32).unwrap(), 31);
    assert!(cnt_lz(0u8).is_err());
    assert!(cnt_tz(0u64).is_err());
}
#[test]
fn extensions_and_invalid_widths() {
    for bits in 1..64 {
        assert_eq!(zext(u64::MAX, bits).unwrap(), (1u64 << bits) - 1);
        assert_eq!(sext(u64::MAX, bits).unwrap(), -1);
        assert_eq!(
            sext(1u64 << (bits - 1), bits).unwrap(),
            -(1i64 << (bits - 1))
        );
    }
    for bits in [0, 64, 65, u32::MAX] {
        assert!(zext(1, bits).is_err());
        assert!(sext(1, bits).is_err());
    }
}
#[test]
fn leb_known_bytes_and_canaries() {
    let mut bytes = [0xa5; 12];
    assert_eq!(uleb_write(&mut bytes[1..11], 624485).unwrap(), 3);
    assert_eq!(&bytes[1..4], &[0xe5, 0x8e, 0x26]);
    assert_eq!(bytes[0], 0xa5);
    assert_eq!(bytes[4], 0xa5);
    assert_eq!(sleb_write(&mut bytes[1..11], -624485).unwrap(), 3);
    assert_eq!(&bytes[1..4], &[0x9b, 0xf1, 0x59]);
    for v in [0, 1, 127, 128, u64::MAX] {
        let n = uleb_write(&mut bytes, v).unwrap();
        assert_eq!(n, uleb_len(v));
    }
    for v in [i64::MIN, -65, -64, -1, 0, 63, 64, i64::MAX] {
        let n = sleb_write(&mut bytes, v).unwrap();
        assert_eq!(n, sleb_len(v));
    }
    let mut short = [0xcc; 9];
    assert!(uleb_write(&mut short, u64::MAX).is_err());
    assert_eq!(short, [0xcc; 9]);
    assert!(sleb_write(&mut short, i64::MIN).is_err());
    assert_eq!(short, [0xcc; 9]);
}
#[test]
fn bit_iteration_both_directions() {
    let set = 0x8000_0000_0000_0089;
    assert_eq!(
        BitSetIterator::<false>::new(set).collect::<Vec<_>>(),
        [0, 3, 7, 63]
    );
    assert_eq!(
        BitSetIterator::<true>::new(set).collect::<Vec<_>>(),
        [63, 7, 3, 0]
    );
    let mut iter = BitSetIterator::<true>::new(set).begin();
    assert_eq!(iter.len(), 4);
    assert_eq!(iter.current().unwrap(), 63);
    iter.advance().unwrap();
    assert_eq!(iter.current().unwrap(), 7);
    let mut end = iter.end();
    assert!(end.current().is_err());
    assert!(end.advance().is_err());
    assert_eq!(end.next(), None);
    assert_eq!(BitSetIterator::<false>::new(u64::MAX).count(), 64);
}
#[test]
fn writer_growth_and_flush() {
    let context = MemoryContext::new_bump("writer");
    let mut v = SmallVec::<u8, 8>::new(context.mcx());
    {
        let mut w = VectorWriter::new(&mut v);
        assert_eq!(w.capacity(), 0);
        w.reserve(1).unwrap();
        assert_eq!(w.capacity(), 8);
        assert_eq!(w.size(), 0);
        w.write(b"abcdefgh").unwrap();
        w.write(b"i").unwrap();
        assert_eq!(w.capacity(), 17);
        assert_eq!(w.data().unwrap(), b"abcdefghi");
        w.flush();
        assert_eq!(w.capacity(), 9);
        w.reserve(1).unwrap();
        assert_eq!(w.capacity(), 17);
    }
    assert_eq!(&*v, b"abcdefghi");
    assert_eq!(v.capacity(), 17);
}
#[test]
fn writer_offset_rewind_and_overwrite() {
    let context = MemoryContext::new_bump("writer");
    let mut v = SmallVec::<u8, 8>::new(context.mcx());
    v.extend_from_slice(b"abcdefgh").unwrap();
    {
        let mut w = VectorWriter::at(&mut v, 2).unwrap();
        w.write(b"XY").unwrap();
        w.skip_unchecked(2).unwrap();
        assert_eq!(&w.data().unwrap()[..w.size()], b"abXYef");
        w.unskip(3).unwrap();
        w.write(b"12345").unwrap();
    }
    assert_eq!(&*v, b"abX12345");
}
#[test]
fn writer_skip_zero_and_initialized_access() {
    let context = MemoryContext::new_bump("writer");
    let mut v = SmallVec::<u8, 8>::new(context.mcx());
    {
        let mut w = VectorWriter::new(&mut v);
        w.reserve(32).unwrap();
        assert!(w.data().unwrap().is_empty());
        w.skip(4).unwrap();
        assert_eq!(w.data().unwrap(), [0; 4]);
        w.data_mut().unwrap().fill(7);
        w.unskip(2).unwrap();
        w.zero(5).unwrap();
        assert_eq!(w.data().unwrap(), [7, 7, 0, 0, 0, 0, 0]);
    }
    assert_eq!(v.len(), 7);
}
#[test]
fn writer_move_and_assignment_flush_old_owner() {
    let context = MemoryContext::new_bump("writer");
    let mut a = SmallVec::<u8, 8>::new(context.mcx());
    let mut b = SmallVec::<u8, 8>::new(context.mcx());
    {
        let mut w = VectorWriter::new(&mut a);
        w.write(b"abc").unwrap();
        w.unskip(1).unwrap();
        let mut other = VectorWriter::new(&mut b);
        other.write(b"def").unwrap();
        w = other;
        w.write(b"g").unwrap();
        let mut moved = core::mem::take(&mut w);
        assert!(!w.is_bound());
        assert!(w.write(b"bad").is_err());
        w.flush();
        moved.unskip(1).unwrap();
    }
    assert_eq!(&*a, b"ab");
    assert_eq!(&*b, b"def");
}
#[test]
fn writer_flushes_during_unwind() {
    let context = MemoryContext::new_bump("writer");
    let mut v = SmallVec::<u8, 8>::new(context.mcx());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut w = VectorWriter::new(&mut v);
        w.write(b"abc").unwrap();
        w.unskip(1).unwrap();
        panic!("test unwind");
    }));
    assert!(result.is_err());
    assert_eq!(&*v, b"ab");
}
#[test]
fn writer_integral_forms_and_lebs() {
    let context = MemoryContext::new_bump("writer");
    let mut v = SmallVec::<u8, 8>::new(context.mcx());
    {
        let mut w = VectorWriter::new(&mut v);
        w.write_int(true).unwrap();
        w.write_int(0x1234u16).unwrap();
        w.write_int(-2i32).unwrap();
        w.write_uleb(624485).unwrap();
        w.write_sleb(-624485).unwrap();
        w.reserve(20).unwrap();
        w.write_uleb_unchecked(u64::MAX).unwrap();
        w.write_sleb_unchecked(i64::MIN).unwrap();
    }
    assert_eq!(&v[..7], &[1, 0x34, 0x12, 0xfe, 0xff, 0xff, 0xff]);
    assert_eq!(v.len(), 33);
}
#[test]
fn writer_invalid_operations_are_atomic() {
    let context = MemoryContext::new_bump("writer");
    let mut v = SmallVec::<u8, 8>::new(context.mcx());
    assert!(VectorWriter::at(&mut v, 1).is_err());
    let mut w = VectorWriter::new(&mut v);
    w.write(b"ok").unwrap();
    assert!(w.write_unchecked(&[0; 20]).is_err());
    assert!(w.skip_unchecked(20).is_err());
    assert!(w.zero_unchecked(20).is_err());
    assert!(w.unskip(3).is_err());
    assert!(w.reserve(usize::MAX).is_err());
    assert_eq!(w.size(), 2);
    assert_eq!(w.data().unwrap(), b"ok");
    w.flush();
    assert!(w.write_sleb_unchecked(0).is_err());
    assert!(w.write_int_unchecked(1u64).is_err());
    w.write(b"!").unwrap();
    assert_eq!(w.data().unwrap(), b"ok!");
}
#[test]
fn writer_budget_failure_and_fitting_reuse() {
    let context = MemoryContext::new("writer").with_limit(8192);
    let mut v = SmallVec::<u8, 8>::new(context.mcx());
    let mut w = VectorWriter::new(&mut v);
    w.write(b"ok").unwrap();
    assert!(w.reserve(9000).is_err());
    assert_eq!(w.capacity(), 8);
    assert_eq!(w.data().unwrap(), b"ok");
    w.write(b"!").unwrap();
    assert_eq!(w.size(), 3);
}
#[test]
fn layout_and_no_drop_storage() {
    assert!(!core::mem::needs_drop::<SmallVec<'_, u8, 8>>());
    assert!(core::mem::needs_drop::<VectorWriter<'_, '_, 8>>());
    assert!(core::mem::size_of::<VectorWriter<'_, '_, 8>>() <= 128);
    assert_eq!(dwarf::a64::DW_reg_fp, dwarf::a64::DW_reg_x29);
    assert_eq!(dwarf::a64::DW_reg_lr, dwarf::a64::DW_reg_x30);
}

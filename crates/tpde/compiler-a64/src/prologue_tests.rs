// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use super::*;
use std::vec::Vec;

#[test]
fn frame_rounding_preserves_small_immediates_and_large_shifted_immediates() {
    for (size, expected) in [
        (0, 0),
        (1, 16),
        (16, 16),
        (17, 32),
        (496, 496),
        (497, 512),
        (4080, 4080),
        (4081, 4096),
        (4095, 4096),
        (4096, 4096),
        (4097, 8192),
        (0x00ff_f000, 0x00ff_f000),
    ] {
        let frame = final_frame_size(size).unwrap();
        assert_eq!(frame, expected);
        assert_ne!(de64_SUBxi(DA_SP, DA_SP, i64::from(frame)), 0);
        assert_ne!(de64_ADDxi(DA_SP, DA_SP, i64::from(frame)), 0);
    }
}

#[test]
fn frame_limits_refuse_rounding_overflow_and_unencodable_frames() {
    for size in [0x00ff_f001, 0x0100_0000, u32::MAX - 15, u32::MAX] {
        assert!(final_frame_size(size).is_err(), "accepted {size:x}");
    }
}

#[test]
fn save_pairing_never_crosses_register_banks() {
    let mask = (1 << 19) | (1 << 20) | (1 << 28) | (1 << 40) | (1 << 47);
    assert_eq!(
        SaveSlots::new(mask).collect::<Vec<_>>(),
        [
            SaveSlot {
                first: 19,
                second: Some(20),
                offset: 16
            },
            SaveSlot {
                first: 28,
                second: None,
                offset: 32
            },
            SaveSlot {
                first: 40,
                second: Some(47),
                offset: 40
            },
        ]
    );
    assert_eq!(SaveSlots::new(0).count(), 0);
}

#[test]
fn save_slots_cover_every_selected_register_once_without_padding() {
    let registers = [19u8, 20, 21, 28, 40, 41, 47];
    for selection in 0..(1 << registers.len()) {
        let mask = registers.iter().enumerate().fold(0u64, |mask, (bit, reg)| {
            mask | if selection & (1 << bit) != 0 {
                1u64 << reg
            } else {
                0
            }
        });
        let mut expected_offset = 16;
        let mut visited = 0u64;
        for slot in SaveSlots::new(mask) {
            assert_eq!(slot.offset, expected_offset);
            assert_eq!(visited & (1 << slot.first), 0);
            visited |= 1 << slot.first;
            expected_offset += 8;
            if let Some(second) = slot.second {
                assert_eq!(slot.first / 32, second / 32);
                assert!(slot.first < second);
                assert_eq!(visited & (1 << second), 0);
                visited |= 1 << second;
                expected_offset += 8;
            }
        }
        assert_eq!(visited, mask);
        assert_eq!(expected_offset, 16 + mask.count_ones() * 8);
    }
}

#[test]
fn paired_and_single_saves_restore_identical_locations() {
    let registers = [19u8, 20, 21, 28, 40, 41, 47];
    for selection in 1..(1 << registers.len()) {
        let mask = registers.iter().enumerate().fold(0u64, |mask, (bit, reg)| {
            mask | if selection & (1 << bit) != 0 {
                1u64 << reg
            } else {
                0
            }
        });
        for base in [DA_SP, GRegSp::new(29).unwrap()] {
            for slot in SaveSlots::new(mask) {
                let store = save_word(slot, base, false).unwrap();
                let load = save_word(slot, base, true).unwrap();
                assert_eq!(store ^ load, 1 << 22);
                assert_eq!((store >> 5) & 31, u32::from(base.value()));
                assert_eq!(store & 31, u32::from(slot.first % 32));
                assert_eq!((store >> 26) & 1, u32::from(slot.first >= 32));
                if let Some(second) = slot.second {
                    assert_eq!((store >> 10) & 31, u32::from(second % 32));
                    assert_eq!((store >> 15) & 0x7f, slot.offset / 8);
                } else {
                    assert_eq!((store >> 10) & 0xfff, slot.offset / 8);
                }
            }
        }
    }
}

#[test]
fn register_argument_counts_follow_last_assigned_register() {
    assert_eq!(argument_count(0), 0);
    for register in 0..8 {
        assert_eq!(argument_count(1 << register), register + 1);
        assert_eq!(argument_count((1 << (register + 1)) - 1), register + 1);
    }
    assert_eq!(argument_count(0x55), 7);
    assert_eq!(argument_count(0xaa), 8);
    assert_eq!(argument_count(0x100), 0);
}

#[test]
fn frame_instruction_refusal_rejects_invalid_save_offsets() {
    assert!(checked_word(0).is_err());
    assert_eq!(checked_word(0xd65f_03c0).unwrap(), 0xd65f_03c0);
    assert!(save_word(
        SaveSlot {
            first: 19,
            second: Some(20),
            offset: 512
        },
        DA_SP,
        false
    )
    .is_err());
    assert!(save_word(
        SaveSlot {
            first: 40,
            second: None,
            offset: 32768
        },
        DA_SP,
        true
    )
    .is_err());
}

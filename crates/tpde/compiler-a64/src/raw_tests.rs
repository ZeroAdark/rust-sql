// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use super::*;

#[test]
fn assembler_register_classes_preserve_sp_vector_and_invalid_boundaries() {
    for id in 0..=255 {
        let reg = Reg::new(id);
        assert_eq!(gp(reg).is_ok(), id < 32);
        assert_eq!(zr(reg).is_ok(), id < 31);
        assert_eq!(sp(reg).is_ok(), id < 32);
        assert_eq!(vector(reg).is_ok(), (32..64).contains(&id));
        if id < 64 {
            assert_eq!(AsmReg::new(id).id(), id as u8);
            assert!(AsmReg::new(id).valid());
        }
        if let Ok(vector) = vector(reg) {
            assert_eq!(vector.value(), (id - 32) as u8);
        }
    }
    assert_eq!(AsmReg::FP, AsmReg::R29);
    assert_eq!(AsmReg::LR, AsmReg::R30);
    assert_eq!(AsmReg::SP.id(), 31);
    assert_eq!(AsmReg::V31.id(), 63);
    assert!(!AsmReg::default().valid());
    assert_eq!(AsmReg::from(Reg::make_invalid()), AsmReg::make_invalid());
    if tpde_util::CHECKED {
        assert!(std::panic::catch_unwind(|| AsmReg::new(64)).is_err());
        assert!(std::panic::catch_unwind(|| AsmReg::new(u64::MAX)).is_err());
    }
}

#[test]
fn register_masks_cover_both_banks_and_ignore_duplicates() {
    assert_eq!(create_bitmask(&[]), 0);
    assert_eq!(
        create_bitmask(&[AsmReg::R0, AsmReg::R0, AsmReg::SP, AsmReg::V0, AsmReg::V31]),
        0x8000_0001_8000_0001
    );
    if tpde_util::CHECKED {
        assert!(std::panic::catch_unwind(|| create_bitmask(&[AsmReg::make_invalid()])).is_err());
    }
}

#[test]
fn branch_inversion_covers_every_kind_and_preserves_payload() {
    use JumpKind::*;
    let pairs = [
        (Eq, Ne),
        (Cs, Cc),
        (Mi, Pl),
        (Vs, Vc),
        (Hi, Ls),
        (Ge, Lt),
        (Gt, Le),
        (Always, Always),
        (Cbz, Cbnz),
        (Tbz, Tbnz),
    ];
    for (a, b) in pairs {
        for (kind, inverse) in [(a, b), (b, a)] {
            let jump = Jump {
                kind,
                cmp_reg: Reg::new(19),
                cmp_is_32: true,
                test_bit: 47,
            };
            assert_eq!(invert_jump(jump), jump.change_kind(inverse));
            assert_eq!(invert_jump(invert_jump(jump)), jump);
        }
    }
}

#[test]
fn swapped_conditions_match_signed_unsigned_operand_relations() {
    use JumpKind::*;
    for (kind, swapped) in [
        (Eq, Eq),
        (Ne, Ne),
        (Cs, Ls),
        (Cc, Hi),
        (Hi, Cc),
        (Ls, Cs),
        (Ge, Le),
        (Lt, Gt),
        (Gt, Lt),
        (Le, Ge),
        (Always, Always),
    ] {
        let jump = Jump {
            kind,
            cmp_reg: Reg::new(7),
            cmp_is_32: true,
            test_bit: 11,
        };
        assert_eq!(swap_jump(jump).unwrap(), jump.change_kind(swapped));
        assert_eq!(swap_jump(swap_jump(jump).unwrap()).unwrap(), jump);
    }
    for kind in [Mi, Pl, Vs, Vc, Cbz, Cbnz, Tbz, Tbnz] {
        assert!(swap_jump(Jump {
            kind,
            ..Jump::default()
        })
        .is_err());
    }
}

#[test]
fn flags_conditions_have_architectural_encodings_and_register_tests_do_not() {
    use JumpKind::*;
    for (kind, encoding) in [
        (Eq, DA_EQ),
        (Ne, DA_NE),
        (Cs, DA_CS),
        (Cc, DA_CC),
        (Mi, DA_MI),
        (Pl, DA_PL),
        (Vs, DA_VS),
        (Vc, DA_VC),
        (Hi, DA_HI),
        (Ls, DA_LS),
        (Ge, DA_GE),
        (Lt, DA_LT),
        (Gt, DA_GT),
        (Le, DA_LE),
        (Always, DA_AL),
    ] {
        assert_eq!(jump_to_cond(Jump::new(kind)).unwrap(), encoding);
    }
    for kind in [Cbz, Cbnz, Tbz, Tbnz] {
        assert!(jump_to_cond(Jump {
            kind,
            ..Jump::default()
        })
        .is_err());
        if tpde_util::CHECKED {
            assert!(std::panic::catch_unwind(|| Jump::new(kind)).is_err());
        }
    }
}

#[test]
fn register_jump_constructors_reject_mismatched_kinds() {
    use JumpKind::*;
    let reg = Reg::new(5);
    assert_eq!(
        Jump::compare(Cbz, reg, true),
        Jump {
            kind: Cbz,
            cmp_reg: reg,
            cmp_is_32: true,
            test_bit: 0
        }
    );
    assert_eq!(
        Jump::test(Tbnz, reg, 63),
        Jump {
            kind: Tbnz,
            cmp_reg: reg,
            cmp_is_32: false,
            test_bit: 63
        }
    );
    if tpde_util::CHECKED {
        assert!(std::panic::catch_unwind(|| Jump::compare(Tbz, reg, false)).is_err());
        assert!(std::panic::catch_unwind(|| Jump::test(Cbnz, reg, 0)).is_err());
    }
    assert_eq!(Jump::default().kind, Always);
}

#[test]
fn raw_branch_words_cover_negative_displacements_and_signed_ranges() {
    use JumpKind::*;
    let reg = Reg::new(3);
    for (jump, expected) in [
        (Jump::new(Always), 0x17ff_ffff),
        (Jump::new(Eq), 0x54ff_ffe0),
        (Jump::compare(Cbz, reg, true), 0x34ff_ffe3),
        (Jump::compare(Cbz, reg, false), 0xb4ff_ffe3),
        (Jump::compare(Cbnz, reg, true), 0x35ff_ffe3),
        (Jump::compare(Cbnz, reg, false), 0xb5ff_ffe3),
        (Jump::test(Tbz, reg, 63), 0xb6ff_ffe3),
        (Jump::test(Tbnz, reg, 63), 0xb7ff_ffe3),
    ] {
        assert_eq!(branch_word(jump, -1).unwrap(), expected);
    }
    for (jump, bits) in [
        (Jump::new(Always), 26),
        (Jump::new(Eq), 19),
        (Jump::compare(Cbz, reg, false), 19),
        (Jump::test(Tbnz, reg, 0), 14),
    ] {
        let bound = 1isize << (bits - 1);
        assert_ne!(branch_word(jump, -bound).unwrap(), 0);
        assert_ne!(branch_word(jump, bound - 1).unwrap(), 0);
        assert_eq!(branch_word(jump, -bound - 1).unwrap(), 0);
        assert_eq!(branch_word(jump, bound).unwrap(), 0);
    }
    assert_eq!(branch_word(Jump::test(Tbz, reg, 64), 0).unwrap(), 0);
    assert!(branch_word(Jump::compare(Cbz, Reg::new(31), false), 0).is_err());
    assert!(branch_word(Jump::test(Tbz, Reg::new(32), 0), 0).is_err());
}

#[test]
fn memory_operand_contract_rejects_unsupported_widths_before_emission() {
    for size in [1, 2, 4, 8] {
        assert!(validate_memory_reg(Reg::new(0), size, false).is_ok());
        assert!(validate_memory_reg(Reg::new(30), size, true).is_ok());
    }
    for size in [1, 2, 4, 8, 16] {
        assert!(validate_memory_reg(Reg::new(32), size, false).is_ok());
        assert!(validate_memory_reg(Reg::new(63), size, false).is_ok());
        assert!(validate_memory_reg(Reg::new(32), size, true).is_err());
    }
    for size in [0, 3, 16, 32, u32::MAX] {
        assert!(validate_memory_reg(Reg::new(0), size, false).is_err());
    }
    for id in [31, 64, 255] {
        assert!(validate_memory_reg(Reg::new(id), 8, false).is_err());
    }
    for size in [0, 3, 32, u32::MAX] {
        assert!(validate_memory_reg(Reg::new(32), size, false).is_err());
    }
}

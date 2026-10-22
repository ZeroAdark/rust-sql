// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use tpde_fadec_gen::model::{InstrDesc, InstrFlags, OpKind, Opcode, encoding, op_order};
use tpde_fadec_gen::parser::{parse_table, verify_opcode_desc};

#[test]
fn opcode_grammar_covers_prefixes_escapes_and_modrm_forms() {
    for (text, escape, opc, extended, modrm) in [
        ("90", 0, 0x90, false, (None, None, None)),
        ("b8+", 0, 0xb8, true, (None, None, None)),
        ("0f", 0, 0x0f, false, (None, None, None)),
        ("0f38", 1, 0x38, false, (None, None, None)),
        ("0f38/m", 1, 0x38, false, (Some('m'), None, None)),
        ("0f38+", 1, 0x38, true, (None, None, None)),
        ("0f/m", 0, 0x0f, false, (Some('m'), None, None)),
        ("0f38f0", 2, 0xf0, false, (None, None, None)),
        ("0f3a0f", 3, 0x0f, false, (None, None, None)),
        ("M5.42", 5, 0x42, false, (None, None, None)),
        ("M6.42", 6, 0x42, false, (None, None, None)),
        ("M7.42", 7, 0x42, false, (None, None, None)),
        ("d9c7", 0, 0xd9, false, (Some('r'), Some(0), Some(7))),
        ("d9ff+", 0, 0xd9, true, (Some('r'), Some(7), Some(7))),
        ("80/3", 0, 0x80, false, (None, Some(3), None)),
        ("80/3m", 0, 0x80, false, (Some('m'), Some(3), None)),
        ("80/3r", 0, 0x80, false, (Some('r'), Some(3), None)),
        ("80/m3", 0, 0x80, false, (Some('m'), None, Some(3))),
        ("80/r3", 0, 0x80, false, (Some('r'), None, Some(3))),
    ] {
        let opcode = Opcode::parse(text).unwrap();
        assert_eq!(
            (opcode.escape, opcode.opc, opcode.extended, opcode.modrm),
            (escape, opc, extended, modrm),
            "{text}"
        );
    }
    for prefix in ["NP", "66", "F2", "F3", "NFx"] {
        for vector in ["", "VEX.", "EVEX."] {
            for length in ["0", "1", "12", "2", "IG"] {
                let opcode = Opcode::parse(&format!("{vector}{prefix}.W1.L{length}.0f58")).unwrap();
                assert_eq!(opcode.prefix.as_deref(), Some(prefix));
                assert_eq!(opcode.rexw.as_deref(), Some("1"));
                assert_eq!(opcode.vexl.as_deref(), Some(length));
                assert_eq!(
                    opcode.vex,
                    if vector.is_empty() {
                        0
                    } else if vector == "VEX." {
                        1
                    } else {
                        2
                    }
                );
            }
        }
    }
}

#[test]
fn opcode_grammar_rejects_nonmatching_forms() {
    for text in [
        "",
        "9",
        "90 ",
        " 90",
        "9A",
        "0F90",
        "M4.90",
        "M8.90",
        "90++",
        "90/8",
        "90/r8",
        "90/0rm",
        "90/rr",
        "90c",
        "90b0",
        "VEX.0f90",
        "VEX.66.W2.0f90",
        "66.L3.0f90",
        "66.L1.W0.0f90",
        "66.W0.W1.0f90",
        "66.66.0f90",
        "🙂",
    ] {
        assert!(Opcode::parse(text).is_err(), "{text}");
    }
    assert_eq!(Opcode::parse("90\n").unwrap(), Opcode::parse("90").unwrap());
}

#[test]
fn flag_packing_masks_values_and_keeps_operand_order_stable() {
    assert_eq!(InstrFlags::default().encode(), 0);
    let flags = InstrFlags {
        modrm_idx: 0xff,
        imm_control: 0xff,
        modrm_size: 2,
        size_fix1: 5,
        size_fix2: 3,
        instr_width: 1,
        modreg_ty: 6,
        imm_ty: 0xff,
        evex_rc: 3,
        opsize: 7,
        ign66: 1,
        ..InstrFlags::default()
    };
    assert_eq!(
        flags.encode(),
        3 | (7 << 12)
            | (2 << 16)
            | (5 << 26)
            | (3 << 29)
            | (1 << 31)
            | (6 << 35)
            | (3 << 40)
            | (7 << 43)
            | (1 << 47)
    );
    assert_eq!(
        op_order("NP").unwrap(),
        ["modrm", "modreg", "vexreg", "imm"]
    );
    assert_eq!(
        op_order("RVMI").unwrap(),
        ["modreg", "vexreg", "modrm", "imm"]
    );
    assert_eq!(
        op_order("TD").unwrap(),
        ["imm", "vexreg", "modrm", "modreg"]
    );
    assert_eq!(encoding("M1").unwrap().imm_control, 1);
    assert_eq!(encoding("RVMR").unwrap().imm_control, 3);
    assert_eq!(encoding("D").unwrap().imm_control, 6);
    assert!(encoding("UNKNOWN").is_err());
}

#[test]
fn canonical_operands_and_absolute_sizes() {
    assert_eq!(OpKind::parse("Ev").unwrap(), OpKind::parse("Gy").unwrap());
    assert_eq!(OpKind::parse("Vss").unwrap(), OpKind::parse("Ud").unwrap());
    assert_ne!(OpKind::parse("Vq").unwrap(), OpKind::parse("Gq").unwrap());
    for (text, expected) in [
        ("Mx", 64),
        ("Mh", 32),
        ("Mf", 16),
        ("Me", 8),
        ("Mv", 8),
        ("Mt", 10),
        ("M", 0),
    ] {
        assert_eq!(
            OpKind::parse(text)
                .unwrap()
                .abssize(Some(8), Some(64))
                .unwrap(),
            expected
        );
    }
    for text in ["Mv", "Mx", "Mh", "Mf", "Me"] {
        assert!(OpKind::parse(text).unwrap().abssize(None, None).is_err());
    }
    for (text, opsz, expected) in [
        ("Ibs", 8, 1),
        ("Iz", 2, 2),
        ("Iz", 8, 4),
        ("Izq", 8, 4),
        ("Iv", 8, 8),
        ("Ib", 8, 1),
    ] {
        assert_eq!(
            OpKind::parse(text).unwrap().immsize(opsz).unwrap(),
            expected
        );
    }
    for text in ["", "Xq"] {
        assert!(OpKind::parse(text).is_err());
    }
    assert!(
        OpKind::parse("Vunknown")
            .unwrap()
            .abssize(Some(8), Some(64))
            .is_err()
    );
}

#[test]
fn descriptor_sizes_flags_and_packed_words() {
    let descriptor = InstrDesc::parse("RM Gv Ev - - ADD+w").unwrap();
    assert_eq!(
        descriptor
            .dynsizes()
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        [-1]
    );
    assert_eq!(
        descriptor.encode("ADD", false, false).unwrap(),
        [14, 0x850a, 0x4009]
    );
    assert_eq!(
        InstrDesc::parse("NP - - - - NOP")
            .unwrap()
            .encode("NOP", false, false)
            .unwrap(),
        [0, 0x0400, 0]
    );
    let descriptor = InstrDesc::parse("MI Ev Ibs - - ADD").unwrap();
    assert_eq!(
        descriptor.encode("ADD", false, false).unwrap(),
        [0x5083, 0x0402, 0x4001]
    );
    assert_eq!(
        InstrDesc::parse("I Id - - - ENTER")
            .unwrap()
            .imm_size(4)
            .unwrap(),
        3
    );
    assert_eq!(
        InstrDesc::parse("M1 Ev Ib - - SHL")
            .unwrap()
            .imm_size(4)
            .unwrap(),
        0
    );
    assert_eq!(
        InstrDesc::parse("RVMR Vx Hx Wx Lx VBLENDVPS")
            .unwrap()
            .imm_size(4)
            .unwrap(),
        1
    );
    let descriptor = InstrDesc::parse("NP - - - - NOP+was k EXTRA U67").unwrap();
    assert!(descriptor.flags.contains("INSTR_WIDTH"));
    assert!(descriptor.flags.contains("USEG"));
    assert!(descriptor.flags.contains("U67"));
    assert!(descriptor.flags.contains("EXTRA"));
}

#[test]
fn descriptor_encoding_rejects_incompatible_size_combinations() {
    for text in [
        "RM Gv Wx - - BAD",
        "RM Vx Wx - - BAD LOCK",
        "RVM Vx Hh Wf - BAD",
        "RVM Vdq Hqq Woq - BAD",
    ] {
        assert!(
            InstrDesc::parse(text)
                .unwrap()
                .encode("BAD", false, false)
                .is_err(),
            "{text}"
        );
    }
    assert!(InstrDesc::parse("NP - - - - BAD+z").is_err());
    assert!(InstrDesc::parse("NP - - - NOP").is_err());
    assert!(
        InstrDesc::parse("MI Ev Izd - - VALID")
            .unwrap()
            .encode("VALID", false, false)
            .is_ok()
    );
}

#[test]
fn table_parsing_preserves_weak_lines_and_filters_after_validation() {
    let table = "# comment\n90 NP - - - - NOP\n*91 NP - - - - OTHER UNDOC\n";
    let documented = parse_table(table, false).unwrap();
    assert_eq!(documented.len(), 1);
    assert_eq!(documented[0].line, 2);
    let all = parse_table(table, true).unwrap();
    assert_eq!(all.len(), 2);
    assert!(all[1].weak);
    assert_eq!(all[1].line, 3);
    assert!(parse_table("90 RM Ib Ev - - BAD UNDOC", false).is_err());
    assert!(
        parse_table("# comment\nnot_an_opcode", false)
            .unwrap_err()
            .starts_with("line 2:")
    );
    for separator in [
        "\n", "\r", "\r\n", "\u{b}", "\u{c}", "\u{1c}", "\u{1d}", "\u{1e}", "\u{85}", "\u{2028}",
        "\u{2029}",
    ] {
        let entries = parse_table(
            &format!("90 NP - - - - NOP{separator}91 NP - - - - OTHER{separator}"),
            true,
        )
        .unwrap();
        assert_eq!(entries.len(), 2, "{separator:?}");
        assert_eq!(entries[1].line, 2, "{separator:?}");
    }
    assert!(parse_table("   ", false).is_err());
}

#[test]
fn opcode_description_validation_rejects_each_constraint_family() {
    for (text, expected) in [
        ("90 RM Ib Ev - - BAD", "modreg operand-regkind mismatch"),
        ("90 RM Gv Rv - - BAD", "modrm operand-regkind mismatch"),
        ("90 AM Kv Ev - - BAD", "vexreg operand-regkind mismatch"),
        ("90 I Gv - - - BAD", "imm operand-regkind mismatch"),
        (
            "VEX.66.0f10 RVMI Vx Hx Wx Ib BAD+w",
            "+w with four operands",
        ),
        ("0f38f0 MI Ev Ib - - BAD", "0f38 has no immediate"),
        ("0f3af0 M Ev - - - BAD", "0f3a must have immediate byte"),
        ("66.90 NP - - - - BAD", "unescaped opcode has prefix"),
        (
            "VEX.NP.0f10+ M Rv - - - BAD",
            "VEX/EVEX must not be extended",
        ),
        ("VEX.NFx.0f10 M Ev - - - BAD", "mandatory prefix"),
        ("VEX.66.LIG.0f10 RM Vx Wx - - BAD", "dynamic vector size"),
        ("0f10 M Ev - - - BAD VSIB", "VSIB for non-memory opcode"),
        (
            "EVEX.66.W0.0f58/r RVM Gv Bv Rv - BAD",
            "EVEX.vvvv must refer to XMM",
        ),
        (
            "EVEX.66.W0.0f58/r MR Ux Kq - - BAD",
            "ModRM.reg mask not first operand",
        ),
        ("EVEX.66.W0.0f58/m RM Vx Mx - - BAD", "missing tuple type"),
        (
            "EVEX.66.W0.0f58/m RM Vx Mx - - BAD TUPLE_FULL_MEM TUPLE_HALF_MEM",
            "missing tuple type",
        ),
        (
            "EVEX.66.W0.0f58/m R Vx - - - BAD TUPLE_FULL_MEM",
            "missing memory operand",
        ),
        (
            "EVEX.66.W0.0f58/m RM Vx Mx - - BAD+b TUPLE_FULL_MEM",
            "broadcast on incompatible type",
        ),
        (
            "EVEX.66.W0.0f58/m RM Vx Mx - - BAD+b TUPLE_FULL_16",
            "bcst16 mismatch",
        ),
        (
            "EVEX.66.W0.0f58/m RM Vx Mx - - BAD TUPLE_FULL_64",
            "incompatible EVEX.W",
        ),
        (
            "EVEX.66.W0.0f58/m RM Vx Mh - - BAD TUPLE_FULL_MEM",
            "memory size",
        ),
    ] {
        let error = parse_table(text, true).unwrap_err();
        assert!(error.contains(expected), "{text}: {error}");
    }
}

#[test]
fn evex_tuple_types_accept_all_declared_shapes() {
    for (tuple, memory, width) in [
        ("TUPLE_FULL_16", "Mx", "0"),
        ("TUPLE_FULL_32", "Mx", "0"),
        ("TUPLE_FULL_64", "Mx", "1"),
        ("TUPLE_HALF_16", "Mh", "0"),
        ("TUPLE_HALF_32", "Mh", "0"),
        ("TUPLE_HALF_64", "Mh", "1"),
        ("TUPLE_QUARTER_16", "Mf", "0"),
        ("TUPLE_FULL_MEM", "Mx", "0"),
        ("TUPLE_HALF_MEM", "Mh", "0"),
        ("TUPLE_QUARTER_MEM", "Mf", "0"),
        ("TUPLE_EIGHTH_MEM", "Me", "0"),
        ("TUPLE1_SCALAR_8", "Mb", "0"),
        ("TUPLE1_SCALAR_16", "Mw", "0"),
        ("TUPLE1_SCALAR_32", "Md", "0"),
        ("TUPLE1_SCALAR_64", "Mq", "1"),
        ("TUPLE1_SCALAR_OPSZ", "Mv", "0"),
        ("TUPLE1_FIXED_32", "Md", "0"),
        ("TUPLE1_FIXED_64", "Mq", "0"),
        ("TUPLE2_32", "Mq", "0"),
        ("TUPLE2_64", "Mdq", "1"),
        ("TUPLE4_32", "Mdq", "0"),
        ("TUPLE4_64", "Mqq", "1"),
        ("TUPLE8_32", "Mqq", "0"),
        ("TUPLE_MEM128", "Mdq", "0"),
        ("TUPLE_MOVDDUP", "Mx", "0"),
    ] {
        let opcode = Opcode::parse(&format!("EVEX.66.W{width}.0f58/m")).unwrap();
        let desc = InstrDesc::parse(&format!("RM Vx {memory} - - TEST {tuple}")).unwrap();
        verify_opcode_desc(&opcode, &desc).unwrap();
    }
    assert!(
        parse_table(
            "EVEX.66.W0.0f58/m RM Vx Mx - - TEST+b BCST16 TUPLE_FULL_16",
            true
        )
        .is_ok()
    );
    assert!(parse_table("EVEX.66.W0.0f58/m RM Vx Mx - - TEST+b TUPLE_FULL_32", true).is_ok());
    assert!(parse_table("EVEX.66.W1.0f58/m RM Vx Mx - - TEST+b TUPLE_FULL_64", true).is_ok());
}

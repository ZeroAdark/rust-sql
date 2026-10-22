// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use std::collections::BTreeMap;

use tpde_fadec_gen::encode::{encode_mnems, generate};
use tpde_fadec_gen::model::{Entry, InstrDesc, Opcode};

fn entries(rows: &[(&str, &str)]) -> Vec<Entry> {
    rows.iter()
        .enumerate()
        .map(|(i, (opcode, desc))| Entry {
            line: i + 1,
            weak: false,
            opcode: Opcode::parse(opcode).unwrap(),
            desc: InstrDesc::parse(desc).unwrap(),
        })
        .collect()
}

fn table(rows: &[(&str, &str)]) -> (BTreeMap<String, u64>, Vec<u64>) {
    let output = generate(&entries(rows)).unwrap();
    let mut mnemonics = BTreeMap::new();
    let mut alternatives = Vec::new();
    for row in output.semantic_tsv.lines() {
        let fields: Vec<_> = row.split('\t').collect();
        let value: u64 = fields[2].parse().unwrap();
        match fields[0] {
            "F" => {
                assert!(mnemonics.insert(fields[1].to_owned(), value).is_none());
                assert!(
                    output
                        .public_rs
                        .contains(&format!("pub const {}: u64 = {value:#x};", fields[1]))
                );
            }
            "A" => {
                assert_eq!(fields[1].parse::<usize>().unwrap(), alternatives.len());
                alternatives.push(value);
            }
            _ => panic!("unknown semantic row {row}"),
        }
    }
    assert!(output.private_rs.starts_with(&format!(
        "pub static FE_ALT_TABLE: [u64; {}]",
        alternatives.len()
    )));
    (mnemonics, alternatives)
}

#[test]
fn builtin_nop_pause_and_sign_extension_aliases() {
    let (words, alts) = table(&[("98", "NP - - - - C_EX+w"), ("99", "NP - - - - C_SEP+w")]);
    assert_eq!(words["FE_PAUSE"], 0x0020_0090);
    assert_eq!(words["FE_NOP"], 0x90);
    for (name, alias, expected) in [
        ("FE_C_EX16", "FE_CBW", 0x0008_0098),
        ("FE_C_EX32", "FE_CWDE", 0x98),
        ("FE_C_EX64", "FE_CDQE", 0x0040_0098),
        ("FE_C_SEP16", "FE_CWD", 0x0008_0099),
        ("FE_C_SEP32", "FE_CDQ", 0x99),
        ("FE_C_SEP64", "FE_CQO", 0x0040_0099),
    ] {
        assert_eq!(words[name], expected);
        assert_eq!(words[alias], expected);
    }
    assert_eq!(alts, [0]);
    let (replacement, _) = table(&[("91", "NP - - - - NOP")]);
    assert_eq!(replacement["FE_NOP"], 0x90);
}

#[test]
fn immediate_width_and_accumulator_priority() {
    let (words, alts) = table(&[
        ("04", "IA Rb Ib - - ADD SZ8"),
        ("80/0", "MI Eb Ib - - ADD SZ8"),
    ]);
    assert_eq!(words["FE_ADD8ri"], 0x0178_a000_0000_0004);
    assert_eq!(words["FE_ADD8mi"], 0x0008_8000_0000_0080);
    assert_eq!(alts, [0, 0x0008_a000_0000_0080]);

    let (words, _) = table(&[
        ("b0+", "OI Rb Ib - - MOVABS SZ8"),
        ("b8+", "OI Rv Iv - - MOVABS"),
    ]);
    assert_eq!(words["FE_MOV8ri"], 0x0060_a000_0000_00b0);
    assert_eq!(words["FE_MOV64ri"], 0x0064_0000_0040_00b8);
    assert!(!words.keys().any(|name| name.contains("MOVABS")));
}

#[test]
fn short_jump_alternatives_use_runtime_sentinel() {
    let rows = [
        ("70", "D Jbs - - - JO F64 ENC_CC_BEGIN"),
        ("0f80", "D Jz - - - JO F64 ENC_CC_BEGIN"),
    ];
    let (words, alts) = table(&rows);
    assert_eq!(words["FE_JO"], 0xff80_8000_0000_0070);
    assert_eq!(words["FE_Jcc"], words["FE_JO"]);
    assert_eq!(alts, [0]);
    let mnemonics = encode_mnems(&entries(&rows)).unwrap();
    let cc = mnemonics.iter().find(|m| m.name == "Jcc").unwrap();
    assert_eq!(cc.variants.len(), 2);
    assert!(cc.variants.iter().all(|variant| variant.flexcc));
}

#[test]
fn high_byte_register_support_tracks_source_operand() {
    let (words, _) = table(&[("0fbe", "RM Gv Eb - - MOVSX ENC_SEPSZ")]);
    assert_eq!(words["FE_MOVSXr32r8"], 0x0030_4000_0001_00be);
    assert_eq!(words["FE_MOVSXr32m8"], 0x0030_0000_0001_00be);
}

#[test]
fn vex_evex_downgrade_broadcast_mask_and_rounding() {
    let rows = [
        ("VEX.NP.0f58", "RVM Vps Hps Wps - VADDPS"),
        (
            "EVEX.NP.W0.0f58",
            "RVM Vps Hps Wps - EVX_ADDPS+kbr TUPLE_FULL_32",
        ),
    ];
    let (words, alts) = table(&rows);
    for (name, word) in [
        ("FE_VADDPS128rrr", 0x00a0_0040_0201_0058),
        ("FE_VADDPS128rrm", 0x00a0_0240_0201_0058),
        ("FE_VADDPS512rrr", 0x00a0_0000_0301_0058),
        ("FE_VADDPS512rrr_mask", 0x00a0_0000_0301_0058),
        ("FE_VADDPS512rrr_maskz", 0x00a0_0010_0301_0058),
        ("FE_VADDPS512rrb", 0x00a0_0100_0701_0058),
        ("FE_VADDPS512rrb_maskz", 0x00a0_0110_0701_0058),
        ("FE_VADDPS512rrr_er", 0x00a0_0000_0601_0058),
    ] {
        assert_eq!(words[name], word, "{name}");
    }
    assert!(!words.keys().any(|name| name.contains("128rrr_er")
        || name.contains("256rrr_er")
        || name.contains("rrm_er")
        || name.contains("rrb_er")));
    assert_eq!(alts, [0]);
    let mnemonics = encode_mnems(&entries(&rows)).unwrap();
    let memory = mnemonics.iter().find(|m| m.name == "VADDPS128rrm").unwrap();
    assert_eq!(memory.variants.len(), 1);
    assert_eq!(memory.variants[0].downgrade, 1);
    assert_eq!(memory.variants[0].evexdisp8scale, 4);
}

#[test]
fn downgrade_preserves_rexw_flip_and_canonical_operand_equality() {
    let rows = [
        ("VEX.66.0f58", "RVM Vpd Hpd Wpd - VADDPD"),
        (
            "EVEX.66.W1.0f58",
            "RVM Vx Hx Wx - EVX_ADDPD+kbr TUPLE_FULL_64",
        ),
    ];
    let (words, _) = table(&rows);
    assert_eq!(words["FE_VADDPD128rrr"], 0x00a0_0440_0251_0058);
    assert_eq!(words["FE_VADDPD128rrm"], 0x00a0_0640_0251_0058);
    let mnemonics = encode_mnems(&entries(&rows)).unwrap();
    assert_eq!(
        mnemonics
            .iter()
            .find(|m| m.name == "VADDPD128rrr")
            .unwrap()
            .variants[0]
            .downgrade,
        2
    );
}

#[test]
fn forced_sib_and_vsib_mask_are_distinct() {
    let (words, _) = table(&[("VEX.66.W0.L0.0f384b/m4", "RM T M - - TILELOADDT1 O64")]);
    assert_eq!(words["FE_TILELOADDT1rm"], 0x0030_0020_0112_004b);
    let rows = [(
        "EVEX.66.W0.0f3892/m",
        "RM Vx Md - - EVX_GATHERDPS+k VSIB TUPLE1_SCALAR_32",
    )];
    let (words, _) = table(&rows);
    assert_eq!(words["FE_VGATHERDPS128rm"], 0x0030_0100_0a12_0092);
    assert_eq!(words["FE_VGATHERDPS512rm"], 0x0030_0100_0b12_0092);
    assert!(!words.keys().any(|name| name.contains("_mask")));
    let mnemonics = encode_mnems(&entries(&rows)).unwrap();
    assert!(
        mnemonics
            .iter()
            .filter(|m| m.name.starts_with("VGATHERDPS"))
            .all(|m| m
                .variants
                .iter()
                .all(|v| v.evexmask == 1 && v.evexdisp8scale == 2))
    );
}

#[test]
fn lock_and_repeat_prefix_admission() {
    let (words, _) = table(&[
        ("01", "MR Ev Gv - - ADD+w LOCK"),
        ("a4", "NP - - - - MOVS+was SZ8 ENC_REP"),
        ("a6", "NP - - - - CMPS+was SZ8 ENC_REPCC"),
    ]);
    assert_eq!(words["FE_LOCK_ADD32mr"] ^ words["FE_ADD32mr"], 0x800000);
    assert!(!words.contains_key("FE_LOCK_ADD32rr"));
    assert_eq!(words["FE_REP_MOVS8"], 0x2000a4);
    assert_eq!(words["FE_REPNZ_CMPS8"], 0x1000a6);
    assert_eq!(words["FE_REPZ_CMPS8"], 0x2000a6);
}

#[test]
fn evex_historical_names_resolve_size_suffixes() {
    let rows = [
        ("EVEX.66.L0.0f7e", "MR Ey Vy - - EVX_MOV_X2G"),
        ("EVEX.66.L0.0f6e", "RM Vy Ey - - EVX_MOV_G2X"),
        ("EVEX.66.L0.0f3a16", "MRI Ey Vdq Ib - EVX_PEXTR"),
        ("EVEX.66.L0.0f3a22", "RVMI Vdq Hdq Ey Ib EVX_PINSR"),
        ("EVEX.66.W0.0f387c/r", "RM Vx Rd - - EVX_PBROADCAST+k"),
    ];
    let mnemonics = encode_mnems(&entries(&rows)).unwrap();
    for name in [
        "VMOVD_G2Xrr",
        "VMOVD_X2Grr",
        "VMOVDrm",
        "VMOVDmr",
        "VMOVQ_G2Xrm",
        "VMOVQ_X2Gmr",
        "VPEXTRDrri",
        "VPEXTRQmri",
        "VPINSRDrrri",
        "VPINSRQrrmi",
        "VPBROADCASTD_GP128rr",
        "VPBROADCASTD_GP512rr_maskz",
    ] {
        assert!(
            mnemonics.iter().any(|m| m.name == name && m.opsize == 0),
            "{name}"
        );
    }
}

#[test]
fn scalar_sae_broadcast16_and_memory_mask_rules() {
    let rows = [
        ("EVEX.F3.W0.LIG.0f58", "RVM Vss Hss Wss - EVX_ADDSS+ke"),
        ("EVEX.66.W0.0f3858", "RM Vx Ww - - EVX_SAMPLE+kb BCST16"),
        ("EVEX.66.W0.0f7f", "MR Wx Vx - - EVX_STORE+k"),
    ];
    let mnemonics = encode_mnems(&entries(&rows)).unwrap();
    assert!(mnemonics.iter().any(|m| m.name == "VADDSSrrr_sae"));
    assert!(!mnemonics.iter().any(|m| m.name == "VADDSSrrm_sae"));
    assert!(
        mnemonics
            .iter()
            .filter(|m| m.name.starts_with("VSAMPLE") && m.ots.contains('b'))
            .all(|m| m.variants.iter().all(|v| v.evexdisp8scale == 1))
    );
    assert!(mnemonics.iter().any(|m| m.name == "VSTORE128mr_mask"));
    assert!(!mnemonics.iter().any(|m| m.name == "VSTORE128mr_maskz"));
}

#[test]
fn dedup_preserves_first_source_except_segment_encodings() {
    let mut source = entries(&[
        ("91", "NP - - - - SAMPLE"),
        ("92", "NP - - - - SAMPLE"),
        ("06", "S S - - - PUSH F64"),
        ("0e", "S S - - - PUSH F64"),
        ("93", "NP - - - - RESERVED_SAMPLE"),
        ("94", "NP - - - - INVALID64 I64"),
    ]);
    source[0].weak = true;
    let mnemonics = encode_mnems(&source).unwrap();
    let sample = mnemonics.iter().find(|m| m.name == "SAMPLE").unwrap();
    assert_eq!(sample.variants.len(), 1);
    assert_eq!(sample.variants[0].opcode.opc, 0x91);
    assert_eq!(
        mnemonics
            .iter()
            .find(|m| m.name == "PUSHr")
            .unwrap()
            .variants
            .len(),
        2
    );
    assert!(
        !mnemonics
            .iter()
            .any(|m| m.name.contains("RESERVED") || m.name.contains("INVALID64"))
    );
}

#[test]
fn invalid_promotion_and_alternate_exhaustion_fail() {
    let source = entries(&[
        ("VEX.NP.0f58", "RVM Vps Hps Wps - VADDPS"),
        ("EVEX.NP.W0.0f59", "RVM Vps Hps Wps - EVX_ADDPS"),
    ]);
    assert!(
        encode_mnems(&source)
            .unwrap_err()
            .contains("cannot downgrade EVEX")
    );
    let source = entries(&[("EVEX.66.W1.0f6e", "RM Vy Ey - - EVX_MOV_G2X")]);
    assert!(
        encode_mnems(&source)
            .unwrap_err()
            .contains("unexpected REXW")
    );
    let template = entries(&[("06", "S S - - - PUSH F64")]).remove(0);
    assert!(
        generate(&vec![template; 258])
            .unwrap_err()
            .contains("alternate bits exhausted")
    );
}

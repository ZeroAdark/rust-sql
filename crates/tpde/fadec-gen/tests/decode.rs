// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use std::collections::BTreeSet;
use tpde_fadec_gen::decode::{DecodeOutput, generate, superstring};
use tpde_fadec_gen::model::{Entry, InstrDesc, Opcode};

fn entry(opcode: &str, mnemonic: &str, weak: bool) -> Entry {
    Entry {
        line: 1,
        weak,
        opcode: Opcode::parse(opcode).unwrap(),
        desc: InstrDesc {
            mnemonic: mnemonic.into(),
            encoding: "NP".into(),
            operands: Vec::new(),
            flags: BTreeSet::new(),
        },
    }
}

fn table(output: &DecodeOutput) -> Vec<u16> {
    output
        .semantic_tsv
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            (fields.next()? == "TABLE").then(|| {
                fields.next().unwrap();
                fields.next().unwrap().parse().unwrap()
            })
        })
        .collect()
}

fn roots(output: &DecodeOutput) -> Vec<(u8, usize)> {
    output
        .semantic_tsv
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            (fields.next()? == "ROOT").then(|| {
                (
                    fields.next().unwrap().parse().unwrap(),
                    fields.next().unwrap().parse().unwrap(),
                )
            })
        })
        .collect()
}

fn walk(table: &[u16], parent: u16, slot: usize) -> u16 {
    table[usize::from(parent & !3) + slot]
}

#[test]
fn empty_tables_keep_all_prefix_values_and_mode_order() {
    for modes in [&[32, 64][..], &[64, 32][..], &[32][..], &[64][..]] {
        let output = generate(&[], modes).unwrap();
        let data = table(&output);
        assert_eq!(data.len(), modes.len() * 256);
        for ((mode, offset), requested) in roots(&output).into_iter().zip(modes) {
            assert_eq!(mode, *requested);
            assert_eq!(data[offset + 0x26], 0xfff8 + u16::from(mode == 32));
            assert_eq!(data[offset + 0x64], 0xfff9);
            assert_eq!(data[offset + 0x65], 0xfff9);
            assert_eq!(data[offset + 0x66], 0xfffa);
            assert_eq!(data[offset + 0x67], 0xfffb);
            assert_eq!(data[offset + 0xf0], 0xfffc);
            assert_eq!(data[offset + 0xf2], 0xfffd);
            assert_eq!(data[offset + 0xf3], 0xfffd);
            for rex in 0x40..0x50 {
                assert_eq!(data[offset + rex], if mode == 64 { 0xfffe } else { 0 });
            }
        }
        assert!(output.semantic_tsv.contains("DESC\t0\t0\t0\t0\t0\n"));
        assert!(
            output
                .semantic_tsv
                .contains("MNEM\t0\tPAUSE\tpause\t0\t5\n")
        );
    }
}

#[test]
fn explicit_no_modes_still_generates_descriptors_and_names() {
    let output = generate(&[entry("90", "NOP", false)], &[]).unwrap();
    assert!(table(&output).is_empty());
    assert!(roots(&output).is_empty());
    assert!(output.semantic_tsv.contains("DESC\t1\t0\t0\t1024\t0\n"));
    assert!(output.public_rs.contains("pub const FDI_NOP: u16 = 0;"));
    assert!(output.stats.contains("Trie -- 0 bytes"));
}

#[test]
fn duplicate_modes_and_invalid_modes_reject() {
    for modes in [&[32, 32][..], &[64, 64][..], &[64, 32, 64][..], &[16][..]] {
        assert!(generate(&[], modes).is_err());
    }
}

#[test]
fn descriptor_identity_includes_mnemonic_and_maps_aliases() {
    let output = generate(
        &[
            entry("90", "MOV_CR2G", false),
            entry("91", "MOV_G2CR", false),
            entry("92", "MOV_DR2G", false),
        ],
        &[32],
    )
    .unwrap();
    let data = table(&output);
    assert_eq!(data[0x90], 4);
    assert_eq!(data[0x91], 4);
    assert_eq!(data[0x92], 8);
    assert_eq!(
        output
            .semantic_tsv
            .lines()
            .filter(|line| line.starts_with("DESC\t"))
            .count(),
        3
    );
    assert!(output.semantic_tsv.contains("MNEM\t0\tMOV_CR\tmov\t"));
    assert!(output.semantic_tsv.contains("MNEM\t1\tMOV_DR\tmov\t"));
}

#[test]
fn mode_flags_skip_only_the_restricted_root() {
    let mut legacy = entry("40", "INC", false);
    legacy.desc.flags.insert("I64".into());
    let mut long = entry("63", "MOVSX", false);
    long.desc.flags.insert("O64".into());
    let output = generate(&[legacy, long], &[32, 64]).unwrap();
    let data = table(&output);
    assert_eq!(data[0x40], 4);
    assert_eq!(data[0x63], 0);
    assert_eq!(data[256 + 0x40], 0xfffe);
    assert_eq!(data[256 + 0x63], 8);
}

#[test]
fn weak_entries_are_replaceable_but_cannot_overwrite_strong_entries() {
    let output = generate(
        &[
            entry("90", "FIRST", true),
            entry("90", "SECOND", true),
            entry("90", "THIRD", false),
            entry("90", "FOURTH", true),
        ],
        &[32],
    )
    .unwrap();
    assert_eq!(table(&output)[0x90], 12);
    let invalid = [entry("90", "FIRST", false), entry("90", "SECOND", false)];
    assert!(
        generate(&invalid, &[32])
            .unwrap_err()
            .contains("redundant non-weak")
    );
    assert_eq!(table(&generate(&invalid[..1], &[32]).unwrap())[0x90], 4);
}

#[test]
fn opcode_extension_covers_eight_consecutive_bytes() {
    let output = generate(&[entry("50+", "PUSH", false)], &[32]).unwrap();
    let data = table(&output);
    assert_eq!(&data[0x50..0x58], &[4; 8]);
    assert_eq!(data[0x58], 0);
}

#[test]
fn mandatory_prefix_refinement_clones_nested_modrm_tables() {
    let output = generate(
        &[
            entry("0f11/r0", "BASE", true),
            entry("66.0f11/r0", "SPECIFIC", false),
        ],
        &[32],
    )
    .unwrap();
    let data = table(&output);
    let escaped = walk(&data, data[0x0f], 1);
    let prefixes = walk(&data, escaped, 0x11);
    assert_eq!(prefixes & 3, 3);
    for prefix in 0..4 {
        let modrm = walk(&data, prefixes, prefix);
        assert_eq!(modrm & 3, 1);
        for reg in 0..8 {
            assert_eq!(walk(&data, modrm, reg * 2), 0);
            let rm = walk(&data, modrm, reg * 2 + 1);
            assert_eq!(rm & 3, 1);
            assert_eq!(walk(&data, rm, 0), if prefix == 1 { 8 } else { 4 });
            for slot in 1..8 {
                assert_eq!(walk(&data, rm, slot), 0);
            }
        }
    }
    assert_eq!(walk(&data, prefixes, 0), walk(&data, prefixes, 2));
    assert_eq!(walk(&data, prefixes, 2), walk(&data, prefixes, 3));
}

#[test]
fn nfx_selects_no_prefix_and_66_but_leaves_repeat_prefixes_empty() {
    let output = generate(&[entry("NFx.0f10", "TEST", false)], &[32]).unwrap();
    let data = table(&output);
    let escaped = walk(&data, data[0x0f], 1);
    let prefixes = walk(&data, escaped, 0x10);
    assert_eq!(prefixes & 3, 3);
    assert_eq!(
        (0..4).map(|i| walk(&data, prefixes, i)).collect::<Vec<_>>(),
        [4, 4, 0, 0]
    );
}

#[test]
fn fixed_modrm_extension_omits_the_rm_selector_table() {
    let output = generate(&[entry("0f01c0+", "TEST", false)], &[32]).unwrap();
    let data = table(&output);
    let escaped = walk(&data, data[0x0f], 1);
    let modrm = walk(&data, escaped, 1);
    assert_eq!(modrm & 3, 1);
    assert_eq!(walk(&data, modrm, 1), 4);
    assert_eq!(walk(&data, modrm, 0), 0);
    assert_eq!(walk(&data, modrm, 3), 0);
}

#[test]
fn vex_and_evex_select_width_and_length_and_share_c4_c5_tables() {
    let output = generate(
        &[
            entry("VEX.NP.W1.L1.0f58", "VECTOR", false),
            entry("EVEX.NP.W0.L12.0f59", "EVECTOR", false),
        ],
        &[64],
    )
    .unwrap();
    let data = table(&output);
    assert_eq!(data[0xc4], data[0xc5]);
    let vex_escaped = walk(&data, data[0xc4], 1);
    let vex_prefixes = walk(&data, vex_escaped, 0x58);
    let vex = walk(&data, vex_prefixes, 0);
    for i in 0..8 {
        assert_eq!(walk(&data, vex, i), if i == 3 { 4 } else { 0 });
    }
    let evex_escaped = walk(&data, data[0x62], 1);
    let evex_prefixes = walk(&data, evex_escaped, 0x59);
    let evex = walk(&data, evex_prefixes, 0);
    for i in 0..8 {
        assert_eq!(walk(&data, evex, i), if matches!(i, 2 | 4) { 8 } else { 0 });
    }
}

#[test]
fn legacy_prefix_opcodes_use_escape_zero_and_do_not_encode_modrm() {
    let output = generate(
        &[
            entry("c4/m", "LES", false),
            entry("VEX.NP.0f58", "VECTOR", false),
        ],
        &[32],
    )
    .unwrap();
    let data = table(&output);
    assert_eq!(data[0xc4] & 3, 2);
    assert_eq!(walk(&data, data[0xc4], 0), 4);
    let vex_escaped = walk(&data, data[0xc4], 1);
    let prefixes = walk(&data, vex_escaped, 0x58);
    assert_eq!(walk(&data, prefixes, 0), 8);
    assert_eq!(walk(&data, data[0xc5], 0), 0);
}

#[test]
fn malformed_opcode_paths_return_errors() {
    for opcode in ["ff+", "c4", "EVEX.NP.L1.0f58", "VEX.NP.L2.0f58"] {
        assert!(
            generate(&[entry(opcode, "TEST", false)], &[32]).is_err(),
            "{opcode}"
        );
    }
    let mut invalid = entry("0f10", "TEST", false);
    invalid.opcode.prefix = Some("bogus".into());
    assert!(generate(&[invalid.clone()], &[32]).is_err());
    invalid.opcode.prefix = None;
    invalid.opcode.modrm = (Some('x'), None, None);
    assert!(generate(&[invalid.clone()], &[32]).is_err());
    invalid.opcode.modrm = (None, Some(8), None);
    assert!(generate(&[invalid.clone()], &[32]).is_err());
    invalid.opcode.modrm = (None, None, None);
    invalid.opcode.escape = 8;
    assert!(generate(&[invalid.clone()], &[32]).is_err());
    invalid.opcode.escape = 1;
    invalid.opcode.vex = 3;
    assert!(generate(&[invalid], &[32]).is_err());
}

#[test]
fn superstring_preserves_containment_and_greedy_tie_order() {
    let cases: &[(&[&str], &str)] = &[
        (&[], ""),
        (&["", ""], ""),
        (&["abc", "bc", "", "abc"], "abc"),
        (&["ab", "bc", "ca"], "cabc"),
        (&["abc", "abd", "za"], "zabcabd"),
        (&["éλ", "λx", "xé"], "λxéλ"),
    ];
    for (input, expected) in cases {
        let input: Vec<String> = input.iter().map(|value| (*value).into()).collect();
        assert_eq!(superstring(&input), *expected, "{input:?}");
    }
}

#[test]
fn intel_names_keep_all_special_rewrites_and_empty_xchg_name() {
    let names = [
        ("SSE_MOVD_X2G", "movd"),
        ("MMX_MOVQ_G2M", "movq"),
        ("EVX_ADD", "vadd"),
        ("MOVABS", "mov"),
        ("RESERVED_X", "x"),
        ("JMPF", "jmp far"),
        ("CALLF", "call far"),
        ("MOV_S2G", "mov"),
        ("REP_MOVS", "rep movs"),
        ("CMPXCHGD", "cmpxchg"),
        ("JCXZ", "jcxz jecxzjrcxz"),
        ("C_SEP", "cwd cdq cqo"),
        ("C_EX", "cbw cwdecdqe"),
        ("XCHG_NOP", ""),
    ];
    let entries: Vec<Entry> = names
        .iter()
        .map(|(name, _)| entry("90", name, true))
        .collect();
    let output = generate(&entries, &[]).unwrap();
    for (_, intel) in names {
        assert!(
            output
                .semantic_tsv
                .lines()
                .filter(|line| line.starts_with("MNEM\t"))
                .any(|line| line.split('\t').nth(3) == Some(intel)),
            "{intel}"
        );
    }
    assert!(output.semantic_tsv.contains("\tXCHG_NOP\t\t0\t0\n"));
}

#[test]
fn empty_stats_include_unallocated_kinds_in_reverse_order() {
    assert_eq!(
        generate(&[], &[32]).unwrap().stats,
        "Decode stats: Descs -- 1 (8 bytes);  Trie -- 512 bytes, {'TABLE_ROOT': 1, 'TABLE_VEX': 0, 'TABLE8E': 0, 'TABLE16': 0, 'TABLE_PREFIX': 0, 'TABLE256': 0, 'ESCAPE': 0}; Mnems -- 5 + 3 bytes"
    );
}

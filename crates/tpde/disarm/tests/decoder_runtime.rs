// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use crate::common;
use std::fs;
use tpde_disarm::{
    da64_classify, da64_decode, da64_format, da64_format_abs, FormatError, Instruction, Operand,
};

pub(super) fn append_format(output: &mut Vec<u8>, inst: &Instruction, address: Option<u64>) {
    let mut bytes = [0x5a; 160];
    let written = if let Some(address) = address {
        da64_format_abs(inst, address, &mut bytes[16..144])
    } else {
        da64_format(inst, &mut bytes[16..144])
    }
    .expect("decoded instruction must format");
    assert!(written < 128);
    assert_eq!(bytes[16 + written], 0);
    assert_eq!(&bytes[..16], &[0x5a; 16]);
    assert_eq!(&bytes[144..], &[0x5a; 16]);
    let visible = bytes[16..17 + written]
        .iter()
        .position(|&byte| byte == 0)
        .unwrap();
    output.extend_from_slice(&(visible as u16).to_le_bytes());
    output.extend_from_slice(&bytes[16..17 + visible]);
}

fn mnemonic_copy_boundaries() {
    if tpde_disarm::GENERATION_PROFILE != "really-all" {
        return;
    }
    for word in [0xd503_467f, 0xd503_477f] {
        let inst = tpde_disarm::decode(word);
        assert_ne!(inst.mnem, 0);
        for absolute in [false, true] {
            let format = |out: &mut [u8]| {
                if absolute {
                    da64_format_abs(&inst, u64::MAX, out)
                } else {
                    da64_format(&inst, out)
                }
            };
            for sentinel in [0, 0x5a, b'?'] {
                let mut expected = *b"msr svcrsmza, #0\0";
                expected[12] = sentinel;
                expected[15] = b'0' + ((word >> 8) & 1) as u8;
                let mut output = [sentinel; 40];
                assert_eq!(format(&mut output[8..25]), Ok(16));
                assert_eq!(&output[8..25], &expected);
                assert_eq!(&output[..8], &[sentinel; 8]);
                assert_eq!(&output[25..], &[sentinel; 15]);
                for capacity in 0..=16 {
                    let mut short = [sentinel; 40];
                    assert_eq!(
                        format(&mut short[8..8 + capacity]),
                        Err(FormatError::OutputTooSmall)
                    );
                    assert_eq!(short, [sentinel; 40]);
                }
            }
        }
    }
    println!("TPDE_DISARM_RUNTIME_MNEMONIC_GAPS_OK profile=really-all words=2 sentinels=3 apis=2 capacities=18");
}

#[test]
fn every_decoder_pattern_and_raw_words_match_fields_and_full_c_text() {
    let dir = common::prepare("decode");
    let references = common::compile_all(&dir, "decoder_reference.cpp");
    mnemonic_copy_boundaries();
    let counts = common::counts(&dir.join("decoder-counts.tsv"));
    assert_eq!(
        counts["patterns"],
        if tpde_disarm::GENERATION_PROFILE == "really-all" {
            9112
        } else {
            9102
        }
    );
    assert!(counts["pattern_cases"] >= counts["patterns"] * 42);
    assert_eq!(counts["random_cases"], 262_144);
    assert_eq!(
        counts["cases"],
        counts["pattern_cases"] + counts["random_cases"] + 4
    );
    assert_eq!(counts["record_bytes"], 20);
    let input = fs::read(dir.join("decoder-input.bin")).unwrap();
    assert_eq!(input.len(), counts["cases"] * 20);
    let mut output = Vec::with_capacity(counts["cases"] * 100);
    let mut op_kinds = [0usize; 256];
    let mut known = 0;
    let mut unknown = 0;
    for (index, record) in input.chunks_exact(20).enumerate() {
        let word = u32::from_le_bytes(record[..4].try_into().unwrap());
        let mut inst = Instruction {
            mnem: u16::MAX,
            ops: [Operand {
                kind: u8::MAX,
                reg: u8::MAX,
                data: u16::MAX,
            }; 5],
            imm64: u64::from_le_bytes(record[4..12].try_into().unwrap()),
        };
        let address = u64::from_le_bytes(record[12..].try_into().unwrap());
        da64_decode(word, &mut inst);
        let mnem = da64_classify(word);
        assert_eq!(mnem, u32::from(inst.mnem), "case {index} word={word:08x}");
        if mnem == 0 {
            unknown += 1;
        } else {
            known += 1;
        }
        output.extend_from_slice(&mnem.to_le_bytes());
        output.extend_from_slice(&inst.mnem.to_le_bytes());
        for op in inst.ops {
            op_kinds[usize::from(op.kind)] += 1;
            output.push(op.kind);
            output.push(op.reg);
            output.extend_from_slice(&op.data.to_le_bytes());
        }
        output.extend_from_slice(&inst.imm64.to_le_bytes());
        append_format(&mut output, &inst, Some(address));
        append_format(&mut output, &inst, None);
        if index < 64 {
            let mut buffer = [0x5a; 128];
            let size = da64_format_abs(&inst, address, &mut buffer).unwrap();
            for length in 0..=size {
                let mut short = [0x5a; 128];
                assert!(da64_format_abs(&inst, address, &mut short[..length]).is_err());
                assert_eq!(
                    short, [0x5a; 128],
                    "short capacity mutated at case {index}, length {length}"
                );
            }
            let mut exact = [0x5a; 128];
            assert_eq!(
                da64_format_abs(&inst, address, &mut exact[..size + 1]).unwrap(),
                size
            );
            assert_eq!(&exact[..size + 1], &buffer[..size + 1]);
        }
    }
    assert!(known > 0 && unknown > 0);
    let observed = op_kinds.iter().filter(|&&count| count > 0).count();
    assert!(observed >= 24, "only {observed} operand kinds observed");
    fs::write(
        dir.join("decoder-operand-kinds.tsv"),
        op_kinds
            .iter()
            .enumerate()
            .filter(|(_, count)| **count > 0)
            .map(|(kind, count)| format!("{kind}\t{count}\n"))
            .collect::<String>(),
    )
    .unwrap();
    fs::write(dir.join("decoder-rust.bin"), &output).unwrap();
    let modes = common::compare_all(&dir, &references, &input, &output, "actual classifier, decoded fields including retained immediate, and complete formatter text");
    println!("TPDE_DISARM_RUNTIME_DECODE_FORMAT_OK profile={} patterns={} cases={} known={known} unknown={unknown} operand_kinds={observed} cpp_modes={modes}", tpde_disarm::GENERATION_PROFILE, counts["patterns"], counts["cases"]);
}

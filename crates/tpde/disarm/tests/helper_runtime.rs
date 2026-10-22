// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use super::common;
use tpde_disarm::runtime::GReg;
use tpde_disarm::{
    da_immadd, da_immfmov32, da_immfmov64, da_immlogical, da_immsimdmovi, da_sext, de64_MOVconst,
};

const CANARY: u32 = 0xa5c3_f19d;

#[derive(Default)]
struct Corpus {
    input: Vec<u8>,
    expected: Vec<u8>,
    counts: [usize; 7],
    lengths: [usize; 4],
    first_instructions: [usize; 4],
}

impl Corpus {
    fn push(&mut self, operation: u8, parameter: u8, value: u64) {
        self.input.extend_from_slice(&[operation, parameter, 0, 0]);
        self.input.extend_from_slice(&value.to_le_bytes());
        self.counts[usize::from(operation - 1)] += 1;
        let result = match operation {
            1 => da_sext(value as i64, u32::from(parameter)) as u64,
            2 => u64::from(da_immadd(value as i64)),
            3 => u64::from(da_immlogical(value, u32::from(parameter))),
            4 => u64::from(da_immfmov32(f32::from_bits(value as u32))),
            5 => u64::from(da_immfmov64(f64::from_bits(value))),
            6 => u64::from(da_immsimdmovi(value)),
            7 => {
                self.mov_const(parameter, value);
                return;
            }
            _ => unreachable!(),
        };
        self.expected.extend_from_slice(&result.to_le_bytes());
    }

    fn mov_const(&mut self, register: u8, value: u64) {
        let register = GReg::new(register).unwrap();
        let mut words = [CANARY; 6];
        let length = de64_MOVconst(&mut words[1..5], register, value).unwrap();
        assert!((1..=4).contains(&length));
        assert_eq!(words[0], CANARY);
        assert_eq!(words[5], CANARY);
        assert!(words[length + 1..5].iter().all(|&word| word == CANARY));
        self.lengths[length - 1] += 1;
        self.first_instructions[first_kind(words[1])] += 1;
        assert_eq!(materialized(&words[1..=length], register.value()), value);
        for capacity in 0..length {
            let mut short = [CANARY; 6];
            assert!(de64_MOVconst(&mut short[1..1 + capacity], register, value).is_err());
            assert_eq!(short, [CANARY; 6]);
            assert_eq!(
                de64_MOVconst(&mut short[1..1 + length], register, value).unwrap(),
                length
            );
            assert_eq!(short, words);
        }
        let mut exact = [CANARY; 6];
        assert_eq!(
            de64_MOVconst(&mut exact[1..1 + length], register, value).unwrap(),
            length
        );
        assert_eq!(exact, words);
        self.expected
            .extend_from_slice(&(length as u32).to_le_bytes());
        for word in &words[1..5] {
            self.expected.extend_from_slice(&word.to_le_bytes());
        }
    }
}

fn first_kind(word: u32) -> usize {
    match word & 0xff80_0000 {
        0xd280_0000 => 0,
        0x9280_0000 => 1,
        0xb200_0000 => 2,
        0x1280_0000 => 3,
        _ => panic!("unexpected first instruction {word:08x}"),
    }
}

fn repeated(element: u64, width: u32) -> u64 {
    let mut result = 0;
    for offset in (0..64).step_by(width as usize) {
        result |= element << offset;
    }
    result
}

fn rotate_element(value: u64, rotation: u32, width: u32) -> u64 {
    if width == 64 {
        value.rotate_right(rotation)
    } else {
        ((value >> rotation) | (value << ((width - rotation) % width))) & ((1 << width) - 1)
    }
}

fn materialized(words: &[u32], register: u8) -> u64 {
    let mut value = 0;
    for (index, &word) in words.iter().enumerate() {
        assert_eq!(word & 31, u32::from(register));
        let shift = ((word >> 21) & 3) * 16;
        let immediate = u64::from((word >> 5) & 0xffff) << shift;
        let is64 = word >> 31 != 0;
        match word & 0x7f80_0000 {
            0x1280_0000 => {
                assert_eq!(index, 0);
                value = !immediate;
            }
            0x5280_0000 => {
                assert_eq!(index, 0);
                value = immediate;
            }
            0x7280_0000 => {
                assert_ne!(index, 0);
                value = (value & !(0xffff << shift)) | immediate;
            }
            0x3200_0000 => {
                assert_eq!(index, 0);
                assert_eq!((word >> 5) & 31, 31);
                let n = (word >> 22) & 1;
                let imms = (word >> 10) & 63;
                let immr = (word >> 16) & 63;
                let discriminator = (n << 6) | (!imms & 63);
                assert_ne!(discriminator, 0);
                let width = 1 << (31 - discriminator.leading_zeros());
                let ones = (imms & (width - 1)) + 1;
                assert!(ones < width);
                let element = (1u64 << ones) - 1;
                value = repeated(rotate_element(element, immr & (width - 1), width), width);
            }
            _ => panic!("unsupported materialization word {word:08x}"),
        }
        if !is64 {
            assert!(shift < 32);
            value &= u64::from(u32::MAX);
        }
    }
    value
}

fn next_random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn sign_extension(corpus: &mut Corpus) {
    for bits in 1..=63 {
        let sign = 1u64 << (bits - 1);
        let mask = (1u64 << bits) - 1;
        for value in [0, 1, sign - 1, sign, sign + 1, mask, !mask, u64::MAX] {
            corpus.push(1, bits, value);
        }
        if bits <= 12 {
            for value in 0..=mask {
                corpus.push(1, bits, value);
            }
        }
    }
}

fn arithmetic(corpus: &mut Corpus) {
    for magnitude in 0..=0xfff_u64 {
        for value in [magnitude, magnitude << 12] {
            for candidate in [value, value.wrapping_neg()] {
                corpus.push(2, 0, candidate);
            }
        }
    }
    for value in [
        0xfff_u64,
        0x1000,
        0xfff000,
        0x1000000,
        i64::MAX as u64,
        1u64 << 63,
    ] {
        for candidate in [value.wrapping_sub(1), value, value.wrapping_add(1)] {
            corpus.push(2, 0, candidate);
            corpus.push(2, 0, candidate.wrapping_neg());
        }
    }
}

fn logical(corpus: &mut Corpus) {
    for is64 in [0, 1] {
        let maximum = if is64 == 0 { 32 } else { 64 };
        for width in [2, 4, 8, 16, 32, 64] {
            if width > maximum {
                continue;
            }
            for ones in 1..width {
                for rotation in 0..width {
                    let value =
                        repeated(rotate_element((1u64 << ones) - 1, rotation, width), width);
                    corpus.push(
                        3,
                        is64,
                        if is64 == 0 {
                            value & u64::from(u32::MAX)
                        } else {
                            value
                        },
                    );
                }
            }
        }
        for value in [0, u64::MAX, 0x1234_5678_9abc_def0, 0x8000_0000_0000_0001] {
            corpus.push(
                3,
                is64,
                if is64 == 0 {
                    value & u64::from(u32::MAX)
                } else {
                    value
                },
            );
        }
    }
}

fn floating(corpus: &mut Corpus) {
    for sign in 0..=1u64 {
        for exponent in 0x7c..=0x83u64 {
            for fraction in 0..16u64 {
                let value32 = (sign << 31) | (exponent << 23) | (fraction << 19);
                corpus.push(4, 0, value32);
                for bit in 0..19 {
                    corpus.push(4, 0, value32 ^ (1 << bit));
                }
                let value64 = (sign << 63) | ((exponent + (0x3fc - 0x7c)) << 52) | (fraction << 48);
                corpus.push(5, 0, value64);
                for bit in 0..48 {
                    corpus.push(5, 0, value64 ^ (1 << bit));
                }
            }
        }
        for exponent in 0..=255u64 {
            corpus.push(4, 0, (sign << 31) | (exponent << 23));
        }
        for exponent in 0..=2047u64 {
            corpus.push(5, 0, (sign << 63) | (exponent << 52));
        }
    }
    for value in [1, 0x7fc0_0001, 0x7f80_0001, 0xffc0_0000, 0x007f_ffff] {
        corpus.push(4, 0, value);
    }
    for value in [
        1,
        0x7ff8_0000_0000_0001,
        0x7ff0_0000_0000_0001,
        0xfff8_0000_0000_0000,
        0x000f_ffff_ffff_ffff,
    ] {
        corpus.push(5, 0, value);
    }
}

fn simd(corpus: &mut Corpus) {
    for immediate in 0..=255u64 {
        corpus.push(6, 0, repeated(immediate, 8));
        let mut mask = 0;
        for byte in 0..8 {
            if immediate & (1 << byte) != 0 {
                mask |= 0xff << (8 * byte);
            }
        }
        corpus.push(6, 0, mask);
        for width in [16, 32] {
            for shift in (0..width).step_by(8) {
                let value = repeated(immediate << shift, width);
                corpus.push(6, 0, value);
                corpus.push(6, 0, !value);
            }
        }
        for shift in [8, 16] {
            let value = repeated((immediate << shift) | ((1 << shift) - 1), 32);
            corpus.push(6, 0, value);
            corpus.push(6, 0, !value);
        }
    }
    for value in [
        0x1234_5678_1234_5678,
        0x1234_1234_1234_1234,
        0x00fe_ff00_ff00_ff00,
        0x1234_5678_9abc_def0,
    ] {
        corpus.push(6, 0, value);
    }
}

fn moves(corpus: &mut Corpus) {
    let mut values = vec![
        0,
        u64::MAX,
        0x1234_ffff,
        0xffff_1234,
        0x1234_5678,
        0x1234_0000_5678,
        0x1234_5678_9abc,
        0x1234_5678_9abc_def0,
        0xff00_ff00_ff00_ff00,
        0x00ff_00ff_00ff_00ff,
        0x8000_0000_0000_0001,
        0x0000_ffff_0000_ffff,
    ];
    for immediate in [
        1u64, 2, 0x7f, 0x80, 0xff, 0x100, 0x7fff, 0x8000, 0xfffe, 0xffff,
    ] {
        for shift in [0, 16, 32, 48] {
            let value = immediate << shift;
            values.extend_from_slice(&[
                value,
                !value,
                value.wrapping_sub(1),
                value.wrapping_add(1),
            ]);
        }
    }
    for register in 0..32 {
        for &value in &values {
            corpus.push(7, register, value);
        }
    }
    for immediate in 0..=u16::MAX {
        corpus.push(7, (immediate & 31) as u8, u64::from(immediate));
        corpus.push(7, (immediate & 31) as u8, !u64::from(immediate));
    }
    let mut random = 0x3165_634b_9e20_481du64;
    for index in 0..8192 {
        let value = next_random(&mut random);
        corpus.push(7, (index & 31) as u8, value);
        corpus.push(3, 1, value);
        corpus.push(3, 0, value & u64::from(u32::MAX));
        corpus.push(6, 0, value);
        corpus.push(2, 0, value);
    }
    assert!(corpus.lengths.iter().all(|&count| count != 0));
    assert!(corpus.first_instructions.iter().all(|&count| count != 0));
}

#[test]
fn actual_runtime_helpers_match_compiled_upstream() {
    let work = common::prepare("runtime-helpers");
    let references = common::compile_all(&work, "helper_reference.cpp");
    let mut corpus = Corpus::default();
    sign_extension(&mut corpus);
    arithmetic(&mut corpus);
    logical(&mut corpus);
    floating(&mut corpus);
    simd(&mut corpus);
    moves(&mut corpus);
    assert_eq!(
        corpus.counts,
        [8_694, 24_612, 23_028, 5_637, 16_645, 12_804, 144_768]
    );
    assert_eq!(corpus.input.len(), 236_188 * 12);
    assert_eq!(corpus.expected.len(), 3_626_720);
    std::fs::write(work.join("helpers-input.bin"), &corpus.input).unwrap();
    std::fs::write(work.join("helpers-rust.bin"), &corpus.expected).unwrap();
    let names = [
        "sext", "immadd", "logical", "fmov32", "fmov64", "simd", "movconst",
    ];
    let mut counts = String::from("operation\tname\tcases\n");
    for (index, (name, count)) in names.iter().zip(&corpus.counts).enumerate() {
        counts.push_str(&format!("{}\t{name}\t{count}\n", index + 1));
    }
    std::fs::write(work.join("helpers-counts.tsv"), counts).unwrap();
    common::compare_all(
        &work,
        &references,
        &corpus.input,
        &corpus.expected,
        "runtime-helpers",
    );
    println!(
        "TPDE_DISARM_RUNTIME_HELPERS_PASS profile={} cases={} sext={} immadd={} logical={} fmov32={} fmov64={} simd={} movconst={} lengths={:?} first_instructions={:?} cpp_modes=4",
        tpde_disarm::GENERATION_PROFILE, corpus.counts.iter().sum::<usize>(), corpus.counts[0], corpus.counts[1], corpus.counts[2],
        corpus.counts[3], corpus.counts[4], corpus.counts[5], corpus.counts[6],
        corpus.lengths, &corpus.first_instructions[..4]
    );
}

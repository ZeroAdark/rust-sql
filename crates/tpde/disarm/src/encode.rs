// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use crate::{
    generated::*,
    runtime::{GReg, DA_ZR},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodeError {
    OutputTooSmall { needed: usize, available: usize },
}

#[inline]
pub fn encode_word(word: u32, output: &mut [u8]) -> Result<usize, EncodeError> {
    if output.len() < 4 {
        return Err(EncodeError::OutputTooSmall {
            needed: 4,
            available: output.len(),
        });
    }
    output[..4].copy_from_slice(&word.to_le_bytes());
    Ok(4)
}

#[allow(non_snake_case)]
pub fn de64_MOVconst(output: &mut [u32], reg: GReg, cnst: u64) -> Result<usize, EncodeError> {
    let mut words = [0; 4];
    let count = materialize(&mut words, reg, cnst);
    if output.len() < count {
        return Err(EncodeError::OutputTooSmall {
            needed: count,
            available: output.len(),
        });
    }
    output[..count].copy_from_slice(&words[..count]);
    Ok(count)
}

fn materialize(words: &mut [u32; 4], reg: GReg, cnst: u64) -> usize {
    let gp = reg;
    let reg = reg.into();
    if cnst < 0x10000 {
        words[0] = de64_MOVZx(reg, cnst as u16 as u64);
        return 1;
    }
    if cnst >= 0xffff_ffff_ffff_0000 {
        words[0] = de64_MOVNx(reg, !cnst as u16 as u64);
        return 1;
    }
    let clz = cnst.leading_zeros() >> 4;
    let iclz = (!cnst).leading_zeros() >> 4;
    let ctz = cnst.trailing_zeros() >> 4;
    let ictz = (!cnst).trailing_zeros() >> 4;
    if clz + ctz == 3 {
        words[0] = de64_MOVZx_shift(reg, cnst >> (ctz * 16), ctz);
        return 1;
    }
    if iclz + ictz == 3 {
        words[0] = de64_MOVNx_shift(reg, !cnst >> (ictz * 16), ictz);
        return 1;
    }
    words[0] = de64_ORRxi(gp.into(), DA_ZR, cnst);
    if words[0] != 0 {
        return 1;
    }
    if clz == 2 && (((!cnst as u32).leading_zeros() >> 4) + ictz >= 1) {
        words[0] = de64_MOVNw_shift(reg, ((!cnst as u32) >> (ictz * 16)) as u64, ictz);
        return 1;
    }
    words[0] = de64_MOVZx_shift(reg, cnst >> (16 * ctz) & 0xffff, ctz);
    let mut count = 1;
    for i in ctz + 1..4 {
        if cnst >> (16 * i) & 0xffff != 0 {
            words[count] = de64_MOVKx_shift(reg, cnst >> (16 * i) & 0xffff, i);
            count += 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::GRegZr;

    #[test]
    fn materialization_choices_and_atomic_capacity() {
        let reg = GReg::new(9).unwrap();
        let cases = [
            (0, 1),
            (0xffff, 1),
            (u64::MAX, 1),
            (0x1234_0000_0000, 1),
            (0xffff_edcb_ffff_ffff, 1),
            (0xff00_ff00_ff00_ff00, 1),
            (0xffff_1234, 1),
            (0x1234_5678, 2),
            (0x1234_5678_9abc, 3),
            (0x1234_5678_9abc_def0, 4),
        ];
        for (value, count) in cases {
            let mut out = [0xa5a5_a5a5; 6];
            assert_eq!(de64_MOVconst(&mut out[1..5], reg, value), Ok(count));
            assert!(out[1..count + 1].iter().all(|&word| word != 0));
            assert_eq!(out[0], 0xa5a5_a5a5);
            assert!(out[count + 1..].iter().all(|&word| word == 0xa5a5_a5a5));
            for available in 0..count {
                let mut guarded = [0x5a5a_5a5a; 6];
                assert_eq!(
                    de64_MOVconst(&mut guarded[1..1 + available], reg, value),
                    Err(EncodeError::OutputTooSmall {
                        needed: count,
                        available
                    })
                );
                assert_eq!(guarded, [0x5a5a_5a5a; 6]);
                assert_eq!(de64_MOVconst(&mut guarded[1..5], reg, value), Ok(count));
            }
        }
    }

    #[test]
    fn generated_forms_call_real_helpers_and_reject_bad_operands() {
        let reg = GRegZr::new(1).unwrap();
        assert_eq!(de64_MOVZx(reg, 0x1234), 0xd282_4681);
        assert_eq!(de64_MOVZx(reg, 0x1_0000), 0);
        assert_ne!(
            de64_ORRxi(GReg::new(1).unwrap().into(), DA_ZR, 0xff00_ff00_ff00_ff00),
            0
        );
        assert_eq!(de64_ORRxi(GReg::new(1).unwrap().into(), DA_ZR, 0), 0);
        assert_eq!(de64_MOVZx_shift(reg, 0x1234, 4), 0);
    }

    #[test]
    fn raw_words_are_little_endian_and_admit_before_effects() {
        let mut out = [0xa5; 6];
        assert_eq!(
            encode_word(0x1234_5678, &mut out[1..4]),
            Err(EncodeError::OutputTooSmall {
                needed: 4,
                available: 3
            })
        );
        assert_eq!(out, [0xa5; 6]);
        assert_eq!(encode_word(0x1234_5678, &mut out[1..]), Ok(4));
        assert_eq!(out, [0xa5, 0x78, 0x56, 0x34, 0x12, 0xa5]);
        assert_eq!(encode_word(0, &mut out[1..]), Ok(4));
        assert_eq!(out, [0xa5, 0, 0, 0, 0, 0xa5]);
    }
}

// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use core::mem::{align_of, needs_drop, size_of};

use tpde_fadec::*;

#[test]
fn register_domains_preserve_class_boundaries_and_sentinels() {
    for idx in 0..=255 {
        assert_eq!(FE_GP(idx).is_ok(), idx < 16);
        assert_eq!(FE_GPH(idx).is_ok(), (4..=7).contains(&idx));
        assert_eq!(FE_GPLH(idx).is_ok(), idx < 16 || (36..=39).contains(&idx));
        assert_eq!(FE_SREG(idx).is_ok(), idx < 6);
        assert_eq!(FE_ST(idx).is_ok(), idx < 8);
        assert_eq!(FE_MM(idx).is_ok(), idx < 8);
        assert_eq!(FE_XMM(idx).is_ok(), idx < 32);
        assert_eq!(FE_K(idx).is_ok(), idx < 8);
        assert_eq!(FE_TMM(idx).is_ok(), idx < 8);
        assert_eq!(FE_CR(idx).is_ok(), idx < 16);
        assert_eq!(FE_DR(idx).is_ok(), idx < 16);
    }
    assert_eq!(FE_IP.index(), 0x20);
    assert_eq!(FE_NOREG.index(), 0x80);
    assert_eq!(FE_MAKE_GPLH(FE_AX).index(), 0);
    assert_eq!(FE_MAKE_GPLH(FE_R15).index(), 15);
    assert_eq!(FE_MAKE_GPLH(FE_AH).index(), 0x24);
    assert_eq!(FE_MAKE_GPLH(FE_BH).index(), 0x27);
}

#[test]
fn header_constants_and_operand_layouts() {
    assert_eq!(size_of::<FeRegGP>(), 1);
    assert_eq!(size_of::<FeRegGPLH>(), 1);
    assert_eq!(size_of::<FeRegXMM>(), 1);
    assert_eq!(size_of::<FeMem>(), 8);
    assert_eq!(size_of::<FeMemV>(), 8);
    assert_eq!(align_of::<FeMem>(), 4);
    assert_eq!(align_of::<FeMemV>(), 4);
    assert!(!needs_drop::<FeMem>());
    assert!(!needs_drop::<FeMemV>());
    assert_eq!([FE_JMPL, FE_ADDR32, FE_SEG_MASK], [8, 16, 7]);
    assert_eq!([FE_RC_RN, FE_RC_RD, FE_RC_RU, FE_RC_RZ], [0, 32, 64, 96]);
    assert_eq!(FE_RC_MASK, 96);
    for (idx, cond) in [
        FE_CC_O, FE_CC_NO, FE_CC_C, FE_CC_NC, FE_CC_Z, FE_CC_NZ, FE_CC_BE, FE_CC_A, FE_CC_S,
        FE_CC_NS, FE_CC_P, FE_CC_NP, FE_CC_L, FE_CC_GE, FE_CC_LE, FE_CC_G,
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(cond, (idx as u32) << 16);
    }
    assert_eq!(FE_CC_MASK, FE_CC_G);
    assert_eq!([FE_CC_B, FE_CC_NAE], [FE_CC_C; 2]);
    assert_eq!([FE_CC_AE, FE_CC_NB], [FE_CC_NC; 2]);
    assert_eq!(FE_CC_E, FE_CC_Z);
    assert_eq!(FE_CC_NE, FE_CC_NZ);
    assert_eq!(FE_CC_NA, FE_CC_BE);
    assert_eq!(FE_CC_NBE, FE_CC_A);
    assert_eq!(FE_CC_PE, FE_CC_P);
    assert_eq!(FE_CC_PO, FE_CC_NP);
    assert_eq!(FE_CC_NGE, FE_CC_L);
    assert_eq!(FE_CC_NL, FE_CC_GE);
    assert_eq!(FE_CC_NG, FE_CC_LE);
    assert_eq!(FE_CC_NLE, FE_CC_G);
    assert_eq!(
        [
            FE_SEG(FE_ES),
            FE_SEG(FE_CS),
            FE_SEG(FE_SS),
            FE_SEG(FE_DS),
            FE_SEG(FE_FS),
            FE_SEG(FE_GS)
        ],
        [1, 2, 3, 4, 5, 6]
    );
}

#[test]
fn memory_operands_enforce_scale_index_and_vector_addressing() {
    for base in [FE_AX, FE_SP, FE_BP, FE_R15, FE_NOREG, FE_IP] {
        for scale in 0..=255 {
            assert_eq!(FE_MEM(base, scale, FE_NOREG, 0).is_ok(), scale == 0);
            assert_eq!(
                FE_MEM(base, scale, FE_CX, 0).is_ok(),
                matches!(scale, 1 | 2 | 4 | 8)
            );
            assert!(FE_MEM(base, scale, FE_SP, 0).is_err());
            assert!(FE_MEM(base, scale, FE_IP, 0).is_err());
            assert_eq!(
                FE_MEMV(base, scale, FE_XMM31, 0).is_ok(),
                base != FE_IP && matches!(scale, 1 | 2 | 4 | 8)
            );
        }
    }
    for off in [i32::MIN, -129, -128, -1, 0, 1, 127, 128, i32::MAX] {
        let mem = FE_MEM(FE_R15, 8, FE_R12, off).unwrap().with_flags(0xff);
        assert_eq!(mem.base(), FE_R15);
        assert_eq!(mem.scale(), 8);
        assert_eq!(mem.index(), FE_R12);
        assert_eq!(mem.offset(), off);
        assert_eq!(mem.flags(), 0xff);
        let vector = FE_MEMV(FE_NOREG, 4, FE_XMM31, off).unwrap().with_flags(8);
        assert_eq!(vector.base(), FE_NOREG);
        assert_eq!(vector.scale(), 4);
        assert_eq!(vector.index(), FE_XMM31);
        assert_eq!(vector.offset(), off);
        assert_eq!(vector.flags(), 8);
    }
}

#[test]
fn legacy_xmm_sentinels_and_capacity_failures_preserve_output() {
    let mut bytes = [0xa5; 16];
    let mut output = Output::new(&mut bytes[1..15], 0x1000);
    assert_eq!(
        fe64_SSE_MOVAPSrr(&mut output, 0, FE_XMM16, FE_XMM0),
        Err(EncodeError::InvalidOperand)
    );
    assert_eq!(
        fe64_SSE_MOVAPSrr(&mut output, 0, FE_XMM0, FE_XMM31),
        Err(EncodeError::InvalidOperand)
    );
    assert_eq!(
        fe64_MOV64rr(&mut output, 0, FE_IP, FE_AX),
        Err(EncodeError::InvalidOperand)
    );
    assert_eq!(
        fe64_MOV8rr(&mut output, 0, FE_AX, FE_NOREG),
        Err(EncodeError::InvalidOperand)
    );
    assert!(output.is_empty());
    assert_eq!(fe64_SSE_MOVAPSrr(&mut output, 0, FE_XMM0, FE_XMM1), Ok(3));
    assert_eq!(output.bytes(), &[0x0f, 0x28, 0xc1]);
    assert_eq!(output.address(), 0x1003);
    assert_eq!(&bytes[..1], &[0xa5]);
    assert_eq!(&bytes[4..], &[0xa5; 12]);

    let mut bytes = [0xa5; 4];
    let mut output = Output::new(&mut bytes[1..3], 0x1000);
    assert_eq!(
        fe64_SSE_MOVAPSrr(&mut output, 0, FE_XMM0, FE_XMM1),
        Err(EncodeError::BufferTooSmall {
            required: 3,
            remaining: 2
        })
    );
    assert!(output.is_empty());
    assert_eq!(bytes, [0xa5; 4]);

    let mut bytes = [0xa5; 8];
    let mut output = Output::new(&mut bytes[1..7], 0x1000);
    assert_eq!(fe64_VMOVAPS512rr(&mut output, 0, FE_XMM16, FE_XMM31), Ok(6));
    assert_eq!(output.bytes()[0], 0x62);
    assert_eq!(bytes[0], 0xa5);
    assert_eq!(bytes[7], 0xa5);
}

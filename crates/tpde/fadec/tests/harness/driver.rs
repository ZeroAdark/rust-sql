// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use std::io::{BufWriter, Write};
use tpde_fadec::oracle::{self, Encode2Runtime, RawRegister, Runtime};
use tpde_fadec::*;

struct Case {
    flags: u32,
    regs: [u8; 4],
    mask: u8,
    imm: i64,
    absolute: u64,
    delta: i64,
    memflags: u8,
    base: u8,
    scale: u8,
    index: u8,
    off: i32,
    opcode: u32,
    ripoff: usize,
    forcesib: u32,
    disp8scale: i32,
    size: usize,
}

static CASES: &[Case] = &[CASE_VALUES];
static SELECTED: &[usize] = &[SELECTION_VALUES];
static OFFSETS: &[usize] = &[OFFSET_VALUES];
static HELPER_CASES: &[(usize, usize)] = &[HELPER_VALUES];
const ENCODER_TOTAL: usize = FUNCTION_COUNT;
const ADDRESS: usize = 0x1_0000_0000;

RUST_GROUP_FUNCTIONS

fn invoke(function: usize, buf: &mut [u8; 32], c: &Case) -> usize {
    match function / RUST_GROUP_SIZE {
        FORM_CALLS
        _ => unreachable!(),
    }
}

fn gplh(index: u8) -> Option<FeRegGPLH> {
    if (36..40).contains(&index) {
        Some(FeRegGPH::new(index & 7).ok()?.into())
    } else {
        Some(FeRegGP::new(index).ok()?.into())
    }
}

fn base(index: u8) -> Option<FeRegGP> {
    match index {
        32 => Some(FE_IP),
        128 => Some(FE_NOREG),
        _ => FeRegGP::new(index).ok(),
    }
}

fn memory(c: &Case) -> Option<FeMem> {
    Some(FeMem::new(base(c.base)?, c.scale, base(c.index)?, c.off).ok()?.with_flags(c.memflags))
}

fn memoryv(c: &Case) -> Option<FeMemV> {
    Some(FeMemV::new(base(c.base)?, c.scale, FeRegXMM::new(c.index).ok()?, c.off).ok()?.with_flags(c.memflags))
}

fn invoke_public(function: usize, out: &mut Output<'_>, c: &Case) -> Option<Result<usize, EncodeError>> {
    match function / RUST_GROUP_SIZE {
        PUBLIC_CALLS
        _ => unreachable!(),
    }
}

fn class_rejection(function: usize, c: &Case) -> bool {
    match function / RUST_GROUP_SIZE {
        PUBLIC_ADMISSION
        _ => false,
    }
}

fn helper(function: usize, buf: &mut [u8; 32], c: &Case) -> usize {
    let mem = oracle::raw_mem(c.memflags, c.base, c.scale, c.index, c.off);
    let memv = oracle::raw_memv(c.memflags, c.base, c.scale, c.index, c.off);
    let reg = u32::from(c.regs[0]);
    let vvvv = u32::from(c.regs[1]);
    match function {
        0 => usize::from(Runtime::op_imm_n(c.imm, c.size)),
        1 => Runtime::enc_seg67(buf, c.flags),
        2 => Runtime::enc_rex_mem(mem, reg) as usize,
        3 => { Runtime::enc_imm(buf, c.imm, c.size); c.size },
        4 => Runtime::enc_mem(buf, c.ripoff, mem, reg, c.forcesib, c.disp8scale),
        5 => Runtime::enc_vex_common(buf, c.opcode, u32::from(c.base), u32::from(c.index), reg, vvvv),
        6 => Runtime::enc_vex_reg(buf, c.opcode, u32::from(c.base), reg, vvvv),
        7 => Runtime::enc_evex_reg(buf, c.opcode, u32::from(c.base), reg, vvvv),
        8 => Runtime::enc_evex_xmm(buf, c.opcode, u32::from(c.base), reg, vvvv),
        9 => Runtime::enc_vex_mem(buf, c.opcode, mem, reg, vvvv, c.ripoff, c.forcesib, c.disp8scale),
        10 => Runtime::enc_evex_mem(buf, c.opcode, mem, reg, vvvv, c.ripoff, c.forcesib, c.disp8scale),
        11 => Runtime::enc_vex_vsib(buf, c.opcode, memv, reg, vvvv, c.ripoff, c.forcesib, c.disp8scale),
        12 => Runtime::enc_evex_vsib(buf, c.opcode, memv, reg, vvvv, c.ripoff, c.forcesib, c.disp8scale),
        _ => unreachable!(),
    }
}

fn record(out: &mut impl Write, function: usize, caseid: usize, result: usize, bytes: &[u8]) {
    write!(out, "{function}\t{caseid}\t{result}\t").unwrap();
    for byte in bytes { write!(out, "{byte:02x}").unwrap(); }
    writeln!(out).unwrap();
}

#[derive(Default)]
struct Counts {
    typed: usize,
    constructed_refusals: usize,
    encoding_refusals: usize,
    class_refusals: usize,
    capacity: usize,
    reuse: usize,
    successful_functions: usize,
}

fn check_public(function: usize, caseid: usize, c: &Case, result: usize, raw: &[u8; 32], capacity: bool, counts: &mut Counts) -> bool {
    let mut bytes = [0xa5; 64];
    let mut out = Output::new(&mut bytes[1..63], ADDRESS - 3);
    assert_eq!(fe64_NOP(&mut out, 3), Ok(3));
    assert_eq!(out.address(), ADDRESS);
    let before = [0x0f, 0x1f, 0x00];
    let Some(actual) = invoke_public(function, &mut out, c) else {
        counts.constructed_refusals += 1;
        assert_eq!(out.bytes(), before);
        assert_eq!(bytes[0], 0xa5);
        assert_eq!(&bytes[4..], &[0xa5; 60]);
        return false;
    };
    counts.typed += 1;
    let class_rejected = class_rejection(function, c);
    if class_rejected {
        assert_eq!(actual, Err(EncodeError::InvalidOperand), "function={function} case={caseid}");
        counts.class_refusals += 1;
    } else if result == 0 {
        assert_eq!(actual, Err(EncodeError::InvalidEncoding), "function={function} case={caseid}");
        counts.encoding_refusals += 1;
    } else {
        assert_eq!(actual, Ok(result), "function={function} case={caseid}");
        assert_eq!(&out.bytes()[..3], before);
        assert_eq!(&out.bytes()[3..], &raw[..result], "function={function} case={caseid}");
        assert_eq!(out.address(), ADDRESS + result);
    }
    if class_rejected || result == 0 {
        assert_eq!(out.bytes(), before);
        assert_eq!(out.address(), ADDRESS);
        drop(out);
        assert_eq!(bytes[0], 0xa5);
        assert_eq!(&bytes[1..4], before);
        assert_eq!(&bytes[4..], &[0xa5; 60]);
        let mut reused = [0xa5; 64];
        let mut output = Output::new(&mut reused[1..63], ADDRESS - 3);
        assert_eq!(fe64_NOP(&mut output, 3), Ok(3));
        assert_eq!(invoke_public(function, &mut output, c), Some(actual));
        assert_eq!(fe64_MOV64rr(&mut output, 0, FE_AX, FE_CX), Ok(3));
        assert_eq!(output.len(), 6);
        assert_eq!(&output.bytes()[3..], &[0x48, 0x89, 0xc8]);
        assert_eq!(reused[0], 0xa5);
        assert_eq!(&reused[7..], &[0xa5; 57]);
        counts.reuse += 1;
        return false;
    }
    assert_eq!(bytes[0], 0xa5);
    assert!(bytes[4+result..].iter().all(|byte| *byte == 0xa5));
    if capacity {
        for remaining in [result-1, result] {
            let mut storage = [0xa5; 64];
            let end = 4 + remaining;
            let mut out = Output::new(&mut storage[1..end], ADDRESS - 3);
            assert_eq!(fe64_NOP(&mut out, 3), Ok(3));
            let actual = invoke_public(function, &mut out, c).unwrap();
            if remaining < result {
                assert_eq!(actual, Err(EncodeError::BufferTooSmall { required: result, remaining }));
                assert_eq!(out.bytes(), before);
                assert_eq!(out.address(), ADDRESS);
                drop(out);
                assert_eq!(storage[0], 0xa5);
                assert_eq!(&storage[1..4], before);
                assert!(storage[4..].iter().all(|byte| *byte == 0xa5));
                if remaining >= 1 {
                    let mut reused = [0xa5; 64];
                    let mut output = Output::new(&mut reused[1..end], ADDRESS - 3);
                    assert_eq!(fe64_NOP(&mut output, 3), Ok(3));
                    assert_eq!(invoke_public(function, &mut output, c), Some(actual));
                    assert_eq!(fe64_CLD(&mut output, 0), Ok(1));
                    assert_eq!(&output.bytes()[3..], &[0xfc]);
                    assert_eq!(reused[0], 0xa5);
                    assert!(reused[5..].iter().all(|byte| *byte == 0xa5));
                    counts.reuse += 1;
                }
            } else {
                assert_eq!(actual, Ok(result));
                assert_eq!(&out.bytes()[3..], &raw[..result]);
                assert_eq!(out.remaining(), 0);
                assert!(storage[end..].iter().all(|byte| *byte == 0xa5));
            }
            assert_eq!(storage[0], 0xa5);
            counts.capacity += 1;
        }
    }
    true
}

fn forms(out: &mut impl Write) {
    assert_eq!(OFFSETS.len(), ENCODER_TOTAL + 1);
    assert_eq!(GENERATED_ENCODER_COUNT, ENCODER_TOTAL);
    let mut counts = Counts::default();
    let mut missing = Vec::new();
    for function in 0..ENCODER_TOTAL {
        assert!(OFFSETS[function] < OFFSETS[function+1]);
        let mut succeeded = false;
        for &caseid in &SELECTED[OFFSETS[function]..OFFSETS[function+1]] {
            let case = &CASES[caseid];
            let mut buf = [0xa5; 32];
            let result = invoke(function, &mut buf, case);
            assert!(result <= 32);
            record(out, function, caseid, result, &buf[..result]);
            succeeded |= check_public(function, caseid, case, result, &buf, !succeeded, &mut counts);
        }
        if succeeded {
            counts.successful_functions += 1;
        } else {
            missing.push(function);
        }
    }
    assert!(counts.typed > ENCODER_TOTAL);
    assert!(missing.is_empty(), "no successful typed call for generated functions {missing:?}");
    assert_eq!(counts.successful_functions, ENCODER_TOTAL);
    assert_eq!(counts.capacity, counts.successful_functions * 2);
    assert!(counts.reuse > 0);
    assert!(counts.constructed_refusals > 0 && counts.encoding_refusals > 0 && counts.class_refusals > 0);
    eprintln!("TPDE_FADEC_RUNTIME_PUBLIC PASS typed={} constructor_refusals={} encoding_refusals={} class_refusals={} successful_functions={} capacity_checks={} reuse_checks={}",
              counts.typed, counts.constructed_refusals, counts.encoding_refusals, counts.class_refusals, counts.successful_functions, counts.capacity, counts.reuse);
}

fn helpers(out: &mut impl Write) {
    for &(function, caseid) in HELPER_CASES {
        let mut bytes = [0xa5; 32];
        let result = helper(function, &mut bytes, &CASES[caseid]);
        record(out, function, caseid, result, &bytes);
    }
}

fn nops(out: &mut impl Write) {
    let lengths = (0..129).chain([255, 256, 511, 512, 1023, 1024, 4095, 4096, 65535, 65536]);
    for (id, length) in lengths.enumerate() {
        let size = length.max(1);
        let mut bytes = vec![0x5a; size + 32];
        let mut output = Output::new(&mut bytes[16..16+size], ADDRESS);
        assert_eq!(fe64_NOP(&mut output, length as u32), Ok(size));
        assert_eq!(output.len(), size);
        assert_eq!(output.address(), ADDRESS + size);
        record(out, id, length, size, output.bytes());
        assert_eq!(&bytes[..16], &[0x5a; 16]);
        assert_eq!(&bytes[16+size..], &[0x5a; 16]);
        let mut short = vec![0xa5; size + 2];
        let mut output = Output::new(&mut short[1..size], ADDRESS);
        assert_eq!(fe64_NOP(&mut output, length as u32), Err(EncodeError::BufferTooSmall { required: size, remaining: size-1 }));
        assert!(output.is_empty());
        assert!(short.iter().all(|byte| *byte == 0xa5));
    }
}

fn main() {
    assert_eq!(oracle::BUILD_DIRECTORY, ORACLE_BUILD_DIRECTORY_LITERAL);
    let stdout = std::io::stdout();
    let mut out = BufWriter::with_capacity(1024 * 1024, stdout.lock());
    match std::env::args().nth(1).as_deref() {
        Some("forms") => forms(&mut out),
        Some("helpers") => helpers(&mut out),
        Some("nops") => nops(&mut out),
        _ => panic!("expected forms, helpers, or nops"),
    }
    out.flush().unwrap();
}

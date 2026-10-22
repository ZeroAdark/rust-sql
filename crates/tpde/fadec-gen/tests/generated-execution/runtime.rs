// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use std::cell::RefCell;
use std::fmt::Write as _;

#[derive(Default)]
struct RecordingState {
    base: usize,
    failure: u32,
    variant: u32,
    trace: String,
}

thread_local! {
    static RECORDING: RefCell<RecordingState> = RefCell::new(RecordingState::default());
}

struct Recorder;

fn recording_reset(buf: &[u8; 32], failure: u32, variant: u32) {
    RECORDING.with_borrow_mut(|state| {
        state.base = buf.as_ptr() as usize;
        state.failure = failure;
        state.variant = variant;
        state.trace.clear();
    });
}

fn recording_event(id: u32, buf: Option<&[u8]>, args: &[u64], result: u64) {
    RECORDING.with_borrow_mut(|state| {
        let offset = buf.map_or(-1, |buf| {
            let offset = (buf.as_ptr() as usize).checked_sub(state.base).unwrap();
            assert!(offset <= 32);
            offset as i64
        });
        write!(state.trace, "{id},{offset}").unwrap();
        for arg in args {
            write!(state.trace, ",{arg}").unwrap();
        }
        write!(state.trace, ",{result};").unwrap();
    });
}

fn recording_marker(id: u32, buf: &mut [u8], args: &[u64], ripoff: usize) -> usize {
    let (length, fail) = RECORDING
        .with_borrow(|state| (if state.variant == 0 { 3 } else { 5 }, state.failure == id));
    assert!(buf.len() >= length);
    for (i, byte) in buf[..length].iter_mut().enumerate() {
        *byte = (id * 17 + i as u32 * 29) as u8;
    }
    let result = if fail { 0 } else { ripoff + length };
    recording_event(id, Some(buf), args, result as u64);
    result
}

fn recording_finish(
    output: &mut impl std::io::Write,
    function: usize,
    case: usize,
    result: usize,
    buf: &[u8; 32],
) {
    assert!(result <= 32);
    write!(output, "{function}\t{case}\t{result}\t").unwrap();
    for byte in buf {
        write!(output, "{byte:02x}").unwrap();
    }
    RECORDING.with_borrow(|state| writeln!(output, "\t{}", state.trace).unwrap());
}

macro_rules! recording_reg {
    ($name:ident, $id:literal) => {
        fn $name(buf: &mut [u8], opcode: u32, rm: u32, reg: u32, vvvv: u32) -> usize {
            recording_marker(
                $id,
                buf,
                &[opcode as u64, rm as u64, reg as u64, vvvv as u64],
                0,
            )
        }
    };
}

macro_rules! recording_mem {
    ($name:ident, $id:literal, $ty:ty) => {
        fn $name(
            buf: &mut [u8],
            opcode: u32,
            rm: $ty,
            reg: u32,
            vvvv: u32,
            ripoff: usize,
            forcesib: u32,
            disp8scale: i32,
        ) -> usize {
            recording_marker(
                $id,
                buf,
                &[
                    opcode as u64,
                    rm.flags as u64,
                    rm.base.idx as u64,
                    rm.scale as u64,
                    rm.idx.idx as u64,
                    rm.off as i64 as u64,
                    reg as u64,
                    vvvv as u64,
                    ripoff as u64,
                    forcesib as u64,
                    u64::from(disp8scale as u32),
                ],
                ripoff,
            )
        }
    };
}

impl Encode2Runtime for Recorder {
    fn op_imm_n(imm: i64, size: usize) -> bool {
        let result = match size {
            0 => imm == 0,
            1 => imm == i64::from(imm as i8),
            2 => imm == i64::from(imm as i16),
            3 => (imm & 0xff_ffff) == imm,
            4 => imm == i64::from(imm as i32),
            8 => true,
            _ => false,
        };
        recording_event(1, None, &[imm as u64, size as u64], u64::from(result));
        result
    }

    fn enc_seg67(buf: &mut [u8], flags: u32) -> usize {
        let mut result = 0;
        if flags & FE_SEG_MASK != 0 {
            buf[result] = (0x0065_643e_362e_2600_u64 >> (8 * (flags & FE_SEG_MASK))) as u8;
            result += 1;
        }
        if flags & FE_ADDR32 != 0 {
            buf[result] = 0x67;
            result += 1;
        }
        recording_event(2, Some(buf), &[flags as u64], result as u64);
        result
    }

    fn enc_rex_mem(mem: FeMem, reg: u32) -> u32 {
        let val = reg
            | u32::from(mem.flags)
            | (u32::from(mem.base.idx) << 8)
            | (u32::from(mem.idx.idx) << 24);
        let masked = val & 0x0800_0808;
        let result = if masked == 0 {
            0
        } else {
            u32::from((masked.wrapping_mul(1 | (1 << 15) | (1 << 25)) >> 26) as u8) + 0x40
        };
        recording_event(
            3,
            None,
            &[
                mem.flags as u64,
                mem.base.idx as u64,
                mem.scale as u64,
                mem.idx.idx as u64,
                mem.off as i64 as u64,
                reg as u64,
            ],
            result as u64,
        );
        result
    }

    fn enc_imm(buf: &mut [u8], imm: i64, size: usize) {
        assert!(size <= 8 && buf.len() >= size);
        buf[..size].copy_from_slice(&(imm as u64).to_le_bytes()[..size]);
        recording_event(4, Some(buf), &[imm as u64, size as u64], 0);
    }

    fn enc_mem(
        buf: &mut [u8],
        ripoff: usize,
        mem: FeMem,
        reg: u32,
        forcesib: u32,
        disp8scale: i32,
    ) -> usize {
        recording_marker(
            5,
            buf,
            &[
                ripoff as u64,
                mem.flags as u64,
                mem.base.idx as u64,
                mem.scale as u64,
                mem.idx.idx as u64,
                mem.off as i64 as u64,
                reg as u64,
                forcesib as u64,
                u64::from(disp8scale as u32),
            ],
            ripoff,
        )
    }

    fn enc_vex_common(
        buf: &mut [u8],
        opcode: u32,
        base: u32,
        idx: u32,
        reg: u32,
        vvvv: u32,
    ) -> usize {
        recording_marker(
            6,
            buf,
            &[
                opcode as u64,
                base as u64,
                idx as u64,
                reg as u64,
                vvvv as u64,
            ],
            0,
        )
    }

    recording_reg!(enc_vex_reg, 7);
    recording_reg!(enc_evex_reg, 8);
    recording_reg!(enc_evex_xmm, 9);
    recording_mem!(enc_vex_mem, 10, FeMem);
    recording_mem!(enc_evex_mem, 11, FeMem);
    recording_mem!(enc_vex_vsib, 12, FeMemV);
    recording_mem!(enc_evex_vsib, 13, FeMemV);
}

// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{create_bitmask, regs::*};
use tpde_core::{
    calling_convention::{CCAssignment, CCAssignmentPolicy, CCInfo},
    Reg, RegBank,
};
use tpde_util::tpde_assert;

pub const CC_INFO: CCInfo = CCInfo::with_red_zone(
    0xffff_0000_ffff & !create_bitmask(&[BP, SP]),
    create_bitmask(&[BX, R12, R13, R14, R15]),
    create_bitmask(&[
        DI, SI, DX, CX, R8, R9, XMM0, XMM1, XMM2, XMM3, XMM4, XMM5, XMM6, XMM7,
    ]),
    128,
);

#[derive(Clone, Copy, Debug, Default)]
pub struct CCAssignerSysV {
    gp_cnt: u32,
    xmm_cnt: u32,
    stack: u32,
    must_assign_stack: u32,
    vararg: bool,
    ret_gp_cnt: u32,
    ret_xmm_cnt: u32,
}

fn align_up(value: u32, alignment: u32) -> u32 {
    tpde_assert!(alignment.is_power_of_two(), "invalid X64 ABI alignment");
    value
        .checked_add(alignment - 1)
        .expect("X64 ABI stack size overflow")
        & !(alignment - 1)
}

impl CCAssignerSysV {
    pub const INFO: CCInfo = CC_INFO;
    pub const fn new(vararg: bool) -> Self {
        Self {
            gp_cnt: 0,
            xmm_cnt: 0,
            stack: 0,
            must_assign_stack: 0,
            vararg,
            ret_gp_cnt: 0,
            ret_xmm_cnt: 0,
        }
    }
}

impl CCAssignmentPolicy for CCAssignerSysV {
    fn reset(&mut self) {
        *self = Self::new(false);
    }
    fn assign_arg(&mut self, arg: &mut CCAssignment) {
        if arg.byval() {
            self.stack = align_up(self.stack, u32::from(arg.align).max(8));
            arg.stack_off = self.stack;
            self.stack = self
                .stack
                .checked_add(arg.size)
                .expect("X64 ABI stack size overflow");
            return;
        }
        if arg.bank == RegBank::new(0) {
            const REGS: [Reg; 6] = [DI, SI, DX, CX, R8, R9];
            if self.must_assign_stack == 0 && self.gp_cnt + u32::from(arg.consecutive) < 6 {
                arg.reg = REGS[self.gp_cnt as usize];
                self.gp_cnt += 1;
            } else {
                self.must_assign_stack = u32::from(arg.consecutive) + 1;
                self.stack = align_up(self.stack, u32::from(arg.align).max(8));
                arg.stack_off = self.stack;
                self.stack = self
                    .stack
                    .checked_add(8)
                    .expect("X64 ABI stack size overflow");
            }
        } else if arg.bank == RegBank::new(1) {
            if self.must_assign_stack == 0 && self.xmm_cnt < 8 {
                arg.reg = Reg::new(32 + u64::from(self.xmm_cnt));
                self.xmm_cnt += 1;
            } else {
                self.must_assign_stack = u32::from(arg.consecutive) + 1;
                let size = align_up(arg.size, 8);
                self.stack = align_up(self.stack, size);
                arg.stack_off = self.stack;
                self.stack = self
                    .stack
                    .checked_add(size)
                    .expect("X64 ABI stack size overflow");
            }
        } else {
            self.stack = align_up(self.stack, u32::from(arg.align).max(8));
            arg.stack_off = self.stack;
            self.stack = self
                .stack
                .checked_add(align_up(arg.size, 8))
                .expect("X64 ABI stack size overflow");
        }
        self.must_assign_stack = self.must_assign_stack.saturating_sub(1);
    }
    fn get_stack_size(&mut self) -> u32 {
        self.stack
    }
    fn is_vararg(&self) -> bool {
        self.vararg
    }
    fn ret_fits(&self, arg: &CCAssignment) -> bool {
        // The register test of upstream `assign_ret` (CompilerX64.hpp
        // 224-245) without its `TPDE_UNREACHABLE` arms.
        if arg.byval() || arg.sret() {
            false
        } else if arg.bank == RegBank::new(0) {
            self.ret_gp_cnt + u32::from(arg.consecutive) < 2
        } else if arg.bank == RegBank::new(1) {
            self.ret_xmm_cnt + u32::from(arg.consecutive) < 2
        } else {
            false
        }
    }
    fn assign_ret(&mut self, arg: &mut CCAssignment) {
        tpde_assert!(!arg.byval() && !arg.sret());
        if arg.bank == RegBank::new(0) {
            tpde_assert!(
                self.ret_gp_cnt + u32::from(arg.consecutive) < 2,
                "too many return values"
            );
            arg.reg = if self.ret_gp_cnt == 0 { AX } else { DX };
            self.ret_gp_cnt += 1;
        } else if arg.bank == RegBank::new(1) {
            tpde_assert!(
                self.ret_xmm_cnt + u32::from(arg.consecutive) < 2,
                "too many return values"
            );
            arg.reg = if self.ret_xmm_cnt == 0 { XMM0 } else { XMM1 };
            self.ret_xmm_cnt += 1;
        } else {
            panic!("return value must have valid register bank");
        }
    }
}

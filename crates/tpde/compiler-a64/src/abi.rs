// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use tpde_core::{
    calling_convention::{CCAssignment, CCAssignmentPolicy, CCInfo},
    Reg, RegBank,
};
use tpde_util::tpde_assert;

pub const GP_BANK: RegBank = RegBank::new(0);
pub const FP_BANK: RegBank = RegBank::new(1);
/// Registers the allocator never hands out: sp (31), fp (29), the linker's
/// ip0/ip1 (16, 17), and on Darwin the platform register x18, which the
/// kernel does not preserve across a context switch (upstream TPDE targets
/// Linux, where x18 is an ordinary caller-saved register; the port keeps
/// that for byte parity there).
pub const RESERVED_REGS: u64 = (1 << 31)
    | (1 << 29)
    | (1 << 16)
    | (1 << 17)
    | if cfg!(target_os = "macos") { 1 << 18 } else { 0 };
pub const AAPCS_INFO: CCInfo = CCInfo::new(
    u64::MAX & !RESERVED_REGS,
    (0x3ff << 19) | (0xff << 40),
    0x1ff | (0xff << 32),
);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CCAssignerAAPCS {
    pub ngrn: u32,
    pub nsrn: u32,
    pub nsaa: u32,
    pub ret_ngrn: u32,
    pub ret_nsrn: u32,
}
fn align(value: u32, alignment: u32) -> u32 {
    tpde_assert!(
        alignment.is_power_of_two(),
        "AAPCS alignment must be a power of two"
    );
    value
        .checked_add(alignment - 1)
        .expect("AAPCS stack size overflow")
        & !(alignment - 1)
}
impl CCAssignmentPolicy for CCAssignerAAPCS {
    fn reset(&mut self) {
        *self = Self::default();
    }
    fn assign_arg(&mut self, arg: &mut CCAssignment) {
        if arg.byval() {
            self.nsaa = align(self.nsaa, u32::from(arg.align).max(8));
            arg.stack_off = self.nsaa;
            self.nsaa = self
                .nsaa
                .checked_add(arg.size)
                .expect("AAPCS stack size overflow");
            return;
        }
        if arg.sret() {
            arg.reg = Reg::new(8);
            return;
        }
        if arg.bank == GP_BANK {
            if arg.align > 8 {
                self.ngrn = align(self.ngrn, 2);
            }
            if self.ngrn + u32::from(arg.consecutive) < 8 {
                arg.reg = Reg::new(u64::from(self.ngrn));
                self.ngrn += 1;
            } else {
                self.ngrn = 8;
                self.nsaa = align(self.nsaa, u32::from(arg.align).max(8));
                arg.stack_off = self.nsaa;
                self.nsaa = self.nsaa.checked_add(8).expect("AAPCS stack size overflow");
            }
        } else if self.nsrn + u32::from(arg.consecutive) < 8 {
            arg.reg = Reg::new(32 + u64::from(self.nsrn));
            self.nsrn += 1;
        } else {
            self.nsrn = 8;
            let size = align(arg.size, 8);
            self.nsaa = align(self.nsaa, size);
            arg.stack_off = self.nsaa;
            self.nsaa = self
                .nsaa
                .checked_add(size)
                .expect("AAPCS stack size overflow");
        }
    }
    fn get_stack_size(&mut self) -> u32 {
        self.nsaa
    }
    fn ret_fits(&self, arg: &CCAssignment) -> bool {
        // The register test of upstream `assign_ret` (CompilerA64.hpp
        // 263-283) without its `assert(false)` arms.
        if arg.byval() || arg.sret() {
            false
        } else if arg.bank == GP_BANK {
            let ngrn = if arg.align > 8 {
                align(self.ret_ngrn, 2)
            } else {
                self.ret_ngrn
            };
            ngrn + u32::from(arg.consecutive) < 8
        } else {
            self.ret_nsrn + u32::from(arg.consecutive) < 8
        }
    }
    fn assign_ret(&mut self, arg: &mut CCAssignment) {
        tpde_assert!(
            !arg.byval() && !arg.sret(),
            "AAPCS indirect return requires hidden argument"
        );
        if arg.bank == GP_BANK {
            if arg.align > 8 {
                self.ret_ngrn = align(self.ret_ngrn, 2);
            }
            tpde_assert!(
                self.ret_ngrn + u32::from(arg.consecutive) < 8,
                "AAPCS return exceeds GP registers"
            );
            arg.reg = Reg::new(u64::from(self.ret_ngrn));
            self.ret_ngrn += 1;
        } else {
            tpde_assert!(
                self.ret_nsrn + u32::from(arg.consecutive) < 8,
                "AAPCS return exceeds FP registers"
            );
            arg.reg = Reg::new(32 + u64::from(self.ret_nsrn));
            self.ret_nsrn += 1;
        }
    }
}
mcx::forget_safe_nodrop!(CCAssignerAAPCS);

#[cfg(test)]
mod tests {
    use super::*;
    fn assignment(bank: RegBank, size: u32, align: u8, consecutive: u8) -> CCAssignment {
        let mut a = CCAssignment::new();
        a.bank = bank;
        a.size = size;
        a.align = align;
        a.consecutive = consecutive;
        a
    }
    #[test]
    fn aapcs_masks_exclude_reserved_and_preserve_only_declared_registers() {
        let excluded = !AAPCS_INFO.allocatable_regs();
        assert_eq!(excluded, RESERVED_REGS);
        assert_eq!(RESERVED_REGS & !(1 << 18), (1 << 16) | (1 << 17) | (1 << 29) | (1 << 31));
        assert_eq!(RESERVED_REGS & (1 << 18) != 0, cfg!(target_os = "macos"));
        assert_eq!(AAPCS_INFO.callee_saved_regs().count_ones(), 18);
        assert_eq!(AAPCS_INFO.arg_regs().count_ones(), 17);
        assert_eq!(AAPCS_INFO.callee_saved_regs() & AAPCS_INFO.arg_regs(), 0);
        for id in [
            19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 40, 41, 42, 43, 44, 45, 46, 47,
        ] {
            assert_ne!(AAPCS_INFO.callee_saved_regs() & (1 << id), 0);
        }
    }
    #[test]
    fn independent_banks_return_cursors_and_overflow_stack_slots() {
        let mut policy = CCAssignerAAPCS::default();
        for index in 0..10 {
            let mut gp = assignment(GP_BANK, 8, 8, 0);
            let mut fp = assignment(FP_BANK, 16, 16, 0);
            policy.assign_arg(&mut gp);
            policy.assign_arg(&mut fp);
            if index < 8 {
                assert_eq!(gp.reg, Reg::new(index));
                assert_eq!(fp.reg, Reg::new(32 + index));
            } else {
                assert!(gp.reg.invalid() && fp.reg.invalid());
                assert_eq!(gp.stack_off, ((index - 8) * 32) as u32);
                assert_eq!(fp.stack_off, ((index - 8) * 32 + 16) as u32);
            }
        }
        assert_eq!(policy.get_stack_size(), 64);
        let mut ret = assignment(GP_BANK, 8, 8, 0);
        policy.assign_ret(&mut ret);
        assert_eq!(ret.reg, Reg::new(0));
        policy.reset();
        assert_eq!(policy, CCAssignerAAPCS::default());
    }
    #[test]
    fn aligned_multipart_byval_and_sret_follow_assignment_order() {
        let mut policy = CCAssignerAAPCS::default();
        let mut first = assignment(GP_BANK, 8, 8, 0);
        policy.assign_arg(&mut first);
        let mut pair = assignment(GP_BANK, 8, 16, 1);
        policy.assign_arg(&mut pair);
        assert_eq!(pair.reg, Reg::new(2));
        let mut tail = assignment(GP_BANK, 8, 1, 0);
        policy.assign_arg(&mut tail);
        assert_eq!(tail.reg, Reg::new(3));
        let mut byval = assignment(GP_BANK, 17, 32, 0);
        byval.set_byval(true);
        policy.assign_arg(&mut byval);
        assert!(byval.reg.invalid());
        assert_eq!(byval.stack_off, 0);
        let mut sret = assignment(GP_BANK, 8, 8, 0);
        sret.set_sret(true);
        policy.assign_arg(&mut sret);
        assert_eq!(sret.reg, Reg::new(8));
        assert_eq!(policy.ngrn, 4);
        assert_eq!(policy.nsaa, 17);
        let mut oversized = assignment(GP_BANK, 8, 8, 4);
        policy.assign_arg(&mut oversized);
        assert!(oversized.reg.invalid());
        assert_eq!(oversized.stack_off, 24);
        assert_eq!(policy.ngrn, 8);
        let mut later = assignment(GP_BANK, 8, 8, 0);
        policy.assign_arg(&mut later);
        assert_eq!(later.stack_off, 32);
    }
    /// Lane P: `ret_fits` is the register test of upstream `assign_ret`
    /// (CompilerA64.hpp 263-283) without its `assert(false)` arms.  The X64
    /// seed-template shape (`{ i128, i64 }`, three general-purpose parts) fits
    /// AAPCS's x0-x7, so A64 has no gap for it; the ninth part is refused
    /// where `assign_ret` keeps the contract assert.
    #[test]
    fn return_fit_is_queried_before_the_assigner_contract_assert() {
        let mut policy = CCAssignerAAPCS::default();
        for index in 0..8u64 {
            let mut part = assignment(GP_BANK, 8, 8, 0);
            assert!(policy.ret_fits(&part));
            policy.assign_ret(&mut part);
            assert_eq!(part.reg, Reg::new(index));
        }
        assert!(!policy.ret_fits(&assignment(GP_BANK, 8, 8, 0)));
        assert!(policy.ret_fits(&assignment(FP_BANK, 16, 16, 0)));
        for index in 0..8u64 {
            let mut part = assignment(FP_BANK, 16, 16, 0);
            assert!(policy.ret_fits(&part));
            policy.assign_ret(&mut part);
            assert_eq!(part.reg, Reg::new(32 + index));
        }
        assert!(!policy.ret_fits(&assignment(FP_BANK, 16, 16, 0)));

        let mut aligned = CCAssignerAAPCS::default();
        for _ in 0..7 {
            aligned.assign_ret(&mut assignment(GP_BANK, 8, 8, 0));
        }
        assert!(aligned.ret_fits(&assignment(GP_BANK, 8, 8, 0)));
        assert!(!aligned.ret_fits(&assignment(GP_BANK, 16, 16, 0)));
        let mut pair = assignment(GP_BANK, 8, 8, 1);
        assert!(!aligned.ret_fits(&pair));
        aligned.reset();
        assert!(aligned.ret_fits(&pair));
        aligned.assign_ret(&mut pair);
        let mut byval = assignment(GP_BANK, 8, 8, 0);
        byval.set_byval(true);
        assert!(!aligned.ret_fits(&byval));
    }
    #[test]
    fn assign_ret_keeps_the_upstream_contract_assert() {
        if !tpde_util::CHECKED {
            return;
        }
        let failure = std::panic::catch_unwind(|| {
            let mut policy = CCAssignerAAPCS::default();
            for _ in 0..9 {
                policy.assign_ret(&mut assignment(GP_BANK, 8, 8, 0));
            }
        })
        .expect_err("assign_ret keeps the upstream contract assert");
        let message = failure
            .downcast_ref::<std::string::String>()
            .map(|s| s.as_str())
            .or_else(|| failure.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            message.contains("AAPCS return exceeds GP registers"),
            "{message}"
        );
    }
}

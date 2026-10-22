// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{Reg, RegBank};

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CCAssignment {
    pub reg: Reg,
    pub consecutive: u8,
    flags: u8,
    pub int_ext: u8,
    pub align: u8,
    pub bank: RegBank,
    pub size: u32,
    pub stack_off: u32,
}

impl CCAssignment {
    pub const fn new() -> Self {
        Self {
            reg: Reg::make_invalid(),
            consecutive: 0,
            flags: 0,
            int_ext: 0,
            align: 0,
            bank: RegBank::new(u8::MAX),
            size: 0,
            stack_off: 0,
        }
    }

    pub const fn sret(&self) -> bool {
        self.flags & 1 != 0
    }

    pub fn set_sret(&mut self, value: bool) {
        self.flags = (self.flags & !1) | u8::from(value);
    }

    pub const fn byval(&self) -> bool {
        self.flags & 2 != 0
    }

    pub fn set_byval(&mut self, value: bool) {
        self.flags = (self.flags & !2) | (u8::from(value) << 1);
    }
}

impl Default for CCAssignment {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CCInfo {
    allocatable_regs: u64,
    callee_saved_regs: u64,
    arg_regs: u64,
    red_zone_size: u8,
}

impl CCInfo {
    pub const fn new(allocatable_regs: u64, callee_saved_regs: u64, arg_regs: u64) -> Self {
        Self::with_red_zone(allocatable_regs, callee_saved_regs, arg_regs, 0)
    }

    pub const fn with_red_zone(
        allocatable_regs: u64,
        callee_saved_regs: u64,
        arg_regs: u64,
        red_zone_size: u8,
    ) -> Self {
        Self {
            allocatable_regs,
            callee_saved_regs,
            arg_regs,
            red_zone_size,
        }
    }

    pub const fn allocatable_regs(&self) -> u64 {
        self.allocatable_regs
    }

    pub const fn callee_saved_regs(&self) -> u64 {
        self.callee_saved_regs
    }

    pub const fn arg_regs(&self) -> u64 {
        self.arg_regs
    }

    pub const fn red_zone_size(&self) -> u8 {
        self.red_zone_size
    }
}

pub trait CCAssignmentPolicy {
    fn reset(&mut self);
    fn assign_arg(&mut self, assignment: &mut CCAssignment);
    fn get_stack_size(&mut self) -> u32;
    fn is_vararg(&self) -> bool {
        false
    }
    fn assign_ret(&mut self, assignment: &mut CCAssignment);
    /// Whether `assign_ret` can place `assignment` from the current state.
    /// `false` exactly where the upstream assigner's `assign_ret` reaches
    /// `TPDE_UNREACHABLE` ("too many return values"); `assign_ret` keeps
    /// that contract check, so a compiler asks here first and refuses the
    /// function instead of tripping it.
    fn ret_fits(&self, assignment: &CCAssignment) -> bool {
        let _ = assignment;
        true
    }
}

#[doc = include_str!("../tests/compiler-contracts-ownership.md")]
pub struct CCAssigner<'cc, P: CCAssignmentPolicy> {
    pub ccinfo: &'cc CCInfo,
    policy: P,
}

impl<'cc, P: CCAssignmentPolicy> CCAssigner<'cc, P> {
    pub const fn new(ccinfo: &'cc CCInfo, policy: P) -> Self {
        const {
            assert!(
                !core::mem::needs_drop::<P>(),
                "CC assigner policy must not need drop"
            )
        };
        Self { ccinfo, policy }
    }

    pub const fn get_ccinfo(&self) -> &'cc CCInfo {
        self.ccinfo
    }

    pub const fn policy(&self) -> &P {
        &self.policy
    }

    pub fn policy_mut(&mut self) -> &mut P {
        &mut self.policy
    }
}

mod sealed {
    pub trait Assigner {}
}

pub trait CallingConventionAssigner: sealed::Assigner {
    fn get_ccinfo(&self) -> &CCInfo;
    fn reset(&mut self);
    fn assign_arg(&mut self, assignment: &mut CCAssignment);
    fn get_stack_size(&mut self) -> u32;
    fn is_vararg(&self) -> bool;
    fn assign_ret(&mut self, assignment: &mut CCAssignment);
    /// [`CCAssignmentPolicy::ret_fits`] of the wrapped policy.
    fn ret_fits(&self, assignment: &CCAssignment) -> bool;
}

impl<P: CCAssignmentPolicy> sealed::Assigner for CCAssigner<'_, P> {}

impl<P: CCAssignmentPolicy> CallingConventionAssigner for CCAssigner<'_, P> {
    fn get_ccinfo(&self) -> &CCInfo {
        self.ccinfo
    }

    fn reset(&mut self) {
        self.policy.reset();
    }

    fn assign_arg(&mut self, assignment: &mut CCAssignment) {
        self.policy.assign_arg(assignment);
    }

    fn get_stack_size(&mut self) -> u32 {
        self.policy.get_stack_size()
    }

    fn is_vararg(&self) -> bool {
        self.policy.is_vararg()
    }

    fn assign_ret(&mut self, assignment: &mut CCAssignment) {
        self.policy.assign_ret(assignment);
    }

    fn ret_fits(&self, assignment: &CCAssignment) -> bool {
        self.policy.ret_fits(assignment)
    }
}

mcx::forget_safe_nodrop!(CCAssignment, CCInfo);

const _: () = {
    assert!(core::mem::size_of::<CCAssignment>() == 16);
    assert!(core::mem::size_of::<CCInfo>() == 32);
    assert!(core::mem::offset_of!(CCAssignment, size) == 8);
    assert!(core::mem::offset_of!(CCAssignment, stack_off) == 12);
};

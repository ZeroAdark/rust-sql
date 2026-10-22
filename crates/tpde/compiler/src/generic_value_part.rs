// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
use tpde_core::Reg;
use tpde_util::{tpde_assert, tpde_assert_eq};
use types_error::PgResult;

use crate::{guard_machine::GuardMachine, scratch_reg::ScratchReg, value_part_ref::ValuePartRef};

pub enum ExprReg<'c, M: GuardMachine<N>, const N: usize> {
    Borrowed(Reg),
    Scratch(ScratchReg<'c, M, N>),
}

impl<M: GuardMachine<N>, const N: usize> ExprReg<'_, M, N> {
    pub fn reg(&self) -> Reg {
        match self {
            Self::Borrowed(reg) => *reg,
            Self::Scratch(scratch) => scratch.cur_reg(),
        }
    }
}

pub struct Expr<'c, M: GuardMachine<N>, const N: usize> {
    pub base: ExprReg<'c, M, N>,
    pub index: ExprReg<'c, M, N>,
    pub scale: i64,
    pub disp: i64,
}

impl<'c, M: GuardMachine<N>, const N: usize> Expr<'c, M, N> {
    pub const fn new() -> Self {
        Self {
            base: ExprReg::Borrowed(Reg::make_invalid()),
            index: ExprReg::Borrowed(Reg::make_invalid()),
            scale: 0,
            disp: 0,
        }
    }

    pub fn from_reg(base: Reg, disp: i64) -> Self {
        Self {
            base: ExprReg::Borrowed(base),
            disp,
            ..Self::new()
        }
    }

    pub fn from_scratch(base: ScratchReg<'c, M, N>, disp: i64) -> Self {
        Self {
            base: ExprReg::Scratch(base),
            disp,
            ..Self::new()
        }
    }

    pub fn base_reg(&self) -> Reg {
        self.base.reg()
    }

    pub fn has_base(&self) -> bool {
        match &self.base {
            ExprReg::Borrowed(reg) => reg.valid(),
            ExprReg::Scratch(_) => true,
        }
    }

    pub fn index_reg(&self) -> Reg {
        tpde_assert!(self.scale != 0, "expression index is not active");
        tpde_assert!(self.scale != 1 || self.has_base());
        self.index.reg()
    }

    pub fn has_index(&self) -> bool {
        self.scale != 0
    }
}

impl<M: GuardMachine<N>, const N: usize> Default for Expr<'_, M, N> {
    fn default() -> Self {
        Self::new()
    }
}

pub enum GenericValuePart<'c, M: GuardMachine<N>, const N: usize> {
    Empty,
    Value(ValuePartRef<'c, M, N>),
    Scratch(ScratchReg<'c, M, N>),
    Expr(Expr<'c, M, N>),
}

impl<'c, M: GuardMachine<N>, const N: usize> GenericValuePart<'c, M, N> {
    pub const fn new() -> Self {
        Self::Empty
    }

    pub fn is_expr(&self) -> bool {
        matches!(self, Self::Expr(_))
    }

    pub fn is_imm(&self) -> bool {
        matches!(self, Self::Value(value) if value.is_const())
    }

    pub fn imm_size(&self) -> u32 {
        tpde_assert!(self.is_imm());
        self.val_ref().part_size()
    }

    pub fn imm64(&self) -> u64 {
        tpde_assert!(self.imm_size() <= 8);
        self.val_ref().const_data()[0]
    }

    pub fn val_ref(&self) -> &ValuePartRef<'c, M, N> {
        let Self::Value(value) = self else {
            panic!("generic value does not contain a value reference");
        };
        value
    }

    pub fn val_ref_mut(&mut self) -> &mut ValuePartRef<'c, M, N> {
        let Self::Value(value) = self else {
            panic!("generic value does not contain a value reference");
        };
        value
    }

    pub fn reset(&mut self) {
        *self = Self::Empty;
    }

    pub fn take(&mut self) -> Self {
        core::mem::take(self)
    }
}

impl<'c, M: GuardMachine<N>, const N: usize> From<ScratchReg<'c, M, N>>
    for GenericValuePart<'c, M, N>
{
    fn from(scratch: ScratchReg<'c, M, N>) -> Self {
        tpde_assert!(scratch.has_reg());
        Self::Scratch(scratch)
    }
}

impl<'c, M: GuardMachine<N>, const N: usize> From<ValuePartRef<'c, M, N>>
    for GenericValuePart<'c, M, N>
{
    fn from(value: ValuePartRef<'c, M, N>) -> Self {
        Self::Value(value)
    }
}

impl<'c, M: GuardMachine<N>, const N: usize> From<Expr<'c, M, N>> for GenericValuePart<'c, M, N> {
    fn from(expr: Expr<'c, M, N>) -> Self {
        Self::Expr(expr)
    }
}

impl<M: GuardMachine<N>, const N: usize> Default for GenericValuePart<'_, M, N> {
    fn default() -> Self {
        Self::new()
    }
}

pub fn gval_as_reg<'c, M: GuardMachine<N>, const N: usize>(
    machine: &'c M,
    value: &mut GenericValuePart<'c, M, N>,
) -> PgResult<Reg> {
    match value {
        GenericValuePart::Scratch(scratch) => Ok(scratch.cur_reg()),
        GenericValuePart::Value(value) => {
            if value.has_reg() {
                Ok(value.cur_reg())
            } else {
                value.load_to_reg()
            }
        }
        GenericValuePart::Expr(expr) if expr.has_base() && !expr.has_index() && expr.disp == 0 => {
            Ok(expr.base_reg())
        }
        GenericValuePart::Expr(_) => machine.gval_expr_as_reg(value),
        GenericValuePart::Empty => panic!("cannot load an empty generic value"),
    }
}

pub fn gval_as_reg_reuse_scratch<'c, M: GuardMachine<N>, const N: usize>(
    machine: &'c M,
    value: &mut GenericValuePart<'c, M, N>,
    destination: &mut ScratchReg<'c, M, N>,
) -> PgResult<Reg> {
    let reg = gval_as_reg(machine, value)?;
    if !destination.has_reg() {
        match value {
            GenericValuePart::Scratch(scratch) => *destination = scratch.take(),
            GenericValuePart::Value(value) if value.can_salvage() => {
                destination.alloc_specific(value.salvage())?;
                tpde_assert_eq!(destination.cur_reg(), reg, "value salvage changed register");
            }
            _ => {}
        }
    }
    Ok(reg)
}

pub fn gval_as_reg_reuse_value<'c, M: GuardMachine<N>, const N: usize>(
    machine: &'c M,
    value: &mut GenericValuePart<'c, M, N>,
    destination: &mut ValuePartRef<'c, M, N>,
) -> PgResult<Reg> {
    let reg = gval_as_reg(machine, value)?;
    if !destination.has_reg()
        && (!destination.has_assignment()
            || !destination.with_assignment(|assignment| assignment.fixed_assignment()))
    {
        let transferred = match value {
            GenericValuePart::Scratch(scratch) => {
                destination.set_scratch(scratch.take())?;
                true
            }
            GenericValuePart::Value(value) if value.can_salvage() => {
                destination.set_value_from(value)?;
                true
            }
            _ => false,
        };
        if transferred && destination.has_assignment() {
            destination.lock();
        }
    }
    Ok(reg)
}

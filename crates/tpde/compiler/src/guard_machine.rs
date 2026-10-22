// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use tpde_base::ValLocalIdx;
use tpde_core::{
    compiler::ValRefSpecialStruct, value_assignment::Assignment, AssignmentPartRef, BlockIndex,
    LivenessInfo, Reg, RegBank, RegisterFile,
};
use types_error::PgResult;

pub const INVALID_VALUE: ValLocalIdx = ValLocalIdx::new(u32::MAX);

/// A register-allocation event the compiler emitted code for, reported
/// to the target per block (`GuardMachine::regalloc_event`,
/// `CompilerTarget::regalloc_event`): the frame traffic and phi moves a
/// frontend accounts per block when it measures its own programs
/// (`docs/design/gravity-simple/tpde-fixedregs.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegallocEvent {
    /// A value's register stored to its frame slot (`spill_part`: before
    /// a branch, an eviction, a demotion).
    Spill,
    /// A value reloaded from its frame slot into a register for a use.
    Reload,
    /// A `mov` into a fixed or pinned phi register at an edge.
    PhiMove,
    /// A reload from the incoming value's slot straight into a phi register.
    PhiReload,
    /// A stack phi's slot written at an edge.
    PhiStore,
    /// A fixed register saved to its slot around a call (`call_spill_part`).
    CallSave,
    /// ... and reloaded after the call.
    CallRestore,
    /// A fixed holder demoted to an ordinary register-valid value (spilled)
    /// so a heavier value takes its register (`init_assignment`).
    Demote,
    /// The branch region of a terminator begins: from here to `RegionEnd`
    /// the code is edge code (phi moves), emitted on one edge only, and no
    /// value may change state (`may_change_value_state`).
    RegionBegin,
    RegionEnd,
    /// The allocator's verifier (D117): a block entered other than as the only-entry successor of the block laid out before
    /// it begins with a register still assumed to hold a non-fixed value — a state its other paths never wrote.
    StaleEntry,
    /// D398: a fixed register lent as an instruction's scratch (`select_reg` found every register of the bank locked): its
    /// holder is saved before and reloaded after the instruction.
    Lend,
}

pub trait GuardMachine<const N: usize>: Sized {
    type Special: ValRefSpecialStruct;
    const GP_BANK: RegBank;

    fn with_registers<R>(&self, operation: impl FnOnce(&RegisterFile<N, 32>) -> R) -> R;
    fn with_assignment<R>(
        &self,
        local: ValLocalIdx,
        operation: impl FnOnce(&Assignment<'_, '_>) -> R,
    ) -> R;
    fn with_part<R>(
        &self,
        local: ValLocalIdx,
        part: u32,
        operation: impl FnOnce(AssignmentPartRef<'_, '_, '_>) -> R,
    ) -> R;
    fn liveness(&self, local: ValLocalIdx) -> LivenessInfo;
    fn current_block(&self) -> BlockIndex;
    /// Code for a register-allocation event was emitted in the current
    /// block (see `RegallocEvent`). Default: not accounted.
    fn regalloc_event(&self, event: RegallocEvent) {
        let _ = event;
    }
    fn may_change_value_state(&self) -> bool;
    fn release_assignment(&self, local: ValLocalIdx);
    fn decrement_fixed_count(&self, bank: RegBank);
    /// A pinned (coalesced-phi) fixed assignment released its register.
    fn decrement_pinned_count(&self, bank: RegBank) {
        let _ = bank;
    }
    /// `local`'s fixed register belongs to a phi (`shadow_phi`): it is
    /// never salvaged and freeing the local does not release it.
    fn shadow_assignment(&self, local: ValLocalIdx) -> bool {
        let _ = local;
        false
    }
    fn select_reg(&self, bank: RegBank) -> PgResult<Reg>;
    fn evict_reg(&self, reg: Reg) -> PgResult<()>;
    fn reload_to_reg(&self, reg: Reg, local: ValLocalIdx, part: u32) -> PgResult<()>;
    /// Emits without allocating or evicting registers; failure abandons the function's output.
    fn mov(&self, destination: Reg, source: Reg, size: u32) -> PgResult<()>;
    fn materialize_constant(
        &self,
        words: &[u64],
        bank: RegBank,
        size: u32,
        reg: Reg,
    ) -> PgResult<()>;
    fn generate_raw_intext(
        &self,
        destination: Reg,
        source: Reg,
        sign: bool,
        from: u32,
        to: u32,
    ) -> PgResult<()>;
    fn gval_expr_as_reg<'c>(
        &'c self,
        value: &mut crate::generic_value_part::GenericValuePart<'c, Self, N>,
    ) -> PgResult<Reg>;
    fn special_part<'s>(
        &'s self,
        special: &'s Self::Special,
        part: u32,
    ) -> crate::value_part_ref::ValuePartRef<'s, Self, N>;
}

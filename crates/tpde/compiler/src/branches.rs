// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use alloc::boxed::Box;
use mcx::Mcx;
use tpde_base::ValLocalIdx;
use tpde_core::{
    ir_adaptor::{IrAdaptor, PhiRef},
    BlockIndex, Reg,
};
use tpde_function_writer::Label;
use tpde_util::{misc::BitSetIterator, tpde_assert, tpde_assert_eq, SmallVectorGuard};
use types_error::{PgError, PgResult};

use crate::{
    guard_machine::{GuardMachine, RegallocEvent, INVALID_VALUE},
    phi_guard::{BranchRegionGuard, PhiScratchReg, TempStackGuard},
    scratch_reg::ScratchReg,
    value_ref::ValueRef,
};

pub type BranchValue<M, const N: usize> = <<M as BranchMachine<N>>::Adaptor as IrAdaptor>::ValueRef;
pub type BranchBlock<M, const N: usize> = <<M as BranchMachine<N>>::Adaptor as IrAdaptor>::BlockRef;

pub trait BranchMachine<const N: usize>: GuardMachine<N> {
    type Adaptor: IrAdaptor<ValueRef: Copy, BlockRef: Copy>;
    type Jump: Copy;
    fn adaptor(&self) -> &Self::Adaptor;
    fn block_idx(&self, block: BranchBlock<Self, N>) -> BlockIndex;
    fn block_ref(&self, index: BlockIndex) -> BranchBlock<Self, N>;
    fn block_has_phis(&self, index: BlockIndex) -> bool;
    fn block_has_multiple_incoming(&self, block: BranchBlock<Self, N>) -> bool;
    /// Phi coalescing at the edge is on (`CompilerTarget::PHI_COALESCING`).
    fn coalesces_phis(&self) -> bool {
        false
    }
    /// At the anchor edge of `phi`'s block: create `phi`'s assignment pinned
    /// to a register. `Ok(true)`: the incoming value's register was taken
    /// over, no move needed. `Ok(false)`: either no assignment was created
    /// (classic path) or it was pinned to a fresh register to move into.
    fn anchor_phi(
        &self,
        phi: BranchValue<Self, N>,
        incoming: BranchValue<Self, N>,
    ) -> PgResult<bool> {
        let _ = (phi, incoming);
        Ok(false)
    }
    /// Spill weight of a value, for ordering phis at an anchor edge.
    fn phi_weight(&self, value: BranchValue<Self, N>) -> u32 {
        let _ = value;
        0
    }
    fn block_label(&self, index: BlockIndex) -> Label;
    fn label_create(&self) -> PgResult<Label>;
    fn label_place(&self, label: Label) -> PgResult<()>;
    fn align_text(&self, alignment: u32) -> PgResult<()>;
    fn unconditional_jump(&self) -> Self::Jump;
    fn invert_jump(&self, jump: Self::Jump) -> Self::Jump;
    fn generate_raw_jump(&self, jump: Self::Jump, label: Label) -> PgResult<()>;
    fn spill_part(&self, local: ValLocalIdx, part: u32) -> PgResult<()>;
    fn free_reg(&self, reg: Reg);
    fn allocate_spill_part(&self, local: ValLocalIdx, part: u32) -> PgResult<()>;
    fn load_from_stack(&self, reg: Reg, offset: i32, size: u32) -> PgResult<()>;
    fn spill_reg(&self, reg: Reg, offset: i32, size: u32) -> PgResult<()>;
    fn allocate_stack_slot(&self, size: u32) -> PgResult<i32>;
    fn free_stack_slot(&self, offset: i32, size: u32);
    /// `free_stack_slot(offset, freed_size)` of a slot allocated with
    /// `allocated_size` (see `FrameState::free_reclassified`).
    fn free_stack_slot_reclassified(
        &self,
        offset: i32,
        allocated_size: u32,
        freed_size: u32,
    ) -> PgResult<()>;
    fn value_ref(&self, value: BranchValue<Self, N>) -> PgResult<ValueRef<'_, Self, N>>;
    fn result_ref(&self, value: BranchValue<Self, N>) -> PgResult<ValueRef<'_, Self, N>>;
    fn assignment_exists(&self, local: ValLocalIdx) -> bool;
    fn set_branch_region(&self, active: bool);
    fn record_failure(&self, error: Box<PgError>);
    fn with_phi_scratch<R>(
        &self,
        operation: impl FnOnce(&mut PhiScratch<'_, BranchValue<Self, N>>) -> PgResult<R>,
    ) -> PgResult<R>;
    fn with_switch_scratch<R>(
        &self,
        operation: impl FnOnce(&mut SwitchScratch<'_, BranchBlock<Self, N>>) -> PgResult<R>,
    ) -> PgResult<R>;
    fn switch_emit_cmpeq(
        &self,
        label: Label,
        cmp: Reg,
        tmp: Reg,
        value: u64,
        width32: bool,
    ) -> PgResult<()>;
    fn switch_emit_binary_step(
        &self,
        equal: Label,
        greater: Label,
        cmp: Reg,
        tmp: Reg,
        value: u64,
        width32: bool,
    ) -> PgResult<()>;
    fn switch_create_jump_table(
        &self,
        default: Label,
        cmp: Reg,
        tmp: Reg,
        low: u64,
        high: u64,
        width32: bool,
        fill: impl FnOnce(&mut [Label]),
    ) -> PgResult<bool>;
}

#[derive(Clone, Copy)]
struct NodeEntry<V> {
    phi: V,
    incoming: V,
    local: ValLocalIdx,
    incoming_local: ValLocalIdx,
    references: u32,
}

pub struct PhiScratch<'m, V: Copy> {
    nodes: SmallVectorGuard<'m, NodeEntry<V>, 16>,
    ready: SmallVectorGuard<'m, u32, 32>,
    waiting: SmallVectorGuard<'m, u64, 4>,
}
impl<'m, V: Copy> PhiScratch<'m, V> {
    pub fn new(mcx: Mcx<'m>) -> Self {
        Self {
            nodes: SmallVectorGuard::new(mcx),
            ready: SmallVectorGuard::new(mcx),
            waiting: SmallVectorGuard::new(mcx),
        }
    }
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.ready.clear();
        self.waiting.clear();
    }
    fn set_waiting(&mut self, index: usize, value: bool) {
        let word = &mut self.waiting[index / 64];
        if value {
            *word |= 1 << (index % 64);
        } else {
            *word &= !(1 << (index % 64));
        }
    }
    fn first_waiting(&self) -> Option<usize> {
        self.waiting
            .iter()
            .enumerate()
            .find_map(|(i, word)| (*word != 0).then(|| i * 64 + word.trailing_zeros() as usize))
    }
}

pub struct SwitchScratch<'m, B: Copy> {
    labels: SmallVectorGuard<'m, Label, 64>,
    blocks: SmallVectorGuard<'m, (Label, B), 64>,
}
impl<'m, B: Copy> SwitchScratch<'m, B> {
    pub fn new(mcx: Mcx<'m>) -> Self {
        Self {
            labels: SmallVectorGuard::new(mcx),
            blocks: SmallVectorGuard::new(mcx),
        }
    }
    pub fn clear(&mut self) {
        self.labels.clear();
        self.blocks.clear();
    }
}

pub fn next_block<M: BranchMachine<N>, const N: usize>(machine: &M) -> BlockIndex {
    BlockIndex(machine.current_block().0.wrapping_add(1))
}

pub fn spill_before_branch<M: BranchMachine<N>, const N: usize>(
    machine: &M,
    force: bool,
) -> PgResult<u64> {
    tpde_assert!(machine.may_change_value_state());
    let current = machine.current_block();
    let mut earliest = BlockIndex(u32::MAX);
    let mut must_spill = force;
    if !force {
        let mut next_is_successor = false;
        let mut next_has_multiple_incoming = false;
        let mut successors = 0u32;
        for block in machine.adaptor().block_succs(machine.block_ref(current)) {
            successors = successors.wrapping_add(1);
            let index = machine.block_idx(block);
            if index == next_block(machine) {
                next_is_successor = true;
                next_has_multiple_incoming |= machine.block_has_multiple_incoming(block);
            } else if index > current && index < earliest {
                earliest = index;
            }
        }
        must_spill = !next_is_successor || next_has_multiple_incoming;
        if successors == 1 && !must_spill {
            return Ok(0);
        }
    }
    let mut release = 0;
    let used = machine.with_registers(|registers| registers.used.get());
    for id in BitSetIterator::<false>::new(used) {
        let reg = Reg::new(id);
        let (local, part) = machine
            .with_registers(|registers| (registers.reg_local_idx(reg), registers.reg_part(reg)));
        if local == INVALID_VALUE {
            continue;
        }
        let (fixed, modified, variable) = machine.with_part(local, part, |ap| {
            (ap.fixed_assignment(), ap.modified(), ap.variable_ref())
        });
        if fixed {
            continue;
        }
        if must_spill {
            release |= 1 << id;
        }
        if !modified || variable {
            continue;
        }
        let last = machine.liveness(local).last;
        if last <= current {
            continue;
        }
        if must_spill || earliest <= last {
            machine.spill_part(local, part)?;
        }
    }
    Ok(release)
}

/// After a branch whose jump read `reg` through a reference the caller held across `generate_cond_branch` (a `cbz` / `cbnz`
/// operand): that lock kept `release_spilled_regs` from freeing the register, so a block laid out next that is not the
/// branch's only-entry successor would start believing the value is still in it — on a path that never wrote it (D117:
/// a constant materialized for the jump in a non-dominating block, read as a phi source two blocks later). Called once the
/// reference is dropped; frees `reg` when the branch had to release its registers and nothing else holds it.
pub fn release_jump_operand<M: BranchMachine<N>, const N: usize>(machine: &M, reg: Reg) {
    tpde_assert!(machine.may_change_value_state());
    let current = machine.current_block();
    let next = next_block(machine);
    let mut next_is_successor = false;
    let mut next_has_multiple_incoming = false;
    for block in machine.adaptor().block_succs(machine.block_ref(current)) {
        if machine.block_idx(block) == next {
            next_is_successor = true;
            next_has_multiple_incoming |= machine.block_has_multiple_incoming(block);
        }
    }
    if next_is_successor && !next_has_multiple_incoming {
        return;
    }
    if !machine.with_registers(|r| r.is_used(reg) && !r.is_fixed(reg)) {
        return;
    }
    let (local, part) = machine.with_registers(|r| (r.reg_local_idx(reg), r.reg_part(reg)));
    if local == INVALID_VALUE {
        return;
    }
    if machine.with_part(local, part, |ap| ap.fixed_assignment()) {
        return;
    }
    machine.free_reg(reg);
}

pub fn release_spilled_regs<M: BranchMachine<N>, const N: usize>(machine: &M, regs: u64) {
    tpde_assert!(machine.may_change_value_state());
    let used = machine.with_registers(|registers| registers.used.get());
    for id in BitSetIterator::<false>::new(regs & used) {
        let reg = Reg::new(id);
        if !machine.with_registers(|registers| registers.is_fixed(reg)) {
            machine.free_reg(reg);
        }
    }
}

pub fn release_regs_after_return<M: BranchMachine<N>, const N: usize>(machine: &M) {
    let used = machine.with_registers(|registers| registers.used.get());
    for id in BitSetIterator::<false>::new(used) {
        let reg = Reg::new(id);
        if !machine.with_registers(|registers| registers.is_fixed(reg)) {
            machine.free_reg(reg);
        }
    }
}

pub fn branch_needs_split<M: BranchMachine<N>, const N: usize>(
    machine: &M,
    target: BranchBlock<M, N>,
) -> bool {
    if !machine.block_has_phis(machine.block_idx(target)) {
        return false;
    }
    // An edge on which every phi receives itself (a join forwarded to its
    // successor's phi, a loop-carried value this arm leaves alone) moves
    // nothing: the moves before the jump emit no code, so no split.
    let adaptor = machine.adaptor();
    let current = machine.block_ref(machine.current_block());
    !adaptor.block_phis(target).into_iter().all(|phi| {
        let incoming = adaptor.val_as_phi(phi).incoming_val_for_block(current);
        adaptor.val_const(incoming).is_none()
            && adaptor.val_local_idx(phi) == adaptor.val_local_idx(incoming)
    })
}

pub fn generate_branch_to_block<M: BranchMachine<N>, const N: usize>(
    machine: &M,
    jump: M::Jump,
    target: BranchBlock<M, N>,
    needs_split: bool,
    last_inst: bool,
) -> PgResult<()> {
    let index = machine.block_idx(target);
    let label = machine.block_label(index);
    if !needs_split {
        move_to_phi_nodes(machine, index)?;
        if !last_inst || index != next_block(machine) {
            machine.generate_raw_jump(jump, label)?;
        }
    } else {
        let temporary = machine.label_create()?;
        machine.generate_raw_jump(machine.invert_jump(jump), temporary)?;
        move_to_phi_nodes(machine, index)?;
        machine.generate_raw_jump(machine.unconditional_jump(), label)?;
        machine.label_place(temporary)?;
    }
    Ok(())
}

pub fn generate_uncond_branch<M: BranchMachine<N>, const N: usize>(
    machine: &M,
    target: BranchBlock<M, N>,
) -> PgResult<()> {
    let spilled = spill_before_branch(machine, false)?;
    {
        let _region = BranchRegionGuard::new(machine);
        generate_branch_to_block(machine, machine.unconditional_jump(), target, false, true)?;
    }
    release_spilled_regs(machine, spilled);
    Ok(())
}

pub fn generate_cond_branch<M: BranchMachine<N>, const N: usize>(
    machine: &M,
    jump: M::Jump,
    true_target: BranchBlock<M, N>,
    false_target: BranchBlock<M, N>,
) -> PgResult<()> {
    let next = machine.block_ref(next_block(machine));
    let true_split = branch_needs_split(machine, true_target);
    let false_split = branch_needs_split(machine, false_target);
    let spilled = spill_before_branch(machine, false)?;
    {
        let _region = BranchRegionGuard::new(machine);
        if next == true_target || (next != false_target && true_split) {
            generate_branch_to_block(
                machine,
                machine.invert_jump(jump),
                false_target,
                false_split,
                false,
            )?;
            generate_branch_to_block(
                machine,
                machine.unconditional_jump(),
                true_target,
                false,
                true,
            )?;
        } else if next == false_target {
            generate_branch_to_block(machine, jump, true_target, true_split, false)?;
            generate_branch_to_block(
                machine,
                machine.unconditional_jump(),
                false_target,
                false,
                true,
            )?;
        } else {
            tpde_assert!(!true_split);
            generate_branch_to_block(machine, jump, true_target, false, false)?;
            generate_branch_to_block(
                machine,
                machine.unconditional_jump(),
                false_target,
                false,
                true,
            )?;
        }
    }
    release_spilled_regs(machine, spilled);
    Ok(())
}

struct SwitchRange<'a, M: BranchMachine<N>, const N: usize> {
    machine: &'a M,
    cases: &'a [(u64, BranchBlock<M, N>)],
    labels: &'a [Label],
    default: Label,
    cmp: Reg,
    tmp: Reg,
    width32: bool,
}
impl<M: BranchMachine<N>, const N: usize> SwitchRange<'_, M, N> {
    fn build(&self, begin: usize, end: usize) -> PgResult<()> {
        tpde_assert!(begin <= end);
        let count = end - begin;
        if count <= 4 {
            for i in begin..end {
                self.machine.switch_emit_cmpeq(
                    self.labels[i],
                    self.cmp,
                    self.tmp,
                    self.cases[i].0,
                    self.width32,
                )?;
            }
            return self
                .machine
                .generate_raw_jump(self.machine.unconditional_jump(), self.default);
        }
        let low = self.cases[begin].0;
        let high = self.cases[end - 1].0;
        let range = high.wrapping_sub(low).wrapping_add(1);
        if range != 0 && range / (count as u64) < 8 {
            let made = self.machine.switch_create_jump_table(
                self.default,
                self.cmp,
                self.tmp,
                low,
                high,
                self.width32,
                |labels| {
                    tpde_assert_eq!(labels.len() as u64, range);
                    if range == count as u64 {
                        labels.copy_from_slice(&self.labels[begin..end]);
                    } else {
                        labels.fill(self.default);
                        for i in begin..end {
                            labels[(self.cases[i].0 - low) as usize] = self.labels[i];
                        }
                    }
                },
            )?;
            if made {
                return Ok(());
            }
        }
        let middle = begin + count / 2;
        let greater = self.machine.label_create()?;
        self.machine.switch_emit_binary_step(
            self.labels[middle],
            greater,
            self.cmp,
            self.tmp,
            self.cases[middle].0,
            self.width32,
        )?;
        self.build(begin, middle)?;
        self.machine.label_place(greater)?;
        self.build(middle + 1, end)
    }
}

pub fn generate_switch<M: BranchMachine<N>, const N: usize>(
    machine: &M,
    condition: ScratchReg<'_, M, N>,
    width: u32,
    default_block: BranchBlock<M, N>,
    cases: &[(u64, BranchBlock<M, N>)],
) -> PgResult<()> {
    tpde_assert!(
        core::ptr::eq(machine, condition.machine()),
        "switch condition belongs to another compiler"
    );
    tpde_assert!(width <= 64);
    tpde_assert!(
        cases.len() < u32::MAX as usize,
        "large switches are unsupported"
    );
    let cmp = condition.cur_reg();
    let rounded = width.wrapping_add(31) & !31;
    if width != rounded {
        machine.generate_raw_intext(cmp, cmp, false, width, rounded)?;
    }
    let mut temporary = ScratchReg::new(machine);
    let tmp = temporary.alloc_gp()?;
    let spilled = spill_before_branch(machine, false)?;
    {
        let _region = BranchRegionGuard::new(machine);
        machine.with_switch_scratch(|scratch| {
            scratch.clear();
            for &(_, block) in cases {
                let index = machine.block_idx(block);
                let label = if machine.block_has_phis(index) {
                    let label = machine.label_create()?;
                    scratch.blocks.push((label, block))?;
                    label
                } else {
                    machine.block_label(index)
                };
                scratch.labels.push(label)?;
            }
            let default = machine.label_create()?;
            SwitchRange {
                machine,
                cases,
                labels: &scratch.labels,
                default,
                cmp,
                tmp,
                width32: width <= 32,
            }
            .build(0, cases.len())?;
            machine.label_place(default)?;
            generate_branch_to_block(
                machine,
                machine.unconditional_jump(),
                default_block,
                false,
                false,
            )?;
            for &(label, block) in scratch.blocks.iter() {
                machine.align_text(8)?;
                machine.label_place(label)?;
                generate_branch_to_block(
                    machine,
                    machine.unconditional_jump(),
                    block,
                    false,
                    false,
                )?;
            }
            Ok(())
        })?;
    }
    release_spilled_regs(machine, spilled);
    Ok(())
}

pub fn move_to_phi_nodes<M: BranchMachine<N>, const N: usize>(
    machine: &M,
    target: BlockIndex,
) -> PgResult<()> {
    if machine.block_has_phis(target) {
        move_to_phi_nodes_impl(machine, target)?;
    }
    Ok(())
}

fn move_to_phi<M: BranchMachine<N>, const N: usize>(
    machine: &M,
    scratch: &mut PhiScratchReg<'_, M, N>,
    phi: BranchValue<M, N>,
    incoming: BranchValue<M, N>,
) -> PgResult<()> {
    let same = machine.adaptor().val_local_idx(phi) == machine.adaptor().val_local_idx(incoming);
    if machine.coalesces_phis()
        && !same
        && !machine.assignment_exists(machine.adaptor().val_local_idx(phi))
        && machine.anchor_phi(phi, incoming)?
    {
        // The incoming value's register is now the phi's: consume the
        // edge's reference to each and emit nothing.
        drop(machine.value_ref(incoming)?);
        drop(machine.result_ref(phi)?);
        return Ok(());
    }
    let phi_ref = machine.result_ref(phi)?;
    if let Some(bits) = machine.adaptor().val_const(incoming) {
        // A folded constant has no assignment: materialize it into the
        // phi's register, or into a scratch register stored to its slot.
        let local = phi_ref.local_idx();
        let parts = machine.with_assignment(local, |assignment| assignment.part_count());
        tpde_assert_eq!(parts, 1, "a constant phi incoming with several parts");
        let (fixed, destination, size, bank) = machine.with_part(local, 0, |ap| {
            (ap.fixed_assignment(), ap.get_reg(), ap.part_size(), ap.bank())
        });
        if fixed {
            machine.materialize_constant(&[bits], bank, size, destination)?;
            machine.regalloc_event(RegallocEvent::PhiMove);
        } else {
            let source = scratch.alloc_from_bank(bank)?;
            machine.materialize_constant(&[bits], bank, size, source)?;
            machine.allocate_spill_part(local, 0)?;
            let offset = machine.with_part(local, 0, |ap| ap.frame_off());
            machine.spill_reg(source, offset, size)?;
            machine.regalloc_event(RegallocEvent::PhiStore);
            machine.with_part(local, 0, |ap| ap.set_stack_valid());
        }
        return Ok(());
    }
    let incoming_ref = machine.value_ref(incoming)?;
    if same {
        return Ok(());
    }
    let local = phi_ref.local_idx();
    let parts = machine.with_assignment(local, |assignment| assignment.part_count());
    for part in 0..parts {
        let value = incoming_ref.part(part);
        let (fixed, destination, size) = machine.with_part(local, part, |ap| {
            (ap.fixed_assignment(), ap.get_reg(), ap.part_size())
        });
        let mut source = value.cur_reg_unlocked();
        if fixed {
            if source == destination {
                // Already coalesced into the phi's register.
            } else if source.valid() {
                machine.mov(destination, source, size)?;
                machine.regalloc_event(RegallocEvent::PhiMove);
            } else {
                value.reload_into_specific_fixed(destination, 0)?;
                machine.regalloc_event(RegallocEvent::PhiReload);
            }
        } else {
            if source.invalid() {
                source = scratch.alloc_from_bank(value.bank())?;
                value.reload_into_specific_fixed(source, 0)?;
                machine.regalloc_event(RegallocEvent::PhiReload);
            }
            machine.allocate_spill_part(local, part)?;
            let offset = machine.with_part(local, part, |ap| ap.frame_off());
            machine.spill_reg(source, offset, size)?;
            machine.regalloc_event(RegallocEvent::PhiStore);
            machine.with_part(local, part, |ap| ap.set_stack_valid());
        }
    }
    Ok(())
}

pub fn move_to_phi_nodes_impl<M: BranchMachine<N>, const N: usize>(
    machine: &M,
    target: BlockIndex,
) -> PgResult<()> {
    machine.with_phi_scratch(|state| {
        state.clear();
        let current = machine.block_ref(machine.current_block());
        for phi in machine.adaptor().block_phis(machine.block_ref(target)) {
            let incoming = machine
                .adaptor()
                .val_as_phi(phi)
                .incoming_val_for_block(current);
            state.nodes.push(NodeEntry {
                phi,
                incoming,
                local: machine.adaptor().val_local_idx(phi),
                incoming_local: INVALID_VALUE,
                references: 0,
            })?;
        }
        tpde_assert!(
            !state.nodes.is_empty(),
            "block marked as having PHIs has none"
        );
        let mut scratch = PhiScratchReg::new(machine);
        if state.nodes.len() == 1 {
            let node = state.nodes[0];
            move_to_phi(machine, &mut scratch, node.phi, node.incoming)?;
            return scratch.reset();
        }
        if machine.coalesces_phis()
            && state
                .nodes
                .iter()
                .all(|node| !machine.assignment_exists(node.local))
            && state.nodes.iter().all(|node| {
                let incoming = machine.adaptor().val_local_idx(node.incoming);
                !state.nodes.iter().any(|other| other.local == incoming)
            })
        {
            // Anchor edge: no phi of the target exists yet and no incoming
            // value is one of them, so there are no cycles. Pin the
            // heaviest phis first so they win the incoming registers.
            for i in 0..state.nodes.len() {
                state.nodes[i].references = machine.phi_weight(state.nodes[i].phi);
            }
            state.nodes.sort_unstable_by(|a, b| {
                b.references
                    .cmp(&a.references)
                    .then(a.local.get().cmp(&b.local.get()))
            });
            for node in state.nodes.iter() {
                move_to_phi(machine, &mut scratch, node.phi, node.incoming)?;
            }
            return scratch.reset();
        }
        state.nodes.sort_unstable_by_key(|node| node.local.get());
        let mut all_zero = true;
        for i in 0..state.nodes.len() {
            let node = state.nodes[i];
            // An incoming value that is (or, through the adaptor's local
            // index, aliases) another phi of this block must be read before
            // that phi is written.
            let incoming = machine.adaptor().val_local_idx(node.incoming);
            if incoming == node.local {
                continue;
            }
            let found = state
                .nodes
                .partition_point(|node| node.local.get() < incoming.get());
            if found == state.nodes.len() || state.nodes[found].local != incoming {
                continue;
            }
            state.nodes[i].incoming_local = incoming;
            state.nodes[found].references += 1;
            all_zero = false;
        }
        if all_zero {
            for node in state.nodes.iter() {
                move_to_phi(machine, &mut scratch, node.phi, node.incoming)?;
            }
            return scratch.reset();
        }
        state.ready.reserve(state.nodes.len())?;
        state.waiting.resize(state.nodes.len().div_ceil(64), 0)?;
        state.waiting.fill(0);
        for i in 0..state.nodes.len() {
            if state.nodes[i].references != 0 {
                state.set_waiting(i, true);
            } else {
                state.ready.push(i as u32)?;
            }
        }
        let mut handled = 0;
        let mut temporary_value = M::Adaptor::INVALID_VALUE_REF;
        let mut temporary_parts = 0;
        let mut temporary_stack = TempStackGuard::new(machine);
        let mut temporary_low = PhiScratchReg::new(machine);
        let mut temporary_high = PhiScratchReg::new(machine);
        while handled != state.nodes.len() {
            if state.ready.is_empty() {
                let current_index = state
                    .first_waiting()
                    .expect("PHI cycle has no waiting node");
                tpde_assert_eq!(state.nodes[current_index].references, 1);
                tpde_assert!(temporary_value == M::Adaptor::INVALID_VALUE_REF);
                temporary_value = state.nodes[current_index].phi;
                let value = machine.value_ref(temporary_value)?;
                let local = value.local_idx();
                temporary_parts =
                    machine.with_assignment(local, |assignment| assignment.part_count());
                if temporary_parts > 2 {
                    temporary_stack
                        .allocate(machine.with_assignment(local, |assignment| assignment.size()))?;
                    for part in 0..temporary_parts {
                        let (fixed, offset, stack, bank, valid, reg, size, frame) = machine
                            .with_part(local, part, |ap| {
                                (
                                    ap.fixed_assignment(),
                                    ap.part_off(),
                                    ap.stack_valid(),
                                    ap.bank(),
                                    ap.register_valid(),
                                    ap.get_reg(),
                                    ap.part_size(),
                                    if ap.register_valid() {
                                        0
                                    } else {
                                        ap.frame_off()
                                    },
                                )
                            });
                        tpde_assert!(!fixed);
                        let destination =
                            (temporary_stack.offset() as u32).wrapping_add(offset) as i32;
                        if valid {
                            machine.spill_reg(reg, destination, size)?;
                            machine.regalloc_event(RegallocEvent::PhiStore);
                        } else {
                            let reg = temporary_low.alloc_from_bank(bank)?;
                            tpde_assert!(stack);
                            machine.load_from_stack(reg, frame, size)?;
                            machine.spill_reg(reg, destination, size)?;
                            machine.regalloc_event(RegallocEvent::PhiReload);
                            machine.regalloc_event(RegallocEvent::PhiStore);
                        }
                    }
                } else {
                    let low = value.part(0);
                    let reg = temporary_low.alloc_from_bank(low.bank())?;
                    low.reload_into_specific_fixed(reg, 0)?;
                    machine.regalloc_event(RegallocEvent::PhiReload);
                    if temporary_parts == 2 {
                        let high = value.part(1);
                        let reg = temporary_high.alloc_from_bank(high.bank())?;
                        high.reload_into_specific_fixed(reg, 0)?;
                        machine.regalloc_event(RegallocEvent::PhiReload);
                    }
                }
                state.nodes[current_index].references = 0;
                state.ready.push(current_index as u32)?;
                state.set_waiting(current_index, false);
            }
            let mut ready_index = 0;
            while ready_index < state.ready.len() {
                handled += 1;
                let index = state.ready[ready_index] as usize;
                ready_index += 1;
                let node = state.nodes[index];
                if machine.adaptor().val_local_idx(node.incoming) == node.local {
                    drop(machine.value_ref(node.incoming)?);
                    drop(machine.value_ref(node.phi)?);
                    continue;
                }
                if temporary_value != M::Adaptor::INVALID_VALUE_REF
                    && machine.adaptor().val_local_idx(node.incoming)
                        == machine.adaptor().val_local_idx(temporary_value)
                {
                    let value = machine.value_ref(node.phi)?;
                    let local = value.local_idx();
                    if temporary_parts <= 2 {
                        let (fixed, reg, frame, size) = machine.with_part(local, 0, |ap| {
                            (
                                ap.fixed_assignment(),
                                ap.get_reg(),
                                if ap.fixed_assignment() {
                                    0
                                } else {
                                    ap.frame_off()
                                },
                                ap.part_size(),
                            )
                        });
                        let saved = temporary_low.cur_reg();
                        if fixed {
                            machine.mov(reg, saved, size)?;
                            machine.regalloc_event(RegallocEvent::PhiMove);
                        } else {
                            machine.spill_reg(saved, frame, size)?;
                            machine.regalloc_event(RegallocEvent::PhiStore);
                        }
                        if temporary_parts == 2 {
                            let (fixed, frame, size) = machine.with_part(local, 1, |ap| {
                                (ap.fixed_assignment(), ap.frame_off(), ap.part_size())
                            });
                            tpde_assert!(!fixed);
                            machine.spill_reg(temporary_high.cur_reg(), frame, size)?;
                            machine.regalloc_event(RegallocEvent::PhiStore);
                        }
                    } else {
                        for part in 0..temporary_parts {
                            let (fixed, offset, bank, frame, size) =
                                machine.with_part(local, part, |ap| {
                                    (
                                        ap.fixed_assignment(),
                                        ap.part_off(),
                                        ap.bank(),
                                        ap.frame_off(),
                                        ap.part_size(),
                                    )
                                });
                            tpde_assert!(!fixed);
                            let source =
                                (temporary_stack.offset() as u32).wrapping_add(offset) as i32;
                            let reg = temporary_low.alloc_from_bank(bank)?;
                            machine.load_from_stack(reg, source, size)?;
                            machine.spill_reg(reg, frame, size)?;
                            machine.regalloc_event(RegallocEvent::PhiReload);
                            machine.regalloc_event(RegallocEvent::PhiStore);
                        }
                    }
                    drop(value);
                    if temporary_parts > 2 {
                        temporary_stack.reset();
                    }
                    temporary_value = M::Adaptor::INVALID_VALUE_REF;
                    continue;
                }
                move_to_phi(machine, &mut scratch, node.phi, node.incoming)?;
                if node.incoming_local == INVALID_VALUE {
                    continue;
                }
                let found = state
                    .nodes
                    .partition_point(|entry| entry.local.get() < node.incoming_local.get());
                tpde_assert!(
                    found != state.nodes.len() && state.nodes[found].local == node.incoming_local,
                    "incoming PHI local index is inconsistent"
                );
                tpde_assert!(state.nodes[found].references > 0);
                state.nodes[found].references -= 1;
                if state.nodes[found].references == 0 {
                    state.ready.push(found as u32)?;
                    state.set_waiting(found, false);
                }
            }
            state.ready.clear();
        }
        temporary_high.reset()?;
        temporary_low.reset()?;
        scratch.reset()
    })
}

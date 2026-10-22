// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{
    assignment_store_guard::{AssignmentSlotStorage, AssignmentSlots},
    branches::{self, BranchMachine, PhiScratch, SwitchScratch},
    calls::CallMachine,
    frame_guard::FrameState,
    generic_value_part::GenericValuePart,
    guard_machine::{GuardMachine, RegallocEvent, INVALID_VALUE},
    target::{CompilerOutput, CompilerTarget, TargetMachine},
    value_part_ref::ValuePartRef,
    value_ref::ValueRef,
};
use alloc::boxed::Box;
use core::cell::{Cell, RefCell};
use mcx::{ForgetSafe, Mcx};
use tpde_assembler::{Architecture, AssemblerElf, SectionKind, SymBinding, SymRef};
use tpde_base::ValLocalIdx;
use tpde_core::{
    analyzer::{Analyzer, BlockIndex, LivenessInfo},
    calling_convention::{CCAssigner, CallingConventionAssigner},
    compiler::ValueParts,
    ir_adaptor::IrAdaptor,
    value_assignment::{
        with_assignment_allocator, Assignment, AssignmentAllocator, AssignmentInit,
    },
    AssignmentPartRef, Reg, RegBank, RegisterFile,
};
use tpde_function_writer::{FunctionWriter, Label};
use tpde_util::{
    misc::BitSetIterator, tpde_assert, tpde_assert_eq, tpde_assert_ne, SmallVectorGuard,
};
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};

pub trait CompilerAdaptor:
    IrAdaptor<
    ValueRef: Copy + ForgetSafe,
    InstRef: Copy + ForgetSafe,
    BlockRef: Copy + ForgetSafe,
    FuncRef: Copy + ForgetSafe,
>
{
    fn compiler_insts(&self, block: Self::BlockRef) -> impl Iterator<Item = Self::InstRef> + Clone;
    /// `value` is defined by an instruction of block `X` and passed by
    /// `X`'s terminator to the returned phi; every use of `value` is in
    /// `X`; and the phi is not read again on any path from `value`'s
    /// definition that does not pass through the phi's block. Under
    /// `CompilerTarget::PHI_REUSE` the compiler computes such a value into
    /// the phi's fixed register (`FunctionSession::shadow_phi`).
    fn phi_reuse(&self, value: Self::ValueRef) -> Option<Self::ValueRef> {
        let _ = value;
        None
    }
    /// Each function's start alignment (a power of two; upstream's 16). The
    /// text section takes its functions' largest (`align_reserved`) and the
    /// mapper places the section at it.
    fn func_align(&self) -> u32 {
        16
    }
}

pub struct CompilerBase<'m, 'id, A: CompilerAdaptor, T: CompilerTarget<A, N>, const N: usize> {
    pub adaptor: A,
    pub analyzer: Analyzer<'m, A>,
    target: RefCell<T>,
    allocator: AssignmentAllocator<'m, 'id>,
    slots: AssignmentSlotStorage<'m>,
    registers: RegisterFile<N, 32>,
    frame: RefCell<FrameState<'m>>,
    fixed: RefCell<[u32; N]>,
    pinned: RefCell<[u32; N]>,
    /// Effective spill weight per local (frontend override or analyzer),
    /// recorded when the assignment is created, for demotion decisions.
    weights: RefCell<SmallVectorGuard<'m, u32, 64>>,
    /// Locals whose fixed register belongs to a phi (`shadow_phi`): the
    /// register is not released when the local is freed.
    shadows: RefCell<SmallVectorGuard<'m, bool, 64>>,
    delayed: RefCell<SmallVectorGuard<'m, ValLocalIdx, 64>>,
    labels: RefCell<SmallVectorGuard<'m, Label, 64>>,
    symbols: SmallVectorGuard<'m, SymRef, 16>,
    personalities: RefCell<SmallVectorGuard<'m, (SymRef, SymRef), 4>>,
    phi: RefCell<PhiScratch<'m, A::ValueRef>>,
    switch: RefCell<SwitchScratch<'m, A::BlockRef>>,
    dynamic_allocas: SmallVectorGuard<'m, (A::ValueRef, u32, u32), 16>,
    writer: FunctionWriter<'m, T::Writer>,
    assembler: AssemblerElf<'m>,
    complete: bool,
    failed: bool,
}

pub fn with_compiler_base<'m, A: CompilerAdaptor, T: CompilerTarget<A, N>, R, const N: usize>(
    mcx: Mcx<'m>,
    adaptor: A,
    target: T,
    writer: T::Writer,
    architecture: Architecture,
    operation: impl for<'id> FnOnce(CompilerBase<'m, 'id, A, T, N>) -> R,
) -> PgResult<R> {
    const {
        assert!(N != 0, "compiler requires at least one register bank");
    }
    assert!(
        (T::GP_BANK.id() as usize) < N,
        "general-purpose register bank outside target"
    );
    assert!(
        T::PLATFORM_POINTER_SIZE.is_power_of_two() && T::PLATFORM_POINTER_SIZE <= 128,
        "invalid platform pointer size"
    );
    assert_eq!(
        architecture,
        T::ARCHITECTURE,
        "compiler target and object architecture disagree"
    );
    let assembler = AssemblerElf::new(mcx, architecture)?;
    Ok(with_assignment_allocator(mcx, |allocator| {
        operation(CompilerBase {
            analyzer: Analyzer::new(mcx),
            adaptor,
            target: RefCell::new(target),
            allocator,
            slots: AssignmentSlotStorage::new(mcx),
            registers: RegisterFile::new(),
            frame: RefCell::new(FrameState::new(mcx)),
            fixed: RefCell::new([0; N]),
            pinned: RefCell::new([0; N]),
            weights: RefCell::new(SmallVectorGuard::new(mcx)),
            shadows: RefCell::new(SmallVectorGuard::new(mcx)),
            delayed: RefCell::new(SmallVectorGuard::new(mcx)),
            labels: RefCell::new(SmallVectorGuard::new(mcx)),
            symbols: SmallVectorGuard::new(mcx),
            personalities: RefCell::new(SmallVectorGuard::new(mcx)),
            phi: RefCell::new(PhiScratch::new(mcx)),
            switch: RefCell::new(SwitchScratch::new(mcx)),
            dynamic_allocas: SmallVectorGuard::new(mcx),
            writer: FunctionWriter::new(mcx, writer),
            assembler,
            complete: false,
            failed: false,
        })
    }))
}

pub struct FunctionSession<
    's,
    'a,
    'm,
    'id,
    A: CompilerAdaptor,
    T: CompilerTarget<A, N>,
    O: CompilerOutput<'m>,
    C: CallingConventionAssigner,
    const N: usize,
> {
    adaptor: &'s A,
    analyzer: &'s Analyzer<'m, A>,
    target: &'s RefCell<T>,
    slots: RefCell<AssignmentSlots<'s, 'a, 'm, 'id>>,
    allocator: &'a AssignmentAllocator<'m, 'id>,
    registers: &'s RegisterFile<N, 32>,
    registers_loaned: Cell<bool>,
    frame: &'s RefCell<FrameState<'m>>,
    fixed: &'s RefCell<[u32; N]>,
    pinned: &'s RefCell<[u32; N]>,
    weights: &'s RefCell<SmallVectorGuard<'m, u32, 64>>,
    shadows: &'s RefCell<SmallVectorGuard<'m, bool, 64>>,
    delayed: &'s RefCell<SmallVectorGuard<'m, ValLocalIdx, 64>>,
    labels: &'s RefCell<SmallVectorGuard<'m, Label, 64>>,
    symbols: &'s [SymRef],
    personalities: &'s RefCell<SmallVectorGuard<'m, (SymRef, SymRef), 4>>,
    phi: &'s RefCell<PhiScratch<'m, A::ValueRef>>,
    switch: &'s RefCell<SwitchScratch<'m, A::BlockRef>>,
    output: &'s RefCell<O>,
    assigner: RefCell<Option<C>>,
    current: Cell<BlockIndex>,
    variable_refs: Cell<ValLocalIdx>,
    branch: Cell<bool>,
    /// The value `feed_phi` gave a register to and whose defining
    /// instruction has not finished: the register holds nothing yet, so
    /// `select_reg` never spills it (it un-feeds it as a last resort).
    fed: Cell<ValLocalIdx>,
    /// The instruction being compiled when it is not its block's last (the
    /// terminator): only then may `select_reg` borrow a fixed register.
    borrow_inst: Cell<Option<A::InstRef>>,
    /// Fixed registers lent as scratch for the current instruction (D398):
    /// (register, holder, part, save slot, size); restored at its end.
    borrowed: RefCell<[(Reg, ValLocalIdx, u32, i32, u32); 4]>,
    nborrowed: Cell<u8>,
    failure: RefCell<Option<Box<PgError>>>,
}

#[cold]
fn invalid(message: &'static str) -> Box<PgError> {
    PgError::error(message)
        .with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE)
        .into()
}

impl<'m, 'id, A: CompilerAdaptor, T: CompilerTarget<A, N>, const N: usize>
    CompilerBase<'m, 'id, A, T, N>
{
    pub fn target(&self) -> core::cell::Ref<'_, T> {
        self.target.borrow()
    }
    pub fn target_mut(&mut self) -> &mut T {
        self.target.get_mut()
    }
    pub fn reset_state_counts(&self) -> (usize, usize, usize) {
        (
            self.symbols.len(),
            self.labels.borrow().len(),
            self.personalities.borrow().len(),
        )
    }
    pub fn assembler(&self) -> PgResult<&AssemblerElf<'m>> {
        if self.complete && !self.failed {
            Ok(&self.assembler)
        } else {
            Err(invalid("compiler output is not complete"))
        }
    }
    /// ```compile_fail
    /// use tpde_compiler::{CompilerAdaptor, CompilerBase, target::CompilerTarget};
    /// fn reset_borrowed<'m, 'id, A: CompilerAdaptor, T: CompilerTarget<A, N>, const N: usize>(
    ///     compiler: &mut CompilerBase<'m, 'id, A, T, N>,
    /// ) {
    ///     let output = compiler.assembler_mut().unwrap();
    ///     compiler.reset().unwrap();
    ///     let _ = output.section_count();
    /// }
    /// ```
    pub fn assembler_mut(&mut self) -> PgResult<&mut AssemblerElf<'m>> {
        if self.complete && !self.failed {
            Ok(&mut self.assembler)
        } else {
            Err(invalid("compiler output is not complete"))
        }
    }
    pub fn reset(&mut self) -> PgResult<()> {
        self.complete = false;
        self.failed = true;
        self.target.get_mut().reset();
        self.adaptor.reset();
        self.frame.get_mut().reset();
        self.assembler.reset()?;
        self.symbols.clear();
        self.labels.get_mut().clear();
        self.personalities.get_mut().clear();
        self.allocator.reset();
        self.complete = false;
        self.failed = false;
        Ok(())
    }
    pub fn compile(&mut self) -> PgResult<bool> {
        tpde_assert!(self.symbols.is_empty(), "reset compiler before reuse");
        self.complete = false;
        self.failed = true;
        let mut module = self.writer.begin_module(&mut self.assembler)?;
        let text = module
            .assembler_mut()
            .get_default_section(SectionKind::Text)?;
        module.switch_section(text)?;
        self.symbols.reserve(self.adaptor.func_count() as usize)?;
        for function in self.adaptor.funcs() {
            let binding = if self.adaptor.func_has_weak_linkage(function) {
                SymBinding::Weak
            } else if self.adaptor.func_only_local(function) {
                SymBinding::Local
            } else {
                SymBinding::Global
            };
            let name = self.adaptor.func_link_name(function);
            let symbol = if self.adaptor.func_extern(function) {
                module.assembler_mut().sym_add_undef(name, binding)?
            } else {
                module.assembler_mut().sym_predef_func(name, binding)?
            };
            let index = self.symbols.len() as u32;
            self.symbols.push(symbol)?;
            self.target
                .get_mut()
                .define_func_idx(&self.adaptor, function, index);
        }
        if !self.target.get_mut().hook_post_func_sym_init(
            &mut self.adaptor,
            module.assembler_mut(),
            &self.symbols,
        )? {
            return Ok(false);
        }
        let output = RefCell::new(module);
        let mut success = true;
        for (index, function) in self.adaptor.funcs().into_iter().enumerate() {
            if self.adaptor.func_extern(function) {
                continue;
            }
            if !self.target.get_mut().before_function(
                &mut self.adaptor,
                &mut *output.borrow_mut(),
                function,
                index as u32,
                self.symbols[index],
            )? {
                success = false;
                continue;
            }
            let compiled = (|| -> PgResult<bool> {
                if !self.adaptor.switch_func(function) {
                    return Ok(false);
                }
                self.target.get_mut().analysis_start(&self.adaptor);
                self.analyzer.switch_func(&self.adaptor, function)?;
                self.target.get_mut().analysis_end(&self.adaptor);
                let leaf = !self.target.get_mut().cur_func_may_emit_calls(&self.adaptor);
                self.frame
                    .get_mut()
                    .begin_function(self.adaptor.cur_has_dynamic_alloca(), leaf);
                *self.fixed.get_mut() = [0; N];
                *self.pinned.get_mut() = [0; N];
                if T::WEIGHTED_REGALLOC {
                    self.weights.get_mut().clear();
                    self.weights
                        .get_mut()
                        .resize(self.analyzer.liveness.len(), 0)?;
                }
                if T::PHI_REUSE {
                    self.shadows.get_mut().clear();
                    self.shadows
                        .get_mut()
                        .resize(self.analyzer.liveness.len(), false)?;
                }
                self.allocator.reset();
                self.registers.reset();
                self.delayed.get_mut().clear();
                self.delayed
                    .get_mut()
                    .resize(self.analyzer.block_layout.len(), INVALID_VALUE)?;
                let result = self.slots.with_slots(
                    &self.allocator,
                    self.analyzer.liveness.len(),
                    |slots| {
                        let info;
                        let session = FunctionSession {
                            adaptor: &self.adaptor,
                            analyzer: &self.analyzer,
                            target: &self.target,
                            slots: RefCell::new(slots),
                            allocator: &self.allocator,
                            registers: &self.registers,
                            registers_loaned: Cell::new(false),
                            frame: &self.frame,
                            fixed: &self.fixed,
                            pinned: &self.pinned,
                            weights: &self.weights,
                            shadows: &self.shadows,
                            delayed: &self.delayed,
                            labels: &self.labels,
                            symbols: &self.symbols,
                            personalities: &self.personalities,
                            phi: &self.phi,
                            switch: &self.switch,
                            output: &output,
                            assigner: RefCell::new(None),
                            fed: Cell::new(INVALID_VALUE),
                            borrow_inst: Cell::new(None),
                            borrowed: RefCell::new([(Reg::make_invalid(), INVALID_VALUE, 0, 0, 0); 4]),
                            nborrowed: Cell::new(0),
                            current: Cell::new(
                                self.analyzer
                                    .block_idx(&self.adaptor, self.adaptor.cur_entry_block()),
                            ),
                            variable_refs: Cell::new(INVALID_VALUE),
                            branch: Cell::new(false),
                            failure: RefCell::new(None),
                        };
                        session.begin_function(index as u32)?;
                        info = self.target.borrow().cc_info(&self.adaptor);
                        let policy = self.target.borrow().new_cc_policy(&self.adaptor);
                        *session.assigner.borrow_mut() = Some(CCAssigner::new(&info, policy));
                        session.compile_function(index as u32, &mut self.dynamic_allocas)
                    },
                )??;
                Ok(result)
            })();
            let after = self.target.get_mut().after_function(
                &mut self.adaptor,
                &mut *output.borrow_mut(),
                function,
                index as u32,
                compiled.as_ref().is_ok_and(|value| *value),
            );
            let result = compiled?;
            if !after? || !result {
                success = false
            }
        }
        let mut module = output.into_inner();
        module.flush()?;
        module.end_module()?;
        module.assembler_mut().finalize();
        self.complete = success;
        self.failed = !success;
        Ok(success)
    }
}

impl<
        's,
        'a,
        'm,
        'id,
        A: CompilerAdaptor,
        T: CompilerTarget<A, N>,
        O: CompilerOutput<'m>,
        C: CallingConventionAssigner,
        const N: usize,
    > FunctionSession<'s, 'a, 'm, 'id, A, T, O, C, N>
{
    /// Allocates and registers the assignment of `value` (no register
    /// decision). Returns the part count and the bank of part zero.
    fn allocate_assignment(
        &self,
        value: A::ValueRef,
        local: ValLocalIdx,
    ) -> PgResult<(u32, RegBank)> {
        tpde_assert!(!self.assignment_exists(local));
        let parts = self.target.borrow().val_parts(self.adaptor, value);
        let count = parts.count();
        tpde_assert_ne!(count, 0);
        let mut maximum = 0;
        for part in 0..count {
            let size = parts.size_bytes(part);
            tpde_assert!(size.is_power_of_two() && size <= 128);
            tpde_assert!((parts.reg_bank(part).id() as usize) < N);
            maximum = maximum.max(size)
        }
        let live = self.liveness(local);
        let assignment = self.allocator.allocate(count)?.initialize(
            AssignmentInit {
                references_left: live.ref_count,
                max_part_size: maximum as u8,
                delay_free: live.last_full,
                ..AssignmentInit::default()
            },
            |part| {
                (1 << 9)
                    | (u16::from(parts.reg_bank(part).id()) << 5)
                    | ((parts.size_bytes(part).trailing_zeros() as u16) << 12)
            },
        );
        self.slots
            .borrow_mut()
            .insert(local.get() as usize, assignment);
        if T::WEIGHTED_REGALLOC {
            let weight = self.value_weight(value, local);
            self.weights.borrow_mut()[local.get() as usize] = weight;
        }
        if self.adaptor.val_is_phi(value) {
            // Every edge into the phi's block writes its register (fixed
            // path) or its slot: a fixed phi must keep its register.
            for part in 0..count {
                self.with_part(local, part, |ap| ap.set_no_demote(true));
            }
        }
        Ok((count, parts.reg_bank(0)))
    }
    /// The spill weight the allocator ranks `value` by: a forced fixed
    /// assignment is heaviest, then the frontend's number, then the
    /// analyzer's loop-scaled reference count.
    fn value_weight(&self, value: A::ValueRef, local: ValLocalIdx) -> u32 {
        let target = self.target.borrow();
        if target.try_force_fixed_assignment(self.adaptor, value) {
            return u32::MAX;
        }
        target
            .value_weight(self.adaptor, value)
            .unwrap_or_else(|| self.liveness(local).weight)
    }
    /// The lightest classic fixed assignment of `bank` whose register carries
    /// no other lock, with its weight; a candidate for demotion.
    fn lightest_fixed(&self, bank: RegBank) -> Option<(Reg, ValLocalIdx, u32, u32)> {
        let registers = self.registers;
        let slots = self.slots.borrow();
        let mut best: Option<(Reg, ValLocalIdx, u32, u32)> = None;
        let candidates = registers.used.get() & RegisterFile::<N, 32>::bank_regs(bank);
        for index in BitSetIterator::<false>::new(candidates) {
            let reg = Reg::new(index as u64);
            if registers.reg_lock_count(reg) != 1 {
                continue;
            }
            let (local, part) = (registers.reg_local_idx(reg), registers.reg_part(reg));
            if local == INVALID_VALUE {
                continue;
            }
            let Some(ap) = slots.part(local.get() as usize, part) else {
                continue;
            };
            if !ap.fixed_assignment() || ap.pinned() || ap.no_demote() || ap.variable_ref() {
                continue;
            }
            let weight = self.weights.borrow()[local.get() as usize];
            if best.is_none_or(|(_, _, _, w)| weight < w) {
                best = Some((reg, local, part, weight));
            }
        }
        best
    }
    fn init_assignment(&self, value: A::ValueRef, local: ValLocalIdx) -> PgResult<()> {
        let (count, bank) = self.allocate_assignment(value, local)?;
        if count != 1 {
            return Ok(());
        }
        if T::PHI_REUSE {
            if let Some(phi) = self.adaptor.phi_reuse(value) {
                if self.shadow_phi(local, bank, phi) {
                    return Ok(());
                }
            }
        }
        if T::PHI_COALESCING && self.may_change_value_state() {
            if let Some(phi) = self.adaptor.phi_reuse(value) {
                if self.feed_phi(local, bank, phi) {
                    return Ok(());
                }
            }
        }
        let live = self.liveness(local);
        let fixed = self.fixed.borrow()[bank.id() as usize];
        let loop_info = self
            .analyzer
            .loop_from_idx(self.analyzer.block_loop_idx(self.current.get()));
        let budget = T::FIXED_ASSIGNMENTS[bank.id() as usize];
        let mut try_fixed = live.last > self.current.get()
            && loop_info.definitions_in_childs.wrapping_add(fixed) < budget
            && self.nborrowed.get() == 0
            && self.fixed_room(bank);
        let forced = self
            .target
            .borrow()
            .try_force_fixed_assignment(self.adaptor, value);
        if forced {
            try_fixed = fixed < budget && self.nborrowed.get() == 0 && self.fixed_room(bank)
        }
        if !try_fixed
            && self.nborrowed.get() == 0
            && T::WEIGHTED_REGALLOC
            && live.last > self.current.get()
            && fixed >= budget
            && self.may_change_value_state()
        {
            // The budget is full: a heavier value demotes the lightest
            // classic holder, which is spilled here and becomes an ordinary
            // register-valid value (evictable, released at the next branch
            // that must). Only a holder defined in this block may be
            // demoted: the spill is emitted here, and the compiler's linear
            // state ("on the stack") flows into every block compiled after
            // this one, so the store must sit on every path from the
            // holder's definition to its later uses. The definition block
            // dominates the uses (SSA), and a basic block is left only past
            // its end, so a store in the definition block is on every such
            // path; a store in any other block is not (a diamond whose other
            // arm keeps the holder in its register let the join reload an
            // unwritten slot: the tpde arm's garbage group keys of
            // `partition_aggregate:79/81`). The earlier rule, a holder
            // defined inside the innermost loop of this block, covered
            // re-entry through the header but not a forward path around
            // the demoting block.
            let weight = self.value_weight(value, local);
            if let Some((reg, holder, part, holder_weight)) = self.lightest_fixed(bank) {
                if holder_weight < weight
                    && self.liveness(holder).first == self.current.get()
                    && !self.with_part(holder, part, |ap| ap.variable_ref())
                {
                    self.spill_part(holder, part)?;
                    GuardMachine::regalloc_event(self, RegallocEvent::Demote);
                    self.with_part(holder, part, |ap| ap.set_fixed_assignment(false));
                    self.with_registers(|r| r.unmark_fixed(reg));
                    self.decrement_fixed_count(bank);
                    try_fixed = true;
                }
            }
        }
        if try_fixed {
            let reg = T::select_fixed_assignment_reg(self, local, 0, value);
            if reg.valid() && !self.with_registers(|r| r.is_used(reg)) {
                self.with_part(local, 0, |ap| {
                    ap.set_reg(reg);
                    ap.set_register_valid(true);
                    ap.set_fixed_assignment(true)
                });
                self.with_registers(|r| {
                    r.mark_used(reg, local, 0);
                    r.inc_lock_count(reg);
                    r.mark_clobbered(reg)
                });
                self.fixed.borrow_mut()[bank.id() as usize] += 1;
            }
        }
        Ok(())
    }
    /// The anchor side of `shadow_phi`: `local` feeds phi `phi` at its
    /// block's edge and is read nowhere else (`CompilerAdaptor::phi_reuse`),
    /// but the phi has no register yet, so this edge will be the phi's
    /// anchor (`anchor_phi`) and the phi takes the incoming's register if
    /// that register is one the phi may be pinned to. Define the value
    /// into such a register: a free one from the phi's pin mask
    /// (`docs/design/gravity-simple/tpde-fixedregs.md`). The value stays an
    /// ordinary register-valid value (unlocked, evictable under pressure,
    /// in which case the edge falls back to a move); nothing here is an
    /// invariant.
    fn feed_phi(&self, local: ValLocalIdx, bank: RegBank, phi: A::ValueRef) -> bool {
        let phi_local = self.adaptor.val_local_idx(phi);
        if self.assignment_exists(phi_local) || self.nborrowed.get() != 0 || !self.fixed_room(bank) {
            return false;
        }
        if self.pinned.borrow()[bank.id() as usize] >= T::PINNED_ASSIGNMENTS[bank.id() as usize] {
            return false;
        }
        let live = self.liveness(phi_local);
        let mask = T::pin_reg_mask(self, bank, live.crosses_call, live.crosses_cold_call, phi)
            & RegisterFile::<N, 32>::bank_regs(bank)
            & self.with_registers(|r| r.allocatable.get() & !r.used.get());
        if mask == 0 {
            return false;
        }
        let reg = Reg::new(u64::from(mask.trailing_zeros()));
        self.with_part(local, 0, |ap| {
            ap.set_reg(reg);
            ap.set_register_valid(true);
        });
        self.with_registers(|r| {
            r.mark_used(reg, local, 0);
            r.mark_clobbered(reg);
        });
        self.fed.set(local);
        true
    }
    /// Computing a back-edge incoming into the phi's own register
    /// (`docs/design/gravity-simple/tpde-phi-coalescing.md`, section 3):
    /// `local` is being defined by an instruction of the current block `X`
    /// whose terminator passes it to `phi`, every use of `local` is in
    /// `X`, and `phi` is not read again before an edge into its block
    /// rewrites its register (`CompilerAdaptor::phi_reuse`). When `phi`
    /// sits in a fixed register `R`, `local` is defined straight into `R`:
    /// its part is fixed in `R` (pinned like the phi) but the register
    /// file keeps `phi` as `R`'s owner and the budgets are untouched; the
    /// edge from `X` finds the source already in the phi's register and
    /// emits no move; freeing `local` at that edge leaves `R` alone
    /// (`shadows`); a later edge from another block moves its own
    /// incoming into `R` as before, after `local` is dead.
    fn shadow_phi(&self, local: ValLocalIdx, bank: RegBank, phi: A::ValueRef) -> bool {
        if !self.may_change_value_state() {
            return false;
        }
        let phi_local = self.adaptor.val_local_idx(phi);
        if !self.assignment_exists(phi_local) || phi_local == local {
            return false;
        }
        let (parts, variable) =
            self.with_assignment(phi_local, |a| (a.part_count(), a.variable_ref()));
        if parts != 1 || variable {
            return false;
        }
        let (fixed, valid, reg, pinned, phi_bank, phi_size) =
            self.with_part(phi_local, 0, |ap| {
                (
                    ap.fixed_assignment(),
                    ap.register_valid(),
                    ap.get_reg(),
                    ap.pinned(),
                    ap.bank(),
                    ap.part_size(),
                )
            });
        let size = self.with_part(local, 0, |ap| ap.part_size());
        if !fixed || !valid || phi_bank != bank || phi_size > 8 || size > phi_size {
            return false;
        }
        if !self.with_registers(|r| {
            r.is_fixed(reg) && r.reg_local_idx(reg) == phi_local && r.reg_part(reg) == 0
        }) {
            return false;
        }
        self.with_part(local, 0, |ap| {
            ap.set_reg(reg);
            ap.set_register_valid(true);
            ap.set_fixed_assignment(true);
            ap.set_pinned(pinned);
        });
        self.shadows.borrow_mut()[local.get() as usize] = true;
        true
    }
    /// Phi coalescing at the anchor edge (the first compiled edge into the
    /// phi's block, `docs/design/gravity-simple/tpde-phi-coalescing.md`):
    /// creates `phi`'s assignment pinned to a register. Returns `true` when
    /// the incoming value's register was taken over and no move is needed
    /// on this edge; `false` without creating the assignment when the phi
    /// cannot be pinned (the caller takes the classic path), or after
    /// pinning it to a fresh register the caller must move into.
    fn anchor_phi(&self, phi: A::ValueRef, incoming: A::ValueRef) -> PgResult<bool> {
        let local = self.adaptor.val_local_idx(phi);
        if self.assignment_exists(local) {
            return Ok(false);
        }
        let parts = self.target.borrow().val_parts(self.adaptor, phi);
        if parts.count() != 1 {
            return Ok(false);
        }
        let bank = parts.reg_bank(0);
        if self.pinned.borrow()[bank.id() as usize] >= T::PINNED_ASSIGNMENTS[bank.id() as usize] || !self.fixed_room(bank) {
            return Ok(false);
        }
        let live = self.liveness(local);
        // A 16-byte part (a vector) may be pinned only while no call is
        // crossed: the callee-saved vector registers keep their low eight
        // bytes only, and the call sequence saves eight.
        if parts.size_bytes(0) > 8 && (live.crosses_call || live.crosses_cold_call) {
            return Ok(false);
        }
        let mask = T::pin_reg_mask(self, bank, live.crosses_call, live.crosses_cold_call, phi)
            & RegisterFile::<N, 32>::bank_regs(bank)
            & self.with_registers(|r| r.allocatable.get());
        if mask == 0 {
            return Ok(false);
        }
        let special = self
            .target
            .borrow()
            .val_ref_special(self.adaptor, incoming)
            .is_some();
        let in_local = self.adaptor.val_local_idx(incoming);
        // The incoming value's register, when it can be taken over: the
        // value dies at this edge (this edge holds its last reference and
        // its range ends in this block) or is already on the stack, and
        // nothing else locks the register. The take-over makes this edge's
        // code move-free, which is sound only while every execution of the
        // edge finds the incoming in that register: an edge inside a loop
        // the incoming is defined outside of runs again after the loop's
        // other edges into the phi's block rewrote the register
        // (`tests/anchor_loop.rs`); a range that runs past its last
        // reference (`extend_through_sunk`: a sunk block, compiled later,
        // jumps back before this edge) still needs the register there
        // (`tests/poll_sink.rs`, D3896).
        let cur_loop = self
            .analyzer
            .loop_from_idx(self.analyzer.block_loop_idx(self.current.get()));
        let in_first = self.liveness(in_local).first;
        let defined_inside = cur_loop.begin <= in_first && in_first < cur_loop.end;
        let mut steal = Reg::make_invalid();
        let mut in_fixed = false;
        let mut in_pinned = false;
        if !special && phi != incoming && defined_inside && self.assignment_exists(in_local) {
            let (variable, count, refs_left) = self.with_assignment(in_local, |a| {
                (a.variable_ref(), a.part_count(), a.references_left())
            });
            if !variable && count == 1 {
                let (valid, reg, stack_valid, fixed, pinned) =
                    self.with_part(in_local, 0, |ap| {
                        (
                            ap.register_valid(),
                            ap.get_reg(),
                            ap.stack_valid(),
                            ap.fixed_assignment(),
                            ap.pinned(),
                        )
                    });
                let dies = refs_left == 1 && self.liveness(in_local).last == self.current.get();
                if valid
                    && mask & (1u64 << reg.id()) != 0
                    && (dies || (stack_valid && !fixed))
                    && self.with_registers(|r| r.reg_lock_count(reg)) == u8::from(fixed)
                {
                    steal = reg;
                    in_fixed = fixed;
                    in_pinned = pinned;
                }
            }
        }
        let reg = if steal.valid() {
            steal
        } else {
            let free = self.with_registers(|r| mask & !r.used.get());
            if free != 0 {
                Reg::new(u64::from(free.trailing_zeros()))
            } else {
                // No free register: take one whose value is already on the
                // stack (spilled before the branch, or a constant) and is
                // neither fixed nor locked -- the register the edge code's
                // release would free anyway -- preferring the value that
                // dies first. The value reloads from its slot at its next
                // use; the phi keeps the register through its loop.
                let used = self.with_registers(|r| mask & r.used.get());
                let mut pick = Reg::make_invalid();
                let mut pick_last = BlockIndex(0);
                for id in BitSetIterator::<false>::new(used) {
                    let candidate = Reg::new(id);
                    let (local_c, part_c, fixed_c, locked) = self.with_registers(|r| {
                        (
                            r.reg_local_idx(candidate),
                            r.reg_part(candidate),
                            r.is_fixed(candidate),
                            r.reg_lock_count(candidate) != 0,
                        )
                    });
                    if fixed_c || locked || local_c == INVALID_VALUE {
                        continue;
                    }
                    // `stack_valid` alone says "not modified since the last
                    // spill"; a value never spilled has no slot (a phi given
                    // its register at an anchor edge, a call result) and can
                    // only be taken when it is a constant, rematerialized at
                    // its reload.
                    let on_stack = self.with_part(local_c, part_c, |ap| {
                        ap.register_valid()
                            && ap.stack_valid()
                            && !ap.fixed_assignment()
                            && !ap.variable_ref()
                            && (ap.assignment().frame_off() != 0
                                || self.adaptor.local_const(local_c).is_some())
                    });
                    if !on_stack {
                        continue;
                    }
                    let last = self.liveness(local_c).last;
                    if pick.invalid() || last < pick_last {
                        pick = candidate;
                        pick_last = last;
                    }
                }
                if pick.invalid() {
                    return Ok(false);
                }
                let (local_c, part_c) =
                    self.with_registers(|r| (r.reg_local_idx(pick), r.reg_part(pick)));
                self.with_part(local_c, part_c, |ap| ap.set_register_valid(false));
                self.with_registers(|r| r.unmark_used(pick));
                pick
            }
        };
        let (count, created_bank) = self.allocate_assignment(phi, local)?;
        assert_eq!((count, created_bank), (1, bank));
        if steal.valid() {
            self.with_part(in_local, 0, |ap| {
                ap.set_register_valid(false);
                ap.set_fixed_assignment(false);
                ap.set_pinned(false);
            });
            self.with_registers(|r| {
                if in_fixed {
                    r.dec_lock_count_must_zero(reg, 1);
                }
                r.unmark_used(reg);
            });
            if in_fixed {
                if in_pinned {
                    self.decrement_pinned_count(bank);
                } else {
                    self.decrement_fixed_count(bank);
                }
            }
        }
        self.with_part(local, 0, |ap| {
            ap.set_reg(reg);
            ap.set_register_valid(true);
            ap.set_fixed_assignment(true);
            ap.set_pinned(true);
        });
        self.with_registers(|r| {
            r.mark_used(reg, local, 0);
            r.inc_lock_count(reg);
            r.mark_clobbered(reg)
        });
        self.pinned.borrow_mut()[bank.id() as usize] += 1;
        Ok(steal.valid())
    }
    fn free_assignment(&self, local: ValLocalIdx) {
        let assignment = self
            .slots
            .borrow_mut()
            .take(local.get() as usize)
            .expect("free of missing assignment");
        let shadow = T::PHI_REUSE && self.shadows.borrow()[local.get() as usize];
        for part in 0..assignment.part_count() {
            let ap = AssignmentPartRef::new(&assignment, part);
            if shadow {
                // The register is the phi's (`shadow_phi`): nothing to
                // release, and no lock of ours is outstanding.
                tpde_assert!(ap.fixed_assignment() && part == 0);
            } else if ap.fixed_assignment() {
                let reg = ap.get_reg();
                if ap.pinned() {
                    self.decrement_pinned_count(ap.bank());
                } else {
                    self.decrement_fixed_count(ap.bank());
                }
                self.with_registers(|r| {
                    // A register parked for a call sequence (the A64
                    // `calls.rs`: released from the file, the part keeps
                    // its state) is not ours to release; the value dying
                    // inside the call leaves it to the unpark.
                    if r.is_used(reg) && r.reg_local_idx(reg) == local && r.reg_part(reg) == part {
                        tpde_assert!(r.is_fixed(reg));
                        r.dec_lock_count_must_zero(reg, 1);
                        r.unmark_used(reg)
                    }
                })
            } else if ap.register_valid() {
                self.with_registers(|r| {
                    tpde_assert!(!r.is_fixed(ap.get_reg()));
                    r.unmark_used(ap.get_reg())
                })
            }
        }
        if shadow {
            self.shadows.borrow_mut()[local.get() as usize] = false;
        }
        if tpde_util::CHECKED {
            self.with_registers(|r| {
                for index in r.used_regs() {
                    tpde_assert_ne!(r.reg_local_idx(Reg::new(index as u64)), local)
                }
            });
        }
        let has_stack = if T::FRAME_INDEXING_NEGATIVE {
            assignment.frame_off() < 0
        } else {
            assignment.frame_off() != 0
        };
        if !assignment.variable_ref() && has_stack {
            self.frame
                .borrow_mut()
                .free(assignment.frame_off(), assignment.size())
        }
        self.allocator.deallocate(assignment);
    }
    pub fn init_variable_ref(&self, local: ValLocalIdx, data: u32) -> PgResult<()> {
        tpde_assert!(!self.assignment_exists(local));
        let assignment = self.allocator.allocate_slow(1, true)?.initialize(
            AssignmentInit {
                frame_off: data as i32,
                references_left: self.variable_refs.get().get(),
                max_part_size: T::PLATFORM_POINTER_SIZE as u8,
                variable_ref: true,
                ..AssignmentInit::default()
            },
            |_| {
                (1 << 9)
                    | (u16::from(T::GP_BANK.id()) << 5)
                    | ((T::PLATFORM_POINTER_SIZE.trailing_zeros() as u16) << 12)
            },
        );
        self.slots
            .borrow_mut()
            .insert(local.get() as usize, assignment);
        self.variable_refs.set(local);
        Ok(())
    }
    pub fn value_ref(&self, value: A::ValueRef) -> PgResult<ValueRef<'_, Self, N>> {
        if let Some(special) = self.target.borrow().val_ref_special(self.adaptor, value) {
            return Ok(ValueRef::special(self, special));
        }
        let local = self.adaptor.val_local_idx(value);
        let slots = self.slots.borrow();
        let assignment = slots
            .get(local.get() as usize)
            .unwrap_or_else(|| panic!("value use before definition: local {}", local.get()));
        Ok(ValueRef::from_token(self, local, assignment))
    }
    pub fn result_ref(&self, value: A::ValueRef) -> PgResult<ValueRef<'_, Self, N>> {
        let local = self.adaptor.val_local_idx(value);
        {
            let slots = self.slots.borrow();
            if let Some(assignment) = slots.get(local.get() as usize) {
                return Ok(ValueRef::from_token(self, local, assignment));
            }
        }
        self.init_assignment(value, local)?;
        Ok(ValueRef::new(self, local))
    }
    pub fn with_value_ref_single<R>(
        &self,
        value: A::ValueRef,
        operation: impl FnOnce(&ValueRef<'_, Self, N>, ValuePartRef<'_, Self, N>) -> R,
    ) -> PgResult<R> {
        let reference = self.value_ref(value)?;
        let part = reference.part(0);
        Ok(operation(&reference, part))
    }
    pub fn with_result_ref_single<R>(
        &self,
        value: A::ValueRef,
        operation: impl FnOnce(&ValueRef<'_, Self, N>, ValuePartRef<'_, Self, N>) -> R,
    ) -> PgResult<R> {
        let reference = self.result_ref(value)?;
        let part = reference.part(0);
        Ok(operation(&reference, part))
    }
    pub fn result_ref_alias(
        &self,
        destination: A::ValueRef,
        mut source: ValueRef<'_, Self, N>,
    ) -> PgResult<ValueRef<'_, Self, N>> {
        let local = self.adaptor.val_local_idx(destination);
        tpde_assert!(!self.assignment_exists(local));
        tpde_assert!(source.is_owned());
        let old = source.local_idx();
        let parts =
            tpde_util::CHECKED.then(|| self.target.borrow().val_parts(self.adaptor, destination));
        self.with_assignment(old, |assignment| {
            tpde_assert!(!assignment.pending_free() && !assignment.variable_ref());
            tpde_assert_eq!(assignment.references_left(), 1);
            if let Some(parts) = &parts {
                tpde_assert_eq!(parts.count(), assignment.part_count())
            }
        });
        let count = self.with_assignment(old, |assignment| assignment.part_count());
        for part in 0..count {
            self.with_part(old, part, |ap| {
                if let Some(parts) = &parts {
                    tpde_assert_eq!(parts.reg_bank(part), ap.bank());
                    tpde_assert_eq!(parts.size_bytes(part), ap.part_size());
                }
                if ap.register_valid() {
                    self.with_registers(|r| r.update_reg_assignment(ap.get_reg(), local, part))
                }
            })
        }
        let live = self.liveness(local);
        self.with_assignment(old, |assignment| {
            assignment.set_delay_free(live.last_full);
            assignment.set_references_left(live.ref_count)
        });
        self.slots
            .borrow_mut()
            .move_slot(old.get() as usize, local.get() as usize);
        source.disown();
        source.reset();
        Ok(ValueRef::new(self, local))
    }
    pub fn result_ref_stack_slot(
        &self,
        destination: A::ValueRef,
        base_local: ValLocalIdx,
        base_part: u32,
        offset: i32,
    ) -> PgResult<ValueRef<'_, Self, N>> {
        let frame = self
            .with_part(base_local, base_part, |ap| ap.variable_stack_off())
            .wrapping_add(offset);
        let local = self.adaptor.val_local_idx(destination);
        self.init_variable_ref(local, 0)?;
        self.with_assignment(local, |assignment| {
            assignment.set_stack_variable(true);
            assignment.set_frame_off(frame)
        });
        Ok(ValueRef::new(self, local))
    }
    pub fn allocate_spill_part(&self, local: ValLocalIdx, part: u32) -> PgResult<()> {
        let (variable, frame, size, valid) = self.with_part(local, part, |ap| {
            (
                ap.variable_ref(),
                ap.assignment().frame_off(),
                if ap.variable_ref() {
                    0
                } else {
                    ap.assignment().size()
                },
                ap.stack_valid(),
            )
        });
        tpde_assert!(!variable);
        if frame == 0 {
            tpde_assert!(!valid);
            let slot = self
                .frame
                .borrow_mut()
                .allocate(size, T::FRAME_INDEXING_NEGATIVE)?;
            tpde_assert_ne!(slot, 0);
            self.with_assignment(local, |assignment| assignment.set_frame_off(slot))
        }
        Ok(())
    }
    pub fn spill_part(&self, local: ValLocalIdx, part: u32) -> PgResult<()> {
        let needed = self.with_part(local, part, |ap| !ap.stack_valid() && !ap.variable_ref());
        if needed {
            // Inside a branch region the store would run on one edge only
            // while the state says "on the stack" on every path: refuse in
            // every build profile rather than miscompile.
            if !self.may_change_value_state() {
                return Err(invalid("tpde: a value spilled inside a branch region (edge code must not change value state)"));
            }
            tpde_assert!(self.with_part(local, part, |ap| ap.register_valid()));
            // A constant is rematerialized at its reload (`reload_to_reg`):
            // no slot, no store.
            if self.adaptor.local_const(local).is_some() {
                return Ok(self.with_part(local, part, |ap| ap.set_stack_valid()));
            }
            self.allocate_spill_part(local, part)?;
            let (reg, offset, size) = self.with_part(local, part, |ap| {
                (ap.get_reg(), ap.frame_off(), ap.part_size())
            });
            T::spill_reg(self, reg, offset, size)?;
            GuardMachine::regalloc_event(self, RegallocEvent::Spill);
            self.with_part(local, part, |ap| ap.set_stack_valid())
        }
        Ok(())
    }
    pub fn evict_part(&self, local: ValLocalIdx, part: u32) -> PgResult<()> {
        tpde_assert!(self.with_part(local, part, |ap| ap.register_valid()));
        self.spill_part(local, part)?;
        let reg = self.with_part(local, part, |ap| {
            ap.set_register_valid(false);
            ap.get_reg()
        });
        self.with_registers(|r| r.unmark_used(reg));
        Ok(())
    }
    pub fn free_reg(&self, reg: Reg) {
        tpde_assert!(self.may_change_value_state());
        let (local, part) = self.with_registers(|r| {
            tpde_assert!(!r.is_fixed(reg));
            (r.reg_local_idx(reg), r.reg_part(reg))
        });
        tpde_assert_ne!(local, INVALID_VALUE);
        self.with_part(local, part, |ap| {
            tpde_assert!(ap.register_valid());
            tpde_assert_eq!(ap.get_reg(), reg);
            tpde_assert!(
                !ap.modified() || ap.variable_ref(),
                "freeing register {} of unspilled value {} part {} (block {}, liveness {:?}, fixed {}, pinned {}, refs {})",
                reg.id(),
                local.get(),
                part,
                self.current.get().0,
                self.liveness(local),
                ap.fixed_assignment(),
                ap.pinned(),
                ap.assignment().references_left()
            );
            ap.set_register_valid(false)
        });
        self.with_registers(|r| r.unmark_used(reg))
    }
}
impl<
        's,
        'a,
        'm,
        'id,
        A: CompilerAdaptor,
        T: CompilerTarget<A, N>,
        O: CompilerOutput<'m>,
        C: CallingConventionAssigner,
        const N: usize,
    > FunctionSession<'s, 'a, 'm, 'id, A, T, O, C, N>
{
    /// D398: a new fixed or pinned assignment in `bank` must leave
    /// `SCRATCH_RESERVE` of its allocatable registers to the instructions'
    /// operands and results: the budgets (`FIXED_ASSIGNMENTS` +
    /// `PINNED_ASSIGNMENTS`) may add up past the bank, and a terminator (a
    /// fused compare's operands, a phi cycle's temporary) cannot borrow.
    fn fixed_room(&self, bank: RegBank) -> bool {
        const SCRATCH_RESERVE: u32 = 4;
        let held = self.fixed.borrow()[bank.id() as usize] + self.pinned.borrow()[bank.id() as usize];
        let regs = (self.registers.allocatable.get() & RegisterFile::<N, 32>::bank_regs(bank)).count_ones();
        held + SCRATCH_RESERVE < regs
    }
    /// D398: every register of `bank` is locked (fixed and pinned
    /// assignments plus this instruction's operands) and a scratch register
    /// is still needed. Lend one held only by its fixed assignment, whose
    /// holder this instruction neither reads nor defines: its value is
    /// saved to a fresh slot here and reloaded when the instruction ends
    /// (`end_borrows`), so every later path finds it in its register as
    /// before. Not inside a terminator (its code ends in the jump) nor a
    /// branch region.
    fn borrow_fixed(&self, bank: RegBank) -> PgResult<Option<Reg>> {
        let Some(inst) = self.borrow_inst.get() else { return Ok(None) };
        let n = self.nborrowed.get() as usize;
        if n >= 4 || !self.may_change_value_state() {
            return Ok(None);
        }
        // Registers this instruction reads or defines.
        let mut busy = 0u64;
        let mut mark = |local: ValLocalIdx| {
            if local == INVALID_VALUE || !self.assignment_exists(local) {
                return;
            }
            let parts = self.with_assignment(local, |a| a.part_count());
            for part in 0..parts {
                self.with_part(local, part, |ap| {
                    if ap.register_valid() {
                        busy |= 1u64 << ap.get_reg().id();
                    }
                });
            }
        };
        for v in self.adaptor.inst_operands(inst) {
            mark(self.adaptor.val_local_idx(v));
        }
        for v in self.adaptor.inst_results(inst) {
            mark(self.adaptor.val_local_idx(v));
        }
        let cands = self.registers.used.get() & self.registers.allocatable.get() & RegisterFile::<N, 32>::bank_regs(bank) & !busy;
        let mut pick = None;
        for index in BitSetIterator::<false>::new(cands) {
            let reg = Reg::new(index as u64);
            if self.registers.reg_lock_count(reg) != 1 {
                continue;
            }
            let (local, part) = (self.registers.reg_local_idx(reg), self.registers.reg_part(reg));
            if local == INVALID_VALUE || local == self.fed.get() {
                continue;
            }
            let ok = self.with_part(local, part, |ap| ap.fixed_assignment() && ap.register_valid() && ap.get_reg() == reg && !ap.variable_ref() && ap.part_size() <= 8);
            if ok {
                pick = Some((reg, local, part));
                break;
            }
        }
        let Some((reg, local, part)) = pick else { return Ok(None) };
        let size = self.with_part(local, part, |ap| ap.part_size());
        let slot = self.frame.borrow_mut().allocate(size.max(8), T::FRAME_INDEXING_NEGATIVE)?;
        T::spill_reg(self, reg, slot, size)?;
        GuardMachine::regalloc_event(self, RegallocEvent::Lend);
        self.with_registers(|r| {
            r.dec_lock_count_must_zero(reg, 1);
            r.unmark_used(reg);
        });
        self.borrowed.borrow_mut()[n] = (reg, local, part, slot, size);
        self.nborrowed.set(n as u8 + 1);
        Ok(Some(reg))
    }
    /// The instruction is done: give each lent register back to its fixed
    /// holder (the value now in it, if any, is evicted to its own slot).
    fn end_borrows(&self) -> PgResult<()> {
        let n = self.nborrowed.get() as usize;
        if n == 0 {
            return Ok(());
        }
        for i in (0..n).rev() {
            let (reg, local, part, slot, size) = self.borrowed.borrow()[i];
            if self.with_registers(|r| r.is_used(reg)) {
                if self.with_registers(|r| r.is_fixed(reg)) {
                    return Err(invalid("tpde: a lent fixed register was fixed again before its return (D398)"));
                }
                self.evict_reg(reg)?;
            }
            T::load_from_stack(self, reg, slot, size)?;
            self.with_registers(|r| {
                r.mark_used(reg, local, part);
                r.mark_fixed(reg);
                r.mark_clobbered(reg);
            });
            self.frame.borrow_mut().free(slot, size.max(8));
        }
        self.nborrowed.set(0);
        Ok(())
    }
    fn begin_function(&self, index: u32) -> PgResult<()> {
        self.output
            .borrow_mut()
            .begin_func(self.adaptor.func_align(), self.analyzer.num_insts.wrapping_mul(8).wrapping_add(64))?;
        T::start_func(self, index)?;
        {
            let mut labels = self.labels.borrow_mut();
            labels.clear();
            labels.reserve(self.analyzer.block_layout.len())?;
            for _ in &self.analyzer.block_layout[..] {
                labels.push(self.output.borrow_mut().label_create()?)?
            }
        }
        Ok(())
    }
    fn compile_function(
        &self,
        index: u32,
        dynamic_allocas: &mut SmallVectorGuard<'m, (A::ValueRef, u32, u32), 16>,
    ) -> PgResult<bool> {
        self.with_cur_cc_assigner(|assigner| -> PgResult<()> {
            let info = *assigner.get_ccinfo();
            tpde_assert_eq!(info.allocatable_regs() & info.arg_regs(), info.arg_regs());
            self.with_registers(|r| r.allocatable.set(info.allocatable_regs()));
            assigner.reset();
            self.with_registers(|r| r.allocatable.set(r.allocatable.get() & !info.arg_regs()));
            T::prologue_begin(self, assigner)?;
            for (arg_index, arg) in self.adaptor.cur_args().into_iter().enumerate() {
                T::prologue_assign_arg(self, assigner, arg_index as u32, &arg)?
            }
            T::prologue_end(self, assigner)?;
            self.with_registers(|r| r.allocatable.set(r.allocatable.get() | info.arg_regs()));
            let withheld = crate::TEST_WITHHELD.load(core::sync::atomic::Ordering::Relaxed);
            if withheld != 0 {
                self.with_registers(|r| r.allocatable.set(r.allocatable.get() & !withheld));
            }
            Ok(())
        })?;
        dynamic_allocas.clear();
        for value in self.adaptor.cur_static_allocas() {
            let size = self.adaptor.val_alloca_size(value);
            let alignment = self.adaptor.val_alloca_align(value);
            tpde_assert!(alignment.is_power_of_two());
            if alignment > 16 || size > T::MAX_STATIC_ALLOCA_SIZE {
                dynamic_allocas.push((value, size, alignment))?;
                self.frame.borrow_mut().has_dynamic_alloca = true;
                continue;
            }
            let local = self.adaptor.val_local_idx(value);
            self.init_variable_ref(local, 0)?;
            let padded = size
                .checked_add(alignment - 1)
                .ok_or_else(|| invalid("static alloca alignment overflow"))?
                & !(alignment - 1);
            let slot = self
                .frame
                .borrow_mut()
                .allocate(padded, T::FRAME_INDEXING_NEGATIVE)?;
            self.with_assignment(local, |assignment| {
                assignment.set_stack_variable(true);
                assignment.set_frame_off(slot)
            });
        }
        if !T::DEFAULT_VAR_REF_HANDLING {
            T::setup_var_ref_assignments(self)?
        }
        for &(value, size, align) in &dynamic_allocas[..] {
            let reference = self.result_ref(value)?;
            T::alloca_fixed(self, size, align, reference.part(0))?;
        }
        for (block_index, &block) in self.analyzer.block_layout.iter().enumerate() {
            if !self.compile_block(block, block_index as u32)? {
                return Ok(false);
            }
        }
        let mut local = self.variable_refs.get();
        while local != INVALID_VALUE {
            let next = self.with_assignment(local, |a| a.next_delayed_free_entry());
            self.slots.borrow_mut().discard(local.get() as usize);
            local = next;
        }
        tpde_assert_eq!(
            self.slots.borrow().occupied(),
            0,
            "unreleased value assignment"
        );
        if let Some(error) = self.failure.borrow_mut().take() {
            return Err(error);
        }
        let body_end = self.output.borrow().offset();
        T::finish_func(self, index)?;
        if self.adaptor.records_offsets() {
            let skew = self.output.borrow_mut().with_base(|base| base.label_skew());
            self.adaptor.func_compiled(body_end, skew);
        }
        self.output.borrow_mut().finish_func()?;
        if let Some(error) = self.failure.borrow_mut().take() {
            return Err(error);
        }
        Ok(true)
    }
    fn compile_block(&self, block: A::BlockRef, index: u32) -> PgResult<bool> {
        self.current.set(BlockIndex(index));
        self.label_place(self.block_label(BlockIndex(index)))?;
        // D117's verifier: the linear state holds only when this block is the only-entry successor of the one laid out before it
        let linear = index > 0 && !self.block_has_multiple_incoming(block) && self.adaptor.block_succs(self.block_ref(BlockIndex(index - 1))).into_iter().any(|b| b == block);
        if !linear {
            let used = self.with_registers(|r| r.used.get());
            for id in tpde_util::misc::BitSetIterator::<false>::new(used) {
                let reg = Reg::new(id);
                let local = self.with_registers(|r| r.reg_local_idx(reg));
                // a dead value's register awaits its delayed free and is never read: only a value live into this block counts
                if local != INVALID_VALUE && self.liveness(local).first.0 < index && self.liveness(local).last.0 >= index && !self.with_part(local, self.with_registers(|r| r.reg_part(reg)), |ap| ap.fixed_assignment() || ap.variable_ref()) {
                    GuardMachine::regalloc_event(self, RegallocEvent::StaleEntry);
                }
            }
        }
        let mut instructions = self.adaptor.compiler_insts(block);
        while let Some(instruction) = instructions.next() {
            if self.adaptor.inst_fused(instruction) {
                continue;
            }
            self.fed.set(INVALID_VALUE);
            self.borrow_inst.set(instructions.clone().next().map(|_| instruction));
            if !T::compile_inst(self, instruction, instructions.clone())? {
                return Ok(false);
            }
            self.borrow_inst.set(None);
            self.end_borrows()?;
            if let Some(error) = self.failure.borrow_mut().take() {
                return Err(error);
            }
        }
        if tpde_util::CHECKED {
            self.with_registers(|registers| {
                for index in registers.used_regs() {
                    let reg = Reg::new(index as u64);
                    let local = registers.reg_local_idx(reg);
                    tpde_assert_ne!(local, INVALID_VALUE);
                    self.with_part(local, registers.reg_part(reg), |ap| {
                        tpde_assert!(ap.register_valid());
                        tpde_assert_eq!(ap.get_reg(), reg);
                        tpde_assert!(!registers.is_fixed(reg) || ap.fixed_assignment())
                    });
                }
            });
        }
        let mut local = self.delayed.borrow()[index as usize];
        while local != INVALID_VALUE {
            let next = self.with_assignment(local, |a| a.next_delayed_free_entry());
            self.free_assignment(local);
            local = next;
        }
        if let Some(error) = self.failure.borrow_mut().take() {
            return Err(error);
        }
        Ok(true)
    }
    pub fn with_cur_cc_assigner<R>(&self, operation: impl FnOnce(&mut C) -> R) -> R {
        let mut loan = crate::compiler_guard::AssignerLoan::new(&self.assigner);
        operation(loan.get_mut())
    }
    pub fn personality_symbol(&self) -> PgResult<SymRef> {
        if !self.adaptor.cur_needs_unwind_info() {
            return Ok(SymRef::default());
        }
        let function = self.target.borrow().cur_personality_func(self.adaptor);
        if !function.valid() {
            return Ok(SymRef::default());
        }
        if let Some(&(_, symbol)) = self
            .personalities
            .borrow()
            .iter()
            .find(|&&(source, _)| source == function)
        {
            return Ok(symbol);
        }
        {
            let mut symbols = self.personalities.borrow_mut();
            let count = symbols.len() + 1;
            symbols.reserve(count)?;
        }
        let symbol = {
            let mut output = self.output.borrow_mut();
            let assembler = output.assembler_mut();
            let section = assembler.get_default_section(SectionKind::DataRelRO)?;
            let (symbol, offset) =
                assembler.sym_def_data(section, b"", &[0; 8], 8, SymBinding::Local)?;
            assembler.reloc_abs(section, function, offset, 0)?;
            symbol
        };
        self.personalities.borrow_mut().push((function, symbol))?;
        Ok(symbol)
    }
    pub fn reloc_text(&self, symbol: SymRef, kind: u32, offset: u64, addend: i64) -> PgResult<()> {
        let mut output = self.output.borrow_mut();
        let section = output.section();
        output.assembler_mut().reloc_sec(
            section,
            symbol,
            kind,
            u32::try_from(offset).map_err(|_| invalid("text relocation exceeds u32"))?,
            addend,
        )
    }
}

impl<
        's,
        'a,
        'm,
        'id,
        A: CompilerAdaptor,
        T: CompilerTarget<A, N>,
        O: CompilerOutput<'m>,
        C: CallingConventionAssigner,
        const N: usize,
    > GuardMachine<N> for FunctionSession<'s, 'a, 'm, 'id, A, T, O, C, N>
{
    type Special = T::Special;
    const GP_BANK: RegBank = T::GP_BANK;
    fn with_registers<R>(&self, operation: impl FnOnce(&RegisterFile<N, 32>) -> R) -> R {
        if tpde_util::CHECKED {
            assert!(
                !self.registers_loaned.replace(true),
                "register file already loaned"
            );
            let result = operation(self.registers);
            self.registers_loaned.set(false);
            return result;
        }
        operation(self.registers)
    }
    fn with_assignment<R>(
        &self,
        local: ValLocalIdx,
        operation: impl FnOnce(&Assignment<'_, '_>) -> R,
    ) -> R {
        let slots = self.slots.borrow();
        operation(
            slots
                .get(local.get() as usize)
                .expect("missing value assignment"),
        )
    }
    fn with_part<R>(
        &self,
        local: ValLocalIdx,
        part: u32,
        operation: impl FnOnce(AssignmentPartRef<'_, '_, '_>) -> R,
    ) -> R {
        let slots = self.slots.borrow();
        operation(
            slots
                .part(local.get() as usize, part)
                .expect("missing value assignment"),
        )
    }
    fn liveness(&self, local: ValLocalIdx) -> LivenessInfo {
        *self.analyzer.liveness_info(local)
    }
    fn regalloc_event(&self, event: RegallocEvent) {
        // The target is borrowed mutably only between functions (analysis,
        // prologue/epilogue), never while an instruction emits.
        if let Ok(target) = self.target.try_borrow() {
            target.regalloc_event(self.adaptor, event, self.analyzer.block_ref(self.current.get()));
        }
    }
    fn current_block(&self) -> BlockIndex {
        self.current.get()
    }
    fn may_change_value_state(&self) -> bool {
        !self.branch.get()
    }
    fn shadow_assignment(&self, local: ValLocalIdx) -> bool {
        T::PHI_REUSE && self.shadows.borrow()[local.get() as usize]
    }
    fn release_assignment(&self, local: ValLocalIdx) {
        if !self.with_assignment(local, |a| a.delay_free()) {
            self.free_assignment(local);
            return;
        }
        let last = self.liveness(local).last.0 as usize;
        let head = self.delayed.borrow()[last];
        self.with_assignment(local, |assignment| {
            assignment.set_next_delayed_free_entry(head);
            assignment.set_pending_free(true)
        });
        self.delayed.borrow_mut()[last] = local;
    }
    fn decrement_fixed_count(&self, bank: RegBank) {
        let mut fixed = self.fixed.borrow_mut();
        tpde_assert!(
            fixed[bank.id() as usize] > 0,
            "fixed assignment count underflow"
        );
        fixed[bank.id() as usize] = fixed[bank.id() as usize].wrapping_sub(1);
    }
    fn decrement_pinned_count(&self, bank: RegBank) {
        let mut pinned = self.pinned.borrow_mut();
        tpde_assert!(
            pinned[bank.id() as usize] > 0,
            "pinned assignment count underflow"
        );
        pinned[bank.id() as usize] = pinned[bank.id() as usize].wrapping_sub(1);
    }
    fn select_reg(&self, bank: RegBank) -> PgResult<Reg> {
        let free = self.with_registers(|r| r.find_first_free_excluding(bank, 0));
        if free.valid() {
            return Ok(free);
        }
        let mut candidate = Reg::make_invalid();
        let mut maximum = 0;
        {
            let registers = self.registers;
            let slots = self.slots.borrow();
            let liveness = self.analyzer.liveness_table();
            let candidates = registers.used.get() & RegisterFile::<N, 32>::bank_regs(bank);
            for index in BitSetIterator::<false>::new(candidates) {
                let reg = Reg::new(index as u64);
                if registers.is_fixed(reg) {
                    continue;
                }
                let (local, part) = (registers.reg_local_idx(reg), registers.reg_part(reg));
                tpde_assert_ne!(local, INVALID_VALUE);
                if local == self.fed.get() {
                    continue;
                }
                let ap = slots
                    .part(local.get() as usize, part)
                    .expect("missing value assignment");
                if ap.variable_ref() {
                    candidate = reg;
                    break;
                }
                let (spilled, pending, references) = (
                    ap.stack_valid(),
                    ap.assignment().pending_free(),
                    ap.assignment().references_left(),
                );
                let info = liveness.info(local);
                let distance = info.last.0.wrapping_sub(self.current.get().0);
                let mut refs = if pending { 0 } else { references };
                if T::WEIGHTED_REGALLOC && !pending && info.ref_count != 0 {
                    // Weight still ahead of this value: its static weight
                    // scaled by the references left.
                    refs = ((u64::from(info.weight) * u64::from(references))
                        / u64::from(info.ref_count))
                        .min(0xffff) as u32;
                }
                let score = (u32::from(spilled) << 31)
                    | ((if distance < 0x8000 {
                        0x8000 - distance
                    } else {
                        0
                    }) << 16)
                    | if refs < 0xffff { 0x10000 - refs } else { 1 };
                tpde_assert_ne!(score, 0);
                if score > maximum {
                    maximum = score;
                    candidate = reg
                }
            }
        }
        if candidate.invalid() {
            // Only the fed register is left: take it back without a spill
            // (the value it was given to is not computed yet; its
            // definition allocates afresh).
            let fed = self.fed.get();
            if fed != INVALID_VALUE {
                let reg = self.with_part(fed, 0, |ap| {
                    if ap.register_valid() && ap.bank() == bank { ap.get_reg() } else { Reg::make_invalid() }
                });
                if reg.valid() && !self.with_registers(|r| r.is_fixed(reg)) {
                    self.with_part(fed, 0, |ap| ap.set_register_valid(false));
                    self.with_registers(|r| r.unmark_used(reg));
                    self.fed.set(INVALID_VALUE);
                    return Ok(reg);
                }
            }
            if let Some(reg) = self.borrow_fixed(bank)? {
                return Ok(reg);
            }
            return Err(invalid("ran out of registers for scratch registers"));
        }
        self.evict_reg(candidate)?;
        Ok(candidate)
    }
    fn evict_reg(&self, reg: Reg) -> PgResult<()> {
        if !self.may_change_value_state() {
            return Err(invalid("tpde: a register evicted inside a branch region (edge code must not change value state)"));
        }
        let (local, part) = self.with_registers(|r| {
            tpde_assert!(!r.is_fixed(reg));
            (r.reg_local_idx(reg), r.reg_part(reg))
        });
        tpde_assert_ne!(local, INVALID_VALUE);
        self.with_part(local, part, |ap| {
            tpde_assert!(ap.register_valid());
            tpde_assert_eq!(ap.get_reg(), reg)
        });
        self.spill_part(local, part)?;
        self.with_part(local, part, |ap| ap.set_register_valid(false));
        self.with_registers(|r| r.unmark_used(reg));
        Ok(())
    }
    fn reload_to_reg(&self, reg: Reg, local: ValLocalIdx, part: u32) -> PgResult<()> {
        let (variable, stack) = self.with_part(local, part, |ap| {
            (ap.variable_ref(), ap.is_stack_variable())
        });
        if !variable {
            if let Some(bits) = self.adaptor.local_const(local) {
                let (bank, size) = self.with_part(local, part, |ap| (ap.bank(), ap.part_size()));
                return T::materialize_constant(self, &[bits], bank, size, reg);
            }
            let (offset, size) = self.with_part(local, part, |ap| {
                tpde_assert!(ap.stack_valid());
                (ap.frame_off(), ap.part_size())
            });
            T::load_from_stack(self, reg, offset, size)
        } else if stack {
            T::load_address_of_stack_var(self, reg, local, part)
        } else {
            assert!(
                !T::DEFAULT_VAR_REF_HANDLING,
                "non-stack variable requires target handler"
            );
            T::load_address_of_var_reference(self, reg, local, part)
        }
    }
    fn mov(&self, destination: Reg, source: Reg, size: u32) -> PgResult<()> {
        T::mov(self, destination, source, size)
    }
    fn materialize_constant(
        &self,
        words: &[u64],
        bank: RegBank,
        size: u32,
        reg: Reg,
    ) -> PgResult<()> {
        T::materialize_constant(self, words, bank, size, reg)
    }
    fn generate_raw_intext(
        &self,
        destination: Reg,
        source: Reg,
        sign: bool,
        from: u32,
        to: u32,
    ) -> PgResult<()> {
        T::generate_raw_intext(self, destination, source, sign, from, to)
    }
    fn gval_expr_as_reg<'c>(&'c self, value: &mut GenericValuePart<'c, Self, N>) -> PgResult<Reg> {
        T::gval_expr_as_reg(self, value)
    }
    fn special_part<'c>(
        &'c self,
        special: &'c Self::Special,
        part: u32,
    ) -> ValuePartRef<'c, Self, N> {
        T::special_part(self, special, part)
    }
}

impl<
        's,
        'a,
        'm,
        'id,
        A: CompilerAdaptor,
        T: CompilerTarget<A, N>,
        O: CompilerOutput<'m>,
        C: CallingConventionAssigner,
        const N: usize,
    > BranchMachine<N> for FunctionSession<'s, 'a, 'm, 'id, A, T, O, C, N>
{
    type Adaptor = A;
    type Jump = T::Jump;
    fn adaptor(&self) -> &A {
        self.adaptor
    }
    fn block_idx(&self, block: A::BlockRef) -> BlockIndex {
        self.analyzer.block_idx(self.adaptor, block)
    }
    fn block_ref(&self, index: BlockIndex) -> A::BlockRef {
        self.analyzer.block_ref(index)
    }
    fn block_has_phis(&self, index: BlockIndex) -> bool {
        self.analyzer.block_has_phis(self.adaptor, index)
    }
    fn coalesces_phis(&self) -> bool {
        T::PHI_COALESCING
    }
    fn anchor_phi(&self, phi: A::ValueRef, incoming: A::ValueRef) -> PgResult<bool> {
        FunctionSession::anchor_phi(self, phi, incoming)
    }
    fn phi_weight(&self, value: A::ValueRef) -> u32 {
        self.value_weight(value, self.adaptor.val_local_idx(value))
    }
    fn block_has_multiple_incoming(&self, block: A::BlockRef) -> bool {
        self.analyzer
            .block_ref_has_multiple_incoming(self.adaptor, block)
    }
    fn block_label(&self, index: BlockIndex) -> Label {
        self.labels.borrow()[index.0 as usize]
    }
    fn label_create(&self) -> PgResult<Label> {
        self.output.borrow_mut().label_create()
    }
    fn label_place(&self, label: Label) -> PgResult<()> {
        self.output.borrow_mut().label_place(label)
    }
    fn align_text(&self, alignment: u32) -> PgResult<()> {
        self.output.borrow_mut().align(alignment as usize)
    }
    fn unconditional_jump(&self) -> T::Jump {
        T::unconditional_jump()
    }
    fn invert_jump(&self, jump: T::Jump) -> T::Jump {
        T::invert_jump(jump)
    }
    fn generate_raw_jump(&self, jump: T::Jump, label: Label) -> PgResult<()> {
        T::generate_raw_jump(self, jump, label)
    }
    fn spill_part(&self, local: ValLocalIdx, part: u32) -> PgResult<()> {
        FunctionSession::spill_part(self, local, part)
    }
    fn free_reg(&self, reg: Reg) {
        FunctionSession::free_reg(self, reg)
    }
    fn allocate_spill_part(&self, local: ValLocalIdx, part: u32) -> PgResult<()> {
        FunctionSession::allocate_spill_part(self, local, part)
    }
    fn load_from_stack(&self, reg: Reg, offset: i32, size: u32) -> PgResult<()> {
        T::load_from_stack(self, reg, offset, size)
    }
    fn spill_reg(&self, reg: Reg, offset: i32, size: u32) -> PgResult<()> {
        T::spill_reg(self, reg, offset, size)
    }
    fn allocate_stack_slot(&self, size: u32) -> PgResult<i32> {
        self.frame
            .borrow_mut()
            .allocate(size, T::FRAME_INDEXING_NEGATIVE)
    }
    fn free_stack_slot(&self, offset: i32, size: u32) {
        self.frame.borrow_mut().free(offset, size)
    }
    fn free_stack_slot_reclassified(
        &self,
        offset: i32,
        allocated_size: u32,
        freed_size: u32,
    ) -> PgResult<()> {
        self.frame
            .borrow_mut()
            .free_reclassified(offset, allocated_size, freed_size)
    }
    fn value_ref(&self, value: A::ValueRef) -> PgResult<ValueRef<'_, Self, N>> {
        FunctionSession::value_ref(self, value)
    }
    fn result_ref(&self, value: A::ValueRef) -> PgResult<ValueRef<'_, Self, N>> {
        FunctionSession::result_ref(self, value)
    }
    fn assignment_exists(&self, local: ValLocalIdx) -> bool {
        self.slots.borrow().get(local.get() as usize).is_some()
    }
    fn set_branch_region(&self, active: bool) {
        // The flag itself must not sit inside an assertion: `tpde_assert!`
        // is compiled out of release builds, and a region never entered let
        // the release compiler evict and demote inside one edge's phi moves
        // (`gravity_tpde/tests/edge_region.rs`: the store landed on that
        // edge only, the other edge reached the join with the slot unwritten).
        let previous = self.branch.replace(active);
        tpde_assert_ne!(previous, active, "invalid branch region transition");
        GuardMachine::regalloc_event(
            self,
            if active {
                RegallocEvent::RegionBegin
            } else {
                RegallocEvent::RegionEnd
            },
        );
    }
    fn record_failure(&self, error: Box<PgError>) {
        let mut failure = self.failure.borrow_mut();
        if failure.is_none() {
            *failure = Some(error)
        }
    }
    fn with_phi_scratch<R>(
        &self,
        operation: impl FnOnce(&mut PhiScratch<'_, A::ValueRef>) -> PgResult<R>,
    ) -> PgResult<R> {
        operation(&mut self.phi.borrow_mut())
    }
    fn with_switch_scratch<R>(
        &self,
        operation: impl FnOnce(&mut SwitchScratch<'_, A::BlockRef>) -> PgResult<R>,
    ) -> PgResult<R> {
        operation(&mut self.switch.borrow_mut())
    }
    fn switch_emit_cmpeq(
        &self,
        label: Label,
        cmp: Reg,
        tmp: Reg,
        value: u64,
        width32: bool,
    ) -> PgResult<()> {
        T::switch_emit_cmpeq(self, label, cmp, tmp, value, width32)
    }
    fn switch_emit_binary_step(
        &self,
        equal: Label,
        greater: Label,
        cmp: Reg,
        tmp: Reg,
        value: u64,
        width32: bool,
    ) -> PgResult<()> {
        T::switch_emit_binary_step(self, equal, greater, cmp, tmp, value, width32)
    }
    fn switch_create_jump_table(
        &self,
        default: Label,
        cmp: Reg,
        tmp: Reg,
        low: u64,
        high: u64,
        width32: bool,
        fill: impl FnOnce(&mut [Label]),
    ) -> PgResult<bool> {
        T::switch_create_jump_table(self, default, cmp, tmp, low, high, width32, fill)
    }
}

impl<
        's,
        'a,
        'm,
        'id,
        A: CompilerAdaptor,
        T: CompilerTarget<A, N>,
        O: CompilerOutput<'m>,
        C: CallingConventionAssigner,
        const N: usize,
    > CallMachine<N> for FunctionSession<'s, 'a, 'm, 'id, A, T, O, C, N>
{
    type CallAdaptor = A;
    const PLATFORM_POINTER_SIZE: u32 = T::PLATFORM_POINTER_SIZE;
    fn call_adaptor(&self) -> &A {
        self.adaptor
    }
    fn call_val_parts_count(&self, value: &A::ValueRef) -> u32 {
        self.target.borrow().val_parts(self.adaptor, *value).count()
    }
    fn call_val_idx(&self, value: &A::ValueRef) -> ValLocalIdx {
        self.adaptor.val_local_idx(*value)
    }
    fn call_val_ref(&self, value: &A::ValueRef) -> PgResult<ValueRef<'_, Self, N>> {
        self.value_ref(*value)
    }
    fn call_result_ref(&self, value: &A::ValueRef) -> PgResult<ValueRef<'_, Self, N>> {
        self.result_ref(*value)
    }
    fn call_is_leaf_function(&self) -> bool {
        self.frame.borrow().is_leaf_function
    }
    fn call_spill_part(&self, local: ValLocalIdx, part: u32) -> PgResult<()> {
        // Unconditionally: a fixed register is written in place by the phi
        // edge moves (`move_to_phi`) and by in-register updates without the
        // part's `modified` flag following, so `stack_valid` says nothing
        // about the slot's freshness; the register is the authoritative
        // copy and the reload after the call must see exactly it.
        let variable = self.with_part(local, part, |ap| ap.variable_ref());
        if variable {
            return Ok(());
        }
        assert!(self.with_part(local, part, |ap| ap.register_valid()));
        if self.adaptor.local_const(local).is_some() {
            // Rematerialized after the call (`reload_to_reg`).
            return Ok(self.with_part(local, part, |ap| ap.set_stack_valid()));
        }
        self.allocate_spill_part(local, part)?;
        let (reg, offset, size) = self.with_part(local, part, |ap| {
            (ap.get_reg(), ap.frame_off(), ap.part_size())
        });
        T::spill_reg(self, reg, offset, size)?;
        GuardMachine::regalloc_event(self, RegallocEvent::CallSave);
        self.with_part(local, part, |ap| ap.set_stack_valid());
        Ok(())
    }
    fn call_set_generated_call(&self) {
        self.frame.borrow_mut().generated_call = true
    }
    fn call_free_assignment_if_present(&self, local: ValLocalIdx) {
        if self.assignment_exists(local) {
            self.free_assignment(local)
        }
    }
    fn call_init_stack_variable(&self, local: ValLocalIdx, frame_off: i32) -> PgResult<()> {
        self.init_variable_ref(local, 0)?;
        self.with_assignment(local, |a| {
            a.set_stack_variable(true);
            a.set_frame_off(frame_off)
        });
        Ok(())
    }
    fn prologue_assign_arg_part(
        &self,
        value: ValuePartRef<'_, Self, N>,
        assignment: tpde_core::calling_convention::CCAssignment,
    ) -> PgResult<Option<i32>> {
        T::prologue_assign_arg_part(self, value, assignment)
    }
    fn gen_func_epilog(&self) -> PgResult<()> {
        T::gen_func_epilog(self)
    }
    fn release_regs_after_return(&self) {
        branches::release_regs_after_return(self)
    }
}

impl<
        's,
        'a,
        'm,
        'id,
        A: CompilerAdaptor,
        T: CompilerTarget<A, N>,
        O: CompilerOutput<'m>,
        C: CallingConventionAssigner,
        const N: usize,
    > TargetMachine<'m, A, N> for FunctionSession<'s, 'a, 'm, 'id, A, T, O, C, N>
{
    type Target = T;
    type Output = O;
    type Assigner = C;
    fn with_target<R>(&self, operation: impl FnOnce(&mut T) -> R) -> R {
        operation(&mut self.target.borrow_mut())
    }
    #[inline(always)]
    fn with_output<R>(&self, operation: impl FnOnce(&mut O) -> R) -> R {
        operation(&mut self.output.borrow_mut())
    }
    fn with_cur_cc_assigner<R>(&self, operation: impl FnOnce(&mut C) -> R) -> R {
        FunctionSession::with_cur_cc_assigner(self, operation)
    }
    fn frame_size(&self) -> u32 {
        self.frame.borrow().frame_size
    }
    fn set_frame_size(&self, size: u32) {
        self.frame.borrow_mut().frame_size = size
    }
    fn frame_has_dynamic_alloca(&self) -> bool {
        self.frame.borrow().has_dynamic_alloca
    }
    fn frame_generated_call(&self) -> bool {
        self.frame.borrow().generated_call
    }
    fn frame_used(&self) -> bool {
        self.frame.borrow().frame_used
    }
    fn set_frame_used(&self, used: bool) {
        self.frame.borrow_mut().frame_used = used
    }
    fn function_symbol(&self, index: u32) -> SymRef {
        self.symbols[index as usize]
    }
    fn personality_symbol(&self) -> PgResult<SymRef> {
        FunctionSession::personality_symbol(self)
    }
    fn init_variable_ref(&self, local: ValLocalIdx, data: u32) -> PgResult<()> {
        FunctionSession::init_variable_ref(self, local, data)
    }
    fn init_assignment(&self, value: A::ValueRef, local: ValLocalIdx) -> PgResult<()> {
        FunctionSession::init_assignment(self, value, local)
    }
    fn assignment_slot_count(&self) -> usize {
        self.slots.borrow().len()
    }
    fn fixed_assignment_count(&self, bank: RegBank) -> u32 {
        self.fixed.borrow()[bank.id() as usize]
    }
    fn result_ref_alias(
        &self,
        value: A::ValueRef,
        source: ValueRef<'_, Self, N>,
    ) -> PgResult<ValueRef<'_, Self, N>> {
        FunctionSession::result_ref_alias(self, value, source)
    }
    fn result_ref_stack_slot(
        &self,
        value: A::ValueRef,
        base_local: ValLocalIdx,
        base_part: u32,
        offset: i32,
    ) -> PgResult<ValueRef<'_, Self, N>> {
        FunctionSession::result_ref_stack_slot(self, value, base_local, base_part, offset)
    }
    fn with_value_ref_single<R>(
        &self,
        value: A::ValueRef,
        operation: impl FnOnce(&ValueRef<'_, Self, N>, ValuePartRef<'_, Self, N>) -> R,
    ) -> PgResult<R> {
        FunctionSession::with_value_ref_single(self, value, operation)
    }
    fn with_result_ref_single<R>(
        &self,
        value: A::ValueRef,
        operation: impl FnOnce(&ValueRef<'_, Self, N>, ValuePartRef<'_, Self, N>) -> R,
    ) -> PgResult<R> {
        FunctionSession::with_result_ref_single(self, value, operation)
    }
    fn evict_part(&self, local: ValLocalIdx, part: u32) -> PgResult<()> {
        FunctionSession::evict_part(self, local, part)
    }
    fn reloc_text(&self, symbol: SymRef, kind: u32, offset: u64, addend: i64) -> PgResult<()> {
        FunctionSession::reloc_text(self, symbol, kind, offset, addend)
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TLSModel {
    GlobalDynamic,
    LocalDynamic,
    InitialExec,
    LocalExec,
}

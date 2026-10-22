// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use core::mem::MaybeUninit;
use tpde_compiler::{
    target::{CompilerOutput, TargetMachine},
    CompilerAdaptor, ValuePartRef,
};
use tpde_core::{
    calling_convention::{CCAssignment, CallingConventionAssigner},
    Reg, RegBank,
};
use tpde_disarm::*;
use tpde_util::dwarf;
use types_error::PgResult;

use crate::{invalid, raw, A64CompilerState};

fn checked_word(word: u32) -> PgResult<u32> {
    if word == 0 {
        Err(invalid("A64 frame instruction is not encodable"))
    } else {
        Ok(word)
    }
}

fn offset32(offset: usize) -> PgResult<u32> {
    u32::try_from(offset).map_err(|_| invalid("A64 text offset exceeds 32 bits"))
}

fn final_frame_size(size: u32) -> PgResult<u32> {
    let size = size
        .checked_add(15)
        .ok_or_else(|| invalid("A64 frame size overflow"))?
        & !15;
    let size = if size > 4095 {
        size.checked_add(4095)
            .ok_or_else(|| invalid("A64 frame size overflow"))?
            & !4095
    } else {
        size
    };
    if size >= 16 * 1024 * 1024 {
        return Err(invalid("A64 frame exceeds the encodable stack adjustment"));
    }
    Ok(size)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SaveSlot {
    first: u8,
    second: Option<u8>,
    offset: u32,
}

struct SaveSlots {
    remaining: u64,
    offset: u32,
}

impl SaveSlots {
    fn new(registers: u64) -> Self {
        Self {
            remaining: registers,
            offset: 16,
        }
    }
}

impl Iterator for SaveSlots {
    type Item = SaveSlot;

    fn next(&mut self) -> Option<SaveSlot> {
        if self.remaining == 0 {
            return None;
        }
        let first = self.remaining.trailing_zeros() as u8;
        self.remaining &= self.remaining - 1;
        let candidate = self.remaining.trailing_zeros() as u8;
        let second = if self.remaining != 0 && first / 32 == candidate / 32 {
            self.remaining &= self.remaining - 1;
            Some(candidate)
        } else {
            None
        };
        let slot = SaveSlot {
            first,
            second,
            offset: self.offset,
        };
        self.offset += if second.is_some() { 16 } else { 8 };
        Some(slot)
    }
}

fn save_word(slot: SaveSlot, base: GRegSp, restore: bool) -> PgResult<u32> {
    let off = i64::from(slot.offset);
    let word = match (slot.first < 32, slot.second, restore) {
        (true, Some(second), false) => de64_STPx(
            GRegZr::new(slot.first).unwrap(),
            GRegZr::new(second).unwrap(),
            base,
            off,
        ),
        (true, Some(second), true) => de64_LDPx(
            GRegZr::new(slot.first).unwrap(),
            GRegZr::new(second).unwrap(),
            base,
            off,
        ),
        (false, Some(second), false) => de64_STPd(
            VReg::new(slot.first - 32).unwrap(),
            VReg::new(second - 32).unwrap(),
            base,
            off,
        ),
        (false, Some(second), true) => de64_LDPd(
            VReg::new(slot.first - 32).unwrap(),
            VReg::new(second - 32).unwrap(),
            base,
            off,
        ),
        (true, None, false) => de64_STRxu(GRegZr::new(slot.first).unwrap(), base, off as u64),
        (true, None, true) => de64_LDRxu(GRegZr::new(slot.first).unwrap(), base, off as u64),
        (false, None, false) => de64_STRdu(VReg::new(slot.first - 32).unwrap(), base, off as u64),
        (false, None, true) => de64_LDRdu(VReg::new(slot.first - 32).unwrap(), base, off as u64),
    };
    checked_word(word)
}

pub fn prologue_begin<
    'm,
    A: CompilerAdaptor,
    M: TargetMachine<'m, A, 2>,
    C: CallingConventionAssigner,
>(
    machine: &M,
    assigner: &mut C,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    let start = machine.with_output(|output| offset32(output.offset()))?;
    let saved = assigner.get_ccinfo().callee_saved_regs();
    let gp = (saved as u32).count_ones();
    let fp = (saved >> 32).count_ones();
    let allocation = 4 * (gp.div_ceil(2) + fp.div_ceil(2)) + 12;
    let frame = 16 + ((gp * 8 + fp * 8 + 15) & !15);
    machine.with_target(|target| {
        target.a64_clear_returns();
        let state = target.a64_state_mut();
        state.callee_saved_regs = saved;
        state.func_start_off = start;
        state.func_prologue_alloc = allocation;
        state.func_arg_stack_add_off = u32::MAX;
    });
    machine.set_frame_size(frame);
    machine.with_output(|output| output.skip(allocation as usize))?;
    if machine.adaptor().cur_is_vararg() {
        machine.set_frame_used(true);
        machine.with_target(|target| target.a64_state_mut().reg_save_frame_off = frame);
        machine.set_frame_size(frame + 208);
        machine.with_output(|output| output.ensure_space(32))?;
        for first in [0u8, 2, 4, 6] {
            raw::emit(
                machine,
                de64_STPx(
                    GRegZr::new(first).unwrap(),
                    GRegZr::new(first + 1).unwrap(),
                    DA_SP,
                    i64::from(frame + u32::from(first) * 8),
                ),
            )?;
        }
        for first in [0u8, 2, 4, 6] {
            raw::emit(
                machine,
                de64_STPq(
                    VReg::new(first).unwrap(),
                    VReg::new(first + 1).unwrap(),
                    DA_SP,
                    i64::from(frame + 64 + u32::from(first) * 16),
                ),
            )?;
        }
    }
    Ok(())
}

fn argument_stack_base<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
) -> PgResult<Reg>
where
    M::Target: A64CompilerState,
{
    let stack = Reg::new(17);
    if machine.with_registers(|registers| registers.allocatable.get() & (1 << stack.id()) != 0) {
        return Err(invalid(
            "A64 argument stack scratch register is allocatable",
        ));
    }
    let needs_patch =
        machine.with_target(|target| target.a64_state().func_arg_stack_add_off == u32::MAX);
    if needs_patch {
        let offset = machine.with_output(|output| offset32(output.offset()))?;
        raw::emit(machine, de64_ADDxi(GRegSp::new(17).unwrap(), DA_SP, 0))?;
        machine.with_target(|target| {
            let state = target.a64_state_mut();
            state.func_arg_stack_add_off = offset;
            state.func_arg_stack_add_reg = stack;
        });
    }
    Ok(stack)
}

pub fn prologue_assign_arg_part<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    mut part: ValuePartRef<'_, M, 2>,
    assignment: CCAssignment,
) -> PgResult<Option<i32>>
where
    M::Target: A64CompilerState,
{
    if assignment.reg.valid() {
        part.set_value_reg(assignment.reg)?;
        machine.with_registers(|registers| {
            registers
                .allocatable
                .set(registers.allocatable.get() | 1u64 << assignment.reg.id());
        });
        return Ok(None);
    }
    let destination = part.alloc_reg()?;
    let stack = argument_stack_base(machine)?;
    if assignment.byval() {
        let word = de64_ADDxi(
            raw::sp(destination)?,
            raw::sp(stack)?,
            i64::from(assignment.stack_off),
        );
        if word != 0 {
            raw::emit(machine, word)?;
        } else {
            raw::materialize_constant(
                machine,
                &[u64::from(assignment.stack_off)],
                RegBank::new(0),
                8,
                destination,
            )?;
            raw::emit(
                machine,
                de64_ADDx(
                    raw::zr(destination)?,
                    raw::zr(stack)?,
                    raw::zr(destination)?,
                ),
            )?;
        }
    } else {
        raw::load_off(
            machine,
            destination,
            stack,
            assignment.stack_off,
            assignment.size,
            false,
        )?;
    }
    Ok(None)
}

fn argument_count(mask: u64) -> u32 {
    8 - (((mask & 0xff) as u16) << 8 | 0x80).leading_zeros()
}

pub fn prologue_end<
    'm,
    A: CompilerAdaptor,
    M: TargetMachine<'m, A, 2>,
    C: CallingConventionAssigner,
>(
    machine: &M,
    assigner: &mut C,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    if machine.adaptor().cur_is_vararg() {
        machine.set_frame_used(true);
        let stack = argument_stack_base(machine)?;
        raw::emit(
            machine,
            de64_ADDxi(
                raw::sp(stack)?,
                raw::sp(stack)?,
                i64::from(assigner.get_stack_size()),
            ),
        )?;
        let saved = machine.with_target(|target| target.a64_state().reg_save_frame_off);
        raw::emit(
            machine,
            de64_STRxu(
                raw::zr(stack)?,
                GRegSp::new(29).unwrap(),
                u64::from(saved + 192),
            ),
        )?;
        let args = machine.with_registers(|registers| {
            registers.allocatable.get() & assigner.get_ccinfo().arg_regs()
        });
        machine.with_target(|target| {
            let state = target.a64_state_mut();
            state.scalar_arg_count = argument_count(args);
            state.vec_arg_count = argument_count(args >> 32);
        });
    }
    Ok(())
}

pub fn finish_func<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    index: u32,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    let saved = machine.with_cur_cc_assigner(|assigner| {
        machine.with_registers(|registers| {
            registers.clobbered.get() & assigner.get_ccinfo().callee_saved_regs()
        })
    });
    let dynamic = machine.frame_has_dynamic_alloca();
    let stack = if dynamic {
        GRegSp::new(29).unwrap()
    } else {
        DA_SP
    };
    let frame = final_frame_size(machine.frame_size())?;
    let needs_frame = machine.frame_used()
        || machine.frame_generated_call()
        || dynamic
        || saved != 0
        || machine.with_registers(|registers| registers.clobbered.get() & (1 << 30) != 0);
    if needs_frame && frame < 16 {
        return Err(invalid("A64 frame has no space for FP and LR"));
    }
    let (start, allocation, arg_offset, arg_reg, has_returns) = machine.with_target(|target| {
        let state = target.a64_state();
        (
            state.func_start_off,
            state.func_prologue_alloc,
            state.func_arg_stack_add_off,
            state.func_arg_stack_add_reg,
            !target.a64_return_offsets().is_empty(),
        )
    });
    let personality = machine.personality_symbol()?;
    machine.with_output(|output| output.with_base(|base| base.eh_begin_fde(personality)))?;
    let mut prologue = [MaybeUninit::<u32>::uninit(); 35];
    let mut count = 0usize;
    if needs_frame {
        machine.with_output(|output| {
            output.with_base(|base| -> PgResult<()> {
                if has_returns && frame <= 0x1f8 {
                    base.eh_write_inst(dwarf::DW_CFA_remember_state)?;
                }
                base.eh_write_inst1(dwarf::DW_CFA_advance_loc, 1)?;
                base.eh_write_inst1(dwarf::DW_CFA_def_cfa_offset, u64::from(frame))?;
                if frame <= 0x1f8 {
                    prologue[count].write(checked_word(de64_STPx_pre(
                        GRegZr::new(29).unwrap(),
                        GRegZr::new(30).unwrap(),
                        DA_SP,
                        -i64::from(frame),
                    ))?);
                    count += 1;
                } else {
                    if has_returns {
                        base.eh_write_inst(dwarf::DW_CFA_remember_state)?;
                    }
                    prologue[count].write(checked_word(de64_SUBxi(
                        DA_SP,
                        DA_SP,
                        i64::from(frame),
                    ))?);
                    prologue[count + 1].write(checked_word(de64_STPx(
                        GRegZr::new(29).unwrap(),
                        GRegZr::new(30).unwrap(),
                        DA_SP,
                        0,
                    ))?);
                    count += 2;
                }
                prologue[count].write(checked_word(de64_MOV_SPx(GRegSp::new(29).unwrap(), DA_SP))?);
                count += 1;
                let advance_offset = base.eh_size();
                base.eh_write_inst1(dwarf::DW_CFA_advance_loc, 0)?;
                base.eh_write_inst1(
                    dwarf::DW_CFA_def_cfa_register,
                    u64::from(dwarf::a64::DW_reg_fp),
                )?;
                base.eh_write_inst2(
                    dwarf::DW_CFA_offset,
                    u64::from(dwarf::a64::DW_reg_fp),
                    u64::from(frame / 8),
                )?;
                base.eh_write_inst2(
                    dwarf::DW_CFA_offset,
                    u64::from(dwarf::a64::DW_reg_lr),
                    u64::from(frame / 8 - 1),
                )?;
                for slot in SaveSlots::new(saved) {
                    for (register, offset) in [
                        (Some(slot.first), slot.offset),
                        (slot.second, slot.offset + 8),
                    ] {
                        let Some(register) = register else { continue };
                        let dwarf_reg = if register < 32 {
                            register
                        } else {
                            register + 32
                        };
                        let opcode = if dwarf_reg & dwarf::DWARF_CFI_PRIMARY_OPCODE_MASK == 0 {
                            dwarf::DW_CFA_offset
                        } else {
                            dwarf::DW_CFA_offset_extended
                        };
                        let cfa_offset = frame
                            .checked_sub(offset)
                            .ok_or_else(|| invalid("A64 save slot exceeds frame"))?
                            / 8;
                        base.eh_write_inst2(opcode, u64::from(dwarf_reg), u64::from(cfa_offset))?;
                    }
                    prologue[count].write(save_word(slot, stack, false)?);
                    count += 1;
                }
                if count * 4 > allocation as usize {
                    return Err(invalid("A64 prologue exceeds its reservation"));
                }
                base.eh_data_mut()?[advance_offset] = dwarf::DW_CFA_advance_loc | (count - 1) as u8;
                let data = base.data_mut()?;
                let destination = data
                    .get_mut(start as usize..start as usize + count * 4)
                    .ok_or_else(|| invalid("A64 prologue patch exceeds text"))?;
                for (bytes, word) in destination.chunks_exact_mut(4).zip(&prologue[..count]) {
                    // SAFETY: count advances only after each corresponding word is written.
                    let word = unsafe { word.assume_init() };
                    bytes.copy_from_slice(&word.to_le_bytes());
                }
                Ok(())
            })
        })?;
    }
    if arg_offset != u32::MAX {
        let word = checked_word(if needs_frame {
            de64_ADDxi(raw::sp(arg_reg)?, DA_SP, i64::from(frame))
        } else {
            de64_MOV_SPx(raw::sp(arg_reg)?, DA_SP)
        })?;
        machine.with_output(|output| {
            output.with_base(|base| -> PgResult<()> {
                base.data_mut()?
                    .get_mut(arg_offset as usize..arg_offset as usize + 4)
                    .ok_or_else(|| invalid("A64 argument stack patch exceeds text"))?
                    .copy_from_slice(&word.to_le_bytes());
                Ok(())
            })
        })?;
    }
    if has_returns {
        let end = machine.with_output(|output| offset32(output.offset()))?;
        let trailing = machine.with_target(|target| {
            target.a64_return_offsets().last().copied() == end.checked_sub(4)
        });
        if trailing {
            machine.with_output(|output| {
                output.with_base(|base| base.set_offset((end - 4) as usize))
            })?;
            machine.with_target(|target| target.a64_pop_return());
        }
        let end = machine.with_output(|output| offset32(output.offset()))?;
        machine.with_target(|target| {
            machine.with_output(|output| {
                output.with_base(|base| -> PgResult<()> {
                    let data = base.data_mut()?;
                    for &offset in target.a64_return_offsets() {
                        let delta = end
                            .checked_sub(offset)
                            .ok_or_else(|| invalid("A64 return patch follows epilogue"))?;
                        let word = checked_word(de64_B((delta / 4) as isize))?;
                        data.get_mut(offset as usize..offset as usize + 4)
                            .ok_or_else(|| invalid("A64 return patch exceeds text"))?
                            .copy_from_slice(&word.to_le_bytes());
                    }
                    Ok(())
                })
            })
        })?;
        machine.with_output(|output| output.ensure_space(count * 4 + 4))?;
        if dynamic {
            raw::emit(machine, de64_MOV_SPx(DA_SP, GRegSp::new(29).unwrap()))?;
        }
        for slot in SaveSlots::new(saved) {
            raw::emit(machine, save_word(slot, stack, true)?)?;
        }
        if needs_frame {
            let body_start = start
                .checked_add(allocation)
                .ok_or_else(|| invalid("A64 body offset overflow"))?;
            machine.with_output(|output| -> PgResult<()> {
                let advance = offset32(output.offset())?
                    .checked_sub(body_start)
                    .and_then(|value| value.checked_add(4))
                    .ok_or_else(|| invalid("A64 epilogue CFI offset overflow"))?;
                output.eh_advance(u64::from(advance))?;
                output.with_base(|base| base.eh_write_inst(dwarf::DW_CFA_restore_state))
            })?;
            if frame <= 0x1f8 {
                raw::emit(
                    machine,
                    de64_LDPx_post(
                        GRegZr::new(29).unwrap(),
                        GRegZr::new(30).unwrap(),
                        DA_SP,
                        i64::from(frame),
                    ),
                )?;
            } else {
                raw::emit(
                    machine,
                    de64_LDPx(GRegZr::new(29).unwrap(), GRegZr::new(30).unwrap(), DA_SP, 0),
                )?;
                raw::emit(machine, de64_ADDxi(DA_SP, DA_SP, i64::from(frame)))?;
                machine.with_output(|output| {
                    output.with_base(|base| -> PgResult<()> {
                        base.eh_write_inst1(dwarf::DW_CFA_advance_loc, 1)?;
                        base.eh_write_inst1(dwarf::DW_CFA_def_cfa_offset, 0)
                    })
                })?;
            }
        }
        raw::emit(machine, de64_RET(GRegZr::new(30).unwrap()))?;
    }
    let prologue_size = (count * 4) as u32;
    let symbol = machine.function_symbol(index);
    machine.with_output(|output| -> PgResult<()> {
        output.with_base(|base| {
            base.remove_prologue_bytes(start + prologue_size, allocation - prologue_size)
        })?;
        let size = offset32(output.offset())?
            .checked_sub(start)
            .ok_or_else(|| invalid("A64 function end precedes start"))?;
        let section = output.section();
        output
            .assembler_mut()
            .sym_def(symbol, section, u64::from(start), u64::from(size))?;
        output.with_base(|base| -> PgResult<()> {
            base.eh_end_fde()?;
            base.except_encode_func()
        })
    })
}

pub fn gen_func_epilog<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    let offset = machine.with_output(|output| offset32(output.offset()))?;
    machine.with_target(|target| target.a64_push_return(offset))?;
    machine.with_output(|output| output.skip(4))
}

#[cfg(test)]
#[path = "prologue_tests.rs"]
mod tests;

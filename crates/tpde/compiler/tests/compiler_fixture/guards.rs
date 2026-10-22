// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use super::{adaptor::Adaptor, RecordingTarget};
use tpde_base::ValLocalIdx;
use tpde_compiler::{
    generic_value_part::{gval_as_reg_reuse_scratch, gval_as_reg_reuse_value, Expr},
    target::TargetMachine,
    GenericValuePart, ScratchReg, ValuePartRef, ValueRef,
};
use tpde_core::{Reg, RegBank};
use tpde_test_ir::IRValueRef;
use types_error::PgResult;

pub fn run<'m, 'a, 'ir: 'a, 'log, const NEG: bool, M>(machine: &M) -> PgResult<()>
where
    M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = RecordingTarget<'log, NEG>>,
{
    let record = |tag: &str, fields: &[u64]| RecordingTarget::<NEG>::record(machine, tag, fields);
    let registers = || {
        machine.with_registers(|registers| {
            record(
                "registers",
                &[registers.used.get(), registers.clobbered.get()],
            );
            for id in registers.used_regs() {
                let reg = Reg::new(id as u64);
                record(
                    "register",
                    &[
                        id as u64,
                        u64::from(registers.reg_local_idx(reg).get()),
                        u64::from(registers.reg_part(reg)),
                        u64::from(registers.lock_counts[id as usize / 32][id as usize % 32].get()),
                    ],
                );
            }
        });
    };
    for bank in 0..2 {
        let mut scratch = ScratchReg::new(machine);
        let has_reg = scratch.has_reg();
        let released = scratch.release();
        record(
            "scratch.empty",
            &[u64::from(has_reg), u64::from(released.id())],
        );
        record(
            "scratch.alloc",
            &[u64::from(scratch.alloc(RegBank::new(bank))?.id())],
        );
        record(
            "scratch.same",
            &[u64::from(scratch.alloc(RegBank::new(bank))?.id())],
        );
        registers();
        let mut moved = scratch.take();
        record(
            "scratch.move",
            &[
                u64::from(scratch.has_reg()),
                u64::from(moved.cur_reg().id()),
            ],
        );
        record("scratch.release", &[u64::from(moved.release().id())]);
        registers();
    }
    {
        let empty = ValuePartRef::empty(machine);
        record(
            "part.default",
            &[
                u64::from(empty.bank().id()),
                u64::from(empty.part_size()),
                u64::from(empty.has_assignment()),
                u64::from(empty.is_const()),
                u64::from(empty.has_reg()),
            ],
        );
        let mut owner = ValuePartRef::constant_inline(machine, 19, 8, RegBank::new(1));
        owner.load_to_reg()?;
        {
            let mut borrowed = owner.get_unowned_ref();
            record(
                "part.unowned",
                &[
                    u64::from(borrowed.bank().id()),
                    u64::from(borrowed.part_size()),
                    u64::from(borrowed.has_assignment()),
                    u64::from(borrowed.is_const()),
                    u64::from(borrowed.cur_reg().id()),
                ],
            );
            borrowed.reset();
        }
        record(
            "part.owner",
            &[
                u64::from(owner.cur_reg().id()),
                u64::from(machine.with_registers(|registers| registers.is_fixed(owner.cur_reg()))),
            ],
        );
        owner.reset();
        registers();
    }
    for size in [1, 2, 4, 8] {
        let mut value = GenericValuePart::from(ValuePartRef::constant_inline(
            machine,
            0xfedc_ba98_7654_3210,
            size,
            RegBank::new(0),
        ));
        record(
            "immediate",
            &[
                u64::from(value.is_imm()),
                u64::from(value.imm_size()),
                value.imm64(),
            ],
        );
        let mut destination = ScratchReg::new(machine);
        let reg = gval_as_reg_reuse_scratch(machine, &mut value, &mut destination)?;
        record("generic.scratch", &[u64::from(reg.id())]);
        record(
            "generic.source",
            &[
                u64::from(value.val_ref().is_const()),
                u64::from(value.val_ref().has_reg()),
            ],
        );
        destination.reset();
        let mut target = ValuePartRef::temporary(machine, RegBank::new(1));
        let reg = gval_as_reg_reuse_value(machine, &mut value, &mut target)?;
        record("generic.value", &[u64::from(reg.id())]);
        record(
            "generic.source",
            &[
                u64::from(value.val_ref().is_const()),
                u64::from(value.val_ref().has_reg()),
                u64::from(value.val_ref().bank().id()),
            ],
        );
        target.reset();
        value.reset();
        registers();
    }
    let assignment = |reg: Reg, fixed: bool, local: u32| -> PgResult<()> {
        let local = ValLocalIdx::new(local);
        machine.with_target(|target| target.guard_force_fixed = fixed.then_some(reg));
        let initialized = machine.init_assignment(IRValueRef(local.get()), local);
        machine.with_target(|target| target.guard_force_fixed = None);
        initialized?;
        machine.with_assignment(local, |assignment| {
            assert_eq!(assignment.part_count(), 1);
            assignment.set_frame_off(0);
            assignment.set_references_left(1);
            assignment.set_max_part_size(8);
            assignment.set_pending_free(false);
            assignment.set_variable_ref(false);
            assignment.set_stack_variable(false);
            assignment.set_delay_free(false);
        });
        machine.with_part(local, 0, |part| {
            assert_eq!(part.fixed_assignment(), fixed);
            if fixed {
                assert_eq!(part.get_reg(), reg);
            }
            part.reset();
            part.set_bank(RegBank::new(0));
            part.set_part_size(8);
            part.set_reg(reg);
            part.set_register_valid(true);
            part.set_fixed_assignment(fixed);
        });
        if !fixed {
            machine.with_registers(|registers| {
                registers.mark_used(reg, local, 0);
                registers.mark_clobbered(reg);
            });
        }
        Ok(())
    };
    {
        let local = ValLocalIdx::new(0);
        assignment(Reg::new(2), false, 0)?;
        let mut reference = ValueRef::new(machine, local);
        reference.disown();
        {
            let mut part = reference.part_unowned(0);
            part.alloc_specific(Reg::new(7))?;
            let fields = machine.with_registers(|registers| {
                [
                    u64::from(part.cur_reg().id()),
                    registers.used.get(),
                    u64::from(registers.reg_local_idx(Reg::new(2)).get()),
                    u64::from(registers.reg_local_idx(Reg::new(7)).get()),
                ]
            });
            record("specific.overwrite", &fields);
            part.reset();
        }
        machine.with_registers(|registers| registers.unmark_used(Reg::new(2)));
        machine.release_assignment(local);
        registers();
    }
    {
        let source = ValLocalIdx::new(0);
        let target = ValLocalIdx::new(1);
        assignment(Reg::new(2), false, 0)?;
        assignment(Reg::new(7), false, 1)?;
        let mut reference = ValueRef::new(machine, source);
        reference.disown();
        {
            let mut part = reference.part_unowned(0);
            machine.with_target(|target| target.spill_scratch = true);
            let loaded = part.load_to_specific(Reg::new(7));
            machine.with_target(|target| target.spill_scratch = false);
            loaded?;
            record(
                "specific.after.eviction",
                &[
                    u64::from(part.cur_reg().id()),
                    machine
                        .with_assignment(source, |assignment| assignment.frame_off() as i64 as u64),
                    u64::from(machine.with_part(source, 0, |part| part.register_valid())),
                    u64::from(machine.with_part(target, 0, |part| part.register_valid())),
                    machine.with_registers(|registers| registers.used.get()),
                ],
            );
            part.reset();
        }
        machine.release_assignment(source);
        machine.release_assignment(target);
        registers();
    }
    {
        let local = ValLocalIdx::new(0);
        assignment(Reg::new(2), false, 0)?;
        let mut reference = ValueRef::new(machine, local);
        assert!(reference.is_owned());
        {
            let mut part = reference.part(0);
            part.lock();
            let mut value = GenericValuePart::from(part);
            let mut destination = ValuePartRef::temporary(machine, RegBank::new(1));
            let reg = gval_as_reg_reuse_value(machine, &mut value, &mut destination)?;
            record("assigned.reuse", &[u64::from(reg.id())]);
            record(
                "assigned.source",
                &[
                    u64::from(value.val_ref().has_assignment()),
                    u64::from(value.val_ref().local_idx().get()),
                    u64::from(value.val_ref().part()),
                    u64::from(value.val_ref().has_reg()),
                    u64::from(
                        value
                            .val_ref()
                            .with_assignment(|part| part.register_valid()),
                    ),
                ],
            );
            record(
                "assigned.destination",
                &[
                    u64::from(destination.has_assignment()),
                    u64::from(destination.cur_reg().id()),
                ],
            );
            value.reset();
            destination.reset();
        }
        reference.disown();
        machine.release_assignment(local);
        registers();
    }
    {
        let local = ValLocalIdx::new(0);
        assignment(Reg::new(8), true, 0)?;
        let mut reference = ValueRef::new(machine, local);
        reference.disown();
        {
            let mut destination = reference.part_unowned(0);
            let mut scratch = ScratchReg::new(machine);
            scratch.alloc_specific(Reg::new(2))?;
            let mut value = GenericValuePart::from(scratch);
            let reg = gval_as_reg_reuse_value(machine, &mut value, &mut destination)?;
            record("assigned.fixed", &[u64::from(reg.id())]);
            let GenericValuePart::Scratch(scratch) = &value else {
                unreachable!()
            };
            record(
                "assigned.fixed.state",
                &[
                    u64::from(destination.has_reg()),
                    u64::from(destination.with_assignment(|part| part.get_reg().id())),
                    u64::from(destination.with_assignment(|part| part.fixed_assignment())),
                    u64::from(scratch.has_reg()),
                ],
            );
            value.reset();
            destination.reset();
        }
        machine.release_assignment(local);
        registers();
    }
    {
        let local = ValLocalIdx::new(0);
        assignment(Reg::new(8), false, 0)?;
        machine.with_registers(|registers| registers.unmark_used(Reg::new(8)));
        machine.with_part(local, 0, |part| part.set_register_valid(false));
        let mut reference = ValueRef::new(machine, local);
        reference.disown();
        {
            let mut destination = reference.part_unowned(0);
            let mut scratch = ScratchReg::new(machine);
            scratch.alloc_specific(Reg::new(2))?;
            let mut value = GenericValuePart::from(scratch);
            let reg = gval_as_reg_reuse_value(machine, &mut value, &mut destination)?;
            record("assigned.scratch", &[u64::from(reg.id())]);
            record(
                "assigned.scratch.state",
                &[
                    u64::from(destination.has_reg()),
                    u64::from(destination.cur_reg().id()),
                    u64::from(destination.with_assignment(|part| part.register_valid())),
                    u64::from(
                        machine
                            .with_registers(|registers| registers.is_fixed(destination.cur_reg())),
                    ),
                ],
            );
            value.reset();
            destination.reset();
        }
        machine.release_assignment(local);
        registers();
    }
    {
        let mut base = ScratchReg::new(machine);
        base.alloc_specific(Reg::new(5))?;
        let mut value = GenericValuePart::from(Expr::from_scratch(base, 0));
        let mut destination = ScratchReg::new(machine);
        let reg = gval_as_reg_reuse_scratch(machine, &mut value, &mut destination)?;
        record(
            "expression.trivial",
            &[u64::from(reg.id()), u64::from(destination.has_reg())],
        );
        let GenericValuePart::Expr(expr) = &mut value else {
            unreachable!()
        };
        expr.disp = 24;
        let reg = gval_as_reg_reuse_scratch(machine, &mut value, &mut destination)?;
        record("expression.materialized", &[u64::from(reg.id())]);
        let GenericValuePart::Scratch(scratch) = &value else {
            unreachable!()
        };
        record(
            "expression.owned",
            &[
                u64::from(destination.has_reg()),
                u64::from(scratch.has_reg()),
            ],
        );
        value.reset();
        destination.reset();
        registers();
    }
    Ok(())
}

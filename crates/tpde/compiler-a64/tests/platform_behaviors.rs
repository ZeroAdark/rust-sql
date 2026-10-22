// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#[path = "platform_fixture/adaptor.rs"]
mod adaptor;
#[cfg(all(target_arch = "aarch64", target_os = "linux", not(miri)))]
#[path = "platform_fixture/native.rs"]
mod native;

use adaptor::Adaptor;
use mcx::MemoryContext;
use std::{borrow::ToOwned, format, vec::Vec};
use tpde_assembler::{elf, Architecture, SymRef};
use tpde_compiler::{
    calls::{CallBuilderBase, CallTarget, RetBuilder},
    generic_value_part::{Expr, ExprReg},
    target::{CompilerOutput, CompilerTarget, TargetMachine},
    with_compiler_base, GenericValuePart, ScratchReg, ValuePartRef,
};
use tpde_compiler_a64::{
    calls, prologue, raw, values, A64CompilerState, A64Frontend, CCAssignerAAPCS, CompilerA64,
    Jump, AAPCS_INFO, FP_BANK, GP_BANK,
};
use tpde_core::{
    calling_convention::{CCAssigner, CCAssignment},
    compiler::{ValRefSpecial, ValueParts},
    ir_adaptor::IrAdaptor,
    Reg, RegBank,
};
use tpde_disarm::*;
use tpde_elf_mapper::ElfMapper;
use tpde_function_writer_a64::A64Target;
use tpde_test_ir::{IRFuncRef, IRInstRef, IRValueRef, Op, TestIR};
use types_error::PgResult;

#[derive(Clone, Copy, Debug)]
enum Scenario {
    Frame {
        size: u32,
        saved: u64,
        returns: bool,
    },
    Expression {
        shape: u8,
        owned: u8,
        alias: bool,
        scale: i64,
        disp: i64,
    },
    Alloca {
        dynamic: bool,
        size: u64,
        count: u64,
        align: u32,
        reuse: bool,
    },
    Spill {
        size: u32,
        vector: bool,
        sign: bool,
        high: bool,
    },
    Call {
        indirect: bool,
    },
    WideCall {
        register: u8,
        high: bool,
        indirect: bool,
    },
    StackCall {
        byval: Option<u32>,
    },
    Variadic {
        gp: u32,
        fp: u32,
        byval: bool,
        sret: bool,
    },
    Tls {
        model: calls::TLSModel,
    },
}
impl Scenario {
    fn dynamic(self) -> bool {
        matches!(self, Self::Alloca { .. })
    }
    fn calls(self) -> bool {
        matches!(
            self,
            Self::Call { .. } | Self::WideCall { .. } | Self::StackCall { .. } | Self::Tls { .. }
        )
    }
    fn produces_values(self) -> bool {
        self.dynamic() || matches!(self, Self::WideCall { .. })
    }
}
struct Parts {
    bank: RegBank,
    size: u32,
}
impl ValueParts for Parts {
    fn count(&self) -> u32 {
        1
    }
    fn size_bytes(&self, _: u32) -> u32 {
        self.size
    }
    fn reg_bank(&self, _: u32) -> RegBank {
        self.bank
    }
}
struct Frontend {
    scenario: Scenario,
}
impl<'a, 'ir> A64Frontend<Adaptor<'a, 'ir>> for Frontend {
    type Special = ValRefSpecial<()>;
    type Parts = Parts;
    fn val_parts(&self, adaptor: &Adaptor<'a, 'ir>, value: IRValueRef) -> Parts {
        let name = adaptor
            .ir()
            .name(adaptor.ir().values[value.0 as usize].name);
        Parts {
            size: if name == "v_wide" { 16 } else { 8 },
            bank: if name.starts_with("v_") {
                FP_BANK
            } else {
                GP_BANK
            },
        }
    }
    fn val_ref_special(&self, _: &Adaptor<'a, 'ir>, _: IRValueRef) -> Option<Self::Special> {
        None
    }
    fn cur_func_may_emit_calls(&self, adaptor: &Adaptor<'a, 'ir>) -> bool {
        adaptor.current_function() == 0 && self.scenario.calls()
    }
    fn cur_personality_func(&self, _: &Adaptor<'a, 'ir>) -> SymRef {
        SymRef::default()
    }
    fn define_func_idx(&mut self, _: &Adaptor<'a, 'ir>, function: IRFuncRef, index: u32) {
        assert_eq!(function.0, index);
    }
    fn special_part<'s, 'm, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2>>(
        _: &'s M,
        _: &'s Self::Special,
        _: u32,
    ) -> ValuePartRef<'s, M, 2>
    where
        Adaptor<'a, 'ir>: 's,
        M::Target: CompilerTarget<Adaptor<'a, 'ir>, 2, Special = Self::Special> + A64CompilerState,
    {
        panic!("platform fixture has no special values")
    }
    fn compile_inst<'m, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2>>(
        machine: &M,
        inst: IRInstRef,
        _: impl Iterator<Item = IRInstRef> + Clone,
    ) -> PgResult<bool>
    where
        M::Target:
            A64CompilerState<Frontend = Self> + CompilerTarget<Adaptor<'a, 'ir>, 2, Jump = Jump>,
    {
        let adaptor = machine.adaptor();
        let instruction = inst;
        let inst = adaptor.ir().values[instruction.0 as usize];
        if inst.op == Op::Alloca {
            return Ok(true);
        }
        let scenario = machine.with_target(|target| target.a64_frontend().scenario);
        if scenario.produces_values() && inst.op == Op::Ret {
            let operand = IRValueRef(adaptor.ir().value_operands[inst.op_begin_idx as usize]);
            machine.with_cur_cc_assigner(|assigner| {
                let mut builder = RetBuilder::new(machine, assigner);
                builder.add(&operand)?;
                builder.ret()
            })?;
            return Ok(true);
        }
        if let Scenario::WideCall {
            register,
            high,
            indirect,
        } = scenario
        {
            if adaptor.current_function() == 0 {
                if inst.op_count == 0 {
                    let owner = machine.result_ref(IRValueRef(instruction.0))?;
                    let mut part = owner.part(0);
                    part.alloc_specific(Reg::new(u64::from(register)))?;
                    raw::materialize_constant(
                        machine,
                        &WIDE,
                        FP_BANK,
                        16,
                        part.cur_reg_unlocked(),
                    )?;
                } else {
                    let operand =
                        IRValueRef(adaptor.ir().value_operands[inst.op_begin_idx as usize]);
                    let value = machine.value_ref(operand)?;
                    let slot = machine.frame_size();
                    machine.set_frame_size(slot + 16);
                    machine.set_frame_used(true);
                    assert!(machine.with_part(value.local_idx(), 0, |part| part.register_valid()));
                    emit_local_call(machine, indirect)?;
                    machine.with_part(value.local_idx(), 0, |part| {
                        assert!(
                            !part.register_valid(),
                            "wide preserved vector was not evicted"
                        );
                        assert!(part.stack_valid());
                    });
                    let mut part = value.part(0);
                    let restored = part.load_to_reg()?;
                    raw::store_off(machine, Reg::new(29), slot, restored, 16)?;
                    let result = machine.result_ref(IRValueRef(instruction.0))?;
                    let mut output = result.part(0);
                    let destination = output.alloc_reg()?;
                    raw::load_off(
                        machine,
                        destination,
                        Reg::new(29),
                        slot + u32::from(high) * 8,
                        8,
                        false,
                    )?;
                }
                return Ok(true);
            }
        }
        if inst.op_count != 0 {
            let operand = IRValueRef(adaptor.ir().value_operands[inst.op_begin_idx as usize]);
            drop(machine.value_ref(operand)?);
        }
        if adaptor.current_function() != 0 {
            if matches!(scenario, Scenario::WideCall { .. }) {
                machine.with_registers(|registers| {
                    registers
                        .clobbered
                        .set(registers.clobbered.get() | 0xff << 40)
                });
                for register in 40..48 {
                    raw::materialize_constant(machine, &[0, 0], FP_BANK, 16, Reg::new(register))?;
                }
            }
            if let Scenario::StackCall { byval } = scenario {
                if let Some(size) = byval {
                    raw::materialize_constant(machine, &[0], GP_BANK, 8, Reg::new(0))?;
                    for offset in 0..size {
                        raw::load_off(machine, Reg::new(16), Reg::new(31), offset, 1, false)?;
                        raw::emit(
                            machine,
                            de64_ADDx(
                                raw::zr(Reg::new(0))?,
                                raw::zr(Reg::new(0))?,
                                raw::zr(Reg::new(16))?,
                            ),
                        )?;
                    }
                } else {
                    raw::load_off(machine, Reg::new(0), Reg::new(31), 0, 8, false)?;
                }
            } else {
                raw::materialize_constant(machine, &[4242], GP_BANK, 8, Reg::new(0))?;
            }
        } else {
            execute(machine, scenario, IRValueRef(instruction.0))?;
        }
        if scenario.produces_values() && adaptor.current_function() == 0 {
            return Ok(true);
        }
        machine.with_cur_cc_assigner(|assigner| RetBuilder::new(machine, assigner).ret())?;
        Ok(true)
    }
}

const WIDE: [u64; 2] = [0xfedc_ba98_f654_b281, 0x1234_5678_9abc_def0];

fn emit_local_call<'m, 'a, 'ir: 'a, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2>>(
    machine: &M,
    indirect: bool,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    let symbol = machine.function_symbol(1);
    let target = if indirect {
        let mut target = ValuePartRef::temporary(machine, GP_BANK);
        target.alloc_specific(Reg::new(2))?;
        machine.with_output(|output| output.ensure_space(8))?;
        let offset = machine.with_output(|output| output.offset());
        machine.reloc_text(symbol, elf::R_AARCH64_ADR_PREL_PG_HI21, offset as u64, 0)?;
        raw::emit(machine, de64_ADRP(raw::zr(Reg::new(2))?, 0, 0))?;
        let offset = machine.with_output(|output| output.offset());
        machine.reloc_text(symbol, elf::R_AARCH64_ADD_ABS_LO12_NC, offset as u64, 0)?;
        raw::emit(
            machine,
            de64_ADDxi(raw::sp(Reg::new(2))?, raw::sp(Reg::new(2))?, 0),
        )?;
        CallTarget::Value(target)
    } else {
        CallTarget::Symbol(symbol)
    };
    calls::generate_call(machine, target, &[], None, false)
}

fn execute<'m, 'a, 'ir: 'a, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2>>(
    machine: &M,
    scenario: Scenario,
    result_value: IRValueRef,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    match scenario {
        Scenario::Frame {
            size,
            saved,
            returns,
        } => {
            machine.set_frame_size(size);
            machine.set_frame_used(size != 0);
            machine.with_registers(|registers| {
                registers.clobbered.set(registers.clobbered.get() | saved)
            });
            for register in 0..64 {
                if saved & (1 << register) != 0 {
                    raw::materialize_constant(
                        machine,
                        &[0xdead_0000 + register],
                        if register < 32 { GP_BANK } else { FP_BANK },
                        8,
                        Reg::new(register),
                    )?;
                }
            }
            raw::materialize_constant(machine, &[42], GP_BANK, 8, Reg::new(0))?;
            if returns {
                prologue::gen_func_epilog(machine)?;
                raw::emit(machine, de64_NOP())?;
            }
        }
        Scenario::Expression {
            shape,
            owned,
            alias,
            scale,
            disp,
        } => {
            let mut base = ScratchReg::new(machine);
            let mut index = ScratchReg::new(machine);
            let base_reg = base.alloc_gp()?;
            let index_reg = if alias { base_reg } else { index.alloc_gp()? };
            raw::materialize_constant(machine, &[17], GP_BANK, 8, base_reg)?;
            if !alias {
                raw::materialize_constant(machine, &[11], GP_BANK, 8, index_reg)?;
            }
            let mut expr = Expr::new();
            if shape & 1 != 0 {
                expr.base = if owned & 1 != 0 {
                    ExprReg::Scratch(base.take())
                } else {
                    ExprReg::Borrowed(base_reg)
                };
            }
            if shape & 2 != 0 {
                expr.index = if owned & 2 != 0 {
                    ExprReg::Scratch(if alias { base.take() } else { index.take() })
                } else {
                    ExprReg::Borrowed(index_reg)
                };
                expr.scale = scale;
            }
            expr.disp = disp;
            let mut value = GenericValuePart::Expr(expr);
            let result = values::gval_expr_as_reg(machine, &mut value)?;
            raw::mov(machine, Reg::new(0), result, 8)?;
            drop(value);
            drop(index);
            drop(base);
        }
        Scenario::Alloca {
            dynamic,
            size,
            count,
            align,
            reuse,
        } => {
            assert!(machine.frame_has_dynamic_alloca());
            let mut old = ScratchReg::new(machine);
            old.alloc_specific(Reg::new(19))?;
            raw::emit(machine, de64_MOV_SPx(raw::sp(Reg::new(19))?, DA_SP))?;
            raw::emit(
                machine,
                de64_ANDxi(raw::sp(Reg::new(19))?, raw::zr(Reg::new(19))?, !63u64),
            )?;
            raw::emit(machine, de64_SUBxi(DA_SP, raw::sp(Reg::new(19))?, 48))?;
            raw::emit(machine, de64_MOV_SPx(raw::sp(Reg::new(19))?, DA_SP))?;
            let owner = machine.result_ref(result_value)?;
            if dynamic {
                let mut operand = ValuePartRef::constant_inline(machine, count, 8, GP_BANK);
                operand.load_to_specific(Reg::new(23))?;
                if reuse {
                    values::alloca_dynamic(machine, size, operand, align, owner.part(0))?;
                } else {
                    values::alloca_dynamic(
                        machine,
                        size,
                        operand.get_unowned(),
                        align,
                        owner.part(0),
                    )?;
                    drop(operand);
                }
            } else {
                values::alloca_fixed(machine, size, align, owner.part(0))?;
            }
            let result_reg = owner.part(0).cur_reg_unlocked();
            raw::emit(
                machine,
                de64_SUBx(
                    raw::zr(Reg::new(1))?,
                    raw::zr(Reg::new(19))?,
                    raw::zr(result_reg)?,
                ),
            )?;
            raw::emit(machine, de64_MOV_SPx(raw::sp(Reg::new(0))?, DA_SP))?;
            raw::emit(
                machine,
                de64_SUBx(
                    raw::zr(Reg::new(0))?,
                    raw::zr(Reg::new(19))?,
                    raw::zr(Reg::new(0))?,
                ),
            )?;
            raw::emit(
                machine,
                de64_ORRx_lsl(
                    raw::zr(Reg::new(0))?,
                    raw::zr(Reg::new(0))?,
                    raw::zr(Reg::new(1))?,
                    32,
                ),
            )?;
            raw::mov(machine, result_reg, Reg::new(0), 8)?;
        }
        Scenario::Spill {
            size,
            vector,
            sign,
            high,
        } => {
            machine.set_frame_used(true);
            machine.set_frame_size(512);
            let register = Reg::new(if vector { 34 } else { 2 });
            raw::materialize_constant(
                machine,
                &WIDE,
                if vector { FP_BANK } else { GP_BANK },
                size,
                register,
            )?;
            values::spill_reg(machine, register, 192, size)?;
            raw::materialize_constant(
                machine,
                &[0, 0],
                if vector { FP_BANK } else { GP_BANK },
                size,
                register,
            )?;
            values::load_from_stack(machine, register, 192, size, sign)?;
            if high {
                raw::store_off(machine, Reg::new(29), 208, register, 16)?;
                raw::load_off(machine, Reg::new(0), Reg::new(29), 216, 8, false)?;
            } else {
                raw::mov(machine, Reg::new(0), register, 8)?;
            }
        }
        Scenario::Call { indirect } => {
            emit_local_call(machine, indirect)?;
        }
        Scenario::WideCall { .. } => {
            unreachable!("wide call instructions have value-aware dispatch")
        }
        Scenario::Tls { model } => {
            let symbol = machine.function_symbol(1);
            let result = calls::tls_get_addr(machine, symbol, model)?;
            assert_eq!(result.cur_reg(), Reg::new(0));
            drop(result);
        }
        Scenario::StackCall { byval } => {
            static BYVAL: [u8; 32] = [
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
                24, 25, 26, 27, 28, 29, 30, 31, 32,
            ];
            let mut assigner = CCAssigner::new(&AAPCS_INFO, CCAssignerAAPCS::default());
            let mut hooks = calls::CallBuilder::new();
            let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
            if let Some(size) = byval {
                let mut assignment = CCAssignment::new();
                assignment.bank = GP_BANK;
                assignment.align = 8;
                assignment.size = size;
                assignment.set_byval(true);
                let pointer = BYVAL.as_ptr().addr() as u64;
                builder.add_arg_part(
                    ValuePartRef::constant_inline(machine, pointer, 8, GP_BANK),
                    assignment,
                )?;
            } else {
                for index in 0..10 {
                    builder.add_arg_part(
                        ValuePartRef::constant_inline(machine, 0x41 + index, 8, GP_BANK),
                        CCAssignment::new(),
                    )?;
                }
            }
            builder.call(CallTarget::Symbol(machine.function_symbol(1)))?;
        }
        Scenario::Variadic { gp, fp, .. } => {
            machine.with_target(|target| {
                let state = target.a64_state();
                assert_eq!(state.scalar_arg_count, gp.min(8));
                assert_eq!(state.vec_arg_count, fp.min(8));
                assert_eq!(state.reg_save_frame_off, 160);
                assert_ne!(state.func_arg_stack_add_off, u32::MAX);
                assert_eq!(state.func_arg_stack_add_reg, Reg::new(17));
            });
            assert!(machine.frame_used());
            raw::materialize_constant(machine, &[42], GP_BANK, 8, Reg::new(0))?;
        }
    }
    Ok(())
}

fn run(scenario: Scenario, native: Option<u64>) -> Vec<u8> {
    let context = MemoryContext::new_bump("a64-platform");
    let mcx = context.mcx();
    let mut ir = TestIR::new(mcx);
    let source = if let Scenario::Variadic {
        gp,
        fp,
        byval,
        sret,
    } = scenario
    {
        let mut args = Vec::new();
        if byval {
            args.extend(["%byval_large".to_owned(), "%byval_tail".to_owned()]);
        }
        if sret {
            args.push("%sret_result".to_owned());
        }
        args.extend((0..gp).map(|index| format!("%g{index}")));
        args.extend((0..fp).map(|index| format!("%v_{index}")));
        format!("variadic({}) {{\nentry:\nterminate\n}}\n", args.join(","))
    } else if matches!(scenario, Scenario::WideCall { .. }) {
        "main() {\nentry:\n%v_wide = any\n%answer = any %v_wide\nret %answer\n}\ncallee() {\nentry:\nterminate\n}\n".to_owned()
    } else if scenario.dynamic() {
        "main() {\nentry:\n%slot = alloca 0,32\n%answer = any %slot\nret %answer\n}\n".to_owned()
    } else if scenario.calls() {
        "main() {\nentry:\nterminate\n}\ncallee() {\nentry:\nterminate\n}\n".to_owned()
    } else {
        "main() {\nentry:\nterminate\n}\n".to_owned()
    };
    ir.parse_ir(&source).unwrap();
    with_compiler_base(
        mcx,
        Adaptor::new(&ir),
        CompilerA64::new(mcx, Frontend { scenario }, 0),
        A64Target::new(mcx),
        Architecture::Aarch64,
        |mut compiler| {
            assert!(compiler.compile().unwrap(), "{scenario:?}");
            let assembler = compiler.assembler_mut().unwrap();
            let section = assembler
                .get_default_section(tpde_assembler::SectionKind::Text)
                .unwrap();
            let text = assembler.get_section(section).unwrap().data.to_vec();
            if let Some(expected) = native {
                #[cfg(all(target_arch = "aarch64", target_os = "linux", not(miri)))]
                {
                    if std::env::var_os("TPDE_A64_PLATFORM_CHILD").is_some() {
                        let mut mapper = ElfMapper::new(mcx).unwrap();
                        // SAFETY: generated frame metadata and fixture storage stay alive through mapping and execution.
                        assert!(unsafe {
                            mapper.map(assembler, |_| panic!("unexpected external symbol"))
                        }
                        .unwrap());
                        let main = (0..assembler.global_symbols().len())
                            .map(|index| SymRef::new(index as u32 | 0x8000_0000))
                            .find(|&symbol| assembler.sym_name(symbol).unwrap() == b"main")
                            .expect("main symbol");
                        let symbol = mapper.get_sym_addr(main).expect("main symbol");
                        native::record_pending(scenario);
                        // SAFETY: main has AAPCS signature (u64,u64)->u64 and touches only its stack and fixture bytes.
                        let actual = unsafe { native::invoke_preserving_host(symbol.address()) };
                        assert_eq!(actual, expected, "{scenario:?}");
                        native::record_pass(scenario, actual);
                    }
                }
                #[cfg(not(all(target_arch = "aarch64", target_os = "linux", not(miri))))]
                let _ = expected;
            }
            text
        },
    )
    .unwrap()
}

#[test]
fn prologue_frame_sizes_saved_banks_and_multiple_returns() {
    let masks = [
        0,
        1 << 19,
        (1 << 19) | (1 << 20),
        (1 << 19) | (1 << 40),
        (1 << 19) | (1 << 20) | (1 << 28) | (1 << 40) | (1 << 47),
        (0x3ff << 19) | (0xff << 40),
    ];
    for size in [160, 496, 512, 4080, 4096, 4097] {
        for saved in masks {
            for returns in [false, true] {
                run(
                    Scenario::Frame {
                        size,
                        saved,
                        returns,
                    },
                    Some(42),
                );
            }
        }
    }
    run(
        Scenario::Frame {
            size: 0,
            saved: 0,
            returns: false,
        },
        Some(42),
    );
}

#[test]
fn expressions_preserve_ownership_and_all_operand_shapes() {
    for shape in 0..4 {
        for owned in 0..4 {
            for scale in [2i64, 8, 3, -3] {
                for disp in [0i64, 4095, -4095, 0x1234_5678, -0x1234_5678] {
                    let expected = (if shape & 1 != 0 { 17u64 } else { 0 })
                        .wrapping_add(if shape & 2 != 0 {
                            11u64.wrapping_mul(scale as u64)
                        } else {
                            0
                        })
                        .wrapping_add(disp as u64);
                    run(
                        Scenario::Expression {
                            shape,
                            owned,
                            alias: false,
                            scale,
                            disp,
                        },
                        Some(expected),
                    );
                }
            }
        }
    }
    for owned in [0, 1, 2] {
        for scale in [2i64, 8, 3, -3] {
            for disp in [0i64, 4095, -4095, 0x1234_5678, -0x1234_5678] {
                let expected = 17u64
                    .wrapping_add(17u64.wrapping_mul(scale as u64))
                    .wrapping_add(disp as u64);
                run(
                    Scenario::Expression {
                        shape: 3,
                        owned,
                        alias: true,
                        scale,
                        disp,
                    },
                    Some(expected),
                );
            }
        }
    }
}

#[test]
fn dynamic_and_fixed_allocas_preserve_zero_and_stronger_alignment() {
    for dynamic in [false, true] {
        for size in [0u64, 1, 16, 32, 64, 4095, 4096, 4097, 0x10_0000] {
            for align in [16u32, 32, 64] {
                for reuse in [false, true] {
                    if !dynamic && reuse {
                        continue;
                    }
                    let count = if size < 0x10_0000 { 3 } else { 1 };
                    let bytes = if dynamic {
                        size * count
                    } else {
                        (size + 15) & !15
                    };
                    let base = 0x10_0010u64;
                    let aligned = base.wrapping_sub(bytes) & !(u64::from(align) - 1);
                    let result_delta = base.wrapping_sub(aligned);
                    let sp_delta = if size == 0 { 0 } else { result_delta };
                    run(
                        Scenario::Alloca {
                            dynamic,
                            size,
                            count,
                            align,
                            reuse,
                        },
                        Some(sp_delta | result_delta << 32),
                    );
                }
            }
        }
    }
}

#[test]
fn spill_reload_widths_and_sign_extension() {
    for vector in [false, true] {
        for size in [1u32, 2, 4, 8, 16] {
            if !vector && size == 16 {
                continue;
            }
            for sign in [false, true] {
                if vector && sign {
                    continue;
                }
                let low = WIDE[0];
                let shift = 64 - size.min(8) * 8;
                let expected = if sign {
                    let extended = ((low << shift) as i64 >> shift) as u64;
                    if size < 4 {
                        u64::from(extended as u32)
                    } else {
                        extended
                    }
                } else {
                    (low << shift) >> shift
                };
                run(
                    Scenario::Spill {
                        size,
                        vector,
                        sign,
                        high: false,
                    },
                    Some(expected),
                );
            }
        }
    }
    run(
        Scenario::Spill {
            size: 16,
            vector: true,
            sign: false,
            high: true,
        },
        Some(WIDE[1]),
    );
}

#[test]
fn wide_vectors_survive_direct_and_indirect_calls_in_each_preserved_register() {
    for register in 40..48 {
        for high in [false, true] {
            for indirect in [false, true] {
                run(
                    Scenario::WideCall {
                        register,
                        high,
                        indirect,
                    },
                    Some(WIDE[usize::from(high)]),
                );
            }
        }
    }
}

#[test]
fn native_platform_scenarios_execute_in_bounded_child() {
    #[cfg(all(target_arch = "aarch64", target_os = "linux", not(miri)))]
    native::drive();
}

#[test]
fn direct_and_indirect_calls_execute_local_compiled_functions() {
    for indirect in [false, true] {
        run(Scenario::Call { indirect }, Some(4242));
    }
}

#[test]
fn stack_arguments_and_byval_copy_tails_execute() {
    run(Scenario::StackCall { byval: None }, Some(0x49));
    for size in [0u32, 1, 2, 3, 4, 7, 8, 9, 15, 16, 17, 23, 24, 31] {
        let expected = u64::from(size) * u64::from(size + 1) / 2;
        run(Scenario::StackCall { byval: Some(size) }, Some(expected));
    }
}

#[test]
fn variadic_prologues_save_all_argument_banks_and_patch_incoming_stack() {
    for (gp, fp) in [(0, 0), (1, 2), (7, 7), (8, 8), (10, 10)] {
        for byval in [false, true] {
            for sret in [false, true] {
                let text = run(
                    Scenario::Variadic {
                        gp,
                        fp,
                        byval,
                        sret,
                    },
                    None,
                );
                let words: Vec<_> = text
                    .chunks_exact(4)
                    .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
                    .collect();
                for first in [0u8, 2, 4, 6] {
                    assert!(words.contains(&de64_STPx(
                        GRegZr::new(first).unwrap(),
                        GRegZr::new(first + 1).unwrap(),
                        DA_SP,
                        i64::from(160 + u32::from(first) * 8)
                    )));
                    assert!(words.contains(&de64_STPq(
                        VReg::new(first).unwrap(),
                        VReg::new(first + 1).unwrap(),
                        DA_SP,
                        i64::from(224 + u32::from(first) * 16)
                    )));
                }
                assert!(words.contains(&de64_STRxu(
                    GRegZr::new(17).unwrap(),
                    GRegSp::new(29).unwrap(),
                    352
                )));
            }
        }
    }
}

#[test]
fn tls_models_share_the_upstream_descriptor_sequence() {
    let mut expected = None;
    for model in [
        calls::TLSModel::GlobalDynamic,
        calls::TLSModel::LocalDynamic,
        calls::TLSModel::InitialExec,
        calls::TLSModel::LocalExec,
    ] {
        let text = run(Scenario::Tls { model }, None);
        let words: Vec<_> = text
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
            .collect();
        assert!(words.contains(&de64_MRS(GRegZr::new(1).unwrap(), 0xde82)));
        if let Some(expected) = &expected {
            assert_eq!(&text, expected);
        } else {
            expected = Some(text);
        }
    }
}

#[test]
fn failed_frame_finalization_can_reset_and_compile_again() {
    let context = MemoryContext::new_bump("a64-platform-reuse");
    let mcx = context.mcx();
    let mut ir = TestIR::new(mcx);
    ir.parse_ir("main() {\nentry:\nterminate\n}\n").unwrap();
    let failure = Scenario::Frame {
        size: u32::MAX,
        saved: 0,
        returns: false,
    };
    with_compiler_base(
        mcx,
        Adaptor::new(&ir),
        CompilerA64::new(mcx, Frontend { scenario: failure }, 0),
        A64Target::new(mcx),
        Architecture::Aarch64,
        |mut compiler| {
            assert!(compiler.compile().is_err());
            assert!(compiler.assembler().is_err());
            compiler.reset().unwrap();
            assert_eq!(compiler.reset_state_counts(), (0, 0, 0));
            compiler.target_mut().frontend.scenario = Scenario::Frame {
                size: 160,
                saved: 0,
                returns: false,
            };
            assert!(compiler.compile().unwrap());
            let first = compiler.assembler().unwrap().build_object_file().unwrap();
            compiler.reset().unwrap();
            assert!(compiler.compile().unwrap());
            let second = compiler.assembler().unwrap().build_object_file().unwrap();
            assert_eq!(first, second);
        },
    )
    .unwrap();
}

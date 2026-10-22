// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

//! Shared raw A64 compiler executor: the Rust side of the pinned raw-operation
//! protocol, used by the differential harness and the paired benchmark.
use crate::raw_cases::{Case, Operation};
use std::cell::RefCell;
use tpde_assembler::{Architecture, SymRef};
use tpde_compiler::{
    calls::RetBuilder,
    target::{CompilerOutput, CompilerTarget, TargetMachine},
    with_compiler_base, ValuePartRef,
};
use tpde_compiler_a64::{
    self as a64, raw, A64CompilerState, A64Frontend, CompilerA64, Jump, JumpKind,
};
use tpde_core::{
    calling_convention::{CCAssignment, CCAssignmentPolicy},
    compiler::{ValRefSpecial, ValueParts},
    ir_adaptor::IrAdaptor,
    Reg, RegBank,
};
use tpde_function_writer::Label;
use tpde_function_writer_a64::A64Target;
use tpde_test_ir::{IRFuncRef, IRInstRef, IRValueRef, TestIR, TestIRAdaptor};
use types_error::PgResult;

pub fn put(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}
pub struct Parts;
impl ValueParts for Parts {
    fn count(&self) -> u32 {
        1
    }
    fn size_bytes(&self, _: u32) -> u32 {
        8
    }
    fn reg_bank(&self, _: u32) -> RegBank {
        a64::GP_BANK
    }
}
pub struct Frontend<'s> {
    pub script: &'s [Operation],
    pub output: &'s RefCell<Vec<u8>>,
}
impl<'a, 'ir, 's> A64Frontend<TestIRAdaptor<'a, 'ir>> for Frontend<'s> {
    type Special = ValRefSpecial<()>;
    type Parts = Parts;
    fn val_parts(&self, _: &TestIRAdaptor<'a, 'ir>, _: IRValueRef) -> Parts {
        Parts
    }
    fn val_ref_special(&self, _: &TestIRAdaptor<'a, 'ir>, _: IRValueRef) -> Option<Self::Special> {
        None
    }
    fn cur_func_may_emit_calls(&self, _: &TestIRAdaptor<'a, 'ir>) -> bool {
        false
    }
    fn cur_personality_func(&self, _: &TestIRAdaptor<'a, 'ir>) -> SymRef {
        SymRef::default()
    }
    fn define_func_idx(&mut self, _: &TestIRAdaptor<'a, 'ir>, function: IRFuncRef, index: u32) {
        assert_eq!(function.0, index);
    }
    fn special_part<'v, 'm, M: TargetMachine<'m, TestIRAdaptor<'a, 'ir>, 2>>(
        _: &'v M,
        _: &'v Self::Special,
        _: u32,
    ) -> ValuePartRef<'v, M, 2>
    where
        TestIRAdaptor<'a, 'ir>: 'v,
        M::Target:
            CompilerTarget<TestIRAdaptor<'a, 'ir>, 2, Special = Self::Special> + A64CompilerState,
        TestIRAdaptor<'a, 'ir>: 'v,
    {
        panic!("raw fixture has no special values")
    }
    fn compile_inst<'m, M: TargetMachine<'m, TestIRAdaptor<'a, 'ir>, 2>>(
        machine: &M,
        _: IRInstRef,
        _: impl Iterator<Item = IRInstRef> + Clone,
    ) -> PgResult<bool>
    where
        M::Target: A64CompilerState<Frontend = Self>
            + CompilerTarget<TestIRAdaptor<'a, 'ir>, 2, Jump = Jump>,
    {
        let (script, output) =
            machine.with_target(|t| (t.a64_frontend().script, t.a64_frontend().output));
        execute(machine, script, output)?;
        machine.with_cur_cc_assigner(|assigner| RetBuilder::new(machine, assigner).ret())?;
        Ok(true)
    }
}

pub fn kind(number: u64) -> JumpKind {
    use JumpKind::*;
    [
        Eq, Ne, Cs, Cc, Mi, Pl, Vs, Vc, Hi, Ls, Ge, Lt, Gt, Le, Always, Cbz, Cbnz, Tbz, Tbnz,
    ][number as usize]
}
/// `inline(never)` keeps one host symbol for the tpde-asm-diff capture in bench.rs.
#[inline(never)]
pub fn execute<'m, 'a, 'ir: 'a, M: TargetMachine<'m, TestIRAdaptor<'a, 'ir>, 2>>(
    machine: &M,
    script: &[Operation],
    output: &RefCell<Vec<u8>>,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    let mut labels: Vec<Label> = Vec::new();
    for operation in script {
        let a = operation.args;
        match operation.code {
            1 => raw::load_off(
                machine,
                Reg::new(a[0]),
                Reg::new(a[1]),
                a[2] as u32,
                a[3] as u32,
                a[4] != 0,
            )?,
            2 => raw::store_off(
                machine,
                Reg::new(a[0]),
                a[1] as u32,
                Reg::new(a[2]),
                a[3] as u32,
            )?,
            3 => raw::mov(machine, Reg::new(a[0]), Reg::new(a[1]), a[2] as u32)?,
            4 => raw::materialize_constant(
                machine,
                &a[3..5],
                RegBank::new(a[0] as u8),
                a[1] as u32,
                Reg::new(a[2]),
            )?,
            5 => raw::generate_raw_set(machine, Jump::new(kind(a[0])), Reg::new(a[1]))?,
            6 => raw::generate_raw_mask(machine, Jump::new(kind(a[0])), Reg::new(a[1]))?,
            7 => raw::generate_raw_select(
                machine,
                Jump::new(kind(a[0])),
                Reg::new(a[1]),
                Reg::new(a[2]),
                Reg::new(a[3]),
                a[4] != 0,
            )?,
            8 => raw::generate_raw_intext(
                machine,
                Reg::new(a[0]),
                Reg::new(a[1]),
                a[2] != 0,
                a[3] as u32,
                a[4] as u32,
            )?,
            9 => raw::generate_raw_bfi(
                machine,
                Reg::new(a[0]),
                Reg::new(a[1]),
                a[2] as u32,
                a[3] as u32,
            )?,
            10 => raw::generate_raw_bfiz(
                machine,
                Reg::new(a[0]),
                Reg::new(a[1]),
                a[2] as u32,
                a[3] as u32,
            )?,
            11 => {
                let kind = kind(a[0]);
                let jump = if a[0] < 15 {
                    Jump::new(kind)
                } else if a[0] < 17 {
                    Jump::compare(kind, Reg::new(a[1]), a[2] != 0)
                } else {
                    Jump::test(kind, Reg::new(a[1]), a[3] as u8)
                };
                raw::generate_raw_jump(machine, jump, labels[a[4] as usize])?;
            }
            12 => labels.push(machine.with_output(|writer| writer.label_create())?),
            13 => machine.with_output(|writer| writer.label_place(labels[a[0] as usize]))?,
            14 => {
                let end = machine.with_output(|writer| writer.offset()) + a[0] as usize;
                while machine.with_output(|writer| writer.offset()) < end {
                    raw::emit(machine, 0xd503201f)?;
                }
            }
            15 => raw::switch_emit_cmpeq(
                machine,
                labels[a[0] as usize],
                Reg::new(a[1]),
                Reg::new(a[2]),
                a[3],
                a[4] != 0,
            )?,
            16 => raw::switch_emit_binary_step(
                machine,
                labels[a[0] as usize],
                labels[a[1] as usize],
                Reg::new(a[2]),
                Reg::new(a[3]),
                a[4],
                a[5] != 0,
            )?,
            17 => {
                raw::switch_create_jump_table(
                    machine,
                    labels[a[0] as usize],
                    Reg::new(a[1]),
                    Reg::new(a[2]),
                    a[3],
                    a[4],
                    a[5] != 0,
                    |table| {
                        for (index, entry) in table.iter_mut().enumerate() {
                            *entry = labels[a[6 + index % 2] as usize];
                        }
                    },
                )?;
            }
            18 => {
                machine.with_target(|t| t.a64_state_mut().cpu_feats = a[0] as u32);
                put(
                    &mut output.borrow_mut(),
                    u64::from(raw::has_cpu_feats(machine, a[1] as u32)),
                );
            }
            19 => {
                let jump = Jump::new(kind(a[0]));
                let result = if a[1] != 0 {
                    raw::swap_jump(jump)?
                } else {
                    raw::invert_jump(jump)
                };
                put(&mut output.borrow_mut(), result.kind as u64);
            }
            20 => {
                let jump = if a[0] < 17 {
                    Jump::compare(kind(a[0]), Reg::new(a[1]), a[2] != 0)
                } else {
                    Jump::test(kind(a[0]), Reg::new(a[1]), a[2] as u8)
                };
                let inverted = raw::invert_jump(jump);
                for value in [
                    inverted.kind as u64,
                    inverted.cmp_reg.id() as u64,
                    inverted.cmp_is_32 as u64,
                    inverted.test_bit as u64,
                ] {
                    put(&mut output.borrow_mut(), value);
                }
            }
            _ => panic!("unknown raw operation {}", operation.code),
        }
        put(
            &mut output.borrow_mut(),
            machine.with_output(|writer| writer.offset()) as u64,
        );
        put(
            &mut output.borrow_mut(),
            machine.with_registers(|registers| registers.clobbered.get()),
        );
    }
    Ok(())
}
pub fn run_raw(case: &Case) -> Vec<u8> {
    let context = mcx::MemoryContext::new_bump("A64 raw oracle");
    let mut ir = TestIR::new(context.mcx());
    ir.parse_ir("raw() {\nentry:\nterminate\n}\n").unwrap();
    let output = RefCell::new(Vec::new());
    with_compiler_base(
        context.mcx(),
        TestIRAdaptor::new(&ir),
        CompilerA64::new(
            context.mcx(),
            Frontend {
                script: &case.operations,
                output: &output,
            },
            0,
        ),
        A64Target::new(context.mcx()),
        Architecture::Aarch64,
        |mut compiler| {
            let mut previous = None;
            for iteration in 0..2 {
                output.borrow_mut().clear();
                compiler.adaptor.start_compile();
                assert!(compiler.compile().unwrap(), "{}", case.name);
                compiler.adaptor.end_compile();
                let object = compiler.assembler().unwrap().build_object_file().unwrap();
                put(&mut output.borrow_mut(), object.len() as u64);
                output.borrow_mut().extend_from_slice(&object);
                if let Some(previous) = &previous {
                    assert_eq!(&*output.borrow(), previous, "{} reuse", case.name);
                }
                if iteration == 0 {
                    previous = Some(output.borrow().clone());
                }
                compiler.reset().unwrap();
                assert_eq!(compiler.reset_state_counts(), (0, 0, 0));
            }
        },
    )
    .unwrap();
    output.into_inner()
}
pub fn run_abi(case: &Case) -> Vec<u8> {
    let mut output = Vec::new();
    let mut assigner = a64::CCAssignerAAPCS::default();
    for operation in &case.operations {
        let a = operation.args;
        match operation.code {
            0 => {
                assigner.reset();
                put(&mut output, assigner.get_stack_size() as u64);
            }
            3 => {
                let info = a64::AAPCS_INFO;
                for value in [
                    info.allocatable_regs(),
                    info.callee_saved_regs(),
                    info.arg_regs(),
                    info.red_zone_size() as u64,
                ] {
                    put(&mut output, value);
                }
            }
            1 | 2 => {
                let mut assignment = CCAssignment::new();
                assignment.bank = RegBank::new(a[0] as u8);
                assignment.size = a[1] as u32;
                assignment.align = a[2] as u8;
                assignment.consecutive = a[3] as u8;
                assignment.set_sret(a[4] & 1 != 0);
                assignment.set_byval(a[4] & 2 != 0);
                assignment.int_ext = a[5] as u8;
                assignment.reg = Reg::new(a[6]);
                assignment.stack_off = a[7] as u32;
                if operation.code == 1 {
                    assigner.assign_arg(&mut assignment);
                } else {
                    assigner.assign_ret(&mut assignment);
                }
                for value in [
                    assignment.reg.id() as u64,
                    assignment.stack_off as u64,
                    assigner.get_stack_size() as u64,
                ] {
                    put(&mut output, value);
                }
            }
            _ => panic!("unknown ABI operation"),
        }
    }
    output
}

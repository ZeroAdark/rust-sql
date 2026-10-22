// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#[path = "adaptor.rs"]
mod adaptor;

use adaptor::Adaptor;
use mcx::MemoryContext;
use std::{
    borrow::ToOwned, cell::RefCell, fmt::Write, format, rc::Rc, string::String, vec, vec::Vec,
};
use tpde_assembler::{Architecture, SymBinding, SymRef};
use tpde_base::ValLocalIdx;
use tpde_compiler::{
    branches,
    calls::{CallBuilderBase, CallMachine, CallTarget},
    compiler_base::with_compiler_base,
    generic_value_part::{Expr, ExprReg, GenericValuePart},
    guard_machine::GuardMachine,
    target::{CompilerOutput, CompilerTarget, TargetMachine},
    ScratchReg, ValuePartRef, ValueRef,
};
use tpde_compiler_x64::{
    emission as emit, CCAssignerSysV, CompilerX64, Jump, X64CompilerState, X64Frontend, CC_INFO,
};
use tpde_core::{
    calling_convention::{CCAssigner, CCAssignment, CallingConventionAssigner},
    compiler::{ValRefSpecial, ValueParts},
    ir_adaptor::IrAdaptor,
    Reg, RegBank,
};
use tpde_function_writer::{FunctionWriterBase, Label};
use tpde_function_writer_x64::X64Target;
use tpde_test_ir::{IRFuncRef, IRInstRef, IRValueRef, Op, TestIR};
use types_error::PgResult;

#[derive(Clone)]
struct Command {
    name: String,
    args: Vec<u64>,
}

fn commands(input: &str) -> Vec<Command> {
    input
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut parts = line.split_whitespace();
            Command {
                name: parts.next().unwrap().to_owned(),
                args: parts.map(|part| part.parse().unwrap()).collect(),
            }
        })
        .collect()
}

fn record(log: &RefCell<String>, tag: &str, fields: &[u64]) {
    let mut log = log.borrow_mut();
    log.push_str(tag);
    for field in fields {
        write!(log, " {field}").unwrap();
    }
    log.push('\n');
}

fn blob(log: &RefCell<String>, tag: &str, bytes: &[u8]) {
    let mut log = log.borrow_mut();
    write!(log, "{tag} ").unwrap();
    for byte in bytes {
        write!(log, "{byte:02x}").unwrap();
    }
    log.push('\n');
}

fn assignment(a: &[u64]) -> CCAssignment {
    let mut out = CCAssignment::new();
    out.bank = RegBank::new(a[0] as u8);
    out.size = a[1] as u32;
    out.align = a[2] as u8;
    out.consecutive = a[3] as u8;
    out.set_byval(a[4] != 0);
    out.set_sret(a[5] != 0);
    out
}

fn cc(commands: &[Command]) -> String {
    let log = RefCell::new(String::new());
    let mut assigner = CCAssigner::new(&CC_INFO, CCAssignerSysV::new(commands[0].args[0] != 0));
    for cmd in &commands[1..] {
        match cmd.name.as_str() {
            "ccarg" | "ccret" => {
                let mut a = assignment(&cmd.args);
                if cmd.name == "ccarg" {
                    assigner.assign_arg(&mut a);
                } else {
                    assigner.assign_ret(&mut a);
                }
                record(
                    &log,
                    &cmd.name,
                    &[
                        a.reg.id().into(),
                        a.stack_off.into(),
                        a.bank.id().into(),
                        a.size.into(),
                        a.align.into(),
                        a.consecutive.into(),
                        a.byval().into(),
                        a.sret().into(),
                        assigner.get_stack_size().into(),
                        assigner.is_vararg().into(),
                    ],
                );
            }
            "ccreset" => {
                assigner.reset();
                record(
                    &log,
                    "ccreset",
                    &[
                        assigner.get_stack_size().into(),
                        assigner.is_vararg().into(),
                    ],
                );
            }
            "ccinfo" => record(
                &log,
                "ccinfo",
                &[
                    CC_INFO.allocatable_regs(),
                    CC_INFO.callee_saved_regs(),
                    CC_INFO.arg_regs(),
                    CC_INFO.red_zone_size().into(),
                ],
            ),
            _ => panic!("unknown CC command {}", cmd.name),
        }
    }
    log.into_inner()
}

pub struct Parts {
    bank: RegBank,
    size: u32,
    count: u32,
}
impl ValueParts for Parts {
    fn count(&self) -> u32 {
        self.count
    }
    fn size_bytes(&self, _: u32) -> u32 {
        self.size
    }
    fn reg_bank(&self, _: u32) -> RegBank {
        self.bank
    }
}

struct Frontend {
    script: Vec<Command>,
    log: Rc<RefCell<String>>,
    bank: RegBank,
    size: u32,
    count: u32,
    force_fixed: bool,
}

impl<'a, 'ir> X64Frontend<Adaptor<'a, 'ir>> for Frontend {
    type Special = ValRefSpecial<()>;
    type Parts = Parts;
    fn val_parts(&self, _: &Adaptor<'a, 'ir>, _: IRValueRef) -> Parts {
        Parts {
            bank: self.bank,
            size: self.size,
            count: self.count,
        }
    }
    fn val_ref_special(&self, _: &Adaptor<'a, 'ir>, _: IRValueRef) -> Option<Self::Special> {
        None
    }
    fn cur_func_may_emit_calls(&self, a: &Adaptor<'a, 'ir>) -> bool {
        !a.leaf
    }
    fn cur_personality_func(&self, _: &Adaptor<'a, 'ir>) -> SymRef {
        SymRef::default()
    }
    fn define_func_idx(&mut self, _: &Adaptor<'a, 'ir>, _: IRFuncRef, _: u32) {}
    fn allow_fixed_assignment(&self, _: &Adaptor<'a, 'ir>, _: IRValueRef) -> bool {
        self.force_fixed
    }
    fn try_force_fixed_assignment(&self, _: &Adaptor<'a, 'ir>, _: IRValueRef) -> bool {
        self.force_fixed
    }
    fn special_part<'s, 'm, M>(_: &'s M, _: &'s Self::Special, _: u32) -> ValuePartRef<'s, M, 2>
    where
        Adaptor<'a, 'ir>: 's,
        M: TargetMachine<'m, Adaptor<'a, 'ir>, 2>,
        M::Target: X64CompilerState<Frontend = Self>
            + CompilerTarget<Adaptor<'a, 'ir>, 2, Special = Self::Special, Jump = Jump>,
    {
        panic!("fixture has no special values")
    }
    fn compile_inst<'m, M>(
        machine: &M,
        inst: IRInstRef,
        _: impl Iterator<Item = IRInstRef> + Clone,
    ) -> PgResult<bool>
    where
        M: TargetMachine<'m, Adaptor<'a, 'ir>, 2>,
        M::Target: X64CompilerState<Frontend = Self>
            + CompilerTarget<Adaptor<'a, 'ir>, 2, Special = Self::Special, Jump = Jump>,
    {
        if machine.call_adaptor().ir().values[inst.0 as usize].op != Op::Terminate {
            return Ok(true);
        }
        if machine.with_target(|target| {
            target
                .frontend()
                .script
                .iter()
                .any(|cmd| cmd.name == "cancel")
        }) {
            return Ok(false);
        }
        drive(machine)?;
        if !machine.with_target(|target| {
            target
                .frontend()
                .script
                .iter()
                .any(|cmd| cmd.name == "epilog")
        }) {
            machine.gen_func_epilog()?;
        }
        branches::release_regs_after_return(machine);
        Ok(true)
    }
}

fn jump(n: u64) -> Jump {
    [
        Jump::jo,
        Jump::jno,
        Jump::jb,
        Jump::jae,
        Jump::je,
        Jump::jne,
        Jump::jbe,
        Jump::ja,
        Jump::js,
        Jump::jns,
        Jump::jp,
        Jump::jnp,
        Jump::jl,
        Jump::jge,
        Jump::jle,
        Jump::jg,
        Jump::jmp,
    ][n as usize]
}

fn expr_reg<'s, M: GuardMachine<2>>(
    machine: &'s M,
    kind: u64,
    reg: u64,
) -> PgResult<ExprReg<'s, M, 2>> {
    Ok(match kind {
        0 => ExprReg::Borrowed(Reg::make_invalid()),
        1 => ExprReg::Borrowed(Reg::new(reg)),
        2 => {
            let mut value = ScratchReg::new(machine);
            value.alloc_specific(Reg::new(reg))?;
            ExprReg::Scratch(value)
        }
        _ => panic!("invalid expression owner"),
    })
}

fn drive<'m, 'a, 'ir: 'a, M>(machine: &M) -> PgResult<()>
where
    M: TargetMachine<'m, Adaptor<'a, 'ir>, 2>,
    M::Target: X64CompilerState<Frontend = Frontend>
        + CompilerTarget<Adaptor<'a, 'ir>, 2, Special = ValRefSpecial<()>, Jump = Jump>,
{
    let (commands, log) = machine.with_target(|target| {
        (
            target.frontend().script.clone(),
            target.frontend().log.clone(),
        )
    });
    let mut labels = Vec::<Label>::new();
    let mut held: Vec<Option<ScratchReg<'_, M, 2>>> = (0..64).map(|_| None).collect();
    let mut skip_until = 0;
    let mut tls_symbol = SymRef::default();
    for (index, cmd) in commands.iter().enumerate().skip(1) {
        if index < skip_until {
            continue;
        }
        let a = &cmd.args;
        let r = |index: usize| Reg::new(a[index]);
        match cmd.name.as_str() {
            "begin" | "finish" | "dump" | "reset" => {}
            "reject" => {
                return Err(tpde_compiler_x64::invalid(
                    "injected X64 fixture emission failure",
                ))
            }
            "flags" => machine.with_target(|target| target.x64_mut().set_preserve_flags(a[0] != 0)),
            "features" => record(
                &log,
                "features",
                &[machine
                    .with_target(|target| target.x64().has_cpu_feats(a[0] as u32))
                    .into()],
            ),
            "mov" => emit::mov(machine, r(0), r(1), a[2] as u32)?,
            "spill" => emit::spill_reg(machine, r(0), a[1] as i32, a[2] as u32)?,
            "load" => emit::load_from_stack(machine, r(0), a[1] as i32, a[2] as u32, a[3] != 0)?,
            "constant" => emit::materialize_constant(
                machine,
                &a[3..5],
                RegBank::new(a[0] as u8),
                a[1] as u32,
                r(2),
            )?,
            "intext" => {
                emit::generate_raw_intext(machine, r(0), r(1), a[2] != 0, a[3] as u32, a[4] as u32)?
            }
            "bfi" => emit::generate_raw_bfi(machine, r(0), r(1), a[2] as u32, a[3] as u32)?,
            "bfiz" => emit::generate_raw_bfiz(machine, r(0), r(1), a[2] as u32, a[3] as u32)?,
            "set" => emit::generate_raw_set(machine, jump(a[0]), r(1), a[2] != 0)?,
            "mask" => emit::generate_raw_mask(machine, jump(a[0]), r(1))?,
            "cmov" => emit::generate_raw_cmov(machine, jump(a[0]), r(1), r(2), a[3] != 0)?,
            "invert" => record(&log, "invert", &[emit::invert_jump(jump(a[0])) as u64]),
            "swap" => record(&log, "swap", &[emit::swap_jump(jump(a[0])) as u64]),
            "cond" => record(&log, "cond", &[emit::jump_to_cond(jump(a[0])) as u64]),
            "epilog" => machine.gen_func_epilog()?,
            "regs" => machine.with_registers(|registers| registers.allocatable.set(a[0])),
            "clobber" => machine.with_registers(|registers| {
                registers.clobbered.set(registers.clobbered.get() | a[0])
            }),
            "frame" => {
                machine.set_frame_size(a[0] as u32);
                machine.set_frame_used(a[1] != 0);
                if a[2] != 0 {
                    machine.call_set_generated_call()
                } else {
                    assert!(!machine.frame_generated_call());
                }
            }
            "hold" => {
                let mut scratch = ScratchReg::new(machine);
                scratch.alloc_specific(r(0))?;
                assert!(held[a[0] as usize].replace(scratch).is_none());
            }
            "release" => {
                held[a[0] as usize].take().expect("held register").reset();
            }
            "label" => {
                let label = machine.with_output(|out| out.label_create())?;
                record(&log, "label", &[labels.len() as u64]);
                labels.push(label);
            }
            "place" => machine.with_output(|out| out.label_place(labels[a[0] as usize]))?,
            "jump" => emit::generate_raw_jump(machine, jump(a[0]), labels[a[1] as usize])?,
            "cmpeq" => emit::switch_emit_cmpeq(
                machine,
                labels[a[0] as usize],
                r(1),
                r(2),
                a[3],
                a[4] != 0,
            )?,
            "binary" => emit::switch_emit_binary_step(
                machine,
                labels[a[0] as usize],
                labels[a[1] as usize],
                r(2),
                r(3),
                a[4],
                a[5] != 0,
            )?,
            "pad" => {
                machine.with_output(|out| out.write_bytes(&vec![a[1] as u8; a[0] as usize]))?
            }
            "expr" => {
                let expr = Expr {
                    base: expr_reg(machine, a[0], a[1])?,
                    index: expr_reg(machine, a[2], a[3])?,
                    scale: a[4] as i64,
                    disp: a[5] as i64,
                };
                let mut value = GenericValuePart::Expr(expr);
                let reg = emit::gval_expr_as_reg(machine, &mut value)?;
                record(&log, "expr", &[reg.id().into()]);
                value.reset();
            }
            "afixed" => {
                let mut result = ValuePartRef::temporary(machine, RegBank::new(0));
                emit::alloca_fixed(machine, a[0], a[1] as u32, &mut result)?;
                record(&log, "afixed", &[result.cur_reg().id().into()]);
                result.reset();
            }
            "adynamic" => {
                let mut count = ValuePartRef::constant_inline(machine, a[1], 8, RegBank::new(0));
                if a[3] != 255 {
                    count.load_to_specific(r(3))?;
                }
                let mut result = ValuePartRef::temporary(machine, RegBank::new(0));
                emit::alloca_dynamic(machine, a[0], count, a[2] as u32, &mut result)?;
                record(&log, "adynamic", &[result.cur_reg().id().into()]);
                result.reset();
            }
            "table" => {
                assert!(emit::switch_create_jump_table(
                    machine,
                    labels[a[0] as usize],
                    r(1),
                    r(2),
                    a[3],
                    a[4],
                    a[5] != 0,
                    |table| {
                        for (out, id) in table.iter_mut().zip(&a[6..]) {
                            *out = labels[*id as usize];
                        }
                    }
                )?);
            }
            "stackaddr" | "spillslot" => {
                prepare_assignment(machine, 1, RegBank::new(0), 8, true, false)?;
                let off = a[usize::from(cmd.name == "stackaddr")] as i32;
                machine.with_assignment(ValLocalIdx::new(0), |value| {
                    value.set_frame_off(off);
                    value.set_variable_ref(cmd.name == "stackaddr");
                    value.set_stack_variable(cmd.name == "stackaddr");
                });
                if cmd.name == "stackaddr" {
                    emit::load_address_of_stack_var(machine, r(0), ValLocalIdx::new(0), 0)?;
                } else {
                    machine.with_part(ValLocalIdx::new(0), 0, |part| part.set_stack_valid());
                    let value = tpde_compiler_x64::val_spill_slot(machine, ValLocalIdx::new(0), 0);
                    let GenericValuePart::Expr(expr) = value else {
                        panic!("spill expression expected")
                    };
                    record(
                        &log,
                        "spillslot",
                        &[
                            expr.base_reg().id().into(),
                            expr.scale as u64,
                            expr.disp as u64,
                        ],
                    );
                }
                cleanup_assignment(machine);
            }
            "fixed" => {
                prepare_assignment(machine, 1, RegBank::new(a[0] as u8), 8, true, false)?;
                machine.with_target(|target| {
                    target.x64_mut().fixed_assignment_nonallocatable_mask = a[1]
                });
                let reg = tpde_compiler_x64::frame::select_fixed_assignment_reg(
                    machine,
                    ValLocalIdx::new(0),
                    0,
                );
                record(&log, "fixed", &[reg.id().into()]);
                cleanup_assignment(machine);
            }
            "proarg" => {
                let mode = a[0];
                prepare_assignment(
                    machine,
                    a[3] as u32,
                    RegBank::new(a[1] as u8),
                    a[2] as u32,
                    mode == 0,
                    mode == 2,
                )?;
                let mut value = ValueRef::new(machine, ValLocalIdx::new(0));
                {
                    let mut part = value.part(0);
                    let mut cca = CCAssignment::new();
                    cca.bank = RegBank::new(a[1] as u8);
                    cca.size = a[2] as u32;
                    cca.reg = r(4);
                    cca.stack_off = a[5] as u32;
                    cca.set_byval(a[6] != 0);
                    let result = tpde_compiler_x64::frame::prologue_assign_arg_part(
                        machine, &mut part, cca,
                    )?;
                    machine.with_part(ValLocalIdx::new(0), 0, |assignment| {
                        record(
                            &log,
                            "proarg",
                            &[
                                result.is_some().into(),
                                result.unwrap_or(0) as i64 as u64,
                                assignment.assignment().frame_off() as i64 as u64,
                                assignment.register_valid().into(),
                                assignment.stack_valid().into(),
                                assignment.get_reg().id().into(),
                                part.has_reg().into(),
                            ],
                        )
                    });
                    part.reset();
                }
                value.disown();
                cleanup_assignment(machine);
            }
            "callbegin" => {
                skip_until = index + call_sequence(machine, &commands[index..], &log)?;
            }
            "generatecall" => {
                if a[0] == 3 {
                    prepare_assignment(machine, 1, RegBank::new(0), 8, false, false)?;
                    machine.set_frame_used(true);
                    machine.with_assignment(ValLocalIdx::new(0), |value| value.set_frame_off(-32));
                    machine.with_part(ValLocalIdx::new(0), 0, |part| part.set_stack_valid());
                    let mut owner = ValueRef::new(machine, ValLocalIdx::new(0));
                    tpde_compiler_x64::calls::generate_call(
                        machine,
                        CallTarget::Value(owner.part(0)),
                        &[],
                        None,
                        a[2] != 0,
                    )?;
                    owner.disown();
                    cleanup_assignment(machine);
                } else {
                    let target = call_target(machine, a[0], a[1])?;
                    tpde_compiler_x64::calls::generate_call(machine, target, &[], None, a[2] != 0)?;
                }
            }
            "tls" | "tlslive" => {
                if !tls_symbol.valid() {
                    tls_symbol = machine.with_output(|out| {
                        out.assembler_mut()
                            .sym_add_undef(b"tls", SymBinding::Global)
                    })?;
                }
                let model_index = if cmd.name == "tlslive" { 2 } else { 0 };
                let model = [
                    tpde_compiler_x64::TLSModel::GlobalDynamic,
                    tpde_compiler_x64::TLSModel::LocalDynamic,
                    tpde_compiler_x64::TLSModel::InitialExec,
                    tpde_compiler_x64::TLSModel::LocalExec,
                ][a[model_index] as usize];
                if cmd.name == "tlslive" {
                    let bank = tpde_core::RegisterFile::<2, 32>::reg_bank(r(0));
                    prepare_assignment(machine, 1, bank, 8, false, false)?;
                    let mut owner = ValueRef::new(machine, ValLocalIdx::new(0));
                    {
                        let mut part = owner.part(0);
                        part.alloc_specific(r(0))?;
                        emit::materialize_constant(machine, &a[1..2], bank, 8, part.cur_reg())?;
                        part.reset();
                    }
                    let mut result = emit::tls_get_addr(machine, tls_symbol, model)?;
                    let result_reg = result.cur_reg();
                    result.reset();
                    let state = machine.with_part(ValLocalIdx::new(0), 0, |part| {
                        [
                            part.register_valid().into(),
                            part.stack_valid().into(),
                            part.assignment().frame_off() as i64 as u64,
                        ]
                    });
                    {
                        let mut part = owner.part(0);
                        let reloaded = part.load_to_reg()?;
                        record(
                            &log,
                            "tlslive",
                            &[
                                state[0],
                                state[1],
                                state[2],
                                result_reg.id().into(),
                                reloaded.id().into(),
                            ],
                        );
                        part.reset();
                    }
                    owner.disown();
                    machine.release_assignment(ValLocalIdx::new(0));
                } else {
                    let result = emit::tls_get_addr(machine, tls_symbol, model)?;
                    record(&log, "tls", &[result.cur_reg().id().into()]);
                }
            }
            "adynamic_ref" => {
                prepare_assignment(machine, 1, RegBank::new(0), 8, false, false)?;
                let mut owner = ValueRef::new(machine, ValLocalIdx::new(0));
                {
                    let mut count = owner.part(0);
                    if a[3] == 255 {
                        count.alloc_reg()?;
                    } else {
                        count.alloc_specific(r(3))?;
                    }
                    emit::materialize_constant(
                        machine,
                        &a[1..2],
                        RegBank::new(0),
                        8,
                        count.cur_reg(),
                    )?;
                    let mut result = ValuePartRef::temporary(machine, RegBank::new(0));
                    emit::alloca_dynamic(machine, a[0], count, a[2] as u32, &mut result)?;
                    record(&log, "adynamic_ref", &[result.cur_reg().id().into()]);
                    result.reset();
                }
                owner.disown();
                cleanup_assignment(machine);
            }
            "proend" => {
                let mut assigner = CCAssigner::new(&CC_INFO, CCAssignerSysV::new(false));
                let mut assigned = 0u64;
                for (bank, count) in [(0, a[0]), (1, a[1])] {
                    for _ in 0..count {
                        let mut arg = CCAssignment::new();
                        arg.bank = RegBank::new(bank);
                        arg.size = 8;
                        arg.align = 8;
                        assigner.assign_arg(&mut arg);
                        if arg.reg.valid() {
                            assigned |= 1 << arg.reg.id();
                        }
                    }
                }
                let old = machine.with_registers(|registers| {
                    let old = registers.allocatable.get();
                    registers
                        .allocatable
                        .set((CC_INFO.allocatable_regs() & !CC_INFO.arg_regs()) | assigned);
                    old
                });
                tpde_compiler_x64::frame::prologue_end(machine, &mut assigner)?;
                machine.with_registers(|registers| registers.allocatable.set(old));
            }
            "state" => {
                let state = machine.with_target(|target| *target.x64());
                let registers = machine.with_registers(|registers| {
                    [
                        registers.allocatable.get(),
                        registers.used.get(),
                        registers.clobbered.get(),
                    ]
                });
                record(
                    &log,
                    "state",
                    &[
                        machine.with_output(|out| out.offset()) as u64,
                        machine.frame_size().into(),
                        machine.frame_used().into(),
                        machine.frame_generated_call().into(),
                        machine.frame_has_dynamic_alloca().into(),
                        machine.call_is_leaf_function().into(),
                        registers[0],
                        registers[1],
                        registers[2],
                        state.preserve_flags.into(),
                        state.max_callee_stack_arg_size.into(),
                        state.scalar_arg_count.into(),
                        state.vec_arg_count.into(),
                        state.reg_save_frame_off.into(),
                        state.var_arg_stack_off.into(),
                        state.sym_tls_get_addr.valid().into(),
                    ],
                );
            }
            _ => panic!("unknown fixture command {}", cmd.name),
        }
    }
    assert!(held.iter().all(Option::is_none), "unreleased held register");
    Ok(())
}

pub fn run(input: &str) -> String {
    let commands = commands(input);
    if commands[0].name == "ccinit" {
        return cc(&commands);
    }
    assert_eq!(commands[0].name, "init");
    let config = &commands[0].args;
    let context = MemoryContext::new_bump("compiler-x64-fixture");
    let mut ir = TestIR::new(context.mcx());
    ir.parse_ir("foo() {\nentry:\n%fixture = any\nterminate\n}\n")
        .unwrap();
    let log = Rc::new(RefCell::new(String::new()));
    let frontend = Frontend {
        script: commands.clone(),
        log: log.clone(),
        bank: RegBank::new(0),
        size: 8,
        count: 1,
        force_fixed: false,
    };
    let target = CompilerX64::new(context.mcx(), frontend, config[0] as u32);
    with_compiler_base(
        context.mcx(),
        Adaptor::new(&ir, config[1] != 0, config[2] != 0, config[3] != 0),
        target,
        X64Target,
        Architecture::X86_64,
        |mut compiler| {
            compiler.adaptor.start_compile();
            let success = compiler.compile().unwrap();
            compiler.adaptor.end_compile();
            record(&log, "compile", &[success.into()]);
            assert!(success);
            let assembler = compiler.assembler().unwrap();
            let text = assembler
                .data_sections()
                .find(|section| assembler.sec_name(section.get_ref()).unwrap() == b".text")
                .unwrap();
            blob(&log, "text", &text.data);
            blob(&log, "object", &assembler.build_object_file().unwrap());
            if commands.iter().any(|cmd| cmd.name == "reset") {
                compiler.reset().unwrap();
                let counts = compiler.reset_state_counts();
                let target = compiler.target();
                record(
                    &log,
                    "reset",
                    &[
                        target.return_offsets().len() as u64,
                        target.x64().sym_tls_get_addr.valid().into(),
                        counts.0 as u64,
                        counts.1 as u64,
                        counts.2 as u64,
                    ],
                );
            }
        },
    )
    .unwrap();
    Rc::try_unwrap(log).unwrap().into_inner()
}

fn prepare_assignment<'m, 'a, 'ir: 'a, M>(
    machine: &M,
    count: u32,
    bank: RegBank,
    size: u32,
    owned: bool,
    fixed: bool,
) -> PgResult<()>
where
    M: TargetMachine<'m, Adaptor<'a, 'ir>, 2>,
    M::Target: X64CompilerState<Frontend = Frontend>
        + CompilerTarget<Adaptor<'a, 'ir>, 2, Special = ValRefSpecial<()>, Jump = Jump>,
{
    machine.with_target(|target| {
        let frontend = target.frontend_mut();
        frontend.bank = bank;
        frontend.size = size;
        frontend.count = count;
        frontend.force_fixed = fixed;
    });
    let mask = machine.with_target(|target| {
        let mask = target.x64().fixed_assignment_nonallocatable_mask;
        if fixed {
            target.x64_mut().fixed_assignment_nonallocatable_mask = 0;
        }
        mask
    });
    machine.init_assignment(IRValueRef(0), ValLocalIdx::new(0))?;
    machine.with_target(|target| {
        target.frontend_mut().force_fixed = false;
        target.x64_mut().fixed_assignment_nonallocatable_mask = mask;
    });
    machine.with_assignment(ValLocalIdx::new(0), |value| {
        value.set_frame_off(0);
        value.set_references_left(if owned { 1 } else { 2 });
        value.set_max_part_size(size as u8);
        value.set_pending_free(false);
        value.set_variable_ref(false);
        value.set_stack_variable(false);
        value.set_delay_free(false);
    });
    for index in 0..count {
        machine.with_part(ValLocalIdx::new(0), index, |part| {
            if !fixed {
                part.reset();
            }
            part.set_bank(bank);
            part.set_part_size(size);
        });
    }
    Ok(())
}

fn cleanup_assignment<M: GuardMachine<2>>(machine: &M) {
    machine.with_assignment(ValLocalIdx::new(0), |value| value.set_frame_off(0));
    machine.release_assignment(ValLocalIdx::new(0));
}

fn call_target<'s, 'm, 'a, 'ir: 'a, M>(
    machine: &'s M,
    kind: u64,
    value: u64,
) -> PgResult<CallTarget<'s, M, 2>>
where
    M: TargetMachine<'m, Adaptor<'a, 'ir>, 2>,
    M::Target: X64CompilerState<Frontend = Frontend>
        + CompilerTarget<Adaptor<'a, 'ir>, 2, Special = ValRefSpecial<()>, Jump = Jump>,
{
    Ok(if kind == 0 {
        let name = format!("external{value}");
        CallTarget::Symbol(machine.with_output(|output| {
            output
                .assembler_mut()
                .sym_add_undef(name.as_bytes(), SymBinding::Global)
        })?)
    } else {
        assert!(kind <= 2);
        let mut part = ValuePartRef::constant_inline(machine, value, 8, RegBank::new(0));
        if kind == 2 {
            part.load_to_specific(Reg::new(11))?;
        }
        CallTarget::Value(part)
    })
}

fn call_sequence<'m, 'a, 'ir: 'a, M>(
    machine: &M,
    commands: &[Command],
    log: &RefCell<String>,
) -> PgResult<usize>
where
    M: TargetMachine<'m, Adaptor<'a, 'ir>, 2>,
    M::Target: X64CompilerState<Frontend = Frontend>
        + CompilerTarget<Adaptor<'a, 'ir>, 2, Special = ValRefSpecial<()>, Jump = Jump>,
{
    let mut assigner = CCAssigner::new(&CC_INFO, CCAssignerSysV::new(commands[0].args[0] != 0));
    let mut hooks = tpde_compiler_x64::calls::CallBuilder::<'m, Adaptor<'a, 'ir>>::default();
    let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
    for (index, cmd) in commands.iter().enumerate().skip(1) {
        let a = &cmd.args;
        match cmd.name.as_str() {
            "callarg" | "callarg_loaded" => {
                let mut cca = assignment(a);
                cca.int_ext = a[6] as u8;
                let mut value = ValuePartRef::constant_borrowed(
                    machine,
                    &a[7..9],
                    if cca.byval() { 8 } else { cca.size },
                    if cca.byval() {
                        RegBank::new(0)
                    } else {
                        cca.bank
                    },
                );
                if cmd.name == "callarg_loaded" {
                    value.load_to_reg()?;
                }
                builder.add_arg_part(value, cca)?;
            }
            "calltarget" => {
                if a[0] == 3 {
                    prepare_assignment(machine, 1, RegBank::new(0), 8, false, false)?;
                    machine.set_frame_used(true);
                    machine.with_assignment(ValLocalIdx::new(0), |value| value.set_frame_off(-32));
                    machine.with_part(ValLocalIdx::new(0), 0, |part| part.set_stack_valid());
                    let mut owner = ValueRef::new(machine, ValLocalIdx::new(0));
                    builder.call(CallTarget::Value(owner.part(0)))?;
                    owner.disown();
                    cleanup_assignment(machine);
                } else {
                    builder.call(call_target(machine, a[0], a[1])?)?;
                }
            }
            "callret" => {
                prepare_assignment(
                    machine,
                    1,
                    RegBank::new(a[0] as u8),
                    a[1] as u32,
                    true,
                    false,
                )?;
                let mut owner = ValueRef::new(machine, ValLocalIdx::new(0));
                {
                    let mut part = owner.part(0);
                    builder.add_ret_part(&mut part, CCAssignment::new())?;
                    record(log, "callret", &[part.cur_reg_unlocked().id().into()]);
                    part.reset();
                }
                owner.disown();
                cleanup_assignment(machine);
            }
            "callend" => return Ok(index + 1),
            _ => panic!("unexpected call sequence command {}", cmd.name),
        }
    }
    panic!("unclosed call sequence")
}

pub fn failure_reuse() {
    let context = MemoryContext::new_bump("x64-failure-reuse");
    let mut ir = TestIR::new(context.mcx());
    ir.parse_ir("foo() {\nentry:\n%fixture = any\nterminate\n}\n")
        .unwrap();
    let log = Rc::new(RefCell::new(String::new()));
    let frontend = Frontend {
        script: Vec::new(),
        log,
        bank: RegBank::new(0),
        size: 8,
        count: 1,
        force_fixed: false,
    };
    let target = CompilerX64::new(context.mcx(), frontend, 0);
    with_compiler_base(
        context.mcx(),
        Adaptor::new(&ir, false, false, true),
        target,
        X64Target,
        Architecture::X86_64,
        |mut compiler| {
            for failure in ["cancel", "reject"] {
                compiler.target_mut().frontend_mut().script =
                    commands(&format!("init 0 0 0 1\n{failure}\n"));
                compiler.adaptor.start_compile();
                let result = compiler.compile();
                compiler.adaptor.end_compile();
                if failure == "cancel" {
                    assert_eq!(result.unwrap(), false)
                } else {
                    assert!(result.is_err());
                }
                assert!(compiler.assembler().is_err());
                compiler.reset().unwrap();
                compiler.target_mut().frontend_mut().script =
                    commands("init 0 0 0 1\nconstant 0 8 0 29 0\nepilog\n");
                compiler.adaptor.start_compile();
                assert!(compiler.compile().unwrap());
                compiler.adaptor.end_compile();
                let assembler = compiler.assembler().unwrap();
                let text = assembler
                    .data_sections()
                    .find(|section| assembler.sec_name(section.get_ref()).unwrap() == b".text")
                    .unwrap();
                assert_eq!(&*text.data, &[0xb8, 29, 0, 0, 0, 0xc3]);
                compiler.reset().unwrap();
            }
        },
    )
    .unwrap();
}

pub fn budget_refusal() {
    let source = MemoryContext::new_bump("x64-budget-source");
    let mut ir = TestIR::new(source.mcx());
    ir.parse_ir("foo() {\nentry:\nterminate\n}\n").unwrap();
    let context = MemoryContext::new_bump("x64-budget-zero").with_limit(0);
    let frontend = Frontend {
        script: Vec::new(),
        log: Rc::new(RefCell::new(String::new())),
        bank: RegBank::new(0),
        size: 8,
        count: 1,
        force_fixed: false,
    };
    let target = CompilerX64::new(context.mcx(), frontend, 0);
    let result = with_compiler_base(
        context.mcx(),
        Adaptor::new(&ir, false, false, true),
        target,
        X64Target,
        Architecture::X86_64,
        |_| false,
    );
    assert!(result.is_err());
    assert_eq!(context.used(), 0);
}

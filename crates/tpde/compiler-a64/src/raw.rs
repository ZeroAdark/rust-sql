// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::A64CompilerState;
use tpde_assembler::{elf, SectionKind, SymBinding};
use tpde_compiler::{
    target::{CompilerOutput, TargetMachine},
    CompilerAdaptor,
};
use tpde_core::{Reg, RegBank};
use tpde_disarm::*;
use tpde_function_writer::{Label, LabelFixupKind};
use tpde_util::tpde_assert;
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};

#[cold]
fn invalid(message: &'static str) -> alloc::boxed::Box<PgError> {
    PgError::error(message)
        .with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE)
        .into()
}

fn ensure(condition: bool, message: &'static str) -> PgResult<()> {
    if condition {
        Ok(())
    } else {
        Err(invalid(message))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct AsmReg(pub Reg);

macro_rules! registers {
    ($($name:ident = $id:expr),* $(,)?) => {
        impl AsmReg { $(pub const $name: Self = Self(Reg::new($id));)* }
    };
}
registers! {
    R0=0,R1=1,R2=2,R3=3,R4=4,R5=5,R6=6,R7=7,R8=8,R9=9,R10=10,R11=11,
    R12=12,R13=13,R14=14,R15=15,R16=16,R17=17,R18=18,R19=19,R20=20,R21=21,
    R22=22,R23=23,R24=24,R25=25,R26=26,R27=27,R28=28,R29=29,FP=29,R30=30,LR=30,SP=31,
    V0=32,V1=33,V2=34,V3=35,V4=36,V5=37,V6=38,V7=39,V8=40,V9=41,V10=42,V11=43,
    V12=44,V13=45,V14=46,V15=47,V16=48,V17=49,V18=50,V19=51,V20=52,V21=53,
    V22=54,V23=55,V24=56,V25=57,V26=58,V27=59,V28=60,V29=61,V30=62,V31=63,
}
impl AsmReg {
    pub const fn new(id: u64) -> Self {
        tpde_assert!(id < 64);
        Self(Reg::new(id))
    }
    pub const fn id(self) -> u8 {
        self.0.id()
    }
    pub const fn valid(self) -> bool {
        self.0.valid()
    }
    pub const fn make_invalid() -> Self {
        Self(Reg::make_invalid())
    }
}
impl Default for AsmReg {
    fn default() -> Self {
        Self::make_invalid()
    }
}
impl From<Reg> for AsmReg {
    fn from(reg: Reg) -> Self {
        Self(reg)
    }
}
impl From<AsmReg> for Reg {
    fn from(reg: AsmReg) -> Self {
        reg.0
    }
}

pub const fn create_bitmask(regs: &[AsmReg]) -> u64 {
    let mut mask = 0;
    let mut index = 0;
    while index < regs.len() {
        tpde_assert!(regs[index].id() < 64);
        mask |= 1 << regs[index].id();
        index += 1;
    }
    mask
}

pub fn gp(reg: Reg) -> PgResult<GReg> {
    GReg::new(reg.id()).ok_or_else(|| invalid("A64 general register required"))
}
pub fn zr(reg: Reg) -> PgResult<GRegZr> {
    ensure(reg.id() < 31, "A64 general register cannot denote SP as ZR")?;
    Ok(GRegZr::new(reg.id()).unwrap())
}
pub fn sp(reg: Reg) -> PgResult<GRegSp> {
    GRegSp::new(reg.id()).ok_or_else(|| invalid("A64 general or stack register required"))
}
pub fn vector(reg: Reg) -> PgResult<VReg> {
    ensure((32..64).contains(&reg.id()), "A64 vector register required")?;
    Ok(VReg::new(reg.id() - 32).unwrap())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum JumpKind {
    Eq,
    Ne,
    Cs,
    Cc,
    Mi,
    Pl,
    Vs,
    Vc,
    Hi,
    Ls,
    Ge,
    Lt,
    Gt,
    Le,
    Always,
    Cbz,
    Cbnz,
    Tbz,
    Tbnz,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Jump {
    pub kind: JumpKind,
    pub cmp_reg: Reg,
    pub cmp_is_32: bool,
    pub test_bit: u8,
}
impl Jump {
    pub const fn new(kind: JumpKind) -> Self {
        tpde_assert!((kind as u8) <= JumpKind::Always as u8);
        Self {
            kind,
            cmp_reg: Reg::make_invalid(),
            cmp_is_32: false,
            test_bit: 0,
        }
    }
    pub const fn compare(kind: JumpKind, cmp_reg: Reg, cmp_is_32: bool) -> Self {
        tpde_assert!(matches!(kind, JumpKind::Cbz | JumpKind::Cbnz));
        Self {
            kind,
            cmp_reg,
            cmp_is_32,
            test_bit: 0,
        }
    }
    pub const fn test(kind: JumpKind, cmp_reg: Reg, test_bit: u8) -> Self {
        tpde_assert!(matches!(kind, JumpKind::Tbz | JumpKind::Tbnz));
        Self {
            kind,
            cmp_reg,
            cmp_is_32: false,
            test_bit,
        }
    }
    pub const fn change_kind(self, kind: JumpKind) -> Self {
        Self { kind, ..self }
    }
}
impl Default for Jump {
    fn default() -> Self {
        Self::new(JumpKind::Always)
    }
}

pub const fn invert_jump(jump: Jump) -> Jump {
    use JumpKind::*;
    jump.change_kind(match jump.kind {
        Eq => Ne,
        Ne => Eq,
        Cs => Cc,
        Cc => Cs,
        Mi => Pl,
        Pl => Mi,
        Vs => Vc,
        Vc => Vs,
        Hi => Ls,
        Ls => Hi,
        Ge => Lt,
        Lt => Ge,
        Gt => Le,
        Le => Gt,
        Always => Always,
        Cbz => Cbnz,
        Cbnz => Cbz,
        Tbz => Tbnz,
        Tbnz => Tbz,
    })
}
pub fn swap_jump(jump: Jump) -> PgResult<Jump> {
    use JumpKind::*;
    Ok(jump.change_kind(match jump.kind {
        Eq => Eq,
        Ne => Ne,
        Cc => Hi,
        Cs => Ls,
        Hi => Cc,
        Ls => Cs,
        Ge => Le,
        Lt => Gt,
        Gt => Lt,
        Le => Ge,
        Always => Always,
        _ => return Err(invalid("A64 jump cannot swap operands")),
    }))
}
pub fn jump_to_cond(jump: Jump) -> PgResult<Da64Cond> {
    ensure(
        (jump.kind as u8) <= JumpKind::Always as u8,
        "A64 register-test jump has no flags condition",
    )?;
    Ok(jump.kind as Da64Cond)
}

pub fn ensure_space<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    size: usize,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    machine.with_output(|output| output.ensure_space(size))
}
#[inline(always)]
pub fn emit<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    word: u32,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    ensure(word != 0, "A64 instruction cannot be encoded")?;
    machine.with_output(|output| output.write_bytes(&word.to_le_bytes()))
}
fn emit_reserved<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    word: u32,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    ensure(word != 0, "A64 instruction cannot be encoded")?;
    machine.with_output(|output| output.with_base(|base| base.write_unchecked(word)))
}
fn try_emit<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    word: u32,
) -> PgResult<bool>
where
    M::Target: A64CompilerState,
{
    if word == 0 {
        Ok(false)
    } else {
        emit(machine, word)?;
        Ok(true)
    }
}

pub fn has_cpu_feats<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    features: u32,
) -> bool
where
    M::Target: A64CompilerState,
{
    machine.with_target(|target| target.a64_state().cpu_feats & features == features)
}

pub fn load_off<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    dst: Reg,
    mut base: Reg,
    mut off: u32,
    size: u32,
    sign_extend: bool,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    validate_memory_reg(dst, size, sign_extend)?;
    sp(base)?;
    let mask = 0xfff * size;
    if off & !mask != 0 {
        let old_base = base;
        base = if dst.id() <= 30 {
            dst
        } else {
            machine.with_target(|t| t.a64_state().permanent_scratch_reg)
        };
        if try_emit(
            machine,
            de64_ADDxi(sp(base)?, sp(old_base)?, (off & !mask) as i64),
        )? {
            off &= mask;
        } else {
            materialize_constant(machine, &[off as u64], RegBank::new(0), 8, base)?;
            emit(machine, de64_ADDx(zr(base)?, zr(base)?, zr(old_base)?))?;
            off = 0;
        }
    }
    ensure_space(machine, 4)?;
    let address = sp(base)?;
    let offset = off as u64;
    let word = if dst.id() <= 30 {
        let dst = zr(dst)?;
        match (size, sign_extend) {
            (1, false) => de64_LDRBu(dst, address, offset),
            (2, false) => de64_LDRHu(dst, address, offset),
            (4, false) => de64_LDRwu(dst, address, offset),
            (8, _) => de64_LDRxu(dst, address, offset),
            (1, true) => de64_LDRSBwu(dst, address, offset),
            (2, true) => de64_LDRSHwu(dst, address, offset),
            (4, true) => de64_LDRSWxu(dst, address, offset),
            _ => unreachable!(),
        }
    } else {
        let dst = vector(dst)?;
        match size {
            1 => de64_LDRbu(dst, address, offset),
            2 => de64_LDRhu(dst, address, offset),
            4 => de64_LDRsu(dst, address, offset),
            8 => de64_LDRdu(dst, address, offset),
            16 => de64_LDRqu(dst, address, offset),
            _ => unreachable!(),
        }
    };
    emit_reserved(machine, word)
}

fn validate_memory_reg(reg: Reg, size: u32, sign_extend: bool) -> PgResult<()> {
    ensure(
        size.is_power_of_two(),
        "A64 memory operand size must be a power of two",
    )?;
    if reg.id() <= 30 {
        ensure(size <= 8, "A64 GP memory operand exceeds eight bytes")
    } else {
        vector(reg)?;
        ensure(
            size <= 16 && !sign_extend,
            "invalid A64 vector memory operand",
        )
    }
}

pub fn store_off<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    mut base: Reg,
    mut off: u32,
    src: Reg,
    size: u32,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    validate_memory_reg(src, size, false)?;
    sp(base)?;
    ensure_space(machine, 8)?;
    let mask = 0xfff * size;
    if off & !mask != 0 {
        let scratch = machine.with_target(|t| t.a64_state().permanent_scratch_reg);
        if try_emit(
            machine,
            de64_ADDxi(sp(scratch)?, sp(base)?, (off & !mask) as i64),
        )? {
            off &= mask;
        } else {
            materialize_constant(machine, &[off as u64], RegBank::new(0), 8, scratch)?;
            emit(machine, de64_ADDx(zr(scratch)?, zr(scratch)?, zr(base)?))?;
            off = 0;
        }
        base = scratch;
    }
    let address = sp(base)?;
    let offset = off as u64;
    let word = if src.id() <= 30 {
        let src = zr(src)?;
        match size {
            1 => de64_STRBu(src, address, offset),
            2 => de64_STRHu(src, address, offset),
            4 => de64_STRwu(src, address, offset),
            8 => de64_STRxu(src, address, offset),
            _ => unreachable!(),
        }
    } else {
        let src = vector(src)?;
        match size {
            1 => de64_STRbu(src, address, offset),
            2 => de64_STRhu(src, address, offset),
            4 => de64_STRsu(src, address, offset),
            8 => de64_STRdu(src, address, offset),
            16 => de64_STRqu(src, address, offset),
            _ => unreachable!(),
        }
    };
    emit_reserved(machine, word)
}

pub fn mov<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    dst: Reg,
    src: Reg,
    size: u32,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    let word = match (dst.id() <= 31, src.id() <= 31) {
        (true, true) => {
            if size > 4 {
                de64_MOVx(zr(dst)?, zr(src)?)
            } else {
                de64_MOVw(zr(dst)?, zr(src)?)
            }
        }
        (false, false) => de64_ORR16b(vector(dst)?, vector(src)?, vector(src)?),
        (true, false) => {
            ensure(size <= 8, "A64 vector-to-GP move exceeds eight bytes")?;
            if size <= 4 {
                de64_FMOVws(zr(dst)?, vector(src)?)
            } else {
                de64_FMOVxd(zr(dst)?, vector(src)?)
            }
        }
        (false, true) => {
            ensure(size <= 8, "A64 GP-to-vector move exceeds eight bytes")?;
            if size <= 4 {
                de64_FMOVsw(vector(dst)?, zr(src)?)
            } else {
                de64_FMOVdx(vector(dst)?, zr(src)?)
            }
        }
    };
    ensure_space(machine, 4)?;
    emit_reserved(machine, word)
}

pub fn materialize_constant<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    words: &[u64],
    bank: RegBank,
    size: u32,
    dst: Reg,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    ensure(
        size <= 16 && !words.is_empty() && (size <= 8 || words.len() >= 2),
        "invalid A64 constant buffer",
    )?;
    let low = words[0];
    if bank == RegBank::new(0) {
        ensure(size <= 8, "A64 GP constant exceeds eight bytes")?;
        let reg = gp(dst)?;
        ensure_space(machine, 20)?;
        if low == 0 {
            return emit_reserved(machine, de64_MOVZw(zr(dst)?, 0));
        }
        let mut encoded = [0; 4];
        let count = de64_MOVconst(&mut encoded, reg, low)
            .map_err(|_| invalid("A64 constant output buffer exhausted"))?;
        for &word in &encoded[..count] {
            emit_reserved(machine, word)?;
        }
        return Ok(());
    }
    ensure(
        bank == RegBank::new(1),
        "invalid A64 constant register bank",
    )?;
    let reg = vector(dst)?;
    ensure_space(machine, 20)?;
    let immediate = match size {
        4 => {
            try_emit(machine, de64_FMOVsi(reg, f32::from_bits(low as u32)))?
                || try_emit(machine, de64_MOVId(reg, low as u32 as u64))?
        }
        8 => {
            try_emit(machine, de64_FMOVdi(reg, f64::from_bits(low)))?
                || try_emit(machine, de64_MOVId(reg, low))?
        }
        16 => {
            (low == words[1] && try_emit(machine, de64_MOVI2d(reg, low))?)
                || (words[1] == 0 && try_emit(machine, de64_MOVId(reg, low))?)
        }
        _ => false,
    };
    if immediate {
        return Ok(());
    }
    let scratch = machine.with_target(|t| t.a64_state().permanent_scratch_reg);
    machine.with_registers(|registers| registers.mark_clobbered(scratch));
    if size <= 8 {
        materialize_constant(machine, words, RegBank::new(0), size, scratch)?;
        return emit_reserved(
            machine,
            if size <= 4 {
                de64_FMOVsw(reg, zr(scratch)?)
            } else {
                de64_FMOVdx(reg, zr(scratch)?)
            },
        );
    }
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&low.to_le_bytes());
    bytes[8..].copy_from_slice(&words[1].to_le_bytes());
    let symbol = machine.with_output(|output| {
        let assembler = output.assembler_mut();
        let section = assembler.get_default_section(SectionKind::ReadOnly)?;
        assembler
            .sym_def_data(section, b"", &bytes[..size as usize], 16, SymBinding::Local)
            .map(|(symbol, _)| symbol)
    })?;
    ensure_space(machine, 8)?;
    let offset = machine.with_output(|output| output.offset()) as u64;
    machine.reloc_text(symbol, elf::R_AARCH64_ADR_PREL_PG_HI21, offset, 0)?;
    emit_reserved(machine, de64_ADRP(zr(scratch)?, 0, 0))?;
    machine.reloc_text(symbol, elf::R_AARCH64_LDST128_ABS_LO12_NC, offset + 4, 0)?;
    emit_reserved(machine, de64_LDRqu(reg, sp(scratch)?, 0))
}

fn branch_word(jump: Jump, displacement: isize) -> PgResult<u32> {
    use JumpKind::*;
    Ok(match jump.kind {
        Always => de64_B(displacement),
        Cbz => {
            if jump.cmp_is_32 {
                de64_CBZw(zr(jump.cmp_reg)?, displacement)
            } else {
                de64_CBZx(zr(jump.cmp_reg)?, displacement)
            }
        }
        Cbnz => {
            if jump.cmp_is_32 {
                de64_CBNZw(zr(jump.cmp_reg)?, displacement)
            } else {
                de64_CBNZx(zr(jump.cmp_reg)?, displacement)
            }
        }
        Tbz => de64_TBZ(zr(jump.cmp_reg)?, jump.test_bit as u32, displacement),
        Tbnz => de64_TBNZ(zr(jump.cmp_reg)?, jump.test_bit as u32, displacement),
        _ => de64_BCOND(jump_to_cond(jump)?, displacement),
    })
}

pub fn generate_raw_jump<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    jump: Jump,
    label: Label,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    let pending =
        machine.with_output(|output| output.with_base(|base| base.label_is_pending(label)))?;
    ensure(branch_word(jump, 0)? != 0, "invalid A64 branch operand")?;
    ensure_space(machine, 4)?;
    let offset = if pending {
        0
    } else {
        machine.with_output(|output| {
            let label_offset = output.with_base(|base| base.label_offset(label))?;
            output
                .offset()
                .checked_sub(label_offset as usize)
                .ok_or_else(|| invalid("A64 branch target follows current offset"))
        })?
    };
    ensure(
        offset & 3 == 0 && offset < 128 * 1024 * 1024,
        "A64 branch target is unaligned or out of range",
    )?;
    let (limit, fixup) = match jump.kind {
        JumpKind::Always => (128 * 1024 * 1024, LabelFixupKind::AARCH64_BR),
        JumpKind::Tbz | JumpKind::Tbnz => (32 * 1024, LabelFixupKind::AARCH64_TEST_BR),
        _ => (1024 * 1024, LabelFixupKind::AARCH64_COND_BR),
    };
    if offset <= limit {
        emit_reserved(machine, branch_word(jump, -(offset as isize) / 4)?)?;
        if pending {
            machine.with_output(|output| {
                let offset = u32::try_from(output.offset() - 4)
                    .map_err(|_| invalid("A64 branch fixup exceeds u32"))?;
                output.label_ref(label, offset, fixup)
            })?;
        }
    } else {
        ensure_space(machine, 8)?;
        emit_reserved(machine, branch_word(invert_jump(jump), 2)?)?;
        emit_reserved(machine, de64_B(-((offset + 4) as isize) / 4))?;
    }
    Ok(())
}

pub fn generate_raw_set<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    condition: Jump,
    dst: Reg,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    emit(machine, de64_CSETw(zr(dst)?, jump_to_cond(condition)?))
}
pub fn generate_raw_mask<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    condition: Jump,
    dst: Reg,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    emit(machine, de64_CSETMx(zr(dst)?, jump_to_cond(condition)?))
}
pub fn generate_raw_select<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    condition: Jump,
    dst: Reg,
    if_true: Reg,
    if_false: Reg,
    width64: bool,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    let cond = jump_to_cond(condition)?;
    emit(
        machine,
        if width64 {
            de64_CSELx(zr(dst)?, zr(if_true)?, zr(if_false)?, cond)
        } else {
            de64_CSELw(zr(dst)?, zr(if_true)?, zr(if_false)?, cond)
        },
    )
}
pub fn generate_raw_intext<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    dst: Reg,
    src: Reg,
    sign: bool,
    from: u32,
    to: u32,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    ensure(
        from < to && to <= 64,
        "invalid A64 integer extension widths",
    )?;
    emit(
        machine,
        match (sign, to <= 32) {
            (true, true) => de64_SBFXw(zr(dst)?, zr(src)?, 0, from),
            (true, false) => de64_SBFXx(zr(dst)?, zr(src)?, 0, from),
            (false, true) => de64_UBFXw(zr(dst)?, zr(src)?, 0, from),
            (false, false) => de64_UBFXx(zr(dst)?, zr(src)?, 0, from),
        },
    )
}
pub fn generate_raw_bfi<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    dst: Reg,
    src: Reg,
    lsb: u32,
    width: u32,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    emit(machine, de64_BFIx(zr(dst)?, zr(src)?, lsb, width))
}
pub fn generate_raw_bfiz<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    dst: Reg,
    src: Reg,
    lsb: u32,
    width: u32,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    emit(machine, de64_UBFIZx(zr(dst)?, zr(src)?, lsb, width))
}

pub fn switch_emit_cmp<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    cmp: Reg,
    tmp: Reg,
    value: u64,
    width32: bool,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    let word = if width32 {
        de64_CMPwi(sp(cmp)?, value as i32)
    } else {
        de64_CMPxi(sp(cmp)?, value as i64)
    };
    if !try_emit(machine, word)? {
        materialize_constant(machine, &[value], RegBank::new(0), 4, tmp)?;
        emit(
            machine,
            if width32 {
                de64_CMPw(zr(cmp)?, zr(tmp)?)
            } else {
                de64_CMPx(zr(cmp)?, zr(tmp)?)
            },
        )?;
    }
    Ok(())
}
pub fn switch_emit_cmpeq<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    label: Label,
    cmp: Reg,
    tmp: Reg,
    value: u64,
    width32: bool,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    switch_emit_cmp(machine, cmp, tmp, value, width32)?;
    generate_raw_jump(machine, Jump::new(JumpKind::Eq), label)
}
pub fn switch_emit_binary_step<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    equal: Label,
    greater: Label,
    cmp: Reg,
    tmp: Reg,
    value: u64,
    width32: bool,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    switch_emit_cmpeq(machine, equal, cmp, tmp, value, width32)?;
    generate_raw_jump(machine, Jump::new(JumpKind::Hi), greater)
}
pub fn switch_create_jump_table<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    default: Label,
    cmp: Reg,
    tmp: Reg,
    low: u64,
    high: u64,
    width32: bool,
    fill: impl FnOnce(&mut [Label]),
) -> PgResult<bool>
where
    M::Target: A64CompilerState,
{
    let range = high
        .checked_sub(low)
        .and_then(|range| range.checked_add(1))
        .and_then(|range| u32::try_from(range).ok())
        .ok_or_else(|| invalid("A64 jump table range exceeds u32"))?;
    if low > 0 {
        let word = if width32 {
            de64_SUBwi(sp(cmp)?, sp(cmp)?, low as i32)
        } else {
            de64_SUBxi(sp(cmp)?, sp(cmp)?, low as i64)
        };
        if !try_emit(machine, word)? {
            materialize_constant(machine, &[low], RegBank::new(0), 4, tmp)?;
            emit(
                machine,
                if width32 {
                    de64_SUBw(zr(cmp)?, zr(cmp)?, zr(tmp)?)
                } else {
                    de64_SUBx(zr(cmp)?, zr(cmp)?, zr(tmp)?)
                },
            )?;
        }
    }
    switch_emit_cmp(machine, cmp, tmp, high - low, width32)?;
    generate_raw_jump(machine, Jump::new(JumpKind::Hi), default)?;
    machine.with_output(|output| output.jump_table(range, cmp, tmp, width32, fill))?;
    Ok(true)
}

#[cfg(test)]
#[path = "raw_tests.rs"]
mod tests;

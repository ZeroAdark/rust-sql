// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use mcx::Mcx;
use tpde_assembler::{elf, SymBinding};
use tpde_core::Reg;
use tpde_disarm::*;
use tpde_function_writer::{
    FunctionWriter, FunctionWriterBase, FunctionWriterJumpTables, FunctionWriterTarget, Label,
    LabelFixupKind, ModuleWriter, TargetCieInfo,
};
use tpde_util::{dwarf, SmallVec};
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};

const TBZ_MAX_DIST: u32 = 4 << 13;
const CBZ_MAX_DIST: u32 = 4 << 18;
const MAX_SECTION: usize = 128 * 1024 * 1024;
const TABLE_CODE_SIZE: usize = 24;

#[cold]
fn invalid(message: &'static str) -> alloc::boxed::Box<PgError> {
    PgError::error(message)
        .with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE)
        .into()
}
fn ensure(ok: bool, message: &'static str) -> PgResult<()> {
    if ok {
        Ok(())
    } else {
        Err(invalid(message))
    }
}
fn word(base: &FunctionWriterBase<'_, '_, '_>, off: u32) -> PgResult<u32> {
    let bytes = base
        .data()?
        .get(off as usize..off as usize + 4)
        .ok_or_else(|| invalid("A64 fixup outside emitted text"))?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}
fn patch(base: &mut FunctionWriterBase<'_, '_, '_>, off: u32, inst: u32) -> PgResult<()> {
    let bytes = base
        .data_mut()?
        .get_mut(off as usize..off as usize + 4)
        .ok_or_else(|| invalid("A64 fixup outside emitted text"))?;
    bytes.copy_from_slice(&inst.to_le_bytes());
    Ok(())
}
fn reg(value: Reg) -> PgResult<GReg> {
    GReg::new(value.id()).ok_or_else(|| invalid("A64 register exceeds 31"))
}

pub struct A64Target<'m> {
    veneers: SmallVec<'m, u32, 16>,
    unresolved_cond_brs: u32,
    unresolved_test_brs: u32,
}
pub type FunctionWriterA64<'m> = FunctionWriter<'m, A64Target<'m>>;

impl<'m> A64Target<'m> {
    pub fn new(mcx: Mcx<'m>) -> Self {
        Self {
            veneers: SmallVec::new(mcx),
            unresolved_cond_brs: 0,
            unresolved_test_brs: 0,
        }
    }
    fn fix_condbr(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        label_off: u32,
        fixup_off: u32,
        raw_fixup_off: u32,
        nbits: u32,
    ) -> PgResult<()> {
        let mut diff = i64::from(label_off) - i64::from(fixup_off);
        ensure(
            (0..MAX_SECTION as i64).contains(&diff),
            "A64 branch displacement out of range",
        )?;
        if diff >= (4i64 << (nbits - 1)) {
            // A consumed veneer cursor may equal the next real branch's raw offset.
            let index = self.veneers.partition_point(|&off| off <= raw_fixup_off);
            let veneer = *self
                .veneers
                .get(index)
                .ok_or_else(|| invalid("A64 branch has no reachable veneer"))?;
            let off = veneer
                .checked_sub(base.label_skew())
                .ok_or_else(|| invalid("A64 veneer precedes prologue skew"))?;
            ensure(
                word(base, off)? == 0,
                "A64 veneer would overwrite instructions",
            )?;
            let displacement = label_off
                .wrapping_sub(veneer)
                .wrapping_add(base.label_skew())
                / 4;
            let inst = de64_B(displacement as isize);
            ensure(inst != 0, "A64 veneer branch cannot be encoded")?;
            patch(base, off, inst)?;
            diff = i64::from(off) - i64::from(fixup_off);
            self.veneers[index] = veneer
                .checked_add(4)
                .ok_or_else(|| invalid("A64 veneer offset overflow"))?;
        }
        let mask = ((1u32 << nbits) - 1) << 5;
        let inst = (word(base, fixup_off)? & !mask) | (((diff / 4) as u32) << 5);
        patch(base, fixup_off, inst)
    }
}
impl Drop for A64Target<'_> {
    fn drop(&mut self) {
        self.veneers.release();
    }
}
const _: () = assert!(core::mem::size_of::<A64Target<'_>>() <= 128);

impl FunctionWriterTarget for A64Target<'_> {
    const CIE: TargetCieInfo<'static> = TargetCieInfo {
        instrs: &[dwarf::DW_CFA_def_cfa, 31, 0],
        return_addr_register: 30,
        code_alignment_factor: 4,
        data_alignment_factor: 120,
    };
    fn before_begin_func(&mut self) {
        self.veneers.clear();
        self.unresolved_cond_brs = 0;
        self.unresolved_test_brs = 0;
    }
    fn more_space(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        size: usize,
    ) -> PgResult<()> {
        ensure(
            base.allocated_size() < MAX_SECTION,
            "AArch64 doesn't support sections larger than 128 MiB",
        )?;
        let unresolved = self
            .unresolved_cond_brs
            .checked_add(self.unresolved_test_brs)
            .ok_or_else(|| invalid("A64 unresolved branch count overflow"))?;
        let veneer_size = if unresolved == 0 {
            0
        } else {
            unresolved
                .checked_mul(4)
                .and_then(|n| n.checked_add(4))
                .ok_or_else(|| invalid("A64 veneer size overflow"))?
        };
        let required = size
            .checked_add(veneer_size as usize)
            .ok_or_else(|| invalid("A64 reservation size overflow"))?;
        base.more_space(required)?;
        let max_dist = if self.unresolved_test_brs != 0 {
            TBZ_MAX_DIST
        } else {
            CBZ_MAX_DIST
        };
        let max_dist = max_dist.wrapping_sub(veneer_size.wrapping_sub(4));
        let first_condbr = self.veneers.last().copied().unwrap_or(0);
        if unresolved != 0 && first_condbr.wrapping_add(max_dist) as usize <= base.allocated_size()
        {
            let off =
                u32::try_from(base.offset()).map_err(|_| invalid("A64 text offset exceeds u32"))?;
            self.veneers.push(
                off.checked_add(4)
                    .ok_or_else(|| invalid("A64 veneer offset overflow"))?,
            )?;
            self.unresolved_cond_brs = 0;
            self.unresolved_test_brs = 0;
            base.write_unchecked(de64_B((veneer_size / 4) as isize))?;
            base.skip_unchecked(veneer_size as usize - 4)?;
        }
        base.cap_reserved_end(
            base.allocated_size()
                .min(base.offset() + TBZ_MAX_DIST as usize / 2),
        )
    }
    #[inline(always)]
    fn ensure_space(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        size: usize,
    ) -> PgResult<()> {
        ensure(
            size <= TBZ_MAX_DIST as usize / 2,
            "A64 advancement exceeds 16 KiB",
        )?;
        base.ensure_space_with(self, size)
    }
    fn align(&mut self, base: &mut FunctionWriterBase<'_, '_, '_>, align: usize) -> PgResult<()> {
        ensure(align.is_power_of_two(), "invalid A64 function alignment")?;
        let section_align =
            u32::try_from(align).map_err(|_| invalid("A64 alignment exceeds u32"))?;
        if base.allocated_size() - base.offset() < align {
            self.more_space(base, align)?;
        }
        let padding = base.offset().wrapping_neg() & (align - 1);
        base.skip_unchecked(padding)?;
        let section = base.get_sec_ref();
        let data = base.assembler_mut().get_section_mut(section)?;
        data.align = data.align.max(section_align);
        Ok(())
    }
    fn label_ref(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        label: Label,
        off: u32,
        kind: LabelFixupKind,
    ) -> PgResult<()> {
        base.label_ref(label, off, kind)?;
        if kind == LabelFixupKind::AARCH64_COND_BR {
            self.unresolved_cond_brs = self.unresolved_cond_brs.wrapping_add(1);
        } else if kind == LabelFixupKind::AARCH64_TEST_BR {
            self.unresolved_test_brs = self.unresolved_test_brs.wrapping_add(1);
        }
        Ok(())
    }
    fn eh_advance(&mut self, base: &mut FunctionWriterBase<'_, '_, '_>, size: u64) -> PgResult<()> {
        base.eh_advance_raw(size / 4)
    }
    fn handle_fixups(&mut self, base: &mut FunctionWriterBase<'_, '_, '_>) -> PgResult<()> {
        for index in 0..base.label_fixups().len() {
            let fixup = base.label_fixups()[index];
            let label_off = base.label_offset(fixup.label)?;
            let off = fixup
                .off
                .checked_sub(base.label_skew())
                .ok_or_else(|| invalid("A64 fixup precedes prologue skew"))?;
            match fixup.kind {
                LabelFixupKind::AARCH64_BR => {
                    let diff = i64::from(label_off) - i64::from(off);
                    ensure(
                        (0..MAX_SECTION as i64).contains(&diff),
                        "A64 branch displacement out of range",
                    )?;
                    patch(base, off, de64_B((diff / 4) as isize))?;
                }
                LabelFixupKind::AARCH64_COND_BR | LabelFixupKind::AARCH64_TEST_BR => {
                    let conditional = fixup.kind == LabelFixupKind::AARCH64_COND_BR;
                    if self
                        .veneers
                        .last()
                        .is_none_or(|&veneer| veneer <= fixup.off)
                    {
                        let count = if conditional {
                            &mut self.unresolved_cond_brs
                        } else {
                            &mut self.unresolved_test_brs
                        };
                        *count = count
                            .checked_sub(1)
                            .ok_or_else(|| invalid("A64 unresolved branch counter underflow"))?;
                    }
                    self.fix_condbr(
                        base,
                        label_off,
                        off,
                        fixup.off,
                        if conditional { 19 } else { 14 },
                    )?;
                }
                _ => return Err(invalid("unexpected A64 label fixup kind")),
            }
        }
        let section = base.get_jump_table_section()?;
        for table in 0..base.jump_table_count() {
            let header = *base.jump_table_header(table)?;
            let code_off = header
                .off
                .checked_sub(base.label_skew())
                .ok_or_else(|| invalid("A64 table precedes prologue skew"))?;
            let mut table_base = code_off
                .checked_add(12)
                .ok_or_else(|| invalid("A64 jump table offset overflow"))?;
            let mut min_off = table_base;
            let mut max_off = table_base;
            for &label in base.jump_table_labels(table)? {
                let off = base.label_offset(label)?;
                min_off = min_off.min(off);
                max_off = max_off.max(off);
            }
            let element_size = if max_off - min_off < 1 << 10 {
                table_base = min_off;
                1
            } else if max_off - min_off < 1 << 18 {
                table_base = min_off;
                2
            } else {
                4
            };
            let idx = reg(header.idx)?;
            let tmp = reg(header.tmp)?;
            let load = match (element_size, header.misc != 0) {
                (1, true) => de64_LDRBr_uxtw(idx.into(), tmp.into(), idx.into(), true),
                (1, false) => de64_LDRBr_lsl(idx.into(), tmp.into(), idx.into(), true),
                (2, true) => de64_LDRHr_uxtw(idx.into(), tmp.into(), idx.into(), true),
                (2, false) => de64_LDRHr_lsl(idx.into(), tmp.into(), idx.into(), true),
                (4, true) => de64_LDRSWxr_uxtw(idx.into(), tmp.into(), idx.into(), true),
                (4, false) => de64_LDRSWxr_lsl(idx.into(), tmp.into(), idx.into(), true),
                _ => unreachable!(),
            };
            let code = [
                de64_ADRP(tmp.into(), 0, 0),
                de64_ADDxi(tmp.into(), tmp.into(), 0),
                load,
                de64_ADR(tmp.into(), (code_off + 12) as usize, table_base as usize),
                de64_ADDx_lsl(tmp.into(), tmp.into(), idx.into(), 2),
                de64_BR(tmp.into()),
            ];
            ensure(
                code.iter().all(|&inst| inst != 0),
                "A64 jump table instruction cannot be encoded",
            )?;
            let sym = base
                .assembler_mut()
                .sym_predef_data(b"", SymBinding::Local)?;
            for (index, inst) in code.into_iter().enumerate() {
                patch(base, code_off + index as u32 * 4, inst)?;
            }
            base.reloc(sym, elf::R_AARCH64_ADR_PREL_PG_HI21, u64::from(code_off), 0)?;
            base.reloc(
                sym,
                elf::R_AARCH64_ADD_ABS_LO12_NC,
                u64::from(code_off + 4),
                0,
            )?;
            let section_off = base.assembler_mut().sym_def_predef_data(
                section,
                sym,
                u64::from(header.size()) * element_size as u64,
                element_size,
            )?;
            for index in 0..header.size() as usize {
                let label = base.jump_table_labels(table)?[index];
                let diff = base.label_offset(label)?.wrapping_sub(table_base);
                ensure(
                    u64::from(diff) < 4u64 << (8 * element_size),
                    "A64 jump table element out of range",
                )?;
                // LDRSW requires a signed word displacement for backward table entries.
                let value = if element_size == 4 {
                    ((diff as i32) >> 2) as u32
                } else {
                    diff >> 2
                };
                let bytes = value.to_le_bytes();
                let off = section_off as usize + index * element_size as usize;
                base.assembler_mut().get_section_mut(section)?.data
                    [off..off + element_size as usize]
                    .copy_from_slice(&bytes[..element_size as usize]);
            }
        }
        Ok(())
    }
}
impl FunctionWriterJumpTables for A64Target<'_> {
    fn create_jump_table(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        size: u32,
        index: Reg,
        temporary: Reg,
        index_is_32: bool,
        fill: impl FnOnce(&mut [Label]),
    ) -> PgResult<()> {
        reg(index)?;
        reg(temporary)?;
        let table = base.jump_table_count();
        base.alloc_jump_table(size, index, temporary)?
            .set_misc(u8::from(index_is_32));
        self.ensure_space(base, TABLE_CODE_SIZE)?;
        let off =
            u32::try_from(base.offset()).map_err(|_| invalid("A64 table offset exceeds u32"))?;
        // Growth can insert veneers between table allocation and its code reservation.
        base.jump_table_mut(table)?.set_off(off);
        base.skip_unchecked(TABLE_CODE_SIZE)?;
        fill(base.jump_table_mut(table)?.labels_mut());
        Ok(())
    }
}
pub trait A64WriteInst {
    fn try_write_inst(&mut self, inst: u32) -> PgResult<bool>;
    fn write_inst(&mut self, inst: u32) -> PgResult<()>;
    fn write_inst_unchecked(&mut self, inst: u32) -> PgResult<()>;
}
impl A64WriteInst for ModuleWriter<'_, '_, '_, A64Target<'_>> {
    fn try_write_inst(&mut self, inst: u32) -> PgResult<bool> {
        if inst == 0 {
            return Ok(false);
        }
        self.write(inst)?;
        Ok(true)
    }
    fn write_inst(&mut self, inst: u32) -> PgResult<()> {
        ensure(inst != 0, "cannot write invalid A64 instruction")?;
        self.write(inst)
    }
    fn write_inst_unchecked(&mut self, inst: u32) -> PgResult<()> {
        ensure(inst != 0, "cannot write invalid A64 instruction")?;
        self.write_unchecked(inst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spilled_veneer_storage_is_retained_on_reset_and_released_on_drop() {
        let context = mcx::MemoryContext::new("A64 veneer owner");
        {
            let mut target = A64Target::new(context.mcx());
            for off in 0..33 {
                target.veneers.push(off * 4).unwrap();
            }
            target.unresolved_cond_brs = 19;
            target.unresolved_test_brs = 3;
            let used = context.stats().used;
            let capacity = target.veneers.capacity();
            assert!(used > 0);
            assert!(capacity >= 33);
            target.before_begin_func();
            assert!(target.veneers.is_empty());
            assert_eq!(target.veneers.capacity(), capacity);
            assert_eq!(context.stats().used, used);
            assert_eq!(target.unresolved_cond_brs, 0);
            assert_eq!(target.unresolved_test_brs, 0);
            for off in 0..33 {
                target.veneers.push(off * 8).unwrap();
            }
            assert_eq!(context.stats().used, used);
        }
        assert_eq!(context.stats().used, 0);
    }
}

// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![no_std]
#![doc = include_str!("../tests/ownership.md")]
extern crate alloc;

use tpde_core::Reg;
use tpde_fadec::{
    fe64_JMPr, fe64_LEA64rm, fe64_MOV32rm, fe64_NOP, fe64_SUB64rr, EncodeError, FeRegGP, Output,
    FE_GP, FE_IP, FE_MEM, FE_NOREG,
};
use tpde_function_writer::{
    FunctionWriter, FunctionWriterBase, FunctionWriterJumpTables, FunctionWriterTarget, Label,
    LabelFixupKind, TargetCieInfo,
};
use tpde_util::dwarf;
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};

pub const JUMP_TABLE_CODE_SIZE: usize = 18;
pub type FunctionWriterX64<'m> = FunctionWriter<'m, X64Target>;

#[derive(Clone, Copy, Debug, Default)]
pub struct X64Target;

#[cold]
fn invalid(message: &'static str) -> alloc::boxed::Box<PgError> {
    PgError::error(message)
        .with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE)
        .into()
}

#[cold]
fn encoding_error(_: EncodeError) -> alloc::boxed::Box<PgError> {
    invalid("invalid X64 writer instruction encoding")
}

fn registers(index: Reg, temporary: Reg) -> PgResult<(FeRegGP, FeRegGP)> {
    let index = FE_GP(index.id()).map_err(encoding_error)?;
    let temporary = FE_GP(temporary.id()).map_err(encoding_error)?;
    FE_MEM(temporary, 4, index, 0).map_err(encoding_error)?;
    Ok((index, temporary))
}

impl X64Target {
    pub const fn new() -> Self {
        Self
    }
}

impl FunctionWriterTarget for X64Target {
    const CIE: TargetCieInfo<'static> = TargetCieInfo {
        instrs: &[
            dwarf::DW_CFA_def_cfa,
            dwarf::x64::DW_reg_rsp,
            8,
            dwarf::DW_CFA_offset | dwarf::x64::DW_reg_ra,
            1,
        ],
        return_addr_register: dwarf::x64::DW_reg_ra,
        code_alignment_factor: 1,
        data_alignment_factor: 120,
    };

    fn align(&mut self, base: &mut FunctionWriterBase<'_, '_, '_>, align: usize) -> PgResult<()> {
        let old = base.offset();
        base.align(align)?;
        let count = base.offset() - old;
        if count != 0 {
            let data = base.data_mut()?;
            let address = data.as_ptr().addr().wrapping_add(old);
            let mut output = Output::new(&mut data[old..], address);
            fe64_NOP(&mut output, count as u32).map_err(encoding_error)?;
        }
        Ok(())
    }

    fn handle_fixups(&mut self, base: &mut FunctionWriterBase<'_, '_, '_>) -> PgResult<()> {
        for fixup in base.label_fixups() {
            if fixup.kind != LabelFixupKind::X64_JMP_OR_MEM_DISP {
                return Err(invalid("unexpected X64 label fixup kind"));
            }
            base.label_offset(fixup.label)?;
            let off = fixup
                .off
                .checked_sub(base.label_skew())
                .ok_or_else(|| invalid("X64 fixup precedes removed prologue"))?;
            if off as usize + 4 > base.offset() {
                return Err(invalid("X64 fixup exceeds function text"));
            }
        }
        for index in 0..base.label_fixups().len() {
            let fixup = base.label_fixups()[index];
            let off = fixup.off - base.label_skew();
            let value = base
                .label_offset(fixup.label)?
                .wrapping_sub(off)
                .wrapping_sub(4);
            base.data_mut()?[off as usize..off as usize + 4].copy_from_slice(&value.to_le_bytes());
        }
        for table in 0..base.jump_table_count() {
            let header = *base.jump_table_header(table)?;
            let (index, temporary) = registers(header.idx, header.tmp)?;
            let code_off = header
                .off
                .checked_sub(base.label_skew())
                .ok_or_else(|| invalid("X64 jump table precedes removed prologue"))?
                as usize;
            if code_off + JUMP_TABLE_CODE_SIZE > base.offset() {
                return Err(invalid("X64 jump table code exceeds function text"));
            }
            self.align(base, 4)?;
            let bytes = (header.size() as usize)
                .checked_mul(4)
                .ok_or_else(|| invalid("X64 jump table size overflow"))?;
            self.ensure_space(base, bytes)?;
            let table_off = u32::try_from(base.offset())
                .map_err(|_| invalid("X64 jump table offset exceeds u32"))?;
            for &label in base.jump_table_labels(table)? {
                if base.label_offset(label)? >= table_off {
                    return Err(invalid("X64 jump table target must precede table"));
                }
            }
            let memory = FE_MEM(
                FE_IP,
                0,
                FE_NOREG,
                table_off.wrapping_sub(code_off as u32) as i32,
            )
            .map_err(encoding_error)?;
            {
                let data = base.data_mut()?;
                let address = data.as_ptr().addr().wrapping_add(code_off);
                let mut output = Output::new(
                    &mut data[code_off..code_off + JUMP_TABLE_CODE_SIZE],
                    address,
                );
                fe64_LEA64rm(&mut output, 0, temporary, memory).map_err(encoding_error)?;
                fe64_MOV32rm(
                    &mut output,
                    0,
                    index,
                    FE_MEM(temporary, 4, index, 0).map_err(encoding_error)?,
                )
                .map_err(encoding_error)?;
                fe64_SUB64rr(&mut output, 0, temporary, index).map_err(encoding_error)?;
                fe64_JMPr(&mut output, 0, temporary).map_err(encoding_error)?;
                let remaining = output.remaining();
                if remaining != 0 {
                    fe64_NOP(&mut output, remaining as u32).map_err(encoding_error)?;
                }
            }
            for entry in 0..header.size() as usize {
                let label = base.jump_table_labels(table)?[entry];
                base.write_unchecked(table_off - base.label_offset(label)?)?;
            }
        }
        Ok(())
    }
}

impl FunctionWriterJumpTables for X64Target {
    fn create_jump_table(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        size: u32,
        index: Reg,
        temporary: Reg,
        _index_is_32: bool,
        fill: impl FnOnce(&mut [Label]),
    ) -> PgResult<()> {
        registers(index, temporary)?;
        let table = base.jump_table_count();
        base.alloc_jump_table(size, index, temporary)?;
        self.ensure_space(base, JUMP_TABLE_CODE_SIZE)?;
        base.skip_unchecked(JUMP_TABLE_CODE_SIZE)?;
        fill(base.jump_table_mut(table)?.labels_mut());
        Ok(())
    }
}

const _: () = {
    assert!(core::mem::size_of::<X64Target>() <= 128);
    assert!(!core::mem::needs_drop::<X64Target>());
};

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests;

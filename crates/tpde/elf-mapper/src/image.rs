// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{ensure, invalid};
use mcx::{Mcx, PgBox};
use tpde_assembler::{elf::*, Architecture, AssemblerElf, Relocation, SecRef, SectionKind, SymRef};
use tpde_util::SmallVec;
use types_error::PgResult;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Permission {
    pub offset: usize,
    pub flags: u32,
}
#[derive(Clone, Copy)]
struct Section {
    reference: SecRef,
    key: u32,
}
mcx::forget_safe_nodrop!(Permission, Section);

struct State<'m> {
    sections: SmallVec<'m, Section, 16>,
    permissions: SmallVec<'m, Permission, 8>,
    addresses: SmallVec<'m, usize, 64>,
    slots: SmallVec<'m, usize, 64>,
    local_count: usize,
    size: usize,
    code_end: usize,
    page: usize,
    eh_frame: SecRef,
    architecture: Architecture,
    resolved: bool,
}

/// The image's tables live in the mapper's context and go back to it with the image (`Drop`): a context that must be
/// empty when its holder drops it (a code-cache entry's root, an mcx `SendRoot`) frees to zero.
pub struct MapperImage<'m> {
    state: PgBox<'m, State<'m>>,
}
impl Drop for MapperImage<'_> {
    fn drop(&mut self) {
        let s = &mut *self.state;
        s.sections.release();
        s.permissions.release();
        s.addresses.release();
        s.slots.release();
    }
}
impl<'m> MapperImage<'m> {
    pub fn new(mcx: Mcx<'m>) -> PgResult<Self> {
        let state = State {
            sections: SmallVec::new(mcx),
            permissions: SmallVec::new(mcx),
            addresses: SmallVec::new(mcx),
            slots: SmallVec::new(mcx),
            local_count: 0,
            size: 0,
            code_end: 0,
            page: 0,
            eh_frame: SecRef::default(),
            architecture: Architecture::Aarch64,
            resolved: false,
        };
        Ok(Self {
            state: mcx::alloc_in(mcx, state)?,
        })
    }
    pub fn reset(&mut self) {
        let s = &mut *self.state;
        s.sections.clear();
        s.permissions.clear();
        s.addresses.clear();
        s.slots.clear();
        s.local_count = 0;
        s.size = 0;
        s.code_end = 0;
        s.page = 0;
        s.eh_frame = SecRef::default();
        s.resolved = false;
    }
    pub fn size(&self) -> usize {
        self.state.size
    }
    /// End of the executable bytes of the layout: the PLT slots and every
    /// executable section, which precede all other sections and start at
    /// offset 0. Only these bytes need instruction-cache publication.
    pub fn code_end(&self) -> usize {
        self.state.code_end
    }
    pub fn permissions(&self) -> &[Permission] {
        &self.state.permissions
    }
    pub fn architecture(&self) -> Architecture {
        self.state.architecture
    }
    pub fn eh_frame_section(&self) -> SecRef {
        self.state.eh_frame
    }
    pub fn symbol_address(&self, sym: SymRef) -> Option<usize> {
        if !self.state.resolved {
            return None;
        }
        let idx = symbol_index(sym, self.state.local_count)?;
        self.state.addresses.get(idx).copied().filter(|&v| v != 0)
    }
    pub fn layout(&mut self, assembler: &mut AssemblerElf<'_>, page: usize) -> PgResult<usize> {
        self.reset();
        ensure(usize::BITS == 64, "ELF mapper requires 64-bit addresses")?;
        ensure(page.is_power_of_two(), "invalid mapping page size")?;
        let arch = match assembler.target().elf_machine {
            EM_AARCH64 => Architecture::Aarch64,
            EM_X86_64 => Architecture::X86_64,
            _ => return Err(invalid("unsupported ELF architecture")),
        };
        // Create the terminator-bearing section before collecting the allocation roster.
        let eh_frame = assembler.get_default_section(SectionKind::EHFrame)?;
        let frame = assembler.get_section(eh_frame)?;
        ensure(
            frame.flags & SHF_ALLOC != 0 && frame.kind != SHT_NOBITS && !frame.is_virtual,
            "EH frame requires allocated initialized storage",
        )?;
        let s = &mut *self.state;
        s.architecture = arch;
        s.page = page;
        s.eh_frame = eh_frame;
        s.local_count = assembler.local_symbol_count();
        let count = s
            .local_count
            .checked_add(assembler.global_symbols().len())
            .ok_or_else(|| invalid("symbol count overflow"))?;
        s.addresses.resize(count, 0)?;
        s.slots.resize(count, 0)?;
        for sec in assembler.data_sections() {
            if sec.flags & SHF_ALLOC == 0 {
                continue;
            }
            let key = u32::from(sec.flags & SHF_EXECINSTR == 0) * 4
                + u32::from(sec.flags & SHF_WRITE != 0) * 2
                + u32::from(sec.kind == SHT_NOBITS);
            s.sections.push(Section {
                reference: sec.get_ref(),
                key,
            })?;
        }
        s.sections
            .sort_unstable_by_key(|v| (v.key, v.reference.id()));
        let mut offset = count
            .checked_mul(16)
            .ok_or_else(|| invalid("PLT size overflow"))?;
        s.code_end = offset;
        let mut previous = 0;
        if count != 0 {
            previous = SHF_ALLOC | SHF_EXECINSTR;
            s.permissions.push(Permission {
                offset: 0,
                flags: previous,
            })?;
        }
        for entry in &*s.sections {
            let sec = assembler.get_section_mut(entry.reference)?;
            ensure(sec.align.is_power_of_two(), "invalid section alignment")?;
            if previous != sec.flags {
                offset = align_up(offset, page)?;
                s.permissions.push(Permission {
                    offset,
                    flags: sec.flags,
                })?;
                previous = sec.flags;
            } else {
                offset = align_up(offset, sec.align as usize)?;
            }
            sec.addr = offset as u64;
            let mut size =
                usize::try_from(sec.size()).map_err(|_| invalid("section size overflow"))?;
            if entry.reference == eh_frame {
                size = size
                    .checked_add(4)
                    .ok_or_else(|| invalid("EH frame size overflow"))?;
            }
            offset = offset
                .checked_add(size)
                .ok_or_else(|| invalid("mapped image size overflow"))?;
            ensure(
                offset <= isize::MAX as usize,
                "mapped image exceeds pointer range",
            )?;
            if sec.flags & SHF_EXECINSTR != 0 {
                s.code_end = offset;
            }
        }
        s.permissions.push(Permission { offset, flags: 0 })?;
        s.size = offset;
        Ok(offset)
    }
    fn admit_buffer(&self, length: usize, base: usize) -> PgResult<()> {
        ensure(
            length >= self.size() && self.size() != 0,
            "mapping layout missing or buffer too small",
        )?;
        ensure(
            base != 0 && self.state.page != 0 && base % self.state.page == 0,
            "mapping base must be nonzero and page aligned",
        )?;
        ensure(
            base.checked_add(self.size()).is_some(),
            "mapping address overflow",
        )?;
        Ok(())
    }
    pub fn relocate(
        &mut self,
        assembler: &AssemblerElf<'_>,
        image: &mut [u8],
        base: usize,
        resolver: impl FnMut(&[u8]) -> usize,
    ) -> PgResult<bool> {
        self.state.resolved = false;
        self.admit_buffer(image.len(), base)?;
        image[..self.size()].fill(0);
        self.relocate_zeroed(assembler, image, base, resolver)
    }
    pub(crate) fn relocate_zeroed(
        &mut self,
        assembler: &AssemblerElf<'_>,
        image: &mut [u8],
        base: usize,
        mut resolver: impl FnMut(&[u8]) -> usize,
    ) -> PgResult<bool> {
        self.state.resolved = false;
        self.admit_buffer(image.len(), base)?;
        let state = &mut *self.state;
        ensure(
            assembler.local_symbol_count() == state.local_count
                && assembler.global_symbols().len() + state.local_count == state.addresses.len(),
            "symbol table changed after layout",
        )?;
        state.addresses.fill(0);
        state.slots.fill(0);
        // The symbol and slot tables are resolved to slices once for the whole
        // relocation walk (upstream indexes `sym_addrs`/`got_plt_slots` directly).
        let s = &mut Fixups {
            addresses: &mut state.addresses,
            slots: &mut state.slots,
            local_count: state.local_count,
            size: state.size,
            architecture: state.architecture,
        };
        let mut success = true;
        let mut next_slot = 0;
        for (idx, sym) in assembler.global_symbols().iter().enumerate() {
            if sym.st_shndx != SHN_UNDEF
                && (sym.st_shndx < SHN_LORESERVE || sym.st_shndx == SHN_XINDEX)
            {
                resolve_symbol(
                    s,
                    assembler,
                    SymRef::new(0x8000_0000 | idx as u32),
                    base,
                    &mut resolver,
                    &mut success,
                )?;
            }
        }
        for entry in &*state.sections {
            let section = assembler.get_section(entry.reference)?;
            let start =
                usize::try_from(section.addr).map_err(|_| invalid("section address overflow"))?;
            let size =
                usize::try_from(section.size()).map_err(|_| invalid("section size overflow"))?;
            let end = start
                .checked_add(size)
                .ok_or_else(|| invalid("section range overflow"))?;
            ensure(end <= s.size, "section changed after layout")?;
            if section.kind != SHT_NOBITS {
                ensure(
                    !section.is_virtual && section.data.len() == size,
                    "invalid initialized section storage",
                )?;
                image[start..end].copy_from_slice(&section.data);
            }
            for reloc in section.relocations() {
                let symbol = resolve_symbol(
                    s,
                    assembler,
                    reloc.symbol,
                    base,
                    &mut resolver,
                    &mut success,
                )?;
                let symbol_idx = symbol_index(reloc.symbol, s.local_count)
                    .ok_or_else(|| invalid("symbol index overflow"))?;
                let relative = reloc.offset as usize;
                let width = if reloc.kind
                    == match s.architecture {
                        Architecture::Aarch64 => R_AARCH64_ABS64,
                        Architecture::X86_64 => R_X86_64_64,
                    } {
                    8
                } else {
                    4
                };
                if relative.checked_add(width).is_none_or(|v| v > size) {
                    success = false;
                    continue;
                }
                let at = start + relative;
                if !apply_relocation(
                    s,
                    image,
                    base,
                    at,
                    reloc,
                    symbol_idx,
                    symbol,
                    &mut next_slot,
                )? {
                    success = false;
                }
            }
        }
        if !success {
            state.addresses.fill(0);
        }
        state.resolved = success;
        Ok(success)
    }
}
/// The relocation walk's view of the image state: the symbol-address and
/// PLT-slot tables as slices plus the layout constants.
struct Fixups<'a> {
    addresses: &'a mut [usize],
    slots: &'a mut [usize],
    local_count: usize,
    size: usize,
    architecture: Architecture,
}
fn align_up(value: usize, align: usize) -> PgResult<usize> {
    value
        .checked_add(align - 1)
        .map(|v| v & !(align - 1))
        .ok_or_else(|| invalid("mapping alignment overflow"))
}
fn symbol_index(sym: SymRef, locals: usize) -> Option<usize> {
    let index = AssemblerElf::sym_index(sym);
    if AssemblerElf::is_local(sym) {
        (index < locals).then_some(index)
    } else {
        index.checked_add(locals)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SymbolSection {
    Undefined,
    Absolute,
    Defined,
    Unsupported,
}
pub(crate) fn symbol_section(symbol: &Elf64_Sym) -> SymbolSection {
    match symbol.st_shndx {
        SHN_UNDEF => SymbolSection::Undefined,
        SHN_ABS => SymbolSection::Absolute,
        value if value < SHN_LORESERVE || value == SHN_XINDEX => SymbolSection::Defined,
        _ => SymbolSection::Unsupported,
    }
}
fn resolve_symbol(
    state: &mut Fixups<'_>,
    assembler: &AssemblerElf<'_>,
    reference: SymRef,
    base: usize,
    resolver: &mut impl FnMut(&[u8]) -> usize,
    success: &mut bool,
) -> PgResult<usize> {
    let idx = symbol_index(reference, state.local_count)
        .ok_or_else(|| invalid("invalid local symbol index"))?;
    let cached = *state
        .addresses
        .get(idx)
        .ok_or_else(|| invalid("invalid global symbol index"))?;
    if cached != 0 {
        return Ok(cached);
    }
    let symbol = assembler.symbol(reference)?;
    let address = match symbol_section(symbol) {
        SymbolSection::Undefined => {
            let address = resolver(assembler.sym_name(reference)?);
            if address == 0 && symbol.st_bind() == STB_GLOBAL {
                *success = false;
            }
            address
        }
        SymbolSection::Absolute => symbol.st_value as usize,
        SymbolSection::Defined => {
            let sec = assembler.get_section(assembler.sym_section(reference)?)?;
            if sec.flags & SHF_ALLOC == 0 || symbol.st_value > sec.size() {
                *success = false;
                0
            } else {
                let offset = sec
                    .addr
                    .checked_add(symbol.st_value)
                    .ok_or_else(|| invalid("symbol offset overflow"))?;
                if offset > state.size as u64 {
                    *success = false;
                    0
                } else {
                    base + offset as usize
                }
            }
        }
        SymbolSection::Unsupported => {
            *success = false;
            0
        }
    };
    state.addresses[idx] = address;
    Ok(address)
}
fn sign_fits(value: usize, width: u32) -> bool {
    ((value << (usize::BITS - width)) as isize >> (usize::BITS - width)) == value as isize
}
fn blend(image: &mut [u8], offset: usize, mask: u32, data: usize) {
    let old = u32::from_le_bytes(image[offset..offset + 4].try_into().unwrap());
    image[offset..offset + 4]
        .copy_from_slice(&((data as u32 & mask) | (old & !mask)).to_le_bytes());
}
fn plt_entry(
    state: &mut Fixups<'_>,
    image: &mut [u8],
    base: usize,
    idx: usize,
    address: usize,
    next: &mut usize,
) -> PgResult<usize> {
    if state.slots[idx] != 0 {
        return Ok(state.slots[idx]);
    }
    let offset = *next;
    let end = offset
        .checked_add(16)
        .ok_or_else(|| invalid("PLT offset overflow"))?;
    ensure(
        end <= state.addresses.len() * 16 && end <= image.len(),
        "insufficient PLT slots",
    )?;
    match state.architecture {
        Architecture::Aarch64 => {
            let reg = tpde_disarm::GReg::new(16).unwrap();
            let load = tpde_disarm::de64_LDRx_pcrel(reg.into(), 2);
            let branch = tpde_disarm::de64_BR(reg.into());
            image[offset..offset + 4].copy_from_slice(&load.to_le_bytes());
            image[offset + 4..offset + 8].copy_from_slice(&branch.to_le_bytes());
        }
        Architecture::X86_64 => {
            use tpde_fadec::{fe64_JMPm, fe64_UD2, Output, FE_IP, FE_MEM, FE_NOREG};
            let mut out = Output::new(&mut image[offset..offset + 8], base + offset);
            let mem =
                FE_MEM(FE_IP, 0, FE_NOREG, 8).map_err(|_| invalid("invalid PLT memory operand"))?;
            fe64_JMPm(&mut out, 0, mem).map_err(|_| invalid("PLT jump encoding failed"))?;
            fe64_UD2(&mut out, 0).map_err(|_| invalid("PLT trap encoding failed"))?;
            ensure(out.len() == 8, "unexpected PLT encoding length")?;
        }
    }
    image[offset + 8..end].copy_from_slice(&(address as u64).to_le_bytes());
    let result = base + offset;
    state.slots[idx] = result;
    *next = end;
    Ok(result)
}
fn apply_relocation(
    state: &mut Fixups<'_>,
    image: &mut [u8],
    base: usize,
    at: usize,
    reloc: &Relocation,
    idx: usize,
    symbol: usize,
    next: &mut usize,
) -> PgResult<bool> {
    let pc = base + at;
    let addend = reloc.addend as isize as usize;
    let syma = symbol.wrapping_add(addend);
    let mut value = syma.wrapping_sub(pc);
    let mut success = true;
    match state.architecture {
        Architecture::X86_64 => match reloc.kind {
            R_X86_64_64 => image[at..at + 8].copy_from_slice(&(syma as u64).to_le_bytes()),
            R_X86_64_PC32 | R_X86_64_PLT32 | R_X86_64_GOTPCREL => {
                if reloc.kind == R_X86_64_PLT32 && !sign_fits(value, 32) {
                    value = plt_entry(state, image, base, idx, symbol, next)?
                        .wrapping_add(addend)
                        .wrapping_sub(pc);
                } else if reloc.kind == R_X86_64_GOTPCREL {
                    value = plt_entry(state, image, base, idx, symbol, next)?
                        .wrapping_add(8)
                        .wrapping_add(addend)
                        .wrapping_sub(pc);
                }
                success = sign_fits(value, 32);
                image[at..at + 4].copy_from_slice(&(value as u32).to_le_bytes());
            }
            _ => success = false,
        },
        Architecture::Aarch64 => match reloc.kind {
            R_AARCH64_ABS64 => image[at..at + 8].copy_from_slice(&(syma as u64).to_le_bytes()),
            R_AARCH64_PREL32 => {
                success = sign_fits(value, 32);
                image[at..at + 4].copy_from_slice(&(value as u32).to_le_bytes());
            }
            R_AARCH64_CALL26 => {
                if value & 3 != 0 || !sign_fits(value, 28) {
                    value = plt_entry(state, image, base, idx, symbol, next)?
                        .wrapping_add(addend)
                        .wrapping_sub(pc);
                }
                // The replacement branch must satisfy CALL26, including alignment.
                success = value & 3 == 0 && sign_fits(value, 28);
                blend(image, at, 0x03ff_ffff, value >> 2);
            }
            R_AARCH64_ADR_PREL_PG_HI21 | R_AARCH64_ADR_GOT_PAGE => {
                let target = if reloc.kind == R_AARCH64_ADR_GOT_PAGE {
                    plt_entry(state, image, base, idx, symbol, next)?.wrapping_add(8)
                } else {
                    syma
                };
                value = (target & !0xfff).wrapping_sub(pc & !0xfff);
                success = sign_fits(value, 33);
                value >>= 12;
                blend(
                    image,
                    at,
                    0x60ff_ffe0,
                    ((value & 3) << 29) | (((value >> 2) & 0x7ffff) << 5),
                );
            }
            R_AARCH64_ADD_ABS_LO12_NC => blend(image, at, 0xfff << 10, syma << 10),
            R_AARCH64_LDST128_ABS_LO12_NC => {
                // The compiler emits this relocation; the upstream mapper omits it.
                success = syma & 15 == 0;
                if success {
                    blend(image, at, 0xfff << 10, ((syma & 0xfff) >> 4) << 10);
                }
            }
            R_AARCH64_LDST64_ABS_LO12_NC => {
                // Generated encodegen consumers emit this relocation for their
                // eight-byte constant loads; the upstream mapper omits it too.
                success = syma & 7 == 0;
                if success {
                    blend(image, at, 0xfff << 10, ((syma & 0xfff) >> 3) << 10);
                }
            }
            R_AARCH64_LD64_GOT_LO12_NC => {
                value = plt_entry(state, image, base, idx, symbol, next)?.wrapping_add(8);
                blend(image, at, 0xfff << 10, (value & 0xfff) << 7);
            }
            _ => success = false,
        },
    }
    Ok(success)
}
const _: () = {
    assert!(!core::mem::needs_drop::<State<'static>>());
    assert!(core::mem::size_of::<MapperImage<'static>>() <= 128);
};

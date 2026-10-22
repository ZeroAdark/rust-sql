// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{elf::*, *};
use tpde_string_table::StringTable;

pub(crate) const SHSTRTAB: &[u8] = b"\0.note.GNU-stack\0.rela.eh_frame\0.symtab\0.strtab\0.shstrtab\0.bss\0.tbss\0.rela.rodata\0.rela.text\0.rela.data.rel.ro\0.rela.data\0.rela.tdata\0.rela.gcc_except_table\0.rela.init_array\0.rela.fini_array\0.group\0.symtab_shndx\0\0";
pub(crate) const PREDEFINED: usize = 5;
pub(crate) const fn sec_off(name: &[u8]) -> u32 {
    let mut i = 1;
    while i + name.len() < SHSTRTAB.len() {
        let mut j = 0;
        while j < name.len() && SHSTRTAB[i + j] == name[j] {
            j += 1;
        }
        if j == name.len() && SHSTRTAB[i + j] == 0 {
            return i as u32;
        }
        i += 1;
    }
    panic!("unknown built-in section name")
}
const fn section_flags() -> [SectionFlags; 9] {
    let base = SectionFlags {
        kind: SHT_PROGBITS,
        flags: SHF_ALLOC,
        name: 0,
        align: 1,
        has_relocs: true,
        is_bss: false,
    };
    [
        SectionFlags {
            flags: SHF_ALLOC | SHF_EXECINSTR,
            name: sec_off(b".text"),
            align: 16,
            ..base
        },
        SectionFlags {
            name: sec_off(b".rodata"),
            has_relocs: false,
            ..base
        },
        SectionFlags {
            name: sec_off(b".eh_frame"),
            align: 8,
            ..base
        },
        SectionFlags {
            name: sec_off(b".gcc_except_table"),
            align: 8,
            ..base
        },
        SectionFlags {
            flags: SHF_ALLOC | SHF_WRITE,
            name: sec_off(b".data"),
            ..base
        },
        SectionFlags {
            flags: SHF_ALLOC | SHF_WRITE,
            name: sec_off(b".data.rel.ro"),
            ..base
        },
        SectionFlags {
            kind: SHT_NOBITS,
            flags: SHF_ALLOC | SHF_WRITE,
            name: sec_off(b".bss"),
            has_relocs: false,
            is_bss: true,
            ..base
        },
        SectionFlags {
            flags: SHF_ALLOC | SHF_WRITE | SHF_TLS,
            name: sec_off(b".tdata"),
            ..base
        },
        SectionFlags {
            kind: SHT_NOBITS,
            flags: SHF_ALLOC | SHF_WRITE | SHF_TLS,
            name: sec_off(b".tbss"),
            has_relocs: false,
            is_bss: true,
            ..base
        },
    ]
}
static A64: TargetInfo = TargetInfo {
    reloc_pc32: R_AARCH64_PREL32,
    reloc_abs64: R_AARCH64_ABS64,
    section_flags: section_flags(),
    elf_osabi: ELFOSABI_SYSV,
    elf_machine: EM_AARCH64,
};
static X64: TargetInfo = TargetInfo {
    reloc_pc32: R_X86_64_PC32,
    reloc_abs64: R_X86_64_64,
    section_flags: section_flags(),
    elf_osabi: ELFOSABI_SYSV,
    elf_machine: EM_X86_64,
};

pub(crate) struct State<'m> {
    pub mcx: Mcx<'m>,
    pub target: &'m TargetInfo,
    pub sections: SmallVec<'m, Option<&'m mut DataSection<'m>>, 16>,
    defaults: [SecRef; 9],
    pub globals: SmallVec<'m, Elf64_Sym, 0>,
    pub locals: SmallVec<'m, Elf64_Sym, 0>,
    pub global_shndx: SmallVec<'m, u32, 0>,
    pub local_shndx: SmallVec<'m, u32, 0>,
    pub strings: StringTable<'m>,
    pub extra_names: StringTable<'m>,
}
pub struct AssemblerElf<'m> {
    pub(crate) state: &'m mut State<'m>,
}
impl<'m> AssemblerElf<'m> {
    /// ```compile_fail
    /// let mut ctx = mcx::MemoryContext::new_bump("asm");
    /// let asm = tpde_assembler::AssemblerElf::new(ctx.mcx(), tpde_assembler::Architecture::Aarch64).unwrap();
    /// ctx.reset();
    /// let _ = asm.build_object_file();
    /// ```
    pub fn new(mcx: Mcx<'m>, architecture: Architecture) -> PgResult<Self> {
        Self::with_target(
            mcx,
            match architecture {
                Architecture::Aarch64 => &A64,
                Architecture::X86_64 => &X64,
            },
        )
    }
    pub fn with_target(mcx: Mcx<'m>, target: &'m TargetInfo) -> PgResult<Self> {
        let mut state = State {
            mcx,
            target,
            sections: SmallVec::new(mcx),
            defaults: [SecRef::default(); 9],
            globals: SmallVec::new(mcx),
            locals: SmallVec::new(mcx),
            global_shndx: SmallVec::new(mcx),
            local_shndx: SmallVec::new(mcx),
            strings: StringTable::new(mcx),
            extra_names: StringTable::new(mcx),
        };
        state.locals.push(Elf64_Sym::default())?;
        for _ in 0..PREDEFINED {
            state.sections.push(None)?;
        }
        Ok(Self {
            state: mcx::leak_in(mcx::alloc_in(mcx, state)?),
        })
    }
    pub fn target(&self) -> &TargetInfo {
        self.state.target
    }
    pub fn section_count(&self) -> usize {
        self.state.sections.len()
    }
    /// ```compile_fail,E0502
    /// fn rejected(asm: &mut tpde_assembler::AssemblerElf<'_>) {
    ///     let mut sections = asm.data_sections();
    ///     asm.reset().unwrap();
    ///     let _ = sections.next();
    /// }
    /// ```
    pub fn data_sections<'a>(&'a self) -> impl Iterator<Item = &'a DataSection<'m>> + 'a {
        self.state.sections.iter().filter_map(Option::as_deref)
    }
    pub fn local_symbol_count(&self) -> usize {
        self.state.locals.len()
    }
    /// ```compile_fail,E0502
    /// fn rejected(asm: &mut tpde_assembler::AssemblerElf<'_>) {
    ///     let globals = asm.global_symbols();
    ///     asm.reset().unwrap();
    ///     assert!(globals.is_empty());
    /// }
    /// ```
    pub fn global_symbols(&self) -> &[Elf64_Sym] {
        &self.state.globals
    }
    pub fn reset(&mut self) -> PgResult<()> {
        let s = &mut self.state;
        s.sections.clear();
        s.defaults.fill(SecRef::default());
        s.globals.clear();
        s.locals.truncate(1);
        s.locals[0] = Elf64_Sym::default();
        // Upstream retains stale extended-index rows, breaking reuse after a large object.
        s.local_shndx.clear();
        s.global_shndx.clear();
        s.strings = StringTable::new(s.mcx);
        s.extra_names = StringTable::new(s.mcx);
        for _ in 0..PREDEFINED {
            s.sections.push(None)?;
        }
        Ok(())
    }
    pub fn get_section(&self, reference: SecRef) -> PgResult<&DataSection<'m>> {
        self.state
            .sections
            .get(reference.id() as usize)
            .and_then(Option::as_deref)
            .ok_or_else(|| invalid("invalid or reserved section reference"))
    }
    /// ```compile_fail
    /// use tpde_assembler::{AssemblerElf, Architecture, SectionKind};
    /// let ctx = mcx::MemoryContext::new_bump("asm");
    /// let mut asm = AssemblerElf::new(ctx.mcx(), Architecture::Aarch64).unwrap();
    /// let id = asm.create_section(SectionKind::Text).unwrap();
    /// let section = asm.get_section_mut(id).unwrap();
    /// asm.reset().unwrap();
    /// section.write(&42u32).unwrap();
    /// ```
    #[inline(always)]
    pub fn get_section_mut(&mut self, reference: SecRef) -> PgResult<&mut DataSection<'m>> {
        self.state
            .sections
            .get_mut(reference.id() as usize)
            .and_then(Option::as_deref_mut)
            .ok_or_else(|| invalid("invalid or reserved section reference"))
    }
    fn prepare_section(&mut self, flags: SectionFlags) -> PgResult<&'m mut DataSection<'m>> {
        ensure(flags.align.is_power_of_two(), "invalid section alignment")?;
        let end = self
            .section_count()
            .checked_add(if flags.has_relocs { 2 } else { 1 })
            .ok_or_else(|| invalid("section count overflow"))?;
        u32_size(end)?;
        self.state.sections.reserve(end)?;
        let section = DataSection::new(
            self.state.mcx,
            SecRef::new(self.section_count() as u32),
            flags,
        );
        Ok(mcx::leak_in(mcx::alloc_in(self.state.mcx, section)?))
    }
    fn commit_section(&mut self, section: &'m mut DataSection<'m>) -> PgResult<SecRef> {
        let reference = section.get_ref();
        let relocs = section.has_relocs;
        self.state.sections.push(Some(section))?;
        if relocs {
            self.state.sections.push(None)?;
        }
        Ok(reference)
    }
    pub fn create_section_with_flags(&mut self, flags: SectionFlags) -> PgResult<SecRef> {
        let section = self.prepare_section(flags)?;
        self.commit_section(section)
    }
    pub fn create_section(&mut self, kind: SectionKind) -> PgResult<SecRef> {
        self.create_section_with_flags(self.state.target.section_flags[kind as usize])
    }
    pub fn get_default_section(&mut self, kind: SectionKind) -> PgResult<SecRef> {
        let index = kind as usize;
        if !self.state.defaults[index].valid() {
            self.state.defaults[index] = self.create_section(kind)?;
        }
        Ok(self.state.defaults[index])
    }
    pub fn rename_section(&mut self, reference: SecRef, name: &[u8]) -> PgResult<()> {
        let sec = self.get_section(reference)?;
        ensure(!sec.sym.valid(), "cannot rename a section with a symbol")?;
        let relocs = sec.has_relocs;
        let upper = SHSTRTAB
            .len()
            .checked_add(self.state.extra_names.size())
            .and_then(|v| v.checked_add(5))
            .and_then(|v| v.checked_add(name.len()))
            .ok_or_else(|| invalid("section name overflow"))?;
        u32_size(upper)?;
        let offset = self
            .state
            .extra_names
            .add_prefix(if relocs { b".rela" } else { b"" }, name)?;
        self.get_section_mut(reference)?.name =
            (SHSTRTAB.len() + offset + if relocs { 5 } else { 0 }) as u32;
        Ok(())
    }
    pub fn sec_name(&self, reference: SecRef) -> PgResult<&[u8]> {
        let offset = self.get_section(reference)?.name as usize;
        if offset < SHSTRTAB.len() {
            c_string(SHSTRTAB, offset)
        } else {
            c_string(self.state.extra_names.data(), offset - SHSTRTAB.len())
        }
    }
    pub fn create_structor_section(&mut self, init: bool, group: SecRef) -> PgResult<SecRef> {
        if group.valid() {
            self.reserve_group(group, 8)?;
        }
        let section = self.prepare_section(SectionFlags {
            kind: if init { SHT_INIT_ARRAY } else { SHT_FINI_ARRAY },
            flags: SHF_ALLOC | SHF_WRITE,
            align: 8,
            ..SectionFlags::default()
        })?;
        let name: &[u8] = if init { b".init_array" } else { b".fini_array" };
        let upper = self
            .state
            .extra_names
            .size()
            .checked_add(SHSTRTAB.len() + 5 + name.len())
            .ok_or_else(|| invalid("section name overflow"))?;
        u32_size(upper)?;
        let offset = self.state.extra_names.add_prefix(b".rela", name)?;
        section.name = (SHSTRTAB.len() + offset + 5) as u32;
        let reference = self.commit_section(section)?;
        if group.valid() {
            self.add_to_group(group, reference)?;
        }
        Ok(reference)
    }
    pub fn create_group_section(&mut self, signature: SymRef, comdat: bool) -> PgResult<SecRef> {
        self.symbol(signature)?;
        let section = self.prepare_section(SectionFlags {
            kind: SHT_GROUP,
            name: const { sec_off(b".group") },
            align: 4,
            has_relocs: false,
            ..SectionFlags::default()
        })?;
        section.sym = signature;
        section.write(&(if comdat { GRP_COMDAT } else { 0 }))?;
        self.commit_section(section)
    }
    fn reserve_group(&mut self, reference: SecRef, bytes: usize) -> PgResult<()> {
        let group = self.get_section_mut(reference)?;
        ensure(
            group.kind == SHT_GROUP && !group.is_virtual,
            "invalid section group",
        )?;
        let end = group
            .data
            .len()
            .checked_add(bytes)
            .ok_or_else(|| invalid("section group overflow"))?;
        group.data.reserve(end)
    }
    pub fn add_to_group(&mut self, group: SecRef, reference: SecRef) -> PgResult<()> {
        ensure(group != reference, "group cannot contain itself")?;
        let sec = self.get_section(reference)?;
        ensure(
            sec.flags & SHF_GROUP == 0,
            "section already belongs to a group",
        )?;
        let relocs = sec.has_relocs;
        self.reserve_group(group, if relocs { 8 } else { 4 })?;
        self.get_section_mut(group)?.write(&reference.id())?;
        if relocs {
            self.get_section_mut(group)?.write(&(reference.id() + 1))?;
        }
        self.get_section_mut(reference)?.flags |= SHF_GROUP;
        Ok(())
    }
    pub fn is_local(sym: SymRef) -> bool {
        sym.id() & 0x8000_0000 == 0
    }
    pub fn sym_index(sym: SymRef) -> usize {
        (sym.id() & 0x7fff_ffff) as usize
    }
    pub fn symbol(&self, sym: SymRef) -> PgResult<&Elf64_Sym> {
        ensure(sym.valid(), "invalid symbol reference")?;
        let symbols = if Self::is_local(sym) {
            &self.state.locals
        } else {
            &self.state.globals
        };
        symbols
            .get(Self::sym_index(sym))
            .ok_or_else(|| invalid("symbol reference out of range"))
    }
    fn symbol_mut(&mut self, sym: SymRef) -> PgResult<&mut Elf64_Sym> {
        ensure(sym.valid(), "invalid symbol reference")?;
        let symbols = if Self::is_local(sym) {
            &mut self.state.locals
        } else {
            &mut self.state.globals
        };
        symbols
            .get_mut(Self::sym_index(sym))
            .ok_or_else(|| invalid("symbol reference out of range"))
    }
    fn reserve_xindex(&mut self, sym: SymRef, sec: SecRef) -> PgResult<()> {
        if sec.id() >= SHN_LORESERVE as u32 {
            let tab = if Self::is_local(sym) {
                &mut self.state.local_shndx
            } else {
                &mut self.state.global_shndx
            };
            tab.reserve(Self::sym_index(sym) + 1)?;
        }
        Ok(())
    }
    fn set_xindex(&mut self, sym: SymRef, sec: SecRef) -> PgResult<()> {
        let tab = if Self::is_local(sym) {
            &mut self.state.local_shndx
        } else {
            &mut self.state.global_shndx
        };
        if tab.len() <= Self::sym_index(sym) {
            tab.resize(Self::sym_index(sym) + 1, 0)?;
        }
        tab[Self::sym_index(sym)] = sec.id();
        Ok(())
    }
    pub fn section_symbol(&mut self, reference: SecRef) -> PgResult<SymRef> {
        let old = self.get_section(reference)?.sym;
        if old.valid() {
            return Ok(old);
        }
        ensure(
            self.state.locals.len() < 0x7fff_ffff,
            "too many local symbols",
        )?;
        let sym = SymRef::new(self.state.locals.len() as u32);
        self.state.locals.reserve(self.state.locals.len() + 1)?;
        self.reserve_xindex(sym, reference)?;
        let extended = reference.id() >= SHN_LORESERVE as u32;
        self.state.locals.push(Elf64_Sym {
            st_info: st_info(STB_LOCAL as u32, STT_SECTION as u32) as u8,
            st_shndx: if extended {
                SHN_XINDEX
            } else {
                reference.id() as u16
            },
            ..Elf64_Sym::default()
        })?;
        if extended {
            self.set_xindex(sym, reference)?;
        }
        self.get_section_mut(reference)?.sym = sym;
        Ok(sym)
    }
    fn sym_add(&mut self, name: &[u8], binding: SymBinding, kind: u8) -> PgResult<SymRef> {
        let symbols = if binding == SymBinding::Local {
            &mut self.state.locals
        } else {
            &mut self.state.globals
        };
        ensure(symbols.len() < 0x7fff_ffff, "too many symbols")?;
        symbols.reserve(symbols.len() + 1)?;
        let index = symbols.len() as u32;
        let end = self
            .state
            .strings
            .size()
            .checked_add(name.len())
            .ok_or_else(|| invalid("symbol name overflow"))?;
        u32_size(end)?;
        let offset = self.state.strings.add(name)?;
        let bind = match binding {
            SymBinding::Local => STB_LOCAL,
            SymBinding::Weak => STB_WEAK,
            SymBinding::Global => STB_GLOBAL,
        };
        let symbol = Elf64_Sym {
            st_name: offset as u32,
            st_info: st_info(bind as u32, kind as u32) as u8,
            ..Elf64_Sym::default()
        };
        if binding == SymBinding::Local {
            self.state.locals.push(symbol)?;
            Ok(SymRef::new(index))
        } else {
            self.state.globals.push(symbol)?;
            Ok(SymRef::new(index | 0x8000_0000))
        }
    }
    pub fn sym_add_undef(&mut self, name: &[u8], binding: SymBinding) -> PgResult<SymRef> {
        self.sym_add(name, binding, STT_NOTYPE)
    }
    pub fn sym_predef_func(&mut self, name: &[u8], binding: SymBinding) -> PgResult<SymRef> {
        self.sym_add(name, binding, STT_FUNC)
    }
    pub fn sym_predef_data(&mut self, name: &[u8], binding: SymBinding) -> PgResult<SymRef> {
        self.sym_add(name, binding, STT_OBJECT)
    }
    pub fn sym_predef_tls(&mut self, name: &[u8], binding: SymBinding) -> PgResult<SymRef> {
        self.sym_add(name, binding, STT_TLS)
    }
    pub fn sym_name(&self, sym: SymRef) -> PgResult<&[u8]> {
        c_string(
            self.state.strings.data(),
            self.symbol(sym)?.st_name as usize,
        )
    }
    pub fn sym_section(&self, sym: SymRef) -> PgResult<SecRef> {
        let index = self.symbol(sym)?.st_shndx;
        if index > SHN_UNDEF && index < SHN_LORESERVE {
            return Ok(SecRef::new(index as u32));
        }
        ensure(index == SHN_XINDEX, "symbol has no defined section")?;
        let tab = if Self::is_local(sym) {
            &self.state.local_shndx
        } else {
            &self.state.global_shndx
        };
        tab.get(Self::sym_index(sym))
            .copied()
            .map(SecRef::new)
            .ok_or_else(|| invalid("missing extended symbol section"))
    }
    pub fn sym_def(&mut self, sym: SymRef, sec: SecRef, pos: u64, size: u64) -> PgResult<()> {
        self.get_section(sec)?;
        ensure(
            self.symbol(sym)?.st_shndx == SHN_UNDEF,
            "cannot redefine symbol",
        )?;
        self.reserve_xindex(sym, sec)?;
        let extended = sec.id() >= SHN_LORESERVE as u32;
        if extended {
            self.set_xindex(sym, sec)?;
        }
        let symbol = self.symbol_mut(sym)?;
        symbol.st_value = pos;
        symbol.st_size = size;
        symbol.st_shndx = if extended {
            SHN_XINDEX
        } else {
            sec.id() as u16
        };
        Ok(())
    }
    pub fn sym_def_absolute(&mut self, sym: SymRef, value: u64, size: u64) -> PgResult<()> {
        let symbol = self.symbol_mut(sym)?;
        ensure(symbol.st_shndx == SHN_UNDEF, "cannot redefine symbol")?;
        symbol.st_value = value;
        symbol.st_size = size;
        symbol.st_shndx = SHN_ABS;
        Ok(())
    }
    pub fn sym_set_visibility(&mut self, sym: SymRef, visibility: SymVisibility) -> PgResult<()> {
        self.symbol_mut(sym)?.st_other = visibility as u8;
        Ok(())
    }
    pub fn sym_copy(&mut self, dst: SymRef, src: SymRef) -> PgResult<()> {
        let source = *self.symbol(src)?;
        self.symbol(dst)?;
        if source.st_shndx == SHN_XINDEX {
            let section = self.sym_section(src)?;
            self.reserve_xindex(dst, section)?;
            self.set_xindex(dst, section)?;
        }
        let symbol = self.symbol_mut(dst)?;
        symbol.st_shndx = source.st_shndx;
        symbol.st_size = source.st_size;
        symbol.st_value = source.st_value;
        Ok(())
    }
    /// Move the section's bytes into `into` and lock the section (upstream's
    /// `locked` window) until `unlock_section_data` restores them.
    pub fn lock_section_data(
        &mut self,
        sec: SecRef,
        into: &mut SmallVec<'m, u8, 256>,
    ) -> PgResult<()> {
        let section = self.get_section_mut(sec)?;
        ensure(!section.is_virtual, "cannot lock a virtual section")?;
        ensure(!section.locked, "section data is already locked")?;
        into.move_assign(&mut section.data)?;
        section.locked = true;
        Ok(())
    }
    /// Return the bytes taken by `lock_section_data` and unlock the section.
    pub fn unlock_section_data(
        &mut self,
        sec: SecRef,
        from: &mut SmallVec<'m, u8, 256>,
    ) -> PgResult<()> {
        let section = self.get_section_mut(sec)?;
        ensure(section.locked, "section data is not locked")?;
        section.data.move_assign(from)?;
        section.locked = false;
        Ok(())
    }
    fn data_range(&self, sec: SecRef, size: u64, align: u32) -> PgResult<(usize, usize)> {
        ensure(
            !self.get_section(sec)?.locked,
            "section data is locked by a function writer",
        )?;
        let old = usize::try_from(self.get_section(sec)?.size())
            .map_err(|_| invalid("section size overflow"))?;
        let pos = align_up(old, align)?;
        let end = pos
            .checked_add(usize::try_from(size).map_err(|_| invalid("data size overflow"))?)
            .ok_or_else(|| invalid("data range overflow"))?;
        u32_size(pos)?;
        u32_size(end)?;
        Ok((pos, end))
    }
    pub fn sym_def_predef_data(
        &mut self,
        sec: SecRef,
        sym: SymRef,
        size: u64,
        align: u32,
    ) -> PgResult<u32> {
        ensure(
            !self.get_section(sec)?.is_virtual,
            "cannot add data to virtual section",
        )?;
        ensure(
            self.symbol(sym)?.st_shndx == SHN_UNDEF,
            "cannot redefine symbol",
        )?;
        let (pos, end) = self.data_range(sec, size, align)?;
        self.get_section_mut(sec)?.data.reserve(end)?;
        self.reserve_xindex(sym, sec)?;
        self.sym_def(sym, sec, pos as u64, size)?;
        let section = self.get_section_mut(sec)?;
        section.data.resize_bytes(end)?;
        section.align = section.align.max(align);
        Ok(pos as u32)
    }
    pub fn sym_def_predef_bytes(
        &mut self,
        sec: SecRef,
        sym: SymRef,
        data: &[u8],
        align: u32,
    ) -> PgResult<u32> {
        ensure(
            !self.get_section(sec)?.is_virtual,
            "cannot add data to virtual section",
        )?;
        ensure(
            self.symbol(sym)?.st_shndx == SHN_UNDEF,
            "cannot redefine symbol",
        )?;
        let (pos, end) = self.data_range(sec, data.len() as u64, align)?;
        self.get_section_mut(sec)?.data.reserve(end)?;
        self.reserve_xindex(sym, sec)?;
        self.sym_def(sym, sec, pos as u64, data.len() as u64)?;
        let section = self.get_section_mut(sec)?;
        section.data.resize_bytes(pos)?;
        section.data.extend_from_slice(data)?;
        section.align = section.align.max(align);
        Ok(pos as u32)
    }
    pub fn sym_def_data(
        &mut self,
        sec: SecRef,
        name: &[u8],
        data: &[u8],
        align: u32,
        binding: SymBinding,
    ) -> PgResult<(SymRef, u32)> {
        ensure(
            !self.get_section(sec)?.is_virtual,
            "cannot add data to virtual section",
        )?;
        let (_, end) = self.data_range(sec, data.len() as u64, align)?;
        self.get_section_mut(sec)?.data.reserve(end)?;
        let idx = if binding == SymBinding::Local {
            self.state.locals.len() as u32
        } else {
            self.state.globals.len() as u32 | 0x8000_0000
        };
        self.reserve_xindex(SymRef::new(idx), sec)?;
        let sym = self.sym_predef_data(name, binding)?;
        let pos = self.sym_def_predef_bytes(sec, sym, data, align)?;
        Ok((sym, pos))
    }
    pub fn sym_def_predef_zero(
        &mut self,
        sec: SecRef,
        sym: SymRef,
        size: u32,
        align: u32,
    ) -> PgResult<u32> {
        ensure(
            self.symbol(sym)?.st_shndx == SHN_UNDEF,
            "cannot redefine symbol",
        )?;
        let (pos, end) = self.data_range(sec, size as u64, align)?;
        if !self.get_section(sec)?.is_virtual {
            self.get_section_mut(sec)?.data.reserve(end)?;
        }
        self.reserve_xindex(sym, sec)?;
        self.sym_def(sym, sec, pos as u64, size as u64)?;
        let section = self.get_section_mut(sec)?;
        section.align = section.align.max(align);
        if section.is_virtual {
            section.vsize = end as u64;
        } else {
            section.data.resize_bytes(end)?;
        }
        Ok(pos as u32)
    }
    pub fn reserve_relocations(&mut self, sec: SecRef, additional: usize) -> PgResult<()> {
        let section = self.get_section_mut(sec)?;
        ensure(
            section.has_relocs && !section.is_virtual,
            "section cannot contain relocations",
        )?;
        let end = section
            .relocs
            .len()
            .checked_add(additional)
            .ok_or_else(|| invalid("relocation capacity overflow"))?;
        section.relocs.reserve(end)
    }
    pub fn reloc_sec(
        &mut self,
        sec: SecRef,
        sym: SymRef,
        kind: u32,
        offset: u32,
        addend: i64,
    ) -> PgResult<()> {
        let addend = i32::try_from(addend).map_err(|_| invalid("non-32-bit relocation addend"))?;
        self.symbol(sym)?;
        let section = self.get_section_mut(sec)?;
        ensure(
            section.has_relocs && !section.is_virtual,
            "section cannot contain relocations",
        )?;
        section.relocs.push(Relocation {
            offset,
            symbol: sym,
            kind,
            addend,
        })
    }
    pub fn reloc_pc32(
        &mut self,
        sec: SecRef,
        sym: SymRef,
        offset: u32,
        addend: i64,
    ) -> PgResult<()> {
        self.reloc_sec(sec, sym, self.state.target.reloc_pc32, offset, addend)
    }
    pub fn reloc_abs(
        &mut self,
        sec: SecRef,
        sym: SymRef,
        offset: u32,
        addend: i64,
    ) -> PgResult<()> {
        self.reloc_sec(sec, sym, self.state.target.reloc_abs64, offset, addend)
    }
    pub fn finalize(&mut self) {}
}
pub(crate) fn c_string(bytes: &[u8], offset: usize) -> PgResult<&[u8]> {
    let tail = bytes
        .get(offset..)
        .ok_or_else(|| invalid("string offset out of range"))?;
    let length = tail
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| invalid("unterminated string"))?;
    Ok(&tail[..length])
}

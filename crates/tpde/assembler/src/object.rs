// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{
    assembler::{sec_off, PREDEFINED, SHSTRTAB},
    elf::*,
    *,
};
use mcx::PgVec;

fn add(size: usize, extra: usize) -> PgResult<usize> {
    size.checked_add(extra)
        .ok_or_else(|| invalid("object size overflow"))
}
fn mul(count: usize, size: usize) -> PgResult<usize> {
    count
        .checked_mul(size)
        .ok_or_else(|| invalid("object table size overflow"))
}
fn set_header(out: &mut [u8], index: usize, header: Elf64_Shdr) {
    let offset = 64 + index * 64;
    out[offset..offset + 64].copy_from_slice(&header.to_le_bytes());
}
fn append(out: &mut PgVec<'_, u8>, bytes: &[u8]) -> PgResult<()> {
    mcx::vec_append_bytes(out, bytes)
}
fn pad(out: &mut PgVec<'_, u8>, length: usize) -> PgResult<()> {
    let end = add(out.len(), align_up(length, 8)? - length)?;
    out.resize(end, 0);
    Ok(())
}
impl<'m> AssemblerElf<'m> {
    pub fn build_object_file(&self) -> PgResult<PgVec<'m, u8>> {
        let s = &self.state;
        let extended = s.sections.len() >= SHN_LORESERVE as usize;
        let symbol_count = add(s.locals.len(), s.globals.len())?;
        let section_count = add(s.sections.len(), usize::from(extended))?;
        u32_size(symbol_count)?;
        u32_size(section_count)?;
        ensure(
            s.local_shndx.len() <= s.locals.len() && s.global_shndx.len() <= s.globals.len(),
            "extended symbol table length mismatch",
        )?;
        ensure(
            extended || (s.local_shndx.is_empty() && s.global_shndx.is_empty()),
            "unexpected extended symbol rows",
        )?;
        let headers_end = add(64, mul(section_count, 64)?)?;
        let names_len = add(SHSTRTAB.len(), s.extra_names.size())?;
        let mut size = add(headers_end, mul(symbol_count, 24)?)?;
        size = add(size, align_up(s.strings.size(), 8)?)?;
        size = add(size, align_up(names_len, 8)?)?;
        for slot in &s.sections[PREDEFINED..] {
            let Some(sec) = slot else {
                continue;
            };
            ensure(sec.align.is_power_of_two(), "invalid section alignment")?;
            ensure(!sec.locked, "flush the function writer before exporting")?;
            self.sec_name(sec.get_ref())?;
            ensure(
                !sec.has_relocs || sec.name >= 5,
                "relocation section name lacks prefix",
            )?;
            size = add(size, align_up(sec.data.len(), 8)?)?;
            if sec.has_relocs {
                size = add(size, mul(sec.relocs.len(), 24)?)?;
            }
        }
        if extended {
            size = add(size, mul(symbol_count, 4)?)?;
        }
        u32_size(size)?;
        let mut out = mcx::vec_with_capacity_in(s.mcx, size)?;
        out.resize(headers_end, 0);
        let mut hdr = Elf64_Ehdr {
            e_type: ET_REL,
            e_machine: s.target.elf_machine,
            e_version: EV_CURRENT,
            e_shoff: 64,
            e_ehsize: 64,
            e_shentsize: 64,
            e_shnum: if extended { 0 } else { section_count as u16 },
            e_shstrndx: 4,
            ..Elf64_Ehdr::default()
        };
        hdr.e_ident[..4].copy_from_slice(&ELFMAG[..4]);
        hdr.e_ident[4] = ELFCLASS64;
        hdr.e_ident[5] = ELFDATA2LSB;
        hdr.e_ident[6] = EV_CURRENT as u8;
        hdr.e_ident[7] = s.target.elf_osabi;
        out[..64].copy_from_slice(&hdr.to_le_bytes());
        if extended {
            set_header(
                &mut out,
                0,
                Elf64_Shdr {
                    sh_size: section_count as u64,
                    ..Elf64_Shdr::default()
                },
            );
        }
        let offset = out.len() as u64;
        set_header(
            &mut out,
            1,
            Elf64_Shdr {
                sh_name: const { sec_off(b".note.GNU-stack") },
                sh_type: SHT_PROGBITS,
                sh_offset: offset,
                sh_addralign: 1,
                ..Elf64_Shdr::default()
            },
        );
        for sym in s.locals.iter().chain(s.globals.iter()) {
            append(&mut out, &sym.to_le_bytes())?;
        }
        set_header(
            &mut out,
            2,
            Elf64_Shdr {
                sh_name: const { sec_off(b".symtab") },
                sh_type: SHT_SYMTAB,
                sh_offset: offset,
                sh_size: (symbol_count * 24) as u64,
                sh_link: 3,
                sh_info: s.locals.len() as u32,
                sh_addralign: 8,
                sh_entsize: 24,
                ..Elf64_Shdr::default()
            },
        );
        let offset = out.len() as u64;
        append(&mut out, s.strings.data())?;
        pad(&mut out, s.strings.size())?;
        set_header(
            &mut out,
            3,
            Elf64_Shdr {
                sh_name: const { sec_off(b".strtab") },
                sh_type: SHT_STRTAB,
                sh_offset: offset,
                sh_size: align_up(s.strings.size(), 8)? as u64,
                sh_addralign: 1,
                ..Elf64_Shdr::default()
            },
        );
        let offset = out.len() as u64;
        append(&mut out, SHSTRTAB)?;
        append(&mut out, s.extra_names.data())?;
        pad(&mut out, names_len)?;
        set_header(
            &mut out,
            4,
            Elf64_Shdr {
                sh_name: const { sec_off(b".shstrtab") },
                sh_type: SHT_STRTAB,
                sh_offset: offset,
                sh_size: names_len as u64,
                sh_addralign: 1,
                ..Elf64_Shdr::default()
            },
        );
        let mut index = PREDEFINED;
        while index < s.sections.len() {
            let sec = s.sections[index]
                .as_deref()
                .ok_or_else(|| invalid("unexpected reserved section slot"))?;
            let mut header = Elf64_Shdr {
                sh_name: sec.name,
                sh_type: sec.kind,
                sh_flags: sec.flags as u64,
                sh_offset: out.len() as u64,
                sh_size: sec.size(),
                sh_addralign: sec.align as u64,
                ..Elf64_Shdr::default()
            };
            if sec.kind == SHT_GROUP {
                header.sh_info = self.object_symbol_index(sec.sym)?;
                header.sh_link = 2;
                header.sh_entsize = 4;
            }
            set_header(&mut out, index, header);
            append(&mut out, &sec.data)?;
            pad(&mut out, sec.data.len())?;
            if sec.has_relocs {
                ensure(
                    s.sections.get(index + 1).is_some_and(Option::is_none),
                    "missing reserved relocation slot",
                )?;
                let header = Elf64_Shdr {
                    sh_name: sec.name - 5,
                    sh_type: SHT_RELA,
                    sh_flags: (SHF_INFO_LINK | (sec.flags & SHF_GROUP)) as u64,
                    sh_offset: out.len() as u64,
                    sh_size: (24 * sec.relocs.len()) as u64,
                    sh_link: 2,
                    sh_info: index as u32,
                    sh_addralign: 8,
                    sh_entsize: 24,
                    ..Elf64_Shdr::default()
                };
                set_header(&mut out, index + 1, header);
                for reloc in sec.relocs.iter() {
                    let rela = Elf64_Rela {
                        r_offset: reloc.offset as u64,
                        r_info: r_info(self.object_symbol_index(reloc.symbol)? as u64, reloc.kind),
                        r_addend: reloc.addend as i64,
                    };
                    append(&mut out, &rela.to_le_bytes())?;
                }
                index += 1;
            }
            index += 1;
        }
        if extended {
            let header = Elf64_Shdr {
                sh_name: const { sec_off(b".symtab_shndx") },
                sh_type: SHT_SYMTAB_SHNDX,
                sh_offset: out.len() as u64,
                sh_size: (4 * symbol_count) as u64,
                sh_link: 2,
                sh_addralign: 4,
                sh_entsize: 4,
                ..Elf64_Shdr::default()
            };
            set_header(&mut out, s.sections.len(), header);
            for i in 0..s.locals.len() {
                append(
                    &mut out,
                    &s.local_shndx.get(i).copied().unwrap_or(0).to_le_bytes(),
                )?;
            }
            for i in 0..s.globals.len() {
                append(
                    &mut out,
                    &s.global_shndx.get(i).copied().unwrap_or(0).to_le_bytes(),
                )?;
            }
        }
        ensure(out.len() == size, "object size accounting mismatch")?;
        Ok(out)
    }
    fn object_symbol_index(&self, sym: SymRef) -> PgResult<u32> {
        let offset = if Self::is_local(sym) {
            0
        } else {
            self.state.locals.len()
        };
        u32_size(add(Self::sym_index(sym), offset)?)
    }
}

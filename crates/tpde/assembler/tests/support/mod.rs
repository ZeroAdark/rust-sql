// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use tpde_assembler::*;

pub struct Script<'m> {
    pub input: Vec<u8>,
    pub expected: Vec<u8>,
    pub asm: AssemblerElf<'m>,
    pub snapshots: usize,
}
impl<'m> Script<'m> {
    pub fn new(mcx: mcx::Mcx<'m>, arch: Architecture) -> Self {
        Self {
            input: vec![if arch == Architecture::Aarch64 { 0 } else { 1 }],
            expected: vec![],
            asm: AssemblerElf::new(mcx, arch).unwrap(),
            snapshots: 0,
        }
    }
    fn op(&mut self, op: u8) {
        self.input.push(op);
        self.expected.push(op);
    }
    fn u32(&mut self, n: u32) {
        self.input.extend_from_slice(&n.to_le_bytes());
    }
    fn u64(&mut self, n: u64) {
        self.input.extend_from_slice(&n.to_le_bytes());
    }
    fn bytes(&mut self, bytes: &[u8]) {
        self.u32(bytes.len() as u32);
        self.input.extend_from_slice(bytes);
    }
    fn result(&mut self, n: u64) {
        self.expected.extend_from_slice(&n.to_le_bytes());
    }
    fn binding(binding: SymBinding) -> u8 {
        match binding {
            SymBinding::Local => 0,
            SymBinding::Weak => 1,
            SymBinding::Global => 2,
        }
    }
    pub fn section(&mut self, kind: SectionKind, default: bool) -> SecRef {
        self.op(if default { 1 } else { 0 });
        self.input.push(kind as u8);
        let s = if default {
            self.asm.get_default_section(kind)
        } else {
            self.asm.create_section(kind)
        }
        .unwrap();
        self.result(s.id() as u64);
        s
    }
    pub fn raw_section(&mut self, flags: SectionFlags) -> SecRef {
        self.op(2);
        self.u32(flags.kind);
        self.u32(flags.flags);
        self.u32(flags.name);
        self.input
            .extend_from_slice(&[flags.align, flags.has_relocs as u8, flags.is_bss as u8]);
        let s = self.asm.create_section_with_flags(flags).unwrap();
        self.result(s.id() as u64);
        s
    }
    pub fn rename(&mut self, sec: SecRef, name: &[u8]) {
        self.op(3);
        self.u32(sec.id());
        self.bytes(name);
        self.asm.rename_section(sec, name).unwrap();
    }
    pub fn symbol(&mut self, kind: u8, binding: SymBinding, name: &[u8]) -> SymRef {
        self.op(4);
        self.input
            .extend_from_slice(&[kind, Self::binding(binding)]);
        self.bytes(name);
        let s = match kind {
            0 => self.asm.sym_add_undef(name, binding),
            1 => self.asm.sym_predef_func(name, binding),
            2 => self.asm.sym_predef_data(name, binding),
            3 => self.asm.sym_predef_tls(name, binding),
            _ => panic!(),
        }
        .unwrap();
        self.result(s.id() as u64);
        s
    }
    pub fn define(&mut self, sym: SymRef, sec: SecRef, pos: u64, size: u64) {
        self.op(5);
        self.u32(sym.id());
        self.u32(sec.id());
        self.u64(pos);
        self.u64(size);
        self.asm.sym_def(sym, sec, pos, size).unwrap();
    }
    pub fn def_bytes(&mut self, sec: SecRef, sym: SymRef, align: u32, bytes: &[u8]) {
        self.op(6);
        self.u32(sec.id());
        self.u32(sym.id());
        self.u32(align);
        self.bytes(bytes);
        let off = self
            .asm
            .sym_def_predef_bytes(sec, sym, bytes, align)
            .unwrap();
        self.result(off as u64);
    }
    pub fn zero(&mut self, sec: SecRef, sym: SymRef, size: u32, align: u32) {
        self.op(7);
        self.u32(sec.id());
        self.u32(sym.id());
        self.u32(size);
        self.u32(align);
        let off = self.asm.sym_def_predef_zero(sec, sym, size, align).unwrap();
        self.result(off as u64);
    }
    pub fn allocate_fill(&mut self, sec: SecRef, sym: SymRef, size: u32, align: u32, fill: u8) {
        self.op(8);
        self.u32(sec.id());
        self.u32(sym.id());
        self.u32(size);
        self.u32(align);
        self.input.push(fill);
        let off = self
            .asm
            .sym_def_predef_data(sec, sym, size as u64, align)
            .unwrap();
        self.asm.get_section_mut(sec).unwrap().data[off as usize..off as usize + size as usize]
            .fill(fill);
        self.result(off as u64);
    }
    pub fn data(
        &mut self,
        sec: SecRef,
        name: &[u8],
        binding: SymBinding,
        align: u32,
        bytes: &[u8],
    ) -> SymRef {
        self.op(9);
        self.u32(sec.id());
        self.bytes(name);
        self.input.push(Self::binding(binding));
        self.u32(align);
        self.bytes(bytes);
        let (s, off) = self
            .asm
            .sym_def_data(sec, name, bytes, align, binding)
            .unwrap();
        self.result(s.id() as u64);
        self.result(off as u64);
        s
    }
    pub fn section_symbol(&mut self, sec: SecRef) -> SymRef {
        self.op(10);
        self.u32(sec.id());
        let s = self.asm.section_symbol(sec).unwrap();
        self.result(s.id() as u64);
        s
    }
    pub fn visibility(&mut self, s: SymRef, v: SymVisibility) {
        self.op(11);
        self.u32(s.id());
        self.input.push(v as u8);
        self.asm.sym_set_visibility(s, v).unwrap();
    }
    pub fn copy(&mut self, dst: SymRef, src: SymRef) {
        self.op(12);
        self.u32(dst.id());
        self.u32(src.id());
        self.asm.sym_copy(dst, src).unwrap();
    }
    pub fn group(&mut self, s: SymRef, comdat: bool) -> SecRef {
        self.op(13);
        self.u32(s.id());
        self.input.push(comdat as u8);
        let r = self.asm.create_group_section(s, comdat).unwrap();
        self.result(r.id() as u64);
        r
    }
    pub fn add_group(&mut self, group: SecRef, sec: SecRef) {
        self.op(14);
        self.u32(group.id());
        self.u32(sec.id());
        self.asm.add_to_group(group, sec).unwrap();
    }
    pub fn structor(&mut self, init: bool, group: SecRef) -> SecRef {
        self.op(15);
        self.input.push(init as u8);
        self.u32(group.id());
        let sec = self.asm.create_structor_section(init, group).unwrap();
        self.result(sec.id() as u64);
        sec
    }
    pub fn write(&mut self, sec: SecRef, bytes: &[u8]) {
        self.op(16);
        self.u32(sec.id());
        self.bytes(bytes);
        self.asm.get_section_mut(sec).unwrap().write(bytes).unwrap();
    }
    pub fn write_u64(&mut self, sec: SecRef, n: u64) {
        self.op(17);
        self.u32(sec.id());
        self.u64(n);
        self.asm.get_section_mut(sec).unwrap().write(&n).unwrap();
    }
    pub fn relocation(&mut self, sec: SecRef, s: SymRef, kind: u32, off: u32, addend: i64) {
        self.op(18);
        self.u32(sec.id());
        self.u32(s.id());
        self.u32(kind);
        self.u32(off);
        self.u64(addend as u64);
        self.asm.reloc_sec(sec, s, kind, off, addend).unwrap();
    }
    pub fn target_reloc(&mut self, abs: bool, sec: SecRef, s: SymRef, off: u32, addend: i64) {
        self.op(19);
        self.input.push(abs as u8);
        self.u32(sec.id());
        self.u32(s.id());
        self.u32(off);
        self.u64(addend as u64);
        if abs {
            self.asm.reloc_abs(sec, s, off, addend)
        } else {
            self.asm.reloc_pc32(sec, s, off, addend)
        }
        .unwrap();
    }
    pub fn adjust(&mut self, sec: SecRef, start: usize, offset: u32) {
        self.op(20);
        self.u32(sec.id());
        self.u64(start as u64);
        self.u32(offset);
        self.asm
            .get_section_mut(sec)
            .unwrap()
            .adjust_relocation_offsets(start, offset);
    }
    pub fn snapshot(&mut self) {
        self.op(22);
        self.asm.finalize();
        let out = self.asm.build_object_file().unwrap();
        self.result(out.len() as u64);
        self.expected.extend_from_slice(&out);
        self.snapshots += 1;
    }
    pub fn reset(&mut self) {
        self.op(23);
        self.asm.reset().unwrap();
    }
    pub fn bulk(&mut self, target: u32) -> SecRef {
        self.op(24);
        self.u32(target);
        let sec = loop {
            let sec = self.asm.create_section(SectionKind::ReadOnly).unwrap();
            if sec.id() >= target {
                break sec;
            }
        };
        self.result(sec.id() as u64);
        sec
    }
    pub fn query_section(&mut self, sec: SecRef, relocs: bool, name: bool) {
        self.op(25);
        self.u32(sec.id());
        self.input.extend_from_slice(&[relocs as u8, name as u8]);
        let s = self.asm.get_section(sec).unwrap();
        let id = s.get_ref().id();
        let size = s.size();
        let count = if relocs { s.reloc_count().unwrap() } else { 0 };
        self.result(id as u64);
        self.result(size);
        if relocs {
            self.result(count as u64);
        }
        if name {
            let bytes = self.asm.sec_name(sec).unwrap().to_vec();
            self.result(bytes.len() as u64);
            self.expected.extend_from_slice(&bytes);
        }
    }
    pub fn query_symbol(&mut self, s: SymRef, defined: bool) {
        self.op(26);
        self.u32(s.id());
        self.input.push(defined as u8);
        let name = self.asm.sym_name(s).unwrap().to_vec();
        self.result(name.len() as u64);
        self.expected.extend_from_slice(&name);
        if defined {
            self.result(self.asm.sym_section(s).unwrap().id() as u64);
        }
    }
}

// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![no_std]
extern crate alloc;
mod assembler;
pub mod elf;
mod object;
pub use assembler::AssemblerElf;
use mcx::Mcx;
pub use tpde_util::SmallVec;
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
fn align_up(size: usize, align: u32) -> PgResult<usize> {
    ensure(
        align.is_power_of_two(),
        "alignment must be a nonzero power of two",
    )?;
    size.checked_add(align as usize - 1)
        .map(|v| v & !(align as usize - 1))
        .ok_or_else(|| invalid("aligned size overflow"))
}
fn u32_size(size: usize) -> PgResult<u32> {
    u32::try_from(size).map_err(|_| invalid("ELF offset exceeds u32"))
}
macro_rules! reference {
    ($name:ident) => {
        #[repr(transparent)]
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub struct $name(u32);
        impl $name {
            pub const fn new(id: u32) -> Self {
                Self(id)
            }
            pub const fn id(self) -> u32 {
                self.0
            }
            pub const fn valid(self) -> bool {
                self.0 != 0
            }
        }
    };
}
reference!(SymRef);
reference!(SecRef);
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Relocation {
    pub offset: u32,
    pub symbol: SymRef,
    pub kind: u32,
    pub addend: i32,
}
mcx::forget_safe_nodrop!(Relocation, elf::Elf64_Sym);
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionKind {
    Text,
    ReadOnly,
    EHFrame,
    LSDA,
    Data,
    DataRelRO,
    BSS,
    ThreadData,
    ThreadBSS,
}
impl SectionKind {
    pub const ALL: [Self; 9] = [
        Self::Text,
        Self::ReadOnly,
        Self::EHFrame,
        Self::LSDA,
        Self::Data,
        Self::DataRelRO,
        Self::BSS,
        Self::ThreadData,
        Self::ThreadBSS,
    ];
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymBinding {
    Local,
    Weak,
    Global,
}
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymVisibility {
    Default = 0,
    Internal = 1,
    Hidden = 2,
    Protected = 3,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Architecture {
    Aarch64,
    X86_64,
}
#[derive(Clone, Copy, Debug)]
pub struct SectionFlags {
    pub kind: u32,
    pub flags: u32,
    pub name: u32,
    pub align: u8,
    pub has_relocs: bool,
    pub is_bss: bool,
}
impl Default for SectionFlags {
    fn default() -> Self {
        Self {
            kind: 0,
            flags: 0,
            name: 0,
            align: 1,
            has_relocs: true,
            is_bss: false,
        }
    }
}
#[derive(Clone, Copy)]
pub struct TargetInfo {
    pub reloc_pc32: u32,
    pub reloc_abs64: u32,
    pub section_flags: [SectionFlags; 9],
    pub elf_osabi: u8,
    pub elf_machine: u16,
}

pub struct DataSection<'m> {
    pub data: SmallVec<'m, u8, 256>,
    pub addr: u64,
    pub vsize: u64,
    pub kind: u32,
    pub flags: u32,
    pub name: u32,
    pub align: u32,
    pub target_info: Option<usize>,
    pub is_virtual: bool,
    /// Upstream `DataSection::locked` (debug-only there): the bytes are owned by
    /// a function writer between its first reservation and `flush`, so `data`
    /// is empty and every data-level access is refused until it is restored.
    pub locked: bool,
    pub(crate) sym: SymRef,
    reference: SecRef,
    pub(crate) relocs: SmallVec<'m, Relocation, 4>,
    pub(crate) has_relocs: bool,
}
impl<'m> DataSection<'m> {
    fn new(mcx: Mcx<'m>, reference: SecRef, flags: SectionFlags) -> Self {
        Self {
            data: SmallVec::new(mcx),
            addr: 0,
            vsize: 0,
            kind: flags.kind,
            flags: flags.flags,
            name: flags.name,
            align: flags.align as u32,
            target_info: None,
            is_virtual: flags.is_bss,
            locked: false,
            sym: SymRef::default(),
            reference,
            relocs: SmallVec::new(mcx),
            has_relocs: flags.has_relocs,
        }
    }
    pub fn get_ref(&self) -> SecRef {
        self.reference
    }
    pub fn size(&self) -> u64 {
        if self.is_virtual {
            self.vsize
        } else {
            self.data.len() as u64
        }
    }
    pub fn write<T: SectionValue + ?Sized>(&mut self, value: &T) -> PgResult<()> {
        ensure(!self.is_virtual, "cannot write a virtual section")?;
        ensure(!self.locked, "section data is locked by a function writer")?;
        value.append_to(&mut self.data)
    }
    pub fn reloc_count(&self) -> PgResult<usize> {
        ensure(
            !self.is_virtual && self.has_relocs,
            "section has no relocation storage",
        )?;
        Ok(self.relocs.len())
    }
    pub fn relocations(&self) -> &[Relocation] {
        &self.relocs
    }
    pub fn adjust_relocation_offsets(&mut self, start: usize, offset: u32) {
        if start >= self.relocs.len() {
            return;
        }
        for reloc in &mut self.relocs[start..] {
            reloc.offset = reloc.offset.wrapping_sub(offset);
        }
    }
}
pub trait SectionValue {
    fn append_to(&self, data: &mut SmallVec<'_, u8, 256>) -> PgResult<()>;
}
macro_rules! primitive_value {
    ($($t:ty),*) => {
        $(
            impl SectionValue for $t {
                fn append_to(&self, data: &mut SmallVec<'_, u8, 256>) -> PgResult<()> {
                    data.extend_from_slice(&self.to_le_bytes())
                }
            }
        )*
    };
}
primitive_value!(
    u8,
    i8,
    u16,
    i16,
    u32,
    i32,
    u64,
    i64,
    u128,
    i128,
    usize,
    isize,
    f32,
    f64,
    elf::Elf64_Ehdr,
    elf::Elf64_Shdr,
    elf::Elf64_Sym,
    elf::Elf64_Rela
);
impl SectionValue for bool {
    fn append_to(&self, data: &mut SmallVec<'_, u8, 256>) -> PgResult<()> {
        data.push(u8::from(*self))
    }
}
impl SectionValue for [u8] {
    fn append_to(&self, data: &mut SmallVec<'_, u8, 256>) -> PgResult<()> {
        data.extend_from_slice(self)
    }
}
impl<const N: usize> SectionValue for [u8; N] {
    fn append_to(&self, data: &mut SmallVec<'_, u8, 256>) -> PgResult<()> {
        data.extend_from_slice(self)
    }
}
const _: () = {
    assert!(core::mem::size_of::<Relocation>() == 16);
    assert!(core::mem::size_of::<AssemblerElf<'static>>() <= 128);
    assert!(!core::mem::needs_drop::<DataSection<'static>>());
    assert!(!core::mem::needs_drop::<AssemblerElf<'static>>());
};
#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests;

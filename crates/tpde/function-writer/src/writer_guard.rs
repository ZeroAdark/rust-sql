// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use core::{
    mem::{self, MaybeUninit},
    ops::{Deref, DerefMut},
    ptr::NonNull,
};
use mcx::Mcx;
use tpde_assembler::{AssemblerElf, DataSection, SecRef, SectionKind, SymRef};
use tpde_core::Reg;
use tpde_util::{
    dwarf, misc, tpde_assert, BumpAllocator, Integral, SmallVec, UninitResizeGuard, VectorWriter,
};
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};

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
fn offset32(offset: usize) -> PgResult<u32> {
    u32::try_from(offset).map_err(|_| invalid("function writer offset exceeds u32"))
}
fn add(a: usize, b: usize) -> PgResult<usize> {
    a.checked_add(b)
        .ok_or_else(|| invalid("function writer size overflow"))
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Label(pub u32);
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LabelFixupKind(pub u8);
impl LabelFixupKind {
    pub const ARCH_KIND_BEGIN: Self = Self(0);
    pub const X64_JMP_OR_MEM_DISP: Self = Self(0);
    pub const AARCH64_BR: Self = Self(0);
    pub const AARCH64_COND_BR: Self = Self(1);
    pub const AARCH64_TEST_BR: Self = Self(2);
}
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LabelFixup {
    pub label: Label,
    pub off: u32,
    pub kind: LabelFixupKind,
}
#[derive(Clone, Copy)]
pub struct TargetCieInfo<'a> {
    pub instrs: &'a [u8],
    pub return_addr_register: u8,
    pub code_alignment_factor: u8,
    pub data_alignment_factor: u8,
}
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct JumpTableHeader {
    size: u32,
    pub off: u32,
    pub idx: Reg,
    pub tmp: Reg,
    pub misc: u8,
}
impl JumpTableHeader {
    pub fn size(&self) -> u32 {
        self.size
    }
}
pub struct JumpTable<'a> {
    header: &'a mut JumpTableHeader,
    labels: &'a mut [Label],
}
impl JumpTable<'_> {
    pub fn labels(&self) -> &[Label] {
        self.labels
    }
    pub fn labels_mut(&mut self) -> &mut [Label] {
        self.labels
    }
    pub fn set_off(&mut self, off: u32) {
        self.header.off = off;
    }
    pub fn set_idx(&mut self, idx: Reg) {
        self.header.idx = idx;
    }
    pub fn set_tmp(&mut self, tmp: Reg) {
        self.header.tmp = tmp;
    }
    pub fn set_misc(&mut self, misc: u8) {
        self.header.misc = misc;
    }
}
impl Deref for JumpTable<'_> {
    type Target = JumpTableHeader;
    fn deref(&self) -> &Self::Target {
        self.header
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
struct ExceptCallSiteInfo {
    start: u64,
    len: u64,
    landing_pad: Label,
    action_entry: u32,
}
mcx::forget_safe_nodrop!(Label, LabelFixupKind, LabelFixup, ExceptCallSiteInfo);
const _: () = {
    assert!(mem::size_of::<JumpTableHeader>() == 12);
    assert!(mem::size_of::<LabelFixup>() == 12);
    assert!(mem::size_of::<ExceptCallSiteInfo>() == 24);
};

struct WriterState<'m> {
    section: SecRef,
    /// Upstream `data_begin..data_reserve_end`: while `owned`, the current
    /// section's bytes live here (the section is locked in the assembler) and
    /// every write is an owned-buffer store; `flush` hands them back.
    text: SmallVec<'m, u8, 256>,
    owned: bool,
    cursor: usize,
    reserved_end: usize,
    func_begin: u32,
    reloc_begin: usize,
    label_skew: u32,
    label_offsets: SmallVec<'m, u32, 64>,
    label_fixups: SmallVec<'m, LabelFixup, 21>,
    growth_size: u32,
    jump_table_alloc: BumpAllocator<'m>,
    jump_tables: SmallVec<'m, NonNull<JumpTableHeader>, 32>,
    eh_section: SecRef,
    eh_cursor: usize,
    personality: SymRef,
    cie_offset: u32,
    fde_start: u32,
    fde_open: bool,
    call_sites: SmallVec<'m, ExceptCallSiteInfo, 0>,
    encoded_call_sites: SmallVec<'m, u8, 0>,
    actions: SmallVec<'m, u8, 0>,
    type_info: SmallVec<'m, u32, 0>,
    specs: SmallVec<'m, u8, 0>,
}
impl<'m> WriterState<'m> {
    fn new(mcx: Mcx<'m>) -> Self {
        Self {
            section: SecRef::default(),
            text: SmallVec::new(mcx),
            owned: false,
            cursor: 0,
            reserved_end: 0,
            func_begin: 0,
            reloc_begin: 0,
            label_skew: 0,
            label_offsets: SmallVec::new(mcx),
            label_fixups: SmallVec::new(mcx),
            growth_size: 0x10000,
            jump_table_alloc: BumpAllocator::new(mcx),
            jump_tables: SmallVec::new(mcx),
            eh_section: SecRef::default(),
            eh_cursor: 0,
            personality: SymRef::default(),
            cie_offset: 0,
            fde_start: 0,
            fde_open: false,
            call_sites: SmallVec::new(mcx),
            encoded_call_sites: SmallVec::new(mcx),
            actions: SmallVec::new(mcx),
            type_info: SmallVec::new(mcx),
            specs: SmallVec::new(mcx),
        }
    }
}
impl Drop for WriterState<'_> {
    fn drop(&mut self) {
        self.text.release();
        self.label_offsets.release();
        self.label_fixups.release();
        self.jump_tables.release();
        self.call_sites.release();
        self.encoded_call_sites.release();
        self.actions.release();
        self.type_info.release();
        self.specs.release();
    }
}

pub trait FunctionWriterTarget: Sized {
    const CIE: TargetCieInfo<'static>;
    fn before_begin_func(&mut self) {}
    fn handle_fixups(&mut self, base: &mut FunctionWriterBase<'_, '_, '_>) -> PgResult<()>;
    fn more_space(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        size: usize,
    ) -> PgResult<()> {
        base.more_space(size)
    }
    fn ensure_space(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        size: usize,
    ) -> PgResult<()> {
        base.ensure_space_with(self, size)
    }
    fn label_ref(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        label: Label,
        off: u32,
        kind: LabelFixupKind,
    ) -> PgResult<()> {
        base.label_ref(label, off, kind)
    }
    fn eh_advance(&mut self, base: &mut FunctionWriterBase<'_, '_, '_>, size: u64) -> PgResult<()> {
        base.eh_advance(size)
    }
    fn align(&mut self, base: &mut FunctionWriterBase<'_, '_, '_>, align: usize) -> PgResult<()> {
        base.ensure_space_with(self, align)?;
        base.align_reserved(align)
    }
}

pub trait FunctionWriterJumpTables: FunctionWriterTarget {
    fn create_jump_table(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        size: u32,
        index: Reg,
        temporary: Reg,
        index_is_32: bool,
        fill: impl FnOnce(&mut [Label]),
    ) -> PgResult<()>;
}

pub struct FunctionWriter<'m, T: FunctionWriterTarget> {
    state: WriterState<'m>,
    target: T,
    cie: TargetCieInfo<'m>,
}
impl<'m, T: FunctionWriterTarget> FunctionWriter<'m, T> {
    pub fn new(mcx: Mcx<'m>, target: T) -> Self {
        Self {
            state: WriterState::new(mcx),
            target,
            cie: T::CIE,
        }
    }
    pub fn with_cie(mcx: Mcx<'m>, target: T, cie: TargetCieInfo<'m>) -> Self {
        Self {
            state: WriterState::new(mcx),
            target,
            cie,
        }
    }
    pub fn target(&self) -> &T {
        &self.target
    }
    pub fn target_mut(&mut self) -> &mut T {
        &mut self.target
    }
    pub fn begin_module<'w, 'a>(
        &'w mut self,
        assembler: &'a mut AssemblerElf<'m>,
    ) -> PgResult<ModuleWriter<'w, 'a, 'm, T>> {
        let eh_section = assembler.get_default_section(SectionKind::EHFrame)?;
        self.state.section = SecRef::default();
        // A module abandoned without flush leaves its section locked in that
        // assembler; the bytes are dropped here and export reports the lock.
        self.state.text.release();
        self.state.owned = false;
        self.state.cursor = 0;
        self.state.reserved_end = 0;
        self.state.eh_section = eh_section;
        self.state.eh_cursor = assembler.get_section(eh_section)?.data.len();
        self.state.personality = SymRef::default();
        self.state.fde_open = false;
        let mut base = FunctionWriterBase {
            state: &mut self.state,
            assembler,
            cie: self.cie,
        };
        base.eh_init_cie(SymRef::default())?;
        Ok(ModuleWriter {
            base,
            target: &mut self.target,
        })
    }
}

pub struct FunctionWriterBase<'w, 'a, 'm> {
    state: &'w mut WriterState<'m>,
    assembler: &'a mut AssemblerElf<'m>,
    cie: TargetCieInfo<'m>,
}
pub struct ModuleWriter<'w, 'a, 'm, T: FunctionWriterTarget> {
    pub base: FunctionWriterBase<'w, 'a, 'm>,
    target: &'w mut T,
}
impl<'w, 'a, 'm, T: FunctionWriterTarget> Deref for ModuleWriter<'w, 'a, 'm, T> {
    type Target = FunctionWriterBase<'w, 'a, 'm>;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}
impl<T: FunctionWriterTarget> DerefMut for ModuleWriter<'_, '_, '_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.base
    }
}
impl<'w, 'a, 'm, T: FunctionWriterTarget> ModuleWriter<'w, 'a, 'm, T> {
    pub fn target(&self) -> &T {
        self.target
    }
    pub fn parts_mut(&mut self) -> (&mut T, &mut FunctionWriterBase<'w, 'a, 'm>) {
        (self.target, &mut self.base)
    }
    pub fn reserve_uninitialized(&mut self, size: usize) -> PgResult<TextReservationGuard<'_, 'm>> {
        self.ensure_space(size)?;
        self.base.reserve_uninitialized_unchecked(size)
    }
    pub fn begin_func(&mut self, align: u32, expected_size: u32) -> PgResult<()> {
        ensure(align.is_power_of_two(), "invalid function alignment")?;
        let requested_hint = add(align as usize, expected_size as usize)?;
        self.base.state.actions.reserve(2)?;
        self.target.before_begin_func();
        self.base.state.growth_size = expected_size;
        ensure(
            self.base.state.reserved_end >= self.base.state.cursor,
            "invalid text reservation",
        )?;
        if self.base.state.reserved_end - self.base.state.cursor < requested_hint {
            self.target.more_space(&mut self.base, requested_hint)?;
        }
        self.align(align as usize)?;
        self.base.begin_func()
    }
    pub fn finish_func(&mut self) -> PgResult<()> {
        self.target.handle_fixups(&mut self.base)
    }
    #[inline(always)]
    pub fn ensure_space(&mut self, size: usize) -> PgResult<()> {
        self.target.ensure_space(&mut self.base, size)
    }
    pub fn label_ref(&mut self, label: Label, off: u32, kind: LabelFixupKind) -> PgResult<()> {
        self.target.label_ref(&mut self.base, label, off, kind)
    }
    pub fn eh_advance(&mut self, size: u64) -> PgResult<()> {
        self.target.eh_advance(&mut self.base, size)
    }
    pub fn align(&mut self, align: usize) -> PgResult<()> {
        ensure(align.is_power_of_two(), "invalid function alignment")?;
        self.target.align(&mut self.base, align)
    }
    #[inline(always)]
    pub fn write<I: Integral>(&mut self, value: I) -> PgResult<()> {
        self.ensure_space(I::SIZE)?;
        self.base.write_unchecked(value)
    }
    #[inline(always)]
    pub fn write_bytes(&mut self, bytes: &[u8]) -> PgResult<()> {
        self.ensure_space(bytes.len())?;
        self.base.write_bytes_unchecked(bytes)
    }
    pub fn skip(&mut self, size: usize) -> PgResult<()> {
        self.ensure_space(size)?;
        self.base.skip_unchecked(size)
    }
}

impl<T: FunctionWriterJumpTables> ModuleWriter<'_, '_, '_, T> {
    pub fn create_jump_table(
        &mut self,
        size: u32,
        index: Reg,
        temporary: Reg,
        index_is_32: bool,
        fill: impl FnOnce(&mut [Label]),
    ) -> PgResult<()> {
        self.target
            .create_jump_table(&mut self.base, size, index, temporary, index_is_32, fill)
    }
}

pub struct TextReservationGuard<'a, 'm> {
    reservation: UninitResizeGuard<'a, 'm, u8, 256>,
    cursor: &'a mut usize,
}
impl TextReservationGuard<'_, '_> {
    /// # Safety
    /// The first `initialized` bytes must be initialized before publishing them.
    pub unsafe fn commit(self, initialized: usize) -> PgResult<()> {
        ensure(
            initialized <= self.reservation.len(),
            "text emission exceeds reservation",
        )?;
        let Self {
            reservation,
            cursor,
        } = self;
        // SAFETY: caller initialized this prefix, and the vector guard restores spare-byte poisoning.
        unsafe {
            reservation.assume_init_prefix(initialized)?;
        }
        *cursor += initialized;
        Ok(())
    }
}
impl Deref for TextReservationGuard<'_, '_> {
    type Target = [MaybeUninit<u8>];
    fn deref(&self) -> &Self::Target {
        &self.reservation
    }
}
impl DerefMut for TextReservationGuard<'_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.reservation
    }
}

pub struct EhWriterGuard<'a, 'm> {
    writer: VectorWriter<'a, 'm>,
    cursor: &'a mut usize,
}
impl<'a, 'm> Deref for EhWriterGuard<'a, 'm> {
    type Target = VectorWriter<'a, 'm>;
    fn deref(&self) -> &Self::Target {
        &self.writer
    }
}
impl DerefMut for EhWriterGuard<'_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.writer
    }
}
impl Drop for EhWriterGuard<'_, '_> {
    fn drop(&mut self) {
        *self.cursor = self.writer.size();
    }
}

impl<'m> FunctionWriterBase<'_, '_, 'm> {
    pub fn assembler(&self) -> &AssemblerElf<'m> {
        self.assembler
    }
    pub fn assembler_mut(&mut self) -> &mut AssemblerElf<'m> {
        self.assembler
    }
    pub fn get_sec_ref(&self) -> SecRef {
        self.state.section
    }
    pub fn get_section(&self) -> PgResult<&DataSection<'m>> {
        self.assembler.get_section(self.state.section)
    }
    #[inline(always)]
    pub fn offset(&self) -> usize {
        self.state.cursor
    }
    pub fn allocated_size(&self) -> usize {
        self.state.reserved_end
    }
    pub fn growth_size(&self) -> u32 {
        self.state.growth_size
    }
    pub fn func_begin(&self) -> u32 {
        self.state.func_begin
    }
    pub fn label_skew(&self) -> u32 {
        self.state.label_skew
    }
    pub fn label_fixups(&self) -> &[LabelFixup] {
        &self.state.label_fixups
    }
    pub fn label_fixups_mut(&mut self) -> &mut [LabelFixup] {
        &mut self.state.label_fixups
    }
    pub fn data(&self) -> PgResult<&[u8]> {
        let bytes: &[u8] = if self.state.owned {
            &self.state.text
        } else {
            &self.get_section()?.data
        };
        bytes
            .get(..self.offset())
            .ok_or_else(|| invalid("text cursor exceeds initialized data"))
    }
    pub fn data_mut(&mut self) -> PgResult<&mut [u8]> {
        self.ensure_owned()?;
        let cursor = self.state.cursor;
        self.state
            .text
            .get_mut(..cursor)
            .ok_or_else(|| invalid("text cursor exceeds initialized data"))
    }
    /// Initialized bytes of the current section (the writer's buffer while owned).
    pub fn text_len(&self) -> usize {
        if self.state.owned {
            self.state.text.len()
        } else {
            self.get_section().map_or(0, |section| section.data.len())
        }
    }
    pub fn text_capacity(&self) -> usize {
        if self.state.owned {
            self.state.text.capacity()
        } else {
            self.get_section().map_or(0, |section| section.data.capacity())
        }
    }
    pub fn owns_text(&self) -> bool {
        self.state.owned
    }
    /// Take the current section's bytes (upstream: first `more_space` sets
    /// `section->locked`). Idempotent while owned.
    fn ensure_owned(&mut self) -> PgResult<()> {
        if !self.state.owned {
            self.assembler
                .lock_section_data(self.state.section, &mut self.state.text)?;
            self.state.owned = true;
        }
        Ok(())
    }
    pub fn set_offset(&mut self, offset: usize) -> PgResult<()> {
        ensure(
            offset <= self.state.reserved_end && offset <= self.text_len(),
            "text cursor outside initialized reservation",
        )?;
        self.state.cursor = offset;
        Ok(())
    }
    pub fn cap_reserved_end(&mut self, end: usize) -> PgResult<()> {
        ensure(
            end >= self.state.cursor && end <= self.state.reserved_end,
            "invalid text reservation limit",
        )?;
        self.state.reserved_end = end;
        Ok(())
    }
    pub fn switch_section(&mut self, section: SecRef) -> PgResult<()> {
        ensure(
            self.state.cursor == self.state.reserved_end && !self.state.owned,
            "flush text before switching sections",
        )?;
        let data = self.assembler.get_section(section)?;
        ensure(!data.is_virtual, "cannot write virtual text section")?;
        ensure(!data.locked, "section data is locked by a function writer")?;
        self.state.cursor = data.data.len();
        self.state.reserved_end = self.state.cursor;
        self.state.section = section;
        Ok(())
    }
    /// Upstream `flush`: publish exactly `offset()` bytes and give the section
    /// its bytes back (unlocking it); the reservation collapses to the cursor.
    pub fn flush(&mut self) -> PgResult<()> {
        if self.state.owned {
            self.state.text.truncate(self.state.cursor);
            self.assembler
                .unlock_section_data(self.state.section, &mut self.state.text)?;
            // The moved-out buffer keeps no inline capacity; restore the 256-byte
            // baseline so the next lock/more_space sees the same capacity sequence
            // a fresh section would (reuse must reproduce the first module's bytes).
            self.state.text.release();
            self.state.owned = false;
        } else if self.state.section.valid() {
            // A rewound cursor after an earlier flush still publishes exactly
            // offset() bytes.
            self.assembler
                .get_section_mut(self.state.section)?
                .data
                .truncate(self.state.cursor);
        }
        self.state.reserved_end = self.state.cursor;
        Ok(())
    }
    pub fn begin_func(&mut self) -> PgResult<()> {
        let begin = offset32(self.offset())?;
        let reloc = self.get_section()?.reloc_count()?;
        self.state.actions.reserve(2)?;
        self.state.func_begin = begin;
        self.state.reloc_begin = reloc;
        self.state.label_offsets.clear();
        self.state.label_skew = 0;
        self.state.label_fixups.clear();
        self.state.jump_tables.clear();
        self.state.jump_table_alloc.reset();
        self.state.call_sites.clear();
        self.state.actions.clear();
        self.state.type_info.clear();
        self.state.specs.clear();
        self.state.actions.resize_bytes(2)?;
        Ok(())
    }
    pub fn more_space(&mut self, size: usize) -> PgResult<()> {
        self.ensure_owned()?;
        let current = self.allocated_size();
        let required = add(current, size)?;
        let capacity = self.state.text.capacity();
        let (new_size, growth) = if required <= capacity {
            (capacity, self.state.growth_size)
        } else {
            let new_size = add(current, size.max(self.state.growth_size as usize))?;
            let growth = self
                .state
                .growth_size
                .saturating_add(self.state.growth_size >> 1)
                .min(0x1000000);
            (new_size, growth)
        };
        offset32(new_size)?;
        self.state.text.reserve(new_size)?;
        self.state.reserved_end = new_size;
        self.state.growth_size = growth;
        Ok(())
    }
    #[inline(always)]
    pub fn ensure_space_with<T: FunctionWriterTarget>(
        &mut self,
        target: &mut T,
        size: usize,
    ) -> PgResult<()> {
        ensure(
            self.state.reserved_end >= self.state.cursor,
            "invalid text reservation",
        )?;
        if self.state.reserved_end - self.state.cursor < size {
            target.more_space(self, size)?;
        }
        ensure(
            self.state.reserved_end - self.state.cursor >= size,
            "target did not reserve requested text space",
        )
    }
    pub fn align(&mut self, align: usize) -> PgResult<()> {
        ensure(align.is_power_of_two(), "invalid function alignment")?;
        if self.state.reserved_end - self.state.cursor < align {
            self.more_space(align)?;
        }
        self.align_reserved(align)
    }
    fn align_reserved(&mut self, align: usize) -> PgResult<()> {
        ensure(align.is_power_of_two(), "invalid function alignment")?;
        let end = add(self.offset(), align - 1)? & !(align - 1);
        let align = offset32(align)?;
        self.skip_unchecked(end - self.offset())?;
        let section = self.assembler.get_section_mut(self.state.section)?;
        section.align = section.align.max(align);
        Ok(())
    }
    /// Upstream `write_unchecked`: one bounds check against the reservation and a
    /// `memcpy` at the cursor. The section's live length is the initialized
    /// high-water mark and only grows when the cursor passes it.
    #[inline(always)]
    pub fn write_unchecked<I: Integral>(&mut self, value: I) -> PgResult<()> {
        value.with_ne_bytes(|bytes| self.write_bytes_unchecked(bytes))
    }
    #[inline(always)]
    fn admit_write(&self, size: usize) -> PgResult<usize> {
        let end = add(self.state.cursor, size)?;
        ensure(end <= self.state.reserved_end, "text reservation exhausted")?;
        Ok(end)
    }
    #[inline(always)]
    pub fn write_bytes_unchecked(&mut self, bytes: &[u8]) -> PgResult<()> {
        let end = self.admit_write(bytes.len())?;
        if bytes.is_empty() {
            return Ok(());
        }
        // A non-empty admitted write implies reserved_end > cursor, which only
        // more_space (taking ownership) establishes; the length check inside
        // write_at (offset <= len) rejects the unowned empty buffer otherwise.
        self.state.text.write_at(self.state.cursor, bytes)?;
        self.state.cursor = end;
        Ok(())
    }
    #[inline(always)]
    pub fn skip_unchecked(&mut self, size: usize) -> PgResult<()> {
        let end = self.admit_write(size)?;
        if size == 0 {
            return Ok(());
        }
        self.state.text.zero_at(self.state.cursor, size)?;
        self.state.cursor = end;
        Ok(())
    }
    fn check_write(&mut self, size: usize) -> PgResult<()> {
        self.admit_write(size)?;
        self.ensure_owned()?;
        ensure(
            self.state.cursor <= self.state.text.len(),
            "text cursor exceeds initialized data",
        )
    }
    pub fn reserve_uninitialized_unchecked(
        &mut self,
        size: usize,
    ) -> PgResult<TextReservationGuard<'_, 'm>> {
        self.check_write(size)?;
        ensure(
            self.offset() == self.state.text.len(),
            "uninitialized reservation requires append cursor",
        )?;
        let end = add(self.state.cursor, size)?;
        let reservation = self.state.text.resize_uninitialized(end)?;
        Ok(TextReservationGuard {
            reservation,
            cursor: &mut self.state.cursor,
        })
    }
    pub fn reloc(&mut self, sym: SymRef, kind: u32, off: u64, addend: i64) -> PgResult<()> {
        let off = u32::try_from(off).map_err(|_| invalid("relocation offset exceeds u32"))?;
        self.assembler
            .reloc_sec(self.state.section, sym, kind, off, addend)
    }
    pub fn remove_prologue_bytes(&mut self, start: u32, size: u32) -> PgResult<()> {
        let end = start
            .checked_add(size)
            .ok_or_else(|| invalid("prologue range overflow"))?;
        ensure(
            start >= self.state.func_begin && end as usize <= self.offset(),
            "prologue range outside function",
        )?;
        ensure(
            self.state.label_skew == 0,
            "multiple prologue removals unsupported",
        )?;
        ensure(
            self.state.label_offsets.iter().all(|&off| off >= end),
            "label in removed prologue",
        )?;
        if size == 0 {
            return Ok(());
        }
        self.ensure_owned()?;
        let cursor = self.state.cursor;
        ensure(
            cursor <= self.state.text.len(),
            "text cursor exceeds initialized data",
        )?;
        self.state
            .text
            .copy_within(end as usize..cursor, start as usize);
        self.state.text.truncate(cursor - size as usize);
        self.assembler
            .get_section_mut(self.state.section)?
            .adjust_relocation_offsets(self.state.reloc_begin, size);
        self.state.cursor -= size as usize;
        self.state.label_skew += size;
        Ok(())
    }
    pub fn label_count(&self) -> usize {
        self.state.label_offsets.len()
    }
    pub fn label_create(&mut self) -> PgResult<Label> {
        let label = Label(offset32(self.state.label_offsets.len())?);
        self.state.label_offsets.push(u32::MAX)?;
        Ok(label)
    }
    pub fn label_is_pending(&self, label: Label) -> PgResult<bool> {
        self.state
            .label_offsets
            .get(label.0 as usize)
            .map(|&off| off == u32::MAX)
            .ok_or_else(|| invalid("invalid function label"))
    }
    pub fn label_offset(&self, label: Label) -> PgResult<u32> {
        ensure(!self.label_is_pending(label)?, "unplaced function label")?;
        self.state.label_offsets[label.0 as usize]
            .checked_sub(self.state.label_skew)
            .ok_or_else(|| invalid("label offset precedes prologue skew"))
    }
    pub fn label_place(&mut self, label: Label, off: u32) -> PgResult<()> {
        ensure(
            self.state.label_skew == 0,
            "cannot place label after prologue removal",
        )?;
        ensure(
            self.label_is_pending(label)?,
            "function label already placed",
        )?;
        self.state.label_offsets[label.0 as usize] = off;
        Ok(())
    }
    pub fn label_ref(&mut self, label: Label, off: u32, kind: LabelFixupKind) -> PgResult<()> {
        self.label_is_pending(label)?;
        self.state
            .label_fixups
            .push(LabelFixup { label, off, kind })
    }
    pub fn alloc_jump_table(&mut self, size: u32, idx: Reg, tmp: Reg) -> PgResult<JumpTable<'_>> {
        let off = offset32(self.offset())?;
        let bytes = add(
            mem::size_of::<JumpTableHeader>(),
            (size as usize)
                .checked_mul(mem::size_of::<Label>())
                .ok_or_else(|| invalid("jump table size overflow"))?,
        )?;
        self.state
            .jump_tables
            .reserve(add(self.state.jump_tables.len(), 1)?)?;
        let allocation = self
            .state
            .jump_table_alloc
            .allocate(bytes, mem::align_of::<JumpTableHeader>())?;
        let pointer = allocation.as_mut_ptr().cast::<JumpTableHeader>();
        // SAFETY: bump allocation is aligned and large enough for this header and its trailing labels.
        unsafe {
            pointer.write(JumpTableHeader {
                size,
                off,
                idx,
                tmp,
                misc: 0,
            });
            let labels = pointer.add(1).cast::<Label>();
            for index in 0..size as usize {
                labels.add(index).write(Label(0));
            }
        }
        let index = self.state.jump_tables.len();
        self.state
            .jump_tables
            .push(NonNull::new(pointer).unwrap())?;
        self.jump_table_mut(index)
    }
    pub fn jump_table_count(&self) -> usize {
        self.state.jump_tables.len()
    }
    pub fn jump_table_header(&self, index: usize) -> PgResult<&JumpTableHeader> {
        let pointer = self
            .state
            .jump_tables
            .get(index)
            .ok_or_else(|| invalid("invalid jump table index"))?;
        // SAFETY: pointers remain live until exclusive begin_func; the borrow prevents reset or mutation.
        Ok(unsafe { pointer.as_ref() })
    }
    pub fn jump_table_labels(&self, index: usize) -> PgResult<&[Label]> {
        let pointer = self
            .state
            .jump_tables
            .get(index)
            .ok_or_else(|| invalid("invalid jump table index"))?
            .as_ptr();
        // SAFETY: the original allocation pointer covers header and labels; exclusive reset is blocked by this borrow.
        Ok(unsafe {
            core::slice::from_raw_parts(pointer.add(1).cast::<Label>(), (*pointer).size as usize)
        })
    }
    pub fn jump_table_mut(&mut self, index: usize) -> PgResult<JumpTable<'_>> {
        let pointer = self
            .state
            .jump_tables
            .get(index)
            .ok_or_else(|| invalid("invalid jump table index"))?
            .as_ptr();
        // SAFETY: exclusive writer borrow excludes all table borrows and reset; header and labels are disjoint.
        unsafe {
            let size = (*pointer).size as usize;
            let labels = core::slice::from_raw_parts_mut(pointer.add(1).cast::<Label>(), size);
            Ok(JumpTable {
                header: &mut *pointer,
                labels,
            })
        }
    }
    pub fn get_jump_table_section(&mut self) -> PgResult<SecRef> {
        self.assembler.get_default_section(SectionKind::ReadOnly)
    }
    pub fn eh_writer(&mut self) -> PgResult<EhWriterGuard<'_, 'm>> {
        let writer = VectorWriter::at(
            &mut self.assembler.get_section_mut(self.state.eh_section)?.data,
            self.state.eh_cursor,
        )?;
        Ok(EhWriterGuard {
            writer,
            cursor: &mut self.state.eh_cursor,
        })
    }
    pub fn eh_size(&self) -> usize {
        self.state.eh_cursor
    }
    pub fn eh_data(&self) -> PgResult<&[u8]> {
        Ok(&self.assembler.get_section(self.state.eh_section)?.data)
    }
    pub fn eh_data_mut(&mut self) -> PgResult<&mut [u8]> {
        Ok(&mut self.assembler.get_section_mut(self.state.eh_section)?.data)
    }
    pub const fn encode_eh_inst1(opcode: u8, arg: u64) -> ([u8; 11], usize) {
        let mut bytes = [0; 11];
        if opcode & dwarf::DWARF_CFI_PRIMARY_OPCODE_MASK != 0 {
            tpde_assert!(arg & dwarf::DWARF_CFI_PRIMARY_OPCODE_MASK as u64 == 0);
            bytes[0] = opcode | arg as u8;
            return (bytes, 1);
        }
        bytes[0] = opcode;
        let mut rest = arg;
        let mut len = 1;
        loop {
            let byte = rest as u8 & 127;
            rest >>= 7;
            bytes[len] = byte | if rest != 0 { 128 } else { 0 };
            len += 1;
            if rest == 0 {
                return (bytes, len);
            }
        }
    }
    pub const fn encode_eh_inst2(opcode: u8, first: u64, second: u64) -> ([u8; 21], usize) {
        let (prefix, prefix_len) = Self::encode_eh_inst1(opcode, first);
        let mut bytes = [0; 21];
        let mut len = 0;
        while len < prefix_len {
            bytes[len] = prefix[len];
            len += 1;
        }
        let mut rest = second;
        loop {
            let byte = rest as u8 & 127;
            rest >>= 7;
            bytes[len] = byte | if rest != 0 { 128 } else { 0 };
            len += 1;
            if rest == 0 {
                return (bytes, len);
            }
        }
    }
    pub fn write_eh_inst1(dst: &mut [u8], opcode: u8, arg: u64) -> PgResult<usize> {
        ensure(!dst.is_empty(), "CFI output buffer exhausted")?;
        if opcode & dwarf::DWARF_CFI_PRIMARY_OPCODE_MASK != 0 {
            ensure(
                arg & dwarf::DWARF_CFI_PRIMARY_OPCODE_MASK as u64 == 0,
                "invalid primary CFI operand",
            )?;
            dst[0] = opcode | arg as u8;
            Ok(1)
        } else {
            ensure(
                dst.len() >= 1 + misc::uleb_len(arg),
                "CFI output buffer exhausted",
            )?;
            dst[0] = opcode;
            Ok(1 + misc::uleb_write(&mut dst[1..], arg)?)
        }
    }
    pub fn write_eh_inst2(dst: &mut [u8], opcode: u8, first: u64, second: u64) -> PgResult<usize> {
        let mut bytes = [0; 21];
        let first_len = Self::write_eh_inst1(&mut bytes, opcode, first)?;
        let len = first_len + misc::uleb_write(&mut bytes[first_len..], second)?;
        ensure(dst.len() >= len, "CFI output buffer exhausted")?;
        dst[..len].copy_from_slice(&bytes[..len]);
        Ok(len)
    }
    pub fn eh_align_frame(&mut self) -> PgResult<()> {
        let count = self.state.eh_cursor.wrapping_neg() & 3;
        if count != 0 {
            self.eh_writer()?.zero(count)?;
        }
        Ok(())
    }
    pub fn eh_write_inst(&mut self, opcode: u8) -> PgResult<()> {
        self.eh_writer()?.write_int(opcode)
    }
    pub fn eh_write_inst1(&mut self, opcode: u8, arg: u64) -> PgResult<()> {
        let mut bytes = [0; 11];
        let len = Self::write_eh_inst1(&mut bytes, opcode, arg)?;
        self.eh_writer()?.write(&bytes[..len])
    }
    pub fn eh_write_inst2(&mut self, opcode: u8, first: u64, second: u64) -> PgResult<()> {
        let mut bytes = [0; 21];
        let len = Self::write_eh_inst2(&mut bytes, opcode, first, second)?;
        self.eh_writer()?.write(&bytes[..len])
    }
    pub fn eh_advance_raw(&mut self, units: u64) -> PgResult<()> {
        ensure(units <= u32::MAX as u64, "CFI advance beyond 32 bits")?;
        let mut writer = self.eh_writer()?;
        writer.reserve(5)?;
        if units < 0x40 {
            writer.write_int_unchecked(dwarf::DW_CFA_advance_loc | units as u8)?;
        } else if units <= u8::MAX as u64 {
            writer.write_int_unchecked(dwarf::DW_CFA_advance_loc1)?;
            writer.write_int_unchecked(units as u8)?;
        } else if units <= u16::MAX as u64 {
            writer.write_int_unchecked(dwarf::DW_CFA_advance_loc2)?;
            writer.write_int_unchecked(units as u16)?;
        } else {
            writer.write_int_unchecked(dwarf::DW_CFA_advance_loc4)?;
            writer.write_int_unchecked(units as u32)?;
        }
        Ok(())
    }
    pub fn eh_advance(&mut self, size: u64) -> PgResult<()> {
        self.eh_advance_raw(size)
    }
    fn eh_init_cie(&mut self, personality: SymRef) -> PgResult<()> {
        ensure(self.state.eh_cursor % 4 == 0, "unaligned CIE")?;
        ensure(
            self.cie.code_alignment_factor < 128 && self.cie.data_alignment_factor < 128,
            "CIE alignment factors require one-byte LEB",
        )?;
        let start = offset32(self.state.eh_cursor)?;
        let capacity = add(32, self.cie.instrs.len())?;
        let end = add(start as usize, capacity)?;
        offset32(end)?;
        self.assembler
            .get_section_mut(self.state.eh_section)?
            .data
            .reserve(end)?;
        if personality.valid() {
            self.assembler.symbol(personality)?;
            self.assembler
                .reserve_relocations(self.state.eh_section, 1)?;
        }
        let cie = self.cie;
        let mut writer = self.eh_writer()?;
        writer.reserve(capacity)?;
        writer.write_int_unchecked(0u32)?;
        writer.write_int_unchecked(0u32)?;
        writer.write_int_unchecked(1u8)?;
        writer.write_unchecked(if personality.valid() {
            b"zPLR\0"
        } else {
            b"zR\0"
        })?;
        writer.write_int_unchecked(cie.code_alignment_factor)?;
        writer.write_int_unchecked(cie.data_alignment_factor)?;
        writer.write_int_unchecked(cie.return_addr_register)?;
        writer.write_int_unchecked(if personality.valid() { 7u8 } else { 1u8 })?;
        let mut personality_offset = 0;
        if personality.valid() {
            writer.write_int_unchecked(
                dwarf::DW_EH_PE_pcrel | dwarf::DW_EH_PE_sdata4 | dwarf::DW_EH_PE_indirect,
            )?;
            personality_offset = offset32(writer.size())?;
            writer.write_int_unchecked(0u32)?;
            writer.write_int_unchecked(dwarf::DW_EH_PE_pcrel | dwarf::DW_EH_PE_sdata4)?;
        }
        writer.write_int_unchecked(dwarf::DW_EH_PE_sdata4 | dwarf::DW_EH_PE_pcrel)?;
        writer.write_unchecked(cie.instrs)?;
        let pad = writer.size().wrapping_neg() & 3;
        writer.zero_unchecked(pad)?;
        let size = offset32(writer.size() - start as usize - 4)?;
        writer.data_mut()?[start as usize..start as usize + 4].copy_from_slice(&size.to_ne_bytes());
        drop(writer);
        if personality.valid() {
            self.assembler
                .reloc_pc32(self.state.eh_section, personality, personality_offset, 0)?;
        }
        self.state.cie_offset = start;
        self.state.fde_start = start;
        Ok(())
    }
    pub fn eh_begin_fde(&mut self, personality: SymRef) -> PgResult<()> {
        ensure(!self.state.fde_open, "previous FDE still open")?;
        if personality != self.state.personality {
            self.eh_init_cie(personality)?;
            self.state.personality = personality;
        }
        ensure(self.state.eh_cursor % 4 == 0, "unaligned FDE")?;
        let start = offset32(self.state.eh_cursor)?;
        let id = start
            .checked_sub(self.state.cie_offset)
            .and_then(|n| n.checked_add(4))
            .ok_or_else(|| invalid("FDE CIE offset overflow"))?;
        let mut writer = self.eh_writer()?;
        writer.zero(if personality.valid() { 21 } else { 17 })?;
        let data = writer.data_mut()?;
        data[start as usize + 4..start as usize + 8].copy_from_slice(&id.to_ne_bytes());
        if personality.valid() {
            data[start as usize + 16] = 4;
        }
        drop(writer);
        self.state.fde_start = start;
        self.state.fde_open = true;
        Ok(())
    }
    pub fn eh_end_fde(&mut self) -> PgResult<()> {
        ensure(self.state.fde_open, "no open FDE")?;
        let size = offset32(self.offset())?
            .checked_sub(self.state.func_begin)
            .ok_or_else(|| invalid("function ends before begin"))?;
        ensure(
            self.state.func_begin <= i32::MAX as u32,
            "FDE function addend exceeds i32",
        )?;
        let symbol = self.assembler.section_symbol(self.state.section)?;
        self.assembler
            .reserve_relocations(self.state.eh_section, 1)?;
        let pad = self.state.eh_cursor.wrapping_neg() & 3;
        self.assembler
            .get_section_mut(self.state.eh_section)?
            .data
            .reserve(add(self.state.eh_cursor, pad)?)?;
        self.eh_align_frame()?;
        let start = self.state.fde_start as usize;
        let len = offset32(self.state.eh_cursor - start - 4)?;
        let data = self.eh_data_mut()?;
        ensure(start + 16 <= data.len(), "truncated FDE")?;
        data[start..start + 4].copy_from_slice(&len.to_ne_bytes());
        data[start + 12..start + 16].copy_from_slice(&size.to_ne_bytes());
        self.assembler.reloc_pc32(
            self.state.eh_section,
            symbol,
            self.state.fde_start + 8,
            self.state.func_begin as i64,
        )?;
        self.state.fde_open = false;
        Ok(())
    }
    pub fn end_module(&mut self) -> PgResult<()> {
        ensure(
            self.state.cursor == self.state.reserved_end,
            "flush text before ending module",
        )?;
        ensure(!self.state.fde_open, "cannot end module with open FDE")?;
        ensure(self.state.eh_cursor % 4 == 0, "unaligned EH frame section")?;
        if self.state.eh_cursor & 7 != 0 {
            let start = self.state.fde_start as usize;
            let data = self.eh_data()?;
            ensure(start + 4 <= data.len(), "missing final CIE or FDE")?;
            let len = u32::from_ne_bytes(data[start..start + 4].try_into().unwrap())
                .checked_add(4)
                .ok_or_else(|| invalid("EH record length overflow"))?;
            self.eh_writer()?.zero(4)?;
            self.eh_data_mut()?[start..start + 4].copy_from_slice(&len.to_ne_bytes());
        }
        self.eh_writer()?.flush();
        Ok(())
    }
    pub fn except_add_call_site(
        &mut self,
        text_off: u32,
        len: u32,
        landing_pad: Label,
        is_cleanup: bool,
    ) -> PgResult<()> {
        self.label_is_pending(landing_pad)?;
        if let Some(last) = self.state.call_sites.last() {
            ensure(
                last.start < text_off as u64,
                "exception call sites must be strictly increasing",
            )?;
        }
        let action_entry = if is_cleanup {
            0
        } else {
            offset32(add(self.state.actions.len(), 1)?)?
        };
        self.state.call_sites.push(ExceptCallSiteInfo {
            start: text_off as u64,
            len: len as u64,
            landing_pad,
            action_entry,
        })
    }
    pub fn except_add_cleanup_action(&mut self) -> PgResult<()> {
        ensure(
            !self.state.actions.is_empty(),
            "cleanup action requires preceding action offset",
        )?;
        let len = self.state.actions.len() - 1;
        let offset = i64::try_from(len).map_err(|_| invalid("exception action offset overflow"))?;
        self.state.actions.reserve(add(len, 10)?)?;
        self.state.actions.truncate(len);
        VectorWriter::new(&mut self.state.actions).write_sleb(-offset)
    }
    pub fn except_add_action(&mut self, first_action: bool, type_sym: SymRef) -> PgResult<()> {
        ensure(
            first_action || !self.state.actions.is_empty(),
            "no preceding exception action",
        )?;
        if type_sym.valid() {
            self.assembler.symbol(type_sym)?;
        }
        let (idx, append) = if !type_sym.valid() {
            (0, false)
        } else if let Some(index) = self
            .state
            .type_info
            .iter()
            .position(|&s| s == type_sym.id())
        {
            (offset32(index + 1)?, false)
        } else {
            (offset32(self.state.type_info.len() + 1)?, true)
        };
        self.state
            .actions
            .reserve(add(self.state.actions.len(), 10)?)?;
        if append {
            self.state
                .type_info
                .reserve(add(self.state.type_info.len(), 1)?)?;
        }
        if !first_action {
            *self.state.actions.last_mut().unwrap() = 1;
        }
        if append {
            self.state.type_info.push(type_sym.id())?;
        }
        VectorWriter::new(&mut self.state.actions).write_sleb(idx as i64 + 1)?;
        self.state.actions.push(0)
    }
    pub fn except_add_empty_spec_action(&mut self, first_action: bool) -> PgResult<()> {
        ensure(
            first_action || !self.state.actions.is_empty(),
            "no preceding exception action",
        )?;
        self.state
            .actions
            .reserve(add(self.state.actions.len(), 2)?)?;
        self.state.specs.reserve(4)?;
        if !first_action {
            *self.state.actions.last_mut().unwrap() = 1;
        }
        if self.state.specs.is_empty() {
            self.state.specs.resize_bytes(4)?;
        }
        self.state.actions.extend_from_slice(&[127, 0])
    }
    pub fn except_type_idx_for_sym(&self, sym: SymRef) -> PgResult<u32> {
        self.state
            .type_info
            .iter()
            .position(|&s| s == sym.id())
            .map(|index| (index + 2) as u32)
            .ok_or_else(|| invalid("exception type symbol not registered"))
    }
    pub fn except_encode_func(&mut self) -> PgResult<()> {
        if !self.state.personality.valid() {
            return Ok(());
        }
        let fn_end = self.offset() as u64;
        let fn_begin = self.state.func_begin as u64;
        let mut cur = fn_begin;
        for info in &self.state.call_sites[..] {
            let start = info
                .start
                .checked_sub(self.state.label_skew as u64)
                .ok_or_else(|| invalid("call site precedes prologue skew"))?;
            ensure(
                start >= fn_begin && start.checked_add(info.len).is_some_and(|end| end <= fn_end),
                "call site outside function",
            )?;
            let landing = self.label_offset(info.landing_pad)? as u64;
            ensure(
                landing >= fn_begin && landing < fn_end,
                "landing pad outside function",
            )?;
        }
        let reserve = add(
            self.state
                .call_sites
                .len()
                .checked_mul(16)
                .ok_or_else(|| invalid("call site table overflow"))?,
            40,
        )?;
        self.state.encoded_call_sites.reserve(reserve)?;
        self.state.encoded_call_sites.clear();
        for index in 0..self.state.call_sites.len() {
            let info = self.state.call_sites[index];
            let start = info.start - self.state.label_skew as u64;
            let landing = self.label_offset(info.landing_pad)? as u64 - fn_begin;
            let mut writer = VectorWriter::new(&mut self.state.encoded_call_sites);
            writer.reserve(80)?;
            if start > cur {
                writer.write_uleb_unchecked(cur - fn_begin)?;
                writer.write_uleb_unchecked(start - cur)?;
                writer.write_uleb_unchecked(0)?;
                writer.write_uleb_unchecked(0)?;
            }
            writer.write_uleb_unchecked(start - fn_begin)?;
            writer.write_uleb_unchecked(info.len)?;
            writer.write_uleb_unchecked(landing)?;
            writer.write_uleb_unchecked(info.action_entry as u64)?;
            cur = start + info.len;
        }
        {
            let mut writer = VectorWriter::new(&mut self.state.encoded_call_sites);
            writer.reserve(42)?;
            if cur < fn_end {
                writer.write_uleb_unchecked(cur - fn_begin)?;
                writer.write_uleb_unchecked(fn_end - cur)?;
                writer.write_uleb_unchecked(0)?;
                writer.write_uleb_unchecked(0)?;
            }
            writer.write_unchecked(&[0, 0])?;
        }
        let lsda = self.assembler.get_default_section(SectionKind::LSDA)?;
        let lsda_start = offset32(self.assembler.get_section(lsda)?.data.len())?;
        ensure(lsda_start <= i32::MAX as u32, "LSDA addend exceeds i32")?;
        let table_bytes = add(self.state.type_info.len(), 1)?
            .checked_mul(4)
            .ok_or_else(|| invalid("exception type table overflow"))?;
        let class_offset = add(
            add(table_bytes, self.state.actions.len())?,
            add(
                self.state.encoded_call_sites.len(),
                misc::uleb_len(self.state.encoded_call_sites.len() as u64) + 1,
            )?,
        )?;
        let total = add(
            add(
                3 + misc::uleb_len(class_offset as u64),
                misc::uleb_len(self.state.encoded_call_sites.len() as u64),
            )?,
            add(
                add(
                    self.state.encoded_call_sites.len(),
                    self.state.actions.len(),
                )?,
                add(table_bytes, self.state.specs.len())?,
            )?,
        )?;
        let final_end = add(lsda_start as usize, total)?;
        offset32(final_end)?;
        self.assembler
            .get_section_mut(lsda)?
            .data
            .reserve(final_end)?;
        self.assembler
            .reserve_relocations(lsda, self.state.type_info.len())?;
        self.assembler
            .reserve_relocations(self.state.eh_section, 1)?;
        let symbol = self.assembler.section_symbol(lsda)?;
        let type_end;
        {
            let mut writer = VectorWriter::new(&mut self.assembler.get_section_mut(lsda)?.data);
            writer.reserve(total)?;
            writer.write_int_unchecked(dwarf::DW_EH_PE_omit)?;
            if self.state.actions.is_empty() {
                ensure(
                    self.state.type_info.is_empty(),
                    "type table without exception actions",
                )?;
                writer.write_int_unchecked(dwarf::DW_EH_PE_omit)?;
            } else {
                writer.write_int_unchecked(
                    dwarf::DW_EH_PE_sdata4 | dwarf::DW_EH_PE_pcrel | dwarf::DW_EH_PE_indirect,
                )?;
                writer.write_uleb_unchecked(class_offset as u64)?;
            }
            writer.write_int_unchecked(dwarf::DW_EH_PE_uleb128)?;
            writer.write_uleb_unchecked(self.state.encoded_call_sites.len() as u64)?;
            writer.write_unchecked(&self.state.encoded_call_sites)?;
            writer.write_unchecked(&self.state.actions)?;
            if !self.state.actions.is_empty() {
                writer.zero_unchecked(table_bytes)?;
            }
            type_end = writer.size();
            if !self.state.actions.is_empty() {
                writer.write_unchecked(&self.state.specs)?;
            }
        }
        if !self.state.actions.is_empty() {
            for (index, &sym) in self.state.type_info.iter().enumerate() {
                self.assembler.reloc_pc32(
                    lsda,
                    SymRef::new(sym),
                    offset32(type_end - 8 - index * 4)?,
                    0,
                )?;
            }
        }
        self.assembler.reloc_pc32(
            self.state.eh_section,
            symbol,
            self.state
                .fde_start
                .checked_add(17)
                .ok_or_else(|| invalid("FDE LSDA offset overflow"))?,
            lsda_start as i64,
        )
    }
}

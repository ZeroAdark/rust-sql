//! The build slot's row store (M7 decision 1, form A): the rows a hash build's entries point at, one bump slice per partition, carved from the top of the build's region; a row is `spool::encode_row`'s image (its first word the row's length in words), so a store is position-independent and an entry's payload is the row's byte offset from the rows base. The store's offset from the build header rides `HashBuild.pad2` (0: no store, the legacy inline payload).
use core::mem::size_of;

use crate::breaker::{HashBuild, HASHBUILD_ENTRY};

#[repr(C)]
pub struct RowStore {
    pub len: u32,
    pub cap: u32,
    pub parts: u16,
    /// The per-row estimate the region was split by (`split`), so a merge lays the leader's store out alike.
    pub row_hdr: u16,
}

pub const ROWS_NONE: u32 = 0;
pub const ROWSTORE_HDR: usize = 16;
/// Every slice starts with its own rows-base offset, so a spilled slice carries the origin its offsets are relative to.
pub const SLICE_HDR: usize = 8;
/// A carve's cut (the store's start = the entry table's end) is a whole number of entries, so the table below it
/// holds no partial entry; an entry (16 B) also covers the store header's 8-byte alignment (`open`).
const ALIGN: usize = HASHBUILD_ENTRY;
const _: () = assert!(ALIGN % 8 == 0);

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Slice {
    start: u32,
    used: u32,
}

const fn round_up(n: usize, a: usize) -> usize {
    (n + a - 1) / a * a
}

impl RowStore {
    pub const fn dir_end(parts: u16) -> usize {
        round_up(ROWSTORE_HDR + parts as usize * size_of::<Slice>(), 8)
    }

    /// The bytes a store of `cap_rows` rows of `row_bytes` in `parts` slices needs.
    pub const fn bytes(cap_rows: u32, row_bytes: u32, parts: u16) -> usize {
        RowStore::dir_end(parts) + parts as usize * SLICE_HDR + round_up(cap_rows as usize * row_bytes as usize, 8)
    }

    /// The store's share of a build region of `region` bytes whose rows are `row_bytes` each beside a 16-byte entry at up to half load; 0 = no store.
    pub const fn split(region: u32, row_bytes: u32) -> u32 {
        if row_bytes == 0 {
            return 0;
        }
        let s = (region as u64 * row_bytes as u64 / (row_bytes as u64 + 2 * HASHBUILD_ENTRY as u64)) as u32;
        s / ALIGN as u32 * ALIGN as u32
    }

    /// # Safety
    /// `p` points at `bytes` writable bytes, 8-aligned, the header zeroed or a live store of the same shape.
    pub unsafe fn open<'a>(p: *mut u8, bytes: usize, parts: u16, row_hdr: u16) -> Option<&'a mut RowStore> {
        let s = crate::breaker::aligned_mut::<RowStore>(p);
        if s.cap != ROWS_NONE {
            return (s.parts == parts).then_some(s);
        }
        let dir = RowStore::dir_end(parts);
        if parts == 0 || bytes < dir + parts as usize * SLICE_HDR {
            return None;
        }
        let slice = (bytes - dir) / parts as usize / 8 * 8;
        s.cap = (slice * parts as usize) as u32;
        s.parts = parts;
        s.row_hdr = row_hdr;
        s.len = 0;
        for i in 0..parts as usize {
            let start = (i * slice) as u32;
            *s.dir_mut().add(i) = Slice { start, used: SLICE_HDR as u32 };
            *(s.rows_mut().add(start as usize) as *mut u64) = start as u64;
        }
        Some(s)
    }

    /// # Safety
    /// `p` points at a store `open` laid out.
    pub unsafe fn view<'a>(p: *const u8) -> &'a RowStore {
        crate::breaker::aligned_ref::<RowStore>(p)
    }

    /// The store of the build at `build` (the offset in `pad2`); `None` for the legacy form.
    /// # Safety
    /// `build` points at a live or share-armed build region.
    pub unsafe fn of<'a>(build: *const u8) -> Option<&'a RowStore> {
        let off = (*(build as *const HashBuild)).pad2;
        if off == 0 {
            return None;
        }
        let s = RowStore::view(build.add(off as usize));
        (s.cap != ROWS_NONE).then_some(s)
    }

    /// # Safety
    /// As `of`, the region writable.
    pub unsafe fn of_mut<'a>(build: *mut u8) -> Option<&'a mut RowStore> {
        let off = (*(build as *const HashBuild)).pad2;
        if off == 0 {
            return None;
        }
        let s = crate::breaker::aligned_mut::<RowStore>(build.add(off as usize));
        (s.cap != ROWS_NONE).then_some(s)
    }

    /// Carve a store of `split`'s share out of the top of the build region at `build` — share-armed (the arm word
    /// reduced) or laid out with room past its entries (`region` reduced) — and lay it out; `None` = no room.
    /// # Safety
    /// `build` points at a share-armed or live build over its whole region.
    pub unsafe fn carve<'a>(build: *mut u8, row_bytes: u32, parts: u16) -> Option<&'a mut RowStore> {
        let b = crate::breaker::aligned_mut::<HashBuild>(build);
        if b.pad2 != 0 {
            return RowStore::of_mut(build);
        }
        let (armed, total) = if b.cap == 0 { (true, b.table) } else { (false, b.region) };
        let store = RowStore::split(total, row_bytes);
        let off = total.saturating_sub(store) as usize / ALIGN * ALIGN;
        if store == 0 || off < ROWSTORE_HDR || (!armed && HashBuild::bytes(b.cap) as usize > off) {
            return None;
        }
        // fresh carve: a recycled region's tail may hold an earlier statement's store, never `open` it (D103)
        core::ptr::write_bytes(build.add(off), 0, ROWSTORE_HDR);
        let s = RowStore::open(build.add(off), total as usize - off, parts, row_bytes.min(u16::MAX as u32) as u16)?;
        if armed {
            b.table = off as u32;
        } else {
            b.region = off as u32;
        }
        b.pad2 = off as u32;
        Some(s)
    }

    pub fn bits(&self) -> u8 {
        self.parts.trailing_zeros() as u8
    }

    /// The slice an entry keyed `key` belongs to: the entry's spill partition at the store's bits.
    pub fn part_for(&self, key: i32) -> u16 {
        HashBuild::partition_of(key, self.bits()) as u16
    }

    #[inline(always)]
    fn dir(&self) -> *const Slice {
        unsafe { (self as *const RowStore as *const u8).add(ROWSTORE_HDR) as *const Slice }
    }

    #[inline(always)]
    fn rows(&self) -> *const u8 {
        unsafe { (self as *const RowStore as *const u8).add(RowStore::dir_end(self.parts)) }
    }

    /// The directory and rows for a writer, from `&mut self` (D1358: a write through `dir` / `rows`' pointer, a shared
    /// borrow of the store, is undefined behaviour the optimizer may act on).
    #[inline(always)]
    fn dir_mut(&mut self) -> *mut Slice {
        unsafe { (self as *mut RowStore as *mut u8).add(ROWSTORE_HDR) as *mut Slice }
    }

    #[inline(always)]
    fn rows_mut(&mut self) -> *mut u8 {
        unsafe { (self as *mut RowStore as *mut u8).add(RowStore::dir_end(self.parts)) }
    }

    #[inline(always)]
    fn slice_bytes(&self) -> usize {
        self.cap as usize / self.parts as usize
    }

    #[inline(always)]
    fn slice(&self, part: u16) -> Slice {
        debug_assert!(part < self.parts);
        unsafe { *self.dir().add(part as usize) }
    }

    /// Append `row` (8-byte multiple) to slice `part`; the offset the entry's payload carries, `None` = the slice is full.
    #[inline(always)]
    pub fn append(&mut self, part: u16, row: &[u8]) -> Option<u32> {
        let s = self.slice(part);
        if s.used as usize + row.len() > self.slice_bytes() {
            return None;
        }
        let off = s.start + s.used;
        unsafe {
            core::ptr::copy_nonoverlapping(row.as_ptr(), self.rows_mut().add(off as usize), row.len());
            (*self.dir_mut().add(part as usize)).used += row.len() as u32;
        }
        self.len += 1;
        Some(off)
    }

    #[inline(always)]
    pub fn at(&self, off: u32) -> &[u8] {
        debug_assert!((off as usize) < self.cap as usize);
        unsafe {
            let p = self.rows().add(off as usize);
            core::slice::from_raw_parts(p, (*(p as *const u64)) as usize * 8)
        }
    }

    #[inline(always)]
    pub fn at_ptr(&self, off: u32) -> *const u8 {
        unsafe { self.rows().add(off as usize) }
    }

    /// Slice `part`'s bytes `(lo, hi)` as rows offsets, the slice header included (its first word is `lo`).
    pub fn part_range(&self, part: u16) -> (u32, u32) {
        let s = self.slice(part);
        (s.start, s.start + s.used)
    }

    pub fn part_bytes(&self, part: u16) -> &[u8] {
        let (lo, hi) = self.part_range(part);
        unsafe { core::slice::from_raw_parts(self.rows().add(lo as usize), (hi - lo) as usize) }
    }

    /// Raw bytes onto slice `part` (a spilled image streamed in chunks); `None` = no room.
    pub fn append_raw(&mut self, part: u16, bytes: &[u8]) -> Option<()> {
        let s = self.slice(part);
        if s.used as usize + bytes.len() > self.slice_bytes() {
            return None;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), self.rows_mut().add((s.start + s.used) as usize), bytes.len());
            (*self.dir_mut().add(part as usize)).used += bytes.len() as u32;
        }
        Some(())
    }

    /// Append a spilled or merged slice image (`part_bytes`'s, its origin word first) to slice `part`; the delta
    /// every entry offset of that image takes, `None` = no room.
    pub fn append_slice(&mut self, part: u16, image: &[u8]) -> Option<u32> {
        if image.len() < SLICE_HDR {
            return Some(0);
        }
        let origin = u64::from_ne_bytes(image[..SLICE_HDR].try_into().expect("a slice origin word")) as u32;
        let rows = &image[SLICE_HDR..];
        let s = self.slice(part);
        if s.used as usize + rows.len() > self.slice_bytes() {
            return None;
        }
        let at = s.start + s.used;
        unsafe {
            core::ptr::copy_nonoverlapping(rows.as_ptr(), self.rows_mut().add(at as usize), rows.len());
            (*self.dir_mut().add(part as usize)).used += rows.len() as u32;
        }
        Some(at.wrapping_sub(origin + SLICE_HDR as u32))
    }

    /// Every slice of `partial` appended to this store's, `rebase[part]` the delta its entry offsets take; false = no room (nothing appended).
    pub fn merge_from(&mut self, partial: &RowStore, rebase: &mut [u32]) -> bool {
        if partial.parts != self.parts || rebase.len() < self.parts as usize {
            return false;
        }
        for p in 0..self.parts {
            let (lo, hi) = partial.part_range(p);
            let s = self.slice(p);
            if s.used as usize + (hi - lo) as usize - SLICE_HDR > self.slice_bytes() {
                return false;
            }
        }
        for p in 0..self.parts {
            rebase[p as usize] = self.append_slice(p, partial.part_bytes(p)).expect("room checked");
        }
        self.len += partial.len;
        true
    }

    /// Room for every slice of `partial` on top of this store's.
    pub fn room_for(&self, partial: &RowStore) -> bool {
        partial.parts == self.parts && (0..self.parts).all(|p| { let (lo, hi) = partial.part_range(p); self.slice(p).used as usize + (hi - lo) as usize - SLICE_HDR <= self.slice_bytes() })
    }

    /// # Safety
    /// `this` is a live store shared by the partition jobs (`len += n` atomically).
    pub unsafe fn add_len(this: *mut RowStore, n: u32) {
        (*(core::ptr::addr_of_mut!((*this).len) as *const core::sync::atomic::AtomicU32)).fetch_add(n, core::sync::atomic::Ordering::Relaxed);
    }

    pub fn clear(&mut self) {
        for p in 0..self.parts as usize {
            unsafe { (*self.dir_mut().add(p)).used = SLICE_HDR as u32 };
        }
        self.len = 0;
    }
}

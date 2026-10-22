//! The pgrc family's raw reads (T-unsafe): every `unsafe` the family's host files need, each a named function with its
//! SAFETY text, so those files compile under the crate's `#![deny(unsafe_code)]`.
#![allow(unsafe_code)]

use gravity_abi::PtrWord;

/// An 8-aligned word buffer viewed as its bytes (the decode arenas: `ByteArena` over `u64` storage).
pub(crate) fn words_as_bytes_mut(words: &mut [u64]) -> &mut [u8] {
    // SAFETY: u8 has no alignment or validity requirement and the view covers exactly the words' bytes, borrowed
    // mutably for the view's life.
    unsafe { core::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, words.len() * 8) }
}

/// A dictionary walk's entry `v` as its bytes, borrowed while the walk's callback holds `v` (text_hosts `RenderMemo`).
pub(crate) fn walk_bytes(v: &gravity_dict::View) -> &[u8] {
    // SAFETY: a `gravity_dict::Dict::entries` walk hands its callback only views of live bytes, live until the next
    // entry; the borrow of `v` ends with the callback.
    unsafe { v.bytes() }
}

/// The varlena bind word `w`'s payload appended to `out` as its little-endian u32 length then its bytes; `None` (nothing
/// appended) for a NULL pointer, an external (TOAST) pointer or a compressed image. The caller passes only the varlena
/// bind words of the execution's parameter words.
pub(crate) fn push_bind_varlena(out: &mut Vec<u8>, w: u64) -> Option<()> {
    // SAFETY: a varlena bind word points at the statement's datum for the execution.
    let data = unsafe { varlena_data(w as *const u8) }?;
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    Some(())
}

/// NEW-72: `f` over the image the bind word `w` points at: a varlena's payload (`len` `None`: the 1-byte short and the
/// plain 4-byte forms) or `len` bytes of a fixed-length by-reference datum; `None` for a NULL pointer, an external (TOAST)
/// pointer or a compressed image. The caller passes only a by-reference literal's word of the execution (a bound zone
/// constant, an IN list element inside the execution's array).
pub(crate) fn with_bind_image<R>(w: u64, len: Option<usize>, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
    let p = w as *const u8;
    let data = match len {
        // SAFETY: a varlena bind word points at the statement's datum for the execution (or is null).
        None => unsafe { varlena_data(p) }?,
        // SAFETY: a fixed-length by-reference bind word points at `n` bytes of the statement's datum.
        Some(n) if !p.is_null() => unsafe { core::slice::from_raw_parts(p, n) },
        Some(_) => return None,
    };
    Some(f(data))
}

/// The `(block, offset)` elements of the tid[] bind word `w` (lowerv4 ops/scan/pgrc/lower.rs `tid_keys`: a leading
/// `ctid = ANY (a)`'s array): PostgreSQL's ArrayType after its varlena header (ndim, dataoffset, elemtype, then dims and
/// lbounds), one dimension and no NULL bitmap, the 6-byte elements (bi_hi, bi_lo, posid) from data byte 20 at alignment
/// 's'; empty for no dimension. `None` for a NULL word, an external or compressed image, another shape or a short image.
pub(crate) fn tid_array(w: u64) -> Option<Vec<(u32, u16)>> {
    // SAFETY: the word is a tid[] bind word: null, or the statement's datum for the execution.
    let data = unsafe { varlena_data(w as *const u8) }?;
    let word = |at: usize| data.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    match (word(0)?, word(4)?) {
        (0, _) => return Some(Vec::new()), // alloc: Exec
        (1, 0) => {}
        _ => return None,
    }
    let n = word(12)? as usize;
    let elems = data.get(20..20 + n.checked_mul(6)?)?;
    Some(elems.chunks_exact(6).map(|e| ((u16::from_le_bytes([e[0], e[1]]) as u32) << 16 | u16::from_le_bytes([e[2], e[3]]) as u32, u16::from_le_bytes([e[4], e[5]]))).collect()) // alloc: Exec
}

/// A varlena datum's data read in place (PostgreSQL varatt.h, little-endian headers): the 1-byte short and the plain
/// 4-byte forms; `None` for an external (TOAST) pointer or a compressed image.
/// # Safety — `p` is null or points at a varlena datum.
unsafe fn varlena_data<'a>(p: *const u8) -> Option<&'a [u8]> {
    if p.is_null() {
        return None;
    }
    let b0 = *p;
    if b0 == 0x01 {
        return None;
    }
    if b0 & 0x01 == 0x01 {
        let len = (b0 >> 1) as usize;
        return Some(core::slice::from_raw_parts(p.add(1), len.checked_sub(1)?));
    }
    let h = u32::from_le_bytes([b0, *p.add(1), *p.add(2), *p.add(3)]);
    if h & 0x03 != 0 {
        return None;
    }
    Some(core::slice::from_raw_parts(p.add(4), ((h >> 2) as usize).checked_sub(4)?))
}

/// Whole u64 words viewed as their bytes (records handed to a byte sink).
pub(crate) fn words_as_bytes(words: &[u64]) -> &[u8] {
    // SAFETY: u8 has no alignment or validity requirement and the view covers exactly the words' bytes.
    unsafe { core::slice::from_raw_parts(words.as_ptr() as *const u8, words.len() * 8) }
}

/// A table's entry runs by address (entry_runs.rs): its records and its bucket offsets, flat, so a partition's walk over
/// many parts reads these and not each table's header. The runs' owner keeps the buffers alive (and unchanged) for as
/// long as it holds the span; `words` borrows from that owner.
#[derive(Clone, Copy)]
pub(crate) struct RunSpan {
    recs: *const u64,
    nrecs: usize,
    off: *const u32,
    noff: usize,
}

// SAFETY: a span views an immutable buffer its owner holds; it is read, never written.
unsafe impl Send for RunSpan {}
unsafe impl Sync for RunSpan {}

impl RunSpan {
    /// The span of `recs` and `off`; the caller keeps both buffers alive and unchanged while it holds the span.
    pub(crate) fn new(recs: &[u64], off: &[u32]) -> RunSpan {
        RunSpan { recs: recs.as_ptr(), nrecs: recs.len(), off: off.as_ptr(), noff: off.len() }
    }

    /// Offset `b`.
    #[inline(always)]
    pub(crate) fn at(self, b: usize) -> usize {
        assert!(b < self.noff);
        // SAFETY: `b` is inside the offsets, which the owner keeps alive.
        unsafe { *self.off.add(b) as usize }
    }

    /// Words `lo..hi` of the records, borrowed for the life of `owner` (the holder of the buffers).
    #[inline(always)]
    pub(crate) fn words<'a, T: ?Sized>(self, _owner: &'a T, lo: usize, hi: usize) -> &'a [u64] {
        assert!(lo <= hi && hi <= self.nrecs);
        // SAFETY: `lo..hi` is inside the records, which `owner` keeps alive and unchanged for `'a`.
        unsafe { core::slice::from_raw_parts(self.recs.add(lo), hi - lo) }
    }

    /// The address of word `lo` (a prefetch hint; never dereferenced here).
    #[inline(always)]
    pub(crate) fn addr(self, lo: usize) -> *const u8 {
        self.recs.wrapping_add(lo) as *const u8
    }
}

/// A stack gather of up to `N` words (uninitialised past `len`), handed to a byte sink as one run.
pub(crate) struct Gather<const N: usize> {
    buf: [core::mem::MaybeUninit<u64>; N],
    n: usize,
}

impl<const N: usize> Gather<N> {
    pub(crate) fn new() -> Self {
        Gather { buf: [core::mem::MaybeUninit::uninit(); N], n: 0 }
    }

    pub(crate) fn len(&self) -> usize {
        self.n
    }

    /// Append `w` (`len() + w.len() <= N`).
    #[inline(always)]
    pub(crate) fn push(&mut self, w: &[u64]) {
        assert!(w.len() <= N - self.n);
        // SAFETY: `n + w.len() <= N` words of `buf`; the source is a whole slice.
        unsafe { core::ptr::copy_nonoverlapping(w.as_ptr(), self.buf.as_mut_ptr().add(self.n) as *mut u64, w.len()) };
        self.n += w.len();
    }

    /// The gathered words' bytes.
    pub(crate) fn bytes(&self) -> &[u8] {
        // SAFETY: the first `n` words were written by `push`.
        words_as_bytes(unsafe { core::slice::from_raw_parts(self.buf.as_ptr() as *const u64, self.n) })
    }

    pub(crate) fn clear(&mut self) {
        self.n = 0;
    }
}

/// The source side of kernels' `entry_runs_offer` (entry_runs.rs `install`): site `site`'s source sees the offer.
/// # Safety
/// `ctx` is bound (the claim's own, on its thread).
pub(crate) unsafe fn entry_runs_offer(ctx: *mut gravity_abi::Ctx, site: usize, layout: &gravity_abi::breaker::RecordLayout, bits: u8) {
    // SAFETY: the caller's contract.
    if let Some(s) = unsafe { gravity_driver::source::site_source_mut(ctx, site) } {
        s.entry_runs_offer(layout, bits);
    }
}

/// The first `n` u32 cells of an 8-aligned word buffer (a code lane's slot: `ROWS` datum words holding u32 codes).
pub(crate) fn words_as_u32_mut(words: &mut [u64], n: usize) -> &mut [u32] {
    assert!(n <= words.len() * 2);
    // SAFETY: u32 needs 4-byte alignment (u64 storage gives 8) and has no invalid values; `n` cells fit in the words,
    // borrowed mutably for the view's life.
    unsafe { core::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u32, n) }
}

/// M6 R3: the per-code table over a shred code lane a part did not elect: no entries, its data a zero word, so an invalid
/// row's code 0 reads in bounds (the program never answers an invalid row from a table).
struct EmptyTable(gravity_dict::table::Header);
// SAFETY: immutable; its data pointer is an immutable static's
unsafe impl Sync for EmptyTable {}
static ZERO_WORD: [u64; 2] = [0; 2];
const fn empty_header(elem: u8, data: *const u8) -> EmptyTable {
    let n = core::ptr::null();
    EmptyTable(gravity_dict::table::Header { elem, flags: 0, pad: [0; 2], ncodes: 0, faults: gravity_abi::word(core::ptr::null()), data: gravity_abi::word(data), base: gravity_abi::word(n), canon: gravity_abi::word(core::ptr::null()), hashes: gravity_abi::word(core::ptr::null()), ndistinct: 0, nfaults: 0 })
}
// ELEM_FP128: a jsonb path group key's fingerprint table (its code-0 entry the two zero words)
static EMPTY_TABLES: [EmptyTable; 3] = [empty_header(gravity_dict::lane::ELEM_BITS, ZERO_WORD.as_ptr() as *const u8), empty_header(gravity_dict::lane::ELEM_I64, ZERO_WORD.as_ptr() as *const u8), empty_header(gravity_dict::lane::ELEM_FP128, ZERO_WORD.as_ptr() as *const u8)];

/// Publishes the empty table of element kind `elem` (a shred code lane the part did not elect) as table lane `ordinal`
/// over lane `over`; `None` (nothing published) for a kind with no empty table.
pub(crate) fn publish_empty_table(elem: u8, ordinal: u32, over: u8, out: &mut gravity_abi::Lane) -> Option<()> {
    let t = EMPTY_TABLES.iter().find(|t| t.0.elem == elem)?;
    // SAFETY: a header of immutable zero words, for the process
    unsafe { gravity_dict::lane::publish(&t.0, ordinal, over, out) };
    Some(())
}

/// (2') DK1-MISEST (granule_source.rs `TableState::enter`): an unfilled adaptive Fp128 table's fill, `(filled, read)` over
/// `sample` evenly spaced entries (every entry of a smaller table); an entry is filled when its fingerprint is not `[0, 0]`
/// (a row stored it, lowerv4 percode_key.rs `cell`). `None` for a null header or one that is not an Fp128 table.
pub(crate) fn adapt_fill(h: *const gravity_dict::table::Header, sample: u32) -> Option<(u64, u64)> {
    // SAFETY: the caller passes a header its source built in its own region for the part it just left (`build_table`), whose
    // region is not reused before this read; the header is live and, for ELEM_FP128, `data` holds `ncodes` 16-byte entries
    let hdr = unsafe { h.as_ref() }?;
    if hdr.elem != gravity_dict::lane::ELEM_FP128 || hdr.data.ptr().is_null() {
        return None;
    }
    let n = hdr.ncodes;
    let read = n.min(sample);
    let mut filled = 0u64;
    // bounded("small-fixed: at most ADAPT_SAMPLE_ENTRIES (4096) table entries sampled")
    for k in 0..read {
        let code = (k as u64 * n as u64 / read.max(1) as u64) as usize;
        // SAFETY: code < ncodes, and each entry is two u64 words at `data + 16 * code` (read unaligned)
        let (lo, hi) = unsafe { (core::ptr::read_unaligned(hdr.data.ptr().add(code * 16) as *const u64), core::ptr::read_unaligned(hdr.data.ptr().add(code * 16 + 8) as *const u64)) };
        filled += (lo | hi != 0) as u64;
    }
    Some((filled, read as u64))
}

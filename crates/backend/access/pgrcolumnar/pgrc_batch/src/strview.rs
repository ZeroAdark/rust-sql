
use core::ptr::NonNull;

use ::datum::Datum;
use ::types_tuple::varatt;

use crate::arena::ClaimArena;
use crate::batch::Column;
use crate::rep::ColRep;
use crate::selection::Selection;
use crate::staging::classify_varlena;

pub const STRVIEW_INLINE_MAX: usize = 12;

const STRVIEW_HASH_SEED: u64 = u64::from_le_bytes(*b"lxstrvw1");

/// The 16-byte string-view cell (charter §2). Plain `Copy` data — the
/// pointer form stores its pointer as bytes, so the type is POD and lanes
/// of cells are memcpy-safe.
///
/// Reading THROUGH a pointer-form cell is claim-scoped (R1–R6): the
/// payload it aliases must be live — arena values until reset, pinned
/// source images for the batch claim, dictionary payload regions for the
/// handle's life.
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StrCell {
    pub(crate) len: u32,
    pub(crate) prefix: [u8; 4],
    pub(crate) tail: [u8; 8],
}

impl StrCell {
    #[inline]
    pub const fn zeroed() -> StrCell {
        StrCell { len: 0, prefix: [0; 4], tail: [0; 8] }
    }

    #[inline]
    pub fn inline_from_bytes(s: &[u8]) -> StrCell {
        debug_assert!(s.len() <= STRVIEW_INLINE_MAX);
        let mut prefix = [0u8; 4];
        let mut tail = [0u8; 8];
        let n = s.len();
        let np = n.min(4);
        prefix[..np].copy_from_slice(&s[..np]);
        if n > 4 {
            tail[..n - 4].copy_from_slice(&s[4..]);
        }
        StrCell { len: n as u32, prefix, tail }
    }

    /// # Safety
    ///
    /// `image` points to a live plain 4B-U varlena whose payload length is
    /// `> 12`, live for the claim scope the cell is staged into.
    #[inline]
    pub unsafe fn long_from_image(image: *const u8) -> StrCell {
        // SAFETY: caller contract — live 4B-U image.
        let total = unsafe { varatt::varsize_4b(image) };
        let len = total - varatt::VARHDRSZ;
        debug_assert!(len > STRVIEW_INLINE_MAX, "long_from_image wants len > 12 (canonical form)");
        let mut prefix = [0u8; 4];
        // SAFETY: payload has > 12 bytes; first 4 are readable.
        unsafe {
            core::ptr::copy_nonoverlapping(image.add(varatt::VARHDRSZ), prefix.as_mut_ptr(), 4);
        }
        StrCell { len: len as u32, prefix, tail: (image as usize as u64).to_ne_bytes() }
    }

    pub fn from_bytes(s: &[u8], arena: &mut ClaimArena) -> StrCell {
        if s.len() <= STRVIEW_INLINE_MAX {
            return StrCell::inline_from_bytes(s);
        }
        let total = varatt::VARHDRSZ + s.len();
        let dst = arena.alloc(total).as_ptr();
        // SAFETY: fresh arena range of `total` bytes.
        unsafe {
            dst.cast::<u32>().write(varatt::set_varsize_4b_word(total as u32));
            core::ptr::copy_nonoverlapping(s.as_ptr(), dst.add(varatt::VARHDRSZ), s.len());
            StrCell::long_from_image(dst)
        }
    }

    /// # Safety
    ///
    /// `p` points to a live plain-inline varlena image (1B short or 4B-U),
    /// readable for its header-declared extent, live for the claim scope.
    pub unsafe fn from_plain_image(p: *const u8, arena: &mut ClaimArena) -> StrCell {
        // SAFETY: caller contract for all header/payload reads below.
        unsafe {
            if varatt::varatt_is_1b(p) {
                let len = varatt::varsize_1b(p) - varatt::VARHDRSZ_SHORT;
                let payload = core::slice::from_raw_parts(p.add(varatt::VARHDRSZ_SHORT), len);
                StrCell::from_bytes(payload, arena)
            } else {
                debug_assert!(varatt::varatt_is_4b_u(p), "from_plain_image wants plain inline");
                let len = varatt::varsize_4b(p) - varatt::VARHDRSZ;
                if len <= STRVIEW_INLINE_MAX {
                    let payload = core::slice::from_raw_parts(p.add(varatt::VARHDRSZ), len);
                    StrCell::inline_from_bytes(payload)
                } else {
                    StrCell::long_from_image(p)
                }
            }
        }
    }

    #[doc(hidden)]
    pub fn from_raw(len: u32, prefix: [u8; 4], tail: [u8; 8]) -> StrCell {
        StrCell { len, prefix, tail }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn is_inline(&self) -> bool {
        self.len as usize <= STRVIEW_INLINE_MAX
    }

    #[inline]
    pub fn word0(&self) -> u64 {
        (self.len as u64) | ((u32::from_ne_bytes(self.prefix) as u64) << 32)
    }

    #[inline]
    pub fn word1(&self) -> u64 {
        u64::from_ne_bytes(self.tail)
    }

    #[inline]
    pub fn prefix_bytes(&self) -> [u8; 4] {
        self.prefix
    }

    #[inline]
    pub fn image_ptr(&self) -> *const u8 {
        debug_assert!(!self.is_inline());
        u64::from_ne_bytes(self.tail) as usize as *const u8
    }

    /// # Safety
    ///
    /// For pointer-form cells the caller asserts the claim scope (payload
    /// live). Inline cells borrow from the cell itself. The returned slice
    /// must not outlive the claim.
    #[inline]
    pub unsafe fn content<'a>(&'a self) -> &'a [u8] {
        if self.is_inline() {
            // SAFETY: in-bounds view of the cell's own 12 content bytes.
            unsafe { core::slice::from_raw_parts(self.prefix.as_ptr(), self.len as usize) }
        } else {
            // SAFETY: caller contract — live 4B-U payload of `len` bytes.
            unsafe {
                core::slice::from_raw_parts(
                    self.image_ptr().add(varatt::VARHDRSZ),
                    self.len as usize,
                )
            }
        }
    }

    /// # Safety
    ///
    /// Claim scope for pointer-form payloads (both cells).
    #[inline]
    pub unsafe fn eq(a: &StrCell, b: &StrCell) -> bool {
        if a.word0() != b.word0() {
            return false;
        }
        if a.is_inline() {
            return a.word1() == b.word1();
        }
        // SAFETY: caller contract.
        unsafe { a.content() == b.content() }
    }

    /// # Safety
    ///
    /// Claim scope for pointer-form payloads.
    #[inline]
    pub unsafe fn hash(&self) -> u64 {
        use ::order_key::hash::{fold64, mix64};
        let acc = fold64(STRVIEW_HASH_SEED, self.word0());
        if self.is_inline() {
            return mix64(fold64(acc, self.word1()));
        }
        // SAFETY: caller contract.
        let bytes = unsafe { self.content() };
        let mut acc = acc;
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            acc = fold64(acc, u64::from_le_bytes(c.try_into().expect("len 8")));
        }
        let rem = chunks.remainder();
        if !rem.is_empty() {
            let mut t = [0u8; 8];
            t[..rem.len()].copy_from_slice(rem);
            acc = fold64(acc, u64::from_le_bytes(t));
        }
        mix64(acc)
    }

    /// # Safety
    ///
    /// Claim scope for pointer-form payloads.
    pub unsafe fn cmp_c(a: &StrCell, b: &StrCell) -> core::cmp::Ordering {
        use core::cmp::Ordering;
        if a.is_inline() && b.is_inline() {
            let a0 = u64::from_be_bytes([
                a.prefix[0], a.prefix[1], a.prefix[2], a.prefix[3], a.tail[0], a.tail[1],
                a.tail[2], a.tail[3],
            ]);
            let b0 = u64::from_be_bytes([
                b.prefix[0], b.prefix[1], b.prefix[2], b.prefix[3], b.tail[0], b.tail[1],
                b.tail[2], b.tail[3],
            ]);
            let c = a0.cmp(&b0);
            if c != Ordering::Equal {
                return c;
            }
            let a1 = u32::from_be_bytes([a.tail[4], a.tail[5], a.tail[6], a.tail[7]]);
            let b1 = u32::from_be_bytes([b.tail[4], b.tail[5], b.tail[6], b.tail[7]]);
            let c = a1.cmp(&b1);
            if c != Ordering::Equal {
                return c;
            }
            return a.len.cmp(&b.len);
        }
        if !a.is_inline() && !b.is_inline() {
            // Both lengths >= 13: all 4 prefix bytes are real content.
            let ap = u32::from_be_bytes(a.prefix);
            let bp = u32::from_be_bytes(b.prefix);
            let c = ap.cmp(&bp);
            if c != Ordering::Equal {
                return c;
            }
        }
        // SAFETY: caller contract. Slice Ord IS memcmp-then-length.
        unsafe { a.content().cmp(b.content()) }
    }

    /// # Safety
    ///
    /// Pointer-form cells are dereferenced (header + first 4 payload
    /// bytes) — claim scope required.
    pub unsafe fn check(&self) -> Result<(), &'static str> {
        let len = self.len as usize;
        if len <= STRVIEW_INLINE_MAX {
            for i in len..STRVIEW_INLINE_MAX {
                let byte = if i < 4 { self.prefix[i] } else { self.tail[i - 4] };
                if byte != 0 {
                    return Err("strview: inline padding not zeroed (invariant 1)");
                }
            }
            Ok(())
        } else {
            let p = self.image_ptr();
            if p.is_null() {
                return Err("strview: null payload pointer (invariant 4)");
            }
            // SAFETY: caller contract — live image header.
            let header = unsafe { p.cast::<u32>().read_unaligned() };
            // SAFETY: header word read above; 4B-U test is a bit test.
            if !unsafe { varatt::varatt_is_4b_u(p) } {
                return Err("strview: payload not a plain 4B-U varlena (invariant 4)");
            }
            let total = varatt::varsize_4b_word(header) as usize;
            if total != varatt::VARHDRSZ + len {
                return Err("strview: payload length disagrees with cell len (invariant 4)");
            }
            // SAFETY: payload has len > 12 bytes; first 4 readable.
            let mut first4 = [0u8; 4];
            unsafe {
                core::ptr::copy_nonoverlapping(p.add(varatt::VARHDRSZ), first4.as_mut_ptr(), 4)
            };
            if first4 != self.prefix {
                return Err("strview: prefix disagrees with payload bytes (invariant 1/4)");
            }
            Ok(())
        }
    }
}

#[derive(Clone, Copy)]
pub struct StrViews {
    cells: NonNull<StrCell>,
    epoch: u64,
}

impl StrViews {
    /// # Safety
    ///
    /// The producer asserts: `cells` points to at least `nrows` cells for
    /// the batch this lane is staged into, allocated under the claim
    /// arena's `epoch`; every cell satisfies [`StrCell::check`]; the
    /// column's datum lane holds valid plain varlena pointers for every
    /// selected valid row (the overlay law); pointer-form payloads are
    /// live for the claim scope.
    #[inline]
    pub unsafe fn publish(cells: NonNull<StrCell>, epoch: u64) -> StrViews {
        StrViews { cells, epoch }
    }

    #[inline]
    pub fn cells_ptr(self) -> NonNull<StrCell> {
        self.cells
    }

    /// # Safety
    ///
    /// Caller asserts the publish contract still holds (claim-scoped, R1)
    /// and `nrows` does not exceed the published row count. Callers with
    /// the arena in hand `assert_epoch(self.epoch())` first.
    #[inline]
    pub unsafe fn cells<'a>(self, nrows: usize) -> &'a [StrCell] {
        // SAFETY: publish contract.
        unsafe { core::slice::from_raw_parts(self.cells.as_ptr(), nrows) }
    }

    #[inline]
    pub fn epoch(self) -> u64 {
        self.epoch
    }
}

impl PartialEq for StrViews {
    fn eq(&self, other: &Self) -> bool {
        self.cells == other.cells && self.epoch == other.epoch
    }
}

impl Eq for StrViews {}

impl core::fmt::Debug for StrViews {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StrViews")
            .field("cells", &self.cells)
            .field("epoch", &self.epoch)
            .finish()
    }
}

impl Column {
    #[inline]
    pub fn strviews(&self) -> Option<StrViews> {
        match self.rep {
            ColRep::StrView(sv) => Some(sv),
            _ => None,
        }
    }

    /// # Safety
    ///
    /// The column is staged per R1–R6: every selected valid datum points
    /// to a live varlena image readable for its header-declared extent,
    /// live for the claim scope; `nrows` covers every selected position.
    pub unsafe fn build_strviews(
        &mut self,
        sel: &Selection,
        nrows: usize,
        arena: &mut ClaimArena,
    ) -> bool {
        debug_assert!(
            matches!(self.base, ColRep::Varlena { .. }),
            "build_strviews wants a varlena-based column, base {:?}",
            self.base
        );
        if !matches!(self.rep, ColRep::Varlena { .. }) {
            return false;
        }
        let lane = arena.alloc(nrows * core::mem::size_of::<StrCell>());
        let cells = lane.cast::<StrCell>();
        // SAFETY: fresh arena range of nrows cells; zero cell = all-zero.
        unsafe {
            core::ptr::write_bytes(cells.as_ptr(), 0, nrows);
        }
        for &pos in sel.as_slice() {
            let row = pos as usize;
            if !self.validity.is_valid(row) {
                continue;
            }
            let p = self.datums[row].as_usize() as *const u8;
            // SAFETY: caller contract — live image header.
            if unsafe { classify_varlena(p) }.needs_detoast() {
                return false;
            }
            // SAFETY: plain inline per the classify above; claim scope per
            // caller contract. Row < nrows per caller contract.
            unsafe {
                cells.as_ptr().add(row).write(StrCell::from_plain_image(p, arena));
            }
        }
        // SAFETY: publish contract established by this pass (cells under
        // the live epoch; datum lane untouched and plain-inline-proven).
        self.rep = ColRep::StrView(unsafe { StrViews::publish(cells, arena.epoch()) });
        true
    }

    /// # Safety
    ///
    /// The dict publish contract holds (claim-scoped codes + live provider
    /// per R1–R6); `nrows` does not exceed the published row count.
    pub unsafe fn build_strviews_from_dict(&mut self, nrows: usize, arena: &mut ClaimArena) -> bool {
        let ColRep::DictCodes(dc) = self.rep else {
            return false;
        };
        // SAFETY: publish contract (caller).
        let codes = unsafe { dc.codes(nrows) };
        let space = unsafe { dc.dict().space() };
        if !matches!(space.base_rep(), ColRep::Varlena { .. }) {
            return false;
        }
        debug_assert!(
            space.base_rep().same_base_class(self.base),
            "dict base_rep {:?} not the column's base class {:?}",
            space.base_rep(),
            self.base
        );
        self.ensure_rows(nrows);
        let lane = arena.alloc(nrows * core::mem::size_of::<StrCell>());
        let cells = lane.cast::<StrCell>();
        // SAFETY: fresh arena range of nrows cells.
        unsafe {
            core::ptr::write_bytes(cells.as_ptr(), 0, nrows);
        }
        for row in 0..nrows {
            if !self.validity.is_valid(row) {
                self.datums[row] = Datum::null();
                continue;
            }
            let code = codes[row];
            let d = space.entry_datum(code);
            self.datums[row] = d;
            let len = space.byte_len(code) as usize;
            let image = d.as_usize() as *const u8;
            let cell = if len <= STRVIEW_INLINE_MAX {
                // SAFETY: §7b — the entry datum is a plain 4B-U image with
                // `len` payload bytes, generation-stable.
                let payload =
                    unsafe { core::slice::from_raw_parts(image.add(varatt::VARHDRSZ), len) };
                StrCell::inline_from_bytes(payload)
            } else {
                // SAFETY: §7b image, len > 12 — zero-copy alias.
                unsafe { StrCell::long_from_image(image) }
            };
            // SAFETY: row < nrows cells written above.
            unsafe { cells.as_ptr().add(row).write(cell) };
        }
        // SAFETY: publish contract established (datum lane = entry datums,
        // plain images per §7b; cells under the live epoch).
        self.rep = ColRep::StrView(unsafe { StrViews::publish(cells, arena.epoch()) });
        true
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TextSpan {
    base: NonNull<u8>,
    len: usize,
}

impl TextSpan {
    /// # Safety
    ///
    /// The producer asserts: `base..base+len` is ONE live readable
    /// allocation for the claim scope of the batch this witness rides;
    /// every selected valid datum of the witnessed column points at a
    /// plain 4B-U varlena image whose full extent lies inside the span;
    /// the window's datum pointers are strictly ascending in row order.
    #[inline]
    pub unsafe fn publish(base: NonNull<u8>, len: usize) -> TextSpan {
        TextSpan { base, len }
    }

    #[inline]
    pub fn base(self) -> NonNull<u8> {
        self.base
    }

    #[inline]
    pub fn len(self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.len == 0
    }

    /// # Safety
    ///
    /// Caller asserts the publish contract still holds (claim-scoped)
    /// and the returned slice does not outlive the claim.
    #[inline]
    pub unsafe fn bytes<'a>(self) -> &'a [u8] {
        // SAFETY: publish contract.
        unsafe { core::slice::from_raw_parts(self.base.as_ptr(), self.len) }
    }

    /// # Safety
    ///
    /// `lo..hi` is one live allocation, readable for the claim scope the
    /// witness will ride (the only dereference is the last image's
    /// header, proven in-bounds before the read).
    pub unsafe fn certify(
        datums: &[Datum],
        validity: &crate::Validity,
        lo: usize,
        hi: usize,
    ) -> Option<TextSpan> {
        if datums.is_empty() || hi <= lo {
            return None;
        }
        let mut prev = 0usize;
        for (r, d) in datums.iter().enumerate() {
            if !validity.is_valid(r) {
                return None;
            }
            let p = d.as_usize();
            if p <= prev || p < lo || p.checked_add(varatt::VARHDRSZ)? > hi {
                return None;
            }
            // SAFETY: header readable (bounds proven above); allocation
            // live per the caller contract.
            if !unsafe { varatt::varatt_is_4b_u(p as *const u8) } {
                return None;
            }
            prev = p;
        }
        // SAFETY: header read proven in-bounds + 4B-U proven above.
        let end = prev.checked_add(unsafe { varatt::varsize_4b(prev as *const u8) })?;
        if end > hi {
            return None;
        }
        let first = datums[0].as_usize();
        // SAFETY: the publish contract is exactly what this pass proved
        // (ascending in-allocation pointers; extent inside the span) plus
        // the caller's provenance/liveness assertion.
        Some(unsafe { TextSpan::publish(NonNull::new(first as *mut u8)?, end - first) })
    }
}

pub struct StrScratch {
    arena: ClaimArena,
}

impl Default for StrScratch {
    fn default() -> Self {
        Self::new()
    }
}

impl StrScratch {
    pub fn new() -> StrScratch {
        StrScratch { arena: ClaimArena::new() }
    }

    /// # Safety
    ///
    /// Claim scope for pointer-form cells (the payload the datum aliases
    /// must be live for the datum's use).
    #[inline]
    pub unsafe fn materialize(&mut self, cell: &StrCell) -> Datum {
        if !cell.is_inline() {
            return Datum::from_usize(cell.image_ptr() as usize);
        }
        let len = cell.len();
        let total = varatt::VARHDRSZ_SHORT + len;
        debug_assert!(total <= 13);
        let mut img = [0u8; 16];
        img[1..5].copy_from_slice(&cell.prefix);
        img[5..13].copy_from_slice(&cell.tail);
        // SAFETY: img[0] is writable; total <= VARATT_SHORT_MAX.
        unsafe { varatt::set_varsize_short(img.as_mut_ptr(), total) };
        let dst = self.arena.alloc(16).as_ptr();
        let lo = u64::from_ne_bytes(img[0..8].try_into().expect("len 8"));
        let hi = u64::from_ne_bytes(img[8..16].try_into().expect("len 8"));
        // SAFETY: fresh 16-byte ≥8-aligned arena range.
        unsafe {
            dst.cast::<u64>().write(lo);
            dst.add(8).cast::<u64>().write(hi);
        }
        Datum::from_usize(dst as usize)
    }

    pub fn reset(&mut self) {
        self.arena.reset();
    }

    pub fn used(&self) -> usize {
        self.arena.used()
    }
}

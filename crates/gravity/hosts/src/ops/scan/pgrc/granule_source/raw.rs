//! granule_source's raw half: every `unsafe` of granule_source.rs and its fetch.rs, lanes.rs and late.rs lives here, so
//! those files compile under the crate's `#![deny(unsafe_code)]`.
//! - The raw operations: the lane decodes (`decode_into`, `framed_at`, `decode_mantissa`, with `narrow` and `scatter`), the
//!   row-mask kernels (`mask_ne_u32` with its SSE2 / AVX2 bodies, `mask_range_i32`, `mask_range_i16`, `mask_table_u32`,
//!   `mask_delta` on stored deltas), the selection walk (`walk_sel`), `rel_of`, and the source's `Send`.
//! - Safe faces over the contracts the call sites stated in their SAFETY text: the bound source's `Resolved`, relation
//!   and part list (`resolved_unbound`, `rel_unbound`, `list_unbound`), the table state's regions (`cells_region`,
//!   `tail_region`), a lane slot's cells and packed frames (`cells`, `packed_frames`), the two decodes over a lane slot
//!   (`decode_lane_into`, `decode_lane_mantissa`), a table lane's publish (`publish_lane`) and a rendered image's header
//!   (`image_header`), a bound code set's key cell and text value (`key_cell`, `bind_text`).
#![allow(unsafe_code)]

use super::*;

impl GranuleRangeSource {
    /// The statement's `Resolved`, unbound from the source's borrow (the Binding outlives every call on a bound source).
    /// Called only while the source is bound (every caller is past bind, a staged window or a claim).
    #[inline]
    pub(super) fn resolved_unbound<'r>(&self) -> &'r Resolved {
        // SAFETY: bound: `resolved` is the pointer `bind` stored, and the statement's `Resolved` outlives this call.
        unsafe { &*self.resolved }
    }

    /// D3206: the statement's fingerprint memo (`None` on an unbound source: its own lookups, as before).
    #[inline]
    pub(super) fn fp_memo<'r>(&self) -> Option<&'r crate::dict::text_hosts::FpMemo> {
        // SAFETY: a non-null `resolved` is the pointer `bind` stored, and the statement's `Resolved` outlives this call.
        (!self.resolved.is_null()).then(|| unsafe { (*self.resolved).fp_memo() })
    }

    /// This source's `RelParts` in the statement's `Resolved`, unbound from the source's borrow; called only while bound.
    #[inline]
    pub(super) fn rel_unbound<'r>(&self) -> &'r RelParts {
        // SAFETY: bound; the statement's `Resolved` outlives this call.
        unsafe { rel_of(self.resolved, self.rel) }
    }

    /// The bound part list, unbound from the source's borrow (`None`: unbound). A borrow, not an Arc::clone per granule.
    #[inline]
    pub(super) fn list_unbound<'r>(&self) -> Option<&'r PartList> {
        // SAFETY: `self.list` is replaced only by bind and unbind, never under stage; its Arc keeps the list alive for the
        // caller's call.
        self.list.as_ref().map(|l| unsafe { &*Arc::as_ptr(l) })
    }
}

// SAFETY: the `Resolved` pointer is read only while bound, from the thread running the source's claim.
unsafe impl Send for GranuleRangeSource {}

impl TableState {
    /// The once-cells' region over `cells` (an 8-aligned buffer this state owns for its lifetime; the once table built in
    /// it lives beside it in this state).
    #[inline]
    pub(super) fn cells_region(&mut self) -> gravity_dict::Region {
        // SAFETY: an 8-aligned buffer owned by this state for its lifetime.
        unsafe { gravity_dict::Region::new(self.cells.as_mut_ptr() as *mut u8, self.cells.len() * 8) }
    }

    /// The region past `used` in the last table chunk, and that chunk's byte length.
    #[inline]
    pub(super) fn tail_region(&mut self) -> (gravity_dict::Region, usize) {
        let chunk = self.bufs.last_mut().expect("chunk");
        let (base, cap) = (chunk.as_mut_ptr() as *mut u8, chunk.len() * 8);
        assert!(self.used <= cap);
        // SAFETY: the tail of an 8-aligned chunk owned by this state; earlier tables lie below `used` or in earlier chunks.
        (unsafe { gravity_dict::Region::new(base.add(self.used), cap - self.used) }, cap)
    }
}

/// A lane slot's cell type: an integer, so every bit pattern is a value, aligned to at most 8.
pub(super) trait Cell: Copy {}
impl Cell for u32 {}
impl Cell for i64 {}
impl Cell for i32 {}
impl Cell for i16 {}
impl Cell for i8 {}

/// The first `n` cells of lane slot `words` at the lane's width (`T`).
#[inline]
/// PS1-22: whether no entry of a part's dictionary `Fp128` table is among `fps` (ascending `lo | hi << 64` images); `None`
/// when the table faulted (a code without its image: the caller keeps the part).
pub(super) fn fp_none_in(fp: &gravity_dict::Fp128, fps: &[u128]) -> Option<bool> {
    if fp.has_faults() {
        return None;
    }
    // SAFETY: every code below the table's `ncodes` has its two words
    Some((0..fp.ncodes()).all(|c| {
        let [lo, hi] = unsafe { fp.get(c) };
        fps.binary_search(&(lo as u128 | (hi as u128) << 64)).is_err()
    }))
}

pub(super) fn cells<T: Cell>(words: &[u64], n: usize) -> &[T] {
    assert!(n * core::mem::size_of::<T>() <= words.len() * 8);
    // SAFETY: `T` is an integer (no invalid values) aligned to at most 8 (u64 storage gives 8); `n` cells fit in the words,
    // borrowed for the view's life.
    unsafe { core::slice::from_raw_parts(words.as_ptr() as *const T, n) }
}

/// The packed frames table at the head of `words` (lane k's table words past the lane slots).
#[inline]
pub(super) fn packed_frames(words: &mut [u64]) -> &mut PackedFrames {
    assert!(words.len() >= PF_WORDS);
    // SAFETY: 8-aligned words; `PackedFrames` is plain u64 words, `PF_WORDS` of them, all inside `words`, borrowed mutably
    // for the view's life.
    unsafe { &mut *(words.as_mut_ptr() as *mut PackedFrames) }
}

/// `decode_into` over lane slot `slot` (the lane's datum words, at least ROWS): `dense` / `sel16` / `sel32` hold the `n`
/// selected rows, each a row of the granule.
#[allow(clippy::too_many_arguments)]
#[inline]
pub(super) fn decode_lane_into(cur: &mut StreamCursor, kind: u8, width: u8, lg: u32, rows: usize, sel: Option<usize>, slot: &mut [u64], dense: &mut [u64], sel16: &[u16], sel32: &[u32], arena: &mut [u8], used: &mut usize) -> (usize, pgrc_read::ReadResult<u32>) {
    assert!(rows <= ROWS && slot.len() >= ROWS);
    // SAFETY: lane k's slot is ROWS datum words; the selection holds `n` rows of the granule.
    unsafe { decode_into(cur, kind, width, lg, rows, sel, slot.as_mut_ptr(), dense, sel16, sel32, arena, used) }
}

/// `decode_mantissa` over lane slot `slot` (the lane's datum words, at least ROWS): `sel16` holds the `n` selected rows.
#[inline]
pub(super) fn decode_lane_mantissa(cur: &mut StreamCursor, scale: i32, lg: u32, rows: usize, sel: Option<usize>, slot: &mut [u64], sel16: &[u16]) -> (usize, pgrc_read::ReadResult<u32>) {
    assert!(rows <= ROWS && slot.len() >= ROWS);
    // SAFETY: lane k's slot is ROWS datum words; `sel16` holds `n` when selected.
    unsafe { decode_mantissa(cur, scale, lg, rows, sel, slot.as_mut_ptr(), sel16) }
}

/// Publishes table header `h` as table lane `ordinal` over lane `over`. `h` is a header this source published into (or
/// allocated in) its own region for this part generation.
#[inline]
pub(super) fn publish_lane(h: *const gravity_dict::table::Header, ordinal: u32, over: u8, out: &mut Lane) {
    // SAFETY: the caller's header: this source's own, live for the part generation.
    unsafe { gravity_dict::lane::publish(h, ordinal, over, out) }
}

/// The 4-byte header word of the 4B-U image at datum `d` (a render decode's datum into this source's chunk).
#[inline]
pub(super) fn image_header(d: u64) -> u32 {
    // SAFETY: the decoded datum is a 4B-U image in the chunk.
    unsafe { *(d as *const u32) }
}

/// `RelParts` `i` of the statement's `Resolved`, unbound from the source's borrow (the Binding outlives every call on a bound source).
/// # Safety — `resolved` is the pointer `bind` stored and the source is bound.
unsafe fn rel_of<'r>(resolved: *const Resolved, i: usize) -> &'r RelParts {
    let r: &'r Resolved = &*resolved;
    &r.rels[i]
}

/// The `rows` datum words at the head of `slot` narrowed in place to `width` bytes each (forward: a target never passes its source).
unsafe fn narrow(slot: *mut u64, rows: usize, width: u8) {
    match width {
        1 => {
            let out = slot as *mut u8;
            // bounded("small-fixed: the rows of one granule, at most GRANULE_ROWS (8192); the claim's walk ticks per granule")
            for i in 0..rows {
                *out.add(i) = *slot.add(i) as u8;
            }
        }
        4 => {
            let out = slot as *mut u32;
            // bounded("small-fixed: the rows of one granule, at most GRANULE_ROWS (8192); the claim's walk ticks per granule")
            for i in 0..rows {
                *out.add(i) = *slot.add(i) as u32;
            }
        }
        _ => {}
    }
}

/// H06 (the code verdict): a kept row of `from..to` whose code table `b` rejects (`Bits::rejects`: bit clear, entry
/// unfaulted) leaves the mask words; only kept rows' codes are read.
pub(super) fn mask_table_u32(mask: &mut [u64], from: usize, to: usize, codes: &[u32], b: &gravity_dict::table::Bits) {
    // bounded("small-fixed: the mask words of one granule, at most VALIDITY_WORDS (128)")
    for (j, m) in mask.iter_mut().enumerate() {
        let base = from + j * 64;
        let mut keep = *m;
        let mut out = keep;
        // bounded("small-fixed: the set bits of one 64-bit word")
        while keep != 0 {
            let i = keep.trailing_zeros() as usize;
            debug_assert!(base + i < to, "a kept row inside the range");
            out &= !((b.rejects(codes[base + i]) as u64) << i);
            keep &= keep - 1;
        }
        *m = out;
    }
}

/// `codes[i] != c` into the mask words of rows `from..to`: sixteen codes per step on NEON (the
/// compare narrowed to bytes, weighted, summed per eight rows), the partial last word scalar.
pub(super) fn mask_ne_u32(mask: &mut [u64], from: usize, to: usize, codes: &[u32], c: u32) {
    let full = (to - from) / 64;
    #[cfg(target_arch = "aarch64")]
    {
        use core::arch::aarch64::*;
        const WEIGHTS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
        // SAFETY: NEON is the aarch64 baseline; every load reads sixteen codes of a full word inside `codes[..to]`.
        unsafe {
            let vc = vdupq_n_u32(c);
            let weights = vld1q_u8(WEIGHTS.as_ptr());
            // bounded("small-fixed: the mask words of one granule, at most VALIDITY_WORDS (128)")
            for (j, m) in mask[..full].iter_mut().enumerate() {
                let p = codes.as_ptr().add(from + j * 64);
                let mut bits = 0u64;
                for q in 0..4 {
                    let p = p.add(q * 16);
                    let e0 = vceqq_u32(vld1q_u32(p), vc);
                    let e1 = vceqq_u32(vld1q_u32(p.add(4)), vc);
                    let e2 = vceqq_u32(vld1q_u32(p.add(8)), vc);
                    let e3 = vceqq_u32(vld1q_u32(p.add(12)), vc);
                    let n01 = vuzp1q_u16(vreinterpretq_u16_u32(e0), vreinterpretq_u16_u32(e1));
                    let n23 = vuzp1q_u16(vreinterpretq_u16_u32(e2), vreinterpretq_u16_u32(e3));
                    let w = vandq_u8(vuzp1q_u8(vreinterpretq_u8_u16(n01), vreinterpretq_u8_u16(n23)), weights);
                    let eq = vaddv_u8(vget_low_u8(w)) as u64 | (vaddv_u8(vget_high_u8(w)) as u64) << 8;
                    bits |= (!eq & 0xFFFF) << (16 * q);
                }
                *m &= bits;
            }
        }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: the full words end inside `codes[..to]`; the AVX2 body runs only after the detection.
        unsafe {
            if full > 0 && is_x86_feature_detected!("avx2") {
                mask_ne_u32_avx2(&mut mask[..full], &codes[from..from + full * 64], c);
            } else {
                mask_ne_u32_sse2(&mut mask[..full], &codes[from..from + full * 64], c);
            }
        }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    mask_rows(&mut mask[..full], from, from + full * 64, |i| codes[i] != c);
    mask_rows(&mut mask[full..], from + full * 64, to, |i| codes[i] != c);
}

/// SSE2 (the x86-64 baseline): four compares narrowed by two saturating packs, one movemask per sixteen rows.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn mask_ne_u32_sse2(mask: &mut [u64], codes: &[u32], c: u32) {
    use core::arch::x86_64::*;
    debug_assert_eq!(codes.len(), mask.len() * 64);
    let vc = _mm_set1_epi32(c as i32);
    // bounded("small-fixed: the mask words of one granule, at most VALIDITY_WORDS (128)")
    for (j, m) in mask.iter_mut().enumerate() {
        let p = codes.as_ptr().add(j * 64) as *const __m128i;
        let mut bits = 0u64;
        for q in 0..4 {
            let p = p.add(q * 4);
            let e0 = _mm_cmpeq_epi32(_mm_loadu_si128(p), vc);
            let e1 = _mm_cmpeq_epi32(_mm_loadu_si128(p.add(1)), vc);
            let e2 = _mm_cmpeq_epi32(_mm_loadu_si128(p.add(2)), vc);
            let e3 = _mm_cmpeq_epi32(_mm_loadu_si128(p.add(3)), vc);
            let eq = _mm_movemask_epi8(_mm_packs_epi16(_mm_packs_epi32(e0, e1), _mm_packs_epi32(e2, e3))) as u32 as u64;
            bits |= (!eq & 0xFFFF) << (16 * q);
        }
        *m &= bits;
    }
}

/// AVX2: eight compares of eight codes, each collapsed by a float movemask (no cross-lane pack).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn mask_ne_u32_avx2(mask: &mut [u64], codes: &[u32], c: u32) {
    use core::arch::x86_64::*;
    debug_assert_eq!(codes.len(), mask.len() * 64);
    let vc = _mm256_set1_epi32(c as i32);
    // bounded("small-fixed: the mask words of one granule, at most VALIDITY_WORDS (128)")
    for (j, m) in mask.iter_mut().enumerate() {
        let p = codes.as_ptr().add(j * 64) as *const __m256i;
        let mut bits = 0u64;
        for q in 0..8 {
            let eq = _mm256_movemask_ps(_mm256_castsi256_ps(_mm256_cmpeq_epi32(_mm256_loadu_si256(p.add(q)), vc))) as u32 as u64;
            bits |= (!eq & 0xFF) << (8 * q);
        }
        *m &= bits;
    }
}

/// The walk of `words_to_sel`, `W16` whether it writes `sel16` too (else `sel16` is left as it was).
#[inline(always)]
pub(super) fn walk_sel<const W16: bool>(words: &[u64], sel: &mut Vec<u32>, sel16: &mut Vec<u16>) -> usize {
    sel.clear();
    if W16 {
        sel16.clear();
    }
    // every store is at an index below `64 * words.len()`: word w starts with `k <= 64 w` and stores at most at
    // `k + popcount(w) - 1`, or at `k` or `k + 1` for its unconditional two
    let cap = words.len() * 64;
    sel.reserve(cap); // alloc: Node (bind reserved ROWS; a granule holds at most ROWS)
    if W16 {
        sel16.reserve(cap); // alloc: Node
    }
    let (p32, p16) = (sel.as_mut_ptr(), sel16.as_mut_ptr());
    let mut k = 0;
    // bounded("small-fixed: the selection words of one granule, at most VALIDITY_WORDS (128)")
    for (w, &m) in words.iter().enumerate() {
        let base = (w * 64) as u32;
        if m == u64::MAX {
            // a full word (a hit that keeps nearly every row) is 64 consecutive rows: 4- and 8-lane chunks stay vector adds
            // and stores (a plain 64-step loop unrolled to scalar stores and spills)
            for c in 0..16 {
                let mut v = [0u32; 4];
                // bounded("small-fixed: the lanes of one vector chunk, 4 or 8")
                for (l, x) in v.iter_mut().enumerate() {
                    *x = IOTA32[c * 4 + l] + base;
                }
                // SAFETY: `k <= 64 w`, so the 64 cells from `k` lie below `64 (w + 1) <= cap`.
                unsafe { p32.add(k + c * 4).cast::<[u32; 4]>().write_unaligned(v) };
            }
            if W16 {
                let b16 = base as u16;
                for c in 0..8 {
                    let mut v = [0u16; 8];
                    // bounded("small-fixed: the lanes of one vector chunk, 4 or 8")
                    for (l, x) in v.iter_mut().enumerate() {
                        *x = IOTA16[c * 8 + l].wrapping_add(b16);
                    }
                    // SAFETY: as above.
                    unsafe { p16.add(k + c * 8).cast::<[u16; 8]>().write_unaligned(v) };
                }
            }
            k += 64;
            continue;
        }
        let mut m = m;
        for _ in 0..2 {
            // `trailing_zeros` of an empty word is 64: a cell the next store overwrites, or one past the length
            let i = base + m.trailing_zeros();
            // SAFETY: `k < cap`, the capacity just reserved (above).
            unsafe {
                *p32.add(k) = i;
                if W16 {
                    *p16.add(k) = i as u16;
                }
            }
            k += (m != 0) as usize;
            m &= m.wrapping_sub(1);
        }
        // bounded("small-fixed: the set bits of one 64-bit word")
        while m != 0 {
            let i = base + m.trailing_zeros();
            // SAFETY: as above.
            unsafe {
                *p32.add(k) = i;
                if W16 {
                    *p16.add(k) = i as u16;
                }
            }
            k += 1;
            m &= m - 1;
        }
    }
    // SAFETY: the first `k` cells hold the set bits' indexes (a later store never lands below the count it was made at).
    unsafe {
        sel.set_len(k);
        if W16 {
            sel16.set_len(k);
        }
    }
    k
}

/// Bytes of 0 / 0xFF, one per row, weighted into their row bits (eight rows per half).
#[cfg(target_arch = "aarch64")]
const ROW_WEIGHTS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];

/// `mask_range` over an i32 lane: sixteen rows per step on NEON (two compares and an and per four, narrowed to bytes as
/// `mask_ne_u32`), the partial last word scalar. The bounds clamp to i32 first (a range outside it keeps no row).
pub(super) fn mask_range_i32(mask: &mut [u64], from: usize, to: usize, cells: &[i32], lo: i64, hi: i64) {
    if lo > hi || lo > i32::MAX as i64 || hi < i32::MIN as i64 {
        mask.iter_mut().for_each(|m| *m = 0);
        return;
    }
    let (l, h) = (lo.max(i32::MIN as i64) as i32, hi.min(i32::MAX as i64) as i32);
    let full = (to - from) / 64;
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is the aarch64 baseline; every load reads four cells of a full word inside `cells[..to]`.
        unsafe {
            use core::arch::aarch64::*;
            let (vl, vh, weights) = (vdupq_n_s32(l), vdupq_n_s32(h), vld1q_u8(ROW_WEIGHTS.as_ptr()));
            // bounded("small-fixed: the mask words of one granule, at most VALIDITY_WORDS (128)")
            for (j, m) in mask[..full].iter_mut().enumerate() {
                let p = cells.as_ptr().add(from + j * 64);
                let mut bits = 0u64;
                for q in 0..4 {
                    let r = |o: usize| {
                        let x = vld1q_s32(p.add(q * 16 + o));
                        vandq_u32(vcgeq_s32(x, vl), vcleq_s32(x, vh))
                    };
                    let n01 = vuzp1q_u16(vreinterpretq_u16_u32(r(0)), vreinterpretq_u16_u32(r(4)));
                    let n23 = vuzp1q_u16(vreinterpretq_u16_u32(r(8)), vreinterpretq_u16_u32(r(12)));
                    let w = vandq_u8(vuzp1q_u8(vreinterpretq_u8_u16(n01), vreinterpretq_u8_u16(n23)), weights);
                    bits |= (vaddv_u8(vget_low_u8(w)) as u64 | (vaddv_u8(vget_high_u8(w)) as u64) << 8) << (16 * q);
                }
                *m &= bits;
            }
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    mask_rows(&mut mask[..full], from, from + full * 64, |i| (l..=h).contains(&cells[i]));
    mask_rows(&mut mask[full..], from + full * 64, to, |i| (l..=h).contains(&cells[i]));
}

/// `mask_range` over an i16 lane: sixteen rows per step on NEON (eight per compare), as `mask_range_i32`.
pub(super) fn mask_range_i16(mask: &mut [u64], from: usize, to: usize, cells: &[i16], lo: i64, hi: i64) {
    if lo > hi || lo > i16::MAX as i64 || hi < i16::MIN as i64 {
        mask.iter_mut().for_each(|m| *m = 0);
        return;
    }
    let (l, h) = (lo.max(i16::MIN as i64) as i16, hi.min(i16::MAX as i64) as i16);
    let full = (to - from) / 64;
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is the aarch64 baseline; every load reads eight cells of a full word inside `cells[..to]`.
        unsafe {
            use core::arch::aarch64::*;
            let (vl, vh, weights) = (vdupq_n_s16(l), vdupq_n_s16(h), vld1q_u8(ROW_WEIGHTS.as_ptr()));
            // bounded("small-fixed: the mask words of one granule, at most VALIDITY_WORDS (128)")
            for (j, m) in mask[..full].iter_mut().enumerate() {
                let p = cells.as_ptr().add(from + j * 64);
                let mut bits = 0u64;
                for q in 0..4 {
                    let r = |o: usize| {
                        let x = vld1q_s16(p.add(q * 16 + o));
                        vandq_u16(vcgeq_s16(x, vl), vcleq_s16(x, vh))
                    };
                    let w = vandq_u8(vuzp1q_u8(vreinterpretq_u8_u16(r(0)), vreinterpretq_u8_u16(r(8))), weights);
                    bits |= (vaddv_u8(vget_low_u8(w)) as u64 | (vaddv_u8(vget_high_u8(w)) as u64) << 8) << (16 * q);
                }
                *m &= bits;
            }
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    mask_rows(&mut mask[..full], from, from + full * 64, |i| (l..=h).contains(&cells[i]));
    mask_rows(&mut mask[full..], from + full * 64, to, |i| (l..=h).contains(&cells[i]));
}

/// D3223 S1: `dlo <= d <= dlo + span` over a frame's run of stored deltas (`deltas`: `w`-byte LE values, w 1 / 2 / 4, one
/// a row, the first row starting `mask`'s first word) into `mask`'s words, as `(d - dlo) mod 2^(8w) <= span`: every `d`
/// and `dlo + span` are at most `2^(8w) - 1`, so a `d` below `dlo` wraps above `span`. Sixteen rows per step on NEON (the
/// compare at the delta's width, narrowed to bytes as `mask_range_i16`), the partial last word scalar.
pub(super) fn mask_delta(mask: &mut [u64], deltas: &[u8], w: u8, dlo: u32, span: u32) {
    match w {
        1 => mask_delta_w::<1>(mask, deltas, dlo, span),
        2 => mask_delta_w::<2>(mask, deltas, dlo, span),
        4 => mask_delta_w::<4>(mask, deltas, dlo, span),
        _ => unreachable!("an encoded lane's delta width is 1, 2 or 4 (enc_stage)"),
    }
}

fn mask_delta_w<const W: usize>(mask: &mut [u64], deltas: &[u8], dlo: u32, span: u32) {
    let n = deltas.len() / W;
    let full = n / 64;
    let m = if W == 4 { u32::MAX } else { (1u32 << (8 * W)) - 1 };
    let keep = |i: usize| {
        let d = deltas[i * W..i * W + W].iter().rev().fold(0u32, |x, &b| x << 8 | b as u32);
        (d.wrapping_sub(dlo) & m) <= span
    };
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is the aarch64 baseline; word j's loads read rows 64 j .. 64 j + 63, `64 W` bytes inside `deltas`
        // (j < full = n / 64), through byte loads (no alignment asked).
        unsafe {
            use core::arch::aarch64::*;
            let weights = vld1q_u8(ROW_WEIGHTS.as_ptr());
            // bounded("small-fixed: the mask words of one granule, at most VALIDITY_WORDS (128)")
            for (j, mw) in mask[..full].iter_mut().enumerate() {
                let p = deltas.as_ptr().add(j * 64 * W);
                let mut bits = 0u64;
                for q in 0..4 {
                    let at = p.add(q * 16 * W);
                    let c = match W {
                        1 => vcleq_u8(vsubq_u8(vld1q_u8(at), vdupq_n_u8(dlo as u8)), vdupq_n_u8(span as u8)),
                        2 => {
                            let r = |o: usize| vcleq_u16(vsubq_u16(vreinterpretq_u16_u8(vld1q_u8(at.add(o * 2))), vdupq_n_u16(dlo as u16)), vdupq_n_u16(span as u16));
                            vuzp1q_u8(vreinterpretq_u8_u16(r(0)), vreinterpretq_u8_u16(r(8)))
                        }
                        _ => {
                            let r = |o: usize| vcleq_u32(vsubq_u32(vreinterpretq_u32_u8(vld1q_u8(at.add(o * 4))), vdupq_n_u32(dlo)), vdupq_n_u32(span));
                            let n01 = vuzp1q_u16(vreinterpretq_u16_u32(r(0)), vreinterpretq_u16_u32(r(4)));
                            let n23 = vuzp1q_u16(vreinterpretq_u16_u32(r(8)), vreinterpretq_u16_u32(r(12)));
                            vuzp1q_u8(vreinterpretq_u8_u16(n01), vreinterpretq_u8_u16(n23))
                        }
                    };
                    let wv = vandq_u8(c, weights);
                    bits |= (vaddv_u8(vget_low_u8(wv)) as u64 | (vaddv_u8(vget_high_u8(wv)) as u64) << 8) << (16 * q);
                }
                *mw &= bits;
            }
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    mask_rows(&mut mask[..full], 0, full * 64, keep);
    mask_rows(&mut mask[full..n.div_ceil(64)], full * 64, n, keep);
}

fn scatter<T: Copy>(slot: *mut T, sel: &[u32], dense: &[u64], f: impl Fn(u64) -> T) {
    // bounded("small-fixed: the selected rows of one granule, at most GRANULE_ROWS (8192)")
    for (&i, &d) in sel.iter().zip(dense) {
        // SAFETY: `i < rows` of the granule the slot holds.
        unsafe { *slot.add(i as usize) = f(d) };
    }
}

/// Lane `k`'s granule `lg` into `slot_ptr` at its width: codes, the `n` selected rows gathered, or the whole granule (narrowed in place
/// for a 1 / 4-byte fixed lane); `(rows wanted, rows decoded)`.
/// # Safety — `slot_ptr` holds ROWS datum words; `dense` / `sel16` / `sel` hold `n` when selected.
#[allow(clippy::too_many_arguments)]
unsafe fn decode_into(cur: &mut StreamCursor, kind: u8, width: u8, lg: u32, rows: usize, sel: Option<usize>, slot_ptr: *mut u64, dense: &mut [u64], sel16: &[u16], sel32: &[u32], arena: &mut [u8], used: &mut usize) -> (usize, pgrc_read::ReadResult<u32>) {
    // a BYTE_FOR by-value lane's selected rows at their positions in the lane at its width, through the cursor's resolved
    // extent (no ctx, no section header parse, no dense copy scattered after); another stream takes the gather below
    if let (lane::KIND_BYVAL, w @ (2 | 4 | 8), Some(n)) = (kind, width, sel) {
        if let Some(r) = framed_at(cur, w, lg, rows, &sel16[..n], slot_ptr) {
            *used = 0;
            return (n, r);
        }
    }
    match (kind, width, sel) {
        (lane::KIND_CODES, _, _) => unreachable!("code lanes decode through their dictionary"),
        (_, w, Some(n)) => {
            let mut out = DecodeOut { datums: &mut dense[..n], arena: ByteArena::new(arena) };
            let r = cur.decode_sel(lg, &Selection { rows: &sel16[..n] }, &mut out);
            *used = out.arena.used();
            if r.is_ok() {
                let dense = &dense[..n];
                let sel = &sel32[..n];
                match w {
                    4 => scatter(slot_ptr as *mut i32, sel, dense, |d| d as i32),
                    2 => scatter(slot_ptr as *mut i16, sel, dense, |d| d as i16),
                    1 => scatter(slot_ptr as *mut u8, sel, dense, |d| d as u8),
                    _ => scatter(slot_ptr, sel, dense, |d| d),
                }
            }
            (n, r)
        }
        (lane::KIND_BYVAL, 4, None) => (rows, cur.decode_i32(lg, core::slice::from_raw_parts_mut(slot_ptr as *mut i32, rows))),
        (lane::KIND_BYVAL, 2, None) => (rows, cur.decode_i16(lg, core::slice::from_raw_parts_mut(slot_ptr as *mut i16, rows))),
        (_, w, None) => {
            let mut out = DecodeOut { datums: core::slice::from_raw_parts_mut(slot_ptr, rows), arena: ByteArena::new(arena) };
            let r = cur.decode_full(lg, &mut out);
            *used = out.arena.used();
            if r.is_ok() {
                narrow(slot_ptr, rows, w);
            }
            (rows, r)
        }
    }
}

/// A BYTE_FOR lane's rows `sel16` of granule `lg`, each at its own row of the lane's slot at lane width `w` (2 / 4 / 8),
/// through the cursor's resolved extent (`StreamCursor::framed`); `None` when the stream is not a framed BYTE_FOR root
/// stream (the ctx face then).
/// # Safety — `slot_ptr` holds ROWS datum words.
unsafe fn framed_at(cur: &mut StreamCursor, w: u8, lg: u32, rows: usize, sel16: &[u16], slot_ptr: *mut u64) -> Option<pgrc_read::ReadResult<u32>> {
    let width = cur.key().width;
    let f = match cur.framed(lg, EncodingId::ByteFor as u16) {
        Ok(Some(f)) => pgrc_codec::bytefor::Framed { payload: f.payload, frame_table: f.frame_table, fbase: f.fbase, values: f.values },
        Ok(None) => return None,
        Err(e) => return Some(Err(e)),
    };
    let sel = Selection { rows: sel16 };
    let r = match w {
        4 => pgrc_codec::bytefor::sel_at_framed(&f, width, &sel, core::slice::from_raw_parts_mut(slot_ptr as *mut i32, rows)),
        2 => pgrc_codec::bytefor::sel_at_framed(&f, width, &sel, core::slice::from_raw_parts_mut(slot_ptr as *mut i16, rows)),
        _ => pgrc_codec::bytefor::sel_at_framed(&f, width, &sel, core::slice::from_raw_parts_mut(slot_ptr, rows)),
    };
    Some(r.map_err(pgrc_read::ReadError::Format))
}

/// A packed numeric lane's granule `lg` as mantissa words (`decode_into`'s contract; another scale than the recipe's refuses typed).
/// # Safety — `slot_ptr` holds ROWS datum words; `sel16` holds `n` when selected.
unsafe fn decode_mantissa(cur: &mut StreamCursor, scale: i32, lg: u32, rows: usize, sel: Option<usize>, slot_ptr: *mut u64, sel16: &[u16]) -> (usize, pgrc_read::ReadResult<u32>) {
    let width = cur.key().width;
    let (want, r) = match sel {
        // the selected rows' mantissas straight to their positions in the lane (no dense copy scattered after), through the
        // cursor's resolved extent when the stream is a framed root stream (no ctx, no section header parse per granule)
        Some(n) => {
            let (sel, out) = (Selection { rows: &sel16[..n] }, core::slice::from_raw_parts_mut(slot_ptr, rows));
            let r = match cur.framed(lg, EncodingId::PackedNumeric as u16) {
                Ok(Some(f)) => {
                    let f = pgrc_codec::bytefor::Framed { payload: f.payload, frame_table: f.frame_table, fbase: f.fbase, values: f.values };
                    Some(pgrc_codec::packednum::mantissa_sel_at_framed(&f, width, &sel, out).map_err(pgrc_read::ReadError::Format))
                }
                Ok(None) => None,
                Err(e) => Some(Err(e)),
            };
            (n, r.unwrap_or_else(|| cur.granule_ctx(lg, |ctx| pgrc_codec::packednum::mantissa_sel_at(ctx, width, &sel, out))))
        }
        None => (rows, cur.granule_ctx(lg, |ctx| pgrc_codec::packednum::mantissa_full(ctx, width, core::slice::from_raw_parts_mut(slot_ptr, rows)))),
    };
    match r {
        Ok((s, n)) if s == scale => (want, Ok(n)),
        Ok(_) => (want, Err(pgrc_read::ReadError::Unsupported { what: "a numeric lane's part at another scale" })),
        Err(e) => (want, Err(e)),
    }
}

/// X5: key cell `c` of the site key arrays `seek` recorded (`GranuleRangeSource::bound_keys`: the words' address, the
/// null bytes' address, their count): its word and null byte; `None` for a cell at or past the count or no arrays.
pub(super) fn key_cell(at: (usize, usize, usize), c: usize) -> Option<(u64, u8)> {
    let (kp, np, n) = at;
    if kp == 0 || np == 0 || c >= n {
        return None;
    }
    // SAFETY: `c < n`, the length of both arrays `seek` was handed: the site's key cells, which live in the program's site
    // state through the execution (the program writes a bound code set's cells there at each window's bind).
    Some(unsafe { (*(kp as *const u64).add(c), *(np as *const u8).add(c)) })
}

/// X5: the text bind word `w`'s payload, copied (a bound EQ table's value); `None` for a NULL pointer, an external
/// (TOAST) pointer or a compressed image (the caller then faults every entry). The caller passes only the non-null text
/// parameter words the program stored in a bound code set's key cells.
pub(super) fn bind_text(w: u64) -> Option<Vec<u8>> {
    use types_tuple::varatt;
    let p = w as usize as *const u8;
    if p.is_null() {
        return None;
    }
    // SAFETY: a non-null text parameter word points at its varlena image, the statement's datum for the execution.
    unsafe {
        if varatt::varatt_is_4b_u(p) {
            Some(core::slice::from_raw_parts(p.add(4), varatt::varsize_4b(p).checked_sub(4)?).to_vec()) // alloc: Node
        } else if varatt::varatt_is_1b(p) && !varatt::varatt_is_1b_e(p) {
            Some(core::slice::from_raw_parts(p.add(1), varatt::varsize_1b(p).checked_sub(1)?).to_vec()) // alloc: Node
        } else {
            None
        }
    }
}

/// P5-02: the text values of the array bind word `w` (a bound list's whole array, `FLAG_LIST`), copied; `None` for a NULL
/// pointer, an external (TOAST) or compressed image, an array holding a NULL element, or an element that is no inline
/// text image (the caller then faults every entry, as for `bind_text`).
pub(super) fn bind_text_list(w: u64) -> Option<Vec<Vec<u8>>> {
    use types_tuple::varatt;
    let p = w as usize as *const u8;
    if p.is_null() {
        return None;
    }
    // SAFETY: a non-null bound list word points at its array's varlena image, the statement's datum for the execution;
    // every offset read below stays within the image's own size (checked against `total`).
    unsafe {
        // the image with a 4-byte header (a short-header image re-headed, as bitmap_array's `array_image`)
        let owned: Vec<u8>;
        let img: &[u8] = if varatt::varatt_is_4b_u(p) {
            core::slice::from_raw_parts(p, varatt::varsize_4b(p))
        } else if varatt::varatt_is_1b(p) && !varatt::varatt_is_1b_e(p) {
            let total = varatt::varsize_1b(p);
            let mut v = ((total - 1 + 4) as u32).wrapping_shl(2).to_le_bytes().to_vec(); // alloc: Node
            v.extend_from_slice(core::slice::from_raw_parts(p.add(1), total - 1));
            owned = v;
            &owned[..]
        } else {
            return None;
        };
        if img.len() < 16 || arrayfuncs::arr_hasnull(img) {
            return None;
        }
        let (ndim, dims, _) = arrayfuncs::read_dims_lbounds(img);
        let n: usize = if ndim <= 0 { 0 } else { dims[..ndim as usize].iter().try_fold(1usize, |a, &d| a.checked_mul(usize::try_from(d).ok()?))? };
        let total = img.len();
        let mut off = arrayfuncs::arr_data_offset(img);
        let mut out = Vec::with_capacity(n); // alloc: Node
        // bounded("other: the elements of one array constant of the statement's quals, read once at bind; PG's ExecIndexEvalArrayKeys deconstructs the same array with no poll")
        for _ in 0..n {
            // a varlena element starts at a header byte or after its 'i' alignment pad (att_align_pointer)
            if off < total && img[off] == 0 {
                off = (off + 3) & !3;
            }
            if off >= total {
                return None;
            }
            let e = img.as_ptr().add(off);
            let (data, len) = if varatt::varatt_is_1b(e) && !varatt::varatt_is_1b_e(e) {
                let l = varatt::varsize_1b(e);
                (1usize, l)
            } else if varatt::varatt_is_4b_u(e) && off + 4 <= total {
                (4usize, varatt::varsize_4b(e))
            } else {
                return None;
            };
            if len < data || off + len > total {
                return None;
            }
            out.push(img[off + data..off + len].to_vec()); // alloc: Node
            off += len;
        }
        Some(out)
    }
}

#[cfg(test)]
mod table_mask_tests {
    /// H06: the table mask clears exactly the kept rows whose code the table rejects (a code past `ncodes` is no answer,
    /// so its row stays), whatever the words held.
    #[test]
    fn mask_table_u32_drops_the_rejected_kept_rows() {
        use gravity_dict::{Bits, Codes};
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed };
        let dict = Codes { ncodes: 70, null: Some(69) };
        let mut buf = vec![0u64; 1024]; // alloc: Cold
        // SAFETY: the buffer is 8-aligned, writable and outlives the table.
        let mut region = unsafe { gravity_dict::Region::new(buf.as_mut_ptr() as *mut u8, buf.len() * 8) };
        let t = Bits::of_codes(&dict, &[2, 5, 64, 69], false, &mut region).unwrap();
        for &(from, to) in &[(0usize, 64usize), (0, 200), (64, 320), (3, 3), (128, 128 + 64 * 5 + 17)] {
            let codes: Vec<u32> = (0..to.max(1)).map(|_| (next() % 75) as u32).collect();
            let words = (to - from + 63) / 64;
            let init: Vec<u64> = (0..words).map(|j| next() & if to - from - 64 * j < 64 { (1u64 << (to - from - 64 * j)) - 1 } else { !0 }).collect();
            let (mut got, mut want) = (init.clone(), init.clone());
            super::mask_table_u32(&mut got, from, to, &codes, &t);
            for (j, m) in want.iter_mut().enumerate() {
                for b in 0..64 {
                    if *m >> b & 1 != 0 && t.rejects(codes[from + j * 64 + b]) {
                        *m &= !(1u64 << b);
                    }
                }
            }
            assert_eq!(got, want, "from={from} to={to}");
        }
    }
}

/// D3156 (`kept.rs` `SiteKept`): a site's kept-mode dictionary cell owns its entry (`Box::into_raw`); readers borrow it
/// for as long as the source reads the part, and `dict_cell_take` frees it only once every claim of the pass left the part.
pub(super) fn dict_cell_get<'a>(c: &'a pgsync::atomic::AtomicPtr<DictEntry>) -> Option<&'a DictEntry> {
    let e = c.load(Ordering::Acquire);
    // SAFETY: a non-null cell holds a live entry until `dict_cell_take`, which runs only once every claim left its part
    (!e.is_null()).then(|| unsafe { &*e })
}

/// Publish `entry` into an empty cell: `(the cell's entry, whether it is this one)` (a racing open's entry wins).
pub(super) fn dict_cell_publish(c: &pgsync::atomic::AtomicPtr<DictEntry>, entry: DictEntry) -> (&DictEntry, bool) {
    let new = Box::into_raw(Box::new(entry)); // alloc: Exec
    match c.compare_exchange(core::ptr::null_mut(), new, Ordering::AcqRel, Ordering::Acquire) {
        // SAFETY: just published, see `dict_cell_get`
        Ok(_) => (unsafe { &*new }, true),
        Err(won) => {
            // SAFETY: `new` was never published
            drop(unsafe { Box::from_raw(new) });
            // SAFETY: see `dict_cell_get`
            (unsafe { &*won }, false)
        }
    }
}

/// The cell's entry, taken out (the cell empty again); `None` when it held none.
pub(super) fn dict_cell_take(c: &pgsync::atomic::AtomicPtr<DictEntry>) -> Option<Box<DictEntry>> {
    let e = c.swap(core::ptr::null_mut(), Ordering::AcqRel);
    // SAFETY: the cell owned it; the caller frees it only once no claim reads its part
    (!e.is_null()).then(|| unsafe { Box::from_raw(e) })
}

/// D2985 (`release.rs` `StmtDicts`) + D4410 (`held.rs` `Held`): a statement's dictionary cell holds one `Arc` of its
/// entry (`Arc::into_raw`; the list's warm cell may hold another); readers borrow it for as long as the source reads the
/// part, and `arc_cell_take` lets it go only once every claim of the pass left the part (or the statement ended).
pub(super) fn arc_cell_get<T>(c: &pgsync::atomic::AtomicPtr<T>) -> Option<&T> {
    let e = c.load(Ordering::Acquire);
    // SAFETY: a non-null cell holds a live `Arc` of its entry until `arc_cell_take`, which runs only once every claim left
    // its part (or at the statement's end)
    (!e.is_null()).then(|| unsafe { &*e })
}

/// Publish `entry` into an empty cell: `(the cell's entry, whether it is this one)` (a racing reader's entry wins, this
/// `Arc` drops). D4533: the `Arc` becomes the cell's only once the cell takes its pointer, so a lost race drops it as it
/// came (no `Arc::from_raw` per entry type here; `arc_cell_take` is the cell's one).
pub(super) fn arc_cell_publish<T>(c: &pgsync::atomic::AtomicPtr<T>, entry: Arc<T>) -> (&T, bool) {
    let new = Arc::as_ptr(&entry) as *mut T;
    match c.compare_exchange(core::ptr::null_mut(), new, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {
            // the cell owns this count from now (`arc_cell_take` gives it back)
            let _ = Arc::into_raw(entry);
            // SAFETY: just published, see `arc_cell_get`
            (unsafe { &*new }, true)
        }
        Err(won) => {
            drop(entry);
            // SAFETY: see `arc_cell_get`
            (unsafe { &*won }, false)
        }
    }
}

/// The cell's `Arc`, taken out (the cell empty again); `None` when it held none.
pub(super) fn arc_cell_take<T>(c: &pgsync::atomic::AtomicPtr<T>) -> Option<Arc<T>> {
    let e = c.swap(core::ptr::null_mut(), Ordering::AcqRel);
    // SAFETY: the cell owned this `Arc` (`arc_cell_publish`); the caller lets it go only once no claim reads its part
    (!e.is_null()).then(|| unsafe { Arc::from_raw(e) })
}

/// D3115: `n` zeroed words for a shared sweep's table (a calloc'd vector: pages nobody writes stay unbacked).
pub(super) fn zeroed_words(n: usize) -> Box<[AtomicU64]> {
    let v = vec![0u64; n].into_boxed_slice(); // alloc: Exec
    // SAFETY: AtomicU64 has u64's size, alignment and bit validity (core::sync::atomic); the box moves whole.
    unsafe { Box::from_raw(Box::into_raw(v) as *mut [AtomicU64]) }
}

/// D3115: this source's own header over shared sweep `sw`'s words, in `region` (the source's part generation): a
/// `PROGRAM` table's, faults on until `sweep_finish`.
pub(super) fn sweep_header(region: &mut gravity_dict::Region, sw: &sweep::SharedSweep) -> core::result::Result<*const gravity_dict::table::Header, gravity_dict::BuildError> {
    use gravity_dict::table::{Header, FLAG_FAULTS};
    let hdr = region.alloc_zeroed(core::mem::size_of::<Header>(), 8)? as *mut Header;
    // SAFETY: a fresh zeroed header just reserved in the region; the words it points at live while the source pins `sw`.
    unsafe {
        (*hdr).elem = sw.elem();
        (*hdr).flags = FLAG_FAULTS;
        (*hdr).ncodes = sw.ncodes();
        (*hdr).data = gravity_abi::word(sw.data());
        (*hdr).faults = gravity_abi::word(sw.faults());
        (*hdr).base = gravity_abi::word(sw.data());
    }
    Ok(hdr)
}

/// D3115: header `h` (`sweep_header`'s, this source's) over the swept table: the fault bitmap kept, and its count set,
/// only when an entry of ANY source's chunks faulted (the kernel's done host wrote this source's own count).
pub(super) fn sweep_finish(h: *const gravity_dict::table::Header, sw: &sweep::SharedSweep) {
    use gravity_dict::table::{Header, FLAG_FAULTS};
    let total = sw.total_faults();
    let h = h as *mut Header;
    // SAFETY: this source's own header in its region; only this source's thread reads it.
    unsafe {
        if total == 0 {
            (*h).faults = gravity_abi::word(core::ptr::null());
            (*h).flags &= !FLAG_FAULTS;
            (*h).nfaults = 0;
        } else {
            (*h).faults = gravity_abi::word(sw.faults());
            (*h).flags |= FLAG_FAULTS;
            (*h).nfaults = total;
        }
    }
}

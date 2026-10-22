//! BYTE_FOR — byte-aligned frame-of-reference, the int-family primary
//! (spec §6.10; M0-S4 verdict). Frame = `[ref: i64][deltas:
//! values_in_frame × w]`, w ∈ {1..=8} elected per stream, deltas unsigned
//! LE, `value = ref +(wrapping) delta`.
//!
//! Width set: v3 shipped the pow2 widths {1,2,4,8}; the SB-3 / CMP-B
//! signed disposition (lanev4 format ledger, SIGNED 2026-08-12) adds the
//! missing byte-aligned widths {3,5,6,7} end-to-end, so the int/µs-
//! timestamp family (22-bit-seconds at µs grain ⇒ ~42-bit ranges ⇒ width
//! 6; time-of-day µs ⇒ width 5) stops falling to VERBATIM. Timestamps
//! ride this ordinary ByvalWord election — no timestamp-specific code.
//!
//! Kernel-shape laws carried (S4):
//! - one monomorphized kernel per width behind the fn-pointer vtable —
//!   never a width switch in or near the hot loop (the pow2-switch trap);
//! - flat slice-widen bodies (gate-3 ruling, M3 exit §2.3): bounds proven
//!   once per frame, then `zip(chunks_exact(W))` widening loads with no
//!   `Result` in the loop — the old format's proven NEON shape (the prior
//!   masked-u64-with-per-value-`?` body priced 0.10–0.41× against it);
//! - decode emits exact datum words (the ref is the full extended word and
//!   deltas are exact differences), so no per-value extension runs at all.
//!
//! Null slots encode delta 0 (the frame ref — spec §6.6 canonical
//! placeholder); validity is the only truth on decode.

use crate::section::{frame_base, frame_start, open_section, payload_region, read_le_w};
use pgrc_format::abi::{
    narrow_each, refuse_decode_codes, refuse_dict_handle, validity_from_ctx, CodecVtable,
    DecodeOut, EncodeInput, GranuleEncoder, KernelCtx, KernelKey, NarrowInt, NarrowOut,
    Selection, ValidityVerdict,
};
use pgrc_format::class::CLASS_BYVAL;
use pgrc_format::enc::EncodingId;
use pgrc_format::geom::FRAME_VALUES;
use pgrc_format::meta::{ProbeAnswer, MetaProbe};
use pgrc_format::part::{OverflowSink, StreamSectionWriter};
use pgrc_format::{FormatError, FormatResult};

/// Bytes of frame header (the i64 reference).
pub const FRAME_REF_LEN: usize = 8;

// ---------------------------------------------------------------------------
// analyze (election input: exact, input-decidable)
// ---------------------------------------------------------------------------

/// Smallest legal delta width for one granule's frames, or None when even
/// w=8 cannot hold a frame range (never happens: u64 ranges always fit 8).
/// `signed` picks the min/max domain; the width is a pure function of the
/// input (input-decidable election, charter §3).
pub fn granule_min_width(input: &EncodeInput<'_>, signed: bool) -> u8 {
    let mut need: u8 = 1;
    let rows = input.rows as usize;
    let mut f0 = 0usize;
    while f0 < rows {
        let n = (rows - f0).min(FRAME_VALUES as usize);
        if let Some(range) = frame_range(input, f0, n, signed) {
            let w = width_for_range(range);
            if w > need {
                need = w;
            }
        }
        f0 += n;
    }
    need
}

/// Exact BYTE_FOR payload bytes for `rows` values at width `w` (frames of
/// 1024): the election's candidate size.
pub fn payload_bytes(rows: u32, w: u8) -> usize {
    let full = (rows / FRAME_VALUES) as usize;
    let tail = (rows % FRAME_VALUES) as usize;
    let mut b = full * (FRAME_REF_LEN + FRAME_VALUES as usize * w as usize);
    if tail > 0 {
        b += FRAME_REF_LEN + tail * w as usize;
    }
    b
}

fn frame_range(input: &EncodeInput<'_>, f0: usize, n: usize, signed: bool) -> Option<u64> {
    frame_min_range(input, f0, n, signed).map(|(_, range)| range)
}

/// One frame's (min-in-domain, range) in a SINGLE walk (SEAL-FUSION: the
/// pre-fusion encode ran `frame_range` then `frame_min` — two reductions
/// over the same in-cache values). `None` = all-null frame.
fn frame_min_range(
    input: &EncodeInput<'_>,
    f0: usize,
    n: usize,
    signed: bool,
) -> Option<(u64, u64)> {
    let mut any = false;
    let (mut min_s, mut max_s) = (i64::MAX, i64::MIN);
    let (mut min_u, mut max_u) = (u64::MAX, u64::MIN);
    for r in f0..f0 + n {
        if !input.valid(r as u32) {
            continue;
        }
        any = true;
        let d = input.datums[r];
        if signed {
            min_s = min_s.min(d as i64);
            max_s = max_s.max(d as i64);
        } else {
            min_u = min_u.min(d);
            max_u = max_u.max(d);
        }
    }
    if !any {
        return None;
    }
    Some(if signed {
        (min_s as u64, (max_s as u64).wrapping_sub(min_s as u64))
    } else {
        (min_u, max_u - min_u)
    })
}

/// Smallest byte-aligned width holding `range` (full {1..=8} ladder — the
/// SB-3 widening; pure, input-decidable).
pub fn width_for_range(range: u64) -> u8 {
    if range <= 0xFF {
        1
    } else if range <= 0xFFFF {
        2
    } else if range <= 0xFF_FFFF {
        3
    } else if range <= 0xFFFF_FFFF {
        4
    } else if range <= 0xFF_FFFF_FFFF {
        5
    } else if range <= 0xFFFF_FFFF_FFFF {
        6
    } else if range <= 0xFF_FFFF_FFFF_FFFF {
        7
    } else {
        8
    }
}

// ---------------------------------------------------------------------------
// encode (spec §19.6 faces)
// ---------------------------------------------------------------------------

/// The BYTE_FOR granule encoder for byval word streams. Also reused by
/// PACKED_NUMERIC for its mantissa payload (`packednum.rs`), which is why
/// the frame emission is exposed as [`encode_i64_frames`].
pub struct ByteForEncoder {
    /// Elected delta width (stream-level; from [`granule_min_width`] over
    /// every granule).
    pub width: u8,
    /// Election domain for frame refs.
    pub signed: bool,
    /// The key this encoder writes under (BYTE_FOR for byval words;
    /// PACKED_NUMERIC delegates with its own key).
    pub key: KernelKey,
    /// SEAL-FUSION: the election's per-frame facts, consumed from `next`
    /// in frame order (None = re-reduce per frame, the pre-fusion path).
    pub carry: Option<IntCarryCursor>,
}

/// Per-frame carried facts + cursor (SEAL-FUSION; shared by the BYTE_FOR /
/// FFOR / DELTA_FOR encoders — the facts are the fused int analyzer's
/// [`crate::election::IntFrameFact`] table, frame ordinal = row-dense part
/// offset / 1024, exact at every SB-10 grain).
#[derive(Debug, Clone)]
pub struct IntCarryCursor {
    pub facts: std::sync::Arc<Vec<crate::election::IntFrameFact>>,
    pub next: usize,
}

impl IntCarryCursor {
    pub(crate) fn take(&mut self) -> FormatResult<crate::election::IntFrameFact> {
        let f = self
            .facts
            .get(self.next)
            .copied()
            .ok_or(FormatError::EncodeContract {
                detail: "int frame carry exhausted",
            })?;
        self.next += 1;
        Ok(f)
    }
}

impl ByteForEncoder {
    pub fn new_bytefor(byval_width: u8, delta_width: u8, signed: bool) -> ByteForEncoder {
        // The kernel key (and the §6.3 width byte) carry the DELTA width —
        // the dispatch axis; the byval width is immaterial to decode
        // (datum words are exact) and is recorded in the storage class.
        let _ = byval_width;
        ByteForEncoder {
            width: delta_width,
            signed,
            key: KernelKey {
                encoding: EncodingId::ByteFor.as_u16(),
                class: CLASS_BYVAL,
                width: delta_width,
            },
            carry: None,
        }
    }
}

/// Emit one granule of `rows` datum words as BYTE_FOR frames (ref + deltas)
/// at width `w`. Shared by BYTE_FOR and PACKED_NUMERIC. `carry` = the
/// election's per-frame facts (SEAL-FUSION): the ref/width reduction is a
/// table read instead of a re-walk; the emit loop and the elected-width
/// refusal are unchanged, and `verify_roundtrip` stands behind both paths.
pub fn encode_i64_frames(
    input: &EncodeInput<'_>,
    w: &mut StreamSectionWriter<'_>,
    width: u8,
    signed: bool,
) -> FormatResult<()> {
    encode_i64_frames_carried(input, w, width, signed, None)
}

/// [`encode_i64_frames`] with an optional carried-facts cursor.
pub fn encode_i64_frames_carried(
    input: &EncodeInput<'_>,
    w: &mut StreamSectionWriter<'_>,
    width: u8,
    signed: bool,
    carry: Option<&mut IntCarryCursor>,
) -> FormatResult<()> {
    encode_i64_frames_fused(input, w, width, signed, carry, None)
}

/// [`encode_i64_frames_carried`] with the SEAL-SPEED-2 fold-fusion hook:
/// the delta-emit loop feeds the meta builder per row (`obs` = the
/// streaming face, caller-opened), so the separate observe walk over the
/// same datums disappears. `None` = the classic emit; bytes identical
/// either way.
pub fn encode_i64_frames_fused(
    input: &EncodeInput<'_>,
    w: &mut StreamSectionWriter<'_>,
    width: u8,
    signed: bool,
    mut carry: Option<&mut IntCarryCursor>,
    mut obs: Option<&mut dyn pgrc_format::abi::ColumnMetaBuilder>,
) -> FormatResult<()> {
    let rows = input.rows as usize;
    if input.datums.len() < rows {
        return Err(FormatError::EncodeContract {
            detail: "datums shorter than rows",
        });
    }
    let mut f0 = 0usize;
    while f0 < rows {
        let n = (rows - f0).min(FRAME_VALUES as usize);
        // Ref = the frame's min datum word in the elected domain; an
        // all-null frame uses ref 0 (every slot is a placeholder).
        let mr: Option<(u64, u64)> = match carry.as_mut() {
            Some(c) => {
                let f = c.take()?;
                f.any.then_some((f.refw, f.range))
            }
            None => frame_min_range(input, f0, n, signed),
        };
        let refw: u64 = match mr {
            Some((minw, range)) => {
                if width_for_range(range) > width {
                    return Err(FormatError::EncodeContract {
                        detail: "frame range exceeds elected width",
                    });
                }
                minw
            }
            None => 0,
        };
        // SEAL-SPEED-2 fold fusion, SPAN grain: fold this frame's rows in
        // one virtual call (the builder's tight loop inside); the emit
        // below re-reads the same cache-warm datums.
        if let Some(o) = obs.as_deref_mut() {
            o.observe_rows(input, f0 as u32, n as u32);
        }
        w.begin_frame();
        let buf = w.payload();
        buf.extend_from_slice(&(refw as i64).to_le_bytes()); // alloc: Exec charged w.owner()
        for r in f0..f0 + n {
            let delta = if input.valid(r as u32) {
                input.datums[r].wrapping_sub(refw)
            } else {
                0 // canonical placeholder (spec §6.6)
            };
            buf.extend_from_slice(&delta.to_le_bytes()[..width as usize]); // alloc: Exec charged w.owner()
        }
        f0 += n;
    }
    w.end_granule(input.rows);
    Ok(())
}

impl GranuleEncoder for ByteForEncoder {
    fn key(&self) -> KernelKey {
        self.key
    }

    fn encode_granule(
        &mut self,
        input: &EncodeInput<'_>,
        w: &mut StreamSectionWriter<'_>,
        _ovf: &mut OverflowSink<'_>,
    ) -> FormatResult<()> {
        encode_i64_frames_carried(input, w, self.width, self.signed, self.carry.as_mut())
    }

    /// SEAL-SPEED-2 fold fusion: the delta emit walks the datums row-dense
    /// anyway — the folds ride it (the observe walk disappears).
    fn encode_granule_observed(
        &mut self,
        input: &EncodeInput<'_>,
        w: &mut StreamSectionWriter<'_>,
        ovf: &mut OverflowSink<'_>,
        meta: &mut dyn pgrc_format::abi::ColumnMetaBuilder,
        granule: u32,
    ) -> FormatResult<()> {
        if !meta.row_observe_supported() {
            self.encode_granule(input, w, ovf)?;
            meta.observe_granule(input, granule);
            return Ok(());
        }
        meta.begin_granule_rows(granule);
        encode_i64_frames_fused(
            input,
            w,
            self.width,
            self.signed,
            self.carry.as_mut(),
            Some(meta),
        )?;
        meta.end_granule_rows(input.rows);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// decode kernels (monomorphized per delta width W)
// ---------------------------------------------------------------------------

/// Shared open: the granule's first frame index + the payload slice.
struct ByteForView<'a> {
    payload: &'a [u8],
    frame_table: Option<&'a [u32]>,
    fbase: u32,
    values: u32,
}

fn open_view<'a>(ctx: &KernelCtx<'a>, encoding: u16) -> FormatResult<ByteForView<'a>> {
    let hdr = open_section(ctx.bytes, encoding)?;
    Ok(ByteForView {
        payload: payload_region(&hdr, ctx.bytes)?,
        frame_table: ctx.frame_table,
        fbase: frame_base(&hdr, ctx.bytes, ctx.granule_in_extent)?,
        values: ctx.values,
    })
}

/// A granule's BYTE_FOR-framed values (BYTE_FOR or PACKED_NUMERIC) resolved by a caller that keeps its extent parsed
/// (pgrc_read `StreamCursor::framed`: the section header was checked against the stream entry when the extent loaded):
/// the payload region, the extent's frame table, the granule's first frame and its value count, which `open_view` derives
/// from a ctx's section header on every call. The kernels over it prove the same bounds per frame.
#[derive(Clone, Copy, Debug)]
pub struct Framed<'a> {
    pub payload: &'a [u8],
    pub frame_table: &'a [u32],
    pub fbase: u32,
    pub values: u32,
}

impl<'a> Framed<'a> {
    /// The same granule through a ctx's section header (`open_view`): what `StreamCursor::framed` must equal.
    pub fn of_ctx(ctx: &KernelCtx<'a>, encoding: u16) -> FormatResult<Framed<'a>> {
        let v = open_view(ctx, encoding)?;
        let frame_table = v.frame_table.ok_or(FormatError::Corrupt {
            at: "missing frame table",
        })?;
        Ok(Framed {
            payload: v.payload,
            frame_table,
            fbase: v.fbase,
            values: v.values,
        })
    }

    fn view(&self) -> ByteForView<'a> {
        ByteForView {
            payload: self.payload,
            frame_table: Some(self.frame_table),
            fbase: self.fbase,
            values: self.values,
        }
    }
}

/// A selected value's store: the datum word (`u64`), or the lane's narrow int (the reference narrowed, the delta added at
/// that width: `NarrowInt`'s law, equal to the word's low bytes).
pub trait SelWord: Copy {
    fn of(refw: u64, delta: u64) -> Self;
}

impl SelWord for u64 {
    #[inline(always)]
    fn of(refw: u64, delta: u64) -> u64 {
        refw.wrapping_add(delta)
    }
}

impl SelWord for i32 {
    #[inline(always)]
    fn of(refw: u64, delta: u64) -> i32 {
        <i32 as NarrowInt>::of_word(refw).add_word(delta)
    }
}

impl SelWord for i16 {
    #[inline(always)]
    fn of(refw: u64, delta: u64) -> i16 {
        <i16 as NarrowInt>::of_word(refw).add_word(delta)
    }
}

/// The selected rows of a resolved granule, each value at its own row (`out[r]` for every selected `r`; `out` spans the
/// rows up to the last selected one, the others untouched), at delta width `width` (1..=8): a lane's gather and scatter in
/// one pass with no ctx and no section header parse (TH14's source lever, main 09-24).
pub fn sel_at_framed<T: SelWord>(f: &Framed<'_>, width: u8, sel: &Selection<'_>, out: &mut [T]) -> FormatResult<u32> {
    let v = f.view();
    match width {
        1 => dec_sel_into::<1, true, T>(&v, sel, out),
        2 => dec_sel_into::<2, true, T>(&v, sel, out),
        3 => dec_sel_into::<3, true, T>(&v, sel, out),
        4 => dec_sel_into::<4, true, T>(&v, sel, out),
        5 => dec_sel_into::<5, true, T>(&v, sel, out),
        6 => dec_sel_into::<6, true, T>(&v, sel, out),
        7 => dec_sel_into::<7, true, T>(&v, sel, out),
        8 => dec_sel_into::<8, true, T>(&v, sel, out),
        _ => Err(FormatError::Corrupt {
            at: "byte-for width",
        }),
    }
}

/// A resolved granule's frames in row order at delta width `width` (1..=8): `each(first_row, reference, deltas)` per frame,
/// `deltas` its `n × width` stored bytes (`n` its values), its address and body proven inside the payload. A consumer
/// that compares values on their deltas (gravity's row filter, D3223) reads them through this. Stops at the first `false`
/// (`Ok(false)`).
pub fn each_frame<'a>(f: &Framed<'a>, width: u8, mut each: impl FnMut(usize, u64, &'a [u8]) -> bool) -> FormatResult<bool> {
    if !(1..=8).contains(&width) {
        return Err(FormatError::Corrupt { at: "byte-for width" });
    }
    let (w, values) = (width as usize, f.values as usize);
    let (mut r, mut fi) = (0usize, 0u32);
    while r < values {
        let n = (values - r).min(FRAME_VALUES as usize);
        let fs = frame_start(Some(f.frame_table), f.fbase + fi)?;
        let refw = load_frame_ref(f.payload, fs)?;
        let base = fs + FRAME_REF_LEN;
        let body = f.payload.get(base..base + n * w).ok_or(FormatError::Bounds { at: "byte-for frame payload" })?;
        if !each(r, refw, body) {
            return Ok(false);
        }
        r += n;
        fi += 1;
    }
    Ok(true)
}

/// Decode all values of the granule at delta width W. Gate-3 kernel shape
/// (M3 exit §2.3 ruling): frame addressing is resolved ONCE per frame, the
/// bounds are proven ONCE per frame, and the inner loop is a flat
/// `zip(chunks_exact(W))` slice widen with no `Result` and no per-value
/// bounds check — the old format's proven NEON idiom (`pgrcolumnar`
/// reader.rs widen loops). The prior body's per-value `load_u64_le(..)?`
/// carried an early-exit branch per iteration that blocked vectorization
/// (0.10–0.41× vs old at the decode_full face).
fn dec_full_width<const W: usize>(v: &ByteForView<'_>, out: &mut [u64]) -> FormatResult<u32> {
    if out.len() < v.values as usize {
        return Err(FormatError::Bounds {
            at: "decode_full out",
        });
    }
    let mut r = 0usize;
    let mut f = 0u32;
    while r < v.values as usize {
        let n = (v.values as usize - r).min(FRAME_VALUES as usize);
        let fs = frame_start(v.frame_table, v.fbase + f)?;
        let refw = load_frame_ref(v.payload, fs)?;
        let base = fs + FRAME_REF_LEN;
        let body = frame_run::<W>(v.payload, base, n)?;
        if W == 1 {
            // chunks_exact(1) carries remainder plumbing the vectorizer
            // dislikes; the byte iterator is the clean u8 widen.
            for (o, &b) in out[r..r + n].iter_mut().zip(body[..n].iter()) {
                *o = refw.wrapping_add(b as u64);
            }
        } else {
            widen_into::<W, u64>(body, &mut out[r..r + n], |d| refw.wrapping_add(d));
        }
        r += n;
        f += 1;
    }
    Ok(v.values)
}

/// `dec_full_width` into a lane of the column's width: the frame's reference
/// narrowed once, each delta added at that width — the same flat
/// `zip(chunks_exact(W))` body, half or a quarter of the stores.
fn dec_narrow_width<const W: usize, T: NarrowInt>(v: &ByteForView<'_>, out: &mut [T]) -> FormatResult<u32> {
    if out.len() < v.values as usize {
        return Err(FormatError::Bounds {
            at: "decode_narrow out",
        });
    }
    let mut r = 0usize;
    let mut f = 0u32;
    while r < v.values as usize {
        let n = (v.values as usize - r).min(FRAME_VALUES as usize);
        let fs = frame_start(v.frame_table, v.fbase + f)?;
        let base_v = T::of_word(load_frame_ref(v.payload, fs)?);
        let base = fs + FRAME_REF_LEN;
        let body = frame_run::<W>(v.payload, base, n)?;
        if W == 1 {
            for (o, &b) in out[r..r + n].iter_mut().zip(body[..n].iter()) {
                *o = base_v.add_word(b as u64);
            }
        } else {
            widen_into::<W, T>(body, &mut out[r..r + n], |d| base_v.add_word(d));
        }
        r += n;
        f += 1;
    }
    Ok(v.values)
}

/// `out.len()` consecutive `W`-byte LE values from the head of `src` (`src.len() >= out.len() * W`), zero-extended,
/// each stored as `f(raw)`. The pow2 widths take `read_le_w` (one plain load). The SB-3 widths (3/5/6/7) read each value
/// as ONE unaligned 8-byte load masked to its `W` bytes — the load runs into the bytes after the value (the next values,
/// the next frame's reference), which `src` must hold for it: the caller passes the payload through its end, not the
/// frame body. Groups of 8 values prove their whole span in bounds once (no per-value check); only the values past the
/// last whole group that fits — a frame's last `n % 8`, and the last group before the payload ends — take the
/// byte-assembled `read_le_w`. PGRC-DECODE (main 09-24): the byte assembly was 11 instructions per value
/// at W = 7 (a 64-bit id column), the masked load is 4.
#[inline(always)]
pub(crate) fn widen_into<const W: usize, T>(src: &[u8], out: &mut [T], f: impl Fn(u64) -> T) {
    let n = out.len();
    if matches!(W, 1 | 2 | 4 | 8) {
        for (o, c) in out.iter_mut().zip(src[..n * W].chunks_exact(W)) {
            *o = f(read_le_w::<W>(c));
        }
        return;
    }
    const G: usize = 8;
    let mask = u64::MAX >> (64 - 8 * W);
    // The bytes a group's 8 masked loads touch: 7 values' strides plus one whole word.
    let span = (G - 1) * W + 8;
    let groups = if src.len() >= span { ((src.len() - span) / (G * W) + 1).min(n / G) } else { 0 };
    let (head, tail) = out.split_at_mut(groups * G);
    for (gi, oc) in head.chunks_exact_mut(G).enumerate() {
        let g = &src[gi * G * W..gi * G * W + span];
        let oc: &mut [T; G] = oc.try_into().expect("chunk of G");
        for j in 0..G {
            oc[j] = f(u64::from_le_bytes(g[j * W..j * W + 8].try_into().expect("len 8")) & mask);
        }
    }
    for (o, c) in tail.iter_mut().zip(src[groups * G * W..n * W].chunks_exact(W)) {
        *o = f(read_le_w::<W>(c));
    }
}

/// A frame's `n` deltas at `base`, proven inside the payload, returned as the payload from `base` through its end: the
/// SB-3 widths' masked 8-byte loads (`widen_into`) read past a value into the bytes that follow it.
#[inline(always)]
fn frame_run<const W: usize>(payload: &[u8], base: usize, n: usize) -> FormatResult<&[u8]> {
    match payload.get(base..) {
        Some(run) if run.len() >= n * W => Ok(run),
        _ => Err(FormatError::Bounds {
            at: "byte-for frame payload",
        }),
    }
}

fn load_frame_ref(payload: &[u8], fs: usize) -> FormatResult<u64> {
    let b = payload
        .get(fs..fs + FRAME_REF_LEN)
        .ok_or(FormatError::Bounds {
            at: "byte-for frame ref",
        })?;
    Ok(u64::from_le_bytes(b.try_into().expect("len 8")))
}

/// Survivor-only completion, one frame at a time: the frame's address, reference and body bounds are proven once, then
/// its run of survivors reads with no `Result` in the loop (TH06 / TH14's late lanes). The selection is strictly
/// ascending (`abi::Selection`), so its last row bounds every row and a frame's survivors are one run. `AT`: each value
/// lands at its own row (`out` spans the rows up to the last selected one; the others untouched), the gather and the
/// caller's scatter in one pass; else dense in selection order.
fn dec_sel_width<const W: usize, const AT: bool>(
    v: &ByteForView<'_>,
    sel: &Selection<'_>,
    out: &mut [u64],
) -> FormatResult<u32> {
    dec_sel_into::<W, AT, u64>(v, sel, out)
}

/// `dec_sel_width` at any store (`SelWord`): the one body, monomorphized per width, placement and store.
#[inline(always)]
fn dec_sel_into<const W: usize, const AT: bool, T: SelWord>(
    v: &ByteForView<'_>,
    sel: &Selection<'_>,
    out: &mut [T],
) -> FormatResult<u32> {
    let rows = sel.rows;
    let need = if AT { rows.last().map_or(0, |&r| r as usize + 1) } else { rows.len() };
    if out.len() < need {
        return Err(FormatError::Bounds {
            at: "decode_sel out",
        });
    }
    if rows.last().is_some_and(|&r| r as u32 >= v.values) {
        return Err(FormatError::Bounds {
            at: "decode_sel row",
        });
    }
    let mut i = 0;
    while i < rows.len() {
        let f = rows[i] as u32 / FRAME_VALUES;
        let f0 = f * FRAME_VALUES;
        let n = (v.values - f0).min(FRAME_VALUES);
        let fs = frame_start(v.frame_table, v.fbase + f)?;
        let refw = load_frame_ref(v.payload, fs)?;
        let base = fs + FRAME_REF_LEN;
        let body = v
            .payload
            .get(base..base + n as usize * W)
            .ok_or(FormatError::Bounds {
                at: "byte-for frame payload",
            })?;
        let end = f0 + n;
        while let Some(&r) = rows.get(i).filter(|&&r| (r as u32) < end) {
            let k = (r as u32 - f0) as usize * W;
            out[if AT { r as usize } else { i }] = T::of(refw, read_le_w::<W>(&body[k..k + W]));
            i += 1;
        }
    }
    Ok(rows.len() as u32)
}

fn bf_dec_full<const W: usize>(ctx: &KernelCtx<'_>, out: &mut DecodeOut<'_>) -> FormatResult<u32> {
    let v = open_view(ctx, EncodingId::ByteFor.as_u16())?;
    dec_full_width::<W>(&v, out.datums)
}

fn bf_dec_sel<const W: usize>(
    ctx: &KernelCtx<'_>,
    sel: &Selection<'_>,
    out: &mut DecodeOut<'_>,
) -> FormatResult<u32> {
    let v = open_view(ctx, EncodingId::ByteFor.as_u16())?;
    dec_sel_width::<W, false>(&v, sel, out.datums)
}

fn bf_dec_narrow<const W: usize>(ctx: &KernelCtx<'_>, out: NarrowOut<'_>) -> FormatResult<u32> {
    let v = open_view(ctx, EncodingId::ByteFor.as_u16())?;
    narrow_each(out, |o| dec_narrow_width::<W, i16>(&v, o), |o| dec_narrow_width::<W, i32>(&v, o))
}

fn bf_meta(ctx: &KernelCtx<'_>, probe: &MetaProbe) -> FormatResult<ProbeAnswer> {
    Ok(match probe {
        MetaProbe::RowCount => ProbeAnswer::Count(ctx.rows as u64),
        MetaProbe::NonNullCount => {
            let mut scratch = [0u64; (pgrc_format::geom::GRANULE_ROWS as usize).div_ceil(64)];
            match validity_from_ctx(ctx, &mut scratch)? {
                ValidityVerdict::AllValid => ProbeAnswer::Count(ctx.rows as u64),
                ValidityVerdict::Mixed { nonnull } => ProbeAnswer::Count(nonnull as u64),
            }
        }
        _ => ProbeAnswer::Absent,
    })
}

fn bf_validity(ctx: &KernelCtx<'_>, out: &mut [u64]) -> FormatResult<ValidityVerdict> {
    validity_from_ctx(ctx, out)
}

/// The PACKED_NUMERIC payload shares this frame layout; its kernels reuse
/// these bodies through open-by-id (`packednum.rs`).
pub(crate) fn dec_full_for<const W: usize>(
    ctx: &KernelCtx<'_>,
    encoding: u16,
    out: &mut [u64],
) -> FormatResult<u32> {
    let v = open_view(ctx, encoding)?;
    dec_full_width::<W>(&v, out)
}

pub(crate) fn dec_sel_for<const W: usize>(
    ctx: &KernelCtx<'_>,
    encoding: u16,
    sel: &Selection<'_>,
    out: &mut [u64],
) -> FormatResult<u32> {
    let v = open_view(ctx, encoding)?;
    dec_sel_width::<W, false>(&v, sel, out)
}

/// `dec_sel_for` with each value at its own row (`out[r]`, the rows up to the last selected one).
pub(crate) fn dec_sel_at_for<const W: usize>(
    ctx: &KernelCtx<'_>,
    encoding: u16,
    sel: &Selection<'_>,
    out: &mut [u64],
) -> FormatResult<u32> {
    let v = open_view(ctx, encoding)?;
    dec_sel_width::<W, true>(&v, sel, out)
}

// ---------------------------------------------------------------------------
// vtables: (BYTE_FOR × byval width × delta width) — the stream entry's
// `width` byte carries the DELTA width (spec §6.3 for BYTE_FOR), so the
// kernel key is (encoding=2, class=0, width=delta_w) and the byval width is
// immaterial to decode (datum words are exact).
// ---------------------------------------------------------------------------

const fn bf_vt<const W: usize>() -> CodecVtable {
    CodecVtable {
        key: KernelKey {
            encoding: EncodingId::ByteFor as u16,
            class: CLASS_BYVAL,
            width: W as u8,
        },
        decode_full: bf_dec_full::<W>,
        decode_sel: bf_dec_sel::<W>,
        decode_codes: refuse_decode_codes,
        dict_handle: refuse_dict_handle,
        meta_probe: bf_meta,
        validity: bf_validity,
        decode_narrow: bf_dec_narrow::<W>,
    }
}

/// One vtable per legal delta width — the full {1..=8} ladder (SB-3), each
/// entry a DISTINCT monomorphized kernel pair (pinned by
/// `tests/dispatch_shape.rs`).
pub static VT_BYTE_FOR: [CodecVtable; 8] = [
    bf_vt::<1>(),
    bf_vt::<2>(),
    bf_vt::<3>(),
    bf_vt::<4>(),
    bf_vt::<5>(),
    bf_vt::<6>(),
    bf_vt::<7>(),
    bf_vt::<8>(),
];

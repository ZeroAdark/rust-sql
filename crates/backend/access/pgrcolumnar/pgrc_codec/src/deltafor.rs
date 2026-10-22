//! DELTA_FOR — serial delta, cold/size election ONLY (spec §4 id 4; the
//! S4 verdict struck transposed-delta from hot elections and serial delta
//! stays refused for the hot tier — this encoding exists purely as a size
//! arm for cold/loser chunks, and the election never offers it to a fused
//! path).
//!
//! Payload layout (delegated to M3-C by spec §4): per 1024-value frame —
//!
//! ```text
//! frame := first i64 | width u8 | pad [u8;7] | zz-deltas (values × width)
//! ```
//!
//! `first` is the frame's first non-placeholder-relative datum word; slot i
//! stores zigzag(v[i] − v[i−1]) (slot 0 stores 0), so decode is a serial
//! prefix sum — inherently sequential, which is exactly why S4 refused it
//! for hot decode. Width is per-frame (unlike BYTE_FOR's per-stream width);
//! the kernel key uses width 0. Null slots store delta 0 (the previous
//! value — a canonical placeholder per spec §6.6).

use crate::section::{frame_base, frame_start, open_section, payload_region, read_le_w};
use pgrc_format::abi::{
    narrow_each, NarrowInt, NarrowOut,
    refuse_decode_codes, refuse_dict_handle, validity_from_ctx, CodecVtable, DecodeOut,
    EncodeInput, GranuleEncoder, KernelCtx, KernelKey, Selection, ValidityVerdict,
};
use pgrc_format::class::CLASS_BYVAL;
use pgrc_format::enc::EncodingId;
use pgrc_format::geom::{FRAME_VALUES, GRANULE_ROWS};
use pgrc_format::meta::{ProbeAnswer, MetaProbe};
use pgrc_format::part::{OverflowSink, StreamSectionWriter};
use pgrc_format::{FormatError, FormatResult};

pub(crate) const FRAME_HDR_LEN: usize = 16; // first(8) + width(1) + pad(7)

#[inline]
pub(crate) fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
fn unzigzag(z: u64) -> i64 {
    ((z >> 1) as i64) ^ -((z & 1) as i64)
}

pub(crate) fn width_for(z: u64) -> u8 {
    if z <= u8::MAX as u64 {
        1
    } else if z <= u16::MAX as u64 {
        2
    } else if z <= u32::MAX as u64 {
        4
    } else {
        8
    }
}

/// Exact payload bytes this encoding would use for one granule — the
/// election's size arm (input-decidable, no emission).
pub fn granule_payload_bytes(input: &EncodeInput<'_>) -> usize {
    let rows = input.rows as usize;
    let mut total = 0usize;
    let mut f0 = 0usize;
    while f0 < rows {
        let n = (rows - f0).min(FRAME_VALUES as usize);
        total += FRAME_HDR_LEN + n * frame_width(input, f0, n) as usize;
        f0 += n;
    }
    total
}

fn frame_width(input: &EncodeInput<'_>, f0: usize, n: usize) -> u8 {
    let mut prev = first_value(input, f0, n);
    let mut w: u8 = 1;
    for r in f0..f0 + n {
        if !input.valid(r as u32) {
            continue;
        }
        let v = input.datums[r] as i64;
        let z = zigzag(v.wrapping_sub(prev));
        let need = width_for(z);
        if need > w {
            w = need;
        }
        prev = v;
    }
    w
}

fn first_value(input: &EncodeInput<'_>, f0: usize, n: usize) -> i64 {
    for r in f0..f0 + n {
        if input.valid(r as u32) {
            return input.datums[r] as i64;
        }
    }
    0
}

#[derive(Default)]
pub struct DeltaForEncoder {
    /// SEAL-FUSION: the election's per-frame (width, first) facts —
    /// computed only under the cold posture, which is the only posture
    /// that can elect DELTA_FOR. None = re-reduce (pre-fusion path).
    pub carry: Option<crate::bytefor::IntCarryCursor>,
}

impl GranuleEncoder for DeltaForEncoder {
    fn key(&self) -> KernelKey {
        KernelKey {
            encoding: EncodingId::DeltaFor.as_u16(),
            class: CLASS_BYVAL,
            width: 0,
        }
    }

    fn encode_granule(
        &mut self,
        input: &EncodeInput<'_>,
        w: &mut StreamSectionWriter<'_>,
        _ovf: &mut OverflowSink<'_>,
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
            let (width, first) = match self.carry.as_mut() {
                Some(c) => {
                    let f = c.take()?;
                    // `frame_width`/`first_value` semantics carried:
                    // all-null = (1, 0).
                    (f.df_width, f.df_first)
                }
                None => (frame_width(input, f0, n), first_value(input, f0, n)),
            };
            w.begin_frame();
            let buf = w.payload();
            buf.extend_from_slice(&first.to_le_bytes()); // alloc: Exec charged w.owner()
            buf.push(width); // alloc: Exec charged w.owner()
            buf.extend_from_slice(&[0u8; 7]); // alloc: Exec charged w.owner()
            let mut prev = first;
            for r in f0..f0 + n {
                let z = if input.valid(r as u32) {
                    let v = input.datums[r] as i64;
                    let z = zigzag(v.wrapping_sub(prev));
                    prev = v;
                    z
                } else {
                    0 // placeholder: repeat the previous value (spec §6.6)
                };
                buf.extend_from_slice(&z.to_le_bytes()[..width as usize]); // alloc: Exec charged w.owner()
            }
            f0 += n;
        }
        w.end_granule(input.rows);
        Ok(())
    }

    /// SEAL-SPEED-2 fold fusion: the zigzag emit walks the datums row-dense
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
        let rows = input.rows as usize;
        if input.datums.len() < rows {
            return Err(FormatError::EncodeContract {
                detail: "datums shorter than rows",
            });
        }
        meta.begin_granule_rows(granule);
        let mut f0 = 0usize;
        while f0 < rows {
            let n = (rows - f0).min(FRAME_VALUES as usize);
            let (width, first) = match self.carry.as_mut() {
                Some(c) => {
                    let f = c.take()?;
                    (f.df_width, f.df_first)
                }
                None => (frame_width(input, f0, n), first_value(input, f0, n)),
            };
            // SEAL-SPEED-2 fold fusion, SPAN grain (see bytefor).
            meta.observe_rows(input, f0 as u32, n as u32);
            w.begin_frame();
            let buf = w.payload();
            buf.extend_from_slice(&first.to_le_bytes()); // alloc: Exec charged w.owner()
            buf.push(width); // alloc: Exec charged w.owner()
            buf.extend_from_slice(&[0u8; 7]); // alloc: Exec charged w.owner()
            let mut prev = first;
            for r in f0..f0 + n {
                let z = if input.valid(r as u32) {
                    let v = input.datums[r] as i64;
                    let z = zigzag(v.wrapping_sub(prev));
                    prev = v;
                    z
                } else {
                    0 // placeholder: repeat the previous value (spec §6.6)
                };
                buf.extend_from_slice(&z.to_le_bytes()[..width as usize]); // alloc: Exec charged w.owner()
            }
            f0 += n;
        }
        meta.end_granule_rows(input.rows);
        w.end_granule(input.rows);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// decode (serial prefix sum — cold path by design)
// ---------------------------------------------------------------------------

/// Decode one frame into `buf[..n]`. Slot 0's stored delta is 0 by
/// construction; a nonzero one is structurally tolerated (it still decodes
/// deterministically from `first` — `prev` starts at `first`, so the
/// anchor case is the same wrapping add and needs no branch).
///
/// Gate-3 kernel shape (M3 exit §2.3 ruling): the per-frame width byte is
/// dispatched ONCE per frame to a width-monomorphized body whose inner
/// loop is a flat `zip(chunks_exact(W))` walk — no `Result`, no
/// runtime-width `read_le` memcpy per value. The prefix sum itself stays
/// serial (inherent to the encoding; S4 struck it from hot tiers).
fn dec_frame(payload: &[u8], fs: usize, n: usize, buf: &mut [u64]) -> FormatResult<()> {
    dec_frame_prefix(payload, fs, n, n, buf)
}

/// The first `need` values of a frame of `n` (`need <= n`): the frame's bounds
/// are checked for all `n`, as a whole-frame decode checks them.
fn dec_frame_prefix(payload: &[u8], fs: usize, n: usize, need: usize, buf: &mut [u64]) -> FormatResult<()> {
    debug_assert!(need <= n);
    let hdr = payload
        .get(fs..fs + FRAME_HDR_LEN)
        .ok_or(FormatError::Bounds {
            at: "delta-for frame header",
        })?;
    let first = i64::from_le_bytes(hdr[..8].try_into().expect("len 8"));
    let width = hdr[8] as usize;
    if !matches!(width, 1 | 2 | 4 | 8) {
        return Err(FormatError::Corrupt {
            at: "delta-for width",
        });
    }
    let base = fs + FRAME_HDR_LEN;
    let body = payload
        .get(base..base + n * width)
        .ok_or(FormatError::Bounds {
            at: "delta-for frame payload",
        })?;
    match width {
        1 => dec_frame_width::<1>(first, body, &mut buf[..need]),
        2 => dec_frame_width::<2>(first, body, &mut buf[..need]),
        4 => dec_frame_width::<4>(first, body, &mut buf[..need]),
        _ => dec_frame_width::<8>(first, body, &mut buf[..need]),
    }
    Ok(())
}

fn dec_frame_width<const W: usize>(first: i64, body: &[u8], out: &mut [u64]) {
    let mut prev = first;
    for (o, c) in out.iter_mut().zip(body.chunks_exact(W)) {
        let v = prev.wrapping_add(unzigzag(read_le_w::<W>(c)));
        *o = v as u64;
        prev = v;
    }
}

fn df_dec_full(ctx: &KernelCtx<'_>, out: &mut DecodeOut<'_>) -> FormatResult<u32> {
    let hdr = open_section(ctx.bytes, EncodingId::DeltaFor.as_u16())?;
    let payload = payload_region(&hdr, ctx.bytes)?;
    let fbase = frame_base(&hdr, ctx.bytes, ctx.granule_in_extent)?;
    if out.datums.len() < ctx.values as usize {
        return Err(FormatError::Bounds {
            at: "decode_full out",
        });
    }
    let mut r = 0u32;
    let mut f = 0u32;
    while r < ctx.values {
        let n = (ctx.values - r).min(FRAME_VALUES);
        let fs = frame_start(ctx.frame_table, fbase + f)?;
        dec_frame(
            payload,
            fs,
            n as usize,
            &mut out.datums[r as usize..(r + n) as usize],
        )?;
        r += n;
        f += 1;
    }
    Ok(ctx.values)
}

fn df_narrow<T: NarrowInt>(ctx: &KernelCtx<'_>, out: &mut [T]) -> FormatResult<u32> {
    let hdr = open_section(ctx.bytes, EncodingId::DeltaFor.as_u16())?;
    let payload = payload_region(&hdr, ctx.bytes)?;
    let fbase = frame_base(&hdr, ctx.bytes, ctx.granule_in_extent)?;
    if out.len() < ctx.values as usize {
        return Err(FormatError::Bounds {
            at: "decode_narrow out",
        });
    }
    let mut buf = [0u64; FRAME_VALUES as usize];
    let mut r = 0u32;
    let mut f = 0u32;
    while r < ctx.values {
        let n = (ctx.values - r).min(FRAME_VALUES) as usize;
        let fs = frame_start(ctx.frame_table, fbase + f)?;
        dec_frame(payload, fs, n, &mut buf)?;
        for (o, &w) in out[r as usize..r as usize + n].iter_mut().zip(buf[..n].iter()) {
            *o = T::of_word(w);
        }
        r += n as u32;
        f += 1;
    }
    Ok(ctx.values)
}

fn df_dec_narrow(ctx: &KernelCtx<'_>, out: NarrowOut<'_>) -> FormatResult<u32> {
    narrow_each(out, |o| df_narrow::<i16>(ctx, o), |o| df_narrow::<i32>(ctx, o))
}

fn df_dec_sel(
    ctx: &KernelCtx<'_>,
    sel: &Selection<'_>,
    out: &mut DecodeOut<'_>,
) -> FormatResult<u32> {
    let hdr = open_section(ctx.bytes, EncodingId::DeltaFor.as_u16())?;
    let payload = payload_region(&hdr, ctx.bytes)?;
    let fbase = frame_base(&hdr, ctx.bytes, ctx.granule_in_extent)?;
    if out.datums.len() < sel.rows.len() {
        return Err(FormatError::Bounds {
            at: "decode_sel out",
        });
    }
    // Serial encoding: each run of selected rows in one frame decodes that
    // frame once into a stack buffer, and only up to the run's furthest row
    // (IXPERF F1: a point fetch's one row costs its prefix, not the frame);
    // a frame with no survivors is never decoded.
    let mut buf = [0u64; FRAME_VALUES as usize];
    let mut i = 0usize;
    while i < sel.rows.len() {
        let f = sel.rows[i] as u32 / FRAME_VALUES;
        let mut j = i;
        let mut need = 0usize;
        while j < sel.rows.len() && sel.rows[j] as u32 / FRAME_VALUES == f {
            if sel.rows[j] as u32 >= ctx.values {
                return Err(FormatError::Bounds {
                    at: "decode_sel row",
                });
            }
            need = need.max((sel.rows[j] as u32 % FRAME_VALUES) as usize + 1);
            j += 1;
        }
        let n = (ctx.values - f * FRAME_VALUES).min(FRAME_VALUES) as usize;
        let fs = frame_start(ctx.frame_table, fbase + f)?;
        dec_frame_prefix(payload, fs, n, need, &mut buf)?;
        for k in i..j {
            out.datums[k] = buf[(sel.rows[k] as u32 % FRAME_VALUES) as usize];
        }
        i = j;
    }
    Ok(sel.rows.len() as u32)
}

fn df_meta(ctx: &KernelCtx<'_>, probe: &MetaProbe) -> FormatResult<ProbeAnswer> {
    Ok(match probe {
        MetaProbe::RowCount => ProbeAnswer::Count(ctx.rows as u64),
        MetaProbe::NonNullCount => {
            let mut scratch = [0u64; (GRANULE_ROWS as usize).div_ceil(64)];
            match validity_from_ctx(ctx, &mut scratch)? {
                ValidityVerdict::AllValid => ProbeAnswer::Count(ctx.rows as u64),
                ValidityVerdict::Mixed { nonnull } => ProbeAnswer::Count(nonnull as u64),
            }
        }
        _ => ProbeAnswer::Absent,
    })
}

fn df_validity(ctx: &KernelCtx<'_>, out: &mut [u64]) -> FormatResult<ValidityVerdict> {
    validity_from_ctx(ctx, out)
}

pub static VT_DELTA_FOR: CodecVtable = CodecVtable {
    key: KernelKey {
        encoding: EncodingId::DeltaFor as u16,
        class: CLASS_BYVAL,
        width: 0,
    },
    decode_full: df_dec_full,
    decode_sel: df_dec_sel,
    decode_codes: refuse_decode_codes,
    dict_handle: refuse_dict_handle,
    meta_probe: df_meta,
    validity: df_validity,
    decode_narrow: df_dec_narrow,
};

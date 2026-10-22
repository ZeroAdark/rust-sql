//! The integer pre-pass wrapper ([`Wrapper::IntPre`], id 8), disk-only like the plain zstd wrapper. Per granule block the
//! encoded image is read as integer sequences through a [`View`] picked from the section's encoding and width; each
//! sequence is re-coded by the smallest of patched base, delta + scale + zigzag, RLE and linear runs when that shrinks it
//! by the factor (`PGRUST_PGRC_INTPRE_FACTOR`, default 2), then zstd. A block keeps whichever of that and the plain zstd
//! block is smaller (a section's first blocks settle whether the rest price both), and unwrap rebuilds the exact image.
//!
//! Block: `mode u8`; mode 0 = one zstd frame of the image; mode 1 = `image_len u32 | pre_len u32 | zstd frame of the pre
//! image`. Pre image: `view u8 | width u8 | view body`. A sequence is `scheme u8 | body`: 0 plain (`n x width` LE);
//! 1 patched base `E(n)`; 2 delta `first u64 | scale u64 | E(n-1)` of zigzag(delta / scale); 3 RLE `runs u32 | E` of
//! lengths-1 `| E` of values; 4 linear runs `runs u32 | E` of lengths-1 `| E` of starts `| E` of zigzag strides.
//! `E(k) = base u64 | b u8 | x u32 | k x b low bytes of (v - base)`, then for x exceptions `E(x)` of position gaps and
//! `E(x)` of the high parts `(v - base) >> 8b`, nested at most [`MAX_DEPTH`] deep.

use crate::wrapper::{zstd_compress_block_at, zstd_decompress_block};
use pgrc_format::enc::{EncodingId, Wrapper};
use pgrc_format::geom::FRAME_VALUES;
use pgrc_format::part::{StreamSectionHdr, STREAM_SECTION_HDR_LEN};
use pgrc_format::{try_filled, try_vec, FormatError, FormatResult};
use std::cell::Cell;

pub const MAX_DEPTH: u32 = 6;
const FV: usize = FRAME_VALUES as usize;
const PLAIN: u8 = 0;
const PFOR: u8 = 1;
const DELTA: u8 = 2;
const RLE: u8 = 3;
const LINEAR: u8 = 4;

thread_local! {
    static FORCE: Cell<u8> = const { Cell::new(0) };
}

/// `PGRUST_PGRC_INTPRE` (read once; on unless `0`): seals offer the pre-pass on eligible value streams.
pub fn on() -> bool {
    match FORCE.with(Cell::get) {
        1 => false,
        2 => true,
        _ => {
            static ON: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
            *ON.get_or_init(|| std::env::var("PGRUST_PGRC_INTPRE").map_or(true, |v| v.trim() != "0"))
        }
    }
}

/// Test face: override [`on`] on this thread (`None` = the environment again).
#[doc(hidden)]
pub fn force(on: Option<bool>) {
    FORCE.with(|f| f.set(on.map_or(0, |o| 1 + o as u8)));
}

/// The shrink factor a sequence's coding must reach, in hundredths (`PGRUST_PGRC_INTPRE_FACTOR`, 1..=100, default 2).
pub fn factor_x100() -> u32 {
    static F: pgsync::OnceLock<u32> = pgsync::OnceLock::new();
    *F.get_or_init(|| {
        std::env::var("PGRUST_PGRC_INTPRE_FACTOR")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|f| f.is_finite() && (1.0..=100.0).contains(f))
            .map(|f| (f * 100.0).round() as u32)
            .unwrap_or(200)
    })
}

/// How a block's image reads as integers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// `n x width` LE words (verbatim byval).
    Words(u8),
    /// BYTE_FOR frames: `ref i64 | 1024 x width` deltas, the granule's last frame shorter.
    Frames(u8),
    /// DELTA_FOR frames: `first i64 | width u8 | pad 7 | 1024 x width` zigzag deltas.
    DeltaFrames,
    /// DICT_CODES block: `base u32 | width u8 | pad 3 | codes LSB-first`.
    Codes,
}

pub fn view_for(encoding: u16, width: u8) -> Option<View> {
    match EncodingId::resolve(encoding).ok()? {
        EncodingId::Verbatim if matches!(width, 1 | 2 | 4 | 8) => Some(View::Words(width)),
        EncodingId::ByteFor if (1..=8).contains(&width) => Some(View::Frames(width)),
        EncodingId::DeltaFor => Some(View::DeltaFrames),
        EncodingId::DictCodes => Some(View::Codes),
        _ => None,
    }
}

/// Whether the seal offers [`Wrapper::IntPre`] on a value section of `encoding` / `width`: every view but DICT_CODES,
/// whose repack unwraps at 0.4-0.9x zstd's rate for a 4 % cut of the codes' bytes.
pub fn eligible(encoding: u16, width: u8) -> bool {
    !matches!(view_for(encoding, width), None | Some(View::Codes))
}

fn corrupt(at: &'static str) -> FormatError {
    FormatError::Corrupt { at }
}

struct Rd<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> FormatResult<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&e| e <= self.b.len())
            .ok_or(FormatError::Truncated { at: "intpre image" })?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }
    fn u8(&mut self) -> FormatResult<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> FormatResult<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("len 4")))
    }
    fn u64(&mut self) -> FormatResult<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("len 8")))
    }
}

#[inline]
fn nbytes(v: u64) -> usize {
    (64 - v.leading_zeros() as usize).div_ceil(8)
}

#[inline]
fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
fn unzigzag(z: u64) -> i64 {
    ((z >> 1) as i64) ^ -((z & 1) as i64)
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    if a == 0 {
        return b;
    }
    if b == 0 {
        return a;
    }
    let s = (a | b).trailing_zeros();
    a >>= a.trailing_zeros();
    loop {
        b >>= b.trailing_zeros();
        if a > b {
            core::mem::swap(&mut a, &mut b);
        }
        b -= a;
        if b == 0 {
            return a << s;
        }
    }
}

#[inline(always)]
fn put_n<const B: usize>(vals: &[u64], base: u64, dst: &mut [u8]) {
    for (d, v) in dst.chunks_exact_mut(B).zip(vals) {
        d.copy_from_slice(&v.wrapping_sub(base).to_le_bytes()[..B]);
    }
}

#[inline(always)]
fn get_n<const B: usize>(src: &[u8], base: u64, out: &mut [u64]) {
    for (o, s) in out.iter_mut().zip(src.chunks_exact(B)) {
        let mut w = [0u8; 8];
        w[..B].copy_from_slice(s);
        *o = base.wrapping_add(u64::from_le_bytes(w));
    }
}

/// `vals[i] - base` as `b`-byte LE words into `dst` (`dst.len() == vals.len() * b`).
fn put_words(vals: &[u64], base: u64, b: usize, dst: &mut [u8]) {
    match b {
        1 => put_n::<1>(vals, base, dst),
        2 => put_n::<2>(vals, base, dst),
        3 => put_n::<3>(vals, base, dst),
        4 => put_n::<4>(vals, base, dst),
        5 => put_n::<5>(vals, base, dst),
        6 => put_n::<6>(vals, base, dst),
        7 => put_n::<7>(vals, base, dst),
        8 => put_n::<8>(vals, base, dst),
        _ => {}
    }
}

/// `base + word` for each `b`-byte LE word of `src` (`b == 0`: every value is `base`).
fn get_words(src: &[u8], base: u64, b: usize, out: &mut [u64]) {
    match b {
        1 => get_n::<1>(src, base, out),
        2 => get_n::<2>(src, base, out),
        3 => get_n::<3>(src, base, out),
        4 => get_n::<4>(src, base, out),
        5 => get_n::<5>(src, base, out),
        6 => get_n::<6>(src, base, out),
        7 => get_n::<7>(src, base, out),
        8 => get_n::<8>(src, base, out),
        _ => out.fill(base),
    }
}

fn append_words(vals: &[u64], base: u64, b: usize, out: &mut Vec<u8>) -> FormatResult<()> {
    let start = out.len();
    pgrc_format::try_resize(out, start + vals.len() * b, 0, "intpre words")?;
    put_words(vals, base, b, &mut out[start..]);
    Ok(())
}

fn push_u32(out: &mut Vec<u8>, v: usize) -> FormatResult<()> {
    let v = u32::try_from(v).map_err(|_| FormatError::EncodeContract { detail: "intpre count past u32" })?;
    out.extend_from_slice(&v.to_le_bytes());
    Ok(())
}

/// The byte width `E(k)` takes for values with byte-width histogram `hist`, and its estimated bytes (exceptions priced
/// at one gap byte width plus their high bytes each).
fn e_choose(hist: &[usize; 9], k: usize, depth: u32) -> (usize, usize) {
    let maxb = (0..9).rev().find(|&i| hist[i] > 0).unwrap_or(0);
    let (mut b, mut best) = (maxb, k * maxb);
    if depth < MAX_DEPTH {
        let mut above = 0usize;
        for cand in (0..maxb).rev() {
            above += hist[cand + 1];
            if above * 2 > k {
                break;
            }
            let gap = nbytes((k / above) as u64).max(1);
            let cost = k * cand + 26 + above * (gap + maxb - cand);
            if cost < best {
                best = cost;
                b = cand;
            }
        }
    }
    (b, 13 + best)
}

fn e_put(vals: &[u64], depth: u32, out: &mut Vec<u8>) -> FormatResult<()> {
    let k = vals.len();
    let base = vals.iter().copied().min().unwrap_or(0);
    let mut hist = [0usize; 9];
    for &v in vals {
        hist[nbytes(v - base)] += 1;
    }
    let b = e_choose(&hist, k, depth).0;
    let x = hist[b + 1..].iter().sum::<usize>();
    pgrc_format::try_reserve_more(out, 13 + k * b, "intpre patched base")?;
    out.extend_from_slice(&base.to_le_bytes());
    out.push(b as u8);
    push_u32(out, x)?;
    append_words(vals, base, b, out)?;
    if x > 0 {
        let mut gaps = try_vec::<u64>(x, "intpre gaps")?;
        let mut highs = try_vec::<u64>(x, "intpre highs")?;
        let mut prev = 0usize;
        for (i, &v) in vals.iter().enumerate() {
            let d = v - base;
            if nbytes(d) > b {
                gaps.push((if gaps.is_empty() { i } else { i - prev }) as u64);
                highs.push(d >> (8 * b));
                prev = i;
            }
        }
        e_put(&gaps, depth + 1, out)?;
        e_put(&highs, depth + 1, out)?;
    }
    Ok(())
}

fn e_get(rd: &mut Rd<'_>, out: &mut [u64], depth: u32) -> FormatResult<()> {
    if depth > MAX_DEPTH {
        return Err(corrupt("intpre nesting"));
    }
    let k = out.len();
    let base = rd.u64()?;
    let b = rd.u8()? as usize;
    let x = rd.u32()? as usize;
    if b > 8 || x > k || (x > 0 && b == 8) {
        return Err(corrupt("intpre patched base header"));
    }
    get_words(rd.take(k * b)?, base, b, out);
    if x > 0 {
        let mut gaps = try_filled(x, 0u64, "intpre gaps")?;
        e_get(rd, &mut gaps, depth + 1)?;
        let mut highs = try_filled(x, 0u64, "intpre highs")?;
        e_get(rd, &mut highs, depth + 1)?;
        let sh = 8 * b as u32;
        let mut p = 0usize;
        for (i, (&g, &h)) in gaps.iter().zip(&highs).enumerate() {
            let g = usize::try_from(g).map_err(|_| corrupt("intpre exception gap"))?;
            p = if i == 0 { g } else { p.checked_add(g).ok_or(corrupt("intpre exception gap"))? };
            let slot = out.get_mut(p).ok_or(corrupt("intpre exception position"))?;
            *slot = slot.wrapping_add(h.wrapping_shl(sh));
        }
    }
    Ok(())
}

/// The GCD of the absolute consecutive deltas (1 when none or all zero).
fn delta_scale(vals: &[u64]) -> u64 {
    let mut g = 0u64;
    for w in vals.windows(2) {
        let a = (w[1].wrapping_sub(w[0]) as i64).unsigned_abs();
        if g == 0 || a % g != 0 {
            g = gcd(g, a);
            if g == 1 {
                break;
            }
        }
    }
    g.max(1)
}

fn delta_put(vals: &[u64], out: &mut Vec<u8>) -> FormatResult<bool> {
    if vals.len() < 2 {
        return Ok(false);
    }
    let scale = delta_scale(vals);
    let mut zz = try_vec::<u64>(vals.len() - 1, "intpre deltas")?;
    zz.extend(vals.windows(2).map(|w| zigzag((w[1].wrapping_sub(w[0]) as i64) / scale as i64)));
    out.push(DELTA);
    out.extend_from_slice(&vals[0].to_le_bytes());
    out.extend_from_slice(&scale.to_le_bytes());
    e_put(&zz, 0, out)?;
    Ok(true)
}

fn rle_put(vals: &[u64], out: &mut Vec<u8>) -> FormatResult<bool> {
    let n = vals.len();
    let runs = 1 + vals.windows(2).filter(|w| w[0] != w[1]).count();
    if n == 0 || runs * 2 > n {
        return Ok(false);
    }
    let mut lens = try_vec::<u64>(runs, "intpre run lengths")?;
    let mut vs = try_vec::<u64>(runs, "intpre run values")?;
    let mut i = 0;
    while i < n {
        let mut j = i + 1;
        while j < n && vals[j] == vals[i] {
            j += 1;
        }
        lens.push((j - i - 1) as u64);
        vs.push(vals[i]);
        i = j;
    }
    out.push(RLE);
    push_u32(out, runs)?;
    e_put(&lens, 0, out)?;
    e_put(&vs, 0, out)?;
    Ok(true)
}

fn linear_put(vals: &[u64], out: &mut Vec<u8>) -> FormatResult<bool> {
    let n = vals.len();
    if n < 3 {
        return Ok(false);
    }
    let (mut lens, mut starts, mut strides) = (Vec::new(), Vec::new(), Vec::new());
    let mut i = 0;
    while i < n {
        let stride = if i + 1 < n { vals[i + 1].wrapping_sub(vals[i]) } else { 0 };
        let mut j = (i + 2).min(n);
        while j < n && vals[j].wrapping_sub(vals[j - 1]) == stride {
            j += 1;
        }
        if (lens.len() + 1) * 3 > n {
            return Ok(false);
        }
        pgrc_format::try_reserve_more(&mut lens, 1, "intpre linear runs")?;
        pgrc_format::try_reserve_more(&mut starts, 1, "intpre linear runs")?;
        pgrc_format::try_reserve_more(&mut strides, 1, "intpre linear runs")?;
        lens.push((j - i - 1) as u64);
        starts.push(vals[i]);
        strides.push(zigzag(stride as i64));
        i = j;
    }
    out.push(LINEAR);
    push_u32(out, lens.len())?;
    e_put(&lens, 0, out)?;
    e_put(&starts, 0, out)?;
    e_put(&strides, 0, out)?;
    Ok(true)
}

/// Estimated bytes of patched base, delta, RLE and linear runs over `vals` in two passes (None = not applicable).
fn estimates(vals: &[u64]) -> [Option<usize>; 4] {
    let n = vals.len();
    if n == 0 {
        return [None; 4];
    }
    let (mut lo, mut hi, mut runs, mut breaks) = (u64::MAX, 0u64, 1usize, 0usize);
    let (mut prev, mut pd) = (vals[0], 0u64);
    for (i, &v) in vals.iter().enumerate() {
        lo = lo.min(v);
        hi = hi.max(v);
        let d = v.wrapping_sub(prev);
        runs += (v != prev) as usize;
        breaks += (i > 1 && d != pd) as usize;
        prev = v;
        pd = d;
    }
    let scale = delta_scale(vals);
    let (mut hp, mut hd) = ([0usize; 9], [0usize; 9]);
    let mut prev = vals[0];
    for &v in vals {
        hp[nbytes(v - lo)] += 1;
        let d = v.wrapping_sub(prev) as i64;
        hd[nbytes(zigzag(if scale == 1 { d } else { d / scale as i64 }))] += 1;
        prev = v;
    }
    hd[0] -= 1;
    let vb = nbytes(hi - lo).max(1);
    [
        Some(1 + e_choose(&hp, n, 0).1),
        (n >= 2).then(|| 17 + e_choose(&hd, n - 1, 0).1),
        (runs * 2 <= n).then(|| 31 + runs * (1 + vb)),
        (n >= 3 && (breaks + 1) * 3 <= n).then(|| 44 + (breaks + 1) * (3 + vb)),
    ]
}

/// One sequence: the smallest coding when it reaches `f100 / 100` x smaller than `n x w`, else plain. True when coded.
/// Only the codings estimated within 10 % of the best estimate are built; the gate is on their exact bytes.
fn seq_put(vals: &[u64], w: usize, f100: u32, out: &mut Vec<u8>) -> FormatResult<bool> {
    let raw = vals.len() * w;
    let gate = |len: usize| raw as u64 * 100 >= f100 as u64 * len as u64;
    let est = estimates(vals);
    let Some(top) = est.iter().flatten().copied().min() else {
        out.push(PLAIN);
        append_words(vals, 0, w, out)?;
        return Ok(false);
    };
    let (mut best, mut cand) = (Vec::new(), Vec::new());
    for (scheme, e) in [PFOR, DELTA, RLE, LINEAR].into_iter().zip(est) {
        if !gate(top * 9 / 10) || e.is_none_or(|e| e * 10 > top * 11) {
            continue;
        }
        cand.clear();
        let ok = match scheme {
            PFOR => {
                cand.push(PFOR);
                e_put(vals, 0, &mut cand)?;
                !vals.is_empty()
            }
            DELTA => delta_put(vals, &mut cand)?,
            RLE => rle_put(vals, &mut cand)?,
            _ => linear_put(vals, &mut cand)?,
        };
        if ok && (best.is_empty() || cand.len() < best.len()) {
            core::mem::swap(&mut best, &mut cand);
        }
    }
    if !best.is_empty() && gate(best.len()) {
        pgrc_format::try_reserve_more(out, best.len(), "intpre sequence")?;
        out.extend_from_slice(&best);
        return Ok(true);
    }
    out.push(PLAIN);
    append_words(vals, 0, w, out)?;
    Ok(false)
}

fn seq_get(rd: &mut Rd<'_>, out: &mut [u64], w: usize) -> FormatResult<()> {
    let n = out.len();
    match rd.u8()? {
        PLAIN => get_words(rd.take(n * w)?, 0, w, out),
        PFOR => e_get(rd, out, 0)?,
        DELTA => {
            let (first, rest) = out.split_first_mut().ok_or(corrupt("intpre delta of nothing"))?;
            *first = rd.u64()?;
            let scale = rd.u64()? as i64;
            e_get(rd, rest, 0)?;
            let mut prev = *first;
            for v in rest.iter_mut() {
                prev = prev.wrapping_add(unzigzag(*v).wrapping_mul(scale) as u64);
                *v = prev;
            }
        }
        RLE => {
            let runs = rd.u32()? as usize;
            if runs > n || (runs == 0) != (n == 0) {
                return Err(corrupt("intpre run count"));
            }
            let mut lens = try_filled(runs, 0u64, "intpre run lengths")?;
            e_get(rd, &mut lens, 0)?;
            let mut vs = try_filled(runs, 0u64, "intpre run values")?;
            e_get(rd, &mut vs, 0)?;
            let mut p = 0usize;
            for (&l, &v) in lens.iter().zip(&vs) {
                let end = usize::try_from(l).ok().and_then(|l| p.checked_add(l)?.checked_add(1)).filter(|&e| e <= n);
                let end = end.ok_or(corrupt("intpre run length"))?;
                out[p..end].fill(v);
                p = end;
            }
            if p != n {
                return Err(corrupt("intpre runs short"));
            }
        }
        LINEAR => {
            let runs = rd.u32()? as usize;
            if runs > n || (runs == 0) != (n == 0) {
                return Err(corrupt("intpre run count"));
            }
            let mut lens = try_filled(runs, 0u64, "intpre run lengths")?;
            e_get(rd, &mut lens, 0)?;
            let mut starts = try_filled(runs, 0u64, "intpre run starts")?;
            e_get(rd, &mut starts, 0)?;
            let mut strides = try_filled(runs, 0u64, "intpre run strides")?;
            e_get(rd, &mut strides, 0)?;
            let mut p = 0usize;
            for ((&l, &s), &z) in lens.iter().zip(&starts).zip(&strides) {
                let end = usize::try_from(l).ok().and_then(|l| p.checked_add(l)?.checked_add(1)).filter(|&e| e <= n);
                let end = end.ok_or(corrupt("intpre run length"))?;
                let stride = unzigzag(z) as u64;
                let mut v = s;
                for o in &mut out[p..end] {
                    *o = v;
                    v = v.wrapping_add(stride);
                }
                p = end;
            }
            if p != n {
                return Err(corrupt("intpre runs short"));
            }
        }
        _ => return Err(corrupt("intpre scheme")),
    }
    Ok(())
}

/// BYTE_FOR geometry of an `len`-byte granule image at width `w`: (frames, values).
fn frames_geometry(len: usize, w: usize) -> Option<(usize, usize)> {
    let fs = 8 + FV * w;
    let (full, rem) = (len / fs, len % fs);
    if rem == 0 {
        Some((full, full * FV))
    } else if rem > 8 && (rem - 8) % w == 0 {
        Some((full + 1, full * FV + (rem - 8) / w))
    } else {
        None
    }
}

#[inline]
fn load8(b: &[u8], at: usize) -> u64 {
    let mut w = [0u8; 8];
    let end = (at + 8).min(b.len());
    if at < end {
        w[..end - at].copy_from_slice(&b[at..end]);
    }
    u64::from_le_bytes(w)
}

/// `codes` (masked to `width` bits) LSB-first into `packed`, its bytes past the codes zeroed; the caller checked
/// `codes.len() * width <= packed.len() * 8`. One eight-byte store per 64 bits, not a read-modify-write per code.
fn pack_codes(codes: &[u64], width: usize, packed: &mut [u8]) {
    let mask = (1u64 << width) - 1;
    let (mut acc, mut nbits, mut at) = (0u64, 0usize, 0usize);
    for &c in codes {
        let c = c & mask;
        acc |= c << nbits;
        nbits += width;
        if nbits >= 64 {
            packed[at..at + 8].copy_from_slice(&acc.to_le_bytes());
            at += 8;
            nbits -= 64;
            acc = if nbits == 0 { 0 } else { c >> (width - nbits) };
        }
    }
    let tail = nbits.div_ceil(8);
    packed[at..at + tail].copy_from_slice(&acc.to_le_bytes()[..tail]);
    packed[at + tail..].fill(0);
}

/// The pre image of `raw` under `view` into `pre`; false when the image does not read under the view or no sequence
/// reached the factor.
fn prepass_encode(view: View, raw: &[u8], f100: u32, pre: &mut Vec<u8>) -> FormatResult<bool> {
    let len = raw.len();
    match view {
        View::Words(w) => {
            let wu = w as usize;
            if !(1..=8).contains(&wu) || len == 0 || len % wu != 0 {
                return Ok(false);
            }
            let mut vals = try_filled(len / wu, 0u64, "intpre values")?;
            get_words(raw, 0, wu, &mut vals);
            pre.extend_from_slice(&[0, w]);
            seq_put(&vals, wu, f100, pre)
        }
        View::Frames(w) => {
            let wu = w as usize;
            let Some((nf, n)) = (1..=8).contains(&wu).then(|| frames_geometry(len, wu)).flatten() else {
                return Ok(false);
            };
            if n == 0 {
                return Ok(false);
            }
            let mut refs = try_filled(nf, 0u64, "intpre refs")?;
            let mut vals = try_filled(n, 0u64, "intpre values")?;
            let (mut pos, mut v0) = (0usize, 0usize);
            for r in refs.iter_mut() {
                *r = load8(raw, pos);
                let cnt = (n - v0).min(FV);
                get_words(&raw[pos + 8..pos + 8 + cnt * wu], 0, wu, &mut vals[v0..v0 + cnt]);
                pos += 8 + cnt * wu;
                v0 += cnt;
            }
            pre.extend_from_slice(&[1, w]);
            seq_put(&refs, 8, f100, pre)?;
            seq_put(&vals, wu, f100, pre)
        }
        View::DeltaFrames => {
            let (mut widths, mut firsts, mut vals) = (Vec::new(), Vec::new(), Vec::new());
            pgrc_format::try_reserve_more(&mut vals, len, "intpre values")?;
            let mut pos = 0usize;
            while pos < len {
                if len - pos < 16 || raw[pos + 9..pos + 16] != [0u8; 7] {
                    return Ok(false);
                }
                let wf = raw[pos + 8] as usize;
                if !(1..=8).contains(&wf) {
                    return Ok(false);
                }
                let rest = len - pos - 16;
                let cnt = if rest == FV * wf || rest >= FV * wf + 16 {
                    FV
                } else if rest > 0 && rest < FV * wf && rest % wf == 0 {
                    rest / wf
                } else {
                    return Ok(false);
                };
                pgrc_format::try_reserve_more(&mut widths, 1, "intpre frame widths")?;
                pgrc_format::try_reserve_more(&mut firsts, 1, "intpre frame firsts")?;
                widths.push(wf as u8);
                firsts.push(load8(raw, pos));
                let v0 = vals.len();
                pgrc_format::try_resize(&mut vals, v0 + cnt, 0, "intpre values")?;
                get_words(&raw[pos + 16..pos + 16 + cnt * wf], 0, wf, &mut vals[v0..]);
                pos += 16 + cnt * wf;
            }
            if vals.is_empty() {
                return Ok(false);
            }
            pre.extend_from_slice(&[2, 0]);
            push_u32(pre, widths.len())?;
            push_u32(pre, vals.len())?;
            pre.extend_from_slice(&widths);
            seq_put(&firsts, 8, f100, pre)?;
            seq_put(&vals, 8, f100, pre)
        }
        View::Codes => {
            if len <= 8 || raw[5..8] != [0u8; 3] {
                return Ok(false);
            }
            let width = raw[4] as usize;
            if !(1..=32).contains(&width) {
                return Ok(false);
            }
            let packed = &raw[8..];
            let bits = packed.len() * 8;
            let n = bits / width;
            let mask = (1u64 << width) - 1;
            let mut codes = try_filled(n, 0u64, "intpre codes")?;
            for (i, c) in codes.iter_mut().enumerate() {
                let bit = i * width;
                let at = bit / 8;
                let word = match packed.get(at..at + 8) {
                    Some(s) => u64::from_le_bytes(s.try_into().expect("len 8")),
                    None => load8(packed, at),
                };
                *c = (word >> (bit % 8)) & mask;
            }
            let tail = n * width;
            if tail < bits && (load8(packed, tail / 8) >> (tail % 8)) != 0 {
                return Ok(false);
            }
            pre.extend_from_slice(&[3, 0]);
            pre.extend_from_slice(&raw[..8]);
            push_u32(pre, n)?;
            seq_put(&codes, 4, f100, pre)
        }
    }
}

/// Rebuild the exact image (`dst.len()` bytes) from a pre image. Typed errors on any input, never a panic.
pub fn prepass_decode(pre: &[u8], dst: &mut [u8]) -> FormatResult<()> {
    let mut rd = Rd { b: pre, at: 0 };
    let view = rd.u8()?;
    let w = rd.u8()? as usize;
    let len = dst.len();
    match view {
        0 => {
            if !(1..=8).contains(&w) || len % w != 0 {
                return Err(corrupt("intpre words geometry"));
            }
            let mut vals = try_filled(len / w, 0u64, "intpre values")?;
            seq_get(&mut rd, &mut vals, w)?;
            put_words(&vals, 0, w, dst);
        }
        1 => {
            let (nf, n) = (1..=8)
                .contains(&w)
                .then(|| frames_geometry(len, w))
                .flatten()
                .ok_or(corrupt("intpre frames geometry"))?;
            let mut refs = try_filled(nf, 0u64, "intpre refs")?;
            seq_get(&mut rd, &mut refs, 8)?;
            let mut vals = try_filled(n, 0u64, "intpre values")?;
            seq_get(&mut rd, &mut vals, w)?;
            let (mut pos, mut v0) = (0usize, 0usize);
            for &r in &refs {
                let cnt = (n - v0).min(FV);
                dst[pos..pos + 8].copy_from_slice(&r.to_le_bytes());
                put_words(&vals[v0..v0 + cnt], 0, w, &mut dst[pos + 8..pos + 8 + cnt * w]);
                pos += 8 + cnt * w;
                v0 += cnt;
            }
        }
        2 => {
            let nf = rd.u32()? as usize;
            let n = rd.u32()? as usize;
            if nf == 0 || nf > len / 16 || n > len || n.div_ceil(FV) != nf {
                return Err(corrupt("intpre delta frames geometry"));
            }
            let widths = rd.take(nf)?;
            let mut firsts = try_filled(nf, 0u64, "intpre frame firsts")?;
            seq_get(&mut rd, &mut firsts, 8)?;
            let mut vals = try_filled(n, 0u64, "intpre values")?;
            seq_get(&mut rd, &mut vals, 8)?;
            let (mut pos, mut v0) = (0usize, 0usize);
            for (&wf, &first) in widths.iter().zip(&firsts) {
                let wf = wf as usize;
                let cnt = (n - v0).min(FV);
                let end = pos + 16 + cnt * wf;
                if !(1..=8).contains(&wf) || end > len {
                    return Err(corrupt("intpre delta frame"));
                }
                dst[pos..pos + 8].copy_from_slice(&first.to_le_bytes());
                dst[pos + 8] = wf as u8;
                dst[pos + 9..pos + 16].fill(0);
                put_words(&vals[v0..v0 + cnt], 0, wf, &mut dst[pos + 16..end]);
                pos = end;
                v0 += cnt;
            }
            if pos != len {
                return Err(corrupt("intpre delta frames short"));
            }
        }
        3 => {
            let hdr = rd.take(8)?;
            let width = hdr[4] as usize;
            let n = rd.u32()? as usize;
            if len < 8 || !(1..=32).contains(&width) || n.checked_mul(width).is_none_or(|b| b > (len - 8) * 8) {
                return Err(corrupt("intpre codes geometry"));
            }
            let mut codes = try_filled(n, 0u64, "intpre codes")?;
            seq_get(&mut rd, &mut codes, 4)?;
            dst[..8].copy_from_slice(hdr);
            pack_codes(&codes, width, &mut dst[8..]);
        }
        _ => return Err(corrupt("intpre view")),
    }
    if rd.at != pre.len() {
        return Err(corrupt("intpre image trailing bytes"));
    }
    Ok(())
}

/// One sequence under one forced scheme (`None` when the scheme does not apply), test face.
#[cfg(test)]
pub(crate) fn scheme_image(scheme: u8, vals: &[u64]) -> FormatResult<Option<Vec<u8>>> {
    let mut out = Vec::new();
    let ok = match scheme {
        PLAIN => {
            out.push(PLAIN);
            append_words(vals, 0, 8, &mut out)?;
            true
        }
        PFOR => {
            out.push(PFOR);
            e_put(vals, 0, &mut out)?;
            true
        }
        DELTA => delta_put(vals, &mut out)?,
        RLE => rle_put(vals, &mut out)?,
        _ => linear_put(vals, &mut out)?,
    };
    Ok(ok.then_some(out))
}

/// Decode one sequence image of `n` values at width `w`, refusing trailing bytes (test face).
#[cfg(test)]
pub(crate) fn scheme_decode(img: &[u8], n: usize, w: usize) -> FormatResult<Vec<u64>> {
    let mut rd = Rd { b: img, at: 0 };
    let mut out = try_filled(n, 0u64, "intpre test")?;
    seq_get(&mut rd, &mut out, w)?;
    if rd.at != img.len() {
        return Err(corrupt("intpre image trailing bytes"));
    }
    Ok(out)
}

/// The pre image of one block's `raw` image, or None when the pre-pass does not engage (bench and test face).
#[doc(hidden)]
pub fn prepass_image(view: View, raw: &[u8], f100: u32) -> FormatResult<Option<Vec<u8>>> {
    let mut pre = Vec::new();
    Ok(prepass_encode(view, raw, f100, &mut pre)?.then_some(pre))
}

/// A frame-view pre image at least this many times smaller than a block of at least `SURE_MIN` bytes is kept without
/// pricing the plain zstd block.
const SURE_FACTOR: usize = 4;
const SURE_MIN: usize = 16 << 10;
/// Blocks of a section priced both ways before the section settles its pricing.
const PROBES: u32 = 2;

/// One section's pricing state: after `PROBES` blocks, a section whose pre-pass saved under 3 % stops building pre
/// images, and one that saved a quarter or more stops pricing plain blocks.
#[derive(Default)]
pub(crate) struct Trial {
    probes: u32,
    pre: usize,
    plain: usize,
}

impl Trial {
    fn verdict(&self) -> Option<bool> {
        if self.probes < PROBES {
            None
        } else if self.pre * 100 > self.plain * 97 {
            Some(false)
        } else if self.pre * 4 <= self.plain * 3 {
            Some(true)
        } else {
            None
        }
    }
    fn note(&mut self, chosen: usize, plain: usize) {
        if self.probes < PROBES {
            self.probes += 1;
            self.pre += chosen;
            self.plain += plain;
        }
    }
}

/// One wrapped block of `raw` appended to `out` (at most `1 + zstd_bound(raw.len())` bytes): the pre-pass block when
/// the view engages and it is smaller under zstd than the plain block, else the plain block. True when pre-passed.
/// Both frames are zstd at `level`, the level the plain Zstd arm takes for this section.
pub(crate) fn wrap_block(view: Option<View>, raw: &[u8], out: &mut Vec<u8>, trial: &mut Trial, level: i32) -> FormatResult<bool> {
    let start = out.len();
    let plain = |out: &mut Vec<u8>| -> FormatResult<bool> {
        out.truncate(start);
        out.push(0);
        zstd_compress_block_at(raw, out, level)?;
        Ok(false)
    };
    let verdict = trial.verdict();
    let Some(view) = view.filter(|_| verdict != Some(false)) else { return plain(out) };
    let mut pre = Vec::new();
    let mut rebuilt = prepass_encode(view, raw, factor_x100(), &mut pre)? && pre.len() <= raw.len() + 64;
    if rebuilt {
        let mut check = try_filled(raw.len(), 0u8, "intpre check")?;
        rebuilt = prepass_decode(&pre, &mut check).is_ok() && check == raw;
        debug_assert!(rebuilt, "intpre pre image does not rebuild its block");
    }
    if !rebuilt {
        plain(out)?;
        trial.note(out.len() - start, out.len() - start);
        return Ok(false);
    }
    let mut z = Vec::new();
    zstd_compress_block_at(&pre, &mut z, level)?;
    let frames = matches!(view, View::Frames(_) | View::DeltaFrames);
    if verdict == Some(true) || (frames && raw.len() >= SURE_MIN && pre.len() * SURE_FACTOR <= raw.len()) {
        if 9 + z.len() >= 1 + crate::wrapper::zstd_bound(raw.len()) {
            return plain(out);
        }
        trial.note(9 + z.len(), raw.len());
    } else {
        plain(out)?;
        let p = out.len() - start;
        trial.note(p.min(9 + z.len()), p);
        if 9 + z.len() >= p {
            return Ok(false);
        }
    }
    out.truncate(start);
    out.push(1);
    push_u32(out, raw.len())?;
    push_u32(out, pre.len())?;
    out.extend_from_slice(&z);
    Ok(true)
}

/// Decode one wrapped block into the front of `dst` (the image span left); returns the bytes written.
pub(crate) fn unwrap_block(src: &[u8], dst: &mut [u8]) -> FormatResult<usize> {
    let (&mode, body) = src.split_first().ok_or(FormatError::Truncated { at: "intpre block mode" })?;
    match mode {
        0 => zstd_decompress_block(body, dst),
        1 => {
            let mut rd = Rd { b: body, at: 0 };
            let len = rd.u32()? as usize;
            let pre_len = rd.u32()? as usize;
            if len > dst.len() || pre_len > len + 64 {
                return Err(corrupt("intpre block lengths"));
            }
            let mut pre = try_filled(pre_len, 0u8, "intpre pre image")?;
            if zstd_decompress_block(&body[8..], &mut pre)? != pre_len {
                return Err(corrupt("intpre pre image length"));
            }
            prepass_decode(&pre, &mut dst[..len])?;
            Ok(len)
        }
        _ => Err(corrupt("intpre block mode")),
    }
}

/// The block-offset table of a wrapped `image`: (payload start, payload end, offsets).
fn block_table(image: &[u8]) -> FormatResult<(StreamSectionHdr, usize, Vec<usize>)> {
    let hdr = StreamSectionHdr::decode(image)?;
    let end = match (hdr.frame_table_off, hdr.gcount_table_off) {
        (0, 0) => image.len(),
        (0, g) => g as usize,
        (f, _) => f as usize,
    };
    let p0 = STREAM_SECTION_HDR_LEN;
    if end < p0 + 4 || end > image.len() {
        return Err(FormatError::Bounds { at: "payload region" });
    }
    let first = u32::from_le_bytes(image[p0..p0 + 4].try_into().expect("len 4")) as usize;
    if first < 4 || first % 4 != 0 || p0 + first > end {
        return Err(corrupt("wrapper block table"));
    }
    let offs: Vec<usize> = (0..first / 4)
        .map(|i| u32::from_le_bytes(image[p0 + i * 4..p0 + i * 4 + 4].try_into().expect("len 4")) as usize)
        .collect();
    if offs.windows(2).any(|w| w[0] >= w[1]) || p0 + *offs.last().expect("nonempty") > end {
        return Err(corrupt("wrapper block offsets"));
    }
    Ok((hdr, end, offs))
}

/// Whether every block of the IntPre `image` is a plain (mode 0) block.
fn plain_only(image: &[u8]) -> FormatResult<bool> {
    let (_, _, offs) = block_table(image)?;
    Ok(offs[..offs.len() - 1].iter().all(|&o| image[STREAM_SECTION_HDR_LEN + o] == 0))
}

/// Rewrite a plain-only IntPre `image` in place as the Zstd image of the same section (drops each block's mode byte).
fn to_zstd(image: &mut Vec<u8>) -> FormatResult<()> {
    let (hdr, end, offs) = block_table(image)?;
    let p0 = STREAM_SECTION_HDR_LEN;
    let nb = offs.len() - 1;
    let mut at = offs[0];
    for b in 0..nb {
        let n = offs[b + 1] - offs[b] - 1;
        image.copy_within(p0 + offs[b] + 1..p0 + offs[b + 1], p0 + at);
        image[p0 + (b + 1) * 4..p0 + (b + 1) * 4 + 4].copy_from_slice(&((at + n) as u32).to_le_bytes());
        at += n;
    }
    let tail = image.len() - end;
    image.copy_within(end.., p0 + at);
    image.truncate(p0 + at + tail);
    image[7] = Wrapper::Zstd.as_u8();
    if hdr.frame_table_off != 0 {
        image[12..16].copy_from_slice(&(hdr.frame_table_off - nb as u32).to_le_bytes());
    }
    if hdr.gcount_table_off != 0 {
        image[16..20].copy_from_slice(&(hdr.gcount_table_off - nb as u32).to_le_bytes());
    }
    Ok(())
}

/// A stream's wrapper after its bands were wrapped under `arm`: IntPre bands in which no block pre-passed become the
/// Zstd bands of the same sections (byte-identical to wrapping them under Zstd), so the stream stamps Zstd.
pub fn settle(arm: Wrapper, images: &mut [Vec<u8>]) -> FormatResult<Wrapper> {
    if arm != Wrapper::IntPre {
        return Ok(arm);
    }
    for im in images.iter() {
        if !plain_only(im)? {
            return Ok(arm);
        }
    }
    for im in images.iter_mut() {
        to_zstd(im)?;
    }
    Ok(Wrapper::Zstd)
}

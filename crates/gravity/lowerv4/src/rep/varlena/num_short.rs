//! Road `num.short` (H14, R2-L06): a numeric varlena image decoded in place into its mantissa word at the fold's scale.
//! Budget 160 lines (files.tsv, +25 % gate). Owner M2-REP.
//!
//! Registers `(NUM_SHORT, NumShortImpl)`; its reader is fold/num_word.rs `word` (L0-d), which calls the
//! `class_num::mantissa` host, cold, for every image this declines (`ok = 0`). The form is the lab's (M2-INVENTORY I20:
//! the lab's `dec2` / `short8` readers), not v1's two groups: up to four base-10000 groups read as one masked 8-byte word,
//! met by four independent multiply-adds into `D = value × 10^(12 - 4 weight)`, then `D × 10^(S + 4 weight - 12)` at the
//! fold's scale `S` — up by `10^a` (`a <= 2`: below 10^18) or down by an exact division by `10^k = 2^k 5^k` as a shift
//! and a multiply by `5^-k mod 2^64` (exact: an image carries no digit past its dscale, and the dscale is `<= S`).
//! The per-weight constants are selected, never branched on; the weights served are those a value of dscale `<= S` can
//! have with its mantissa below 10^18 (`weights`). The digits are one unaligned 8-byte word that ENDS at the image's last
//! byte (the groups are its top bytes), so no read passes the image (a datum can end a heap page: pd_special is BLCKSZ),
//! nor leaves the page of its first byte (`end_word`, D3339): a short image can start a mapping (an index window's own
//! copy at a fresh allocation's first byte), so the bytes before it are read only where its page holds them.
//!
//! Exactly the host's answer (`num_mantissa` / `mantissa` with the dscale companion) wherever it fits: a finite
//! short-form image at dscale `S` (with `ds`: `<= S`, its dscale returned) is its mantissa. Declined: a special (the
//! host's sentinel, cold: rare, and 12 ops a row), a TOAST pointer, a compressed image, the long form, more than four
//! groups, a weight out of `weights`, another dscale. Item N3 (the per-page two-loop decode, I21) is measured on bench-M1
//! with the heap source (L4), not built here.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, Type, Value};

use crate::face::fold::{NumShortImpl, NumShortMech};
use crate::frame::env::Env;
use crate::ops::Table;

crate::strategies! {
    /// In-place numeric image decode (<= 3 digit groups).
    NUM_SHORT = 27, "num.short", NumHeap;
}

pub(crate) const ROWS: Table = Table { strategies: &[(NUM_SHORT, &NumShortImpl(&SHORT))], ..crate::ops::NONE };

struct Short;
static SHORT: Short = Short;

/// The weights served at scale `s`: from the least a value of dscale `<= s` has (`-ceil(s / 4)`) to the most whose
/// mantissa stays below 10^18 (`s + 4 w - 12 <= 2`): two to four weights past `wlo` at every scale `>= 0`.
pub(crate) fn weights(s: i16) -> (i64, i64) {
    let s = s as i64;
    (-((s + 3) / 4), (14 - s).div_euclid(4))
}

/// Weight `w`'s `(k, c)` at scale `s`: `m = (D >> k) × c` (`D` a multiple of `10^k`).
pub(crate) fn consts(s: i16, w: i64) -> (i64, u64) {
    let a = s as i64 + 4 * w - 12;
    if a >= 0 {
        return (0, 10u64.pow(a as u32));
    }
    let p5 = 5u64.pow(-a as u32);
    let mut inv = p5;
    for _ in 0..6 {
        inv = inv.wrapping_mul(2u64.wrapping_sub(p5.wrapping_mul(inv)));
    }
    (-a, inv)
}

/// D3339: the 8 bytes ending at `ptr + len` (an I64 address and the image's length) as one little-endian word, the
/// image's last bytes on top, read inside the image or its first byte's 4 KiB page only: the 8 bytes before the end
/// where the image is 8 bytes or more, or its page holds the `8 - len` bytes before it; else the 8 bytes from its first
/// byte (on that page: it is among the page's first 7), shifted up into place, zeros below. rt_app-0136's SIGSEGV was
/// the end-anchored read 1 byte below a 7-byte image at a 64 KiB-aligned index window's first byte, the page below unmapped.
pub(crate) fn end_word(b: &mut FunctionBuilder<'_>, ptr: Value, len: Value) -> Value {
    let c = |b: &mut FunctionBuilder<'_>, v: i64| b.iconst(Type::I64, v);
    let (m8, eight, page, three, zero) = (c(b, -8), c(b, 8), c(b, 4095), c(b, 3), c(b, 0));
    let e = b.int_bin(IntBinOp::Add, ptr, len);
    let back = b.int_bin(IntBinOp::Add, e, m8);
    let under = b.int_bin(IntBinOp::Sub, eight, len);
    let off = b.int_bin(IntBinOp::And, ptr, page);
    let room = b.icmp(IntCond::Sge, off, under);
    let start = b.select(room, back, ptr);
    let wp = b.conv(ConvOp::IntToPtr, Type::Ptr, start);
    let raw = b.load_unaligned(Type::I64, wp, 0);
    let up = b.int_bin(IntBinOp::Shl, under, three);
    let sh = b.select(room, zero, up);
    b.int_bin(IntBinOp::Shl, raw, sh)
}

impl NumShortMech for Short {
    fn decode(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, ptr: Value, scale: i16, ds: bool) -> (Value, Value, Option<Value>) {
        let c = |b: &mut FunctionBuilder<'_>, v: i64| b.iconst(Type::I64, v);
        let bin = |b: &mut FunctionBuilder<'_>, op, x, y| b.int_bin(op, x, y);
        let (z, one) = (env.zero, env.one);
        let at = |b: &mut FunctionBuilder<'_>, w: Value| b.conv(ConvOp::IntToPtr, Type::Ptr, w);
        // the varlena header `(len, off, ok)`: a 1-byte one (not 0x01, a TOAST pointer), the likely edge, or (cold) a
        // 4-byte uncompressed one, read only there (a 3-byte image must not be read past)
        let p = at(b, ptr);
        let b0 = b.load_zext(Type::I8, p, 0);
        let lo1 = bin(b, IntBinOp::And, b0, one);
        let is1 = b.icmp(IntCond::Ne, lo1, z);
        let (l1, l4, lh) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64, Type::I64, Type::Bool]));
        b.mark_cold(l4);
        let cur = b.current();
        b.cond_br(is1, (l1, &[]), (l4, &[]));
        b.set_likely(cur, true);
        b.switch_to(l1);
        let len1 = bin(b, IntBinOp::Ushr, b0, one);
        let ok1 = b.icmp(IntCond::Ne, b0, one);
        b.br(lh, &[len1, one, ok1]);
        b.switch_to(l4);
        let w4 = b.load_zext_unaligned(Type::I32, p, 0);
        let (two, three, four) = (c(b, 2), c(b, 3), c(b, 4));
        let len4 = bin(b, IntBinOp::Ushr, w4, two);
        let low4 = bin(b, IntBinOp::And, w4, three);
        let ok4 = b.icmp(IntCond::Eq, low4, z);
        b.br(lh, &[len4, four, ok4]);
        b.switch_to(lh);
        let (len, off, hdr_ok) = { let v = b.block_params(lh); (v[0], v[1], v[2]) };
        let two = c(b, 2);
        // payload: the 2-byte numeric header and 0..=4 groups (an even count of bytes, 2..=10): `rest` rotated right by
        // one is at most 4 exactly when it is even and at most 8
        let rest = bin(b, IntBinOp::Sub, len, off);
        let rest = bin(b, IntBinOp::Sub, rest, two);
        let r1 = bin(b, IntBinOp::Ushr, rest, one);
        let c63 = c(b, 63);
        let r2 = bin(b, IntBinOp::Shl, rest, c63);
        let rot = bin(b, IntBinOp::Or, r1, r2);
        let c4 = c(b, 4);
        let fits = b.icmp(IntCond::Ule, rot, c4);
        let ok = bin(b, IntBinOp::And, hdr_ok, fits);
        let q = bin(b, IntBinOp::Add, ptr, off);
        let hp = at(b, q);
        let h = b.load_zext_unaligned(Type::I16, hp, 0);
        // the digits: the 8-byte word ending at the image's end (`end_word`), shifted down by `64 - 8 rest` (two shifts
        // of `32 - 4 rest`: no groups is 0); the bytes above them are gone
        let word = end_word(b, ptr, len);
        let r4 = bin(b, IntBinOp::Shl, rest, two);
        let c32 = c(b, 32);
        let t = bin(b, IntBinOp::Sub, c32, r4);
        let v = bin(b, IntBinOp::Ushr, word, t);
        let v = bin(b, IntBinOp::Ushr, v, t);
        // D = d0 10^12 + d1 10^8 + d2 10^4 + d3: four independent multiply-adds
        let g = c(b, 0xFFFF);
        let mut d = z;
        for (i, mul) in [1_000_000_000_000i64, 100_000_000, 10_000, 1].into_iter().enumerate() {
            let x = if i == 0 { v } else { let s = c(b, 16 * i as i64); bin(b, IntBinOp::Ushr, v, s) };
            let x = if i == 3 { x } else { bin(b, IntBinOp::And, x, g) };
            let mv = c(b, mul);
            d = if i == 3 { bin(b, IntBinOp::Add, d, x) } else { let x = bin(b, IntBinOp::Mul, x, mv); bin(b, IntBinOp::Add, d, x) };
        }
        // the weight (7-bit two's complement) picks its `(k, c)` (below); the zero (no groups) is 0 at any weight
        let c57 = c(b, 57);
        let w = bin(b, IntBinOp::Shl, h, c57);
        let w = bin(b, IntBinOp::Sshr, w, c57);
        let (wlo, whi) = weights(scale);
        let none = b.icmp(IntCond::Eq, rest, z);
        let (k, cm, in_w) = if scale >= 0 && whi >= wlo {
            let (wl, span) = (c(b, wlo), c(b, whi - wlo));
            let idx = bin(b, IntBinOp::Sub, w, wl);
            let in_w = b.icmp(IntCond::Ule, idx, span);
            let in_w = bin(b, IntBinOp::Or, in_w, none);
            // the served weight's `(k, c)` by a switch on `w - wlo`, the pair on its edge (PERF-SERIAL, bench-M1: the
            // select chain over every weight's constants was 45 of the decode's 133 machine instructions a row); past the
            // served weights `(0, 0)`, which `in_w` refuses (the zero, no groups, is 0 at any weight)
            let lk = b.create_block(&[Type::I64, Type::I64]);
            let cases = (0..=whi - wlo).map(|i| {
                let (kk, cc) = consts(scale, wlo + i);
                (i as u64, lk, [c(b, kk), c(b, cc as i64)])
            });
            let cases: Vec<(u64, Block, [Value; 2])> = cases.collect(); // alloc: Program
            b.switch(idx, cases, (lk, &[z, z]));
            b.switch_to(lk);
            let v = b.block_params(lk);
            (v[0], v[1], in_w)
        } else {
            (z, z, none)
        };
        // exact by the dscale rule: an image carries no digit past its dscale (numeric.c make_result rounds to it,
        // numeric_recv truncates to it), so at dscale <= S, D is a multiple of 10^k
        let q = bin(b, IntBinOp::Ushr, d, k);
        let m = bin(b, IntBinOp::Mul, q, cm);
        let neg = c(b, 0x2000);
        let neg = bin(b, IntBinOp::And, h, neg);
        let neg = b.icmp(IntCond::Ne, neg, z);
        let mneg = bin(b, IntBinOp::Sub, z, m);
        let m = b.select(neg, mneg, m);
        // the short form and the dscale in one test of the header word (`== S`), or the form then `<= S` (the companion);
        // a negative scale serves nothing (an image's dscale is past it: the host declines), nor `== S` past 63
        let (fin, dsc) = if ds && scale >= 0 {
            let (c7, c3f, form, short) = (c(b, 7), c(b, 0x3F), c(b, 0xC000), c(b, 0x8000));
            let dsc = bin(b, IntBinOp::Ushr, h, c7);
            let dsc = bin(b, IntBinOp::And, dsc, c3f);
            let form = bin(b, IntBinOp::And, h, form);
            let short = b.icmp(IntCond::Eq, form, short);
            let sc = c(b, scale as i64);
            let at_s = b.icmp(IntCond::Ule, dsc, sc);
            (bin(b, IntBinOp::And, short, at_s), dsc)
        } else if !ds && (0..64).contains(&scale) {
            let (mask, want) = (c(b, 0xDF80), c(b, 0x8000 | ((scale as i64) << 7)));
            let hm = bin(b, IntBinOp::And, h, mask);
            (b.icmp(IntCond::Eq, hm, want), z)
        } else {
            (b.bconst(false), z)
        };
        let fin = bin(b, IntBinOp::And, fin, in_w);
        let ok = bin(b, IntBinOp::And, ok, fin);
        (m, ok, ds.then_some(dsc))
    }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use gravity_fn::agg_final::{num_mantissa, MANT_NAN, MANT_NINF, MANT_PINF, NO_MANTISSA};
    use gravity_ir::Type;

    use super::*;
    use crate::frame::stop::tests::run1;

    /// PG's canonical short-form image of `v / 10^d` (numeric.c `make_result`): base-10000 groups aligned to the point,
    /// leading and trailing zero groups stripped, the zero with no groups at weight 0. `(sign, weight, dscale, groups)`.
    fn parts(v: i128, d: u32) -> (bool, i32, u32, Vec<u16>) {
        let (neg, mut x) = (v < 0, v.unsigned_abs());
        let d4 = d.div_ceil(4) * 4;
        x *= 10u128.pow(d4 - d);
        let mut g = Vec::new(); // alloc: Program
        while x > 0 {
            g.push((x % 10_000) as u16); // alloc: Program
            x /= 10_000;
        }
        let mut low = -(d4 as i32 / 4);
        while g.first() == Some(&0) {
            g.remove(0);
            low += 1;
        }
        g.reverse();
        if g.is_empty() {
            return (false, 0, d, g);
        }
        (neg, low + g.len() as i32 - 1, d, g)
    }

    /// The payload (numeric header + groups): the short form, or the long form (`long`).
    fn payload(neg: bool, w: i32, d: u32, g: &[u16], long: bool) -> Vec<u8> {
        let mut p = Vec::new(); // alloc: Program
        if long {
            p.extend_from_slice(&((if neg { 0x4000 } else { 0 }) | d as u16).to_le_bytes());
            p.extend_from_slice(&(w as i16).to_le_bytes());
        } else {
            let h = 0x8000 | if neg { 0x2000 } else { 0 } | ((d as u16) << 7) | (w as u16 & 0x7F);
            p.extend_from_slice(&h.to_le_bytes());
        }
        g.iter().for_each(|x| p.extend_from_slice(&x.to_le_bytes()));
        p
    }

    /// The varlena: a 1-byte header, or a 4-byte one (`compressed` sets its flag bit).
    fn varlena(p: &[u8], four: bool, compressed: bool) -> Vec<u8> {
        let mut v = Vec::new(); // alloc: Program
        if four {
            v.extend_from_slice(&((((p.len() + 4) as u32) << 2) | if compressed { 2 } else { 0 }).to_le_bytes());
        } else {
            v.push((((p.len() + 1) as u8) << 1) | 1); // alloc: Program
        }
        v.extend_from_slice(p);
        v
    }

    /// The host's answer (fold/num_word.rs `mantissa`): `num_mantissa` at `scale`; with `ds` the image's dscale, a
    /// larger one `NO_MANTISSA`, a smaller one widened.
    fn host(img: &[u8], scale: i16, ds: bool) -> (i64, i64) {
        let p = img.as_ptr();
        // SAFETY: `img` is a live numeric varlena for the closure's life.
        let at = |s: i64| unsafe { num_mantissa(core::ptr::null_mut(), p, s).val };
        if !ds {
            return (at(scale as i64), 0);
        }
        let hdr = if img[0] & 1 != 0 { 1 } else { 4 };
        let h = u16::from_le_bytes([img[hdr], img[hdr + 1]]);
        let d = match h & 0xC000 {
            0xC000 => 0,
            0x8000 => ((h & 0x1F80) >> 7) as i64,
            _ => (h & 0x3FFF) as i64,
        };
        if d > scale as i64 {
            return (NO_MANTISSA, d);
        }
        let m = at(d);
        if matches!(m, MANT_NAN | MANT_PINF | MANT_NINF | NO_MANTISSA) {
            return (m, d);
        }
        let m = 10i64.pow((scale as i64 - d) as u32).checked_mul(m).filter(|v| (MANT_NINF + 1..MANT_PINF).contains(v));
        (m.unwrap_or(NO_MANTISSA), d)
    }

    /// The decode of the image at `p`: `(m, ok, dscale)` through the interpreter (a declined image's word is
    /// `NO_MANTISSA`, which a fitting decode never is).
    fn decode(p: *const u8, scale: i16, ds: bool) -> (i64, bool, i64) {
        let one = |dscale: bool| {
            run1(&[], &mut |b, env| {
                let ptr = b.iconst(Type::I64, p as i64);
                let (m, ok, d) = SHORT.decode(b, env, ptr, scale, ds);
                let none = b.iconst(Type::I64, NO_MANTISSA);
                if dscale { d.unwrap() } else { b.select(ok, m, none) }
            })
        };
        let m = one(false);
        (m, m != NO_MANTISSA, if ds { one(true) } else { 0 })
    }

    /// Checks one image at alignments across 8 bytes (a 4-byte header at its int alignment, as PG places it): a decode that fits is the host's answer; every image the road serves
    /// (the short form, <= 4 groups, a served weight, the dscale rule, a mantissa the host finds) fits. Returns the fits.
    fn check(img: &[u8], scale: i16, ds: bool, served: bool) -> usize {
        let mut fits = 0;
        let ats: &[usize] = if img[0] & 1 == 0 { &[0, 4] } else { &[0, 1, 3, 4, 6, 7] };
        for &at in ats {
            let mut buf = vec![0xA5u8; 40]; // alloc: Program
            let base = (buf.as_ptr() as usize).next_multiple_of(8) - buf.as_ptr() as usize + 8 + at;
            buf[base..base + img.len()].copy_from_slice(img);
            let p = buf[base..].as_ptr();
            let (want, wd) = host(&buf[base..], scale, ds);
            let (m, ok, d) = decode(p, scale, ds);
            if ok {
                assert_eq!((m, if ds { d } else { 0 }), (want, wd), "{img:02x?} at {at}, scale {scale}, ds {ds}");
                fits += 1;
            }
            assert_eq!(ok, served && want != NO_MANTISSA, "{img:02x?} at {at}: served {served}, host {want}, scale {scale}, ds {ds}");
        }
        fits
    }

    /// num.short against the host over every group count 0-5, weights -5..=4, dscale 0-20, signs, both headers, at
    /// scales 0-18 with and without the dscale companion: the same word (and dscale) wherever it fits, and it fits for
    /// every short image of <= 4 groups in the served weights.
    #[test]
    fn num_short_matches_the_host() {
        let mut vals: Vec<(i128, u32)> = Vec::new(); // alloc: Program
        for d in [0u32, 1, 2, 3, 5, 8, 9, 14, 20] {
            for digits in [1u32, 3, 4, 5, 8, 9, 13, 16, 17, 21] {
                for v in [10i128.pow(digits - 1), 10i128.pow(digits) - 1, 10i128.pow(digits) / 7 + 1, 10i128.pow(digits - 1) * 5] {
                    vals.extend([(v, d), (-v, d)]); // alloc: Program
                }
            }
            vals.push((0, d)); // alloc: Program
        }
        let mut served_fits = 0;
        for scale in [0i16, 2, 3, 5, 8, 10, 14, 15, 18] {
            let (wlo, whi) = weights(scale);
            for &(v, d) in vals.iter() {
                let (neg, w, d, g) = parts(v, d);
                if w < -64 || w > 63 || d > 63 {
                    continue;
                }
                for ds in [false, true] {
                    let dsok = if ds { d as i16 <= scale } else { d as i16 == scale };
                    let served = g.len() <= 4 && dsok && (g.is_empty() || (wlo..=whi).contains(&(w as i64)));
                    for four in [false, true] {
                        served_fits += check(&varlena(&payload(neg, w, d, &g, false), four, false), scale, ds, served);
                        check(&varlena(&payload(neg, w, d, &g, true), four, false), scale, ds, false);
                    }
                    check(&varlena(&payload(neg, w, d, &g, false), true, true), scale, ds, false);
                }
            }
        }
        assert!(served_fits > 10_000, "{served_fits}");
    }

    /// NaN / ±Infinity decline under either header (the host's sentinel, cold); a TOAST pointer declines.
    #[test]
    fn specials_and_toast() {
        for (h, m) in [(0xC000u16, MANT_NAN), (0xD000, MANT_PINF), (0xF000, MANT_NINF)] {
            for four in [false, true] {
                for (scale, ds) in [(0, false), (2, true), (9, true)] {
                    let img = varlena(&h.to_le_bytes(), four, false);
                    assert_eq!(check(&img, scale, ds, false), 0, "{h:x}");
                    assert_eq!(host(&img, scale, ds).0, m);
                    assert!(!decode(img.as_ptr(), scale, ds).1);
                }
            }
        }
        let mut toast = vec![1u8, 18]; // alloc: Program
        toast.extend_from_slice(&[0x55; 16]);
        assert!(!decode(toast.as_ptr(), 2, false).1);
    }

    /// G4 (does not apply): an image the host reads but the road leaves alone — the long form, five groups, a compressed
    /// image, a dscale past the scale — declines (`ok = 0`) and the reader's host call decodes it.
    #[test]
    fn leaves_other_images_to_the_host() {
        let (neg, w, d, g) = parts(12_345_678_901_234_567_891, 2);
        assert!(g.len() > 4);
        assert!(!decode(varlena(&payload(neg, w, d, &g, false), false, false).as_ptr(), 2, false).1);
        let (neg, w, d, g) = parts(1234, 2);
        for img in [varlena(&payload(neg, w, d, &g, true), true, false), varlena(&payload(neg, w, d, &g, false), true, true)] {
            assert!(!decode(img.as_ptr(), 2, false).1);
        }
        let img = varlena(&payload(neg, w, 3, &parts(12340, 3).3, false), false, false);
        assert!(!decode(img.as_ptr(), 2, true).1, "dscale 3 > scale 2");
        assert_eq!(decode(varlena(&payload(neg, w, d, &g, false), false, false).as_ptr(), 2, false), (1234, true, 0));
    }

    /// PERF-SERIAL: a negative scale serves no image (an image's dscale is past it: the host declines); a scale past any
    /// short image's dscale (63) serves only the zero, under the dscale companion (`<= S`); at both header forms.
    #[test]
    fn far_scales() {
        for scale in [-8i16, -1, 64, 200] {
            for (v, d) in [(0i128, 0u32), (0, 2), (5, 0), (-1234, 2), (123_456, 3), (-99_999_999, 0)] {
                let (neg, w, d, g) = parts(v, d);
                for four in [false, true] {
                    let img = varlena(&payload(neg, w, d, &g, false), four, false);
                    let mut buf = vec![0xA5u8; 48]; // alloc: Program
                    let base = (buf.as_ptr() as usize).next_multiple_of(8) - buf.as_ptr() as usize + 8;
                    buf[base..base + img.len()].copy_from_slice(&img);
                    for ds in [false, true] {
                        let (m, ok, dsc) = decode(buf[base..].as_ptr(), scale, ds);
                        assert_eq!(ok, scale >= 0 && v == 0 && ds, "{v} dscale {d} scale {scale} ds {ds} four {four}");
                        if ok {
                            assert_eq!((m, dsc), (0, d as i64));
                        }
                    }
                }
            }
        }
    }

    extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
        fn munmap(addr: *mut u8, len: usize) -> i32;
        fn getpagesize() -> i32;
    }

    /// D3339 (rt_app-0136's SIGSEGV 1 byte below a 64 KiB-aligned index window): an image at each of its page's first
    /// 16 bytes and last 16 (a 4-byte header 4-aligned), the pages on both sides PROT_NONE, 0..=4 groups under both
    /// headers: the host's word and dscale wherever it fits, and no read off the image's page (that read is a SIGSEGV).
    #[test]
    fn an_image_at_a_page_edge_reads_only_its_page() {
        // SAFETY: a private anonymous three-page mapping, its first and last pages made inaccessible; unmapped below.
        let (base, page) = unsafe {
            let page = getpagesize() as usize;
            let map_anon = if cfg!(target_os = "macos") { 0x1000 } else { 0x20 };
            let base = mmap(core::ptr::null_mut(), 3 * page, 3, 0x02 | map_anon, -1, 0);
            assert!(!base.is_null() && base as isize != -1, "mmap");
            assert_eq!(mprotect(base, page, 0), 0, "mprotect below");
            assert_eq!(mprotect(base.add(2 * page), page, 0), 0, "mprotect above");
            (base, page)
        };
        let mut fits = 0;
        for (v, d) in [(0i128, 2u32), (7, 2), (-49_069, 2), (104_136, 2), (-123_456_789, 2), (1_234_567_890_123, 2)] {
            let (neg, w, d, g) = parts(v, d);
            for four in [false, true] {
                let img = varlena(&payload(neg, w, d, &g, false), four, false);
                // a 4-byte header at its int alignment, as PG places it (the host reads its digits 2-byte aligned)
                for at in (0..16).chain(page - 16..=page - img.len()).filter(|a| !four || a % 4 == 0) {
                    // SAFETY: `at + img.len() <= page`: the bytes lie in the middle page, which is read-write.
                    let p = unsafe {
                        let p = base.add(page + at);
                        core::ptr::copy_nonoverlapping(img.as_ptr(), p, img.len());
                        p as *const u8
                    };
                    // SAFETY: `p` holds the image for the call.
                    let (want, wd) = host(unsafe { core::slice::from_raw_parts(p, img.len()) }, 2, true);
                    let (m, ok, dsc) = decode(p, 2, true);
                    if ok {
                        assert_eq!((m, dsc), (want, wd), "{img:02x?} at page offset {at}");
                        fits += 1;
                    }
                }
            }
        }
        // SAFETY: the mapping made above, unused past here.
        unsafe { munmap(base, 3 * page) };
        assert!(fits > 150, "{fits}");
    }

    /// The decode's IR size at scale 2 (bench-M1 / TH06's numeric(15, 2)): instructions past the constants (M2-INVENTORY
    /// §0.4: 122 before the end-anchored load, the cold specials and the dscale-rule exactness; 77 after; 84 with
    /// D3339's page-bounded `end_word`, 7 more).
    #[test]
    fn decode_ops() {
        let (neg, w, d, g) = parts(12_345, 2);
        let img = [vec![0u8; 8], varlena(&payload(neg, w, d, &g, false), false, false)].concat();
        let mut n = 0;
        run1(&[], &mut |b, env| {
            let ptr = b.iconst(Type::I64, img[8..].as_ptr() as i64);
            let count = |b: &FunctionBuilder<'_>| b.func().blocks.iter().flat_map(|k| k.insts.iter()).filter(|i| !matches!(i.op, gravity_ir::Op::Const { .. })).count();
            let n0 = count(b);
            let (m, _, _) = SHORT.decode(b, env, ptr, 2, true);
            n = count(b) - n0;
            let _ = m;
            b.iconst(Type::I64, 0)
        });
        eprintln!("num.short decode at scale 2: {n} IR instructions");
        assert!(n <= 87, "{n}");
    }
}

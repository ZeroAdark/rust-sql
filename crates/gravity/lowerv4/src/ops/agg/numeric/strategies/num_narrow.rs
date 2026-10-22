//! Road `num.narrow` : a heap numeric image at a typmod scale of 0..8 decoded in
//! place with no weight switch. Hooks / levers: road:num.narrow. Budget 192 lines (files-extra.tsv, +25 % gate).
//!
//! Its reader is fold/num_share.rs `terms`, num.partial's row terms (a partial's column: a tree's leaf, its typmod's
//! scale, or a bare term's, decompose's decided scale, which a column without a typmod never has), so every finite
//! image it reads carries that dscale S (PG's typmod coercion). Served: a 1-byte varlena header, the short form at
//! dscale S, at most four base-10000 groups. The groups are the top bytes of one unaligned 8-byte word that ends at the
//! image's last byte (num.short's `end_word`: no byte past the image, none off its first byte's page, D3339),
//! masked to their `16 n` bits and read right-aligned, `R = g[n-1] + 10^4 g[n-2] + 10^8 g[n-3] + 10^12 g[n-4]`: the
//! value times `10^(4 f)`, `f = n - 1 - weight` the image's fractional groups, `0 ..= ceil(S / 4)` (an image carries no
//! digit past its dscale). Each f has one form: `m = R 10^(S - 4 f)`, or past the point `m = R / 10^(4 f - S)` (exact: a
//! shift and a multiply by `5^-(4 f - S) mod 2^64`); the forms are selected by f, no switch over the weights (num.short's).
//! The zero has no groups. Declined to today's decode (fold/num_word.rs `word`: num.short, then the host), cold: a 4-byte
//! or TOAST header, the long form, a special, another dscale, five or more groups, a stripped trailing integer group
//! (`f < 0`, e.g. 10000.00), `R 10^(S - 4 f)` from 10^18. Its word is the host's at every value.
//!
//! Each decline and the limit it comes from (W9 09-29, the overfit check; NEW-n: the overfit catalog):
//! - no typmod: PG keeps each value's own dscale, so there is no one scale for the word; never a partial term (decompose's
//!   fixed-scale register, DC-06). A packed lane: its word is already the mantissa (today's `word` reads it).
//! - a 4-byte header: an image of 127+ bytes or an unpacked datum; TOAST: out of line; the long form: dscale past 63 or a
//!   weight past the short form's 7 bits; a special: no mantissa (a sentinel). PG's representation, not this decode's.
//! - scale past 8 (`SCALES`), measured (A-24, TH-4's widen): the decode serves every scale to 18 (the unit test's host
//!   match), but past two fractional groups its select chain costs more than num.short's switch. Box gencoll (c8g.4xlarge),
//!   4M heap rows by an 8-value text key, sum / avg, road on vs num.short, W0 ms over two takes: S = 6 237-245 vs 236-238,
//!   S = 8 241-250 vs 241-246 (within 3 % and 10 ms; W8 flat or 0.2-0.7 ms faster), S = 10 253-257 vs 248-249, S = 12
//!   260-265 vs 254-256, four columns at 6..12 371-375 vs 363-364. So S = 9..18 keep num.short; witness
//!   e2e/gravity/v5/numeric_th4_short_scales.sql (S = 5..18).
//! - five or more groups: one 8-byte load holds four 2-byte groups (num.short's form, REP-05), not an i64 bound (NEW-3):
//!   numeric(15, 2) from 10^12 has five groups, 15 digits, and fits the word.
//! - a stripped trailing integer group: make_result drops trailing zero groups, so 10000.00 is one group at weight 1 and
//!   needs a further 10^(4 (-f)); the forms hold f >= 0 only. A design bound, value-dependent (NEW-2).
//! - `R 10^(S - 4 f)` from 10^18 (S - 4 f >= 3): i64 holds every 18-digit decimal (10^18 < 2^63), not every 19-digit one.
//!   An i64 limit (the host takes words up to 2^63 - 2).
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_program::Rep;

use crate::face::strategy::NUM_MEMO;
use crate::frame::env::Env;
use crate::rep::CellRef;

crate::strategies! {
    /// Num.partial's terms read a short heap numeric image at a typmod scale of 0..8 with at most four groups by a
    /// select over its fractional groups, no weight switch.
    NUM_NARROW = 125, "num.narrow", NumHeap;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(NUM_NARROW, &Road)], ..crate::ops::NONE };

/// Road num.narrow's registration value (the decode is `leaf`'s).
struct Road;

/// Cell `c`'s mantissa word at `scale` and its bad bit (NULL or a special): with `narrow` (road num.narrow) `leaf`'s,
/// else today's.
pub(crate) fn term(b: &mut FunctionBuilder<'_>, env: &mut Env, c: CellRef<'_>, scale: i16, narrow: bool) -> (Value, Value) {
    match narrow.then(|| leaf(b, env, c, scale)).flatten() {
        Some(r) => r,
        None => today(b, env, c, scale),
    }
}

/// Today's decode (fold/num_word.rs `word`: num.short, then the host) and its bad bit.
fn today(b: &mut FunctionBuilder<'_>, env: &mut Env, c: CellRef<'_>, scale: i16) -> (Value, Value) {
    let (m, null, _) = crate::ops::agg::numeric::num_word::word(b, env, c, scale, false);
    let yes = b.bconst(true);
    let sp = crate::ops::agg::numeric::num_word::sentinel(b, env, m, yes).1;
    (m, b.int_bin(IntBinOp::Or, null, sp))
}

/// The scales the narrow decode serves: past two fractional groups num.short's switch measured faster (module doc).
const SCALES: i16 = 8;

/// The narrow decode, today's past it (cold); the row memo gets the word, as `word` puts it. `None`: a packed lane, a
/// scale past `SCALES`.
fn leaf(b: &mut FunctionBuilder<'_>, env: &mut Env, c: CellRef<'_>, scale: i16) -> Option<(Value, Value)> {
    if matches!(c.cell.rep, Rep::PackedNum { .. }) || !(0..=SCALES).contains(&scale) {
        return None;
    }
    let mut cx = env.lx(b, env.node);
    let (p, none) = crate::rep::datum(&mut cx, c, &crate::step::expr::tf_datum(1700));
    env.memo = cx.memo;
    let (key, memo, yes) = ((p, none, scale, false), env.applies(NUM_MEMO), b.bconst(true));
    if let Some((m, _)) = env.memo.num(key).filter(|_| memo) {
        let sp = crate::ops::agg::numeric::num_word::sentinel(b, env, m, yes).1;
        return Some((m, b.int_bin(IntBinOp::Or, none, sp)));
    }
    let l_out = b.create_block(&[Type::I64, Type::Bool]);
    if b.const_of(none) != Some((Type::Bool, 0)) {
        let l_dec = b.create_block(&[]);
        b.cond_br(none, (l_out, &[env.zero, yes]), (l_dec, &[]));
        b.switch_to(l_dec);
    }
    let (m, ok) = decode(b, env, p, scale);
    let (l_old, no, cur) = (b.create_block(&[]), b.bconst(false), b.current());
    b.mark_cold(l_old);
    b.cond_br(ok, (l_out, &[m, no]), (l_old, &[]));
    b.set_likely(cur, true);
    b.switch_to(l_old);
    let outer = env.memo.set_aside();
    let (m, bad) = today(b, env, c, scale);
    env.memo.restore(outer);
    b.br(l_out, &[m, bad]);
    b.switch_to(l_out);
    let v = b.block_params(l_out);
    if memo {
        env.memo.put_num(key, (v[0], env.zero));
    }
    Some((v[0], v[1]))
}

/// The image at `ptr` (an I64 address) at scale `s` (0..=18): `(m, ok)`, `m` its mantissa word where `ok`.
fn decode(b: &mut FunctionBuilder<'_>, env: &Env, ptr: Value, s: i16) -> (Value, Value) {
    let c = |b: &mut FunctionBuilder<'_>, v: i64| b.iconst(Type::I64, v);
    let bin = |b: &mut FunctionBuilder<'_>, op, x, y| b.int_bin(op, x, y);
    let at = |b: &mut FunctionBuilder<'_>, w: Value| b.conv(ConvOp::IntToPtr, Type::Ptr, w);
    let (z, one) = (env.zero, env.one);
    // a 1-byte header (not 0x01, a TOAST pointer); any other reads as a 3-byte image (nothing past a 4-byte header)
    let p = at(b, ptr);
    let b0 = b.load_zext(Type::I8, p, 0);
    let lo = bin(b, IntBinOp::And, b0, one);
    let (is1, toast) = (b.icmp(IntCond::Ne, lo, z), b.icmp(IntCond::Eq, b0, one));
    let hdr = bin(b, IntBinOp::Xor, is1, toast);
    let (len1, three) = (bin(b, IntBinOp::Ushr, b0, one), c(b, 3));
    let len = b.select(hdr, len1, three);
    // the groups' bytes `rest`: 0, 2, 4, 6 or 8 exactly when `rest` rotated right by one is at most 4
    let rest = bin(b, IntBinOp::Sub, len, three);
    let (r1, c63) = (bin(b, IntBinOp::Ushr, rest, one), c(b, 63));
    let r2 = bin(b, IntBinOp::Shl, rest, c63);
    let rot = bin(b, IntBinOp::Or, r1, r2);
    let c4 = c(b, 4);
    let fits = b.icmp(IntCond::Ule, rot, c4); // four groups fill the one 8-byte load (NEW-3)
    let hp = bin(b, IntBinOp::Add, ptr, one);
    let hp = at(b, hp);
    let h = b.load_zext_unaligned(Type::I16, hp, 0);
    // the short form at dscale S in one test of the header word (the sign bit masked off)
    let (mask, want) = (c(b, 0xDF80), c(b, 0x8000 | ((s as i64) << 7)));
    let hm = bin(b, IntBinOp::And, h, mask);
    let short = b.icmp(IntCond::Eq, hm, want);
    // the groups: the load's top `8 rest` bits (a mask of two shifts by `32 - 4 rest`: no groups is none), read right-aligned
    let word = crate::rep::varlena::num_short::end_word(b, ptr, len);
    let (two, c32, ones) = (c(b, 2), c(b, 32), c(b, -1));
    let r4 = bin(b, IntBinOp::Shl, rest, two);
    let t = bin(b, IntBinOp::Sub, c32, r4);
    let keep = bin(b, IntBinOp::Shl, ones, t);
    let keep = bin(b, IntBinOp::Shl, keep, t);
    let v = bin(b, IntBinOp::And, word, keep);
    let (g, c48) = (c(b, 0xFFFF), c(b, 48));
    let mut r = bin(b, IntBinOp::Ushr, v, c48);
    for (sh, mul) in [(32, 10_000i64), (16, 100_000_000), (0, 1_000_000_000_000)] {
        let x = if sh == 0 { v } else { let k = c(b, sh); bin(b, IntBinOp::Ushr, v, k) };
        let x = bin(b, IntBinOp::And, x, g);
        let k = c(b, mul);
        let x = bin(b, IntBinOp::Mul, x, k);
        r = bin(b, IntBinOp::Add, r, x);
    }
    // `f + 1 = n - weight` (weight: 7-bit two's complement): f fractional groups, 0 ..= ceil(S / 4) (an image carries no
    // digit past its dscale); f < 0, a stripped trailing integer group, declines (NEW-2); the zero's any
    let c57 = c(b, 57);
    let w = bin(b, IntBinOp::Shl, h, c57);
    let w = bin(b, IntBinOp::Sshr, w, c57);
    let n = bin(b, IntBinOp::Ushr, rest, one);
    let f1 = bin(b, IntBinOp::Sub, n, w);
    let f = bin(b, IntBinOp::Sub, f1, one);
    let fmax = (s as u32).div_ceil(4).min(4);
    let fm = c(b, fmax as i64);
    let mut okf = b.icmp(IntCond::Ule, f, fm);
    // per f, one form: `m = R 10^(S - 4f)`, or `R / 10^(4f - S)` past the point (a shift and a multiply by 5^-k, exact);
    // a multiply by 10^e from e = 3 needs `R < 10^(18 - e)` (i64 holds every 18-digit decimal, R < 10^16)
    let form = |b: &mut FunctionBuilder<'_>, k: u32| -> (Value, Option<Value>) {
        let e = s as i32 - 4 * k as i32;
        if e == 0 {
            (r, None)
        } else if e > 0 {
            let tens = c(b, 10i64.pow(e as u32));
            let m = bin(b, IntBinOp::Mul, r, tens);
            (m, (e >= 3).then(|| { let lim = c(b, 10i64.pow(18 - e as u32)); b.icmp(IntCond::Ult, r, lim) }))
        } else {
            let (kv, inv) = (c(b, -e as i64), c(b, inv5(-e as u32) as i64));
            let q = bin(b, IntBinOp::Ushr, r, kv);
            (bin(b, IntBinOp::Mul, q, inv), None)
        }
    };
    let (mut m, mut small) = form(b, 0);
    for k in 1..=fmax {
        let (mk, sk) = form(b, k);
        let kc = c(b, k as i64);
        let is = b.icmp(IntCond::Eq, f, kc);
        m = b.select(is, mk, m);
        small = match (small, sk) {
            (None, None) => None,
            (x, y) => { let yes = b.bconst(true); Some(b.select(is, y.unwrap_or(yes), x.unwrap_or(yes))) }
        };
    }
    if let Some(sm) = small {
        okf = bin(b, IntBinOp::And, okf, sm);
    }
    let empty = b.icmp(IntCond::Eq, rest, z);
    let okf = bin(b, IntBinOp::Or, okf, empty);
    let neg = c(b, 0x2000);
    let neg = bin(b, IntBinOp::And, h, neg);
    let neg = b.icmp(IntCond::Ne, neg, z);
    let mneg = bin(b, IntBinOp::Sub, z, m);
    let m = b.select(neg, mneg, m);
    let ok = [fits, short, okf].into_iter().fold(hdr, |x, y| bin(b, IntBinOp::And, x, y));
    (m, ok)
}

/// `5^-k mod 2^64` (Newton's iteration: each step doubles the bits right).
fn inv5(k: u32) -> u64 {
    let p = 5u64.pow(k);
    (0..6).fold(p, |x, _| x.wrapping_mul(2u64.wrapping_sub(p.wrapping_mul(x))))
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use gravity_fn::agg_final::{num_mantissa, NO_MANTISSA};

    use super::*;
    use crate::frame::stop::tests::run1;

    /// PG's canonical image of `v / 10^d` (numeric.c `make_result`): base-10000 groups aligned to the point, leading and
    /// trailing zero groups stripped, the zero with no groups at weight 0. `(sign, weight, groups)`.
    fn parts(v: i128, d: u32) -> (bool, i32, Vec<u16>) {
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
            return (false, 0, g);
        }
        (neg, low + g.len() as i32 - 1, g)
    }

    /// The varlena of `v / 10^d`: the short form (or the long, `long`) under a 1-byte header (or a 4-byte one, `four`).
    fn image(v: i128, d: u32, long: bool, four: bool) -> Vec<u8> {
        let (neg, w, g) = parts(v, d);
        let mut p = Vec::new(); // alloc: Program
        if long {
            p.extend_from_slice(&((if neg { 0x4000 } else { 0 }) | d as u16).to_le_bytes());
            p.extend_from_slice(&(w as i16).to_le_bytes());
        } else {
            p.extend_from_slice(&(0x8000 | if neg { 0x2000 } else { 0 } | ((d as u16) << 7) | (w as u16 & 0x7F)).to_le_bytes());
        }
        g.iter().for_each(|x| p.extend_from_slice(&x.to_le_bytes()));
        let mut out = if four { (((p.len() + 4) as u32) << 2).to_le_bytes().to_vec() } else { vec![(((p.len() + 1) as u8) << 1) | 1] }; // alloc: Program
        out.extend_from_slice(&p);
        out
    }

    /// The decode of the image at `p` through the interpreter: its word where it serves, `NO_MANTISSA` where it declines
    /// (which a served word never is).
    fn run(p: *const u8, s: i16) -> i64 {
        run1(&[], &mut |b, env| {
            let ptr = b.iconst(Type::I64, p as i64);
            let (m, ok) = decode(b, env, ptr, s);
            let no = b.iconst(Type::I64, NO_MANTISSA);
            b.select(ok, m, no)
        })
    }

    /// `img` at alignments across 8 bytes (a 4-byte header at its int alignment, as PG places it): where the decode
    /// serves, its word is the host's (`num_mantissa`); it serves exactly when `served` and the host's word is below 10^18
    /// in magnitude. Returns the serves.
    fn check(img: &[u8], s: i16, served: bool) -> usize {
        let mut n = 0;
        let ats: &[usize] = if img[0] & 1 == 0 { &[0, 4] } else { &[0, 1, 3, 6] };
        for &at in ats {
            let mut buf = vec![0xA5u8; 48]; // alloc: Program
            let base = (buf.as_ptr() as usize).next_multiple_of(8) - buf.as_ptr() as usize + 8 + at;
            buf[base..base + img.len()].copy_from_slice(img);
            let p = buf[base..].as_ptr();
            let (got, host) = (run(p, s), unsafe { num_mantissa(core::ptr::null_mut(), p, s as i64).val });
            assert_eq!(got != NO_MANTISSA, served && host.unsigned_abs() < 10u64.pow(18), "{img:02x?} at {at}, scale {s}: got {got}, host {host}");
            if got != NO_MANTISSA {
                assert_eq!(got, host, "{img:02x?} at {at}, scale {s}");
                n += 1;
            }
        }
        n
    }

    /// num.narrow against the host over signs, 1..18 digits, dscales S - 1..S + 1, both headers and the long form, at
    /// scales 0..18: the host's word wherever it serves, and it serves every short 1-byte image at dscale S of at most four
    /// groups with no stripped integer group and at most four fractional ones (the zero too) whose word is below 10^18.
    #[test]
    fn num_narrow_matches_the_host() {
        let mut served = 0;
        for s in 0i16..=18 {
            for d in (s.max(1) - 1) as u32..=s as u32 + 1 {
                let mut vals = vec![0i128]; // alloc: Program
                for digits in 1u32..=18 {
                    for v in [10i128.pow(digits - 1), 10i128.pow(digits) - 1, 10i128.pow(digits) / 7 + 1, 10i128.pow(digits - 1) * 5] {
                        vals.extend([v, -v]); // alloc: Program
                    }
                }
                for v in vals {
                    let (_, w, g) = parts(v, d);
                    let f = g.len() as i32 - 1 - w;
                    let ok = d == s as u32 && g.len() <= 4 && (g.is_empty() || (0..=(s as i32 + 3) / 4).contains(&f) && f <= 4);
                    served += check(&image(v, d, false, false), s, ok);
                    for (long, four) in [(false, true), (true, false), (true, true)] {
                        check(&image(v, d, long, four), s, false);
                    }
                }
            }
        }
        assert!(served > 3_000, "{served}");
    }

    /// NaN / +-Infinity and a TOAST pointer decline (today's decode reads them).
    #[test]
    fn specials_and_toast_decline() {
        for h in [0xC000u16, 0xD000, 0xF000] {
            for s in 0..=18 {
                assert_eq!(check(&[&[(3 << 1) | 1][..], &h.to_le_bytes()].concat(), s, false), 0, "{h:x}");
            }
        }
        let toast = [&[1u8, 18][..], &[0x55; 16]].concat();
        assert_eq!(check(&toast, 2, false), 0);
    }

    extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
        fn munmap(addr: *mut u8, len: usize) -> i32;
        fn getpagesize() -> i32;
    }

    /// D3339: an image at each of its page's first 16 bytes and last 16, the pages on both sides PROT_NONE (num.short's
    /// `end_word`): the host's word wherever it serves, and no read off the image's page (that read is a SIGSEGV here).
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
        let mut served = 0;
        for v in [0i128, 7, -49_069, 104_136, -123_456_789, 1_234_567_890_123] {
            let img = image(v, 2, false, false);
            for at in (0..16).chain(page - 16..=page - img.len()) {
                // SAFETY: `at + img.len() <= page`: the bytes lie in the middle page, which is read-write.
                let p = unsafe {
                    let p = base.add(page + at);
                    core::ptr::copy_nonoverlapping(img.as_ptr(), p, img.len());
                    p as *const u8
                };
                // SAFETY: `p` holds the image for the call.
                let (got, host) = (run(p, 2), unsafe { num_mantissa(core::ptr::null_mut(), p, 2).val });
                if got != NO_MANTISSA {
                    assert_eq!(got, host, "{img:02x?} at page offset {at}");
                    served += 1;
                }
            }
        }
        // SAFETY: the mapping made above, unused past here.
        unsafe { munmap(base, 3 * page) };
        assert!(served > 100, "{served}");
    }

    /// The decode's IR at scale 2 (numeric(15, 2)): straight-line, fewer instructions past the constants than
    /// num.short's 84 and its switch.
    #[test]
    fn narrow_ops() {
        let img = [vec![0u8; 8], image(12_345, 2, false, false)].concat(); // alloc: Program
        let count = |b: &FunctionBuilder<'_>| b.func().blocks.iter().flat_map(|k| k.insts.iter()).filter(|i| !matches!(i.op, gravity_ir::Op::Const { .. })).count();
        let (mut n, mut blocks) = (0, 0);
        run1(&[], &mut |b, env| {
            let (n0, k0) = (count(b), b.num_blocks());
            let ptr = b.iconst(Type::I64, img[8..].as_ptr() as i64);
            let (m, _) = decode(b, env, ptr, 2);
            (n, blocks) = (count(b) - n0, b.num_blocks() - k0);
            m
        });
        eprintln!("num.narrow decode at scale 2: {n} IR instructions, {blocks} blocks");
        assert_eq!(blocks, 0, "straight-line");
        assert!(n < 84, "{n}");
    }
}

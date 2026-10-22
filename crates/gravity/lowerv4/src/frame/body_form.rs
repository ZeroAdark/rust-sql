//! Road `body.form` (H06b, R13-L05): a pgrc window's validity and form pick the fast body: all-valid lanes (no validity
//! test per row) and code lanes served as codes (a step compares or groups by the code, not the decoded value). The guard
//! is the AND of the window's `nonulls` and `codes` facts; `nonulls` is left to `body.versioned` when that road applies
//! (one test, not two). The frame ANDs both roads' guards into one fast body (frame.rs `row_loop`). The packed body is
//! this road's third form: a window whose filter lanes are ByteFor at width 1 / 2 (`WinConds::packed`) runs the block path
//! with the spanned steps' masks on the raw deltas (`packed_word`, taken by mask_vec.rs; the lab kernels' rc.rs `range_mask`), a
//! lower and an upper bound on one lane in adjacent steps as one unsigned compare of `delta - lo` against the span
//! (`packed_range`), and a counting end's count without a movemask (`count`). Budget 160 lines.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use mcx::{CollectIn, PgVec};

use crate::face::strategy::BODY_VERSIONED;
use crate::face::source::{Body, BodyImpl, BodyMech, Packed, WinConds};
use crate::frame::env::Env;

crate::strategies! {
    /// Validity and codes-vs-decoded body per window.
    BODY_FORM = 6, "body.form", Pgrc;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(BODY_FORM, &BodyImpl(&FORM))], ..crate::ops::NONE };

struct Form;
static FORM: Form = Form;

impl BodyMech for Form {
    fn guard(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, f: &WinConds) -> Option<(Value, Body)> {
        let nonulls = if env.applies(BODY_VERSIONED) { None } else { f.nonulls };
        let known: PgVec<'_, Value> = [nonulls, f.codes].into_iter().flatten().collect_in(env.mcx);
        let g = crate::frame::frame::all(b, env, &known[..])?;
        Some((g, Body { nonulls: nonulls.is_some(), codes: f.codes.is_some(), ..Body::default() }))
    }
}

/// The packed mask of one compare over a block's ByteFor deltas: bit i holds when row i's value `reference + delta`
/// stands in `cond` (signed: the lane's integer order, which is the unsigned delta order) to `konst`, before validity
/// (a NULL row's delta is garbage: the caller ANDs validity in). The constant moves into the delta domain; a frame whose
/// `[reference, fmax]` the compare cannot split answers the block whole, its deltas unread (so does width 0, a constant
/// frame); a NULL constant passes none. `count`: the number of rows that hold instead (no movemask).
pub(crate) fn packed_word(b: &mut FunctionBuilder<'_>, pk: &Packed, cond: IntCond, konst: Value, knull: Value, count: bool) -> Value {
    let (r, i64t) = (pk.reference, Type::I64);
    let zero = b.iconst(i64t, 0);
    let hi = frame_top(b, pk);
    let (lt_r, le_r) = (b.icmp(IntCond::Slt, konst, r), b.icmp(IntCond::Sle, konst, r));
    let (gt_hi, ge_hi) = (b.icmp(IntCond::Sgt, konst, hi), b.icmp(IntCond::Sge, konst, hi));
    let (outside, no) = (b.int_bin(IntBinOp::Or, lt_r, gt_hi), b.bconst(false));
    let (none, all, ucond) = match cond {
        IntCond::Eq => (outside, no, IntCond::Eq),
        IntCond::Ne => (no, outside, IntCond::Ne),
        IntCond::Slt => (le_r, gt_hi, IntCond::Ult),
        IntCond::Sle => (lt_r, ge_hi, IntCond::Ule),
        IntCond::Sgt => (ge_hi, lt_r, IntCond::Ugt),
        _ => (gt_hi, le_r, IntCond::Uge),
    };
    // width 0: a constant frame, every value the reference (no deltas read)
    let (is0, at_r) = (b.icmp(IntCond::Eq, pk.width, zero), b.icmp(cond, r, konst));
    let (all, not_r) = (b.select(is0, at_r, all), b.select(at_r, no, is0));
    let none = b.select(is0, not_r, none);
    let none = b.int_bin(IntBinOp::Or, none, knull);
    let all = b.select(knull, no, all);
    let c = b.int_bin(IntBinOp::Sub, konst, r);
    blocks(b, pk, (none, all), None, (ucond, c), count)
}

/// The frame's largest value: `fmax`, else the reference plus the width's largest delta (`i64::MAX` past it).
fn frame_top(b: &mut FunctionBuilder<'_>, pk: &Packed) -> Value {
    pk.fmax.unwrap_or_else(|| {
        let (one, t1, t2) = (b.iconst(Type::I64, 1), b.iconst(Type::I64, 0xff), b.iconst(Type::I64, 0xffff));
        let is1 = b.icmp(IntCond::Eq, pk.width, one);
        let top = b.select(is1, t1, t2);
        let s = b.int_bin(IntBinOp::Add, pk.reference, top);
        let wrapped = b.icmp(IntCond::Slt, s, pk.reference);
        let max = b.iconst(Type::I64, i64::MAX);
        b.select(wrapped, max, s)
    })
}

/// The packed mask of `lane >=/> lo AND lane <=/< hi` (each `(cond, constant, its null)`) as ONE unsigned compare per
/// row: `delta - c <=u span`, `c` and `span` the inclusive bounds clamped to the frame, moved into the delta domain (a
/// delta under `c` wraps past `span`). A bound that steps out of i64 (`> i64::MAX`, `< i64::MIN`), a NULL constant or
/// an empty range passes none; a range covering the frame passes the block whole, its deltas unread. `count` as above.
pub(crate) fn packed_range(b: &mut FunctionBuilder<'_>, pk: &Packed, lo: (IntCond, Value, Value), hi: (IntCond, Value, Value), count: bool) -> Value {
    let (r, i64t) = (pk.reference, Type::I64);
    let (one, zero, no) = (b.iconst(i64t, 1), b.iconst(i64t, 0), b.bconst(false));
    let top = frame_top(b, pk);
    let (min, max) = (b.iconst(i64t, i64::MIN), b.iconst(i64t, i64::MAX));
    let lo_ovf = if lo.0 == IntCond::Sgt { b.icmp(IntCond::Eq, lo.1, max) } else { no };
    let hi_ovf = if hi.0 == IntCond::Slt { b.icmp(IntCond::Eq, hi.1, min) } else { no };
    let lo_i = if lo.0 == IntCond::Sgt { b.int_bin(IntBinOp::Add, lo.1, one) } else { lo.1 };
    let hi_i = if hi.0 == IntCond::Slt { b.int_bin(IntBinOp::Sub, hi.1, one) } else { hi.1 };
    let (under, over) = (b.icmp(IntCond::Slt, lo_i, r), b.icmp(IntCond::Sgt, hi_i, top));
    let (lo_c, hi_c) = (b.select(under, r, lo_i), b.select(over, top, hi_i));
    let (lo_le_r, hi_ge_top) = (b.icmp(IntCond::Sle, lo_i, r), b.icmp(IntCond::Sge, hi_i, top));
    let mut none = b.icmp(IntCond::Sgt, lo_c, hi_c); // empty
    for x in [lo.2, hi.2, lo_ovf, hi_ovf] {
        none = b.int_bin(IntBinOp::Or, none, x);
    }
    let all = b.int_bin(IntBinOp::And, lo_le_r, hi_ge_top);
    // width 0: a constant frame, every value the reference (no deltas read)
    let (is0, hi_ge_r) = (b.icmp(IntCond::Eq, pk.width, zero), b.icmp(IntCond::Sge, hi_i, r));
    let at_r = b.int_bin(IntBinOp::And, lo_le_r, hi_ge_r);
    let (all, none0) = (b.select(is0, at_r, all), b.select(at_r, no, is0));
    let none = b.int_bin(IntBinOp::Or, none, none0);
    let not_none = b.select(none, no, all);
    let (c, span) = (b.int_bin(IntBinOp::Sub, lo_c, r), b.int_bin(IntBinOp::Sub, hi_c, lo_c));
    blocks(b, pk, (none, not_none), Some(c), (IntCond::Ule, span), count)
}

/// The block word (`count`: its row count): `early.1` (every row) or `early.0` (none) without reading a delta, else
/// `delta_word` at the frame's width (the chunk's lanes less `sub`, then compared with `cmp.1`).
fn blocks(b: &mut FunctionBuilder<'_>, pk: &Packed, early: (Value, Value), sub: Option<Value>, cmp: (IntCond, Value), count: bool) -> Value {
    let (i64t, zero, one) = (Type::I64, b.iconst(Type::I64, 0), b.iconst(Type::I64, 1));
    let (ones, all) = (b.iconst(i64t, if count { 64 } else { -1 }), early.1);
    let (early, early_word) = (b.int_bin(IntBinOp::Or, early.0, all), b.select(all, ones, zero));
    let (l_test, l_w1, l_w2, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[i64t]));
    b.cond_br(early, (l_join, &[early_word]), (l_test, &[]));
    b.switch_to(l_test);
    let is1 = b.icmp(IntCond::Eq, pk.width, one);
    b.cond_br(is1, (l_w1, &[]), (l_w2, &[]));
    for (l, vty, lty, rows) in [(l_w1, Type::V16I8, Type::I8, 16u32), (l_w2, Type::V8I16, Type::I16, 8)] {
        b.switch_to(l);
        let word = delta_word(b, pk.deltas, sub, cmp, (vty, lty, rows), count);
        b.br(l_join, &[word]);
    }
    b.switch_to(l_join);
    b.block_params(l_join)[0]
}

/// The 64 deltas from `deltas` (less `sub`, wrapping) compared (`cmp.0`, unsigned) with `cmp.1` 16 bytes at a time, each
/// chunk's bits placed at its rows; `count`: the chunks' true lanes (-1 each) summed lane-wise (at most 8 chunks: a lane
/// stays in -8..=0), then across, negated.
fn delta_word(b: &mut FunctionBuilder<'_>, deltas: Value, sub: Option<Value>, (ucond, c): (IntCond, Value), (vty, lty, rows): (Type, Type, u32), count: bool) -> Value {
    let splat = |b: &mut FunctionBuilder<'_>, x: Value| {
        let t = b.conv(gravity_ir::ConvOp::Trunc, lty, x);
        b.vsplat(vty, t)
    };
    let s = splat(b, c);
    let off = sub.map(|x| splat(b, x));
    let (mut word, mut acc) = (b.iconst(Type::I64, 0), None);
    for k in 0..64 / rows {
        let v = b.vload_unaligned(vty, deltas, (16 * k) as i32);
        let v = match off {
            Some(o) => b.vbin(gravity_ir::VecBinOp::Sub, v, o),
            None => v,
        };
        let bits = b.vcmp(ucond, v, s);
        if count {
            acc = Some(acc.map_or(bits, |a| b.vbin(gravity_ir::VecBinOp::Add, a, bits)));
            continue;
        }
        let mut bits = b.vmovmask(bits);
        if k > 0 {
            let sh = b.iconst(Type::I64, (k * rows) as i64);
            bits = b.int_bin(IntBinOp::Shl, bits, sh);
        }
        word = b.int_bin(IntBinOp::Or, word, bits);
    }
    let Some(a) = acc else { return word };
    let sum = b.vreduce_add(a);
    b.int_bin(IntBinOp::Sub, word, sum)
}

#[cfg(test)]
mod tests {
    use gravity_interp::Interp;
    use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
    use gravity_ir::{IntCond, Type, Value};

    use crate::face::source::Packed;

    const CONDS: [IntCond; 6] = [IntCond::Eq, IntCond::Ne, IntCond::Slt, IntCond::Sle, IntCond::Sgt, IntCond::Sge];

    /// The block's delta bytes at width `w` (width 0 reads none), padded for the last chunk's load.
    fn bytes(d: &[u64], w: i64) -> Vec<u8> {
        d.iter().flat_map(|&x| x.to_le_bytes().into_iter().take(w.max(1) as usize)).chain([0u8; 16]).collect()
    }

    /// The word `emit` builds over block `d` (64 deltas at width `w`, reference `r`), run by the interpreter.
    fn run(d: &[u64], w: i64, r: i64, fmax: Option<i64>, emit: impl Fn(&mut FunctionBuilder<'_>, &Packed) -> Value) -> u64 {
        let bytes = bytes(d, w);
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("packed", &[Type::Ptr], &[Type::I64]);
        let mut b = pb.define(f);
        let deltas = b.params()[0];
        let (width, reference, fmax) = (b.iconst(Type::I64, w), b.iconst(Type::I64, r), fmax.map(|m| b.iconst(Type::I64, m)));
        let word = emit(&mut b, &Packed { deltas, width, reference, fmax });
        b.ret(&[word]);
        let p = pb.finish();
        Interp::new(&p).run(f, &[bytes.as_ptr() as u64]).expect("runs")[0]
    }

    /// `packed_word` over block `d` for each of CONDS (`count`: its count form).
    fn packed(d: &[u64], w: i64, r: i64, fmax: Option<i64>, k: i64, null: bool, count: bool) -> Vec<u64> {
        CONDS.iter().map(|&cond| run(d, w, r, fmax, |b, pk| {
            let (konst, knull) = (b.iconst(Type::I64, k), b.bconst(null));
            super::packed_word(b, pk, cond, konst, knull, count)
        })).collect()
    }

    /// The decoded mask: bit i when row i is valid and `r + d[i]` stands in the compare to `k`.
    fn decoded(d: &[u64], r: i64, k: i64, null: bool, valid: u64) -> Vec<u64> {
        let cmp = |c: IntCond, v: i64| match c { IntCond::Eq => v == k, IntCond::Ne => v != k, IntCond::Slt => v < k, IntCond::Sle => v <= k, IntCond::Sgt => v > k, _ => v >= k };
        CONDS.iter().map(|&c| (0..64).filter(|&i| !null && valid >> i & 1 == 1 && cmp(c, r + d[i] as i64)).fold(0, |m, i| m | 1 << i)).collect()
    }

    /// The mask form ANDed with validity equals the decoded mask; the count form counts the mask form's bits (over every
    /// row: the count form is taken only over a lane with no nulls).
    fn check(d: &[u64], w: i64, r: i64, fmax: Option<i64>, k: i64, null: bool, valid: u64) {
        let words = packed(d, w, r, fmax, k, null, false);
        let got: Vec<u64> = words.iter().map(|m| m & valid).collect();
        assert_eq!(got, decoded(d, r, k, null, valid), "w {w} r {r} fmax {fmax:?} k {k} null {null} d {d:?}");
        let ones: Vec<u64> = words.iter().map(|m| m.count_ones() as u64).collect();
        assert_eq!(packed(d, w, r, fmax, k, null, true), ones, "count: w {w} r {r} fmax {fmax:?} k {k} null {null} d {d:?}");
    }

    #[test]
    fn the_packed_mask_at_the_width_and_frame_boundaries() {
        for (w, top) in [(1i64, 255i64), (2, 65535)] {
            for r in [0i64, -1000, 7_000_000_000, i64::MAX - top, i64::MIN] {
                let d: Vec<u64> = (0..64).map(|i| [0, top, 1, top - 1][i % 4] as u64).collect();
                for k in [r.wrapping_sub(1), r, r + 1, r + top - 1, r + top, r.saturating_add(top + 1)] {
                    check(&d, w, r, None, k, false, !0);
                    check(&d, w, r, Some(r + top), k, false, !0);
                }
            }
        }
        // width 0: a constant frame; fmax inside the width's range
        check(&[0; 64], 0, 42, Some(42), 42, false, !0);
        check(&[0; 64], 0, 42, Some(42), 43, false, !0);
        let d: Vec<u64> = (0..64).map(|i| i as u64).collect();
        for k in [9, 10, 11, 72, 73, 74, 75] {
            check(&d, 1, 10, Some(73), k, false, !0);
            check(&d, 1, 10, Some(73), k, true, !0);
        }
    }

    #[test]
    fn the_packed_mask_equals_the_decoded_mask_on_random_blocks_with_nulls() {
        let mut s = 0x2545_f491_4f6c_dd1du64; // xorshift64 state
        let mut rnd = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for _ in 0..300 {
            let (w, spread) = if rnd() % 2 == 0 { (1i64, 1 + rnd() % 256) } else { (2, 1 + rnd() % 65536) };
            let r = (rnd() % (1 << 41)) as i64 - (1 << 40);
            let valid = if rnd() % 3 == 0 { !0 } else { rnd() & rnd() | rnd() };
            // a NULL row's delta is garbage (validity kills it); a valid row's lies in the frame's range
            let d: Vec<u64> = (0..64).map(|i| if valid >> i & 1 == 1 { rnd() % spread } else { rnd() % (1 << (8 * w)) }).collect();
            let hi = (0..64).filter(|&i| valid >> i & 1 == 1).map(|i| d[i]).max().unwrap_or(0) as i64;
            let fmax = (rnd() % 2 == 0).then_some(r + hi);
            let k = match rnd() % 4 {
                0 => r + d[(rnd() % 64) as usize] as i64,
                1 => r + (rnd() % (spread + 2)) as i64 - 1,
                2 => r + hi + (rnd() % 3) as i64 - 1,
                _ => (rnd() >> 1) as i64 - (1 << 62),
            };
            check(&d, w, r, fmax, k, rnd() % 16 == 0, valid);
        }
    }

    /// `packed_range` over block `d` for each lower (`>=`, `>`) by upper (`<=`, `<`) compare; each bound `(constant, null)`.
    fn ranged(d: &[u64], w: i64, r: i64, fmax: Option<i64>, lo: (i64, bool), hi: (i64, bool), count: bool) -> Vec<u64> {
        let pairs = [(IntCond::Sge, IntCond::Sle), (IntCond::Sge, IntCond::Slt), (IntCond::Sgt, IntCond::Sle), (IntCond::Sgt, IntCond::Slt)];
        pairs.iter().map(|&(lc, hc)| run(d, w, r, fmax, |b, pk| {
            let (lv, ln, hv, hn) = (b.iconst(Type::I64, lo.0), b.bconst(lo.1), b.iconst(Type::I64, hi.0), b.bconst(hi.1));
            super::packed_range(b, pk, (lc, lv, ln), (hc, hv, hn), count)
        })).collect()
    }

    /// The decoded range mask, in `ranged`'s order: bit i when row i is valid and `r + d[i]` lies between the bounds.
    fn range_decoded(d: &[u64], r: i64, lo: (i64, bool), hi: (i64, bool), valid: u64) -> Vec<u64> {
        [(false, false), (false, true), (true, false), (true, true)].iter().map(|&(gt, lt)| {
            let pass = |v: i64| (if gt { v > lo.0 } else { v >= lo.0 }) && (if lt { v < hi.0 } else { v <= hi.0 });
            (0..64).filter(|&i| !lo.1 && !hi.1 && valid >> i & 1 == 1 && pass(r + d[i] as i64)).fold(0, |m, i| m | 1 << i)
        }).collect()
    }

    fn check_range(d: &[u64], w: i64, r: i64, fmax: Option<i64>, lo: (i64, bool), hi: (i64, bool), valid: u64) {
        let words = ranged(d, w, r, fmax, lo, hi, false);
        let got: Vec<u64> = words.iter().map(|m| m & valid).collect();
        assert_eq!(got, range_decoded(d, r, lo, hi, valid), "w {w} r {r} fmax {fmax:?} lo {lo:?} hi {hi:?} d {d:?}");
        let ones: Vec<u64> = words.iter().map(|m| m.count_ones() as u64).collect();
        assert_eq!(ranged(d, w, r, fmax, lo, hi, true), ones, "count: w {w} r {r} fmax {fmax:?} lo {lo:?} hi {hi:?} d {d:?}");
    }

    #[test]
    fn the_packed_range_at_the_width_frame_and_i64_boundaries() {
        for (w, top) in [(1i64, 255i64), (2, 65535)] {
            for r in [0i64, -1000, 7_000_000_000, i64::MAX - top, i64::MIN] {
                let d: Vec<u64> = (0..64).map(|i| [0, top, 1, top - 1, top / 2][i % 5] as u64).collect();
                let ks = [i64::MIN, r.wrapping_sub(1), r, r + 1, r + top / 2, r + top - 1, r + top, r.saturating_add(top + 1), i64::MAX];
                for &lo in ks.iter() {
                    for &hi in ks.iter() {
                        check_range(&d, w, r, None, (lo, false), (hi, false), !0);
                        check_range(&d, w, r, Some(r + top), (lo, false), (hi, false), 0x5555_5555_5555_5555);
                    }
                }
                check_range(&d, w, r, None, (r, true), (r + top, false), !0);
                check_range(&d, w, r, None, (r, false), (r + top, true), !0);
            }
        }
        // width 0: a constant frame (no deltas read)
        for (lo, hi) in [(41, 43), (42, 42), (43, 44), (40, 41), (i64::MIN, i64::MAX)] {
            check_range(&[0; 64], 0, 42, Some(42), (lo, false), (hi, false), !0);
            check_range(&[0; 64], 0, 42, Some(42), (lo, true), (hi, false), !0);
        }
    }

    #[test]
    fn the_packed_range_equals_the_decoded_range_on_random_blocks_with_nulls() {
        let mut s = 88_172_645_463_325_252u64; // xorshift64 state (Marsaglia's seed; not the key hash's PHI64)
        let mut rnd = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for _ in 0..300 {
            let (w, spread) = if rnd() % 2 == 0 { (1i64, 1 + rnd() % 256) } else { (2, 1 + rnd() % 65536) };
            let r = (rnd() % (1 << 41)) as i64 - (1 << 40);
            let valid = if rnd() % 3 == 0 { !0 } else { rnd() & rnd() | rnd() };
            let d: Vec<u64> = (0..64).map(|i| if valid >> i & 1 == 1 { rnd() % spread } else { rnd() % (1 << (8 * w)) }).collect();
            let hi = (0..64).filter(|&i| valid >> i & 1 == 1).map(|i| d[i]).max().unwrap_or(0) as i64;
            let fmax = (rnd() % 2 == 0).then_some(r + hi);
            let mut k = || match rnd() % 5 {
                0 => r + d[(rnd() % 64) as usize] as i64,
                1 => r + (rnd() % (spread + 2)) as i64 - 1,
                2 => r + hi + (rnd() % 3) as i64 - 1,
                3 => [i64::MIN, i64::MAX][(rnd() % 2) as usize],
                _ => (rnd() >> 1) as i64 - (1 << 62),
            };
            let (lo, up) = (k(), k());
            check_range(&d, w, r, fmax, (lo, rnd() % 16 == 0), (up, rnd() % 16 == 0), valid);
        }
    }
}

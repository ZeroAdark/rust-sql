//! Kind `rep.word.order` (cards/M3.md L2, M3-inventory I40): a by-value datum word's order image, one `I64` word that compares
//! as a signed word in PostgreSQL's btree order for the type (rep/code/order.rs's convention: the float order words, a code's
//! prefix with its top bit flipped). Ported from v2 `tail/sort.rs:1` / `text/keys.rs` `float_image`, ladder `sort.rs:1922`.
//! - Integers (`WordImage::Raw`): the value at its type length, sign-extended when signed (int2 / int4 / int8, date, time,
//!   timestamp(tz), money), zero-extended when not (oid, regproc, bool, "char": `charlt` compares unsigned bytes); an 8-byte
//!   unsigned word (xid8) is flipped at its top bit, so its unsigned order compares signed.
//! - Floats (`WordImage::Float`): the IEEE total-order image of the float8 value (a float4 widened, as `float48cmp` compares):
//!   `-0` folded to `+0` and every NaN to `i64::MAX`, the greatest (`float8_cmp_internal`: NaN sorts above every non-NaN
//!   and equals NaN). A float4's image is its float8's, so one image compares across the two sides of a merge.
//! - An enum orders by `enumsortorder`, which `ALTER TYPE .. ADD VALUE` can move with no relcache or pg_proc invalidation,
//!   so a rank table baked into a cached program could go stale: an enum orders through its comparator (`rep.hosted.order`).
//! DESC and NULLS FIRST / LAST fold in `fold` (a run's key column: `gravity_abi` TopK's one signed word, or its
//! `TOPK_PAIRS` pair `[flags, word]`). O3 (M3 decision 16): `name` under a collation that orders by bytes is its 64-byte
//! NUL-padded image, eight words (`name_image`), for psql `\d`'s `ORDER BY` over `nspname` / `relname`.
use gravity_decompose::catalog::typefacts::{KeyColl, TypeFacts};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, FloatBinOp, FloatCond, IntBinOp, IntCond, IntUnOp, Type, Value};
use gravity_program::Rep;

use crate::face::kinds::KindId;
use crate::admit::refusal::{Reason, ShapeWhy, Variant};
use crate::ops::Table;
use crate::rep::{self, CellRef, Lx, OrderArm, Words};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::RepWordOrder, &ARM)], ..crate::ops::NONE };

pub(crate) const ARM: OrderArm = OrderArm { image, name: Some(name_image) };

/// `NAMEDATALEN`: a `name` is this many bytes, NUL-padded (`namein` / `namestrcpy`), never holding a NUL of its own.
pub const NAME_BYTES: u16 = types_core::fmgr::NAMEDATALEN as u16;

/// The order image of a word cell: exact (equal images are equal under the type's `=`: `float8eq` has `-0 = 0`, NaN = NaN).
/// A NULL's image is undefined: `fold` puts the NULL by its flag.
fn image(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, _coll: KeyColl) -> Words {
    if !matches!(c.cell.rep, Rep::Word { .. }) {
        crate::refuse!(cx, Reason::Bug { payload: "a word order image over a cell that is not a word" });
    }
    if tf.image.is_enum() {
        crate::not_built!(cx, Variant::Kind(KindId::RepHostedOrderFixed));
    }
    let (v, _) = rep::datum(cx, c, tf);
    let w = match (tf.image.int_signed(), tf.image.float32()) {
        (Some(signed), _) => int_order(cx.b, v, tf.typlen, signed),
        (_, Some(f32)) => float_order(cx.b, v, f32),
        _ => crate::refuse!(cx, Reason::Bug { payload: "a word cell whose type has no word order" }),
    };
    Words { w: [Some(w), None], exact: true }
}

/// An integer datum word at `typlen` bytes as a signed-comparable word: its low bytes decide, whatever its upper bits.
pub fn int_order(b: &mut FunctionBuilder<'_>, v: Value, typlen: i16, signed: bool) -> Value {
    let ty = match typlen {
        1 => Type::I8,
        2 => Type::I16,
        4 => Type::I32,
        8 if !signed => {
            let top = b.const_raw(Type::I64, 1 << 63);
            return b.int_bin(IntBinOp::Xor, v, top);
        }
        _ => return v,
    };
    let t = b.conv(ConvOp::Trunc, ty, v);
    b.conv(if signed { ConvOp::Sext } else { ConvOp::Zext }, Type::I64, t)
}

/// A float datum word's total-order image (a float4's datum is its bits in the low half): the float8 bits of `x + 0.0`
/// (`-0 + 0 = +0`), a negative's magnitude bits flipped so the words compare signed, NaN the greatest word.
pub fn float_order(b: &mut FunctionBuilder<'_>, v: Value, f32: bool) -> Value {
    let f = if f32 {
        let t = b.conv(ConvOp::Trunc, Type::I32, v);
        let s = b.conv(ConvOp::Bitcast, Type::F32, t);
        b.conv(ConvOp::Fpromote, Type::F64, s)
    } else {
        b.conv(ConvOp::Bitcast, Type::F64, v)
    };
    let fz = b.f64const(0.0);
    let f = b.float_bin(FloatBinOp::Add, f, fz);
    let bits = b.conv(ConvOp::Bitcast, Type::I64, f);
    let (s63, imax) = (b.iconst(Type::I64, 63), b.iconst(Type::I64, i64::MAX));
    let sign = b.int_bin(IntBinOp::Sshr, bits, s63);
    let mask = b.int_bin(IntBinOp::And, sign, imax);
    let img = b.int_bin(IntBinOp::Xor, bits, mask);
    let nan = b.fcmp(FloatCond::Unordered, f, f);
    b.select(nan, imax, img)
}

/// Whether a type's order image fits 33 signed bits (an integer of at most four bytes), so a NULL folds into the one word
/// at `i64::MIN` / `i64::MAX` with room to spare after a DESC's `!` (`fold`). A float's image is 64 bits wide.
pub const fn narrow(tf: &TypeFacts) -> bool {
    tf.typlen <= 4 && tf.image.is_word()
}

/// A key's order image with DESC and NULLS FIRST / LAST folded in, as a run's key column holds it (gravity_abi TopK).
#[derive(Clone, Copy, Debug)]
pub enum Folded {
    /// One word compared signed: the image (`!image` for DESC), a NULL at `i64::MIN` (NULLS FIRST) / `i64::MAX` (LAST).
    One(Value),
    /// `TOPK_PAIRS`: `[flags, word]`, flags compared signed (0 a NULLS FIRST NULL, 1 a value, 2 a NULLS LAST NULL), then
    /// the word unsigned (the image with its top bit flipped, `!` for DESC; 0 for a NULL, so two NULLs tie).
    Pair([Value; 2]),
}

/// `img` (an order arm's signed-comparable word) folded for a sort key: DESC reverses the values only, a NULL's place is
/// its NULLS flag's (PostgreSQL's DESC defaults to NULLS FIRST in the planner, never here). `null` None: never NULL.
pub fn fold(b: &mut FunctionBuilder<'_>, img: Value, null: Option<Value>, narrow: bool, desc: bool, nulls_first: bool) -> Folded {
    let w = if desc { b.int_un(IntUnOp::Not, img) } else { img };
    let Some(null) = null else { return Folded::One(w) };
    if narrow {
        let at = b.iconst(Type::I64, if nulls_first { i64::MIN } else { i64::MAX });
        return Folded::One(b.select(null, at, w));
    }
    let (one, zero) = (b.iconst(Type::I64, 1), b.iconst(Type::I64, 0));
    let f_null = b.iconst(Type::I64, if nulls_first { 0 } else { 2 });
    let flags = b.select(null, f_null, one);
    let top = b.const_raw(Type::I64, 1 << 63);
    let u = b.int_bin(IntBinOp::Xor, w, top);
    Folded::Pair([flags, b.select(null, zero, u)])
}

/// O3: a `name` cell's order image under `coll`: its 64 bytes as eight words, each big-endian with its top bit flipped (the
/// signed convention), compared in order, every byte from the first NUL on zeroed (whatever a producer left past it). That
/// is `namecmp`'s order under a collation that orders by bytes (C / POSIX: `strncmp(a, b, NAMEDATALEN)`, unsigned bytes,
/// stopping at the NUL): a prefix orders first. Any other collation is its sort key's (`key.coll`, M6). A NULL's words
/// are 0 (nothing is read); `fold` places it.
pub fn name_image(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, coll: KeyColl) -> [Value; 8] {
    if !matches!(c.cell.rep, Rep::FixedPtr { len: NAME_BYTES }) {
        crate::refuse!(cx, Reason::Bug { payload: "a name order image over a cell that is not a 64-byte image" });
    }
    if !coll.bytes_order {
        crate::refuse!(cx, Reason::Shape { why: ShapeWhy("a name key under a collation that does not order by bytes") });
    }
    let (p, null) = rep::datum(cx, c, tf);
    let (l_img, l_out) = (cx.b.create_block(&[]), cx.b.create_block(&[Type::I64; 8]));
    let zero = cx.b.iconst(Type::I64, 0);
    cx.b.cond_br(null, (l_out, &[zero; 8]), (l_img, &[]));
    cx.b.switch_to(l_img);
    let ptr = cx.b.conv(ConvOp::IntToPtr, Type::Ptr, p);
    let b = &mut *cx.b;
    let (top, ones, lo7, hi7) = (b.const_raw(Type::I64, 1 << 63), b.iconst(Type::I64, -1), b.const_raw(Type::I64, 0x0101_0101_0101_0101), b.const_raw(Type::I64, 0x8080_8080_8080_8080));
    let (mut seen, zero, one, seven) = (b.bconst(false), b.iconst(Type::I64, 0), b.iconst(Type::I64, 1), b.iconst(Type::I64, 7));
    let mut words = [zero; 8];
    for (i, out) in words.iter_mut().enumerate() {
        // the bytes before the word's first NUL (the haszero trick: its lowest flagged byte is exact), none after a NUL
        let w = b.load_unaligned(Type::I64, ptr, 8 * i as i32);
        let w = if cfg!(target_endian = "little") { w } else { b.int_un(IntUnOp::Bswap, w) };
        let (d, nw) = (b.int_bin(IntBinOp::Sub, w, lo7), b.int_un(IntUnOp::Not, w));
        let t = b.int_bin(IntBinOp::And, d, nw);
        let t = b.int_bin(IntBinOp::And, t, hi7);
        let neg = b.int_un(IntUnOp::Neg, t);
        let low = b.int_bin(IntBinOp::And, t, neg);
        let below = b.int_bin(IntBinOp::Ushr, low, seven);
        let below = b.int_bin(IntBinOp::Sub, below, one);
        let nul = b.icmp(IntCond::Ne, t, zero);
        let keep = b.select(nul, below, ones);
        let keep = b.select(seen, zero, keep);
        let w = b.int_bin(IntBinOp::And, w, keep);
        seen = b.select(seen, seen, nul);
        let be = b.int_un(IntUnOp::Bswap, w);
        *out = b.int_bin(IntBinOp::Xor, be, top);
    }
    cx.b.br(l_out, &words);
    cx.b.switch_to(l_out);
    core::array::from_fn(|i| cx.b.block_params(l_out)[i])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{At, Cell, CellHome, CellLife, Nullable};
    use crate::admit::refusal::Reason;
    use gravity_decompose::catalog::typefacts::{row_of, KeyColl};
    use gravity_interp::Interp;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_program::{NumTypmod, Until};

    const C: KeyColl = KeyColl { oid: 950, bytes_eq: true, bytes_order: true };

    fn cell(rep: Rep, typid: u32) -> Cell {
        Cell { rep, typid, num: NumTypmod::NONE, life: CellLife { until: Until::Row, home: CellHome::RowCx }, at: At::Value(0), null: Nullable::Maybe }
    }

    /// `f` over a function of two `I64` params (the datum word, a NULL flag word) returning `n` words, run on `args`.
    fn run(n: usize, args: [u64; 2], f: &dyn Fn(&mut Lx<'_, '_, '_>, Value, Value) -> Vec<Value>) -> Result<Vec<u64>, crate::Refusal> {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        crate::catch(9, || {
            let mut pb = ProgramBuilder::new();
            let fid = pb.declare_func("o", &[Type::I64, Type::I64], &vec![Type::I64; n]);
            let mut b = pb.define(fid);
            let (v, nw) = (b.params()[0], b.params()[1]);
            let out = {
                let zero = b.iconst(Type::I64, 0);
                let null = b.icmp(IntCond::Ne, nw, zero);
                let mut cx = Lx::new(&mut b, 9, rig.mcx());
                f(&mut cx, v, null)
            };
            b.ret(&out);
            let prog = pb.finish();
            Interp::new(&prog).run(fid, &args).expect("runs")
        })
    }

    /// The order image of the datum word `d` of a word cell of type `typid`, through the registered arm.
    fn img(typid: u32, d: u64) -> i64 {
        let (tf, c) = (*row_of(typid).expect("type facts"), cell(Rep::Word { bytes: 8 }, typid));
        let w = run(1, [d, 0], &|cx, v, _| vec![rep::order_words(cx, CellRef { cell: &c, val: v, null: None }, &tf, C).w[0].unwrap()]);
        w.expect("an image")[0] as i64
    }

    fn ascending(xs: &[i64]) -> bool {
        xs.windows(2).all(|w| w[0] < w[1])
    }

    #[test]
    fn integer_images_order_as_the_type() {
        let i2: Vec<i64> = [-32768i64, -5, 0, 7, 32767].iter().map(|&x| img(21, x as u16 as u64 | 0xABCD_0000)).collect();
        assert!(ascending(&i2), "int2 by its low 16 bits, signed: {i2:?}");
        let i4: Vec<i64> = [i32::MIN as i64, -1, 0, 1, i32::MAX as i64].iter().map(|&x| img(23, x as u64)).collect();
        assert!(ascending(&i4), "{i4:?}");
        let i8: Vec<i64> = [i64::MIN, -1, 0, 1, i64::MAX].iter().map(|&x| img(20, x as u64)).collect();
        assert!(ascending(&i8), "{i8:?}");
        assert!(img(26, 0xFFFF_FFFF) > img(26, 7), "oid unsigned");
        assert!(img(18, 0xFF) > img(18, 0x01), "\"char\" compares unsigned bytes (charlt)");
        assert!(img(16, 1) > img(16, 0), "true after false");
        assert!(img(5069, 1 << 63) > img(5069, 7), "xid8 unsigned");
        assert!(ascending(&[img(1082, -3i64 as u64), img(1082, 0), img(1114, 5)]), "date / timestamp as ints");
    }

    #[test]
    fn floats_order_totally_with_nan_greatest() {
        let f8 = |x: f64| img(701, x.to_bits());
        let xs = [f64::NEG_INFINITY, -1e300, -2.5, -1.0, -1e-300, 0.0, 1e-300, 1.0, 2.5, 1e300, f64::INFINITY, f64::NAN];
        let w: Vec<i64> = xs.iter().map(|&x| f8(x)).collect();
        assert!(ascending(&w), "{w:?}");
        assert_eq!(f8(-0.0), f8(0.0), "-0 = +0");
        assert_eq!(f8(-f64::NAN), f8(f64::NAN), "every NaN is one value");
        assert_eq!(f8(f64::NAN), i64::MAX);
        let f4 = |x: f32| img(700, x.to_bits() as i32 as i64 as u64);
        let w4: Vec<i64> = [f32::NEG_INFINITY, -3.5, -0.0, 1.25, f32::INFINITY, f32::NAN].iter().map(|&x| f4(x)).collect();
        assert!(ascending(&w4), "{w4:?}");
        assert_eq!(f4(1.25), f8(1.25), "a float4 orders as its float8");
        for x in [-7.5f64, 0.0, -0.0, 3.0, f64::NAN, f64::NEG_INFINITY] {
            assert_eq!(f8(x), gravity_kernels::host_abi::float_order(x), "the host's float order word ({x})");
        }
    }

    /// Every (value / NULL) x ASC / DESC x NULLS FIRST / LAST, one word (narrow) and a pair: the folded words order as PostgreSQL's
    /// `ORDER BY` (signed; a pair's flags signed, then its word unsigned).
    #[test]
    fn fold_places_desc_and_nulls() {
        let vals: [Option<i64>; 5] = [Some(-5), None, Some(0), Some(i32::MAX as i64), Some(i32::MIN as i64)];
        for narrow in [true, false] {
            for desc in [false, true] {
                for nulls_first in [false, true] {
                    let key = |x: Option<i64>| -> (i64, u64) {
                        let w = run(2, [x.unwrap_or(0) as u64, x.is_none() as u64], &|cx, v, null| match fold(cx.b, v, Some(null), narrow, desc, nulls_first) {
                            Folded::One(w) => vec![w, cx.b.iconst(Type::I64, 0)],
                            Folded::Pair(p) => p.to_vec(),
                        });
                        let w = w.expect("folds");
                        if narrow { (w[0] as i64, 0) } else { (w[0] as i64, w[1]) }
                    };
                    let mut got = vals.to_vec();
                    got.sort_by_key(|&x| key(x));
                    let mut want: Vec<Option<i64>> = vals.iter().copied().flatten().map(Some).collect();
                    want.sort();
                    if desc {
                        want.reverse();
                    }
                    if nulls_first { want.insert(0, None) } else { want.push(None) }
                    assert_eq!(got, want, "narrow {narrow} desc {desc} nulls_first {nulls_first}");
                }
            }
        }
    }

    /// D443's rule (main 09-25): a folded word's NULL is exactly the value's NULL and sits where the key's NULLS flag puts it,
    /// DESC reversing the values only. Pseudo-random int4 (narrow, one word) and float8 values (a pair: NaN, +-Infinity, -0)
    /// with NULLs, folded under ASC / DESC x NULLS FIRST / LAST, order as PostgreSQL's comparators do (int4cmp; float8's
    /// with NaN greatest and -0 = 0), compared as the value sequence (ties are equal values).
    #[test]
    fn folded_words_order_random_values_and_nulls_as_postgres() {
        let mut seed = 0x0123_4567_89AB_CDEFu64; // any odd LCG seed
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            seed >> 11
        };
        let special = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.0, 0.0, f64::MIN_POSITIVE, -f64::MAX];
        let ints: Vec<Option<i64>> = (0..60).map(|i| (i % 9 != 4).then(|| next() as i32 as i64)).collect();
        let floats: Vec<Option<f64>> = (0..60)
            .map(|i| (i % 7 != 3).then(|| if i % 5 == 0 { special[i % special.len()] } else { (next() as i64 as f64) / 7.0 * if i % 2 == 0 { -1.0 } else { 1.0 } }))
            .collect();
        // PostgreSQL's float8 order key: NaN greatest (all NaN equal), -0 = 0
        let pg = |x: f64| if x.is_nan() { (1u8, 0.0) } else { (0, x + 0.0) };
        for desc in [false, true] {
            for nulls_first in [false, true] {
                let place = |ord: core::cmp::Ordering| if desc { ord.reverse() } else { ord };
                let words = |d: u64, null: bool, float: bool| -> (i64, u64) {
                    let w = run(2, [d, null as u64], &|cx, v, n| {
                        let img = if float { float_order(cx.b, v, false) } else { int_order(cx.b, v, 4, true) };
                        match fold(cx.b, img, Some(n), !float, desc, nulls_first) {
                            Folded::One(w) => vec![w, cx.b.iconst(Type::I64, 0)],
                            Folded::Pair(p) => p.to_vec(),
                        }
                    });
                    let w = w.expect("folds");
                    if float { (w[0] as i64, w[1]) } else { (w[0] as i64, 0) }
                };
                let expect_nulls = |n: usize, mut vals: Vec<String>| {
                    let nulls = vec!["NULL".to_string(); n];
                    if nulls_first { [nulls, vals].concat() } else { vals.extend(nulls); vals }
                };
                // int4
                let mut got = ints.clone();
                got.sort_by_key(|x| words(x.unwrap_or(0) as u64, x.is_none(), false));
                let mut vals: Vec<i64> = ints.iter().flatten().copied().collect();
                vals.sort_by(|a, b| place(a.cmp(b)));
                let want = expect_nulls(ints.len() - vals.len(), vals.iter().map(|v| v.to_string()).collect());
                let got: Vec<String> = got.iter().map(|x| x.map_or("NULL".to_string(), |v| v.to_string())).collect();
                assert_eq!(got, want, "int4 desc {desc} nulls_first {nulls_first}");
                // float8
                let mut got = floats.clone();
                got.sort_by_key(|x| words(x.unwrap_or(0.0).to_bits(), x.is_none(), true));
                let mut vals: Vec<f64> = floats.iter().flatten().copied().collect();
                vals.sort_by(|a, b| place(pg(*a).partial_cmp(&pg(*b)).unwrap()));
                let show = |v: f64| if v.is_nan() { "NaN".to_string() } else { format!("{}", v + 0.0) };
                let want = expect_nulls(floats.len() - vals.len(), vals.iter().map(|&v| show(v)).collect());
                let got: Vec<String> = got.iter().map(|x| x.map_or("NULL".to_string(), show)).collect();
                assert_eq!(got, want, "float8 desc {desc} nulls_first {nulls_first}");
            }
        }
    }

    /// O3: a name's eight words order as its bytes (high-bit bytes after ASCII, a prefix first); a NULL reads nothing.
    #[test]
    fn name_orders_as_its_padded_bytes() {
        let names: [&[u8]; 6] = [b"", b"a", b"ab", b"abc_longer_than_eight_bytes_x", b"b", b"\xc3\xa9t\xc3\xa9"];
        let tf = *row_of(19).expect("name");
        let c = cell(Rep::FixedPtr { len: NAME_BYTES }, 19);
        let image = |p: u64, null: u64, coll: KeyColl| run(8, [p, null], &|cx, v, n| name_image(cx, CellRef { cell: &c, val: v, null: Some(n) }, &tf, coll).to_vec());
        let words: Vec<Vec<i64>> = names
            .iter()
            .map(|s| {
                let mut b = [0u8; 64];
                b[..s.len()].copy_from_slice(s);
                image(b.as_ptr() as u64, 0, C).expect("an image").iter().map(|&w| w as i64).collect()
            })
            .collect();
        assert!(words.windows(2).all(|w| w[0] < w[1]), "{words:x?}");
        // bytes a producer left past the first NUL (a word's middle, a later word) are not the name's
        let padded = |bytes: &[u8]| {
            let mut b = [0u8; 64];
            b[..bytes.len()].copy_from_slice(bytes);
            image(b.as_ptr() as u64, 0, C).expect("an image")
        };
        let mut junk = [0xA5u8; 64];
        junk[..3].copy_from_slice(b"ab\0");
        let garbage = image(junk.as_ptr() as u64, 0, C).expect("an image");
        assert_eq!(garbage, padded(b"ab"), "the bytes after the NUL are zeroed");
        assert!((garbage[0] as i64) < (padded(b"abc")[0] as i64), "a prefix before its extension, whatever follows its NUL");
        junk[..12].copy_from_slice(b"abcdefghij\0x");
        assert_eq!(image(junk.as_ptr() as u64, 0, C).expect("an image"), padded(b"abcdefghij"), "a NUL in the second word");
        assert_eq!(image(0, 1, C).expect("a NULL"), vec![0; 8], "a NULL's words, the pointer never read");
        let en_us = KeyColl { oid: 100, bytes_eq: true, bytes_order: false };
        match image(0, 1, en_us) {
            Err(crate::Refusal { reason: Reason::Shape { .. }, .. }) => {}
            other => panic!("a name under a linguistic collation refuses: {other:?}"),
        }
    }

    /// An enum is rep.hosted.order.fixed's (its enumsortorder can move under a cached program): the constant-0 inexact
    /// word, its comparator the tail (V4R-386); G4: a text key (varlena) and a name (fixed image) never reach this arm.
    #[test]
    fn enums_and_non_words_are_not_claimed() {
        let tf = gravity_decompose::catalog::typefacts::opaque(16500, 16500, 4, true, true);
        let c = cell(Rep::Word { bytes: 4 }, 16500);
        assert_eq!(rep::arm_kind(c.rep, rep::Accessor::Order, &tf), KindId::RepHostedOrderFixed);
        let w = run(1, [3, 0], &|cx, v, _| {
            let w = rep::order_words(cx, CellRef { cell: &c, val: v, null: None }, &tf, C);
            assert!(!w.exact, "an enum's word is inexact: its comparator decides");
            vec![w.w[0].unwrap()]
        });
        assert_eq!(w.expect("an enum's word"), vec![0]);
        let order = rep::Accessor::Order;
        assert_eq!(rep::arm_kind(Rep::VarlenaPtr, order, row_of(25).unwrap()), KindId::RepVarlenaOrder);
        assert_ne!(rep::arm_kind(Rep::FixedPtr { len: NAME_BYTES }, order, row_of(19).unwrap()), KindId::RepWordOrder);
    }
}

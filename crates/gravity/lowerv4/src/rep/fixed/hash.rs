//! Kind `rep.fixed.hash` (`HashArm`, cards/M6.md L3, M6-plan §3.3): the key image of a fixed-length by-reference value
//! whose equality is canonical by value (`WordImage::Fixed`), exact: equal images are equal keys under the type's `=`, so
//! a hit needs no compare past the image. Owner M6-TYPES. Port: V2 `text/keys.rs:559-596` (`interval_norm`, `timetz_words`).
//! - uuid (`FixedNorm::Raw`): its 16 bytes as two words, each half big-endian with its top bit flipped (`uuid_eq` is
//!   memcmp; the words compare signed in `uuid_cmp`'s order).
//! - interval: `interval_cmp_value`'s span (`timestamp.c:2546`: `(month * 30 + day) * USECS_PER_DAY + time`, int128) as
//!   `[days, usecs]`, the span floor-divided by a day (`usecs` in `[0, USECS_PER_DAY)`): one pair per span, so `'1 mon'`
//!   and `'30 days'` key alike (one group, any member shown: Decision 7), and the infinities sit at the span's ends. The
//!   pair compares signed, word by word, in the span's order (rep.fixed.order reads it).
//! - timetz: `timetz_cmp_internal` (`date.c`) orders by `time + zone * USECS_PER_SEC`, then by `zone`; equality is both:
//!   `[time + zone * USECS_PER_SEC, zone]` (V2 `timetz_words`).
//! A NULL's words are 0 (nothing is read); the key image's flags word marks it. `name` never reaches this arm (its image
//! is `Bytes`: rep/mod.rs `arm_kind` routes it to rep.hosted.*, and O3's name order is rep.word.order's).
use gravity_decompose::catalog::typefacts::{FixedNorm, TypeFacts};
use gravity_ir::{ConvOp, IntBinOp, IntCond, IntUnOp, Type, Value};
use gravity_program::Rep;

use crate::face::kinds::KindId;
use crate::admit::refusal::Reason;
use crate::ops::Table;
use crate::rep::{self, CellRef, HashArm, Lx, Scope, StoredRef, Words};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::RepFixedHash, &ARM)], ..crate::ops::NONE };

pub(crate) const ARM: HashArm = HashArm { image, eq };

/// `USECS_PER_DAY`.
pub(crate) const USECS_PER_DAY: i64 = 86_400_000_000;
/// `USECS_PER_SEC`.
const USECS_PER_SEC: i64 = 1_000_000;

fn image(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, _scope: Scope) -> Words {
    let w = words(cx, c, tf);
    Words { w: [Some(w[0]), Some(w[1])], exact: true }
}

/// Equality against a stored image (this arm's words): equal words are equal keys.
fn eq(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, stored: StoredRef, tf: &TypeFacts) -> Value {
    let w = words(cx, c, tf);
    let p = if cx.b.value_type(stored.ptr) == Type::Ptr { stored.ptr } else { cx.b.conv(ConvOp::IntToPtr, Type::Ptr, stored.ptr) };
    let mut d = cx.b.iconst(Type::I64, 0);
    for (i, &x) in w.iter().enumerate() {
        let s = cx.b.load(Type::I64, p, stored.off + 8 * i as i32);
        let x = cx.b.int_bin(IntBinOp::Xor, x, s);
        d = cx.b.int_bin(IntBinOp::Or, d, x);
    }
    let zero = cx.b.iconst(Type::I64, 0);
    cx.b.icmp(IntCond::Eq, d, zero)
}

/// The two canonical words of a fixed cell (the type's `words`), 0 for a NULL: nothing is read through a NULL's pointer.
/// uuid's words are its halves big-endian, top bit flipped: signed word order is memcmp's.
pub(crate) fn words(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts) -> [Value; 2] {
    let (norm, len) = match (tf.image.fixed_norm(), c.cell.rep) {
        (Some(norm), Rep::FixedPtr { len }) => (norm, len),
        _ => crate::refuse!(cx, Reason::Bug { payload: "a fixed key image over a cell that is not a fixed image" }),
    };
    let want = match norm {
        FixedNorm::Raw | FixedNorm::Interval => 16,
        FixedNorm::Timetz => 12,
    };
    if len != want {
        crate::refuse!(cx, Reason::Bug { payload: "a fixed key image whose length is not its type's" });
    }
    let (d, null) = rep::datum(cx, c, tf);
    let (l_img, l_out) = (cx.b.create_block(&[]), cx.b.create_block(&[Type::I64; 2]));
    let zero = cx.b.iconst(Type::I64, 0);
    cx.b.cond_br(null, (l_out, &[zero; 2]), (l_img, &[]));
    cx.b.switch_to(l_img);
    let p = cx.b.conv(ConvOp::IntToPtr, Type::Ptr, d);
    let b = &mut *cx.b;
    let w = match norm {
        FixedNorm::Raw => {
            // each half big-endian, its top bit flipped: equal bytes are equal words, and the words compare signed as the
            // bytes do (uuid_cmp is memcmp), so the group sort and rep.fixed.order read the image as it is
            let top = b.const_raw(Type::I64, 1 << 63);
            [0, 8].map(|o| {
                let x = b.load_unaligned(Type::I64, p, o);
                let x = b.int_un(IntUnOp::Bswap, x);
                b.int_bin(IntBinOp::Xor, x, top)
            })
        }
        FixedNorm::Interval => {
            let time = b.load_unaligned(Type::I64, p, 0);
            let day = b.load_unaligned(Type::I32, p, 8);
            let month = b.load_unaligned(Type::I32, p, 12);
            let (day, month) = (b.conv(ConvOp::Sext, Type::I64, day), b.conv(ConvOp::Sext, Type::I64, month));
            let thirty = b.iconst(Type::I64, 30);
            let days = b.int_bin(IntBinOp::Mul, month, thirty);
            let days = b.int_bin(IntBinOp::Add, days, day);
            // floor division of time by a day: the remainder in [0, USECS_PER_DAY), its quotient folded into days
            let (du, one) = (b.iconst(Type::I64, USECS_PER_DAY), b.iconst(Type::I64, 1));
            let (q, r) = (b.int_bin(IntBinOp::Sdiv, time, du), b.int_bin(IntBinOp::Srem, time, du));
            let neg = b.icmp(IntCond::Slt, r, zero);
            let (r1, q1) = (b.int_bin(IntBinOp::Add, r, du), b.int_bin(IntBinOp::Sub, q, one));
            let (r, q) = (b.select(neg, r1, r), b.select(neg, q1, q));
            [b.int_bin(IntBinOp::Add, days, q), r]
        }
        FixedNorm::Timetz => {
            let time = b.load_unaligned(Type::I64, p, 0);
            let zone = b.load_unaligned(Type::I32, p, 8);
            let zone = b.conv(ConvOp::Sext, Type::I64, zone);
            let us = b.iconst(Type::I64, USECS_PER_SEC);
            let t = b.int_bin(IntBinOp::Mul, zone, us);
            [b.int_bin(IntBinOp::Add, time, t), zone]
        }
    };
    cx.b.br(l_out, &w);
    cx.b.switch_to(l_out);
    [cx.b.block_params(l_out)[0], cx.b.block_params(l_out)[1]]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::cell::{At, Cell, CellHome, CellLife, Nullable};
    use gravity_decompose::catalog::typefacts::row_of;
    use gravity_interp::Interp;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_program::{NumTypmod, Until};

    pub const UUID: u32 = 2950;
    pub use types_core::catalog::{INTERVALOID as INTERVAL, TIMETZOID as TIMETZ};

    /// `f`'s `n` words over a computed cell of `typid` whose datum points at `bytes` (`None`: a NULL, datum 0).
    pub fn run(typid: u32, bytes: Option<&[u8]>, n: usize, f: &dyn Fn(&mut Lx<'_, '_, '_>, CellRef<'_>, &TypeFacts) -> Vec<Value>) -> Result<Vec<u64>, crate::Refusal> {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let tf = *row_of(typid).expect("type facts");
        crate::catch(9, || {
            let c = Cell { rep: Rep::FixedPtr { len: tf.typlen as u16 }, typid, num: NumTypmod::NONE, life: CellLife { until: Until::Row, home: CellHome::RowCx }, at: At::Value(0), null: Nullable::Maybe };
            let mut pb = ProgramBuilder::new();
            let fid = pb.declare_func("w", &[Type::I64, Type::I64], &vec![Type::I64; n]);
            let mut b = pb.define(fid);
            let (v, nw) = (b.params()[0], b.params()[1]);
            let out = {
                let zero = b.iconst(Type::I64, 0);
                let null = b.icmp(IntCond::Ne, nw, zero);
                let mut cx = Lx::new(&mut b, 9, rig.mcx());
                f(&mut cx, CellRef { cell: &c, val: v, null: Some(null) }, &tf)
            };
            b.ret(&out);
            let prog = pb.finish();
            let d = bytes.map_or(0, |x| x.as_ptr() as u64);
            Interp::new(&prog).run(fid, &[d, bytes.is_none() as u64]).expect("runs")
        })
    }

    pub fn img(typid: u32, bytes: Option<&[u8]>) -> Vec<u64> {
        run(typid, bytes, 2, &|cx, c, tf| {
            let w = image(cx, c, tf, Scope::Program);
            assert!(w.exact);
            w.w.into_iter().flatten().collect()
        })
        .expect("an image")
    }

    /// An interval's 16 bytes: time, day, month (PostgreSQL's `Interval`).
    pub fn iv(month: i32, day: i32, time: i64) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&time.to_le_bytes());
        b[8..12].copy_from_slice(&day.to_le_bytes());
        b[12..].copy_from_slice(&month.to_le_bytes());
        b
    }

    /// A timetz's 12 bytes: time, zone (seconds west of UTC).
    pub fn tz(time: i64, zone: i32) -> [u8; 12] {
        let mut b = [0u8; 12];
        b[..8].copy_from_slice(&time.to_le_bytes());
        b[8..].copy_from_slice(&zone.to_le_bytes());
        b
    }

    /// PostgreSQL's `interval_cmp_value`.
    pub fn span(x: &[u8; 16]) -> i128 {
        let time = i64::from_le_bytes(x[..8].try_into().unwrap()) as i128;
        let day = i32::from_le_bytes(x[8..12].try_into().unwrap()) as i128;
        let month = i32::from_le_bytes(x[12..].try_into().unwrap()) as i128;
        time + (month * 30 + day) * USECS_PER_DAY as i128
    }

    /// Decision 7: `'1 mon'` = `'30 days'` = `'720 hours'`, one image; a span that differs by 1 us does not match.
    #[test]
    fn equal_spans_key_alike() {
        let h = USECS_PER_DAY / 24;
        let a = img(INTERVAL, Some(&iv(1, 0, 0)));
        assert_eq!(a, img(INTERVAL, Some(&iv(0, 30, 0))));
        assert_eq!(a, img(INTERVAL, Some(&iv(0, 0, 720 * h))));
        assert_eq!(a, img(INTERVAL, Some(&iv(2, -30, 0))), "a negative field");
        assert_ne!(a, img(INTERVAL, Some(&iv(0, 30, 1))));
        assert_eq!(img(INTERVAL, Some(&iv(0, 0, -1))), vec![-1i64 as u64, (USECS_PER_DAY - 1) as u64], "floor, not truncation");
        assert_eq!(img(INTERVAL, Some(&iv(0, -1, USECS_PER_DAY - 1))), img(INTERVAL, Some(&iv(0, 0, -1))));
    }

    /// Random intervals (the infinities too): equal images exactly when the spans are equal.
    #[test]
    fn images_are_the_span() {
        let mut s = 88_172_645_463_325_252u64;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let mut xs: Vec<[u8; 16]> = vec![iv(i32::MAX, i32::MAX, i64::MAX), iv(i32::MIN, i32::MIN, i64::MIN), iv(0, 0, 0)];
        for _ in 0..300 {
            let (a, b, c) = (next(), next(), next());
            xs.push(iv((a % 7) as i32 - 3, (b % 200) as i32 - 100, (c % (4 * USECS_PER_DAY as u64)) as i64 - 2 * USECS_PER_DAY));
        }
        let imgs: Vec<Vec<u64>> = xs.iter().map(|x| img(INTERVAL, Some(x))).collect();
        for i in 0..xs.len() {
            for j in 0..xs.len() {
                assert_eq!(imgs[i] == imgs[j], span(&xs[i]) == span(&xs[j]), "{i} {j}");
            }
        }
    }

    #[test]
    fn uuid_and_timetz_and_null() {
        let u: Vec<u8> = (1..=16).collect();
        assert_eq!(img(UUID, Some(&u)), vec![u64::from_be_bytes(u[..8].try_into().unwrap()) ^ 1 << 63, u64::from_be_bytes(u[8..].try_into().unwrap()) ^ 1 << 63]);
        assert_eq!(img(UUID, None), vec![0, 0], "a NULL reads nothing");
        let t = img(TIMETZ, Some(&tz(3_600_000_000, -3600)));
        assert_eq!(t, vec![0, -3600i64 as u64], "the instant (UTC midnight), the zone");
        assert_ne!(t, img(TIMETZ, Some(&tz(7_200_000_000, -7200))), "one instant, two zones: not equal (timetz_eq)");
        assert_eq!(t, img(TIMETZ, Some(&tz(3_600_000_000, -3600))));
        assert_eq!(img(TIMETZ, None), vec![0, 0]);
    }

    /// uuid (M6-TYPES R1b): equal bytes give equal words (uuid_eq), and the two words compared signed, word by word, order
    /// as `uuid_cmp` (memcmp over 16 bytes) does: the top-bit bytes of either half, and ties in the first half.
    #[test]
    fn uuid_words_are_uuid_eq_and_uuid_cmp() {
        let mut us: Vec<[u8; 16]> = vec![[0; 16], [0xFF; 16], [0x80; 16], [0x7F; 16]];
        for (h, b) in [(0usize, 0x80u8), (0, 0x7F), (0, 0x00), (0, 0xFF), (8, 0x80), (8, 0x7F), (8, 0x01), (15, 0x80), (15, 0x7F), (7, 0x80)] {
            let mut u = [0x55u8; 16];
            u[h] = b;
            us.push(u);
            let mut v = u;
            v[15 - h % 8] ^= 0x80;
            us.push(v);
        }
        let w: Vec<[i64; 2]> = us.iter().map(|u| {
            let x = img(UUID, Some(u));
            [x[0] as i64, x[1] as i64]
        }).collect();
        for i in 0..us.len() {
            assert_eq!(w[i], { let x = img(UUID, Some(&{ us[i] })); [x[0] as i64, x[1] as i64] }, "{i}: equal uuids, equal words");
            for j in 0..us.len() {
                assert_eq!(w[i].cmp(&w[j]), us[i].cmp(&us[j]), "{i} {j}: uuid_cmp is memcmp");
                assert_eq!(w[i] == w[j], us[i] == us[j], "{i} {j}: uuid_eq");
            }
        }
    }

    #[test]
    fn eq_compares_the_stored_words() {
        let stored = img(INTERVAL, Some(&iv(1, 0, 0)));
        let st: [u64; 2] = [stored[0], stored[1]];
        for (x, want) in [(iv(0, 30, 0), 1), (iv(0, 30, 1), 0)] {
            let r = run(INTERVAL, Some(&x), 1, &|cx, c, tf| {
                let p = cx.b.const_raw(Type::I64, st.as_ptr() as u64);
                let e = eq(cx, c, StoredRef { rep: c.cell.rep, ptr: p, off: 0 }, tf);
                vec![cx.b.conv(ConvOp::Zext, Type::I64, e)]
            });
            assert_eq!(r.expect("eq")[0], want);
        }
    }
}

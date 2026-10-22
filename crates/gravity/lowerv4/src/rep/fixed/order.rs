//! Kind `rep.fixed.order` (`OrderArm`, cards/M6.md L3, M6-plan §3.3): the order image of a fixed-length by-reference key
//! (`WordImage::Fixed`), signed-comparable in PostgreSQL's btree order for the type; DESC and NULLS fold in the caller
//! (rep/word/order.rs `fold`). Owner M6-TYPES.
//! - timetz: one word, exact (`timetz_cmp_internal`: `time + zone * USECS_PER_SEC`, then `zone`): rep.fixed.hash's
//!   instant (|t| < 2^38: a time within 24 h, a zone within 16 h) shifted over the zone biased by 2^20, signed.
//! - uuid (`uuid_cmp`, memcmp): its first 8 bytes big-endian (top bit flipped: the signed convention) as the prefix word,
//!   inexact; the run compares its 16-byte image as bytes on a tie (runs/sort.rs makes a uuid column a key tail, as a
//!   `name`'s).
//! - interval: its `[days, usecs]` pair (rep.fixed.hash's) orders as the span (`interval_cmp_value`); a run key is one
//!   `[flags, word]` pair, so the prefix word is `days` over the time of day in 2^11 us steps, inexact, and the run
//!   compares the pair, stored big-endian with the sign bits flipped after the column's image, as bytes on a tie
//!   (runs/sort.rs `ivl_tail`).
use gravity_decompose::catalog::typefacts::{FixedNorm, KeyColl, TypeFacts};
use gravity_ir::{IntBinOp, Type};

use crate::face::kinds::KindId;
use crate::admit::refusal::Variant;
use crate::ops::Table;
use crate::rep::{CellRef, Lx, OrderArm, Words};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::RepFixedOrder, &ARM)], ..crate::ops::NONE };

pub(crate) const ARM: OrderArm = OrderArm { image, name: None };

/// The timetz word's zone field: the zone (seconds, |zone| < 2^20) biased to be non-negative, below the shifted instant.
const ZONE_BITS: i64 = 21;
/// The interval word's time-of-day field: the 64-bit word less days' 38 signed bits.
const IVL_TIME_BITS: i64 = 26;

fn image(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, _coll: KeyColl) -> Words {
    let Some(norm) = tf.image.fixed_norm() else { crate::not_built!(cx, Variant::Kind(KindId::RepFixedOrder)) };
    let [t, zone] = crate::rep::fixed::hash::words(cx, c, tf);
    if norm == FixedNorm::Interval {
        // days over the time of day in 2^11 us steps: |days| < 2^37 (months x 30, days and the time's whole days, the
        // infinities too) and the time of day (floor-normalized, non-negative) < 2^37 us, so the sum never overflows and
        // orders as the span; equal spans are one word, and the run's tail decides a tie
        let (sh, step) = (cx.b.iconst(Type::I64, IVL_TIME_BITS), cx.b.iconst(Type::I64, 37 - IVL_TIME_BITS));
        let hi = cx.b.int_bin(IntBinOp::Shl, t, sh);
        let lo = cx.b.int_bin(IntBinOp::Ushr, zone, step);
        return Words { w: [Some(cx.b.int_bin(IntBinOp::Add, hi, lo)), None], exact: false };
    }
    if norm == FixedNorm::Raw {
        return Words { w: [Some(t), None], exact: false }; // the image's first word (big-endian, top bit flipped): the tail decides a tie
    }
    let (sh, bias) = (cx.b.iconst(Type::I64, ZONE_BITS), cx.b.iconst(Type::I64, 1 << (ZONE_BITS - 1)));
    let hi = cx.b.int_bin(IntBinOp::Shl, t, sh);
    let lo = cx.b.int_bin(IntBinOp::Add, zone, bias);
    Words { w: [Some(cx.b.int_bin(IntBinOp::Or, hi, lo)), None], exact: true }
}

#[cfg(test)]
mod tests {
    use crate::rep::fixed::hash::tests::{img, iv, run, span, tz, INTERVAL, TIMETZ, UUID};
    use super::*;

    const C: KeyColl = KeyColl { oid: 0, bytes_eq: true, bytes_order: true };

    fn ord(typid: u32, bytes: Option<&[u8]>) -> Result<Vec<u64>, crate::Refusal> {
        run(typid, bytes, 1, &|cx, c, tf| {
            let w = image(cx, c, tf, C);
            assert!(w.exact == (typid == TIMETZ) && w.w[1].is_none());
            vec![w.w[0].unwrap()]
        })
    }

    /// PostgreSQL's `timetz_cmp_internal`.
    fn pg(a: &[u8; 12], b: &[u8; 12]) -> core::cmp::Ordering {
        let f = |x: &[u8; 12]| {
            let (t, z) = (i64::from_le_bytes(x[..8].try_into().unwrap()), i32::from_le_bytes(x[8..].try_into().unwrap()));
            (t + z as i64 * 1_000_000, z)
        };
        f(a).cmp(&f(b))
    }

    /// timetz words order as PostgreSQL over times of a day and zones of +-15:59, the day's ends included.
    #[test]
    fn timetz_orders_by_instant_then_zone() {
        let h = 3_600_000_000i64;
        let mut xs = vec![tz(0, 0), tz(24 * h, 0), tz(0, 57_540), tz(24 * h, -57_540), tz(12 * h, 3600), tz(11 * h, 0)];
        let mut s = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..200 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            xs.push(tz((s % (24 * h as u64 + 1)) as i64, ((s >> 40) % 115_081) as i32 - 57_540));
        }
        let w: Vec<i64> = xs.iter().map(|x| ord(TIMETZ, Some(x)).expect("a word")[0] as i64).collect();
        for i in 0..xs.len() {
            for j in 0..xs.len() {
                assert_eq!(w[i].cmp(&w[j]), pg(&xs[i], &xs[j]), "{i} {j}");
            }
        }
    }

    /// A uuid's prefix word orders as its first 8 bytes (memcmp), signed; a NULL reads nothing.
    #[test]
    fn uuid_prefix_orders_as_its_bytes() {
        let us: Vec<[u8; 16]> = [[0u8; 16], [0xFF; 16], [0x80; 16], [0x7F; 16], [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0], [1, 2, 3, 4, 5, 6, 7, 9, 0, 0, 0, 0, 0, 0, 0, 0]].to_vec();
        let w: Vec<i64> = us.iter().map(|u| ord(UUID, Some(u)).expect("a word")[0] as i64).collect();
        for i in 0..us.len() {
            for j in 0..us.len() {
                assert_eq!(w[i].cmp(&w[j]), us[i][..8].cmp(&us[j][..8]), "{i} {j}");
            }
        }
        assert_eq!(ord(UUID, None).expect("a word"), vec![0], "a NULL reads nothing (its flags word orders it)");
    }

    /// An interval's prefix word (days) then its run tail (rep.fixed.hash's pair, big-endian, sign bits flipped) order as
    /// `interval_cmp_value`'s span: equal spans ('1 mon' = '30 days' = '720 hours') are equal keys, the infinities at the ends.
    #[test]
    fn interval_prefix_then_tail_orders_as_the_span() {
        let d = 86_400_000_000i64;
        let mut xs = vec![iv(1, 0, 0), iv(0, 30, 0), iv(0, 0, 720 * 3_600_000_000), iv(0, 1, -d), iv(0, 0, -1), iv(0, -1, 0), iv(0, 0, 1)];
        xs.extend([iv(i32::MAX, i32::MAX, i64::MAX), iv(i32::MIN, i32::MIN, i64::MIN), iv(-5, 3, d / 2), iv(0, 0, 0), iv(0, 0, -d - 1)]);
        let mut s = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..150 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            xs.push(iv((s % 4001) as i32 - 2000, ((s >> 12) % 60_001) as i32 - 30_000, (s >> 20) as i64 % (40 * d) - 20 * d));
        }
        let key = |x: &[u8; 16]| {
            let p = ord(INTERVAL, Some(x)).expect("a word")[0] as i64;
            let tail: Vec<u8> = img(INTERVAL, Some(x)).iter().flat_map(|w| (w ^ 1 << 63).to_be_bytes()).collect();
            (p, tail)
        };
        let k: Vec<_> = xs.iter().map(key).collect();
        for i in 0..xs.len() {
            for j in 0..xs.len() {
                assert_eq!(k[i].cmp(&k[j]), span(&xs[i]).cmp(&span(&xs[j])), "{i} {j}");
            }
        }
        // the prefix word alone parts spans within one day (a bounded incremental sort stops on it), down to 2^11 us
        let p = |x: [u8; 16]| ord(INTERVAL, Some(&x)).expect("a word")[0] as i64;
        assert!(p(iv(0, 0, -1)) < p(iv(0, 0, 0)) && p(iv(0, 0, 0)) < p(iv(0, 0, 2048)) && p(iv(0, 0, 2048)) < p(iv(0, 0, 3_600_000_000)));
        assert!(p(iv(0, 0, d - 1)) < p(iv(0, 1, 0)) && p(iv(0, 1, -2048)) == p(iv(0, 0, d - 2048)));
    }
}

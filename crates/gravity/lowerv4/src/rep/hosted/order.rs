//! Kind `rep.hosted.order` (`OrderArm`: types with no native image, the fmgr comparator; L3) (cards/M6.md,
//! M6-changes.md §1). Owner M6-TYPES.
//! The image is one word, the constant 0, inexact: every tie of the words (every pair of values) is the run's tail's to
//! decide, and the tail of a hosted key is its var copy (runs/sort.rs: the detoasted varlena behind a 4-byte header, a
//! Datum as PostgreSQL passes it) compared by the key's btree support function 1 (`BTORDER_PROC` of the sort operator's
//! opfamily) under the key's collation, through fmgr (kernels host_abi.rs `RowCmp`, `KFLAG_HOSTED`). A numeric that no
//! packed lane holds, an inet / cidr, an array, a range, a record or a user type orders this way. A caller with no tail
//! for the key refuses before it reads the image (keylane.rs `pairs` takes an inexact word only with a tail).
//! Kind `rep.hosted.order.fixed` (M6-FIXED, V4R-386): a by-value or fixed by-reference key with no native image. A tid,
//! a macaddr and a macaddr8 order by one EXACT signed-comparable word (`bttidcmp`: block then offset, both unsigned;
//! `macaddr_cmp` / `macaddr8_cmp`: the bytes as an unsigned big-endian number), so no comparator runs. An enum orders by
//! `enum_cmp` (`enumsortorder`, which `ALTER TYPE .. ADD VALUE` moves with no invalidation a cached program sees): the
//! constant-0 inexact word, its tail the datum word itself (runs/keylane.rs `WORD_WIDTH`) called through fmgr on the
//! leader (`resolved::pool_safe`). Any other fixed type refuses by name.
use gravity_decompose::catalog::typefacts::{KeyColl, TypeFacts};
use gravity_ir::{ConvOp, IntBinOp, IntUnOp, Type, Value};
use gravity_program::Rep;

use crate::face::kinds::KindId;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;
use crate::rep::{CellRef, Lx, OrderArm, Words};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::RepHostedOrder, &ARM), (KindId::RepHostedOrderFixed, &FIXED)], ..crate::ops::NONE };

pub(crate) const ARM: OrderArm = OrderArm { image, name: None };
const FIXED: OrderArm = OrderArm { image: fixed, name: None };

/// `TIDOID` (by catalog name), `MACADDROID`, `MACADDR8OID`.
use types_core::catalog::TIDOID as TID;
const MACADDR: u32 = 829;
const MACADDR8: u32 = 774;

/// The image of a key `rep.hosted.order.fixed` orders (above). A NULL's word is 0 (nothing is read): `fold` places it.
/// D2474: exact, so rep.hosted.hash keys a macaddr / macaddr8 by this one word too (equal words are equal values).
pub(crate) fn fixed(lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, coll: KeyColl) -> Words {
    let len = match (c.cell.rep, tf.base) {
        (Rep::Word { .. }, _) if tf.image.is_enum() => return image(lx, c, tf, coll),
        (Rep::FixedPtr { len: 6 }, TID | MACADDR) => 6,
        (Rep::FixedPtr { len: 8 }, MACADDR8) => 8,
        _ => crate::refuse!(lx, Reason::Shape { why: ShapeWhy("a fixed-width sort key with no order image (a comparator over its image is not built)") }),
    };
    let (p, null) = crate::rep::datum(lx, c, tf);
    let b = &mut *lx.b;
    let (l_img, l_out) = (b.create_block(&[]), b.create_block(&[Type::I64]));
    let zero = b.iconst(Type::I64, 0);
    b.cond_br(null, (l_out, &[zero]), (l_img, &[]));
    b.switch_to(l_img);
    let ptr = b.conv(ConvOp::IntToPtr, Type::Ptr, p);
    let w = if tf.base == TID {
        // ItemPointerData: bi_hi, bi_lo, ip_posid, native u16s: (block << 16 | offset) < 2^48, non-negative
        let (hi, lo, pos) = (b.load_zext(Type::I16, ptr, 0), b.load_zext(Type::I16, ptr, 2), b.load_zext(Type::I16, ptr, 4));
        let (s32, s16) = (b.iconst(Type::I64, 32), b.iconst(Type::I64, 16));
        let (hi, lo) = (b.int_bin(IntBinOp::Shl, hi, s32), b.int_bin(IntBinOp::Shl, lo, s16));
        let w = b.int_bin(IntBinOp::Or, hi, lo);
        b.int_bin(IntBinOp::Or, w, pos)
    } else {
        // the bytes big-endian: 48 bits stay non-negative; a macaddr8's 64 flip the top bit (the signed convention)
        let mut w = zero;
        for i in 0..len as i32 {
            let (x, s8) = (b.load_zext(Type::I8, ptr, i), b.iconst(Type::I64, 8));
            let sh = b.int_bin(IntBinOp::Shl, w, s8);
            w = b.int_bin(IntBinOp::Or, sh, x);
        }
        if len == 8 {
            let top = b.const_raw(Type::I64, 1 << 63);
            w = b.int_bin(IntBinOp::Xor, w, top);
        }
        w
    };
    b.br(l_out, &[w]);
    b.switch_to(l_out);
    Words { w: [Some(b.block_params(l_out)[0]), None], exact: true }
}

/// D2474: the byte length of a macaddr / macaddr8 key (a domain's by its base) that a group record keeps as one word,
/// `fixed`'s exact order word: rep.hosted.hash keys it by that word, a grouped top orders by it, and the group emit renders
/// the bytes back from it (`bytes_of_word`, table/emit_key.rs `KeyOut::Bytes`). `None` for any other type (a tid keeps its
/// refusal, D1379).
pub(crate) const fn word_key_len(tf: &TypeFacts) -> Option<u16> {
    match (tf.base, tf.typlen, tf.byval, tf.words) {
        (MACADDR, 6, false, 1) => Some(6),
        (MACADDR8, 8, false, 1) => Some(8),
        _ => None,
    }
}

/// D2474: `fixed`'s inverse for a `len`-byte macaddr / macaddr8: the value's bytes from its order word `w` (macaddr8's top
/// bit flipped back, then big-endian to memory order), written to a frame slot valid for the row; the slot's address.
pub(crate) fn bytes_of_word(b: &mut gravity_ir::builder::FunctionBuilder<'_>, w: Value, len: u16) -> Value {
    let w = match len {
        8 => {
            let top = b.const_raw(Type::I64, 1 << 63);
            b.int_bin(IntBinOp::Xor, w, top)
        }
        _ => {
            // the 6 bytes to the word's top, so the swap puts them first in memory
            let s = b.iconst(Type::I64, 8 * (8 - len as i64));
            b.int_bin(IntBinOp::Shl, w, s)
        }
    };
    let w = if cfg!(target_endian = "little") { b.int_un(IntUnOp::Bswap, w) } else { w };
    let slot = b.declare_stack_slot(8, 8);
    let out = b.stack_addr(slot);
    b.store(out, 0, w);
    b.conv(ConvOp::PtrToInt, Type::I64, out)
}

fn image(lx: &mut Lx<'_, '_, '_>, _c: CellRef<'_>, _tf: &TypeFacts, _coll: KeyColl) -> Words {
    let w = lx.b.iconst(Type::I64, 0);
    Words { w: [Some(w), None], exact: false }
}

#[cfg(test)]
mod tests {
    use crate::rep::fixed::hash::tests::run;
    use super::*;
    use core::cmp::Ordering;

    const C: KeyColl = KeyColl { oid: 0, bytes_eq: true, bytes_order: true };
    /// `bttidcmp`, `macaddr_cmp`, `macaddr8_cmp` (pg_proc.dat).
    const CMP: [(u32, u32); 3] = [(TID, 2794), (MACADDR, 836), (MACADDR8, 4119)];

    fn word(typid: u32, v: &[u8]) -> i64 {
        run(typid, Some(v), 1, &|cx, c, tf| {
            let w = fixed(cx, c, tf, C);
            assert!(w.exact && w.w[1].is_none());
            vec![w.w[0].unwrap()]
        })
        .expect("a word")[0] as i64
    }

    /// PostgreSQL's own comparator over the two images, through fmgr (a builtin resolves with no catalog).
    fn pg(typid: u32, a: &[u8], b: &[u8]) -> Ordering {
        let proc = CMP.iter().find(|x| x.0 == typid).unwrap().1;
        let id = gravity_fn::resolved::resolve(proc).expect("a builtin resolves").expect("not refused");
        let mut f = gravity_fn::FmgrCall::bind_resolved(id, 2, 0).expect("binds");
        let args = [gravity_fn::Datum::from_usize(a.as_ptr() as usize), gravity_fn::Datum::from_usize(b.as_ptr() as usize)];
        f.call_site(&args, &[false, false]).expect("compares").value.as_i32().cmp(&0)
    }

    fn tid(block: u32, pos: u16) -> Vec<u8> {
        [((block >> 16) as u16).to_ne_bytes(), (block as u16).to_ne_bytes(), pos.to_ne_bytes()].concat()
    }

    /// Every pair's words compare (signed, the arms' convention) as PostgreSQL's comparator does, top bits set included:
    /// ff.. vs 00.., 80.. vs 7f.., a block at and past 2^31, an offset past 2^15.
    #[test]
    fn exact_words_order_as_the_comparators() {
        let tids: Vec<Vec<u8>> = [(0, 0), (0, 1), (1, 0), (0, 0x8000), (0, 0xffff), (65535, 7), (65536, 1), (1 << 31, 0), ((1 << 31) - 1, 0xffff), (u32::MAX, 0xffff), (u32::MAX, 1)].iter().map(|&(b, p)| tid(b, p)).collect();
        let macs = |n: usize| -> Vec<Vec<u8>> {
            let mut s = 0x2545_F491_4F6C_DD1Du64;
            let mut v: Vec<Vec<u8>> = [0x00u8, 0xff, 0x80, 0x7f, 0x01].iter().flat_map(|&h| [vec![h; n], [vec![h], vec![0; n - 1]].concat(), [vec![h], vec![0xff; n - 1]].concat()]).collect();
            for _ in 0..40 {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                v.push(s.to_be_bytes()[..n].to_vec());
            }
            v
        };
        for (typid, vals) in [(TID, tids), (MACADDR, macs(6)), (MACADDR8, macs(8))] {
            for a in vals.iter() {
                for b in vals.iter() {
                    assert_eq!(word(typid, a).cmp(&word(typid, b)), pg(typid, a, b), "type {typid}: {a:02x?} vs {b:02x?}");
                }
            }
        }
    }

    /// D2474: the group emit's render of a macaddr / macaddr8 key from its order word is the value's bytes (top bits set,
    /// all zero and all ones included); a tid has no word key.
    #[test]
    fn bytes_of_word_inverts_the_order_word() {
        let vals: [(u32, &[u8]); 7] = [
            (MACADDR, &[0x08, 0x00, 0x2b, 0x01, 0x02, 0x03]),
            (MACADDR, &[0xff; 6]),
            (MACADDR, &[0x00; 6]),
            (MACADDR8, &[0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01]),
            (MACADDR8, &[0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe]),
            (MACADDR8, &[0xff; 8]),
            (MACADDR8, &[0x08, 0x00, 0x2b, 0xff, 0xfe, 0x01, 0x02, 0x03]),
        ];
        for (typid, v) in vals {
            let got = run(typid, Some(v), 1, &|cx, c, tf| {
                let w = fixed(cx, c, tf, C).w[0].unwrap();
                let p = bytes_of_word(&mut *cx.b, w, word_key_len(tf).expect("a word key"));
                let p = cx.b.conv(ConvOp::IntToPtr, Type::Ptr, p);
                vec![cx.b.load(Type::I64, p, 0)]
            })
            .expect("a render")[0];
            let mut want = [0u8; 8];
            want[..v.len()].copy_from_slice(v);
            assert_eq!(got.to_ne_bytes(), want, "type {typid}: {v:02x?}");
        }
        let tid = gravity_decompose::catalog::typefacts::row_of(TID).expect("tid facts");
        assert_eq!(word_key_len(tid), None);
    }

    /// An enum is the constant-0 inexact word (its comparator is the run's tail); a NULL's word is 0, the image unread.
    #[test]
    fn an_enum_is_inexact_and_a_null_unread() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        assert_eq!(run(MACADDR, None, 1, &|cx, c, tf| vec![fixed(cx, c, tf, C).w[0].unwrap()]).expect("a NULL"), vec![0]);
        let tf = gravity_decompose::catalog::typefacts::opaque(16500, 16500, 4, true, true);
        assert_eq!(crate::rep::arm_kind(Rep::Word { bytes: 4 }, crate::rep::Accessor::Order, &tf), KindId::RepHostedOrderFixed);
        let c = crate::cell::Cell { rep: Rep::Word { bytes: 4 }, typid: 16500, num: gravity_program::NumTypmod::NONE, life: crate::cell::CellLife { until: gravity_program::Until::Row, home: crate::cell::CellHome::RowCx }, at: crate::cell::At::Value(0), null: crate::cell::Nullable::Maybe };
        let w = crate::catch(9, || {
            let mut pb = gravity_ir::builder::ProgramBuilder::new();
            let fid = pb.declare_func("e", &[Type::I64], &[Type::I64]);
            let mut b = pb.define(fid);
            let v = b.params()[0];
            let out = {
                let mut cx = Lx::new(&mut b, 9, rig.mcx());
                let w = fixed(&mut cx, CellRef { cell: &c, val: v, null: None }, &tf, C);
                assert!(!w.exact && w.w[1].is_none(), "an enum's word is inexact: its comparator decides");
                w.w[0].unwrap()
            };
            b.ret(&[out]);
            let prog = pb.finish();
            gravity_interp::Interp::new(&prog).run(fid, &[16503]).expect("runs")
        });
        assert_eq!(w.expect("an enum's word"), vec![0], "the constant word, whatever the oid");
    }
}

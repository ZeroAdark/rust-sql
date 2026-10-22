//! Kind `rep.varlena.order` (`OrderArm`: text / varchar / bpchar / bytea order images; L3) (M6-plan §3.1, cards/M6.md).
//! Owner M6-TEXT.
//! The image is one prefix word, inexact: a tie of the words is not a tie of the values, and the run's tail (the key's var
//! copy, V4R-267: RowCmp over the payload, the key's collation, `KFLAG_BPCHAR`) decides it. The prefix is key.coll's
//! (face/varkey.rs; the roads asked in order): under a collation that orders by bytes (C / POSIX) the first 8 payload bytes,
//! big-endian, zero-padded, top bit flipped (rep.code.order's word, so a heap key and a code's render compare in one run),
//! a bpchar trimmed first (`bpcharcmp`); under any other collation 0, so every compare is the comparator's (`varstr_cmp`
//! with PG's memcmp tie-break; Decision 5: never a libc strxfrm, the ICU sort-key prefix is key.coll's engaged form).
//! A numeric, inet or cidr key does not order by its bytes (`rep::bytes_order`, the one rule props names its kind by):
//! rep.hosted.order's image, the comparator's (M6-REPS HO).
use gravity_decompose::catalog::typefacts::{KeyColl, TypeFacts};
use gravity_ir::{HostFlags, Type, Value};

use crate::face::kinds::KindId;
use crate::ops::Table;
use crate::rep::{self, CellRef, Lx, OrderArm, Words};

use crate::rep::varlena::hash::{payload, ptr, release};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::RepVarlenaOrder, &ARM)], ..crate::ops::NONE };

pub(crate) const ARM: OrderArm = OrderArm { image, name: None };

fn image(lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, coll: KeyColl) -> Words {
    if !rep::bytes_order(tf) {
        // a numeric, inet or cidr: the hosted comparator's image (its tail decides every tie)
        return (rep::arm::<OrderArm>(&*lx, KindId::RepHostedOrder).image)(lx, c, tf, coll);
    }
    let w = crate::rep::varkey::order_prefix(lx, c, tf, coll, bytes_prefix);
    Words { w: [Some(w), None], exact: false }
}

/// The bytes-order prefix word of the cell's value (0 on a NULL row; the caller's fold places NULLs).
pub(crate) fn bytes_prefix(lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts) -> Value {
    let (p, null) = rep::datum(lx, c, tf);
    let zero = lx.b.iconst(Type::I64, 0);
    let (l_val, l_out) = (lx.b.create_block(&[]), lx.b.create_block(&[Type::I64]));
    lx.b.cond_br(null, (l_out, &[zero]), (l_val, &[]));
    lx.b.switch_to(l_val);
    let p = ptr(lx.b, p);
    let (bytes, len, mark) = payload(lx, p, tf);
    let flags = HostFlags { touches_memory: true, pure: true, ..HostFlags::default() };
    let h = lx.b.declare_host_once("m6_text_prefix", &[Type::Ptr, Type::I64], Some(Type::I64), gravity_abi::host_words!(prefix8(p, len)), flags);
    let w = lx.b.call_host(h, &[bytes, len], Some(Type::I64)).expect("returns the prefix word");
    release(lx, mark);
    lx.b.br(l_out, &[w]);
    lx.b.switch_to(l_out);
    lx.b.block_params(l_out)[0]
}

use super::raw::order::prefix8;

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    /// The prefix words order as the bytes do, compared as signed words; past 8 bytes a tie is the tail's to break.
    #[test]
    fn prefixes_order_as_bytes() {
        // SAFETY: each slice is readable for its length.
        let img = |s: &[u8]| unsafe { super::prefix8(s.as_ptr(), s.len() as u64) as i64 };
        let words: Vec<i64> = [&b""[..], b"\x01", b"a", b"a\x01", b"ab", b"abcdefgh", b"b", b"\xff"].iter().map(|s| img(s)).collect();
        assert!(words.windows(2).all(|w| w[0] < w[1]), "{words:x?}");
        assert_eq!(img(b"abcdefghX"), img(b"abcdefghY"));
    }
}

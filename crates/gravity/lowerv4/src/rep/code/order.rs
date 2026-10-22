//! Code order (`rep.code.order`, plan §3.2; lever R4-L06 / R4-L13): a code orders as its render.
//! - In-part rank: a part's dictionary is byte-rank sorted (hosts granule_source.rs: the writer's dictionary, or a verbatim
//!   part's ranked codes), so inside one part the code IS the byte order of the values; `rank` gives it to the roads that stay
//!   inside a part (the per-part code cut, min / max per part, the per-part front) with no render.
//! - Cross-part: the order image is the render's first 8 bytes, big-endian, zero-padded, with the top bit flipped so the
//!   words compare as signed `I64` (the order-image convention, as the float order words); a tie on the prefix is not a tie of
//!   the values, so the image is inexact and the consumer's exact compare (the full renders) decides a tie.
//! Byte order is PG's order only under a collation that orders by bytes ("C" / POSIX). Under any other collation the order
//! image is the render's, taken by rep.varlena.order (its collated prefix and tail, M6 L0-d1); a rank has no collated form
//! and refuses the `key.coll` road.
use gravity_decompose::catalog::typefacts::{KeyColl, TypeFacts};
use gravity_ir::{HostFlags, Type, Value};
use gravity_program::Rep;

use crate::cell::Cell;
use crate::face::kinds::KindId;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;
use crate::rep::{self, CellRef, Lx, OrderArm, Words};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::RepCodeOrder, &ARM)], ..crate::ops::NONE };

pub(crate) const ARM: OrderArm = OrderArm { image, name: None };

/// The in-part rank of a lane cell (or of a kept `part << 32 | code` word): its code, under a collation that orders by
/// bytes (`coll`: the key's or the aggregate's input collation as decompose read it from the catalog, D247); any other
/// collation refuses `key.coll`.
pub(crate) fn rank(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, coll: KeyColl) -> Value {
    crate::rep::code::datum::not_image_only(cx, c.cell, "an image-only key cell read as a rank");
    bytes_order(cx, coll);
    crate::rep::code::datum::code(cx, c)
}

/// Byte order is this collation's order, or the rank refuses (a rank has no collated form).
fn bytes_order(cx: &Lx<'_, '_, '_>, coll: KeyColl) {
    if !coll.bytes_order {
        crate::refuse!(cx, Reason::Shape { why: ShapeWhy("an in-part code rank under a collation that does not order by bytes") });
    }
}

fn image(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, coll: KeyColl) -> Words {
    crate::rep::code::datum::not_image_only(cx, c.cell, "an image-only key cell read as an order image");
    let (p, null) = rep::datum(cx, c, tf);
    if !coll.bytes_order {
        // the render (the dictionary's resident varlena) orders under the collation as a varlena cell does
        let cell = Cell { rep: Rep::VarlenaPtr, at: crate::rep::word::datum::off_lane(c.cell.at), ..*c.cell };
        return (rep::arm::<OrderArm>(&*cx, KindId::RepVarlenaOrder).image)(cx, CellRef { cell: &cell, val: p, null: Some(null) }, tf, coll);
    }
    let zero = cx.b.iconst(Type::I64, 0);
    let (l_img, l_out) = (cx.b.create_block(&[]), cx.b.create_block(&[Type::I64]));
    cx.b.cond_br(null, (l_out, &[zero]), (l_img, &[]));
    cx.b.switch_to(l_img);
    let h = cx.b.declare_host_once("rep_code_prefix", &[Type::I64], Some(Type::I64), gravity_abi::host_words!(prefix8(p)), HostFlags { touches_memory: true, pure: true, ..HostFlags::default() });
    let w = cx.b.call_host(h, &[p], Some(Type::I64)).expect("returns the prefix word");
    cx.b.br(l_out, &[w]);
    cx.b.switch_to(l_out);
    Words { w: [Some(cx.b.block_params(l_out)[0]), None], exact: false }
}

use super::raw::order::prefix8;

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    /// The prefix words order as the bytes do, compared as signed words; a shorter prefix of a longer string orders first.
    #[test]
    fn prefixes_order_as_bytes() {
        let img = |s: &[u8]| -> i64 {
            let mut v = ((s.len() as u32 + 4) << 2).to_le_bytes().to_vec();
            v.extend_from_slice(s);
            // SAFETY: a 4-byte-header varlena image.
            unsafe { super::prefix8(v.as_ptr() as u64) as i64 }
        };
        let words: Vec<i64> = [&b""[..], b"a", b"ab", b"abcdefgh", b"b", b"\xff"].iter().map(|s| img(s)).collect();
        assert!(words.windows(2).all(|w| w[0] < w[1]), "{words:x?}");
        assert_eq!(img(b"abcdefghX"), img(b"abcdefghY"), "past 8 bytes a tie is the consumer's to break");
    }
}

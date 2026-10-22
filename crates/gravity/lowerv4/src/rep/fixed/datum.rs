//! FixedPtr datum (`rep.fixed.datum`, plan §3.2; KEEP lowerv3 rep/fixed.rs): a pointer to a fixed-length by-reference image
//! (name 64, uuid 16, interval 16, timetz 12, tid / macaddr 6, macaddr8 8, point 16, box / lseg 32, aclitem 16, circle 24),
//! never a varlena.
//! - `datum`: the pointer the cell carries; a lane cell's `val` is its element's address, holding that pointer.
//! - `retain`: the `len` bytes copied into the consumer's home (`Lx.space`), open-coded (the length is the Rep's, known
//!   while lowering: whole words, then a 4 / 2 / 1-byte tail; nothing read past the image); nothing is read for a NULL.
//! - no block form (H01).
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, Type, Value};
use gravity_program::{Rep, Until};

use crate::face::kinds::KindId;
use crate::ops::Table;
use crate::rep::{self, CellRef, DatumArm, Kept, Lx, RetainHome};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::RepFixedDatum, &ARM)], ..crate::ops::NONE };

pub(crate) const ARM: DatumArm = DatumArm { datum, retain, block: None };

fn datum(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, _tf: &TypeFacts) -> (Value, Value) {
    rep::datum_word(cx, c, 8)
}

fn retain(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, until: Until, home: RetainHome) -> Kept {
    let len = match c.cell.rep {
        Rep::FixedPtr { len } if len > 0 => len,
        _ => crate::refuse!(cx, crate::admit::refusal::Reason::Bug { payload: "a FixedPtr cell without its length" }),
    };
    crate::rep::varlena::datum::keep_with(cx, c, tf, until, home, &|b, _p| b.iconst(Type::I64, len as i64), &|b, dst, src, _n| copy_fixed(b, dst, src, len))
}

/// `len` bytes (known while lowering) from the datum pointer `src` to `dst`, as unaligned loads and stores of 8, then the
/// 4 / 2 / 1-byte tail: no loop, no host, no byte read or written past `len`.
pub(crate) fn copy_fixed(b: &mut FunctionBuilder<'_>, dst: Value, src: Value, len: u16) {
    let (d, s) = (b.conv(ConvOp::IntToPtr, Type::Ptr, dst), b.conv(ConvOp::IntToPtr, Type::Ptr, src));
    let mut at = 0i32;
    for (w, ty) in [(8, Type::I64), (4, Type::I32), (2, Type::I16), (1, Type::I8)] {
        while len as i32 - at >= w {
            let v = b.load_unaligned(ty, s, at);
            b.store_unaligned(d, at, v);
            at += w;
            if w < 8 {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::rep::varlena::datum::tests::kept;
    use gravity_program::Rep;

    #[test]
    fn every_length_is_copied_exactly() {
        for len in [6u16, 8, 12, 16, 24, 32, 64] {
            let src: Vec<u8> = (1..=len as u8).collect();
            let mut dst = [0u8; 72];
            let v = kept(Rep::FixedPtr { len }, [(6, 829), (8, 774), (12, 1266), (16, 2950), (24, 718), (32, 603), (64, 19)].iter().find(|x| x.0 == len).unwrap().1, &src, &mut dst);
            assert_eq!(v, dst.as_ptr() as u64);
            assert_eq!(&dst[..len as usize], &src[..], "{len} bytes");
            assert!(dst[len as usize..].iter().all(|&x| x == 0), "{len}: nothing written past the image");
        }
    }
}

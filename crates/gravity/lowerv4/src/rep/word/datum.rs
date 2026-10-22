//! Word datum (`rep.word.datum`, plan §3.2; KEEP lowerv3 rep/word.rs): a by-value datum word of 1 / 2 / 4 / 8 bytes (ints,
//! dates, timestamps, oids, money, float4 / float8, bool, "char": D198).
//! - `datum`: a lane cell's `val` is its element's address, loaded at the lane's width by the lane rule (`rep::datum_word`:
//!   8 as is, 4 / 2 sign-extended as PostgreSQL's `Int32GetDatum` / `Int16GetDatum`, 1 zero-extended); any other cell's
//!   `val` is the word itself.
//! - `retain`: the word is the value, so nothing is copied; the retained cell lives as long as the statement.
//! - `block` (H01): the lane's elements are the block form, `bytes` wide, read by the mask roads (`mask.word`, `mask.vec`).
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_ir::Value;
use gravity_program::{Home, Rep, Until};

use crate::cell::{At, Cell, CellHome, CellLife};
use crate::face::kinds::KindId;
use crate::admit::refusal::Reason;
use crate::ops::Table;
use crate::rep::{self, BlockLane, BlockRef, CellRef, DatumArm, Kept, Lx, RetainHome};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::RepWordDatum, &ARM)], ..crate::ops::NONE };

pub(crate) const ARM: DatumArm = DatumArm { datum, retain, block: Some(block) };

/// A word cell's element width: the `Word`'s `bytes` (a heap lane publishes every by-value lane as an 8-byte datum word).
pub(crate) const fn bytes(rep: Rep) -> u8 {
    match rep {
        Rep::Word { bytes } => bytes,
        _ => 8,
    }
}

/// The life of a word once read: the value itself, valid for the statement.
pub(crate) const WORD_LIFE: CellLife = CellLife { until: Until::Statement, home: CellHome::Lane(Home::Word) };

fn datum(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, _tf: &TypeFacts) -> (Value, Value) {
    rep::datum_word(cx, c, bytes(c.cell.rep))
}

/// The word read once and kept as itself: no copy, whatever `until` (a lane's element address stays behind with its window).
fn retain(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, _until: Until, _home: RetainHome) -> Kept {
    let (w, null) = rep::datum(cx, c, tf);
    Kept { cell: Cell { life: WORD_LIFE, at: off_lane(c.cell.at), ..*c.cell }, val: w, null: Some(null) }
}

/// Where a kept value's `val` is its word (not a lane element's address): a lane's `At` becomes the value of its column.
pub(crate) const fn off_lane(at: At) -> At {
    match at {
        At::Lane(l) => At::Value(l.attno),
        a => a,
    }
}

/// The block form of a word lane: row `j0`'s element (`at.elem`, the frame's address) and the element width. A computed word
/// has no lane behind it; a mask road's predicate never holds over one, so reaching here is a Bug.
fn block(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, _tf: &TypeFacts, at: BlockRef) -> BlockLane {
    if !matches!(c.cell.at, At::Lane(_)) {
        crate::refuse!(cx, Reason::Bug { payload: "the block form of a word cell that is not a lane" });
    }
    BlockLane { base: at.elem, bytes: bytes(c.cell.rep) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{LaneId, Nullable};
    use gravity_interp::Interp;
    use gravity_decompose::catalog::typefacts::row_of;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_ir::Type;
    use gravity_program::NumTypmod;

    fn lane(bytes: u8, typid: u32) -> Cell {
        Cell { rep: Rep::Word { bytes }, typid, num: NumTypmod::NONE, life: WORD_LIFE, at: At::Lane(LaneId::col(0)), null: Nullable::Maybe }
    }

    /// The datum of an element of each width read as PostgreSQL's datum word: 8 as is, 4 / 2 sign-extended, 1 zero-extended.
    #[test]
    fn a_lane_element_reads_as_the_datum_word() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        for (bytes, typid, elem, want) in [(8u8, 20u32, -5i64 as u64, -5i64 as u64), (4, 23, 0xFFFF_FFFB, -5i64 as u64), (2, 21, 0xFFFB, -5i64 as u64), (1, 16, 0xFF, 0xFF)] {
            let mut pb = ProgramBuilder::new();
            let f = pb.declare_func("d", &[Type::Ptr], &[Type::I64]);
            let mut b = pb.define(f);
            let p = b.params()[0];
            let c = lane(bytes, typid);
            let tf = *row_of(typid).expect("type facts");
            let v = {
                let mut cx = Lx::new(&mut b, 1, rig.mcx());
                datum(&mut cx, CellRef { cell: &c, val: p, null: None }, &tf).0
            };
            b.ret(&[v]);
            let prog = pb.finish();
            let buf = elem.to_le_bytes();
            let got = Interp::new(&prog).run(f, &[buf.as_ptr() as u64]).expect("runs");
            assert_eq!(got[0], want, "{bytes}-byte element");
        }
    }

    #[test]
    fn a_retain_copies_nothing() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let c = Cell { life: CellLife { until: Until::Window, home: CellHome::RowCx }, ..lane(8, 20) };
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("r", &[Type::Ptr], &[Type::I64]);
        let mut b = pb.define(f);
        let p = b.params()[0];
        let mut cx = Lx::new(&mut b, 1, rig.mcx());
        let kept = retain(&mut cx, CellRef { cell: &c, val: p, null: None }, row_of(20).unwrap(), Until::Life, RetainHome::Frame);
        assert_eq!((kept.cell.life, kept.cell.rep, kept.cell.at), (WORD_LIFE, c.rep, At::Value(0)));
        assert!(kept.null.is_some());
    }
}

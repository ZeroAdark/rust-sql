//! Kind `rep.packed.order` (`OrderArm`, cards/M6.md L3, M6-plan §3.2): the order image of a packed numeric lane
//! (`Rep::PackedNum`, a pgrc `numeric(p, s)` column) — one exact signed word, the value's mantissa at the lane's scale:
//! a packed window's element as is, a verbatim part's stored image through rep.packed.datum's `retain`
//! (`agg_final::num_mantissa` at the column's scale; a stored `numeric(p, s)` value always has dscale `s`), a kept word as
//! is. PostgreSQL's order (`cmp_numerics`, numeric.c): `-Inf` below every finite value, `+Inf` above, `NaN` above both and
//! equal to itself; the sentinels `MANT_NINF < finite < MANT_PINF < MANT_NAN` order so as signed words (a finite
//! mantissa of p <= 18 digits is below 10^18). Equal words are equal values (one scale: `1.0` and `1.00` are one
//! stored value). DESC and NULLS fold in the caller (rep/word/order.rs `fold`). Owner M6-TYPES.
use gravity_decompose::catalog::typefacts::{KeyColl, TypeFacts};
use gravity_program::{Rep, Until};
use types_core::catalog::NUMERICOID;

use crate::face::kinds::KindId;
use crate::admit::refusal::Reason;
use crate::ops::Table;
use crate::rep::{self, CellRef, DatumArm, Lx, OrderArm, RetainHome, Words};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::RepPackedOrder, &ARM)], ..crate::ops::NONE };

pub(crate) const ARM: OrderArm = OrderArm { image, name: None };

/// The mantissa word (0 for a NULL: `fold` places it by its flag).
fn image(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, _coll: KeyColl) -> Words {
    if !matches!(c.cell.rep, Rep::PackedNum { .. }) {
        crate::refuse!(cx, Reason::Bug { payload: "a packed order image over a cell that is not a packed numeric" });
    }
    let retain = rep::arm::<DatumArm>(&*cx, KindId::RepPackedDatum).retain;
    let kept = retain(cx, c, tf, Until::Row, RetainHome::Frame);
    Words { w: [Some(kept.val), None], exact: true }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{At, Cell, CellHome, CellLife, Nullable};
    use gravity_decompose::catalog::typefacts::row_of;
    use gravity_fn::agg_final::{MANT_NAN, MANT_NINF, MANT_PINF};
    use gravity_interp::Interp;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_ir::{IntCond, Type};
    use gravity_program::{NumTypmod, NumForm};

    /// The order word of a kept packed word `m` (`None`: NULL).
    fn ord(m: Option<i64>) -> i64 {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        crate::catch(9, || {
            let tf = *row_of(NUMERICOID).expect("numeric");
            let facts = NumTypmod::of(12, 2);
            let c = Cell { rep: Rep::PackedNum { facts, form: NumForm::Word }, typid: NUMERICOID, num: facts, life: CellLife { until: Until::Row, home: CellHome::RowCx }, at: At::Value(0), null: Nullable::Maybe };
            let mut pb = ProgramBuilder::new();
            let fid = pb.declare_func("o", &[Type::I64, Type::I64], &[Type::I64]);
            let mut b = pb.define(fid);
            let (v, nw) = (b.params()[0], b.params()[1]);
            let out = {
                let zero = b.iconst(Type::I64, 0);
                let null = b.icmp(IntCond::Ne, nw, zero);
                let mut cx = Lx::new(&mut b, 9, rig.mcx());
                let w = image(&mut cx, CellRef { cell: &c, val: v, null: Some(null) }, &tf, KeyColl::NONE);
                assert!(w.exact && w.w[1].is_none());
                w.w[0].unwrap()
            };
            b.ret(&[out]);
            let prog = pb.finish();
            Interp::new(&prog).run(fid, &[m.unwrap_or(7) as u64, m.is_none() as u64]).expect("runs")[0] as i64
        })
        .expect("an order word")
    }

    /// cmp_numerics' order over the sentinels and finite values; a NULL reads nothing.
    #[test]
    fn words_order_as_postgres() {
        let xs = [MANT_NINF, -999_999_999_999, -1, 0, 1, 12_345, 999_999_999_999, MANT_PINF, MANT_NAN];
        let w: Vec<i64> = xs.iter().map(|&m| ord(Some(m))).collect();
        assert!(w.windows(2).all(|p| p[0] < p[1]), "{w:?}");
        assert_eq!(ord(Some(MANT_NAN)), ord(Some(MANT_NAN)), "NaN = NaN");
        assert_eq!(ord(None), 0);
    }
}

//! `ArrayExpr` in the emitter (expr.md §7; lane hosted-emit, landing 15): every element evaluated, the null mask folded in
//! IR, the element facts word, then the non-strict site (`nodes/array.rs::ARRAY_SITE`; the multidims form's elements are
//! arrays, concatenated a dimension up by `MD_SITE`, V4R-85). Past one masked site's elements the mask words and the
//! elements ride the continuation cells (`ARRAY_WIDE` / `MD_WIDE`, PS4-10).

use gravity_ir::{ConvOp, IntBinOp, Type, Value};
use mcx::{GrowIn, PgVec};

use super::scalar::Em;
use super::{EmitError, Lowered};
use crate::nodes::array::ArrayExpr;
use crate::nodes::Kids;

impl<'e> Em<'e, '_, '_> {
    pub(super) fn array_expr(&mut self, n: &ArrayExpr) -> Result<Lowered, EmitError> {
        let facts = self.b.iconst(Type::I64, n.elem.word());
        if crate::nodes::row::wide(n.elements.len as usize) {
            return self.wide_array(n, facts);
        }
        let args = self.masked(facts, n.elements)?;
        let r = self.call(n.site, &args, false);
        if r.is_ok() {
            fires::fire("expr.array_emit"); // shapecheck CR2-12 (a cached program does not re-emit)
        }
        r
    }

    /// PS4-10: an ARRAY[] past one masked site (`nodes::array::site_of`): its mask words and elements in the continuation
    /// cells (`continued`), then `ARRAY_WIDE` / `MD_WIDE` over the site's own cell, the count and the facts word. The
    /// first raise among the elements rides the cell word, as `masked` keeps it on the head.
    fn wide_array(&mut self, n: &ArrayExpr, facts: Value) -> Result<Lowered, EmitError> {
        let Some(cells) = self.row.cells else { return Err(EmitError::NoCells) };
        let cell = self.site_cell(cells, n.site.0 as usize);
        let err = self.continued(cell, n.elements)?;
        let at = self.b.conv(ConvOp::PtrToInt, Type::I64, cell);
        let count = self.b.iconst(Type::I64, n.elements.len as i64);
        let args = [Lowered { value: at, valid: self.t, err }, Lowered { value: count, valid: self.t, err: self.zero }, Lowered { value: facts, valid: self.t, err: self.zero }];
        let r = self.call(n.site, &args, false);
        if r.is_ok() {
            fires::fire("expr.array_wide"); // shapecheck PS4-10: an ARRAY[] past one site's 97 elements (a cached program does not re-emit)
        }
        r
    }

    /// A masked site's arguments (`nodes::func::masked`): `head`, the null mask's words folded in IR, then every value of
    /// `kids` evaluated in order (D775: a value calling a host only where no earlier one raised; the first raise kept, on
    /// the head word, so `call` never reaches the site past it and keeps it first).
    pub(super) fn masked(&mut self, head: Value, kids: Kids) -> Result<PgVec<'e, Lowered>, EmitError> {
        let n = kids.len as usize;
        let words = crate::nodes::func::mask_words(n);
        let mut args = mcx::reserved_in(1 + words + n, self.row.scratch);
        args.push_in(Lowered { value: head, valid: self.t, err: self.zero });
        for _ in 0..words {
            args.push_in(Lowered { value: self.zero, valid: self.t, err: self.zero });
        }
        let mut err = self.zero;
        for i in 0..n {
            let a = self.after(err, self.p.kids[kids.start as usize + i])?;
            let bit = self.b.iconst(Type::I64, 1i64 << (i % 64));
            let m = self.b.select(a.valid, self.zero, bit);
            let w = &mut args[1 + i / 64].value;
            *w = self.b.int_bin(IntBinOp::Or, *w, m);
            err = self.first(err, a.err);
            args.push_in(Lowered { value: a.value, valid: self.t, err: self.zero });
        }
        args[0].err = err;
        Ok(args)
    }
}

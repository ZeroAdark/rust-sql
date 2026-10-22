//! The `RowExpr` in the emitter (expr.md §7; lane hosted-emit, landing 14): every argument evaluated, the null mask folded
//! in IR, the descriptor loaded from the site's cell (`CELL_AUX`, the owner's fact), then the non-strict site.

use gravity_ir::{ConvOp, Type};
use mcx::GrowIn;

use super::host::{CELL_ARGS, CELL_AUX};
use super::scalar::Em;
use super::{EmitError, Lowered};
use crate::nodes::func::MAX_ARGS;
use crate::nodes::row::{convert_word, wide, ConvertRowtype, RowExpr, CONVERT_HEAD};

impl Em<'_, '_, '_> {
    pub(super) fn row_expr(&mut self, n: &RowExpr) -> Result<Lowered, EmitError> {
        let Some(cells) = self.row.cells else { return Err(EmitError::NoCells) };
        let cell = self.site_cell(cells, n.site.0 as usize);
        if wide(n.args.len as usize) {
            return self.wide_row(n, cell);
        }
        let desc = self.b.load(Type::I64, cell, CELL_AUX);
        let args = self.masked(desc, n.args)?;
        self.call(n.site, &args, false)
    }

    /// CR-WIDE-ROW: a row past one masked site (`nodes::row::row_site`): its words in the continuation cells (`continued`),
    /// then `ROW_WIDE` over the site's own cell and the count. The first raise among the values rides the cell word, as
    /// `masked` keeps it on the head.
    fn wide_row(&mut self, n: &RowExpr, cell: gravity_ir::Value) -> Result<Lowered, EmitError> {
        let err = self.continued(cell, n.args)?;
        let at = self.b.conv(ConvOp::PtrToInt, Type::I64, cell);
        let count = self.b.iconst(Type::I64, n.args.len as i64);
        let args = [Lowered { value: at, valid: self.t, err }, Lowered { value: count, valid: self.t, err: self.zero }];
        let r = self.call(n.site, &args, false);
        if r.is_ok() {
            fires::fire("expr.row_wide"); // shapecheck: a row past one site's 97 values (a cached program does not re-emit)
        }
        r
    }

    /// A wide site's values (`nodes::row::continuation_sites`): their mask words and values, `masked`'s order past the head,
    /// stored word by word into the continuation sites' cells after `cell`, the site's own; the first raise among them.
    pub(super) fn continued(&mut self, cell: gravity_ir::Value, kids: crate::nodes::Kids) -> Result<gravity_ir::Value, EmitError> {
        let zero = self.zero;
        let words = self.masked(zero, kids)?;
        // the continuation sites are `MAX_ARGS` wide, so the program's cells are (`host::cell_width`)
        let stride = self.cell_stride();
        debug_assert_eq!(stride, super::host::cell_size(MAX_ARGS), "a wide site's program keeps MAX_ARGS-wide cells");
        for (w, a) in words[1..].iter().enumerate() {
            let off = (1 + w / MAX_ARGS) * stride + CELL_ARGS as usize + (w % MAX_ARGS) * 8;
            self.b.store(cell, off as i32, a.value);
        }
        Ok(words[0].err)
    }

    /// The rowtype conversion: the composite, then the strict site over the descriptor table from `CELL_AUX`, the
    /// descriptors' word and the map's attribute numbers as constants.
    pub(super) fn convert_rowtype(&mut self, n: &ConvertRowtype) -> Result<Lowered, EmitError> {
        let Some(cells) = self.row.cells else { return Err(EmitError::NoCells) };
        let arg = self.node(n.arg)?;
        let cell = self.site_cell(cells, n.site.0 as usize);
        let descs = self.b.load(Type::I64, cell, CELL_AUX);
        let word = self.b.iconst(Type::I64, convert_word(n));
        let nmap = self.p.sites[n.site.0 as usize].nargs as usize - CONVERT_HEAD;
        let mut args = mcx::reserved_in(CONVERT_HEAD + nmap, self.row.scratch);
        args.push_in(Lowered { value: descs, valid: self.t, err: self.zero });
        args.push_in(Lowered { value: word, valid: self.t, err: self.zero });
        args.push_in(arg);
        for i in 0..nmap {
            let attno = self.p.oids[n.map.unwrap_or(0) as usize + i] as i64;
            let value = self.b.iconst(Type::I64, attno);
            args.push_in(Lowered { value, valid: self.t, err: self.zero });
        }
        self.call(n.site, &args, false)
    }
}

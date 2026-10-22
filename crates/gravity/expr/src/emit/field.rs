//! The `FieldSelect` in the emitter (expr.md §7): the composite, then the strict site; the descriptor from `CELL_AUX` (0 for `record`).

use gravity_ir::Type;

use super::host::CELL_AUX;
use super::scalar::Em;
use super::{EmitError, Lowered};
use crate::nodes::field::{word, FieldSelect};

impl Em<'_, '_, '_> {
    pub(super) fn field_select(&mut self, n: &FieldSelect) -> Result<Lowered, EmitError> {
        let Some(cells) = self.row.cells else { return Err(EmitError::NoCells) };
        let arg = self.node(n.arg)?;
        let cell = self.site_cell(cells, n.site.0 as usize);
        let desc = self.b.load(Type::I64, cell, CELL_AUX);
        let d = Lowered { value: desc, valid: self.t, err: self.zero };
        let value = self.b.iconst(Type::I64, word(n));
        let w = Lowered { value, valid: self.t, err: self.zero };
        self.call(n.site, &[d, w, arg], false)
    }
}

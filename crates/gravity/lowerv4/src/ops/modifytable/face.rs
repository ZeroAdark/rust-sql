//! The RETURNING continuation face (M4-RET; cards/M4.md L5 `write/returning.rs`, M4-plan §3.1 / §3.10). A row-op kind
//! (write.insert, write.update, write.delete's hand-off, the ON CONFLICT DO UPDATE arm) calls the host in its step; on
//! `RowStatus::Done` its `done` continuation (`RowOpMech::call`) hands this face's `lower` the written row, the cells the
//! host left in the site (`RowOpMech::cells`, `written()` applied; the conflict site's after DO UPDATE), and the face
//! hands it on keyed by target attno to the rest of the chain (the decomposer's RETURNING projection, then the end). A
//! write kind calls `returning::make_ret` when its write op carries
//! RETURNING. Declared by registry/m4.d (a face file: no rows of its own).
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;

use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};

pub(crate) trait RetMech {
    /// After the op answered Done for `row`: `written`, `(datum, isnull)` per target attribute in attno order as
    /// `RowOpMech::cells` returns them, handed to `next` as the row; the carry after.
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, written: &[(Value, Value)], next: Next<'_, 'e>) -> Carry<'e>;
    /// `lower` with the version an UPDATE replaced (R4a, RETURNING OLD: `RowOpMech::old_cells`), `old` empty when the
    /// write fetched none.
    fn lower_old<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, written: &[(Value, Value)], old: &[(Value, Value)], next: Next<'_, 'e>) -> Carry<'e> {
        let _ = old;
        self.lower(b, env, row, written, next)
    }
    /// D789: a MERGE action's row: `written` (an UPDATE's new row, a DELETE's deleted version), OLD `old` (the replaced
    /// version; a DELETE's is `written`), NEW `written` when `new` (UPDATE) else NULLs, and `merge_action()`'s text.
    fn lower_merge<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, written: &[(Value, Value)], old: &[(Value, Value)], new: bool, action: &'static [u8], next: Next<'_, 'e>) -> Carry<'e>;
}

/// The maker for write op `j` of pipeline `k` (its RETURNING projection is op `j + 1`).
pub(crate) struct RetMaker(pub for<'g, 'p> fn(&mut Px<'g, 'p>, usize, usize) -> mcx::PgBox<'g, dyn RetMech + 'g>);

//! The Result source (`Source::Const`; v2 source/consts.rs, v3 had none): no site, one row of no cells, a once source. Its
//! target list and one-time quals are the pipeline's own steps (`step.project`, `step.gate`), so a Result reading a column
//! of a scan that does not exist refuses in the expression compile, never here. The frame writes the position after the
//! last row taken back through `write_back` as the resume word (`RESUME_OPEN + cursor`, as `source/values.rs`), so a call
//! resumed inside the row (an expanding step's offset, H17 c) re-reads `[pos, 1)` and a finished row is never re-emitted.
//! Registers kind `src.result`. Owner M1-SOURCE.
use gravity_decompose::Source;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntCond, Type, Value};
use mcx::PgBox;

use crate::cell::Row;
use crate::face::kinds::KindId;
use crate::face::source::{SourceMech, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::{SourceMaker, Win};
use crate::frame::px::Px;
// the resume position lives in the frame; hashjoin's drain and spill sources still name it through this path
pub(crate) use crate::frame::resume::{resume_pos, write_pos};
use crate::frame::row::{Carry, RowVals};
use crate::admit::refusal::Reason;
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::SrcResult, &SourceMaker(make))], ..crate::ops::NONE };

/// The one constant row.
struct ResultRow;

fn make<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let p = &px.gx.pipelines()[k];
    if !matches!(p.source, Source::Const) {
        crate::refuse!(p.node_id, Reason::Bug { payload: "a Result entry over another source" });
    }
    mcx::pg_box_dyn!(px.mcx(), ResultRow => dyn SourceMech + 'g)
}

impl SourceMech for ResultRow {
    fn once(&self) -> bool {
        true
    }

    fn open(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        env.zero
    }

    /// `[pos, 1)`; past the row, the end.
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let pos = resume_pos(b, env);
        let past = b.icmp(IntCond::Sge, pos, env.one);
        let l_win = b.create_block(&[]);
        b.cond_br(past, (l_eof, &[env.zero]), (l_win, &[]));
        b.switch_to(l_win);
        let t = b.iconst(Type::I64, 1);
        let facts = WinConds { nonulls: Some(t), nosel: Some(t), ..WinConds::default() };
        Win { cursor: pos, end: env.one, facts }
    }

    fn row<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        RowVals { row: Row::new_in(env.mcx), vals: mcx::PgVec::new_in(env.mcx), cols: mcx::PgVec::new_in(env.mcx), carry, next, at: j }
    }

    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, _out: Value) {
        write_pos(b, env, cur);
    }
}

//! The array-key index source (cards/M2.md L5; KEEP lowerv3 scan/index.rs `walk`'s `KeyForm::Any` over a Const, the
//! ladder's `index.rs:47-51`): a btree walk with `column = ANY (array)` keys as nbtree's native array keys
//! (`_bt_preprocess_array_keys`: the array sorted and deduplicated once, one descent per distinct element in index order,
//! skipping the elements no row can match), each matching heap row fetched by the host in index order. Every array (a
//! constant's too, V4R-108: never an image the recipe holds, so one program serves every literal list of its shape) and
//! the scalar keys are bound from the bind's words at the seek (hosts V4R-44, `SiteRecipe::from_plan_words`, exec
//! m2_admit.rs `index_array`). Rows in index order; the window's position is the row's tid (the locator, `ctid`); an
//! EvalPlanQual recheck is the host's key test. The graph routes a btree walk with an `= ANY` key here (graph.rs, L0-h);
//! an index-only one is src.index_only's. Registers kind `src.index_array`. Owner M2-INDEX.
use gravity_decompose::Source;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, Value};
use gravity_program::{LaneDesc, Pipeline};
use mcx::PgBox;

use crate::ops::scan::index_only::{key_words, shape};
use crate::face::kinds::KindId;
use crate::face::source::{Locator, SourceMech, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::{SourceMaker, Win, WordWin};
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::site::{index_site, Reader, CTID_AT};
use crate::admit::refusal::Reason;
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::SrcIndexArray, &SourceMaker(make))], ..crate::ops::NONE };

struct IndexArray<'g> {
    rd: Reader<'g>,
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Source::Index { site, am, relid, indexid, keys, only: false, .. } = &p.source else { crate::refuse!(node, Reason::Bug { payload: "an array-key index entry over another source" }) };
    if !crate::ops::scan::index::index_fetch(gx, *am, *relid) {
        shape(node, "an index scan over a relation that is not a heap table");
    }
    let (slots, attnos) = key_words(px, node, *indexid, keys);
    let (mut s, ctid) = index_site(px, site, *relid, &slots, &attnos, node);
    // D2665: the system columns the scan reads (V4R-88 lanes), filled from each fetched row's header (hosts heap_fetch.rs `project`)
    crate::ops::scan::heap::add_sys_lanes(px, &mut s, node);
    // the RowId lane after the attributes: the host's N-row windows, each row's heap tid (V4R-55)
    s.add_lane(LaneDesc::rowid(), false);
    let rd = Reader::new(px, &s, *relid, node, Some(slots), ctid.then_some(CTID_AT));
    px.set_site(k, s);
    mcx::pg_box_dyn!(px.mcx(), IndexArray { rd } => dyn SourceMech + 'g)
}

impl SourceMech for IndexArray<'_> {
    fn declare(&mut self, pb: &mut ProgramBuilder) {
        self.rd.declare(pb);
    }

    /// The decomposer's descriptor: an index walk runs serial on the leader in index order.
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        px.gx.pipelines()[k].desc
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        self.rd.open(b, env)
    }

    /// N heap rows per window (the host's `IosBatch`, V4R-55): no selection, and nulls only where a lane has a bitmap.
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let (cursor, end) = self.rd.window(b, env, l_eof);
        let w = self.rd.vals();
        Win { cursor, end, facts: WinConds { nonulls: self.rd.nonulls(b), nosel: Some(w.sel_null), ..WinConds::default() } }
    }

    fn word_win<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win) -> Option<WordWin<'e>> {
        self.rd.word_win(env.mcx)
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        self.rd.row(b, env, j, carry, next)
    }

    fn locator(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env, _row: &RowVals) -> Option<Locator> {
        self.rd.locator(b)
    }
}

//! The btree index source (KEEP lowerv3 scan/index.rs; the ladder's `lower/src/index.rs`): a btree walk of `column op
//! value` keys bound from the bind's words into the site's key cells before the seek, each matching heap row fetched by
//! the host in index order (hosts `IndexSource`, N rows per window: `IosBatch`, V4R-55). The window loop is the pgrc
//! source's reader (the core's site-window reader, frame/site.rs). Each row's tid is the site's RowId lane: the row op's
//! locator (H28, M1-WRITE) and `ctid` when the scan reads it; an EvalPlanQual recheck is the host's (`IndexSource::substitute`, the key recheck). `= ANY`
//! keys are `src.index_array`'s, index-only scans `src.index_only`'s and the other AMs `src.index_am`'s (M2); the graph
//! routes those kinds. Registers kind `src.index`. Owner M1-SOURCE.
use gravity_decompose::graph::KeyForm;
use mcx::{GrowIn, PgBox};
use gravity_decompose::{Am, Source};
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, Value};
use gravity_program::{limits, LaneDesc, Pipeline};

use crate::ops::scan::pgrc::lower::shape;
use crate::frame::site::{index_site, Reader, CTID_AT};
use crate::face::kinds::KindId;
use crate::face::source::{Locator, SourceMech, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::{SourceMaker, Win, WordWin};
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::admit::refusal::{Reason, Variant};
use crate::lifetime::bound::{self, Feed, KeyAt};
use crate::ops::Table;

pub(crate) fn index_fetch(gx: &crate::graph::Gx<'_, '_>, am: Am, relid: u32) -> bool {
    am == Am::Heap || gx.facts.rel(relid).is_some_and(|r| r.index_fetch())
}

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::SrcIndex, &SourceMaker(make))], ..crate::ops::NONE };

struct Index<'g> {
    rd: Reader<'g>,
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Source::Index { site, am, relid, keys, .. } = &p.source else { crate::refuse!(node, Reason::Bug { payload: "an index entry over another source" }) };
    if !index_fetch(gx, *am, *relid) {
        shape(node, "an index scan over a relation that is not a heap table");
    }
    // the key words in the host's order (hosts `SiteRecipe::words`): a comparison's value, or a row comparison's
    // members' values in column order (PostgreSQL's SK_ROW_MEMBER keys; a NULL member is the host's SK_ISNULL)
    let mut vals = mcx::reserved_in(keys.len(), px.mcx());
    for key in keys.iter() {
        match key.form {
            KeyForm::Op => vals.push_in((key.attno, key.value)),
            KeyForm::Row => {
                let Some(rc) = key.value.as_row_compare_expr() else { crate::refuse!(node, Reason::Bug { payload: "a row comparison index key over another node" }) };
                let idx = gx.facts.index(site.index);
                for (l, r) in rc.largs.iter().zip(rc.rargs.iter()) {
                    let col = l.as_var().map_or(0, |v| v.varattno as usize);
                    let attno = idx.as_ref().filter(|i| (1..=i.nkeys as usize).contains(&col)).map_or(0, |i| i.keys[col - 1]);
                    vals.push_in((attno, r));
                }
            }
            KeyForm::Any | KeyForm::AmAny => crate::not_built!(node, Variant::Kind(KindId::SrcIndexArray)),
            // `column IS [NOT] NULL` (D409): the host's SK_SEARCHNULL / SK_SEARCHNOTNULL key, no word
            KeyForm::Null | KeyForm::NotNull => {}
            KeyForm::AmOp | KeyForm::OrderBy => crate::not_built!(node, Variant::Kind(KindId::SrcIndexAm)),
        }
    }
    if vals.len() > limits::KEYS {
        shape(node, "an index scan with more key words than INDEX_MAX_KEYS");
    }
    let mut slots = mcx::reserved_in(vals.len(), px.mcx());
    let mut attnos = mcx::reserved_in(vals.len(), px.mcx());
    let mut ids = mcx::PgVec::new_in(px.mcx());
    for &(attno, value) in vals.iter() {
        // D1016 / D1630: one computed from executor parameters, or bound at bind on an inner walk, the host's at every seek;
        // D899: else an expression key is PostgreSQL's runtime key, bound when the pipeline reading it starts
        // (lifetime/bound.rs `index_key`)
        match bound::index_key(px, Feed::IndexKey, k, value, node, &mut ids) {
            KeyAt::Host(w) | KeyAt::Word(w) => slots.push_in(w),
            KeyAt::Fold => shape(node, "an index key without a parameter word (a by-reference constant, or a folded expression)"),
        }
        attnos.push_in(attno);
    }
    if crate::rerun::params_key::input_cells(px, node, &ids, &ids, vals.iter().map(|v| v.1), &mut slots, &mut attnos) {
        crate::rerun::params_key::host_keys_serial(px);
    }
    if slots.len() > limits::KEYS {
        shape(node, "an index scan with more key words and computed keys' parameters than INDEX_MAX_KEYS");
    }
    let (mut s, ctid) = index_site(px, site, *relid, &slots, &attnos, node);
    // D2665: the system columns the scan reads (V4R-88 lanes), filled from each fetched row's header (hosts heap_fetch.rs `project`)
    crate::ops::scan::heap::add_sys_lanes(px, &mut s, node);
    // the RowId lane after the attributes: the host's N-row windows, each row's heap tid (V4R-55)
    s.add_lane(LaneDesc::rowid(), false);
    let rd = Reader::new(px, &s, *relid, node, Some(slots), ctid.then_some(CTID_AT));
    px.set_site(k, s);
    mcx::pg_box_dyn!(px.mcx(), Index { rd } => dyn SourceMech + 'g)
}

impl SourceMech for Index<'_> {
    fn declare(&mut self, pb: &mut ProgramBuilder) {
        self.rd.declare(pb);
    }

    /// The decomposer's descriptor: an index walk runs serial on the leader in index order, unless the decomposer made it
    /// eligible (a walk the pool reads), whose pipeline then follows the heap's rule: a hosted call a pool thread cannot run
    /// keeps it on the leader (D3246).
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        let d = px.gx.pipelines()[k].desc;
        if d.is_eligible() { crate::ops::scan::heap::pipeline_desc(px, k) } else { d }
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

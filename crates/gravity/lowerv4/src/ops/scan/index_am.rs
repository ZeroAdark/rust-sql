//! The any-AM index source (cards/M2.md L5): a hash / GiST / SP-GiST walk through `index_getnext_slot` (hosts
//! `IndexSource` with its any-AM walk, hosts v4/m2_index.rs `AmWalk`), a lossy qual rechecked on the heap tuple where the
//! AM asks (`xs_recheck`: the plan's indexqualorig, compiled at BIND), and ORDER BY distance (KNN) walks through
//! PostgreSQL's reorder queue (`xs_orderbyvals`, `xs_recheckorderby`: nodeIndexscan.c IndexNextWithReorder). An index-only
//! walk of these AMs reads the index tuple as src.index_only does (the AMs publish `xs_itup`), in N-row windows. The
//! key words are the clause keys' values in clause order (a null test has none), then the ORDER BY arguments (hosts
//! `SiteRecipe::words`). D1016 row 4 (V4R-557): a qual operand or ORDER BY argument computed from executor parameters is
//! the host's cell with its parameters as input cells (params_key.rs; exec / hosts v4/m8_param_key.rs over M2's recipe). No
//! declared order: a KNN distance is not a column order. No old lowerer served these walks (v3 refused). Registers kind
//! `src.index_am`. Owner M2-INDEX.
use mcx::{CollectIn, GrowIn, PgBox, PgVec};
use gravity_decompose::graph::KeyForm;
use gravity_decompose::{IndexKey, Source};
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, Value};
use gravity_program::{limits, LaneDesc, Pipeline};

use crate::ops::scan::index_only::{only_site, remap, remapped, shape};
use crate::face::kinds::KindId;
use crate::face::source::{Locator, SourceMech, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::{SourceMaker, Win, WordWin};
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::site::{index_site, Reader, CTID_AT};
use crate::admit::refusal::Reason;
use crate::lifetime::bound::{self, Feed, KeyAt};
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::SrcIndexAm, &SourceMaker(make))], ..crate::ops::NONE };

struct IndexAm<'g> {
    rd: Reader<'g>,
    /// An index-only walk: `INDEX_VAR` column `n` is table attribute `index_cols[n - 1]`.
    index_cols: Option<mcx::PgVec<'g, i16>>,
}

/// The key words: each clause key's value, then each ORDER BY argument (the host binds them in that order).
fn key_words<'g, 'p>(px: &mut Px<'g, 'p>, node: i32, keys: &[IndexKey<'p>]) -> (PgVec<'g, u16>, PgVec<'g, i16>) {
    let mut vals = mcx::reserved_in(keys.len(), px.mcx());
    for pass in [false, true] {
        for key in keys.iter().filter(|k| (k.form == KeyForm::OrderBy) == pass) {
            match key.form {
                KeyForm::Op | KeyForm::AmOp | KeyForm::OrderBy => vals.push_in((key.attno, key.value)),
                // no AM but btree searches arrays natively (amsearcharray): the planner puts such keys in a bitmap scan
                KeyForm::Any | KeyForm::AmAny => shape(node, "an = ANY key on an index walk of an access method without native array keys"),
                // `column IS [NOT] NULL` (amsearchnulls): the host's SK_SEARCHNULL / SK_SEARCHNOTNULL key, no word
                KeyForm::Null | KeyForm::NotNull => {}
                KeyForm::Row => shape(node, "a row comparison key on an index walk that is not a btree"),
            }
        }
    }
    if vals.len() > limits::KEYS {
        shape(node, "an index scan with more key words than INDEX_MAX_KEYS");
    }
    let mut slots = mcx::reserved_in(vals.len(), px.mcx());
    let mut attnos = mcx::reserved_in(vals.len(), px.mcx());
    let mut ids = PgVec::new_in(px.mcx());
    let k = px.making.0;
    for &(attno, value) in vals.iter() {
        // D1016 row 4 / D1630: an AM qual's operand or an ORDER BY argument computed from executor parameters, or bound at
        // bind on an inner walk, the host's at every seek (ExecIndexEvalRuntimeKeys); D899: else an expression key is
        // PostgreSQL's runtime key, bound when the pipeline reading it starts (lifetime/bound.rs `index_key`)
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
    (slots, attnos)
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Source::Index { site, am, relid, indexid, keys, only, .. } = &p.source else { crate::refuse!(node, Reason::Bug { payload: "an any-AM index entry over another source" }) };
    if !crate::ops::scan::index::index_fetch(gx, *am, *relid) {
        shape(node, "an index scan over a relation that is not a heap table");
    }
    let (slots, attnos) = key_words(px, node, keys);
    let index_cols = only.then(|| match gx.facts.index(*indexid) {
        Some(f) => mcx::to_vec_in(f.keys(), gx.mcx),
        None => shape(node, "an index-only scan over an index the catalog does not describe"),
    });
    let (mut s, ctid) = match &index_cols {
        Some(_) => (only_site(px, site, *relid, &slots, &attnos, node), false),
        None => index_site(px, site, *relid, &slots, &attnos, node),
    };
    // D2665: the system columns the scan reads (V4R-88 lanes), filled from each fetched row's header (hosts heap_fetch.rs `project`)
    crate::ops::scan::heap::add_sys_lanes(px, &mut s, node);
    // the RowId lane after the attributes: the host's N-row windows, each row's heap tid (V4R-55)
    s.add_lane(LaneDesc::rowid(), false);
    let rd = Reader::new(px, &s, *relid, node, Some(slots), ctid.then_some(CTID_AT));
    px.set_site(k, s);
    mcx::pg_box_dyn!(px.mcx(), IndexAm { rd, index_cols } => dyn SourceMech + 'g)
}

impl<'g> SourceMech for IndexAm<'g> {
    fn declare(&mut self, pb: &mut ProgramBuilder) {
        self.rd.declare(pb);
    }

    /// The decomposer's descriptor: an index walk runs serial on the leader in index (or distance) order.
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        px.gx.pipelines()[k].desc
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        self.rd.open(b, env)
    }

    /// N rows per window (the host's `IosBatch`, V4R-55): no selection, and nulls only where a lane has a bitmap.
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let (cursor, end) = self.rd.window(b, env, l_eof);
        let w = self.rd.vals();
        Win { cursor, end, facts: WinConds { nonulls: self.rd.nonulls(b), nosel: Some(w.sel_null), ..WinConds::default() } }
    }

    fn word_win<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win) -> Option<WordWin<'e>> {
        let w = self.rd.word_win(env.mcx)?;
        Some(match &self.index_cols {
            Some(ic) => WordWin { lanes: remap(&ic[..], &w.lanes, env.mcx), valid: remap(&ic[..], &w.valid, env.mcx), plain: w.plain },
            None => w,
        })
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let mut r = self.rd.row(b, env, j, carry, next);
        if let Some(ic) = &self.index_cols {
            r.cols = remapped(&ic[..], &r.cols).collect_in(env.mcx);
        }
        r
    }

    /// A heap walk's row locator (an index-only walk fetches no row for a write).
    fn locator(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env, _row: &RowVals) -> Option<Locator> {
        if self.index_cols.is_some() {
            return None;
        }
        self.rd.locator(b)
    }
}

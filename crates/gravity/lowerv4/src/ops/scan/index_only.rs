//! The index-only source (cards/M2.md L5; KEEP lowerv3 scan/index.rs `walk` with `only` and scan/range.rs `over`'s
//! INDEX_VAR mapping; the ladder's `indexonly.rs`): a btree walk whose rows' cells come from the index tuple, the heap
//! fetched only for a tuple on a page the visibility map does not vouch for, predicate locks as PG (hosts `indexonly::next`).
//! The site keeps a RowId lane, which asks the host for N-row windows (hosts v4/m2_index.rs `IosBatch`: one row after a
//! seek, doubling per window to the rows one index page addresses; by-reference cells copied into the window's bytes).
//! The rows' `INDEX_VAR` column `n` is the lane of the index's table attribute `n` (key or INCLUDE column; an expression
//! column the decomposer refuses). Rows in index order. `ctid` (C5b-2: a CURRENT OF cursor's identity column) is the RowId
//! lane's word, the index entry's heap tid as execCurrentOf's `xs_heaptid`; the source fetches no row for a write, so no locator. Registers kind `src.index_only`. Owner M2-INDEX.
use gravity_decompose::graph::KeyForm;
use gravity_decompose::{IndexAm, IndexKey, Source};
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, Value};
use gravity_program::{limits, LaneDesc, LaneId as SiteLane, Pipeline, SiteDesc};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};

use crate::face::kinds::KindId;
use crate::face::source::{SourceMech, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::{SourceMaker, Win, WordWin};
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::site::{index_site, Reader, CTID_AT, EXEC_KEY, HOST_KEY};
use crate::admit::refusal::{Reason, ShapeWhy, Variant};
use crate::lifetime::bound::{self, Feed, KeyAt};
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::SrcIndexOnly, &SourceMaker(make))], ..crate::ops::NONE };

pub(crate) fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// The key words in the host's order (hosts `SiteRecipe::words`) as `(param slot, table attno)`: a comparison's value, a
/// row comparison's members' values in column order, an `= ANY` array (a constant's, a parameter's or an expression's)
/// bound from its word like a comparison's value (hosts `SiteRecipe::from_plan_words`, `SK_SEARCHARRAY`, V4R-108).
pub(crate) fn key_words<'g, 'p>(px: &mut Px<'g, 'p>, node: i32, indexid: u32, keys: &[IndexKey<'p>]) -> (PgVec<'g, u16>, PgVec<'g, i16>) {
    let gx = px.gx;
    // (attno, value, whether the value is an `= ANY` key's array: its parameters are no `scalar` input, params_key.rs)
    let mut vals = mcx::reserved_in(keys.len(), px.mcx());
    for key in keys.iter() {
        match key.form {
            KeyForm::Op => vals.push_in((key.attno, Some(key.value), false)),
            KeyForm::Row => {
                let Some(rc) = key.value.as_row_compare_expr() else { crate::refuse!(node, Reason::Bug { payload: "a row comparison index key over another node" }) };
                let idx = gx.facts.index(indexid);
                for (l, r) in rc.largs.iter().zip(rc.rargs.iter()) {
                    let col = l.as_var().map_or(0, |v| v.varattno as usize);
                    let attno = idx.as_ref().filter(|i| (1..=i.nkeys as usize).contains(&col)).map_or(0, |i| i.keys[col - 1]);
                    vals.push_in((attno, Some(r), false));
                }
            }
            // an array, a constant's too (V4R-108: the recipe never holds the image, so one program serves every literal
            // list of its shape): its word bound into the key cell at the seek (hosts V4R-44, `from_plan_words`); D1016 row 2:
            // one computed from executor parameters is the host's cell below (PG's SK_SEARCHARRAY runtime key, evaluated at
            // every rescan: exec / hosts v4/m8_param_key.rs), the seek binding the evaluated array as a parameter's
            KeyForm::Any => vals.push_in((key.attno, Some(key.value), true)),
            KeyForm::AmAny => crate::refuse!(node, Reason::Bug { payload: "an AM operator = ANY key on a btree walk" }),
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
    let (mut ids, mut scalar) = (PgVec::new_in(px.mcx()), PgVec::new_in(px.mcx()));
    let k = px.making.0;
    for &(attno, value, array) in vals.iter() {
        // D1016 / D1630: one computed from executor parameters, or bound at bind on an inner walk, the host's at every seek
        // (lifetime/bound.rs `index_key`), an `= ANY` array too and beside an `= ANY` key (row 2: M8's recipe binds the
        // other arrays from their words); the parameters a comparison or a row member reads are `scalar` too
        // (params_key.rs `input_cells`), in `ids`' order. D899: else an expression key is PostgreSQL's runtime key, bound
        // when the pipeline reading it starts
        let feed = if array { Feed::ArrayKey } else { Feed::IndexKey };
        match value.map(|v| (v, bound::index_key(px, feed, k, v, node, &mut ids))) {
            None => slots.push_in(HOST_KEY),
            Some((v, KeyAt::Host(w))) => {
                slots.push_in(w);
                if !array {
                    crate::rerun::params_key::key_ids_in(v, &mut scalar);
                }
            }
            Some((_, KeyAt::Word(w))) => slots.push_in(w),
            Some((_, KeyAt::Fold)) => shape(node, "an index key without a parameter word (a by-reference constant, or a folded expression)"),
        }
        attnos.push_in(attno);
    }
    let host = crate::rerun::params_key::input_cells(px, node, &ids, &scalar, vals.iter().filter_map(|v| v.1), &mut slots, &mut attnos);
    if host {
        crate::rerun::params_key::host_keys_serial(px);
    }
    // row 2's forms (a host key and an `= ANY` key; row B's alone stay as landed): the no-widen refusals
    if host && vals.iter().any(|v| v.2) {
        crate::rerun::params_key::no_widen(px.gx, node);
    }
    if slots.len() > limits::KEYS {
        shape(node, "an index scan with more key words and computed keys' parameters than INDEX_MAX_KEYS");
    }
    (slots, attnos)
}

struct IndexOnly<'g> {
    rd: Reader<'g>,
    /// `INDEX_VAR` column `n` (1-based) is table attribute `index_cols[n - 1]` (0: an expression column, never read).
    index_cols: PgVec<'g, i16>,
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Source::Index { site, am, relid, indexid, keys, only: true, iam, .. } = &p.source else { crate::refuse!(node, Reason::Bug { payload: "an index-only entry over another source" }) };
    if !crate::ops::scan::index::index_fetch(gx, *am, *relid) {
        shape(node, "an index scan over a relation that is not a heap table");
    }
    if *iam != IndexAm::Btree {
        crate::refuse!(node, Reason::Bug { payload: "an index-only walk of another access method (graph.rs routes it to src.index_am)" });
    }
    let Some(idx) = gx.facts.index(*indexid) else { shape(node, "an index-only scan over an index the catalog does not describe") };
    let index_cols = mcx::to_vec_in(idx.keys(), px.mcx());
    let (slots, attnos) = key_words(px, node, *indexid, keys);
    let ctid = site.lanes().iter().any(|l| l.id == SiteLane::RowId);
    let mut s = only_site(px, site, *relid, &slots, &attnos, node);
    if s.lanes().iter().any(|l| !matches!(l.id, SiteLane::Attr(_))) {
        crate::refuse!(node, Reason::Bug { payload: "an index-only site with a lane that is not a table column" });
    }
    // the RowId lane after the attributes: the host's N-row windows (each row's heap tid)
    s.add_lane(LaneDesc::rowid(), false);
    let rd = Reader::new(px, &s, *relid, node, Some(slots), ctid.then_some(CTID_AT));
    px.set_site(k, s);
    mcx::pg_box_dyn!(px.mcx(), IndexOnly { rd, index_cols } => dyn SourceMech + 'g)
}

/// An index-only walk's site (L0-i's index_site). A scan reading no column (count(*), EXISTS) publishes no attribute
/// lane: the RowId lane `make` adds drives the host's N-row windows (hosts index_window.rs `IosBatch::fill` over no
/// column: each row's heap tid and the row count), as the bitmap site's offset lane does, so nothing is copied per row
/// and an index of expressions alone serves too (random rnd-3147: count(*) over `lower(c_name)`, the smallest index).
pub(crate) fn only_site(px: &Px<'_, '_>, site: &SiteDesc, relid: u32, slots: &[u16], attnos: &[i16], node: i32) -> SiteDesc {
    if site.lanes().iter().any(|l| matches!(l.id, SiteLane::Attr(_))) {
        return index_site(px, site, relid, slots, attnos, node).0;
    }
    // the key words as index_site binds them (an executor parameter's slot is the host's -1)
    let params: PgVec<'_, i16> = slots.iter().map(|&w| if w & EXEC_KEY != 0 { -1 } else { w as i16 }).collect_in(px.mcx());
    SiteDesc::index_source(px.arena, site.relation, site.index, attnos, &params[..], &[])
}

/// The row columns a site's attribute lanes `attnos` fill: an index-only walk's row is the index's (`INDEX_VAR` column `n`
/// is table attribute `index_cols[n - 1]`, `remap`), so its lanes' attributes as the index columns they fill; any other
/// source's row is the table's (`attnos` as they are). step/project.rs `carried` reads it: a bare entry over a column
/// no lane fills (one the consumers do not read, decompose demand.rs) passes on as no cell.
pub(crate) fn row_cols<'g>(gx: &crate::graph::Gx<'g, '_>, k: usize, attnos: PgVec<'g, i16>) -> PgVec<'g, i16> {
    let Source::Index { indexid, only: true, .. } = &gx.pipelines()[k].source else { return attnos };
    let Some(idx) = gx.facts.index(*indexid) else { return attnos };
    idx.keys().iter().enumerate().filter(|&(_, a)| *a > 0 && attnos.contains(a)).map(|(n, _)| n as i16 + 1).collect_in(gx.mcx)
}

/// `INDEX_VAR` columns from the table-attribute map: column `n` is the cell (or lane) of attribute `index_cols[n - 1]`.
pub(crate) fn remap<'m, T: Clone>(index_cols: &[i16], by_attno: &[Option<T>], mcx: mcx::Mcx<'m>) -> PgVec<'m, Option<T>> {
    remapped(index_cols, by_attno).collect_in(mcx)
}

/// `remap`'s columns in order (a row's column map collects them in the planning context).
pub(crate) fn remapped<'a, T: Clone + 'a>(index_cols: &'a [i16], by_attno: &'a [Option<T>]) -> impl Iterator<Item = Option<T>> + 'a {
    index_cols.iter().map(move |&a| usize::try_from(a).ok().and_then(|a| a.checked_sub(1)).and_then(|i| by_attno.get(i).cloned().flatten()))
}

impl<'g> SourceMech for IndexOnly<'g> {
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

    /// Up to the host's window of index rows; `nonulls` and `nosel` published (the host sets no selection).
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let (cursor, end) = self.rd.window(b, env, l_eof);
        let w = self.rd.vals();
        Win { cursor, end, facts: WinConds { nonulls: self.rd.nonulls(b), nosel: Some(w.sel_null), ..WinConds::default() } }
    }

    fn word_win<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win) -> Option<WordWin<'e>> {
        let w = self.rd.word_win(env.mcx)?;
        Some(WordWin { lanes: remap(&self.index_cols[..], &w.lanes, env.mcx), valid: remap(&self.index_cols[..], &w.valid, env.mcx), plain: w.plain })
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let mut r = self.rd.row(b, env, j, carry, next);
        let ctid = r.cols.get(CTID_AT as usize - 1).copied().flatten();
        r.cols = remapped(&self.index_cols[..], &r.cols).collect_in(env.mcx);
        if ctid.is_some() {
            r.cols.resize_in(CTID_AT as usize, None);
            r.cols[CTID_AT as usize - 1] = ctid;
        }
        r
    }
}

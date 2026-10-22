//! The bitmap heap scan (kind `src.bitmap`, cards/M2.md L5 bitmap; the logic of lowerv3 scan/bitmap.rs and the ladder's
//! `bitmap.rs`): one bitmap index scan of any AM, or a BitmapAnd / BitmapOr tree of them (`source_bitmap_tree`), whose
//! TIDBitmap the host builds at the open (work_mem, lossy pages) and walks in block order (hosts `BitmapSource`; its tree
//! form hosts v4/m2_bitmap.rs `Tree`), rechecking `bitmapqualorig` on lossy pages and where the AM asks, and counting the
//! exact / lossy pages for EXPLAIN as PostgreSQL does. The decomposer's range site becomes an index site over the probe's
//! index with the key words bound from the bind's words into the site's key cells before the seek (the core's
//! site-window reader, frame/site.rs); the site keeps a RowId lane, so a window is one heap page's visible tuples
//! (`PageBatch`, v1 `heap_scan_bitmap_next_pagebatch`), or several sparse pages' (D2988), not one row. `ctid` and the
//! locator are the row's RowId word (block, offset). Declared order none; no condition cache. EvalPlanQual is the host's (the recheck
//! of the whole `bitmapqualorig`). D1016 row 6: a leaf key computed from executor parameters is the host's cell (params_key.rs
//! `computed_key`, evaluated at every re-open as ExecReScanBitmapIndexScan's ExecIndexEvalRuntimeKeys), its parameters input
//! cells after every leaf's words (exec / hosts v4/m8_bitmap_param.rs). Registers kind `src.bitmap`. Owner M2-HEAP.
use gravity_decompose::ops::shared::scan::BitmapPlan;
use gravity_decompose::graph::{IndexKey, KeyForm};
use gravity_decompose::Source;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, ConvOp, Type, Value};
use gravity_program::{limits, LaneDesc, LaneId as SiteLane, Pipeline, SiteDesc};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};
use types_nodes::Node;

use crate::ops::scan::heap::tid_image;
use crate::face::kinds::KindId;
use crate::face::source::{Locator, SourceMech, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::{SourceMaker, Win};
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::site::{index_site, Reader, CTID_AT, EXEC_KEY};
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::lifetime::bound::{self, Feed, KeyAt};
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::SrcBitmap, &SourceMaker(make))], ..crate::ops::NONE };

pub(crate) fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// A bitmap site's key cells in the host's order (`slots` / `attnos`) and D1016 row 6's computed keys: the executor parameters
/// they read (`ids`, the input cells after every leaf's words), those a comparison or a row member reads (`scalar`: an `= ANY`
/// array's are not, params_key.rs `input_cells`) and every leaf key's value (PG evaluates a leaf's runtime keys at its rescan).
pub(crate) struct Words<'g, 'p> {
    pub(crate) slots: PgVec<'g, u16>,
    pub(crate) attnos: PgVec<'g, i16>,
    ids: PgVec<'g, i32>,
    scalar: PgVec<'g, i32>,
    vals: PgVec<'g, Node<'p>>,
}

impl<'g> Words<'g, '_> {
    pub(crate) fn new_in(mcx: mcx::Mcx<'g>) -> Self {
        Words { slots: PgVec::new_in(mcx), attnos: PgVec::new_in(mcx), ids: PgVec::new_in(mcx), scalar: PgVec::new_in(mcx), vals: PgVec::new_in(mcx) }
    }
}

/// The key words of one leaf's keys in the host's order (`SiteRecipe::from_plan_any_am`'s `words`, which the host's bitmap
/// walk binds by, as IndexSource does): a comparison's value (any AM's operator), an `= ANY` array (a constant's or a
/// parameter's word, V4R-108; D1016 row 6: a computed one the host's cell), a btree row comparison's members' values in column
/// order (the host's SK_ROW_MEMBER keys), each with its table attno (0: an expression column); a null test has no word (the
/// host's search-null key, D409).
pub(crate) fn leaf_words<'p>(px: &mut Px<'_, 'p>, node: i32, keys: &[IndexKey<'p>], k: &mut Words<'_, 'p>) {
    // D899: an expression key is PostgreSQL's runtime key, bound when the pipeline reading it starts; D1016 row 6 / D1630:
    // one computed from executor parameters, or bound at bind on an inner walk, is the host's at every (re-)open
    // (lifetime/bound.rs `index_key`)
    let q = px.making.0;
    let word = |px: &mut Px<'_, 'p>, v: Node<'p>, scalar: bool, k: &mut Words<'_, 'p>| {
        k.vals.push_in(v);
        let feed = if scalar { Feed::IndexKey } else { Feed::ArrayKey };
        match bound::index_key(px, feed, q, v, node, &mut k.ids) {
            KeyAt::Host(w) => {
                if scalar {
                    crate::rerun::params_key::key_ids_in(v, &mut k.scalar);
                }
                w
            }
            KeyAt::Word(w) => w,
            KeyAt::Fold if gravity_lower::rerun::words::has_exec(v) => crate::rerun::params::key(px.gx, node, v),
            KeyAt::Fold => shape(node, "a bitmap index key without a parameter word (a folded expression)"),
        }
    };
    for key in keys.iter() {
        let w = match key.form {
            KeyForm::Op | KeyForm::AmOp => word(px, key.value, true, k),
            // an array is a word bound at the seek, a constant's (V4R-108: never the recipe's image, so one program serves every
            // list) or a parameter's (D1277: an executor parameter's cell, read at every re-open; a computed one the host's,
            // D1016 row 6): btree searches it natively, every other AM scans once per element (D433, m2_bitmap_array.rs)
            KeyForm::Any | KeyForm::AmAny => word(px, key.value, false, k),
            KeyForm::Row => {
                let Some(rc) = key.value.as_row_compare_expr() else { crate::refuse!(node, Reason::Bug { payload: "a row comparison index key over another node" }) };
                for r in rc.rargs.iter() {
                    let w = word(px, r, true, k);
                    k.slots.push_in(w);
                    k.attnos.push_in(key.attno);
                }
                continue;
            }
            // `column IS [NOT] NULL` (D409, V4R-90): the host recipe's SK_SEARCHNULL / SK_SEARCHNOTNULL key, no word
            KeyForm::Null | KeyForm::NotNull => continue,
            KeyForm::OrderBy => shape(node, "an ordering clause in a bitmap index scan"),
        };
        k.slots.push_in(w);
        k.attnos.push_in(key.attno);
    }
    if k.slots.len() > limits::KEYS * 8 {
        shape(node, "a bitmap scan with more key words than its site holds");
    }
}

/// D1016 row 6 / D1630: the host keys' (`per_open`) parameters as input cells after every leaf's words (params_key.rs `input_cells`: its
/// InitPlan / hash-probe / bound-beside refusals, row B's `beside` / `ahead` names, D2071's declaration). D2194's no-widen
/// name (a hash or merge join in the statement) left at item 14 part 4: D2325 fixed, CAND-M39 not reproduced.
fn computed_cells(px: &mut Px<'_, '_>, node: i32, k: &mut Words<'_, '_>) {
    if !crate::rerun::params_key::input_cells(px, node, &k.ids, &k.scalar, k.vals.iter().copied(), &mut k.slots, &mut k.attnos) {
        return;
    }
    if k.slots.len() > limits::KEYS * 8 {
        shape(node, "a bitmap scan with more key words and computed keys' parameters than its site holds");
    }
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Source::Bitmap { site, am, relid, plan, morsel, .. } = &p.source else { crate::refuse!(node, Reason::Bug { payload: "a bitmap entry over another source" }) };
    if !crate::ops::scan::index::index_fetch(gx, *am, *relid) {
        shape(node, "a bitmap heap scan over a relation that is not a heap table");
    }
    if *morsel {
        return make_morsel(px, k);
    }
    let mut kw = Words::new_in(px.mcx());
    let indexid = match plan {
        BitmapPlan::Index { indexid, keys, .. } => {
            leaf_words(px, node, keys, &mut kw);
            *indexid
        }
        // a tree's site keeps the decomposer's index (exec's registry pairs a site with its graph source by relation and
        // index; the host's tree recipe opens every leaf's own index)
        tree => {
            crate::ops::scan::bitmap_tree::tree(px, node, tree, &mut kw);
            site.index
        }
    };
    computed_cells(px, node, &mut kw);
    let Words { slots, attnos, .. } = kw;
    // the decomposer's range site as an index site over the probe's index: its attribute lanes, and (below) the RowId lane
    // that asks the host for windows of whole heap pages, each row's tid in it (D2988: a window may span pages)
    let params: PgVec<'_, i16> = slots.iter().map(|&w| if w & EXEC_KEY != 0 { -1 } else { w as i16 }).collect_in(px.mcx());
    let mut d = SiteDesc::index_source(px.arena, site.relation, indexid, &attnos, &params[..], &site.out_attnos);
    for l in site.lanes().iter() {
        match l.id {
            SiteLane::Attr(_) => {
                if let Some(i) = d.lane_index(l.id) {
                    d.lanes[i] = *l;
                }
            }
            _ => {}
        }
    }
    // a walk reading no column (count(*)) keeps the RowId lane alone: the window's row count drives it, with no by-value
    // column to borrow (index_site's fallback is for one-row windows). `ctid` (the decomposer's RowId lane) is the row's
    // RowId word, never the decomposer's lane: index_site drops it
    let ctid = site.lanes().iter().any(|l| l.id == SiteLane::RowId);
    let mut s = if d.lanes().iter().any(|l| matches!(l.id, SiteLane::Attr(_))) {
        index_site(px, &d, *relid, &slots, &attnos, node).0
    } else {
        let mut s = d.copy_in(px.arena);
        s.lanes.clear();
        s.filter_lanes.clear();
        s
    };
    // D2665: the system columns the scan reads (V4R-88 lanes) before the RowId lane, filled from each row the window
    // stores (hosts bitmap_window.rs through heap_fetch.rs `project`)
    crate::ops::scan::heap::add_sys_lanes(px, &mut s, node);
    // D2988: the RowId lane last, right after the attribute lanes (the host's `PageBatch` lane `natts`): each row's heap tid
    // (hosts `tid_word`), so a window may hold the rows of several heap pages
    s.add_lane(LaneDesc::rowid(), false);
    let Some(rowid) = s.lane_index(SiteLane::RowId).filter(|&r| r + 1 == s.lanes().len()) else { crate::refuse!(node, Reason::Bug { payload: "a bitmap site whose RowId lane is not the last" }) };
    let rd = Reader::new(px, &s, *relid, node, Some(slots), ctid.then_some(CTID_AT));
    px.set_site(k, s);
    mcx::pg_box_dyn!(px.mcx(), Bitmap { rd, rowid, ctid } => dyn SourceMech + 'g)
}

/// The morsel form (decompose closure.rs `morsel_form`). The site stays the decomposer's heap range
/// site (the claim's `[lo, hi)` is a range of the leader's frozen bitmap entries, hosts bitmap_morsel.rs) and the window
/// loop is the heap source's (heap.rs `make_over`); the bitmap keys' words are allocated here as the serial form's are
/// and recorded as the site's `param_slots` (a host key -1), which exec's bitmap claims read to build the bitmap and
/// D899's `late_owner` reads to bind a runtime key as this pipeline starts. The walk keeps no recheck of its own: the
/// decomposer's `bitmapqualorig` filter tests the rows of a page that asks (step/filter.rs `guard`).
fn make_morsel<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Source::Bitmap { site, relid, plan, .. } = &p.source else { crate::refuse!(node, Reason::Bug { payload: "a bitmap entry over another source" }) };
    let mut kw = Words::new_in(px.mcx());
    match plan {
        BitmapPlan::Index { keys, .. } => leaf_words(px, node, keys, &mut kw),
        tree => {
            crate::ops::scan::bitmap_tree::tree(px, node, tree, &mut kw);
        }
    }
    if !kw.ids.is_empty() || kw.slots.iter().any(|&w| w != crate::frame::site::HOST_KEY && w & EXEC_KEY != 0) {
        crate::refuse!(node, Reason::Bug { payload: "a morsel-form bitmap key read per open" });
    }
    let mut s = site.copy_in(px.arena);
    s.param_slots = gravity_program::ArenaVec::new_in(px.arena);
    for &w in kw.slots.iter() {
        s.param_slots.push(if w == crate::frame::site::HOST_KEY { -1 } else { w as i16 }); // alloc: Program
    }
    crate::ops::scan::heap::make_over(px, k, s, *relid, node)
}

struct Bitmap<'g> {
    rd: Reader<'g>,
    rowid: usize,
    ctid: bool,
}

impl Bitmap<'_> {
    /// Row `i`'s `(block, offset)`: the RowId lane's word of the row (D2988: a window's rows may lie on several pages).
    fn tid(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env, i: Value) -> (Value, Value) {
        crate::frame::site::row_tid(b, &self.rd.vals(), self.rowid, i)
    }
}

impl SourceMech for Bitmap<'_> {
    fn declare(&mut self, pb: &mut ProgramBuilder) {
        self.rd.declare(pb);
    }

    /// The decomposer's descriptor: a bitmap walk runs on the leader (an index site; exec's `serial_over_index_sites`).
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        px.gx.pipelines()[k].desc
    }

    fn fills(&self) -> bool {
        false
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        self.rd.open(b, env)
    }

    /// Whole heap pages' rows per window (one page, or several sparse ones: D2988): no selection.
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let (cursor, end) = self.rd.window(b, env, l_eof);
        let w = self.rd.vals();
        Win { cursor, end, facts: WinConds { nonulls: self.rd.nonulls(b), nosel: Some(w.sel_null), ..WinConds::default() } }
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let mut row = self.rd.row(b, env, j, carry, next);
        // the reader's `ctid` is the row's RowId word
        if let Some(Some(c)) = self.ctid.then(|| row.cols[CTID_AT as usize - 1]) {
            let i = self.rd.row_index(b, env, j);
            let (block, off) = self.tid(b, env, i);
            row.vals[c as usize].0 = tid_image(b, block, off);
        }
        row
    }

    /// H28: the row's `(block, offset)`; the pinned page is not published.
    fn locator(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) -> Option<Locator> {
        let i = self.rd.row_index(b, env, row.at);
        let (block, off) = self.tid(b, env, i);
        Some(Locator { block: b.conv(ConvOp::Zext, Type::I64, block), offset: b.conv(ConvOp::Zext, Type::I64, off), pinned: None, rel: None })
    }
}

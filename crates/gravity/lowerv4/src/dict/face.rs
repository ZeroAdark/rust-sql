//! The codes face (plan §3.1; H09 H10 H11): dictionary-coded lanes (Rep `Code`).
//! - Per-value tables (H09): a pure function of one code lane is evaluated once per dictionary entry and read as a mask.
//! - Code representatives and late render (H10): `part << 32 | code` carried in records, ordered by in-part rank, cut per
//!   part, and rendered for survivors only.
//! - Dictionary open policy and the entries source (H11).
//!
//! Everything is keyed on lane identity (`LaneId`, so shredded path lanes qualify). A code's value is its render, so equality,
//! order and every function result are PG's over the rendered text; a code only ever stands in for its render.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;

use crate::cell::LaneId;
use crate::frame::env::Env;
use crate::frame::frame::{Mask, WordWin};
use crate::frame::px::{ExId, Px};

crate::hook_variants! {
    /// Per-value derived evaluation (H09).
    "H09" pub enum PerValue {
        /// A pure function / predicate of one code lane, evaluated once per dictionary entry and read as a 64-row word mask.
        Table = Strategy(PERCODE_TABLE),
        /// Lengths from the dictionary index.
        Length = Strategy(PERCODE_LENGTH),
        /// Verbatim text LIKE / prefix, open-coded (memmem), not fmgr.
        Memmem = Strategy(TEXT_MEMMEM),
        /// Open-coded text `=` / IN, comparing lengths first.
        TextEq = Strategy(TEXT_EQ),
        /// LIKE over a verbatim pgrc lane: one substring pass per frame and anchor, the hits mapped to rows by the offsets.
        Frame = Strategy(TEXT_FRAME),
    }
}

/// A per-value road's mechanism (registered as `PerValueImpl`).
pub trait PerValueMech: Sync {
    /// The rows of block `[base, base + 64)` within `m` whose code on `lane` passes `expr`. The value is read from the
    /// per-entry table, which is built once per dictionary.
    fn mask(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, w: &WordWin, base: Value, lane: LaneId, expr: ExId, m: Mask) -> Mask;
    /// The value of `expr` for `code`, read from the table, as `(value, null)`.
    fn value(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, lane: LaneId, expr: ExId, code: Value) -> (Value, Value);
}

pub struct PerValueImpl(pub &'static dyn PerValueMech);

crate::hook_variants! {
    /// Code representatives and late render (H10), with the code fronts (H23) and ties.
    "H10 H09 H23 -" pub enum CodeRep {
        /// `part << 32 | code` representatives in records.
        Carry = Strategy(CODE_CARRY),
        /// Only the survivors are rendered.
        Render = Strategy(CODE_RENDER),
        /// A per-part code cut before the render.
        Cut = Strategy(CODE_CUT),
        /// A per-part code front.
        FrontPart = Strategy(CODE_FRONT_PART),
        /// A dense code front, with the key derived per entry.
        FrontDense = Strategy(CODE_FRONT_DENSE),
        /// A heavy / light bound code front with a light tag, flushed per part dictionary (N2 N3, M3 decision 2).
        FrontLight = Strategy(CODE_FRONT_LIGHT),
        /// Shredded path lanes served as code lanes.
        Jsonb = Strategy(JSONB_CODES),
        /// Group by a build key the build proved unique at run time (a duplicate falls back to the full key): the dependent
        /// columns ride as a locator and are fetched for the output groups only.
        Dependent = Strategy(KEY_DEPENDENT),
        /// D3324 R3: a build row's text keys keep its code in the record and render for the output groups only.
        Carried = Strategy(KEY_CARRY),
        /// Order by in-part rank; across parts, by the render's order.
        Rank = Kind(RepCodeOrder),
    }
}

crate::hook_variants! {
    /// Dictionary open policy and the entries source (H11).
    "H11" pub enum DictPolicy {
        /// A consumer of the value set alone reads the dictionary's entries, not the rows.
        Entries = Strategy(DICT_ENTRIES),
        /// Dense / sparse / heads open, chosen by the renders expected.
        Open = Strategy(DICT_OPEN),
        /// The dictionary's entries as the source of a key-only order.
        Heads = Strategy(CODE_HEADS),
    }
}

/// code.front_part's run cache switch (codefront.rs `set_runs`; `PGRUST_GRAVITY_FRONT_RUNS`), set by the executor where
/// it registers the lowerer.
pub fn set_front_runs(on: bool) {
    crate::dict::codefront::set_runs(on);
}

/// How a site's dictionaries open (H11): whole, by point probes (block faults only, never a whole stream per part), or with
/// the heads prefaulted at a part's first granule (`SiteDesc::sparse_dicts`, `head_prefault`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DictOpenForm {
    #[default]
    Dense,
    Sparse,
    Heads,
}

/// M6 R3 (road `jsonb.codes`, `CodeRep::Jsonb`): step `j` of pipeline `k`'s compares over a shredded path read as a code
/// lane, rewritten before step/percode.rs attaches the expression (source/jsonb_codes.rs); `None` when none was.
pub struct CodeRewrite(pub fn(&mut Px<'_, '_>, usize, usize, &mut gravity_expr::Compiled<'_>) -> Option<Undo>);

/// C5 (V4R-468; jsonb.lanes' second face, source/jsonb_keys.rs): a group key `col ->> 'key'` over a declared path's
/// code lane. `path`: key program `c` of pipeline `k` as percode.table's key form over it, its code column of `cols`
/// (step/percode_key.rs `attach`); `key`: group key `key` of producer `k` as a declared path, its row column and lane;
/// `late` (W9 PERF C5 S4): the jsonb column lanes of pipeline `k`'s drafted site only its path keys read (decode.late fills
/// them where the path lane is invalid); `keyed`: key `e`'s `ex` over pipeline `k`, refused unless in the path form (the
/// `bool`) or over no such column.
pub struct PathKey {
    pub path: for<'a> fn(&mut Px<'_, '_>, usize, &gravity_expr::Compiled<'_>, &'a [(i16, u8, u8)]) -> Option<&'a (i16, u8, u8)>,
    pub key: fn(&crate::graph::Gx<'_, '_>, usize, &gravity_decompose::ops::shared::agg::KeyDecl<'_>) -> Option<(i16, u8)>,
    pub late: fn(&Px<'_, '_>, usize, &gravity_program::SiteDesc) -> u64,
    pub keyed: fn(bool, &mut Px<'_, '_>, usize, types_nodes::Node<'_>, i32, ExId) -> ExId,
}

/// The `PathKey` face where M6's jsonb.lanes is built (`node` names a missing face).
pub fn path_key(node: i32) -> Option<&'static PathKey> {
    crate::ops::strategy(crate::face::strategy::JSONB_LANES).map(|_| crate::frame::iface::need_strategy::<PathKey>(&node, crate::face::strategy::JSONB_LANES))
}

/// A rewrite's inverse, applied when no per-code root is served: the program's node / kid / site counts before it, each
/// rewritten node and the node its compare moved to (swapped back, then the appended ones dropped), the scan columns it
/// read as datums (`Source::Outer`, step/percode.rs's rule for a column not read by code), and site `k`'s shreds whose
/// first-filter claims it counted.
pub struct Undo {
    pub k: usize,
    pub len: [usize; 3],
    pub moved: Vec<(usize, usize)>,
    pub outer: Vec<usize>,
    pub claims: Vec<usize>,
}

impl Undo {
    pub fn undo(self, px: &mut Px<'_, '_>, c: &mut gravity_expr::Compiled<'_>) {
        let p = &mut c.prog;
        self.moved.iter().rev().for_each(|&(i, m)| p.nodes.swap(i, m));
        for &i in self.outer.iter() {
            if let gravity_expr::ExprNode::Var(v) = &mut p.nodes[i] {
                v.src = gravity_expr::Source::Scan;
            }
        }
        (p.nodes.truncate(self.len[0]), p.kids.truncate(self.len[1]), p.sites.truncate(self.len[2]));
        if let Some(s) = px.site_mut(self.k) {
            self.claims.iter().for_each(|&n| s.shreds[n].claims -= 1);
        }
    }
}

/// `attach` (step/percode.rs's, of step `j` of pipeline `k`) over `c` after the `jsonb.codes` rewrite (`CodeRewrite`) when
/// the road is built and on, the rewrite undone when no per-code root is served.
pub fn rewritten<'g, 'p, T>(px: &mut Px<'g, 'p>, k: usize, j: usize, c: &mut gravity_expr::Compiled<'p>, attach: impl FnOnce(&mut Px<'g, 'p>, &mut gravity_expr::Compiled<'p>) -> Option<T>) -> Option<T> {
    let r = crate::ops::strategy(crate::face::strategy::JSONB_CODES).and_then(|m| m.downcast_ref::<CodeRewrite>());
    let undo = r.filter(|_| CodeRep::Jsonb.on()).and_then(|r| (r.0)(px, k, j, c));
    let plan = attach(px, c);
    if let (None, Some(u)) = (&plan, undo) {
        u.undo(px, c);
    }
    plan
}

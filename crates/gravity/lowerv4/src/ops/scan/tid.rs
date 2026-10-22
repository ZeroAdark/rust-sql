//! The TID sources (PostgreSQL's TidScan / TidRangeScan; cards/M2.md L6 `src.tid`; KEEP lowerv3 R1b scan/tid.rs at
//! 11453f3408c). One key cell per TID clause in the plan's clause order: the value side of `ctid = v`, the array of `ctid
//! = ANY (a)`, a bound of `ctid op v`, each a word the bind evaluates once (a by-reference `tid` constant is its image's
//! pointer); `WHERE CURRENT OF`'s cell is its cursor, a word bound per execution (D422: the refcursor parameter's value,
//! else the execution's `CurrentOfExpr`; the name is never the program's, so a cached program serves every cursor). At
//! each seek the host (hosts v4/m2_tid_sample.rs `TidSource`) builds the sorted, deduplicated TID list (TidListEval: NULLs and TIDs outside the relation dropped) or the tightened range
//! (TidRangeEval: a NULL bound is no rows), fetches each row under the snapshot (TidNext, a CURRENT OF row followed to its
//! newest version) and hands it on as a one-row window whose position is the row's tid: `ctid` and the row's locator
//! (H28, `write.delete` of `DELETE ... WHERE CURRENT OF`) read it. EvalPlanQual rechecks the substituted tid against the
//! list or the range (the host's). The window loop is the core's site reader (frame/site.rs). A range walk runs serial:
//! its bounds are the key cells, not a claim. Registers kind `src.tid`. Owner M2-SOURCES.
use gravity_decompose::ops::shared::scan::TidForm;
use gravity_decompose::{Am, Source};
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, Value};
use gravity_lower::Place;
use gravity_program::{limits, ArenaVec, LaneId as SiteLane, Parallel, Pipeline, Reason as Why, SiteDesc};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};
use types_nodes::Node;

use crate::face::kinds::KindId;
use crate::face::source::{Locator, SourceMech, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::{SourceMaker, Win};
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::site::{Reader, CTID_AT, HOST_KEY};
use crate::admit::refusal::{Reason, ShapeWhy, Variant};
use crate::lifetime::bound::{self, BoundAt, Feed};
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::SrcTid, &SourceMaker(make))], ..crate::ops::NONE };

/// `ctid` (SelfItemPointerAttributeNumber): a TID key cell's attribute.
const CTID: i16 = -1;

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

struct Tid<'g> {
    rd: Reader<'g>,
}

/// The value side of one TID clause (the decomposer admitted each: `ctid = v`, `ctid = ANY (a)`, `ctid op v`, `v op ctid`).
fn value_of<'p>(q: Node<'p>, varno: i32, node: i32) -> Node<'p> {
    let is_ctid = |n: Node<'_>| n.as_var().is_some_and(|v| v.varno == varno && v.varlevelsup == 0 && v.varattno == CTID);
    if let Some(op) = q.as_op_expr().filter(|o| o.args.len() == 2) {
        return match (is_ctid(op.args.nth(0)), is_ctid(op.args.nth(1))) {
            (true, _) => op.args.nth(1),
            (_, true) => op.args.nth(0),
            _ => shape(node, "a tid clause without its ctid side"),
        };
    }
    match q.as_scalar_array_op_expr().filter(|s| s.args.len() == 2 && s.useOr && is_ctid(s.args.nth(0))) {
        Some(sa) => sa.args.nth(1),
        None => shape(node, "a tid clause that is not a comparison or an array probe of ctid"),
    }
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Source::Tid { site, am, scanrelid, relid, form, quals } = &p.source else { crate::refuse!(node, Reason::Bug { payload: "a tid entry over another source" }) };
    if *am != Am::Heap {
        shape(node, "a tid scan over a relation that is not a heap table");
    }
    // one key word per clause in the plan's clause order (hosts `TidRecipe`); CURRENT OF's is its cursor (D422)
    let mut slots = mcx::reserved_in(quals.len(), px.mcx());
    if *form == TidForm::Current {
        match px.plan.cursor(quals.nth(0)) {
            Place::Word(w) => slots.push_in(w),
            Place::Fold => shape(node, "a CURRENT OF without its cursor word (the statement's words are full)"),
            Place::Exec(_) => crate::not_built!(node, Variant::Kind(KindId::NestParam)),
        }
    } else {
        let mut ids = PgVec::new_in(px.mcx());
        // D3616: on a walk a rerun set re-opens too (a correlated SubPlan's pipeline), not only a nest inner's embedded walk
        let again = bound::reopened(px, k);
        let opens = |v: Node<'_>| again && bound::at_bind(v);
        for q in quals.iter() {
            let v = value_of(q, *scanrelid as i32, node);
            // D1630: a value bound at bind on a nest's or SubPlan's inner walk is the host's at every (re-)open too, as an
            // index key's is (lifetime/bound.rs `per_open`): PG evaluates it at each rescan, never under an empty outer
            if opens(v) {
                bound::declare(px, Feed::Tid, k, v, node, BoundAt::Rescan);
                slots.push_in(crate::rerun::params_key::computed_key(node, v, &mut ids));
                continue;
            }
            match bound::word(px, Feed::Tid, k, v, node) {
                Place::Word(w) => slots.push_in(w),
                // D1016: a value computed from executor parameters (a SubPlan's `ctid >= format('(%s,0)', p.pg)::tid`, tour-scans
                // scan-0128) is the host's at every (re-)open, as PG's TidListEval / TidRangeEval at each rescan's first fetch
                // (hosts ops/scan/tid_param.rs, exec ops/scan/param_tid.rs)
                Place::Fold if crate::rerun::params_key::computed(v) => {
                    bound::declare(px, Feed::Tid, k, v, node, BoundAt::Rescan);
                    slots.push_in(crate::rerun::params_key::computed_key(node, v, &mut ids));
                }
                Place::Fold if gravity_lower::rerun::words::has_exec(v) => crate::rerun::params::key(px.gx, node, v),
                Place::Fold => shape(node, "a tid key without a parameter word (the statement's words are full)"),
                Place::Exec(id) => {
                    let w = crate::rerun::params::exec_key(px, node, id);
                    slots.push_in(w);
                }
            }
        }
        // the host's values' parameters as input cells after the clause cells, read at every (re-)open
        if slots.contains(&HOST_KEY) {
            let vals: PgVec<'_, Node<'_>> = quals.iter().map(|q| value_of(q, *scanrelid as i32, node)).collect_in(px.mcx());
            let mut attnos = PgVec::new_in(px.mcx());
            crate::rerun::params_key::input_cells_opening(px, node, &ids, &ids, vals.iter().copied(), &mut slots, &mut attnos, opens);
        }
    }
    if slots.len() > limits::KEYS {
        shape(node, "a tid scan with more clauses than INDEX_MAX_KEYS");
    }
    let mut s = site_of(px, site, &slots);
    // D2665: the system columns the scan reads (V4R-88 lanes), filled from each fetched row's header (hosts heap_fetch.rs `project`)
    crate::ops::scan::heap::add_sys_lanes(px, &mut s, node);
    let ctid = site.lanes().iter().any(|l| l.id == SiteLane::RowId);
    let rd = Reader::new(px, &s, *relid, node, Some(slots), ctid.then_some(CTID_AT));
    px.set_site(k, s);
    mcx::pg_box_dyn!(px.mcx(), Tid { rd } => dyn SourceMech + 'g)
}

/// A key slot's plain word (below `limits::PARAM_WORDS`: the word plan refuses a statement past it) stays clear of the slot's
/// executor-key flag (frame/site.rs `EXEC_KEY`: every reader takes a word without it as plain, whatever its `INIT_KEY` bit,
/// and `HOST_KEY` sets it) and fits an `i16` param slot (V4R-522). An InitPlan's handle word past `params::init_place`'s
/// 14-bit field refuses by name where it is placed (`params::place`).
const _: () = assert!(limits::PARAM_WORDS <= crate::frame::site::EXEC_KEY as usize && limits::PARAM_WORDS - 1 <= i16::MAX as usize);

/// The TID site: the decomposer's attribute lanes over an index-shaped site (no index) with one `ctid` key cell per word;
/// the rowid lane dropped (`ctid` is the window's position).
fn site_of(px: &Px<'_, '_>, d: &SiteDesc, slots: &[u16]) -> SiteDesc {
    let mut key_attnos: ArenaVec<i16> = ArenaVec::with_capacity_in(slots.len(), px.arena);
    let mut params: ArenaVec<i16> = ArenaVec::with_capacity_in(slots.len(), px.arena);
    for &w in slots {
        key_attnos.push(CTID);
        params.push(if w & crate::frame::site::EXEC_KEY != 0 { -1 } else { w as i16 });
    }
    let mut s = SiteDesc::index_source(px.arena, d.relation, 0, &key_attnos, &params, &d.out_attnos);
    for l in d.lanes().iter().filter(|l| matches!(l.id, SiteLane::Attr(_))) {
        if let Some(i) = s.lane_index(l.id) {
            s.lanes[i] = *l;
        }
    }
    s
}

impl SourceMech for Tid<'_> {
    fn declare(&mut self, pb: &mut ProgramBuilder) {
        self.rd.declare(pb);
    }

    /// Serial always: a point list runs on the leader in tid order, and a range walk's bounds are its key cells (a
    /// claim would re-walk the range per worker); an aggregate end's breaker leaves the decomposer's desc eligible.
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        serial(px.gx.pipelines()[k].desc)
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        self.rd.open(b, env)
    }

    /// One heap row per window: no fact is published.
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let (cursor, end) = self.rd.window(b, env, l_eof);
        Win { cursor, end, facts: WinConds::default() }
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        self.rd.row(b, env, j, carry, next)
    }

    fn locator(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env, _row: &RowVals) -> Option<Locator> {
        self.rd.locator(b)
    }

    fn fills(&self) -> bool {
        false
    }
}

/// `d` read on the leader alone: the host source has no claim (sample.rs too).
pub(crate) fn serial(d: Pipeline) -> Pipeline {
    match d.parallel {
        Parallel::Eligible => Pipeline { parallel: Parallel::Ineligible { site: d.driving_site, reason: Why::Host }, ..d },
        _ => d,
    }
}


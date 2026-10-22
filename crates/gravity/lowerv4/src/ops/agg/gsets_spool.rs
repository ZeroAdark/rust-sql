//! D3054: a grouping-set input spool (kind `keyed.gsets`'s spool rows). Decompose's `gsets::Spool::run_of` closes phase
//! 0's walk as a Materialize the graph names in `spool_runs`: its fill pipeline appends each row to a record spool
//! (rerun/spool/rec.rs), OWNED (D3040: each worker appends to its own slot, exec seals them after the fill) when the
//! statement neither writes nor locks, rechecks nothing and no rerun set refills the fill; else a plain spool its one
//! filler appends to, emptied at a refill. Its readers are the set arms and later phases' re-sorts. An arm that folds
//! without order (its end a hashed or plain fold, no streamed group) reads an owned spool by record claims
//! (`nest.spool.chunks`: exec sizes them once the fill ran), wide, its driving site the fill's, nominal (no row is read
//! through it); any other reader reads the whole spool in one call on the leader, in the order the fill appended (a
//! one-worker fill's walk order).
use gravity_abi::ctx::hdr;
use gravity_decompose::graph::Op;
use gravity_decompose::Source;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::{Parallel, Pipeline, Reason as Why};
use mcx::PgBox;
use types_nodes::list::NodeList;

use crate::admit::refusal::{Reason, ShapeWhy};
use crate::face::end::EndMech;
use crate::face::source::{SourceMech, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::Win;
use crate::frame::px::Px;
use crate::frame::resume::{resume_pos, write_pos};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::graph::Gx;
use crate::rerun::spool::rec::Rec;

/// The rows one window of a reader hands on.
const WINDOW: i64 = crate::ops::shared::cte_reads::WINDOW;

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// Whether pipeline `k` fills a grouping-set input spool (decompose `PipelineGraph::spool_runs`).
pub(crate) fn fills(gx: &Gx<'_, '_>, k: usize) -> bool {
    gx.graph.spool_runs.iter().any(|l| l.0 as usize == k)
}

/// The fill's record columns: its last projection (decompose projects the run's columns right before the Materialize).
fn cols<'p>(gx: &Gx<'_, 'p>, k: usize) -> &'p NodeList<'p> {
    let p = &gx.pipelines()[k];
    match p.ops.iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None }) {
        Some(l) => l,
        None => shape(p.node_id, "a grouping-set spool's fill without its columns' projection"),
    }
}

/// Whether fill `k`'s spool is owned (filled at the fill's width, sealed once): run once, in a statement that neither
/// writes nor locks (a recheck would run the fill again) and asks no recheck.
fn owned(gx: &Gx<'_, '_>, k: usize) -> bool {
    let ps = gx.pipelines();
    let acts = ps.iter().any(|p| p.ops.iter().any(|op| matches!(op, Op::Write(_) | Op::Lock(_))));
    !acts && !gx.graph.recheck && !in_rerun(gx, k)
}

fn in_rerun(gx: &Gx<'_, '_>, k: usize) -> bool {
    gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(k as u16)))
}

/// The fill's end: each row appended to the record.
struct Fill<'g> {
    rec: Rec<'g>,
    owned: bool,
    refill: bool,
}

pub(crate) fn make_end<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let gx = px.gx;
    let node = gx.pipelines()[k].node_id;
    let (rec, _) = Rec::declare(px, k, k, node, cols(gx, k), 0);
    let owned = owned(gx, k);
    let rec = if owned { rec.owned(px, k) } else { rec };
    mcx::pg_box_dyn!(px.mcx(), Fill { rec, owned, refill: !owned && in_rerun(gx, k) } => dyn EndMech + 'g)
}

impl EndMech for Fill<'_> {
    /// An owned spool fills at the pipeline's width; a plain one has one filler.
    fn pipeline_desc(&self, _px: &Px<'_, '_>, _k: usize, d: Pipeline) -> Pipeline {
        match self.owned {
            true => d,
            false => Pipeline { parallel: Parallel::Ineligible { site: d.driving_site, reason: Why::Sink }, ..d },
        }
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        if self.refill {
            self.rec.empty(b, env);
        }
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        self.rec.append(b, env, row);
        row.carry.copy_in(env.mcx)
    }
}

/// A reader of the spool: by record claims (`chunks`: the fill's site, nominal), else the whole spool in one call.
struct Reader<'g> {
    rec: Rec<'g>,
    chunks: Option<usize>,
    rows: u64,
}

/// Whether reader `k` may read the spool by claims: it ends in a hashed or plain fold whose result no row order changes
/// (effects::visible::order_visible: no LIMIT, lock, visible call or error, an order-free fold), its ops filters, gates and
/// projections only (no streamed group, window, join, set or SubPlan).
fn claimable(gx: &Gx<'_, '_>, k: usize) -> bool {
    use gravity_abi::breaker::BreakerKind;
    let folds = matches!(crate::frame::iface::end_breaker(gx, k), Some(BreakerKind::HashAgg | BreakerKind::PlainAgg));
    folds
        && gx.pipelines()[k].ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Gate(_) | Op::Project(_)))
        && crate::effects::visible::order_visible(gx, k).is_none()
}

pub(crate) fn make_source<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    // D4620: a cascaded grouping set's producer (graph.rs `source_kind` names it keyed.gsets)
    if crate::ops::agg::cascade::at(px.gx, k).is_some() {
        return crate::ops::agg::cascade::make_source(px, k);
    }
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Source::Breaker { slot } = p.source else { shape(node, "a grouping-set spool's reader over another source") };
    let Some(f) = gx.life_of(k, slot).map(|l| l.0 as usize) else { shape(node, "a grouping-set spool's reader with no fill") };
    let rec = Rec::find(px, f, node, cols(gx, f));
    let site = px.pipeline_site(f);
    let chunks = site.filter(|_| rec.is_owned() && k < gx.pipelines().len() && claimable(gx, k));
    if chunks.is_some() {
        let (h, _) = rec.words();
        px.declare_proto(Proto::new(ProtoKind::SpoolChunks, k as u16).peer(f as u16).words(h, 1));
    }
    mcx::pg_box_dyn!(px.mcx(), Reader { rec, chunks, rows: gx.pipelines()[f].desc.predicted_rows } => dyn SourceMech + 'g)
}

impl Reader<'_> {
    /// The reader's records: this claim's (`[lo, hi)` past nothing beyond the sealed count), or the whole spool. The claim
    /// words are unsigned: a pipeline run whole (not eligible: its nominal site an index range, say) gets the whole-range
    /// claim `[0, u64::MAX)`, whose end a signed compare would read as -1 (no record).
    fn range(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> (Value, Value) {
        let n = self.rec.count(b, env);
        if self.chunks.is_none() {
            return (env.zero, n);
        }
        let lo = b.load(Type::I64, env.ctx, hdr::CLAIM_LO as i32);
        let hi = b.load(Type::I64, env.ctx, hdr::CLAIM_HI as i32);
        // both ends clamped to the count: a whole run's `[0, u64::MAX)`, a section end's `[u64::MAX, u64::MAX)` (no record)
        let (lo_past, hi_past) = (b.icmp(IntCond::Ugt, lo, n), b.icmp(IntCond::Ugt, hi, n));
        (b.select(lo_past, n, lo), b.select(hi_past, n, hi))
    }
}

impl SourceMech for Reader<'_> {
    fn open(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        env.zero
    }

    /// The records from the position (the resume word, past the range's first) to the range's end, at most `WINDOW`.
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let (lo, hi) = self.range(b, env);
        let at = resume_pos(b, env);
        let cur = b.int_bin(IntBinOp::Add, lo, at);
        let (done, l_go) = (b.icmp(IntCond::Sge, cur, hi), b.create_block(&[]));
        b.cond_br(done, (l_eof, &[env.zero]), (l_go, &[]));
        b.switch_to(l_go);
        let w = b.iconst(Type::I64, WINDOW);
        let far = b.int_bin(IntBinOp::Add, cur, w);
        let lt = b.icmp(IntCond::Slt, far, hi);
        let end = b.select(lt, far, hi);
        Win { cursor: cur, end, facts: WinConds::default() }
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        self.rec.read(b, env, j, carry, next)
    }

    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, _out: Value) {
        let lo = match self.chunks {
            Some(_) => self.range(b, env).0,
            None => env.zero,
        };
        let at = b.int_bin(IntBinOp::Sub, cur, lo);
        write_pos(b, env, at);
    }

    /// The fill's site is the claims' nominal driving site (exec reads no row through it).
    fn nominal_site(&self, _px: &Px<'_, '_>) -> Option<usize> {
        self.chunks
    }

    /// A claims reader is wide (the fill's predicted rows); any other reads on the leader.
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        let d = px.gx.pipelines()[k].desc;
        match self.chunks {
            Some(s) => Pipeline { parallel: Parallel::Eligible, driving_site: s as i16, predicted_rows: self.rows, ..d },
            None => Pipeline { parallel: Parallel::Ineligible { site: d.driving_site, reason: Why::NoDrivingSite }, ..d },
        }
    }
}

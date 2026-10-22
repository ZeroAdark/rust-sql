//! The all-blockers diagnostic mode (docs/gravity/gate.md §16; the M3 day-one dry run, kickoff ruling 09-23 item 1). With
//! env `PGRUST_GRAVITY_DIAG=1` (the exec knobs registry's row, knobs/diag.rs: read once where the lowerer is registered,
//! handed here by `set`) a lowering does not stop at its first refusal: admission records
//! every unbuilt kind and applicable road, and a pipeline maker that refuses is recorded and replaced by a placeholder, so
//! the makers after it run too. Then one `gravity: blocker …` line per record goes to the server log (stderr, unprefixed:
//! the rig ties it to the statement the way it ties the arena census), and the statement refuses with what the mode-off
//! lowering raises (admission's pick, else the first maker's refusal). A lowering with no blocker emits as always.
//! Off: one load per lowering and one test per maker; no catch, no allocation, the same IR. The record is the lowering's
//! own (`Log`, carried by `Px::diag`), never a thread's (D542: no thread-local; the session TLS census stays).
use core::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};

use gravity_decompose::{Facts, PipelineGraph};
use gravity_expr::Catalog;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, Value};
use gravity_lower::{LowerOpts, Lowered, WordPlan};
use gravity_program::ArenaRef;
use mcx::{Mcx, PgBox};

use crate::face::end::EndMech;
use crate::face::strategy::{Strategy, StrategySet};
use crate::face::source::SourceMech;
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{Next, Win};
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::graph::Gx;
use crate::admit::refusal::{raise, Reason, Refusal, Variant};
use crate::applies;

/// The process's mode: off until the executor sets it (exec knobs/lowerer.rs, from knobs/diag.rs's `PGRUST_GRAVITY_DIAG`).
static MODE: AtomicBool = AtomicBool::new(false);

/// The mode `lower_graph` lowers in.
#[inline]
pub fn on() -> bool {
    MODE.load(Ordering::Relaxed)
}

/// Set the mode for the process: the executor, once, where it registers the lowerer.
pub fn set(on: bool) {
    MODE.store(on, Ordering::Relaxed);
}

/// One recorded blocker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Blocker {
    /// The graph pipeline whose maker refused; `None` at admission.
    pub pipe: Option<usize>,
    pub stage: &'static str,
    pub refusal: Refusal,
    /// Met after an earlier blocker in its own or an earlier pipeline, and not a `NotBuilt` (which the graph alone
    /// decides): possibly the placeholder's artefact, never a statement's sole blocker.
    pub cascade: bool,
}

/// One lowering's record in the mode (`lower` owns it; `Px::diag` lends it to the makers).
#[derive(Default)]
pub struct Log {
    blockers: Vec<Blocker>,
    /// Admission's refusal (the one the mode-off lowering raises first).
    admit: Option<Refusal>,
    /// The first maker's refusal, and every pipeline a maker refused in (a NotBuilt admission already listed included).
    first: Option<Refusal>,
    blocked: Vec<usize>,
    /// `stop` raised the recorded blockers' first refusal (D788: set only then; an emission refusal after a passing
    /// `stop` is recorded like a maker's).
    stopped: bool,
    lines: Vec<String>,
    /// The applicable strategies no table provides (`applies::unbuilt_later`): not blockers, one
    /// `unbuilt_strategy_line` each for the exit report.
    later: StrategySet,
}

/// A lowering's blockers in record order and their lines (gate.md §16), as `lower` logged them.
#[derive(Default)]
pub struct Report {
    pub blockers: Vec<Blocker>,
    pub lines: Vec<String>,
}

/// `lower_graph` in the mode whatever the process's (the unit tests): the outcome and the blocker report. PLANMEM: in a
/// planning context of its own, dropped when it returns.
pub fn lower_graph_diag<'p>(arena: ArenaRef, graph: &PipelineGraph<'p>, facts: &dyn Facts, catalog: &dyn Catalog<'p>, plan: &mut dyn WordPlan<'p>, opts: LowerOpts) -> (gravity_lower::Result<Lowered>, Report) {
    let rig = gravity_driver::mem::root::Root::new("GravityPlanRig");
    let mut report = Report::default();
    (crate::lower_in(arena, rig.mcx(), graph, facts, catalog, plan, opts, Some(&mut report)), report)
}

/// The lowering in the mode (lib.rs `lower_in`): admission recorded, the program lowered past every maker's refusal;
/// the blocker lines logged into `report`; the mode-off lowering's refusal, or the program when nothing blocked.
pub fn lower<'p>(arena: ArenaRef, gx: &Gx<'_, 'p>, plan: &mut dyn WordPlan<'p>, opts: LowerOpts, report: &mut Report) -> Result<Lowered, Refusal> {
    let log = RefCell::new(Log::default());
    let r = crate::catch(gx.root_node(), || {
        let p = crate::props::Props::of(gx);
        admit(&log, gx, &p);
        crate::frame::program::lower(arena, gx, &p, plan, opts, Some(&log))
    });
    let mut l = log.into_inner();
    let r = match r {
        Err(e) if matches!(e.reason, Reason::Interrupted) => return Err(e),
        Err(e) if !l.stopped => {
            l.record(None, "lower", e);
            Err(l.admit.or(l.first).unwrap_or(e))
        }
        r => r,
    };
    l.log();
    *report = Report { blockers: l.blockers, lines: l.lines };
    r
}

/// Every unbuilt kind the graph needs and every unbuilt road that applies, in graph / road order; admission's own pick
/// kept for the end.
fn admit(log: &RefCell<Log>, gx: &Gx<'_, '_>, p: &crate::props::Props) {
    let admit = crate::catch(gx.root_node(), || applies::admit(gx, p)).err();
    let mut l = log.borrow_mut();
    l.admit = admit;
    for (k, node) in applies::unbuilt(p) {
        l.push(None, "admit", Refusal { reason: Variant::Kind(k).not_built(), node, native_rel: 0 }, false);
    }
    l.later = applies::unbuilt_later(p);
}

impl Log {
    /// A blocker, once per pipeline, stage, reason and node (a maker asked twice for one unbuilt road is one wall).
    fn push(&mut self, pipe: Option<usize>, stage: &'static str, refusal: Refusal, cascade: bool) {
        if !self.blockers.iter().any(|b| (b.pipe, b.stage, b.refusal) == (pipe, stage, refusal)) {
            self.blockers.push(Blocker { pipe, stage, refusal, cascade }); // alloc: Cold
        }
    }

    /// A maker's refusal: a NotBuilt admission listed is not listed again; its admission line takes the first pipeline
    /// it blocked (`pipe=`), which counts as blocked.
    fn record(&mut self, pipe: Option<usize>, stage: &'static str, r: Refusal) {
        let admitted = |b: &Blocker| b.stage == "admit" && b.refusal.reason == r.reason;
        let listed = matches!(r.reason, Reason::NotBuilt { .. }) && self.blockers.iter().any(admitted);
        if let Some(b) = self.blockers.iter_mut().filter(|b| listed && admitted(b)).find(|b| b.pipe.is_none()) {
            b.pipe = pipe;
        }
        let cascade = !matches!(r.reason, Reason::NotBuilt { .. }) && match pipe {
            Some(k) => self.blocked.iter().any(|&j| j <= k),
            None => self.first.is_some(),
        };
        self.first.get_or_insert(r);
        if let Some(k) = pipe {
            self.blocked.push(k); // alloc: Cold
        }
        if !listed {
            self.push(pipe, stage, r, cascade);
        }
    }

    /// One line per blocker, record order (gate.md §16); kept for the report.
    fn log(&mut self) {
        let n = self.blockers.len();
        for (i, b) in self.blockers.iter().enumerate() {
            let s = b.refusal.to_seam();
            let ms = match b.refusal.reason {
                Reason::NotBuilt { milestone, .. } => milestone.name(),
                _ => "-",
            };
            let pipe = b.pipe.map_or_else(|| "-".to_string(), |k| k.to_string()); // alloc: Cold
            let oid = if s.oid == 0 { String::new() } else { format!(" [{} {}]", s.oid, s.detail) }; // alloc: Cold
            let line = format!( // alloc: Cold
                "gravity: blocker pipe={pipe} node={} stage={} milestone={ms} reason={} k={}/{n} cascade={} text={}{oid}",
                b.refusal.node, b.stage, b.refusal.reason.variant(), i + 1, b.cascade as u8, s.what
            );
            eprintln!("{line}");
            self.lines.push(line); // alloc: Cold
        }
        for r in self.later.iter() {
            eprintln!("{}", unbuilt_strategy_line(r));
        }
    }
}

/// The exit-report line for unbuilt strategy `r` (applies.rs `unbuilt_later`), logged by a lowering in the mode served or
/// not; never a blocker (not in `Report::lines`).
pub fn unbuilt_strategy_line(r: Strategy) -> String {
    format!("gravity: unbuilt-strategy strategy={} text=an applicable strategy with no mechanism; the statement takes its straight form", r.name()) // alloc: Cold
}

/// A maker under the mode: its mechanism, or on a refusal the placeholder (the refusal recorded); off, the maker alone.
#[inline]
pub fn make<'a, 'g, 'p, T: ?Sized + 'g>(px: &'a mut Px<'g, 'p>, k: usize, node: i32, f: impl FnOnce(&'a mut Px<'g, 'p>) -> PgBox<'g, T>, hold: fn(Mcx<'g>) -> PgBox<'g, T>) -> PgBox<'g, T> {
    let mcx = px.mcx();
    let Some(log) = px.diag else {
        return f(px);
    };
    // a panic's Bug is reported at the root node, as the mode-off lowering's one catch (lib.rs) reports it; a raised
    // refusal keeps its own node (D542: regress-full-object_address-002's seam Bug, node 0 vs 1)
    let _ = node;
    let root = px.gx.root_node();
    match crate::catch(root, || f(px)) {
        Ok(m) => m,
        Err(r) if matches!(r.reason, Reason::Interrupted) => raise(r.node, r.reason),
        Err(r) => {
            log.borrow_mut().record(Some(k), "lower", r);
            hold(mcx)
        }
    }
}

/// After every pipeline's mechanisms are made (frame.rs `resolve`): a lowering that recorded a blocker stops here, before
/// any placeholder is emitted.
#[inline]
pub fn stop(px: &Px<'_, '_>) {
    let Some(log) = px.diag else {
        return;
    };
    let first = {
        let mut l = log.borrow_mut();
        // D788: only a stop that raises marks the record stopped (its refusal is already a blocker); a lowering that passes
        // here goes on to emit, and a refusal raised during emission is recorded by `lower` as the maker's would be
        let first = l.admit.or(l.first);
        l.stopped = first.is_some();
        first
    };
    if let Some(r) = first {
        raise(r.node, r.reason);
    }
}

/// The placeholder a refused maker leaves: holds the pipeline's place so the makers after it run; never emitted (`stop`).
pub struct Hold;

pub fn source<'g>(mcx: Mcx<'g>) -> PgBox<'g, dyn SourceMech + 'g> {
    mcx::pg_box_dyn!(mcx, Hold => dyn SourceMech + 'g)
}

pub fn step<'g>(mcx: Mcx<'g>) -> PgBox<'g, dyn StepMech + 'g> {
    mcx::pg_box_dyn!(mcx, Hold => dyn StepMech + 'g)
}

pub fn end<'g>(mcx: Mcx<'g>) -> PgBox<'g, dyn EndMech + 'g> {
    mcx::pg_box_dyn!(mcx, Hold => dyn EndMech + 'g)
}

const NEVER: &str = "diag placeholder emitted (frame.rs resolve stops a blocked lowering first)";

impl SourceMech for Hold {
    fn open(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Value {
        unreachable!("{NEVER}")
    }
    fn next_window(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _l_eof: Block) -> Win {
        unreachable!("{NEVER}")
    }
    fn row<'e>(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env<'e, '_>, _win: &Win, _j: Value, _carry: Carry<'e>, _next: Value) -> RowVals<'e> {
        unreachable!("{NEVER}")
    }
}

impl StepMech for Hold {
    fn lower<'e>(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env<'e, '_>, _row: &RowVals, _next: Next<'_, 'e>) -> Carry<'e> {
        unreachable!("{NEVER}")
    }
}

impl EndMech for Hold {
    fn lower<'e>(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env<'e, '_>, _row: &RowVals, _x: &Exits) -> Carry<'e> {
        unreachable!("{NEVER}")
    }
}

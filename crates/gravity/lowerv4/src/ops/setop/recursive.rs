//! Kind `setop.recursive`: WITH RECURSIVE's RecursiveUnion and WorkTableScan (PostgreSQL 18 nodeRecursiveunion.c,
//! nodeWorktablescan.c), run over the CTE's pulled form (`nest.cte`).
//!
//! Decompose's shape (ops/setop.rs `map_recursive_union`): the seed pipeline fills the working table (`work`), the term's
//! body pipeline fills the intermediate table (`next`) reading `work` through its WorkTableScan, and the driver (source
//! `work`, op `Iter`, end nest.cte's, pulled) hands every generation on into the CTE's store. The term's pipelines PG
//! re-executes per generation are a rerun set (`RECURSIVE_SET`) the driver owns. The two tables are nest.spool records
//! (rerun/spool/rec.rs `Rec`: a handle word and a count word each). Past `work`'s rows the driver empties `next`, re-runs
//! the term into it (`m8_rerun`: ExecReScan with `wtParam` changed), swaps the two tables' words (PG's swap of
//! `working_table` and `intermediate_table`) and hands the new generation on; an empty one ends the union.
//!
//! Exactness: PG pulls the term lazily, one row per CteScan read; the driver runs a generation's term whole when its first
//! row is wanted, except a streamed union (D2034, decompose ops/setop/gen_stream.rs: a term reading another recursive
//! union's CTE that may never end, under a reader that may stop): its term set, the body alone, is pulled a doubling
//! budget at a time (`Scan::streamed`, exec rerun `OP_RERUN_PULL`), the body's end stopping once `next` holds the
//! driver's want, and the driver reads `next` as it fills. The seed runs whole before the driver's first row, except a
//! quiet seed under a reader that may stop (D2034's seed cliff, decompose `IterDesc::seed`): its set is pulled the same
//! way (`Scan::seed_pull`), its end into `work` stopping at the end of the source window in which `work` reached the
//! driver's want (the check once per window, not per row); D2732 (D3377, D3373): or a seed that may raise or call, read
//! on the root path under a partial reader or a reader that may stop (`IterDesc::seed_exact`), pulled a row at a time,
//! its end stopping after the row that reaches the want. The whole-generation form is exact because:
//! - a term error is kept by the generation object (exec ops/setop/generation.rs, an ordinary ERROR only: `m9_gen_hold`)
//!   and raised (`m9_gen_raise`) after the rows the term appended before it, where PG's next pull raises it (CT-2's budget
//!   is 1 then);
//! - a once-run input the generations keep is kept whole (F3, decompose ops/setop.rs; D2081's rule less its spilling Sort):
//!   a Hash build (named growth in a rerun set), a plain aggregate, a Sort: a spilled one's put runs stay for the statement
//!   (D1638) and each generation's merge re-reads them, as PG's ExecReScanSort rewinds its sort; else refused;
//! - UNION (setop P7, D1378): PG's RecursiveUnion keeps a hash table of the rows it returned and drops a seed or term row
//!   already in it (`LookupTupleHashEntry(.., &isnew)`, NULLs equal). Each of the two table ends offers its row's key image
//!   to the generation object's set (`m9_gen_seen`, exec ops/setop/generation.rs `Seen`) and appends only a new row, so
//!   each generation holds only rows new to the union, in the term's order, as PG's working table does. The image is taken
//!   over the key datums in their stored form (the table's record cells), so a seed and a term row of one value image
//!   alike whatever their pipelines' cell forms; a key with no exact image (equal words not its equality: arrays,
//!   composites, ranges, ...) refuses by name. The keys ride on the union (`IterDesc::dedup`): no slot, no pipeline;
//! - decompose (ops/setop.rs `readers`, `project_set`, F3) decides on the plan what the lowering cannot run as PG does and
//!   names it (`IterDesc::wall`, refused here): a reader PG may stop before it reads the union whole (D2223: CteScans
//!   under a LIMIT, a projection, a Unique, a Sort, an Agg, a ModifyTable, a hash join's probe side, or a hash build PG runs
//!   whole are served; the CTE's store keeps every row for its readers), a read through another CTE's plan (a second
//!   union's seed or term) PG may stop before that plan's end (D2212: the lowering runs its seed whole), a
//!   target-list SRF in the seed or the term (D2312) and a once-run term input not kept whole (F3);
//! - a pull of n rows asks the program for a block, which a reader that may drop the union's rows on their way to the
//!   client (decompose's `IterDesc::thin`) may never fill over a non-terminating union: the Generation protocol's
//!   `GEN_ARG_THIN` bit makes a counted pull ask for the rows each run owes only (D2230, exec runner/admission.rs
//!   `thin_pull`); a union read through another CTE's plan the lowering runs whole first (`IterDesc::through`) is named
//!   in the protocol's `peer`, and a counted pull refuses it;
//! - each generation checks for interrupts (`m9_gen_check`, D2231), beside the frame's per-window cancel byte.
//! DEFERRED-VOL (the volatile family): the count and order of volatile or user-code calls a whole
//! generation, the seed and a once-run term input make, and where a held error sits among rows a Hash Join or a hashed
//! Agg / SetOp hands on in another order than PG's (D1383, D1384, D2045).
//! Named debt: an unstreamed generation runs whole (D2034); the CTE's store keeps every row in memory where PG's tuplestore spills
//! past work_mem (D1377, the spools' named grant growth; the two tables reuse their arenas each generation, D2035 fixed;
//! a grouping in the term reuses its owner chunks across generations, D2641 fixed in the driver's pull reattach); a
//! UNION's set is the process allocator's (fallible), not the statement's grant, as PG's table grows past work_mem.
use core::cell::Cell as Once;

use gravity_abi::status::raise;
use gravity_decompose::ops::shared::setop::{IterDesc, SetDesc, RECURSIVE_SEED_SET, RECURSIVE_SET};
use gravity_decompose::catalog::typefacts::{facts_of, KeyColl, TypeFacts};
use gravity_decompose::{Op, Source};
use gravity_expr::emit::hash::emit_key_hash;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, HostFlags, HostFn, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::setop::entries as m9;
use gravity_kernels::ops::setop::raw::{GenCheck, GenSeen};
use gravity_kernels::typesem::userfp::{UserCollideClear, UserCollideProbe};
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::{LifeId, NumTypmod, Parallel, Pipeline, Reason as Why, Rep, Until};
use mcx::{CollectIn, PgBox, PgVec};
use types_nodes::list::NodeList;

use crate::admit::refusal::{Reason, ShapeWhy};
use crate::cell::{At, Cell, CellHome, CellLife, Nullable};
use crate::face::end::EndMech;
use crate::face::kinds::{Face, KindId};
use crate::face::source::{SourceMech, WinConds};
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{Next, Win};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::graph::Gx;
use crate::rep::{CellRef, Scope};
use crate::rerun::sets::Rerun;
use crate::rerun::spool::rec::Rec;
use crate::table::keyimg::{self, KeyCell};

/// A window of the working table (the driver's and the WorkTableScan's).
const WINDOW: i64 = 4096;
/// The Generation proto's `arg`: the working table's handle word, the intermediate one's at bits 16..32, a streamed union's
/// view words at bits 32..48 (exec reads only the kernels' `GEN_ARG_LOUD` bit, D2732, and `GEN_ARG_THIN`, D2230).
const ARG_NEXT_SHIFT: u32 = 16;
const ARG_VIEW_SHIFT: u32 = 32;
/// D2034: the first pull of a streamed generation asks for this many term rows, each later pull of it for twice the last,
/// up to a window (the cost home's `STREAM_FIRST_PULL`).
const FIRST_ASK: i64 = gravity_program::cost::runtime::STREAM_FIRST_PULL;
/// The streamed driver's frame words past its cursor (`WordKey::Open` `i`): what it reads, the body's want, the next ask.
const W_PHASE: u8 = 1;
const W_WANT: u8 = 2;
const W_ASK: u8 = 3;
/// `W_PHASE`: the seed's rows are read from `work`; a generation streams into `next`; its term ended (its rest is read).
const PHASE_SEED: i64 = 0;
const PHASE_PULL: i64 = 1;
const PHASE_ENDED: i64 = 2;
/// D2034, the seed cliff (`IterDesc::seed`): the seeded driver's frame words past its cursor: the seed set's state, the seed
/// end's want, the next ask. `S_START`: the seed set not pulled yet; `S_PULL`: pulled, not ended; `S_ENDED`: the seed ended.
const W_SEED: u8 = 4;
const W_SEED_WANT: u8 = 5;
const W_SEED_ASK: u8 = 6;
const S_START: i64 = 0;
const S_PULL: i64 = 1;
const S_ENDED: i64 = 2;
const NO_SEED_SET: &str = "a recursive union whose pulled seed has no seed set (D2034)";

const INEXACT: &str = "a recursive UNION over a key whose image is not its equality (a type with no exact image: an array, a composite, a range, ...)";
const SET_FILL: &str = "an INTERSECT / EXCEPT whose rows fill a recursive union's table in its term is not built";
/// D3373 (D-e): an Append over the union under a reader that may stop, settled into its spool (`IterDesc::arms`).
const ARMS_SETTLED: &str = "a recursive union read in an Append's arm under a reader that may stop, the Append settled into its spool (it fills from every arm before its first row, where PG may never start a later arm: D2223)";
/// D3373 (:99): a later arm reading the union whose call does not close over an empty table (PG never starts it there).
const ARMS_LATER: &str = "a recursive union read in two arms of an Append under a probe an empty table may end, a later arm not closed over that table (PG never starts it; the lowering would read the union there; D2223)";
const NO_SET: &str = "a recursive term with no WorkTableScan (a constant-false term: no rerun set)";
const STREAM_SET: &str = "a streamed recursive generation whose term set runs more than its body (D2034: only the body is pulled)";
const STREAM_ARM: &str = "a streamed recursive generation whose intermediate table another pipeline than its body fills (D2034)";
const ZERO_UNCHECKED: &str = "a recursive union read whole by a Sort or an Agg under a LIMIT that may count 0 or a one-time gate whose check does not stop every reader of the union before its first pull (D2223)";

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

/// A table's role in its union.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Work,
    Next,
}

/// The union life `life` is a table of: its driver pipeline, its descriptor and the table's role.
fn union_of<'g>(gx: &Gx<'g, '_>, life: Option<LifeId>) -> Option<(usize, &'g IterDesc, Role)> {
    gx.pipelines().iter().enumerate().find_map(|(q, p)| {
        p.ops.iter().find_map(|op| match op {
            Op::SetOp(SetDesc::Iter(d)) if gx.life_of(q, d.work) == life => Some((q, d, Role::Work)),
            Op::SetOp(SetDesc::Iter(d)) if gx.life_of(q, d.next) == life => Some((q, d, Role::Next)),
            // with#170 / #171: an earlier arm of a UNION ALL term fills `next` too (graph.rs `term_arm`)
            Op::SetOp(SetDesc::Iter(d)) if life.is_some_and(|l| gx.term_arm(l.0 as usize) == Some(q)) => Some((q, d, Role::Next)),
            _ => None,
        })
    })
}

/// The row pipeline `k` hands its end or its table: its projection's entries, its LAST op (nest.cte's rule).
fn cols<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, node: i32) -> &'g NodeList<'p> {
    match gx.pipelines()[k].ops.last() {
        Some(Op::Project(cols)) => cols,
        _ => shape(node, "a recursive union's table row whose pipeline does not end in its projection"),
    }
}

/// A union's two tables and its generation object's handle word; a streamed union's view words (D2034: the driver reads
/// `work`'s seed, then each generation's `next` as its term fills it, through one handle word it points at either).
struct Gen<'g> {
    node: i32,
    work: Rec<'g>,
    next: Rec<'g>,
    g: u16,
    view: Option<Rec<'g>>,
}

/// D2312's finite form: a by-value integer literal's `word` (`args[0]` its width in bytes) is at most
/// `gravity_program::cost::expr::SRF_SMALL` in magnitude (1: small, 0: not, or a width that is no integer's).
fn srf_small(word: u64, args: &[u64; 4]) -> u64 {
    let v = match args[0] {
        2 => word as u16 as i16 as i64,
        4 => word as u32 as i32 as i64,
        8 => word as i64,
        _ => return 0,
    };
    (v.unsigned_abs() <= gravity_program::cost::expr::SRF_SMALL) as u64
}

/// The term's rerun set in the graph (decompose declared it with the body).
fn set_of(gx: &Gx<'_, '_>, d: &IterDesc, node: i32) -> usize {
    gx.graph.reruns.iter().position(|r| r.plan_id == RECURSIVE_SET && r.pipelines.contains(&d.body)).unwrap_or_else(|| shape(node, NO_SET))
}

/// The union's shape every face checks, then its tables and generation object, declared once by the first face made (found
/// again by the Generation proto's words, never by a spool's peer).
fn admit<'g>(px: &mut Px<'g, '_>, q: usize, d: &IterDesc) -> Gen<'g> {
    let gx = px.gx;
    let node = gx.pipelines()[q].node_id;
    if let Some(why) = d.wall {
        shape(node, why);
    }
    // D3373 (D2312's finite form): the constants that bound the seed's and the term's target-list set-returning
    // functions (decompose ops/setop.rs `srf_ends`) are small, a literal guard on each (the content key holds them as
    // words, so a statement of the key with a larger one lowers again and is walled here)
    for &(_, c) in gx.graph.srf_lits.iter().filter(|(id, _)| *id == node) {
        let width = c.as_const().map_or(0, |c| c.constlen as u64);
        if gravity_lower::inputs::key::lit_decide(c, srf_small, [width, 0, 0, 0]) != Some(1) {
            shape(node, gravity_decompose::ops::setop::SRF_WALL);
        }
    }
    if let Some(limit) = d.zero {
        zero_checked(px, q, limit, node);
    }
    // D3373 (D-e): an Append over the union under a reader that may stop reads it through its streamed arms only; a settled
    // union's feed reading the union's store fills the union's spool from every arm first
    let store = |k: usize, p: &gravity_decompose::PipelineNode<'_>| matches!(p.source, Source::Breaker { slot } if gx.life_of(k, slot).is_some_and(|l| l.0 as usize == q));
    if d.arms && gx.pipelines().iter().enumerate().any(|(k, p)| crate::rerun::union_arms::feed_of(p).is_some() && store(k, p)) {
        shape(node, ARMS_SETTLED);
    }
    // D3373 (:99, `IterDesc::arms_later`): the streamed arms reading the union's store, by arm index: each after the first
    // closes its call over the empty table that ends PG's join before it starts (frame/iface `later_arm_closes`)
    if d.arms_later {
        let mut held: PgVec<'_, (u16, usize)> = gx.pipelines().iter().enumerate().filter(|&(k, p)| store(k, p)).filter_map(|(k, _)| crate::frame::iface::arm_index(gx, k).map(|i| (i, k))).collect_in(px.mcx());
        held.sort_unstable();
        if held.len() < 2 || held.iter().skip(1).any(|&(_, k)| !crate::frame::iface::later_arm_closes(gx, k)) {
            shape(node, ARMS_LATER);
        }
    }
    if gx.spool_kind(q) != KindId::NestCte {
        shape(node, "a recursive union whose rows are not a CTE's");
    }
    let (Some(seed), Some(body)) = (gx.life_of(q, d.work), gx.life_of(q, d.next)) else { bug(node, "a recursive union without its two tables") };
    let (sc, bc) = (cols(gx, seed.0 as usize, node), cols(gx, body.0 as usize, node));
    if let Some(p) = px.protos.iter().find(|p| p.kind == ProtoKind::Generation && p.pipeline as usize == q) {
        let (g, w, n) = (p.words.base, (p.arg & 0xffff) as u16, (p.arg >> ARG_NEXT_SHIFT & 0xffff) as u16);
        let view = d.stream.then(|| Rec::by_words(px, (p.arg >> ARG_VIEW_SHIFT & 0xffff) as u16, node, sc));
        return Gen { node, work: Rec::by_words(px, w, node, sc), next: Rec::by_words(px, n, node, bc), g, view };
    }
    let layout = Rec::by_words(px, 0, node, sc);
    if !layout.same(&Rec::by_words(px, 0, node, bc)) || !layout.same(&Rec::by_words(px, 0, node, cols(gx, q, node))) {
        shape(node, "a recursive union whose seed and term rows differ in layout");
    }
    let (work, _) = Rec::declare(px, q, seed.0 as usize, node, sc, 0);
    let (next, _) = Rec::declare(px, q, body.0 as usize, node, bc, 0);
    rearm_resets(px, q, &[work.words().1, next.words().1], node);
    let g = px.proto_words(1);
    let loud = if d.pull_loud { gravity_kernels::ops::setop::entries::GEN_ARG_LOUD } else { 0 };
    // D2230: a reader that may drop the union's rows: a counted pull asks for the rows it owes only (exec admission `thin_pull`)
    let thin = if d.thin { gravity_kernels::ops::setop::entries::GEN_ARG_THIN } else { 0 };
    // D2034: the view's handle word and the count word after it (zeroed at bind; the driver stores the handle per window)
    let vw = if d.stream { px.proto_words(2) } else { 0 };
    let arg = work.words().0 as u64 | (next.words().0 as u64) << ARG_NEXT_SHIFT | (vw as u64) << ARG_VIEW_SHIFT | loud | thin;
    // D2230 / D2212: read through another CTE's plan the lowering runs whole first: a counted pull refuses (`peer`)
    let through = if d.through { q as u16 } else { gravity_program::protos::NONE };
    px.declare_proto(Proto::new(ProtoKind::Generation, q as u16).words(g, 1).arg(arg).peer(through));
    let view = d.stream.then(|| Rec::by_words(px, vw, node, sc));
    Gen { node, work, next, g, view }
}

/// K11.rec.l0 / .gl (`IterDesc::zero`): a Sort or an Agg under Limit `limit` (its count may be 0) reads the union whole, so
/// PG reads the union whole or, at a zero count, not at all (ExecLimit never pulls its subplan). Served only where every
/// reader of the union's store (a pipeline whose source or input is driver `q`'s life) is a graph pipeline that Limit's
/// zero check stops before it pulls a row (rerun/union_arms.rs `stops`: its feeder makes the check in its prologue, before
/// its CteScan's first pull runs the union); an embedded inner reading it, a reader a mask may skip or a Limit the feeder
/// does not check refuses by name.
fn zero_checked(px: &mut Px<'_, '_>, q: usize, limit: i32, node: i32) {
    let gx = px.gx;
    let ps = gx.pipelines();
    let reads = |p: usize| {
        let src = if let Source::Breaker { slot } = ps[p].source { Some(slot) } else { None };
        ps[p].desc.consumes.iter().copied().chain(src).any(|s| gx.life_of(p, s) == Some(LifeId(q as u16)))
    };
    let readers: PgVec<'_, usize> = (0..ps.end()).filter(|&p| p != q && reads(p)).collect_in(px.mcx());
    let node_of = |x: usize, j: usize| ps[x].node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(ps[x].node_id, |n| n.node_id);
    let checked = |op: &Op<'_>| matches!(crate::graph::step_kind(op), KindId::StepLimit | KindId::StepGate);
    let at = (0..ps.len()).find_map(|x| (0..ps[x].ops.len()).find(|&j| checked(&ps[x].ops[j]) && node_of(x, j) == limit).map(|j| (x, j)));
    let stopped = at.is_some_and(|(x, j)| !readers.is_empty() && crate::rerun::union_arms::stops(px, &readers[..], x, j));
    if !stopped {
        shape(node, ZERO_UNCHECKED);
    }
}

/// The role of a union's table counts a rescan re-arms (`rearm_resets`).
const REARM_TABLES: &str = "a recursive union's table counts, emptied at each run of the rerun set re-arming its CTE (setop_rec-0043)";

/// setop_rec-0043: the union is the plan of a CTE a rescan re-arms (rerun/sets.rs `rearming_set`: a correlated SubPlan's or
/// a nest's set re-runs the whole plan): PG's ExecReScanRecursiveUnion empties both tables when the CTE's chgParam reaches
/// it. The tables' count words (`counts`) start at zero at every run of that set, stored by its owner before the run (the
/// lifetime home's `Scope::OwnerRun`, as the CTE's store words, ops/ctescan/lower.rs `eager_resets`); the CTE's re-armed
/// pull then runs the seed into an empty working table.
fn rearm_resets(px: &mut Px<'_, '_>, q: usize, counts: &[u16], node: i32) {
    let Some(set) = crate::rerun::sets::rearming_set(px.gx, q) else { return };
    for &w in counts {
        let place = crate::lifetime::Place::Param { off: w as i32 * 8, ty: Type::I64, init: 0 };
        let d = crate::lifetime::Declared { what: REARM_TABLES, scope: crate::lifetime::Scope::OwnerRun { set: set as u16 }, place };
        if px.lifetimes.declare(d).is_err() {
            bug(node, "a recursive union's table counts declared after its re-arming set's owner captured its run (setop_rec-0043)");
        }
    }
}

/// The term's rerun set, owned by the driver `k`, declared through the rerun home (rerun/sets.rs `declare`, keyed by the
/// union's node and `RECURSIVE_SET`). Every read of a slot a member produces is the driver's or a member's.
fn declare_set(px: &mut Px<'_, '_>, k: usize, d: &IterDesc, node: i32) -> Rerun {
    let set = set_of(px.gx, d, node);
    let owner = px.gx.graph.reruns[set].node_id;
    crate::rerun::sets::declare(px, k, owner, RECURSIVE_SET).unwrap_or_else(|| shape(node, NO_SET))
}

fn word(b: &mut FunctionBuilder<'_>, env: &Env, w: u16) -> Value {
    b.load(Type::I64, env.params, w as i32 * 8)
}

fn set_word(b: &mut FunctionBuilder<'_>, env: &Env, w: u16, v: Value) {
    b.store(env.params, w as i32 * 8, v);
}

fn host(b: &mut FunctionBuilder<'_>, name: &str, addr: usize) -> HostFn {
    b.declare_host_once(name, &[Type::Ptr, Type::I64], Some(Type::I64), addr, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() })
}

/// The raise exit with the host's stashed error.
fn raise_host(b: &mut FunctionBuilder<'_>, env: &Env) {
    let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
    b.br(env.l_raise, &[e]);
}

/// A table's filler or reader runs on the leader in the plan's order (the CTE's pull runs there).
fn serial(d: Pipeline, reason: Why) -> Pipeline {
    match d.is_eligible() {
        true => Pipeline { parallel: Parallel::Ineligible { site: d.driving_site, reason }, ..d },
        false => d,
    }
}

// ----- the ends: the seed's into `work`, the body's into `next` -----

struct TableEnd<'g> {
    rec: Rec<'g>,
    /// A UNION's dedup (D1378): its keys and the generation object's handle word.
    dedup: Option<(Dedup<'g>, u16)>,
    /// D2034: the end stops resumably once its table holds the driver's want (its frame word): a streamed union's body end
    /// into `next` after the row that reaches it, a pulled seed's end into `work` at the end of that row's window.
    want: Option<(i32, WantAt)>,
}

/// D2034: where an end with a want checks it. `Row`: after each row it appends (a streamed union's body, D2732's exact
/// seed). `Window`: once per window of its source, at the window's end (a pulled seed: quiet by decompose's
/// `IterDesc::seed`, so the rows the rest of the window adds past the want raise nothing and call nothing PG would not);
/// its row loop keeps no stop exit and no load of the count and the want per row.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WantAt {
    Row,
    Window,
}

/// A recursive UNION's dedup keys (PG's `dupColIdx` / `dupCollations`): each key's column in the table's row, its type facts
/// and its collation.
struct Dedup<'g> {
    node: i32,
    keys: PgVec<'g, (i16, TypeFacts, KeyColl)>,
}

/// The union's dedup keys (`d.dedup` under `d.dedup_colls`); `None` for UNION ALL.
fn dedup_keys<'g>(gx: &Gx<'g, '_>, d: &IterDesc, node: i32) -> Option<Dedup<'g>> {
    if d.dedup.is_empty() {
        return None;
    }
    if d.dedup.len() != d.dedup_colls.len() {
        bug(node, "a recursive UNION whose dedup keys and collations differ in number");
    }
    let keys = d.dedup.iter().zip(d.dedup_colls.iter()).enumerate().map(|(i, (k, &c))| {
        let tf = facts_of(gx.facts, k.typid).unwrap_or_else(|| shape(node, "a recursive UNION key whose type the catalog does not know"));
        // with#62 / #65 / #82 / #86: a record or record[] key over plan-known columns keys by its user key (record_eq / hash_record)
        let tf = crate::rep::hosted::hash::user_record_tf(node, tf, k.opno, c, d.dedup_rows.get(i).map_or(&[][..], |r| &r[..]));
        (k.resno, tf, gx.facts.coll(c))
    });
    Some(Dedup { node, keys: keys.collect_in(gx.mcx) })
}

impl Dedup<'_> {
    /// Whether `row` is new to the union (PG's `isnew`): its key image, over the keys' stored datums (Rec's record cells),
    /// offered to generation object `g`'s set, which holds it when new. A host failure raises.
    fn fresh(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, g: u16) -> Value {
        let node = self.node;
        // D3374 (e) (record keys E1): the per-row record keys (typesem PER_ROW), whose 42804 pairs the row's image keeps
        let per_row: PgVec<'_, usize> = self.keys.iter().enumerate().filter(|(_, (_, tf, _))| tf.user != 0 && gravity_fn::typesem::keysem::user::per_row(tf.user)).map(|(i, _)| i).collect_in(env.mcx);
        if !per_row.is_empty() {
            let Some(keys) = crate::frame::env::user_keys(b, env.ctx) else { crate::refuse!(node, Reason::Bug { payload: crate::frame::env::NO_USER_KEYS }) };
            let f = b.declare_host_once("m6_user_collide_clear", &[Type::Ptr, Type::I64], Some(Type::I64), gravity_kernels::contract::entry1::<UserCollideClear> as *const () as usize, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() });
            let st = b.call_host(f, &[env.ctx, keys], Some(Type::I64)).unwrap_or(env.zero);
            let (failed, l_bad, l_ok) = (b.icmp(IntCond::Slt, st, env.zero), b.create_block(&[]), b.create_block(&[]));
            b.mark_cold(l_bad);
            b.cond_br(failed, (l_bad, &[]), (l_ok, &[]));
            b.switch_to(l_bad);
            raise_host(b, env);
            b.switch_to(l_ok);
        }
        let mut cx = env.lx(b, node);
        let cells: PgVec<'_, (Cell, Value, Value)> = self.keys.iter().enumerate().map(|(i, (resno, tf, _))| {
            let Some(cr) = row.col(*resno) else { shape(node, "a recursive UNION key past the table's row") };
            let (v, null) = crate::rep::datum(&mut cx, cr, tf);
            let life = CellLife { until: Until::Row, home: CellHome::FrameSlot };
            (Cell { rep: Rep::of_storage(tf.typlen, tf.byval), typid: tf.typid, num: NumTypmod::NONE, life, at: At::Slot(i as u16), null: Nullable::Maybe }, v, null)
        }).collect_in(env.mcx);
        let kc: PgVec<'_, KeyCell<'_>> = cells.iter().zip(&self.keys).map(|((c, v, n), (_, tf, coll))| KeyCell { cell: CellRef { cell: c, val: *v, null: Some(*n) }, tf, scope: Scope::Program, coll: *coll }).collect_in(env.mcx);
        let img = keyimg::image(&mut cx, &kc[..]);
        env.memo = cx.memo;
        if !img.exact {
            shape(node, INEXACT);
        }
        let words = &img.words.words;
        let h = emit_key_hash(b, &img.words);
        let at = b.declare_stack_slot(8 * (words.len() as u32 + 1), 8);
        let at = b.stack_addr(at);
        b.store(at, 0, h);
        for (i, &w) in words.iter().enumerate() {
            b.store(at, 8 * (i as i32 + 1), w);
        }
        if !per_row.is_empty() {
            self.collided(b, env, g, at, words.len(), &img.widths, &per_row[..]);
        }
        let f = b.declare_host_once("m9_gen_seen", &[Type::Ptr, Type::I64, Type::Ptr, Type::I64], Some(Type::I64), gravity_kernels::contract::entry3::<GenSeen> as *const () as usize, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() });
        let (g, n) = (word(b, env, g), b.iconst(Type::I64, words.len() as i64));
        let st = b.call_host(f, &[env.ctx, g, at, n], Some(Type::I64)).unwrap_or(env.zero);
        let (failed, l_bad, l_ok) = (b.icmp(IntCond::Slt, st, env.zero), b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_bad);
        b.cond_br(failed, (l_bad, &[]), (l_ok, &[]));
        b.switch_to(l_bad);
        raise_host(b, env);
        b.switch_to(l_ok);
        b.icmp(IntCond::Ne, st, env.zero)
    }

    /// D3374 (e) (record keys E1): the row's image (`1 + n` words at `at`) asked of generation object `g`'s set with each
    /// class a per-row key `per_row` collided with in its place (kernels typesem userfp.rs `UserCollideProbe`, its words at
    /// the offsets `widths` gives): PostgreSQL's table compares this row with such a held row and raises record_eq's
    /// 42804, so the row raises it too.
    fn collided(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, g: u16, at: Value, n: usize, widths: &[u8], per_row: &[usize]) {
        let offs: PgVec<'_, u64> = per_row.iter().map(|&i| widths[..i].iter().map(|&w| u64::from(w)).sum()).collect_in(env.mcx);
        let desc = b.declare_stack_slot(8 * (offs.len() as u32 + 2), 8);
        let desc = b.stack_addr(desc);
        for (i, v) in [n as u64, offs.len() as u64].into_iter().chain(offs).enumerate() {
            let v = b.iconst(Type::I64, v as i64);
            b.store(desc, 8 * i as i32, v);
        }
        let Some(keys) = crate::frame::env::user_keys(b, env.ctx) else { crate::refuse!(self.node, Reason::Bug { payload: crate::frame::env::NO_USER_KEYS }) };
        let f = b.declare_host_once("m6_user_collide_probe", &[Type::Ptr, Type::I64, Type::I64, Type::Ptr, Type::Ptr], Some(Type::I64), gravity_kernels::contract::entry4::<UserCollideProbe> as *const () as usize, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() });
        let g = word(b, env, g);
        let st = b.call_host(f, &[env.ctx, keys, g, at, desc], Some(Type::I64)).unwrap_or(env.zero);
        let (failed, l_bad, l_ok) = (b.icmp(IntCond::Slt, st, env.zero), b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_bad);
        b.cond_br(failed, (l_bad, &[]), (l_ok, &[]));
        b.switch_to(l_bad);
        raise_host(b, env);
        b.switch_to(l_ok);
    }
}

pub(super) fn make_end<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let gx = px.gx;
    let Some((q, d, role)) = union_of(gx, Some(LifeId(k as u16))) else { bug(gx.pipelines()[k].node_id, "a setop.recursive end that fills no table of a union") };
    let g = admit(px, q, d);
    let rec = match role {
        Role::Work => g.work,
        Role::Next => g.next,
    };
    let dedup = dedup_keys(gx, d, g.node).map(|k| (k, g.g));
    let want = match role {
        Role::Next if d.stream => {
            if k != d.body as usize {
                shape(g.node, STREAM_ARM);
            }
            Some((driver_word(px, q, W_WANT), WantAt::Row))
        }
        // D2034: the pulled seed's end stops at the end of the window in which `work` reached the driver's seed want; D2732
        // (D3377): a seed that may raise or call (`IterDesc::seed_exact`) after the row that reaches it
        Role::Work if d.seed.is_some() => {
            if d.seed != Some(k as u16) {
                bug(g.node, "a pulled seed's table filled by another pipeline than its seed set's member");
            }
            // D3373 (F-1): a seed SRF not known to end (`IterDesc::seed_row`) after the row too
            Some((driver_word(px, q, W_SEED_WANT), if d.seed_exact || d.seed_row { WantAt::Row } else { WantAt::Window }))
        }
        _ => None,
    };
    mcx::pg_box_dyn!(px.mcx(), TableEnd { rec, dedup, want } => dyn EndMech + 'g)
}

/// Frame word `i` of driver `q`'s source (the cursor is `i` 0), as a byte offset.
fn driver_word(px: &mut Px<'_, '_>, q: usize, i: u8) -> i32 {
    px.frame_word(WordKey::Open { face: Face::Setop, k: q, j: u64::MAX, i }) as i32 * 8
}

impl TableEnd<'_> {
    /// D2034: the body's row stored, a resumable stop once `next` holds the driver's want (the pull that resumed the body
    /// has its rows), as a pulled run's end stops at its reader's want (face/slot_run.rs).
    fn stop_at_want(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, carry: &Carry, row: &RowVals) {
        let Some((w, WantAt::Row)) = self.want else { return };
        self.stop_if_full(b, env, w, carry, row.next);
    }

    /// The resumable stop at cursor `cur` once the table holds want word `w`'s rows; the builder is left where it goes on.
    fn stop_if_full(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, w: i32, carry: &Carry, cur: Value) {
        let (n, want) = (self.rec.count(b, env), env.load_word(b, w));
        let (l_stop, l_on, full) = (b.create_block(&[]), b.create_block(&[]), b.icmp(IntCond::Sge, n, want));
        b.cond_br(full, (l_stop, &[]), (l_on, &[]));
        b.switch_to(l_stop);
        let st = b.iconst(Type::I64, gravity_abi::status::suspend(false));
        env.stop(b, carry, cur, st);
        b.switch_to(l_on);
    }
}

impl EndMech for TableEnd<'_> {
    /// PG `tuplestore_puttupleslot` into the table; a UNION's row only when new to the union.
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        let Some((dedup, g)) = &self.dedup else {
            self.rec.append(b, env, row);
            let out = b.int_bin(IntBinOp::Add, row.carry.out, env.one);
            let carry = Carry { out, accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) };
            self.stop_at_want(b, env, &carry, row);
            return carry;
        };
        let fresh = dedup.fresh(b, env, row, *g);
        let (l_add, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
        b.cond_br(fresh, (l_add, &[]), (l_join, &[row.carry.out]));
        b.switch_to(l_add);
        let memo = env.memo.set_aside();
        self.rec.append(b, env, row);
        env.memo.restore(memo);
        let out = b.int_bin(IntBinOp::Add, row.carry.out, env.one);
        b.br(l_join, &[out]);
        b.switch_to(l_join);
        let carry = Carry { out: b.block_params(l_join)[0], accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) };
        self.stop_at_want(b, env, &carry, row);
        carry
    }

    /// D2034: a pulled seed's end, past a window's rows: the resumable stop at the window's end once `work` holds the
    /// driver's want (`WantAt::Window`). Only in the row loop's window close: an input end's flush or a recheck's copy row
    /// has no window (`Env::win` is `None`) and its call ends there anyway.
    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, c: &Carry) {
        let Some((w, WantAt::Window)) = self.want else { return };
        let Some(end) = env.win.as_ref().map(|win| win.end) else { return };
        self.stop_if_full(b, env, w, c, end);
    }

    fn pipeline_desc(&self, _px: &Px<'_, '_>, _k: usize, d: Pipeline) -> Pipeline {
        serial(d, Why::Sink)
    }
}

// ----- the sources: the WorkTableScan over `work`, the driver over every generation -----

struct Scan<'g> {
    node: i32,
    gen: Gen<'g>,
    cursor: i32,
    /// The driver's term rerun set (`None`: a WorkTableScan).
    rerun: Option<Rerun>,
    /// D2034: a streamed driver's frame words (`W_PHASE`, `W_WANT`, `W_ASK`).
    stream: Option<[i32; 3]>,
    /// D2034, the seed cliff: the seed set the driver pulls and its frame words (`W_SEED`, `W_SEED_WANT`, `W_SEED_ASK`).
    seed: Option<(Rerun, [i32; 3])>,
    /// D2732 (D3377): the seed is pulled a row at a time (`IterDesc::seed_exact`).
    exact: bool,
    win: Once<bool>,
}

pub(super) fn make_source<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let &Source::Breaker { slot } = &p.source else { bug(p.node_id, "a setop.recursive source that reads no table") };
    let Some((q, d, role)) = union_of(gx, gx.life_of(k, slot)) else { bug(p.node_id, "a setop.recursive source over no union's table") };
    let gen = admit(px, q, d);
    if role != Role::Work {
        bug(gen.node, "a setop.recursive source over a union's intermediate table");
    }
    let rerun = (q == k).then(|| declare_set(px, k, d, gen.node));
    let cursor = driver_word(px, k, 0);
    let stream = (q == k && d.stream).then(|| {
        if !body_alone(px.gx, d, gen.node) {
            shape(gen.node, STREAM_SET);
        }
        [driver_word(px, k, W_PHASE), driver_word(px, k, W_WANT), driver_word(px, k, W_ASK)]
    });
    let seed = (q == k && d.seed.is_some()).then(|| {
        let set = px.gx.graph.reruns.iter().position(|r| r.node_id == gen.node && r.plan_id == RECURSIVE_SEED_SET);
        let Some(set) = set else { shape(gen.node, NO_SEED_SET) };
        let owner = px.gx.graph.reruns[set].node_id;
        let r = crate::rerun::sets::declare(px, k, owner, RECURSIVE_SEED_SET).unwrap_or_else(|| shape(gen.node, NO_SEED_SET));
        (r, [driver_word(px, k, W_SEED), driver_word(px, k, W_SEED_WANT), driver_word(px, k, W_SEED_ASK)])
    });
    mcx::pg_box_dyn!(px.mcx(), Scan { node: gen.node, gen, cursor, rerun, stream, seed, exact: d.seed_exact, win: Once::new(false) } => dyn SourceMech + 'g)
}

/// D2034: whether the term set's run is the body alone (every other member only re-armed: a CTE the set re-arms and its
/// pulled set, exec rerun's `run` list), the one form the set's pull (`OP_RERUN_PULL`) takes.
fn body_alone(gx: &Gx<'_, '_>, d: &IterDesc, node: i32) -> bool {
    let ms = &gx.graph.reruns[set_of(gx, d, node)].pipelines;
    ms.iter().all(|&q| q == d.body || crate::rerun::sets::rearm_only(gx, ms, q as usize)) && ms.contains(&d.body)
}

impl Scan<'_> {
    /// The driver past `work`'s rows: a kept term error raised; else `next` emptied, the term re-run into it (an error it
    /// raises kept when `m9_gen_hold` keeps it, else raised now), the two tables' words swapped and the new generation's
    /// first window (`l_eof` when it is empty and nothing is kept). The generation's resets are the lifetime home's
    /// (lifetime/setop.rs).
    fn generation(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, rerun: &Rerun, l_eof: Block, l_win: Block) {
        let g = word(b, env, self.gen.g);
        let gp = b.conv(ConvOp::IntToPtr, Type::Ptr, g);
        let l_raise = b.create_block(&[]);
        b.mark_cold(l_raise);
        let held = b.load(Type::I64, gp, m9::GEN_HELD as i32);
        let (is_held, l_run) = (b.icmp(IntCond::Ne, held, env.zero), b.create_block(&[]));
        b.cond_br(is_held, (l_raise, &[]), (l_run, &[]));
        b.switch_to(l_run);
        let f = host(b, "m9_gen_check", gravity_kernels::contract::entry1::<GenCheck> as *const () as usize);
        let st = b.call_host(f, &[env.ctx, g], Some(Type::I64)).unwrap_or(env.zero);
        let (stop, l_now, l_go) = (b.icmp(IntCond::Slt, st, env.zero), b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_now);
        b.cond_br(stop, (l_now, &[]), (l_go, &[]));
        b.switch_to(l_now);
        raise_host(b, env);
        b.switch_to(l_go);
        let (work, next) = (self.gen.work.words(), self.gen.next.words());
        crate::lifetime::setop::next_emptied(b, env, next.1);
        let st = rerun.status(b, env);
        let (failed, l_hold, l_swap) = (b.icmp(IntCond::Slt, st, env.zero), b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_hold);
        b.cond_br(failed, (l_hold, &[]), (l_swap, &[]));
        b.switch_to(l_hold);
        let f = host(b, "m9_gen_hold", gravity_abi::host_words!(m9::m9_gen_hold(ctx, h)));
        let st = b.call_host(f, &[env.ctx, g], Some(Type::I64)).unwrap_or(env.zero);
        let (passed, l_now, l_kept) = (b.icmp(IntCond::Slt, st, env.zero), b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_now);
        b.cond_br(passed, (l_now, &[]), (l_kept, &[]));
        b.switch_to(l_now);
        raise_host(b, env);
        b.switch_to(l_kept);
        b.br(l_swap, &[]);
        b.switch_to(l_swap);
        let c = crate::lifetime::setop::swapped(b, env, work, next, self.cursor);
        let (empty, l_end) = (b.icmp(IntCond::Eq, c, env.zero), b.create_block(&[]));
        b.cond_br(empty, (l_end, &[]), (l_win, &[env.zero]));
        b.switch_to(l_end);
        let held = b.load(Type::I64, gp, m9::GEN_HELD as i32);
        let is_held = b.icmp(IntCond::Ne, held, env.zero);
        b.cond_br(is_held, (l_raise, &[]), (l_eof, &[env.zero]));
        // the kept error, where PG's pull after the term's last good row raises it
        b.switch_to(l_raise);
        let f = host(b, "m9_gen_raise", gravity_abi::host_words!(m9::m9_gen_raise(ctx, h)));
        b.call_host(f, &[env.ctx, g], Some(Type::I64));
        raise_host(b, env);
    }
}

impl Scan<'_> {
    /// D2034, the seed cliff: the driver past the seed rows it has read while the seed set has not ended pulls it on for
    /// the next budget (`Rerun::pull`: the seed's end stops at the end of the source window in which `work` reached the
    /// want; the first pull runs the set from its start) and reads the rows it added as the next window, as PG's
    /// ExecRecursiveUnion returns each non-recursive term row as it pulls it. A pull that ends the seed sets `S_ENDED`; with
    /// no row added the driver goes on to the first generation (`l_term`), which starts only once the seed ended. The seed
    /// is quiet (decompose `IterDesc::seed`): a pull that ran past PG's rows raises nothing and calls nothing PG would not,
    /// and an error from it is raised here.
    fn seed_pull(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, seed: &Rerun, [w_state, w_want, w_ask]: [i32; 3], cursor: Value, l_term: Block, l_win: Block) {
        let state = env.load_word(b, w_state);
        let ended_at = b.iconst(Type::I64, S_ENDED);
        let (ended, l_pull) = (b.icmp(IntCond::Eq, state, ended_at), b.create_block(&[]));
        b.cond_br(ended, (l_term, &[]), (l_pull, &[]));
        b.switch_to(l_pull);
        let start_at = b.iconst(Type::I64, S_START);
        let first = b.icmp(IntCond::Eq, state, start_at);
        let first = b.conv(ConvOp::Zext, Type::I64, first);
        let pull_at = b.iconst(Type::I64, S_PULL);
        env.store_word(b, w_state, pull_at);
        // D2732 (D3377): a seed that may raise or call is pulled a row at a time (`exact`), its end stopping after the row
        let budget = if self.exact { env.one } else { env.load_word(b, w_ask) };
        let have = self.gen.work.count(b, env);
        let want = b.int_bin(IntBinOp::Add, have, budget);
        env.store_word(b, w_want, want);
        // uncapped (no window): a reader that takes the whole seed anyway pays about log2(rows) pulls, not rows / WINDOW
        // (1.2x a 1M-row seed's whole run), and the doubling keeps the rows run past PG's under 2x plus the rest of the
        // source window the want was met in; a pull adds rows or ends the seed, so the ask never nears overflow
        if !self.exact {
            let twice = b.int_bin(IntBinOp::Add, budget, budget);
            env.store_word(b, w_ask, twice);
        }
        let st = seed.pull(b, env, budget, first);
        let (failed, l_raise, l_ok) = (b.icmp(IntCond::Slt, st, env.zero), b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_raise);
        b.cond_br(failed, (l_raise, &[]), (l_ok, &[]));
        b.switch_to(l_raise);
        raise_host(b, env);
        b.switch_to(l_ok);
        let done = b.icmp(IntCond::Ne, st, env.zero);
        let (l_done, l_rows) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(done, (l_done, &[]), (l_rows, &[]));
        b.switch_to(l_done);
        env.store_word(b, w_state, ended_at);
        b.br(l_rows, &[]);
        b.switch_to(l_rows);
        let total = self.gen.work.count(b, env);
        let more = b.icmp(IntCond::Slt, cursor, total);
        b.cond_br(more, (l_win, &[cursor]), (l_term, &[]));
    }

    /// D2034, the streamed driver past the rows it has read (PG's ExecRecursiveUnion, one term row per call): while a
    /// generation streams (`PHASE_PULL`) its term set is pulled on for the next budget (`Rerun::pull`: the body stops once
    /// `next` holds the want) and the rows it added are the next window; once the seed is read (`PHASE_SEED`) or the term
    /// ended (`PHASE_ENDED`) a generation starts: a kept error raised, an ended generation that added no row ends the union,
    /// else its rows (all handed on) become `work` (PG's swap of the two tables), `next` is emptied, the interrupt checked
    /// and the term set's first pull re-arms it. A term error is kept by the generation object (`m9_gen_hold`) and raised
    /// after the rows the term added before it, at the next generation's start. A window may be empty (a pull that ended
    /// the term with no new row): the frame asks again.
    fn streamed(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, rerun: &Rerun, [w_phase, w_want, w_budget]: [i32; 3], l_eof: Block, l_win: Block) {
        let g = word(b, env, self.gen.g);
        let gp = b.conv(ConvOp::IntToPtr, Type::Ptr, g);
        let l_raise = b.create_block(&[]);
        b.mark_cold(l_raise);
        let phase = env.load_word(b, w_phase);
        // constants of every path below, made where each path starts
        let (pull_on, ended_at) = (b.iconst(Type::I64, PHASE_PULL), b.iconst(Type::I64, PHASE_ENDED));
        let (more, l_new, l_more, l_pull) = (b.icmp(IntCond::Eq, phase, pull_on), b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64]));
        b.cond_br(more, (l_more, &[]), (l_new, &[]));
        b.switch_to(l_more);
        b.br(l_pull, &[env.zero]);
        // a generation starts
        b.switch_to(l_new);
        let held = b.load(Type::I64, gp, m9::GEN_HELD as i32);
        let (is_held, l_go) = (b.icmp(IntCond::Ne, held, env.zero), b.create_block(&[]));
        b.cond_br(is_held, (l_raise, &[]), (l_go, &[]));
        b.switch_to(l_go);
        let ((wh, wn), (nh, nn)) = (self.gen.work.words(), self.gen.next.words());
        let ended = b.icmp(IntCond::Eq, phase, ended_at);
        let (w, n, wc, c) = (word(b, env, wh), word(b, env, nh), word(b, env, wn), word(b, env, nn));
        let empty = b.icmp(IntCond::Eq, c, env.zero);
        let (l_ended, l_swap) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(ended, (l_ended, &[]), (l_swap, &[]));
        b.switch_to(l_ended);
        b.cond_br(empty, (l_eof, &[env.zero]), (l_swap, &[]));
        b.switch_to(l_swap);
        let (nw, nwc, nn_h) = (b.select(ended, n, w), b.select(ended, c, wc), b.select(ended, w, n));
        set_word(b, env, wh, nw);
        set_word(b, env, wn, nwc);
        set_word(b, env, nh, nn_h);
        set_word(b, env, nn, env.zero);
        let f = host(b, "m9_gen_check", gravity_kernels::contract::entry1::<GenCheck> as *const () as usize);
        let st = b.call_host(f, &[env.ctx, g], Some(Type::I64)).unwrap_or(env.zero);
        let (stop, l_now, l_first) = (b.icmp(IntCond::Slt, st, env.zero), b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_now);
        b.cond_br(stop, (l_now, &[]), (l_first, &[]));
        b.switch_to(l_now);
        raise_host(b, env);
        b.switch_to(l_first);
        env.store_word(b, self.cursor, env.zero);
        env.store_word(b, w_phase, pull_on);
        let first_budget = b.iconst(Type::I64, FIRST_ASK);
        env.store_word(b, w_budget, first_budget);
        b.br(l_pull, &[env.one]);
        // the term set pulled for the budget
        b.switch_to(l_pull);
        let first = b.block_params(l_pull)[0];
        let budget = env.load_word(b, w_budget);
        let have = self.gen.next.count(b, env);
        let want = b.int_bin(IntBinOp::Add, have, budget);
        env.store_word(b, w_want, want);
        let twice = b.int_bin(IntBinOp::Add, budget, budget);
        let cap = b.iconst(Type::I64, WINDOW);
        let past = b.icmp(IntCond::Sgt, twice, cap);
        let next_budget = b.select(past, cap, twice);
        env.store_word(b, w_budget, next_budget);
        let st = rerun.pull(b, env, budget, first);
        let (failed, l_hold, l_ok, l_over, l_hand) = (b.icmp(IntCond::Slt, st, env.zero), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_hold);
        b.cond_br(failed, (l_hold, &[]), (l_ok, &[]));
        b.switch_to(l_hold);
        let f = host(b, "m9_gen_hold", gravity_abi::host_words!(m9::m9_gen_hold(ctx, h)));
        let kept = b.call_host(f, &[env.ctx, g], Some(Type::I64)).unwrap_or(env.zero);
        let (passed, l_now, l_kept) = (b.icmp(IntCond::Slt, kept, env.zero), b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_now);
        b.cond_br(passed, (l_now, &[]), (l_kept, &[]));
        b.switch_to(l_now);
        raise_host(b, env);
        b.switch_to(l_kept);
        b.br(l_over, &[]);
        b.switch_to(l_ok);
        let done = b.icmp(IntCond::Ne, st, env.zero);
        b.cond_br(done, (l_over, &[]), (l_hand, &[]));
        b.switch_to(l_over);
        env.store_word(b, w_phase, ended_at);
        b.br(l_hand, &[]);
        b.switch_to(l_hand);
        let cur = env.load_word(b, self.cursor);
        b.br(l_win, &[cur]);
        // the kept error, where PG's pull after the term's last good row raises it
        b.switch_to(l_raise);
        let f = host(b, "m9_gen_raise", gravity_abi::host_words!(m9::m9_gen_raise(ctx, h)));
        b.call_host(f, &[env.ctx, g], Some(Type::I64));
        raise_host(b, env);
    }

    /// The table this window reads: `work` (the seed, and every generation unstreamed), or a streamed driver's `next` past
    /// the seed, through its view's handle word; and that table's row count.
    fn table(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        let (Some(view), Some([w_phase, _, _])) = (&self.gen.view, self.stream) else { return self.gen.work.count(b, env) };
        let phase = env.load_word(b, w_phase);
        let seed_at = b.iconst(Type::I64, PHASE_SEED);
        let seed = b.icmp(IntCond::Eq, phase, seed_at);
        let (wh, nh) = (word(b, env, self.gen.work.words().0), word(b, env, self.gen.next.words().0));
        let h = b.select(seed, wh, nh);
        set_word(b, env, view.words().0, h);
        let (wc, nc) = (self.gen.work.count(b, env), self.gen.next.count(b, env));
        b.select(seed, wc, nc)
    }
}

impl SourceMech for Scan<'_> {
    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("setop.recursive cursor", Base::Words, self.cursor, Type::I64, 0));
        if let Some([phase, want, budget]) = self.stream {
            w.declare(OpenWord::open("setop.recursive streamed phase", Base::Words, phase, Type::I64, PHASE_SEED));
            w.declare(OpenWord::open("setop.recursive streamed want", Base::Words, want, Type::I64, 0));
            w.declare(OpenWord::open("setop.recursive streamed budget", Base::Words, budget, Type::I64, FIRST_ASK));
        }
        if let Some((_, [state, want, ask])) = &self.seed {
            w.declare(OpenWord::open("setop.recursive seed state", Base::Words, *state, Type::I64, S_START));
            w.declare(OpenWord::open("setop.recursive seed want", Base::Words, *want, Type::I64, 0));
            w.declare(OpenWord::open("setop.recursive seed budget", Base::Words, *ask, Type::I64, FIRST_ASK));
        }
    }

    fn open(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        env.zero
    }

    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let cursor = env.load_word(b, self.cursor);
        let total = self.table(b, env);
        let (more, l_win) = (b.icmp(IntCond::Slt, cursor, total), b.create_block(&[Type::I64]));
        match (&self.rerun, self.stream) {
            (None, _) => b.cond_br(more, (l_win, &[cursor]), (l_eof, &[env.zero])),
            (Some(r), stream) => {
                let l_gen = b.create_block(&[]);
                b.cond_br(more, (l_win, &[cursor]), (l_gen, &[]));
                b.switch_to(l_gen);
                if let Some((seed, words)) = &self.seed {
                    let l_term = b.create_block(&[]);
                    self.seed_pull(b, env, seed, *words, cursor, l_term, l_win);
                    b.switch_to(l_term);
                }
                match stream {
                    Some(w) => self.streamed(b, env, r, w, l_eof, l_win),
                    None => self.generation(b, env, r, l_eof, l_win),
                }
            }
        }
        b.switch_to(l_win);
        let c = b.block_params(l_win)[0];
        let total = self.table(b, env);
        self.win.set(true);
        let w = b.iconst(Type::I64, WINDOW);
        let far = b.int_bin(IntBinOp::Add, c, w);
        let past = b.icmp(IntCond::Sgt, far, total);
        let end = b.select(past, total, far);
        Win { cursor: c, end, facts: WinConds::default() }
    }

    /// Record `j` of the working table (by reference: the table's copy, alive to the statement's end).
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        if !self.win.get() {
            bug(self.node, "a work-table row before its window");
        }
        match (&self.gen.view, self.stream) {
            (Some(view), Some(_)) => view.read(b, env, j, carry, next),
            _ => self.gen.work.read(b, env, j, carry, next),
        }
    }

    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, _out: Value) {
        env.store_word(b, self.cursor, cur);
    }

    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        serial(px.gx.pipelines()[k].desc, Why::Host)
    }
}

// ----- the driver's Iter op: the generation's row, handed on -----

struct Iter;

pub(super) fn make_step<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    match &p.ops[j] {
        Op::SetOp(SetDesc::Iter(d)) if j == 0 => {
            admit(px, k, d);
        }
        // graph.rs pipe_kind gives every set op of a pipeline ending in the union's table to this family. D3373 (G-1): one
        // run once (in no rerun set: the seed's INTERSECT / EXCEPT, setop_recurse :220 / :221) is setop.setop's own step,
        // its rows handed on into the union's table as into any end (lower.rs `make`); one the union's term reruns at each
        // generation keeps the refusal
        Op::SetOp(SetDesc::Set { .. }) if !gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(k as u16))) => return super::lower::make(px, k, j),
        Op::SetOp(SetDesc::Set { .. }) => shape(p.node_id, SET_FILL),
        _ => bug(p.node_id, "a setop.recursive step that is not its union's Iter"),
    }
    mcx::pg_box_dyn!(px.mcx(), Iter => dyn StepMech + 'g)
}

impl StepMech for Iter {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        next(b, env, row)
    }
}

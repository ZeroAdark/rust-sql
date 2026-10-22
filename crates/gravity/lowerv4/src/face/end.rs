//! The end face (plan §3.1; H05 H27): where a pipeline's rows go. That is the out-block sink (serial, or per worker in claim
//! order), a breaker's state (fold, keyed, run, build), or a row op. The v3 trait carries over (lowerv3-final
//! emit/frame.rs), with the sink form declared (`sink`). The accumulators an end carries in registers (`carried`) ride the
//! row loop's carry, and `frame.carry` (step.rs `LoopRoad`) keeps them there per window. End kinds (face End, the keyed and
//! run ends, builds, row ops) are made by `frame::EndMaker`. Closed at M1's exit.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, Value};
use mcx::{PgBox, PgVec};

use gravity_program::{Pipeline, WritePlan};

use crate::frame::env::Env;
use crate::frame::frame::{Mask, Next, Win, WordWin};
use crate::frame::open::OpenWords;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;

crate::hook_variants! {
    /// The sink form an end declares (H05 a, H27).
    "H05 H27" pub enum SinkKind {
        /// The serial out-block sink.
        Serial = Kind(EndSink),
        /// (H05 a) per-worker out blocks streamed in claim order (driver R1D).
        Ordered = Strategy(SINK_ORDERED),
        /// (H27) a row op: DELETE here, and the other write kinds on the same end (write/rowop.rs).
        RowOp = Kind(WriteDelete),
    }
}

crate::strategies! {
    /// Parallel outer, per-worker inner state. No mechanism yet: its readers take the straight form. D4532: switched off
    /// (`gravity_road_off`), a nest over an eager spool keeps to the leader (ops/nestloop/widen.rs `spool`); D3014O's
    /// probe widening does not read it.
    NEST_PARALLEL = 98, "nest.parallel", Nest;
}

crate::hook_variants! {
    /// Parallel sinks and outers (H05).
    "H05" pub enum Parallel {
        /// (a) per-worker out blocks in claim order. LIMIT stops claims, and pool threads are pruned (D234).
        Ordered = Strategy(SINK_ORDERED),
        /// (b) a parallel outer with per-worker inner state, for nests.
        Outer = Strategy(NEST_PARALLEL),
        /// UNION ALL arms run in parallel and are answered per arm.
        Arms = Strategy(APPEND_ARMS),
        /// D4569: a SELECT's quiet union nest copied into each arm, so the arms stream through it (on every worker when D4532
        /// widens a copy) where the union kept its Materialize form.
        ArmNest = Strategy(APPEND_NEST),
        /// D4569: a spooled drain's quiet probe fills an owned spool on every worker, where it filled the spool on the leader.
        DrainWide = Strategy(DRAIN_SPOOL_WIDE),
        /// D4628: an owned spool sealed off the leader's copy (one owner's region lent; several owners' records copied by
        /// the pool's threads), where the leader copied every owner's records alone.
        SpoolSeal = Strategy(SPOOL_SEAL),
        /// SRF pipelines stay parallel.
        Srf = Strategy(SRF_PARALLEL),
    }
}

crate::hook_variants! {
    /// What a row-op end declares per op (H27; H16 a / b for its bind-time forms).
    "H27 H16" pub enum RowOpRoad {
        /// The index plan: `indexUnchanged` per index on a non-HOT update (D235).
        IdxUnchanged = Strategy(WRITE_IDXUNCHANGED),
        /// The checks plan in IR: checks, generated columns, partial and expression index keys.
        Checks = Strategy(WRITE_CHECKS),
        /// Bind-time cells and a per-execution literal table.
        Literals = Strategy(WRITE_LITERALS),
        /// Partition routing on key lanes with bound leaves.
        Route = Strategy(WRITE_ROUTE),
    }
}

/// A row op's answer per row (H27): the host op's status and the branch the end takes on it. An error leaves with the
/// rows finished before it counted (the ENTRY §E3 count).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowStatus {
    /// The row was written: counted, with RETURNING projected.
    Done,
    /// The row was not written and is not counted: it was deleted concurrently, this command already modified it, or a
    /// BEFORE trigger returned NULL.
    Skipped,
    /// Updated concurrently. The EvalPlanQual recheck runs, resuming on the row version through its resume word
    /// (`stop::ResumeWord::EpqRecheck`, H17 b).
    Recheck,
    /// ON CONFLICT found an existing row: DO UPDATE on it, or DO NOTHING.
    Conflict,
}

/// Where a row op finds its target row's position (H28; M4-plan §1.3 L0-b).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocFrom {
    /// The driving source's locator (`Env::loc`): a write over the target's own scan (M1's DELETE).
    Driving,
    /// The target's junk ctid cell carried through a join or an emit (UPDATE ... FROM, DELETE ... USING, MERGE): the kind
    /// hands its datum to each call (`RowOpRow::tid`), and an EvalPlanQual recheck names `site`, the target's scan site.
    JunkCtid { site: usize },
    /// No target row (INSERT): the write site's position stays empty.
    None,
}

/// What one row op declares (L0-a): the host op (`WRITE_INSERT` / `UPDATE` / `DELETE`, abi program.rs), the target, the
/// cells its write site carries (0: the position alone), the frozen write plan (`with_conflict` / `with_route` set by the
/// lowerer), where the position comes from, and whether ON CONFLICT's conflict site (the write site + 1) is added.
pub struct RowOpDecl {
    pub op: u32,
    pub relid: u32,
    pub natts: u16,
    pub plan: WritePlan,
    pub loc: LocFrom,
    pub conflict: bool,
}

/// One call's row: the target's cells in attno order as `(datum, valid)` (`natts` of them, none for a DELETE), and the
/// junk ctid's `(datum, valid)` under `LocFrom::JunkCtid`. mergecmd G2(c): `rel`, a write of several result relations
/// whose rows come from one settled union of their feeds: the row's relation (its junk tableoid, which the multi-target
/// host's `leaf_of` takes from the position) and the recheck code naming that relation's feed scan site, both per row.
pub struct RowOpRow<'a> {
    pub cells: &'a [(Value, Value)],
    pub tid: Option<(Value, Value)>,
    pub rel: Option<(Value, Value)>,
}

/// `call_op`'s extra status arm: ON CONFLICT's on `Conflict`, or (M8-WRITE W6) a MERGE MATCHED action's on
/// `WRITE_NOT_MATCHED` (its target row concurrently deleted): its NOT MATCHED actions, a skip without them.
pub enum Arm<'a, 'e> {
    None,
    Conflict(Next<'a, 'e>),
    Merge(Option<Next<'a, 'e>>),
    /// D655 Row C (V4R-534): WRITE_RECHECK of an op that rechecks in the IR (`RowOpMech::epq_in_ir`): the kind's recheck and
    /// its cells over the passing version (UPDATE's new tuple, DELETE's none), then the op again (write/rowop_epq.rs).
    Epq(&'a dyn EpqRerun, &'a mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env<'e, '_>, &RowVals) -> PgVec<'e, (Value, Value)>),
    /// mergecmd D655 Row C: a MERGE MATCHED action of a write that rechecks in the IR (merge/epq.rs): `Merge`'s arm, and
    /// WRITE_RECHECK enters the MERGE's redo loop at the block (its WHEN MATCHED list again over the newest version).
    MergeEpq(Option<Next<'a, 'e>>, Block),
}

/// D655 Row C: a row-op kind's EvalPlanQual over the newest version the host put in write site `site` (write/epq_ir.rs):
/// `retry` with the row the source's recheck quals and the ops below the write rebuilt, or the carry as it came (skipped).
pub trait EpqRerun {
    fn rerun<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, site: usize, node: i32, retry: Next<'_, 'e>) -> Carry<'e>;
}

/// The row op as a step (H27, L0-g): the write kinds call the host in their step and hand the written row on, so a
/// RETURNING projection after the op reads the row as written. `write/rowop.rs` implements it; M4's kinds reach it with
/// `registry::need_kind::<RowOpMaker>(px, KindId::WriteDelete)`.
pub trait RowOpMech {
    /// At the pipeline's open: the ModifyTable node's EXPLAIN ANALYZE loop, an empty position for `LocFrom::None`.
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env);
    /// Store the cells and the position, call the host, branch on its status: `RowStatus::Done` continues with `done`,
    /// `Skipped` with nothing (the carry as it came), `Conflict` with `conflict` (without one the status is unsupported),
    /// `Recheck` stops after the row naming the recheck site, an error stops at the row.
    fn call<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, r: RowOpRow<'_>, done: Next<'_, 'e>, conflict: Option<Next<'_, 'e>>) -> Carry<'e>;
    /// `call` with this call's host op in place of the declared one: MERGE's action arms (`WRITE_INSERT` / `UPDATE` /
    /// `DELETE`) share one write site (M4-TRIG T2); `arm` its extra status arm.
    fn call_op<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, op: u32, r: RowOpRow<'_>, done: Next<'_, 'e>, arm: Arm<'_, 'e>) -> Carry<'e>;
    /// The write site's cells after `Done` (the host's `written()` applied: a BEFORE ROW trigger's row), or the conflict
    /// site's (the existing row) under `conflict`: `(datum, isnull)` in attno order.
    fn cells<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, conflict: bool) -> PgVec<'e, (Value, Value)>;
    /// M4-RET R4a (V4R-286): after `Done` of an UPDATE whose plan fetches the version it replaced (`WritePlan::with_old`,
    /// RETURNING OLD), the old site's cells (the write site's successor, driver dml.rs): `(datum, isnull)` in attno order.
    fn old_cells<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> PgVec<'e, (Value, Value)> {
        let _ = b;
        PgVec::new_in(env.mcx)
    }
    /// The write site's number (the conflict site is the next one), placed at emit after every source site: read it
    /// while lowering, never at make time.
    fn site(&self) -> usize;
    /// D655 Row C: WRITE_RECHECK reruns in the IR (the claim admitted it: write/rowop_epq.rs), so the kind hands `Arm::Epq`.
    fn epq_in_ir(&self) -> bool {
        false
    }
    /// The ModifyTable plan node the op writes for (EXPLAIN ANALYZE's counts of its rows' outcomes); `-1` for none.
    fn write_node(&self) -> i32 {
        -1
    }
}

pub struct RowOpMaker(pub for<'g, 'p> fn(&mut Px<'g, 'p>, usize, RowOpDecl) -> PgBox<'g, dyn RowOpMech + 'g>);

/// M4-RET R4a (V4R-286): write.returning's image sources for the expression at `(pipeline, op)`: `Some` for a RETURNING
/// projection that reads OLD / NEW (PG 18) or MERGE's `merge_action()` (D789), where its continuation lays those images
/// out; the expression step compiles its OLD / NEW Vars through them (gravity_expr `Builder.returning`).
pub struct RetImages(pub for<'g, 'p> fn(&crate::graph::Gx<'g, 'p>, usize, usize) -> Option<gravity_expr::nodes::Returning>);

/// D774 (M4-CLOSE B3): write.returning's `tableoid` for the RETURNING projection at `(pipeline, op)`: `(varno, relid)` of
/// a target every returned row is written to (a heap target, a root pruned to one leaf), PostgreSQL's
/// `slot->tts_tableOid`, a plan-time constant; `None` elsewhere (a routed INSERT's leaf is the row's: the sys site).
/// D774: the sys site's cells (ctid, xmin, cmin, xmax, cmax, tableoid; hosts write_sys.rs), after every other site of a
/// row op whose plan asks for them (`WritePlan.sys`, write/rowop_step.rs), and `tableoid`'s lane attno in the RETURNING
/// row write.returning lays out: SYS_AT_BASE + 6, past `CTID_AT` and xmin .. cmax (gravity_program v4 `sys_at`).
pub const SYS_CELLS: u16 = 6;
pub const TABLEOID_AT: i16 = gravity_program::protos::SYS_AT_BASE + 6;

pub struct RetTableoid(pub for<'g, 'p> fn(&crate::graph::Gx<'g, 'p>, usize, usize) -> Option<(i32, types_core::Oid)>);

/// A VALUES list's per-execution literal table (road `write.literals`, H16a; L0-m; NEW-29): its `LIT_WORDS` protocol words
/// from `base` (the cells, the null bytes, the row count) and the list's width. CAP-29: `stream`, the stream form (one more
/// word): the rows holding an expression are evaluated into the table at their turn (`LiteralsMech::stream`).
#[derive(Clone, Copy, Debug)]
pub struct LitTable {
    pub base: u16,
    pub cols: u16,
    pub stream: bool,
}

/// `write.literals` as the VALUES source reads it (write/literals.rs).
pub trait LiteralsMech: Sync {
    /// The table for VALUES pipeline `k` (plan node `node`, `cols` wide), its protocol declared, in the stream form when
    /// `stream`; `None`: the source's own form.
    fn table(&self, px: &mut Px<'_, '_>, k: usize, node: i32, cols: usize, stream: bool) -> Option<LitTable>;
    /// CAP-29, the stream form: coded row `j`'s cells that are not a Const evaluated into the table now, by PostgreSQL's
    /// evaluator in the row's turn (valid until the next coded row's); an error raises. D3374: `params` are the values
    /// (and valid flags) of the executor parameters the list's cells read (`values::exec_params`' order), handed over first.
    fn stream(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &LitTable, j: Value, params: &[(Value, Value)]);
    /// Whether row `j` holds a cell that is not a Const (`Bool`): the source's code answers it.
    fn coded(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &LitTable, j: Value) -> Value;
    /// The execution's row count (`I64`).
    fn rows(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &LitTable) -> Value;
    /// Row `j`'s cell `c`: `(datum, valid)`.
    fn cell(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &LitTable, j: Value, c: usize) -> (Value, Value);
}

pub struct LiteralsRoad(pub &'static dyn LiteralsMech);

/// Road `write.route`'s face for a write of several result relations (write/route.rs `target`): an arm's own target
/// `(rti, relid)` and the write plan every arm shares.
pub struct TargetsRoad(pub fn(&crate::graph::Gx<'_, '_>, usize, &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>, gravity_program::ArenaRef) -> (u32, u32, gravity_program::WritePlan));

/// Where a row op's checks run (H27): in the host op (PG's own ExecConstraints path), or in IR (`RowOpRoad::Checks`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChecksAt {
    Host,
    Ir,
}

crate::hook_variants! {
    /// Aggregate finals an end emits.
    "H35 -" pub enum Finals {
        /// CSE across the finals of a once-row end (several aggregates over shared argument expressions).
        Cse = Strategy(FINALS_CSE),
    }
}

pub trait EndMech {
    /// The JIT map's name of this mechanism (frame/jit.rs): its type's.
    fn mech_name(&self) -> &'static str {
        core::any::type_name::<Self>()
    }
    /// The per-claim state slot's bytes (`Env::state`).
    fn state_bytes(&self) -> u32 {
        0
    }
    /// H17 a.
    fn open_words(&self, _w: &mut OpenWords) {}
    fn open(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env) {}
    /// The accumulators the row loop carries (their initial values), after the out count.
    fn carried<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> PgVec<'e, Value> {
        PgVec::new_in(env.mcx)
    }
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, x: &Exits) -> Carry<'e>;
    /// At every window's end, with the carry after its last row.
    fn window_end(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _c: &Carry) {}
    /// D3148: after the steps' `window`, the `Bool` under which the end admits every row of the window without its
    /// admission test (join.domain's every-key set); the frame then lowers a second body with `Env::every` set, where the
    /// end folds as it would with no admission. `None`: none.
    fn every_guard(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Option<Value> {
        None
    }
    /// D3027: whether the row's first key lies strictly past the end's armed bound (a row top's topk.boundfirst: no row
    /// past it can enter the top), for a prefilter in front of the end (dict code.cut ends a key-ordered window there);
    /// `None` for an end without a bound.
    fn past_bound(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _row: &RowVals) -> Option<Value> {
        None
    }
    /// The section close, before `done`.
    fn finish(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _x: &Exits) {}
    /// H05 a / H27.
    fn sink(&self) -> SinkKind {
        SinkKind::Serial
    }
    /// H28: the end reads the row's locator (`Env::loc`) though it is not a row op (road topk.fetch).
    fn wants_locator(&self) -> bool {
        false
    }
    /// D3990: the end copies every byte of the row it reads (a sort run's append: datums, var copies), so nothing past
    /// the end reads the row's hosted result bytes; the frame then gives a row of big results back after it
    /// (`Ir::give_back_big`).
    fn copies_row(&self) -> bool {
        false
    }
    /// H02 f / H01 c: the window taken whole before its rows, emitted at its start with the carry the row loop starts from.
    /// The end may branch to `l_wend` (the row loop's exit, params = `c.params()`) with the window's carry (an answered
    /// window from the site's answer records, a count-only fold's `end - cursor` over a window no step can drop rows from)
    /// and returns true iff it emitted a branch; the builder is left where the rows are walked.
    fn whole(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _win: &Win, _c: &Carry, _l_wend: Block) -> bool {
        false
    }
    /// H01 c (mask.fold): a whole block `[base, base + 64)` whose passing rows are `m` folded at once (a popcount, a masked
    /// sum / min / max over the lanes `w`) instead of walked by ctz: the carry after, or `None` to walk the rows. Asked by
    /// the block path only when every step after the first keeps every row and reads nothing the end needs.
    fn block<'e>(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env<'e, '_>, _w: &WordWin, _base: Value, _m: Mask, _c: &Carry) -> Option<Carry<'e>> {
        None
    }
    /// KEYED-24: `block`'s form for an end whose host may decline at run time (a dense table.small): the carry after a
    /// taken block, with the builder left on the taken edge, and the block a decline enters, where the frame walks the
    /// block's rows as it does after `None`. Asked where `block` is, when `block` gave `None`.
    fn block_or_walk<'e>(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env<'e, '_>, _w: &WordWin, _base: Value, _m: Mask, _c: &Carry) -> Option<(Carry<'e>, Block)> {
        None
    }
    /// H01 c (count form): `block` reads only how many rows passed (count(*) folds, no FILTER), and only where it folds
    /// (mask.fold applies): any mask with that many bits set gives the same carry, and no lane is read.
    fn counts(&self, _env: &Env) -> bool {
        false
    }
    /// EXPLAIN ANALYZE: every row the end takes adds exactly one to its carry's `out` on each of its paths (`lower`,
    /// `block`, `block_or_walk`, `whole`, the count form) and nothing else adds to it, so the rows entering the end are the
    /// `out` each write-back hands on (stop.rs `define_exits`): an instrumented frame counts them there, once per window,
    /// not once per row.
    fn out_counts_rows(&self) -> bool {
        false
    }
    /// H20 (join.mult): the end folds a row of multiplicity `Env::mult` exactly as that many copies of it (count += m,
    /// sum += x * m, min / max / DISTINCT unaffected). A probe sets `Env::mult` only when this holds and no step after it
    /// counts rows (limit); any other end or step never sees `Some`.
    fn takes_mult(&self) -> bool {
        false
    }
    /// join.dedup (hashjoin strategies/dedup.rs): the end keeps one copy of a row whatever its count (a group keyed by its
    /// whole output with no aggregate: a DISTINCT), so a probe before it may drop copies of its build's records.
    fn takes_set(&self) -> bool {
        false
    }
    /// The end's amendments to its pipeline's descriptor (owner form, words, pass 2's admit), applied after the source's.
    fn pipeline_desc(&self, _px: &Px<'_, '_>, _k: usize, d: Pipeline) -> Pipeline {
        d
    }
    /// C10: what the end does past its grant (`spill::plan`), when its lowered code settles that only as it is made (a hash
    /// build's spill and record form), asked after the program's functions are built; `None`: `pipeline_desc`'s answer.
    fn holds(&self) -> Option<gravity_program::spill::Holds> {
        None
    }
}

/// A breaker's runtime merge (the build's or a keyed table's width-1 finish, its W>=2 merge), registered as one more row of
/// the breaker end's kind; the executor answers `merge_for` with `registry::kind_as::<MergeImpl>(kind)`.
pub struct MergeImpl(pub &'static dyn gravity_driver::Merge);

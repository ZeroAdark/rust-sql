//! A program's pipelines: `Pipeline`, `End`, `Reason` (split out of `program/mod.rs` by type, contract-runtime landing 3).

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum End {
    Sink,
    Breaker(Breaker),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reason {
    NoDrivingSite,
    IndexSource,
    /// A range scan whose order the AM vouches for over the whole relation (decompose.md decision 81): read serially like an index source, so the order is the pipeline's; at most one part.
    SortedSource,
    TempSource,
    Write,
    Lock,
    Host,
    Sink,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Parallel {
    Eligible,
    Ineligible { site: i16, reason: Reason },
}

/// What a pipeline's claims range over: the units of its driving site's source (blocks, granules), or the spill partitions of a breaker slot's stream (the partitioned join's third phase: one claim per build partition, the kernel refilling it into its own region).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClaimKind {
    Source,
    SpillPartitions { slot: u8 },
    /// D3024 (b): a keyed group emit's claims over the owner runs of the table it consumes first: a fixed count of slices
    /// (abi `OWNER_RUN_SLICES`), each claim's runs gathered by kernels table/emit.rs `agg_gather_claim`; owner runs are
    /// disjoint by key hash, so the claims split the groups with no overlap.
    OwnerRuns,
    /// D3024 (b): a hash join build drain's claims (RIGHT / FULL / RIGHT SEMI / RIGHT ANTI: lowerv4
    /// ops/hashjoin/drain/source.rs) over the records of the build in slot `slot`: the same fixed slices (abi
    /// `OWNER_RUN_SLICES` = S), claim `[lo, hi)` taking records `[T*lo/S, T*hi/S)` of its T (the NULL-key tail included)
    /// and each spilled partition `p` whose slice `p*S/2^bits` falls in `[lo, hi)`, so each goes out exactly once.
    BuildRecords { slot: u8 },
}

/// Construct C2 (b): which licence a lowering holds for a pipeline it runs ahead of PostgreSQL's demand (decompose
/// `effects::Licence`, its kind only: the program carries no `Licence`). The run-ahead tracer counts any kind but
/// `None`, `Rests`, `Exact` and `Accepted` licensed (those two apart); the whole-read kind's guard it checks at run time
/// too (`whole_read_cut`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LicenceStamp {
    /// No licence recorded (the tracer off, or none held).
    None,
    /// `Licence::recheck`: an error before any row left is checked by the plan-order run.
    Recheck,
    /// `Licence::tagged`: the unchecked licence; its error is tagged.
    Tagged,
    /// `Licence::held_error`: the error is held for PostgreSQL's point.
    Held,
    /// `Licence::whole_read`: run ahead only for a reader that reads every row.
    Whole,
    /// Any other licence: code with no effects (`effects::unobservable`), or one `licence` grants without a recheck.
    Plain,
    /// `Licence::exact` (D1): a structural proof that PostgreSQL 18.6 runs the same code to the same first error. The tracer
    /// counts it apart (AHEAD-EXACT, by `proof`), never as licensed; `node`: the plan node the proof cites; `check`: what
    /// the lowering could check of it.
    Exact { proof: ProofStamp, node: i32, check: ProofCheck },
    /// The accepted class (decompose `effects::accepted` / `effects::volatile_free`): early work whose departure is
    /// only an error PostgreSQL never hits, or extra VOLATILE calls, which are free. The tracer counts it apart
    /// (AHEAD-ACCEPTED), never as a key.
    Accepted,
    /// C2 R3: no licence held for this run, but the pipeline's code rests on the plan-order re-run under a recheck licence
    /// asked for other code it runs (an InitPlan the lowering runs at its open), so the tracer's `Recheck-after-send`
    /// reads it. No licence for the run-ahead verdict.
    Rests,
}

/// D1: the kind of a structural proof (decompose `effects::ProofKind`; the program carries no `Licence`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProofStamp {
    FirstFetch,
    PgReadsWhole,
    PgFailsToo,
    PgRunsFirst,
}

/// D1: what was checked of an exact licence's proof past its constructor's evidence check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProofCheck {
    /// The lowering checked it against this pipeline (PgReadsWhole: the cited node is one of the pipeline's).
    Checked,
    /// Nothing past the constructor checks it (the runner cannot tell a node's first fetch from the run's start).
    Unchecked,
    /// The lowering's check failed: the tracer reports the run as a key.
    Failed,
}

/// A pipeline of the program's DAG (§2 decision 4): what it drives, what it ends in, and the breaker slots it reads (`consumes`: any number, decision 82). A pipeline
/// that produces a breaker declares what it consumes; one that ends in the sink and declares nothing consumes the nearest earlier producer's slot (the chain every program before §2 is).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pipeline {
    pub driving_site: i16,
    pub ends_in: End,
    pub parallel: Parallel,
    /// The planner's rows (`build_rows` too): the content key holds neither (KEY-PREDICTED-ROWS road (b)); a lowering decision
    /// that reads one is a guard of its program (lower inputs/key.rs `decide`), re-run on the next statement's estimates.
    pub predicted_rows: u64,
    /// The consumed slots in declaration order: a static (the kernels') or a slice in the program's arena (`consuming_in`).
    pub consumes: &'static [u8],
    /// The life each consumed slot is read from (`consumes`' order; empty: the nearest earlier producer of each slot).
    pub consumes_from: &'static [LifeId],
    pub claims: ClaimKind,
    pub owner: Option<OwnerDesc>,
    pub admit: Option<TwoPassDesc>,
    pub words: Option<WordsDesc>,
    /// The top-k run this pipeline produces, declared (`TopKDesc`); `None`: the shape table's.
    pub topk: Option<TopKDesc>,
    /// The kernel is called once more per worker with `MorselRange::SECTION_END` after its last claim (item 20).
    pub finish: bool,
    /// A breaker's lowered row: a hash build's row-store estimate (M7 L1a; the admission counts the store beside the
    /// entries), a sort's run row (lowerv4 face/runs.rs `Shape::row_bytes`, RUNS-26: the breaker cost's record); 0 = none
    /// (a hash build's legacy inline entry). Hash admission reads it for a HashBuild breaker only.
    pub store_row_bytes: u32,
    /// V4R-481: the rows a hash build receives, PostgreSQL's estimate at its Hash node (decompose join.rs), what admission
    /// and the grant estimate size its table and row store by (`build_estimate`); 0: none named, the driving scan's
    /// `predicted_rows`. A build fed through a join receives the join's rows, not its scan's (a selective join under a large scan: the build sees the join's few rows).
    /// D3355: a fused drain's probe takes its drain's (lowerv4 drain `Fused`), and under a RIGHT SEMI / ANTI drain, whose
    /// probe hands no row on, any breaker it ends in is sized by them (`cost::breaker::of_at`), not by its driving scan.
    pub build_rows: u64,
    /// D732-rest (V4R-290): an EvalPlanQual recheck never reruns this producer: it feeds only a subquery whose row a COPY
    /// mark carries whole (EvalPlanQualFetchRowMark's copy leg: PostgreSQL never re-evaluates the subquery), so the rerun
    /// opens its slot empty and the consumer's copy entry hands the stored row in.
    pub recheck_skip: bool,
    /// D824 (V4R-298): this pipeline is a lazy hash build of pipeline `lazy_for`'s outer-first probe (PG's HJ_BUILD_HASHTABLE
    /// fetches the outer's first row before it builds): the runner skips it in order and runs it when that probe stops with
    /// `status::BUILD_WAIT`, never when the probe's pipeline ends with no row at the probe. -1: none.
    pub lazy_for: i16,
    /// D2033 step 2: an EvalPlanQual recheck keeps this producer in the statement run's order. A lazy build (`lazy_for`)
    /// waits for its probe's first row in every recheck: none runs it first (driver `Entry::rerun`). An op 14 build's held
    /// error stays held for its probe's first row (exec runner `rerun_producer`). lowerv4 lockrows rerun.rs `first_held`
    /// sets it on the build of a lock pipeline's first probe.
    pub recheck_hold: bool,
    /// D2033 step 3: this `recheck_hold` build is the epq probe's own (lowerv4 lockrows rerun.rs `epq_held`). PG's recheck
    /// builds it at the outer's first row while that join's hj_OuterNotEmpty is unset, and before its outer side once a
    /// recheck of the statement set it (driver `Entry::rerun`'s `seen`, D2050 / D2674): its error is held for the probe's
    /// first row until then, and the recheck's own at once after (exec runner `rerun_producer`).
    pub recheck_hold_epq: bool,
    /// D1236's held inner (op 14 for runs.sort): this full Sort's run is a merge join's inner, and an error its fill raises
    /// is held for the merge's first inner call (lowerv4 ops/mergejoin/held_inner.rs), where PG's ExecSort raises it: exec's
    /// merge for it stamps the run `RUN_HELD` (ops/sort/held.rs) and the driver holds the error (`Entry::defer_failed`).
    /// D449: or a pull-form MergeAppend's Sort arm, held until the merge reader first reads the run's end (PG's first pull).
    /// D1108 (op 14 "held at len"): or a plan Material's fill run at its nest's first outer row (lowerv4 rerun/spool/form.rs
    /// `held`): the Material slot's region stamped the same way, its reader raising the error at the record the fill
    /// stopped at (PG's Material asks its child for that row there).
    pub held_sort: bool,
    /// D2316 (op 14 "held at len" for runs.incremental): a `held_sort` Incremental Sort's run keeps the rows its fill
    /// stored when it raised (exec ops/sort/held.rs `stamp_at`): its reader reads those PostgreSQL's presorted batches
    /// handed on before the raising row and raises the held error past them (lowerv4 ops/sort/inc_held.rs).
    pub held_at: bool,
    /// v5 errorhold (interfaces §2.3, `Batch::Statement`): this pipeline runs a departure under a recheck licence
    /// (`effects::Licence::recheck`): an error the statement raises before any row left it is checked by running the
    /// statement again in plan order, every licence denied (exec `runner::recheck`). Not EvalPlanQual's recheck.
    pub recheck: bool,
    /// D1922 (the portal-kind fact): this pipeline departs from PostgreSQL's order in a way only a reader that takes every
    /// row cannot see (a drain into the statement's sink runs its probe whole before the first row, where PostgreSQL hands
    /// rows on at their first match). The entry delegates the statement when its reader may stop early (exec admit/entry
    /// `PortalKind::partial`: a cursor FETCH, an SPI cursor or PL/pgSQL FOR loop, a SQL function's lazy row).
    pub whole_read: bool,
    /// `DECISIONS.md` 4 (D) b: this pipeline runs code of the plan node named here ahead of PostgreSQL's plan under an
    /// unchecked licence (decompose `effects::licence_unchecked`, `Licence::tagged`): an expression error its run ends in
    /// is the statement's, and carries the tag in the server log (exec glue `pg_error`). `None` for every other pipeline.
    pub ahead: Option<AheadTag>,
    /// Construct C2 (b), the run-ahead tracer: the plan node id of this pipeline's source when it is a node PostgreSQL 18.6
    /// may never run however the rows fall (decompose `pg_demand::never_run`: a nest's or merge's inner, an outer-first
    /// Hash, a subplan, a one-time gate's child); -1: PostgreSQL runs it whenever its consumer runs. Run in program order
    /// such a pipeline runs ahead of PostgreSQL's first demand, which the driver's tracer (`ahead_trace`) checks against
    /// its licence. Never shown.
    pub pg_gate: i32,
    /// D4191: the gate regions holding `pg_gate`'s node, innermost first (decompose `pg_demand::regions`: a region is a
    /// subtree PostgreSQL may skip whole, named by its root's plan node id); its four innermost, -1 after them, all -1
    /// when `pg_gate` is. A demand (a CTE's pull, an Init's first read, a rescan, a lazy build's wait) runs the region of
    /// the pipeline that asked: the tracer takes a run there as PostgreSQL's demand only when this pipeline's innermost
    /// region is one of that pipeline's. Read by the tracer only. Never shown.
    pub pg_regions: [i32; 4],
    /// C2 (b): neither this pipeline nor what feeds it may raise or act (lowerv4 build_class `side` is Safe, and `groups_side`:
    /// the groups they fold into, by their keys and aggregate arguments), so a run of it ahead of PostgreSQL's demand cannot
    /// be seen. Read by the tracer only. Never shown.
    pub quiet: bool,
    /// Construct C2 (b): the kind of licence the lowering holds for running this pipeline ahead of PostgreSQL's demand
    /// (lowerv4 `Px::hold`), stamped only while the tracer is on. Read by the tracer only. Never shown.
    pub licence: LicenceStamp,
    /// C2 (b) diagnosis: the plan node kinds of `pg_gate`'s node and of its parent (decompose `ledger::Kind::tag_name`, e.g.
    /// `("Hash", "HashJoin")`; `"-"`: the plan's root), stamped with `pg_gate` while the tracer is on, for its hit lines.
    /// Read by the tracer only. Never shown.
    pub gate_tags: Option<(&'static str, &'static str)>,
    /// An on-demand producer that runs nested inside its readers with no consumes entry (a LAZY hashed SubPlan's set, built
    /// at its first probe): its slot stays live through pipeline `live_through`, its last reader, so the grant's divisor
    /// and the frees count it where it is held. -1: none (its consumers alone).
    pub live_through: i16,
    /// KEY-PREDICTED-ROWS road (b): the graph pipeline whose planner estimate `predicted_rows` carries (`u16::MAX`: none, or
    /// a value the lowering derived). The content key holds no estimate, so an admission that takes this program for another
    /// statement binds that statement's estimate of the pipeline over this one (exec admit `estimates`). Never shown.
    pub rows_of: u16,
    /// `rows_of` for `build_rows`.
    pub build_of: u16,
    /// D3588: the graph pipeline whose source the driving site reads (lowerv4 `Mechs::shaped`'s `src`: its own, or a swap's
    /// other side); `u16::MAX`: no driving site. The admission's registry binds the site to that source's recipe: the
    /// program's sites are numbered in its pipelines' order, which moves an Append's arms (D1752), so two sources over one
    /// index (a bitmap arm, a nest inner's probe) are not told apart by their relation and index in graph order.
    pub source_of: u16,
    /// D3914: the program pipeline this one is a pass of (lowerv4 `Px::add_pass`: a spilled join's pair pipeline, a HAVING
    /// split's or a top-N's second pass), whose ops it runs again over that pipeline's consumed slots; `u16::MAX`: none. The
    /// driver lets it open over a rerun set's slot not yet filled where it lets that pipeline (`Node::rerun_reader_check`).
    pub pass_of: u16,
    /// DR-1 / DR-4: the planner's row width (`plan_width`, from `stawidth`) of the rows the pipeline carries at its end, the
    /// last plan node it maps (decompose walk.rs); 0: none named. The cost home's breaker record takes it as the floor of the
    /// lowering's layout (`cost::breaker::row_bytes`): a by-reference column's layout holds a reference, the row it stands
    /// for is at least the width PostgreSQL estimated. A size the grant asks by, never a decision: the content key leaves
    /// it out as it leaves out the estimates (lower inputs/key.rs), and a statement that takes this program asks by the
    /// lowering statement's width. Never shown.
    pub row_width: u32,
    /// D2772 POST (modifytable, ExecPostprocessPlan): this pipeline is one of a data-modifying CTE's that nothing reads,
    /// beside the statement's own write; the post pipelines are the program's last ones. Before the first of them the
    /// runner turns (driver entry/post.rs): it queues the statement's AFTER STATEMENT events and binds the CTE's writer
    /// (its BEFORE STATEMENT triggers), as PostgreSQL runs such a CTE's ModifyTable once the statement's own has ended; a
    /// recheck of the statement's own write reruns none of them (EvalPlanQual never runs an unread CTE's plan).
    pub post: bool,
    /// D2877: the kernel makes a check PostgreSQL makes at its node's first call, before it pulls a row, whatever its outer
    /// holds (a Result's one-time qual and its InitPlans: ExecResult's `rs_checkqual`; a feeder's copy of a gate's or a
    /// Limit's check: lowerv4 `StepMech::first_call`). A run no claim reaches (an empty relation, every granule pruned)
    /// calls it once over the empty claim (gravity_parallel `next_claim`), so the check runs, its raise and its InitPlan
    /// with it; the source reads nothing. Never shown.
    pub first_call: bool,
    /// NEW-48 (KEY-PREDICTED-ROWS road (b)): the planner's rows reaching this pipeline's end per iteration of its innermost
    /// loop, fixed point (`END_RATE_ONE` = one row per iteration): a nested loop iterates per outer row times the inner's rows
    /// per loop, a hash probe per probe row, a merge join per row of either side (decompose ops/agg `end_rate`, at a fold's or
    /// a keyed end's close). `END_RATE_NONE` (the default): no loop past the source's (whose block path iterates per 64-row
    /// block). An end's carried registers ride every iteration while only the rows reaching it use them: lowerv4 decides the
    /// carry on it (`frame.carry` for a fold, the run carry for a keyed end) through lower inputs/key.rs `decide_end_rate`.
    /// An estimate: the content key leaves it out as it leaves out `predicted_rows`. Never shown.
    pub end_rate: u64,
    /// D3205: the planner's rows (`plan_rows`) of the rows the pipeline carries at its end, the last plan node it maps
    /// (decompose walk.rs, beside `row_width`); 0: none named. What a breaker that is not a hash build receives, and what
    /// the cost home sizes it by when `predicted_rows` is 0 (`cost::breaker::of_at`): a serial pipeline's (a breaker's
    /// consumer, an index or TID source, a CTE scan) carries none, so its Sort, Materialize or hash aggregate was sized
    /// from 0 rows under `pgrust.gravity_exact_size` = none. D4047: past a narrowing join every breaker but a hash build is
    /// sized by them (`cost::breaker::input_rows`). A size the grant asks by, never a decision: the content key leaves it
    /// out as it leaves out `row_width` (lower inputs/key.rs), and a statement that takes this program binds its own over
    /// it (`end_of`, `bind_estimates`). Never shown.
    pub end_rows: u64,
    /// D4047: `rows_of` for `end_rows`; `u16::MAX`: none (the lowering statement's rows, which a statement of other
    /// estimates drops: `bind_estimates`).
    pub end_of: u16,
    /// The rows a row top's run holds before it cuts (lowerv4 runs.topk_rows: its soft cap over a constant count and
    /// offset), the bound of its Sort breaker's region: the driver caps the shares at them (`grant::top_share_cap`), the
    /// breaker cost sizes the table by them. 0: none known (every other pipeline). Past it the run spills its runs.
    pub top_rows: u64,
    /// C10: what the lowered end does past its grant, when a lowering settled it otherwise than decompose declared
    /// (`spill::plan`: a hash build that does not spill, a keyed table kept resident, a hashed SubPlan's set); `None`: the
    /// declaration's answer (`spill::declared`). Read through `holds()`. A decision the lowered code already carries, so
    /// the content key leaves it out (its code identity differs wherever it does).
    pub holds: Option<crate::spill::Holds>,
    /// C10 2b (D3582): the pipeline's source is a plain serial group emit over a spilling hash aggregate or distinct
    /// (lowerv4 table/emit_group.rs `Emit::refills`): at its records' end it asks for the next batch of the slot it
    /// consumes first (kernels `agg_gather_next`), so the driver may batch that slot (driver mem/batch.rs: one batch of its
    /// partitions in memory at a time). Shown when set.
    pub refills: bool,
    /// D4620: a grouping set folded from a finer set's owned records (lowerv4 ops/agg/cascade.rs, road
    /// `agg.gsets.cascade`). What it holds follows its parent's real records, not the planner's share of the node's groups:
    /// its breaker region starts small and grows in place whatever the statement's start (driver mem/grant.rs
    /// `starts_small`), and its partials hold its groups about once each, so an owner phase whose cut is under them
    /// regrows its owners to their even share first (parallel `own_pooled` at scale 1), not to twice it (the
    /// skewed-partition step that comes next). Shown when set.
    pub cascade: bool,
}

/// KEY-PREDICTED-ROWS road (b): a statement's planner estimates of one program pipeline, `predicted_rows`, `build_rows` and
/// (D4047) `end_rows` (`bind_estimates`, read through `Pipeline::with_est`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PipeEst {
    pub rows: u64,
    pub build: u64,
    pub end: u64,
}

/// KEY-PREDICTED-ROWS road (b): a statement's estimates of `ps`, a program's pipelines, each read from the graph pipeline
/// its descriptor names (`rows_of`, `build_of`, D4047 `end_of`; `graph(j)`: the statement's graph pipeline `j`), else the
/// program's own; `None` when every one equals what the program carries (the admission that lowered it, or a statement
/// of the same estimates). A statement of other estimates drops an `end_rows` no graph pipeline names (the lowering
/// statement's rows past its joins) where it binds rows: that breaker is sized by its bound rows, as before D4047. A
/// serial pipeline's (no rows) stay the program's, as `pgrust.gravity_exact_size` = final | all reads them (D3205).
pub fn bind_estimates<'g>(ps: &[Pipeline], graph: impl Fn(u16) -> Option<&'g Pipeline>) -> Option<Box<[PipeEst]>> {
    let of = |j: u16, filed: u64, f: fn(&Pipeline) -> u64| graph(j).map_or(filed, f);
    let mut est: Vec<PipeEst> = ps.iter().map(|p| PipeEst { rows: of(p.rows_of, p.predicted_rows, |d| d.predicted_rows), build: of(p.build_of, p.build_rows, |d| d.build_rows), end: of(p.end_of, p.end_rows, |d| d.end_rows) }).collect(); // alloc: Cold
    if ps.iter().zip(est.iter()).all(|(p, e)| p.est() == *e) {
        return None;
    }
    for (p, e) in ps.iter().zip(est.iter_mut()) {
        if p.end_of == u16::MAX && e.rows != 0 {
            e.end = 0;
        } else if e.rows == 0 {
            e.end = p.end_rows;
        }
    }
    Some(est.into_boxed_slice())
}

/// `Pipeline::ahead`: the licensed plan node as the harness names it, its index in EXPLAIN's node order (decompose
/// `explain::explain_position`) and EXPLAIN's Node Type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AheadTag {
    pub node: u32,
    pub node_type: &'static str,
}

/// `Pipeline::end_rate` of one row reaching the end per loop iteration.
pub const END_RATE_ONE: u64 = 1_000_000;
/// `Pipeline::end_rate` of a pipeline with no loop past its source's.
pub const END_RATE_NONE: u64 = u64::MAX;

/// The derive's text, `store_row_bytes` shown only when a lowered row is published (a hash build's store, a sort's run row).
impl core::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut d = f.debug_struct("Pipeline");
        d.field("driving_site", &self.driving_site).field("ends_in", &self.ends_in).field("parallel", &self.parallel).field("predicted_rows", &self.predicted_rows).field("consumes", &self.consumes).field("claims", &self.claims).field("owner", &self.owner).field("admit", &self.admit).field("words", &self.words).field("topk", &self.topk).field("finish", &self.finish);
        if self.store_row_bytes != 0 {
            d.field("store_row_bytes", &self.store_row_bytes);
        }
        if self.build_rows != 0 {
            d.field("build_rows", &self.build_rows);
        }
        if self.top_rows != 0 {
            d.field("top_rows", &self.top_rows);
        }
        if self.recheck_skip {
            d.field("recheck_skip", &true);
        }
        if self.recheck_hold {
            d.field("recheck_hold", &true);
        }
        if self.recheck_hold_epq {
            d.field("recheck_hold_epq", &true);
        }
        if self.held_sort {
            d.field("held_sort", &true);
        }
        if self.held_at {
            d.field("held_at", &true);
        }
        if !self.consumes_from.is_empty() {
            d.field("consumes_from", &self.consumes_from);
        }
        if self.lazy_for >= 0 {
            d.field("lazy_for", &self.lazy_for);
        }
        if self.recheck {
            d.field("recheck", &true);
        }
        if self.whole_read {
            d.field("whole_read", &true);
        }
        if let Some(a) = self.ahead {
            d.field("ahead", &a);
        }
        if self.live_through >= 0 {
            d.field("live_through", &self.live_through);
        }
        if self.post {
            d.field("post", &true);
        }
        if let Some(q) = self.pass_of() {
            d.field("pass_of", &q);
        }
        if self.refills {
            d.field("refills", &true);
        }
        if self.cascade {
            d.field("cascade", &true);
        }
        d.finish()
    }
}

/// Every slot id: `consuming_one`'s slices (`&SLOT_TABLE[s..=s]`).
pub(crate) static SLOT_TABLE: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = i as u8;
        i += 1;
    }
    t
};

impl Pipeline {
    pub const fn serial(driving_site: i16, ends_in: End, reason: Reason) -> Pipeline {
        Pipeline { driving_site, ends_in, parallel: Parallel::Ineligible { site: driving_site, reason }, predicted_rows: 0, consumes: &[], consumes_from: &[], claims: ClaimKind::Source, owner: None, admit: None, words: None, topk: None, finish: false, store_row_bytes: 0, build_rows: 0, recheck_skip: false, lazy_for: -1, recheck_hold: false, recheck_hold_epq: false, held_sort: false, held_at: false, recheck: false, whole_read: false, ahead: None, pg_gate: -1, pg_regions: [-1; 4], quiet: false, licence: LicenceStamp::None, gate_tags: None, live_through: -1, rows_of: u16::MAX, build_of: u16::MAX, source_of: u16::MAX, pass_of: u16::MAX, row_width: 0, post: false, first_call: false, end_rate: END_RATE_NONE, end_rows: 0, end_of: u16::MAX, top_rows: 0, holds: None, refills: false, cascade: false }
    }

    pub const fn eligible(driving_site: i16, ends_in: End, predicted_rows: u64) -> Pipeline {  
        Pipeline { driving_site, ends_in, parallel: Parallel::Eligible, predicted_rows, consumes: &[], consumes_from: &[], claims: ClaimKind::Source, owner: None, admit: None, words: None, topk: None, finish: false, store_row_bytes: 0, build_rows: 0, recheck_skip: false, lazy_for: -1, recheck_hold: false, recheck_hold_epq: false, held_sort: false, held_at: false, recheck: false, whole_read: false, ahead: None, pg_gate: -1, pg_regions: [-1; 4], quiet: false, licence: LicenceStamp::None, gate_tags: None, live_through: -1, rows_of: u16::MAX, build_of: u16::MAX, source_of: u16::MAX, pass_of: u16::MAX, row_width: 0, post: false, first_call: false, end_rate: END_RATE_NONE, end_rows: 0, end_of: u16::MAX, top_rows: 0, holds: None, refills: false, cascade: false }
    }

    /// KEY-PREDICTED-ROWS road (b): its estimates as the lowering statement's planner gave them.
    pub const fn est(&self) -> PipeEst {
        PipeEst { rows: self.predicted_rows, build: self.build_rows, end: self.end_rows }
    }

    /// This pipeline with a statement's estimates (`bind_estimates`) over the lowering statement's.
    pub const fn with_est(self, e: PipeEst) -> Pipeline {
        Pipeline { predicted_rows: e.rows, build_rows: e.build, end_rows: e.end, ..self }
    }

    pub const fn admitting(mut self, d: TwoPassDesc) -> Pipeline {
        assert!(matches!(self.ends_in, End::Breaker(_)), "the admitting pass produces a breaker");
        assert!(self.owner.is_some(), "the admitting pass declares its owner phase");
        self.admit = Some(d);
        self
    }

    /// Declare the top-k run (`TopKDesc`): the pipeline ends in a `TopK` breaker and, for a walk, drives a source.
    pub const fn topk(mut self, d: TopKDesc) -> Pipeline {
        assert!(matches!(self.ends_in, End::Breaker(Breaker { kind: gravity_abi::breaker::BreakerKind::TopK, .. })), "a declared top-k run needs a TopK breaker");
        assert!(d.n > 0, "a declared top-k run keeps at least one row");
        assert!(d.walk_attno == 0 || self.driving_site >= 0, "the zone walk orders a driving site's claims");
        self.topk = Some(d);
        self
    }

    pub const fn storing_rows(mut self, bytes: u32) -> Pipeline {
        self.store_row_bytes = bytes;
        self
    }

    /// A row top's run holds at most `rows` rows before it cuts (`top_rows`; 0: unbounded).
    pub const fn keeping_rows(mut self, rows: u64) -> Pipeline {
        self.top_rows = rows;
        self
    }
    pub const fn finishing(mut self) -> Pipeline {
        assert!(matches!(self.parallel, Parallel::Eligible) && self.driving_site >= 0, "the section end is a claim-driven pipeline's");
        self.finish = true;
        self
    }

    pub const fn words(mut self, d: WordsDesc) -> Pipeline {
        let s = d.slots();
        let mut i = 0;
        while i < s.len() {
            assert!(s[i] == WordsDesc::NONE || s[i] < d.nparams, "a kernel word past the extent");
            i += 1;
        }
        let group = d.topk != WordsDesc::NONE || d.dense_lo != WordsDesc::NONE || d.dense_hi != WordsDesc::NONE || d.order != WordsDesc::NONE;
        assert!(!group || matches!(self.ends_in, End::Breaker(Breaker { kind: gravity_abi::breaker::BreakerKind::HashAgg, .. })), "the group facts need a HashAgg");
        assert!(d.bits == WordsDesc::NONE || matches!(self.ends_in, End::Breaker(_)), "the bits word needs a breaker");
        self.words = Some(d);
        self
    }

    pub const fn owner(mut self, d: OwnerDesc) -> Pipeline {
        assert!(matches!(self.ends_in, End::Breaker(_)), "an owner phase needs a breaker");
        assert!(d.layout.nkeys() > 0, "an owner phase folds keyed records");
        if d.ctor.is_none() && d.ctor_desc.is_none() {
            // the generic phase serves three forms (exec's `owner.rs`): unordered runs, `ORDER_KEY` (ascending keys, no top-k), and the count-ordered form — `order_slot` a `Count` slot of the layout with `topk` = n
            if d.order_slot == ORDER_KEY {
                assert!(d.topk == 0, "a key-ordered owner keeps every record (no top-k)");
            } else if d.order_slot != ORDER_NONE {
                assert!(d.topk > 0 && (d.order_slot as usize) < d.layout.nslots() && matches!(d.layout.slot_kind(d.order_slot as usize), SlotKind::Count), "the count-ordered owner form: a Count slot of the layout and a bound");
            }
        }
        if let Some((_, sh)) = d.bits {
            assert!(sh <= 56, "the partition-bits word");
        }
        self.owner = Some(d);
        self
    }

    /// The pipeline's claims are the spill partitions of `slot`'s stream (the driving site is nominal: the kernel reads the files, never the source).
    pub const fn over_spill_partitions(mut self, slot: u8) -> Pipeline {
        self.claims = ClaimKind::SpillPartitions { slot };
        self
    }

    /// Declare the consumed slots (a constant: the kernels' programs; `consuming_in` appends at run time).
    pub const fn consuming(mut self, slots: &'static [u8]) -> Pipeline {
        let mut i = 0;
        while i < slots.len() {
            let mut j = 0;
            while j < i {
                assert!(slots[i] != slots[j], "a pipeline consumes one slot twice");
                j += 1;
            }
            i += 1;
        }
        self.consumes = slots;
        self
    }

    /// Declare one consumed slot (no arena: the slice is the static table's).
    pub const fn consuming_one(mut self, slot: u8) -> Pipeline {
        assert!(self.consumes.is_empty(), "consuming_one declares the first slot");
        self.consumes = SLOT_TABLE.split_at(slot as usize).1.split_at(1).0;
        self
    }

    /// Declare one more consumed slot: the first from the static table, the rest grow in `arena` (the program's).
    pub fn consuming_in(mut self, arena: ArenaRef, slot: u8) -> Pipeline {
        assert!(!self.consumes.contains(&slot), "a pipeline consumes one slot twice");
        if self.consumes.is_empty() {
            return self.consuming_one(slot);
        }
        let mut v: ArenaVec<u8> = ArenaVec::with_capacity_in(self.consumes.len() + 1, arena);
        v.extend_from_slice(self.consumes);
        v.push(slot);
        // SAFETY: the arena outlives every copy of the pipeline (the program's).
        self.consumes = unsafe { &*(v.leak() as *const [u8]) };
        self
    }

    /// Declare one more consumed slot and the life it is read from.
    pub fn consuming_from_in(self, arena: ArenaRef, slot: u8, life: LifeId) -> Pipeline {
        assert!(self.consumes_from.len() == self.consumes.len(), "a pipeline names the life of every slot it consumes or of none");
        let mut p = self.consuming_in(arena, slot);
        let mut v: ArenaVec<LifeId> = ArenaVec::with_capacity_in(self.consumes_from.len() + 1, arena);
        v.extend_from_slice(self.consumes_from);
        v.push(life);
        // SAFETY: the arena outlives every copy of the pipeline (the program's).
        p.consumes_from = unsafe { &*(v.leak() as *const [LifeId]) };
        p
    }

    /// D3914: the program pipeline this one is a pass of (`pass_of`), `None` for every other pipeline.
    pub const fn pass_of(&self) -> Option<usize> {
        if self.pass_of == u16::MAX { None } else { Some(self.pass_of as usize) }
    }

    /// The slot this pipeline produces.
    pub const fn produces(&self) -> Option<u8> {
        match self.ends_in {
            End::Breaker(b) => Some(b.slot),
            End::Sink => None,
        }
    }

    pub const fn nconsumed(&self) -> usize {
        self.consumes.len()
    }

    pub const fn breaker(&self) -> Option<Breaker> {
        match self.ends_in {
            End::Breaker(b) => Some(b),
            End::Sink => None,
        }
    }

    /// C10: what this pipeline's breaker does past its grant (`spill::Holds`): the lowered end's answer, else its
    /// declaration's; `None` for a sink end.
    pub const fn holds(&self) -> Option<crate::spill::Holds> {
        match (self.ends_in, self.holds) {
            (End::Sink, _) => None,
            (End::Breaker(_), Some(h)) => Some(h),
            (End::Breaker(b), None) => Some(crate::spill::declared(&b)),
        }
    }

    /// V4R-481: the rows this pipeline's hash build is sized by: `build_rows` when the plan named them, else `predicted_rows`.
    pub const fn build_estimate(&self) -> u64 {
        if self.build_rows != 0 {
            self.build_rows
        } else {
            self.predicted_rows
        }
    }

    pub const fn is_eligible(&self) -> bool {
        matches!(self.parallel, Parallel::Eligible)
    }
}

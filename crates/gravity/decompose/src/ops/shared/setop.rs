//! The set operation descriptors other families read; their methods stay with ops/setop.
use gravity_program::ArenaVec;
use types_nodes::list::NodeList;

use crate::ops::shared::sort::EqKey;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetCmd {
    Intersect,
    IntersectAll,
    Except,
    ExceptAll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IterUntil {
    Empty,
    Rows(u64),
}

/// The iteration face (milestone-2.md §2.6 item 21): the runner re-runs pipelines `reruns` (driven by
/// slot `work`, producing slot `next` in `body`, the last of them) per iteration, swaps the two, stops at `until`;
/// `dedup` is a UNION's keys, the rows the union holds compared on them (both tables' fills drop a row already held); the
/// emit walks every generation of `work`.
/// `reruns` (D241) is the recursive term's pipelines PG re-executes per generation (ExecRecursiveUnion adds
/// `wt_param` to the term's chgParam: `rerun::rerun_set`); the term's other pipelines run once, before the first
/// generation (a one-batch Hash, a Material / Sort over input with no chgParam keep their rows).
#[derive(Debug)]
pub struct IterDesc {
    pub body: u16,
    pub reruns: ArenaVec<u16>,
    pub work: u8,
    pub next: u8,
    pub until: IterUntil,
    /// D1378: a UNION's keys (PG's `dupColIdx` / `dupOperators`) and each one's collation (`dupCollations`); both
    /// empty for UNION ALL.
    pub dedup: ArenaVec<EqKey>,
    pub dedup_colls: ArenaVec<u32>,
    /// with#62 / #65 / #82 / #86: each dedup key's ROW() columns (type, collation) when it is an anonymous `record` or
    /// `record[]` key whose producers agree on them (facts::record_union); empty for every other key.
    pub dedup_rows: ArenaVec<ArenaVec<(u32, u32)>>,
    pub wt_param: i32,
    /// W8-RECURSE (F5, `rescan::of_plan`): whether what the recursive CTE's first pull runs whole can neither raise nor call
    /// anything volatile or a user function (D1906's rule): the non-recursive term and every term pipeline that runs once
    /// (not in `reruns`; one with no closer is loud: fail closed). PG evaluates them lazily (the seed row by row, a term's
    /// Hash at the first generation), so a lowering that runs them first is exact only then (D1383).
    pub early_quiet: bool,
    /// with#114 (append): every reader reads the union to its end (`readers`: no CteScan of it under a LIMIT, a probe an
    /// empty build may end or an InitPlan's first row), so PostgreSQL reads the seed whole too: lowerv4 rerun/union_arms.rs
    /// fills a settled UNION ALL seed's arms first only then or when `early_quiet` holds. D2732 (setop): PG pulls the seed
    /// row by row, the lowering runs it whole at the first pull, so a seed or a once-run term input that may raise or call a
    /// user or VOLATILE function (`early_quiet` false) under a reader that may stop is walled (a raise PG never reaches).
    pub read_whole: bool,
    /// W8-RECURSE (F5): whether the recursive term calls anything volatile or a user function: PG runs it row by row as
    /// the CTE is pulled, the lowering a generation whole (D1384).
    pub term_volatile: bool,
    /// W8-RECURSE (S6): the recursive term can raise and its rows come through a Hash Join: PG's multi-batch join hands a
    /// generation's rows on in another order than the lowering's, so a held term error sits elsewhere among them.
    pub hash_loud: bool,
    /// D2230 (`readers`): a qual or a join drops the union's rows on their way to the client with no breaker over it (or
    /// the union is read in a nested loop's inner, a SubPlan or an InitPlan, or through another CTE's plan). A pull of n
    /// rows that asks the program for a whole block may never fill it over a non-terminating union, where PG returns the
    /// first n rows: the lowering sets the Generation proto's `GEN_ARG_THIN` bit and exec's pull asks such a program for
    /// the rows each run owes only (D439's owed cap), as PG computes no row past the run's count.
    pub thin: bool,
    /// D2230 / D2212 (`readers`): the union is read through another CTE's plan PG runs only as far as its reader pulls
    /// (no breaker reading it whole over it), or through a recursive union whose generations stream it (D2034). The
    /// lowering runs that plan's seed whole before its first row, so a pull that may stop early (a cursor FETCH n, a
    /// PL/pgSQL FOR loop, a SQL function's lazy row, a bounded Execute) may never return over a non-terminating union where
    /// PG returns the first n rows: the lowering names it in the Generation proto's `peer` and exec refuses such a pull.
    pub through: bool,
    /// Why the union is not built yet, decided here on the plan (D2212, D2223, D2312, F3: `wall`); the lowering
    /// refuses with it. The graph still decomposes (D4's closure: this family refuses only by a ruling).
    pub wall: Option<&'static str>,
    /// K11.rec.l0 / .gl (D2223, `readers`): the plan node of the LIMIT that may count 0 (a constant 0 or a parameter) over
    /// the Sort or Agg that reads the union whole: PG's ExecLimit never pulls its subplan at a zero count, so PG reads the
    /// union whole or not at all. The lowering serves it only where the union's every reader stops at that Limit's zero
    /// check before its first pull (lowerv4 ops/setop/recursive.rs `zero_checked`, result/gate.rs `checks_limit`).
    /// Or the Result of a one-time gate over that Sort or Agg (`checks_gate`): a false or NULL gate pulls nothing.
    pub zero: Option<i32>,
    /// D2732 (cursor form): `early_quiet` false and a CteScan of the union is read on the statement's own root path with no
    /// breaker reading it whole over it, so a pull that may count its rows (a cursor FETCH n, a PL/pgSQL FOR loop, ...)
    /// may stop before the union's end where PG has read only the seed rows it returned. lowerv4 ops/setop/recursive.rs sets
    /// the Generation proto's `GEN_ARG_LOUD` bit, which exec runner/admission.rs refuses on such a pull.
    pub pull_loud: bool,
    /// D2034 (decompose ops/setop/gen_stream.rs): a reader may stop inside a generation, and the term reads another
    /// recursive union's CTE that runs again at every generation, so a generation may never end: the lowering pulls the
    /// term's body a budget at a time (its set's run is the body alone) and hands each pull's rows on before the next.
    pub stream: bool,
    /// D2034 (the seed cliff): a reader may stop before the union's end and the seed is one pipeline that reads no param,
    /// is quiet (`early_quiet`; or pulled a row at a time, `seed_exact`) and is no UNION ALL spool: the seed is the rerun
    /// set `RECURSIVE_SEED_SET` the driver owns, pulled a budget at a time, its end into `work` stopping once `work` holds
    /// the driver's want, as PG's ExecRecursiveUnion returns the non-recursive term's rows as it pulls them; the term
    /// starts only once the seed ended. The seed's pipeline (its one member), or `None`.
    pub seed: Option<u16>,
    /// D2732 (D3377, the cursor form): `seed` is a seed that may raise or call a user or VOLATILE function (not
    /// `early_quiet`'s seed half), read by one CteScan on the statement's root path (`pull_loud`'s shape) under a partial
    /// reader (`PipelineGraph::partial`: a named cursor's FETCH, a counted portal) or (D3373) a reader that may stop (a
    /// LIMIT: the CteScan's pull goes no further than its bound): the driver pulls it a row at a time, its end
    /// into `work` stopping after the row that reaches the want (lowerv4 ops/setop/recursive.rs), so the seed computes no
    /// row past the one the reader takes, as PG's ExecRecursiveUnion pulls the non-recursive term per row it returns.
    /// D4295: or a seed that acts past call-local builtins (a user VOLATILE function that may write what a later call
    /// reads) read whole by one CteScan on the root path, a breaker over it or not: each seed row is handed on before the
    /// next is read, as PG's.
    pub seed_exact: bool,
    /// Construct C2 (decompose ops/setop.rs `map_recursive_union`): the licence the seed and the once-run term inputs run
    /// under ahead of PostgreSQL's row-by-row pull of the seed and its first generation: `effects::quiet_ahead` for quiet
    /// code, else the whole-read licence where every reader reads the union to its end or stops only by raising
    /// (`effects::whole_read_local`, acting code only through call-local builtins with `call_local`'s proof); with
    /// `seed_exact`, the once-run inputs' alone. `None`: `wall` (refused), or a seed that acts past call-local builtins.
    pub seed_ahead: Option<crate::effects::Licence>,
    /// D4470 (#530; decompose ops/setop.rs `calm_early`): the seed and every once-run term input call no user or VOLATILE
    /// function (`effects::setop::acts` over each: what they may do is raise). With `term_volatile` false and built-in
    /// dedup operators, nothing the union runs calls user code, which lowerv4 effects.rs `user_code_of` reads instead of
    /// taking the union for user code (its seed and terms run as their own pipelines, which a feed walk may not name).
    pub calm: bool,
    /// D3373 (D-a, decompose ops/setop.rs `readers`): the WindowAgg (its plan node) straight over a CteScan of the union
    /// that PG does not read whole before its first row: it reads the union as PG's spool_tuples does only through the W10
    /// pulled fill, which lowerv4 ops/setop/recursive.rs requires of it (else refused by D2223's name).
    pub window: Option<i32>,
    /// D3373 (F-1, D2312's pulled form; decompose ops/setop.rs): `seed` holds a target-list SRF not known to end: its end
    /// stops after the row that reaches the driver's want (the pull's budget still doubles), the ProjectSet resuming inside
    /// its set at the next pull, so a set that never ends is read as far as the reader reads.
    pub seed_row: bool,
    /// D3373 (D-e, decompose ops/setop.rs `readers`): an Append under a reader that may stop reads the union in one of its
    /// arms: only its streamed form reads the arms in PG's order, so lowerv4 ops/setop/recursive.rs refuses where a settled
    /// union's feed reads the union's store (its spool runs every arm first).
    pub arms: bool,
    /// D3373 (:99, decompose ops/setop.rs `append_stops`): a later arm of that Append reads the union too, under a stop that
    /// is an INNER or SEMI probe's empty table only: lowerv4 ops/setop/recursive.rs refuses unless each later arm reading the
    /// union's store closes its call over that table (frame/iface `later_arm_closes`).
    pub arms_later: bool,
}

pub enum SetDesc<'p> {
    Identity { keys: ArenaVec<EqKey> },
    /// `form` (ops/setop/stream.rs): how the two inputs are read. Grouped and Ahead: `left` and `right` are the sides'
    /// group slots. Stream: the emit is the left input's own pipeline (`left` is `NO_LEFT`) and `right` the right input's
    /// spool.
    /// `pull_loud` (D2805): a sorted SetOp on the statement's own root path (decompose ops/setop.rs `on_root_path`) whose
    /// left streams a raise (effects::setop::streamed_left_raises): a pull that may count its rows (a cursor FETCH n, a
    /// PL/pgSQL FOR loop, a bounded Execute) stops where PG has read only the left rows it handed on, while the grouped form
    /// reads the left whole at the first pull. lowerv4 ops/setop/lower.rs sets the SetOp proto's `SETOP_ARG_LOUD` bit,
    /// which exec runner/admission.rs refuses on such a pull.
    Set { cmd: SetCmd, hashed: bool, left: u8, right: u8, keys: ArenaVec<EqKey>, groups: u64, tlist: &'p NodeList<'p>, form: SetForm, pull_loud: bool },
    Iter(IterDesc),
}

/// How a sorted or hashed SetOp's two inputs are read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetForm {
    /// Each side a group over the compared columns, the right side read whole (a hashed SetOp, no column, a right Sort, a
    /// quiet right input). Construct C2: the licence of a quiet right input read whole past PostgreSQL's sorted stop
    /// (`effects::quiet_ahead`; decompose ops/setop.rs: granted whenever the grouped form takes a right input by
    /// `effects::setop::stop_unseen`, the same reading); `None` where PostgreSQL reads the right input whole too once the
    /// left has a row (a hashed SetOp, no column, a right Sort) and for the spooled sides lowerv4 refuses
    /// (`SORTED_SPOOL`), so no run ahead holds none. The left's whole read is PostgreSQL's own: setop_retrieve_sorted
    /// reads the left to its end (it stops only when the left has no group left); a reader's stop before that is refused
    /// or marked (`pull_loud`).
    Grouped(Option<crate::effects::Licence>),
    /// The grouped form whose right input may raise but acts nowhere and returns the same rows run to run, read whole
    /// where PG may stop early: the lowering runs it under a recheck licence (an error re-runs the statement in plan
    /// order) or refuses by name (ops/setop/lower.rs).
    Ahead,
    /// The streamed walk (PG's setop_retrieve_sorted): bit `i` of `nulls_first` is key `i`'s NULLS FIRST; `coll` the
    /// collation of every text key (0 when no key is text).
    Stream { nulls_first: u64, coll: u32 },
}

/// A streamed SetOp's left: no slot (the emit is the left input's own pipeline, `SetForm::Stream`).
pub const NO_LEFT: u8 = u8::MAX;

/// W8-RECURSE R2: the `plan_id` of a recursive union's rerun set in `PipelineGraph::reruns` (its `node_id` the union's): no
/// SubPlan id is below 1, and a nest's set is 0.
pub const RECURSIVE_SET: i32 = -1;

/// D2034: the `plan_id` of a recursive union's seed set (`IterDesc::seed`) in `PipelineGraph::reruns` (its `node_id` the
/// union's), beside its term's `RECURSIVE_SET`.
pub const RECURSIVE_SEED_SET: i32 = -2;

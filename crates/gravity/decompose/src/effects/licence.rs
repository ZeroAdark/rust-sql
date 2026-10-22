//! The licence (v5 interfaces §2.2, as corrected by proto1): a departure from PostgreSQL's executor needs a `Licence`, and
//! only `licence` makes one (its fields are private). A family keeps it in its strategy enum (`BuildWhen::Eager(Licence)`),
//! so a departing strategy cannot be built without one. Reordering alone is free and needs none (the user, in session
//! 09-30, `DECISIONS.md` 5 (F): row order under LIMIT / OFFSET / FETCH without ORDER BY, AND / OR conjunct order and the
//! unique-check order may decide rows against an error), and so is the call count and order of an operator class's
//! support function inside work PostgreSQL's plan also does (`DECISIONS.md` 6 (G)). By `DECISIONS.md` 4 (D) b, code that
//! may raise but does not act may run ahead where no plan-order run re-checks it (`licence_unchecked`); its error is the
//! statement's, tagged for the harness (`Licence::tagged`).
use core::cell::Cell;

use types_nodes::jointype::JoinType;
use types_nodes::plannodes::PlannedStmt;
use types_nodes::{Node, NodeTag};

use super::Effects;

/// A departure from PostgreSQL's executor that needs a licence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Departure {
    /// Evaluate code PostgreSQL may never evaluate here: an eager build, read-ahead past a bound, a CASE arm for every
    /// dictionary entry, an InitPlan at open.
    Ahead,
    /// Evaluate it fewer times than PostgreSQL: a cache, a hoist, a table kept where PostgreSQL rebuilds.
    Fewer,
    /// Evaluate it more times than PostgreSQL: a re-open where PostgreSQL keeps state.
    More,
    /// Do not make evaluations PostgreSQL makes, whose result is not needed (the top cut, a hash join's keys over an empty
    /// build, set operations' empty-left skip): an error PostgreSQL would raise there would be lost, so code that may raise
    /// gets no licence, as code that acts gets none.
    Skip,
}

/// What codegen needs to emit a departing strategy. Only `licence` and `licence_unchecked` make one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Licence {
    departure: Departure,
    recheck: bool,
    tagged: bool,
    held: bool,
    whole: bool,
    exact: Option<Proof>,
    accepted: bool,
    _only_effects: (),
}

impl Licence {
    /// An error raised in the licensed code is checked in plan order before it is raised (the batch re-run, `DECISIONS.md`
    /// 4 (D): no error where PostgreSQL returns rows; where it raises, any genuine error passes). After a row of the
    /// statement left, the re-run repairs nothing: the code does not act (`licence` grants no recheck to code that acts),
    /// so the error it may still raise is the accepted class (`accepted`), which the run-ahead tracer counts apart.
    pub const fn recheck(&self) -> bool {
        self.recheck
    }

    pub const fn departure(&self) -> Departure {
        self.departure
    }

    /// `licence_unchecked` granted it to code that may raise: an error that code raises is the statement's as it stands,
    /// perhaps from rows PostgreSQL's plan never reads, and the family marks the code's pipelines with the run-ahead tag
    /// (program `Pipeline::ahead`), so the statement's error names the licensed plan node for the harness's judge.
    pub const fn tagged(&self) -> bool {
        self.tagged
    }

    /// `held_error` granted it: the code's error is held for PostgreSQL's point (op 14's held forms), and the licence
    /// covers that error departure alone.
    pub const fn held_error(&self) -> bool {
        self.held
    }

    /// `whole_read` granted it: the code runs ahead only for a reader that reads the statement whole, which the run-ahead
    /// tracer checks at run time (the guard is recorded here, not trusted).
    pub const fn whole_read(&self) -> bool {
        self.whole
    }

    /// `exact` granted it: the structural proof it rests on (the run-ahead tracer counts it apart, AHEAD-EXACT).
    pub const fn exact(&self) -> Option<Proof> {
        self.exact
    }

    /// `accepted` or `volatile_free` granted it: early work whose departure is only an error PostgreSQL never hits or
    /// extra VOLATILE calls, accepted as a class (the run-ahead tracer counts it apart,
    /// AHEAD-ACCEPTED).
    pub const fn accepted(&self) -> bool {
        self.accepted
    }
}

/// What a structural proof shows (construct C2, D1): PostgreSQL 18.6 runs the same code, to the same first error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProofKind {
    /// The code runs at PostgreSQL's first fetch of the plan node cited, and nothing runs before it.
    FirstFetch,
    /// The plan node cited reads its whole input at its first fetch, so PostgreSQL's demand runs the code whole too.
    PgReadsWhole,
    /// Every raise the code makes is one PostgreSQL's statement makes too, or the statement fails anyway (D2212's cover).
    PgFailsToo,
    /// PostgreSQL surely runs the plan node cited first: it is on the statement's first-fetch chain.
    PgRunsFirst,
}

/// A structural proof and the plan node it cites. Only this file builds one, each constructor checking its evidence
/// against the statement, so a call site cannot assert one; `exact` turns it into a licence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proof {
    kind: ProofKind,
    node: i32,
    /// `subplan_first` alone: the proof orders the cited code's acts too (PostgreSQL runs nothing before it), so
    /// `exact_first` may grant it to code that acts.
    orders_acts: bool,
}

impl Proof {
    pub const fn kind(&self) -> ProofKind {
        self.kind
    }

    /// The plan node the proof cites (its plan_node_id).
    pub const fn node(&self) -> i32 {
        self.node
    }

    /// `FirstFetch`: the CteScan of CTE `plan_id` is the statement's first fetch (`ops::first_fetched_outer`, the walk
    /// `PipelineGraph::first_cte` reads): ExecutorRun's first ExecProcNode reaches it before any other node runs code.
    pub fn first_fetch(stmt: &PlannedStmt<'_>, plan_id: i32) -> Option<Proof> {
        let scan = crate::ops::first_fetched_outer(stmt.planTree)?;
        let c = scan.as_cte_scan().filter(|c| c.ctePlanId == plan_id)?;
        Some(Proof { kind: ProofKind::FirstFetch, node: c.scan.plan.plan_node_id, orders_acts: false })
    }

    /// `PgReadsWhole`: plan node `id` reads its whole input at its first fetch: a Sort (ExecSort's tuplesort_performsort),
    /// a Hash (MultiExecHash) or a plain or hashed Agg (agg_retrieve_direct's one group, agg_fill_hash_table). A
    /// Materialize does not (ExecMaterial pulls one row per fetch), nor a sorted or mixed Agg (a group at a time).
    pub fn pg_reads_whole<'a>(stmt: &'a PlannedStmt<'a>, id: i32) -> Option<Proof> {
        use crate::ops::shared::agg::{AGG_HASHED, AGG_PLAIN};
        let (n, _) = crate::lifetime::node_of(stmt, id)?;
        let whole = match n.node_tag() {
            NodeTag::T_Sort | NodeTag::T_Hash => true,
            NodeTag::T_Agg => n.as_agg().is_some_and(|a| a.aggstrategy == AGG_PLAIN || a.aggstrategy == AGG_HASHED),
            _ => false,
        };
        whole.then_some(Proof { kind: ProofKind::PgReadsWhole, node: id, orders_acts: false })
    }

    /// `PgFailsToo`: plan node `id` is the join whose PostgreSQL run makes the code's raises too (D2212's cover): a Hash
    /// Join (its outer side run before the build PostgreSQL makes first, or a CTE both sides read whole) or a FULL Merge
    /// Join (run to its end it reads both sides whole: ExecMergeJoin's ENDOUTER / ENDINNER fill).
    pub fn pg_fails_too<'a>(stmt: &'a PlannedStmt<'a>, id: i32) -> Option<Proof> {
        let (n, _) = crate::lifetime::node_of(stmt, id)?;
        let join = n.as_hash_join().is_some() || n.as_merge_join().is_some_and(|m| m.join.jointype == JoinType::JOIN_FULL);
        join.then_some(Proof { kind: ProofKind::PgFailsToo, node: id, orders_acts: false })
    }

    /// `PgFailsToo` for a shared CTE's eager build (D1 site 5): Hash Join `id` builds from CTE `plan_id` (a CteScan of it
    /// under the join's Hash), its outer's first-fetch chain reads the same CTE (`ops::first_fetched_outer` from the
    /// join's outer reaches a CteScan of it), and PostgreSQL 18.6 surely runs the join (`pg_demand::never_run` does not
    /// name it: no one-time qual, empty outer or nest gates it). Then the join's first outer fetch runs the CTE: either a
    /// row comes back and the join builds its Hash from the whole CTE, or no row does and the outer has read it whole, so
    /// the CTE's first error is PostgreSQL's too in either order (CteScanNext computes each row once, for whichever reader
    /// pulls it first). A CTE on the build side alone is no such proof: under an empty outer PostgreSQL never runs it.
    /// `m` is the planning context the one-shot `never_run` list is built in.
    pub fn pg_fails_too_shared<'a>(stmt: &'a PlannedStmt<'a>, id: i32, plan_id: i32, m: mcx::Mcx<'_>) -> Option<Proof> {
        let (n, _) = crate::lifetime::node_of(stmt, id)?;
        n.as_hash_join()?;
        let p = n.as_plan()?;
        let outer = crate::ops::first_fetched_outer(p.lefttree).and_then(|o| o.as_cte_scan()).is_some_and(|c| c.ctePlanId == plan_id);
        let build = p.righttree.is_some_and(|h| reads_cte(h, plan_id));
        let runs = || crate::pg_demand::never_run(stmt, m).binary_search(&id).is_err();
        (outer && build && runs()).then_some(Proof { kind: ProofKind::PgFailsToo, node: id, orders_acts: false })
    }

    /// `PgRunsFirst`: plan node `id` is on the statement's first-fetch chain: from it, `ops::first_fetched_outer` reaches
    /// the node the walk from the plan's root reaches (the walk is the same from every node on the chain).
    pub fn pg_runs_first<'a>(stmt: &'a PlannedStmt<'a>, id: i32) -> Option<Proof> {
        let end = |n: Option<Node<'_>>| crate::ops::first_fetched_outer(n).and_then(|e| e.as_plan()).map(|p| p.plan_node_id);
        let (n, _) = crate::lifetime::node_of(stmt, id)?;
        let first = end(stmt.planTree)?;
        (end(Some(n)) == Some(first) && in_tree(stmt.planTree?, id)).then_some(Proof { kind: ProofKind::PgRunsFirst, node: id, orders_acts: false })
    }

    /// `PgRunsFirst` for SubPlan `plan_id` (lowerv4 subplan/per_row.rs `certain_first`, D2117 (b)): PostgreSQL 18.6's first
    /// fetch reaches a Values Scan (`ops::first_fetched_outer` from the root) that makes at least one row of constants and
    /// has no qual and no InitPlan; the SubPlan is one entry of that scan's target list, whole; every entry before it and
    /// the SubPlan's arguments act on nothing, raise nothing and read no exec parameter. So ExecProject of the first row
    /// evaluates the SubPlan before any other code of the statement runs, and its plan's fill runs then whatever it calls.
    /// When the SubPlan is correlated and the list has more rows, PostgreSQL evaluates it again per row, and only a fill
    /// its rescans keep runs once: each of graph pipelines `held` must be outside every rerun set of `g` (decompose's
    /// rerun model, where a kept pipeline is one PostgreSQL's rescan does not run again), else the acts would interleave
    /// with the rows. Cites the SubPlan's plan root. The one proof that orders acts (`exact_first`).
    pub fn subplan_first<'a>(cx: &super::Cx<'_, 'a>, g: &crate::graph::PipelineGraph<'_>, plan_id: i32, held: &[usize]) -> Option<Proof> {
        let stmt = cx.stmt?;
        let n = crate::ops::first_fetched_outer(stmt.planTree)?;
        let v = n.as_values_scan()?;
        let p = &v.scan.plan;
        let konst = |l: Node<'_>| l.as_list().is_some_and(|l| l.iter().all(|c| c.as_const().is_some()));
        if p.qual.iter().next().is_some() || p.initPlan.iter().next().is_some() || v.values_lists.iter().next().is_none() || !v.values_lists.iter().all(konst) {
            return None;
        }
        let quiet = |e: Node<'a>| {
            let f = super::of_expr(cx, e, &super::PLAIN);
            let exec = |x: Node<'_>| x.as_param().is_some_and(|q| q.paramkind == types_nodes::primnodes::ParamKind::PARAM_EXEC);
            !f.acts && !f.may_raise && crate::expr::walk(e, &mut |x| !exec(x))
        };
        let mut sub = None;
        for t in p.targetlist.iter() {
            let e = t.as_target_entry()?.expr;
            if let Some(s) = e.as_sub_plan().filter(|s| s.plan_id == plan_id) {
                sub = Some(s);
                break;
            }
            if !quiet(e) {
                return None;
            }
        }
        let s = sub?;
        if !s.args.iter().all(quiet) {
            return None;
        }
        let correlated = s.parParam.iter().next().is_some() || s.args.iter().next().is_some();
        if correlated && v.values_lists.len() > 1 && !held.iter().all(|&q| g.reruns.iter().all(|r| !r.pipelines.contains(&(q as u16)))) {
            return None;
        }
        let root = stmt.subplans.iter().nth(usize::try_from(plan_id).ok()?.checked_sub(1)?)?.and_then(|r| r.as_plan())?;
        Some(Proof { kind: ProofKind::PgRunsFirst, node: root.plan_node_id, orders_acts: true })
    }
}

/// Plan node `id` is in the tree under `n` (the statement's plan, not a subplan's).
fn in_tree(n: Node<'_>, id: i32) -> bool {
    crate::walk::stack_check();
    if n.as_plan().is_some_and(|p| p.plan_node_id == id) {
        return true;
    }
    let p = n.as_plan();
    let kids = p.into_iter().flat_map(|p| [p.lefttree, p.righttree]).flatten();
    let arms = n.as_append().into_iter().flat_map(|a| a.appendplans.iter());
    kids.chain(arms).any(|k| in_tree(k, id))
}

/// A CteScan of CTE `plan_id` is in the tree under `n` (its plan nodes, EXPLAIN's children: not a subplan's).
fn reads_cte(n: Node<'_>, plan_id: i32) -> bool {
    crate::walk::stack_check();
    n.as_cte_scan().is_some_and(|c| c.ctePlanId == plan_id) || crate::explain::children(n).any(|(k, _)| reads_cte(k, plan_id))
}

/// Not a refusal: the family emits its PostgreSQL form and the statement is served.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Denied;

leader_tls::leader_tls! {
    thread_local! {
        /// Inside `deny_all` (`FULL`) or `deny_all_bounded` (`BOUNDED`); `0` outside.
        static DENY: Cell<u8> = const { Cell::new(0) };
        /// The statement `lowering` lowers: (it acts, it carries a recheck licence).
        static STMT: Cell<(bool, bool)> = const { Cell::new((false, false)) };
        /// The licences `licence` granted on this thread (`grants`), wrapping.
        static GRANTS: Cell<u32> = const { Cell::new(0) };
        /// A `lowering` of a statement that acts entered on this thread since `take_acted` last took it.
        static ACTED: Cell<bool> = const { Cell::new(false) };
    }
}

/// The one rule.
/// - Code that acts: denied, for every departure (a VOLATILE call runs once per row where PostgreSQL needs it, never
///   cached, never ahead).
/// - Code that may raise: granted for `Fewer`; granted with recheck for `Ahead` and `More`; denied for `Skip`.
/// - Anything else: granted.
/// - Inside `deny_all`: denied.
///
/// Code with no effects asks for no licence (its departure is not one): a family keeps a plain strategy for it, so
/// `deny_all` does not force a PostgreSQL form that may not exist. A caller that may get a recheck licence passes the
/// effects of the whole batch it would re-run, so an acting end (a write, a lock, `nextval()` in a target list) denies it.
/// A timeout is not an effect: a licensed eager build that reaches `statement_timeout` where PostgreSQL answers is timing
/// (`DECISIONS.md` 20).
pub fn licence(e: Effects, d: Departure) -> Result<Licence, Denied> {
    if e.acts || denying() || e.may_raise && d == Departure::Skip {
        return Err(Denied);
    }
    GRANTS.with(|g| g.set(g.get().wrapping_add(1)));
    Ok(Licence { departure: d, recheck: e.may_raise && matches!(d, Departure::Ahead | Departure::More), tagged: false, held: false, whole: false, exact: None, accepted: false, _only_effects: () })
}

/// The licences `licence` has granted on this thread (wrapping; `licence_unchecked`'s are not counted, as `deny_all` grants
/// them too). An admission that reads it before and after knows whether its graph holds a departure `deny_all` would
/// deny: exec's admission lowers such a graph again in plan order when its lowering refuses by Shape (runner admission.rs
/// `or_plan_order`).
pub fn grants() -> u32 {
    GRANTS.with(Cell::get)
}

/// Whether a statement `lowering` lowered on this thread since the last call acts (a write, a row lock, an acting call),
/// taken. Exec's admission lowers a licensed form's Shape refusal again in plan order only for a statement that does not
/// act (runner admission.rs `or_plan_order`): a statement that acts has no plan-order run (D3540), and its plan-order
/// form is not PostgreSQL's (the arms of a union under a count PostgreSQL reads from an InitPlan run ahead of that count,
/// calling what PostgreSQL never calls), so its first refusal stands.
pub fn take_acted() -> bool {
    ACTED.with(|a| a.replace(false))
}

/// `DECISIONS.md` 4 (D) b (amending (a) for code that does not act): `Ahead` where no plan-order run re-checks the code
/// (rows may have left the statement before it runs, or it runs again per outer row). Code that may raise is granted
/// without a recheck: an error it raises from rows PostgreSQL's plan never reads is the statement's error where
/// PostgreSQL returns rows, a genuine one, and the licence is `tagged`. Rows never differ.
/// - Code that acts: denied, as by `licence`; so is `user_code`, when the code or what feeds it calls a user function
///   (one may RAISE NOTICE or write through SPI: the ruling keeps such code exact). Acts elsewhere in the statement do
///   not deny it (AGENTS.md "Errors do not constrain evaluation order", 2026-10-17: a failing statement leaves no
///   effects, VOLATILE call counts are free, and the read side of a write is optimized as a SELECT's is).
/// - Inside `deny_all` / `deny_all_bounded` (the plan-order run): granted as outside. No run re-checks the plan-order
///   run, so where a family has no PostgreSQL form there its choice is this or a refusal; the error it raises is the
///   first run's again, now tagged.
///
/// A caller asks it only where it would otherwise refuse: a caller that has a plan-order run for the code asks `licence`
/// first and keeps its recheck, and a family with a PostgreSQL form emits that form when `licence` is denied.
pub fn licence_unchecked(e: Effects, user_code: bool) -> Result<Licence, Denied> {
    if e.acts || user_code {
        return Err(Denied);
    }
    Ok(Licence { departure: Departure::Ahead, recheck: false, tagged: e.may_raise, held: false, whole: false, exact: None, accepted: false, _only_effects: () })
}

/// Construct C2: a run ahead whose error is held for PostgreSQL's point (op 14's held sort / held rows / recheck hold:
/// the error is raised where PostgreSQL's executor would raise it, not where the run met it) holds this licence, of its
/// own kind (`Licence::held_error`), never none: a missing licence always means "not proven". Holding the error covers
/// only the error departure, nothing else the run does ahead, so it is granted only to code that may raise and does
/// nothing else, or acts only inside its calls:
/// - code that acts (a VOLATILE or unknown call, a write, a row lock, a sequence): denied, unless `local` (the effects
///   home's `call_local` proof) holds: every act is a call-local builtin's (nextval, random, gen_random_uuid,
///   clock_timestamp ..: `types_core::quiet::CALL_LOCAL_ACT`) and no other call in the statement can see when it ran.
///   Run ahead, such a call changes no row, only when its value was drawn and how many were, and an extra call is no
///   difference Gravity keeps;
/// - `user_code` (the code or what feeds it calls user code, which may write whatever its volatility:
///   `user_code_may_write`; or a caller's wider reading of user code): denied. A non-VOLATILE SQL or PL/pgSQL function
///   may call a VOLATILE function that writes (D4316), and a held error dropped where PostgreSQL never calls it would
///   keep that write; `licence_unchecked` keeps every user function exact;
/// - inside `deny_all` / `deny_all_bounded`: granted as outside (the held forms are the plan-order run's own).
/// No departure but `Ahead`, no recheck, no tag: the error is PostgreSQL's, at PostgreSQL's point.
pub fn held_error(e: Effects, user_code: bool, local: Option<super::CallLocal>) -> Result<Licence, Denied> {
    if e.acts && local.is_none() || user_code {
        return Err(Denied);
    }
    Ok(Licence { departure: Departure::Ahead, recheck: false, tagged: false, held: true, whole: false, exact: None, accepted: false, _only_effects: () })
}

/// Construct C2: code run ahead of PostgreSQL's demand only for a reader that reads the statement whole (lowerv4
/// `Px::whole_read`: the program is served to such a reader alone, a partial one delegated), so PostgreSQL runs the same
/// code too, in another order. Its order shows only for code that acts (a sequence's values, a write's, a NOTICE's place),
/// so the licence is granted only to code that does not act; what the code raises a whole read raises in PostgreSQL too,
/// and which error comes first is free (`DECISIONS.md` 4 (a)). The guard is recorded in the licence
/// (`Licence::whole_read`): the run-ahead tracer checks at run time that the reader did read whole. Inside `deny_all`:
/// granted as outside (the plan-order run of a whole read reads whole too).
pub fn whole_read(e: Effects) -> Result<Licence, Denied> {
    whole_read_local(e, None)
}

/// Construct C2: `whole_read` for code whose every act is a call-local builtin's (`local`: the effects home's
/// `call_local` proof over that code, the held-error allowlist `types_core::quiet::CALL_LOCAL_ACT`: nextval, random,
/// gen_random_uuid, clock_timestamp ..). Read whole, such code runs every call PostgreSQL's whole read runs, in another
/// order: no row changes, only when each value was drawn, and that order is one PostgreSQL's own plan decides
/// (`DECISIONS.md` 6b), its extra calls free (the user's VOLATILE-count ruling). Any other act (a write, a row lock, a
/// user function, a call that reads or sets what such a call moved) is denied, as by `whole_read`; `None` is
/// `whole_read`. Inside `deny_all`: granted as outside.
pub fn whole_read_local(e: Effects, local: Option<super::CallLocal>) -> Result<Licence, Denied> {
    if e.acts && local.is_none() {
        return Err(Denied);
    }
    Ok(Licence { departure: Departure::Ahead, recheck: false, tagged: false, held: false, whole: true, exact: None, accepted: false, _only_effects: () })
}

/// Construct C2 (licence-typed departures): the licence of code with no effects (it neither raises nor acts), granted
/// everywhere, inside `deny_all` too. Its departure shows nothing (no error, no act, the same rows), so `licence`'s
/// "no effects asks for no licence" stands: this licence denies nothing and forces no form, it only lets a strategy that
/// runs such code ahead hold a `Licence` as every other run ahead does (lint RH, crates/gravity/lint/AHEAD-DEBT). `None`
/// for code with any effect: ask `licence` or `licence_unchecked`.
pub fn unobservable(e: Effects) -> Option<Licence> {
    (e == Effects::NONE).then_some(Licence { departure: Departure::Ahead, recheck: false, tagged: false, held: false, whole: false, exact: None, accepted: false, _only_effects: () })
}

/// Construct C2: the licence of reading ahead code that neither acts nor may raise, whatever its `same_rows`: an
/// `Ahead` departure (rows read past PostgreSQL's stop, a fill before its first fetch) re-runs nothing, so two runs
/// returning other rows is nothing it can show (a WorkTableScan of an outer recursion, a SampleScan without REPEATABLE,
/// a NamedTuplestoreScan read further than PostgreSQL reads them). Granted everywhere, inside `deny_all` too, as
/// `unobservable`'s, with the same fields. Only for an `Ahead` departure: a strategy that runs the code again, or fewer
/// times, keeps `unobservable` (`Effects::NONE`, `same_rows` included). Code that acts is denied: a VOLATILE call acts in
/// the effects model (`DECISIONS.md` 3), random() and clock_timestamp() included, so this licence grants none of them;
/// acting code stays with `held_error` (no user code that may write) and `whole_read_local` (call-local builtins only,
/// with `call_local`'s proof).
pub fn quiet_ahead(e: Effects) -> Option<Licence> {
    (!e.acts && !e.may_raise).then_some(Licence { departure: Departure::Ahead, recheck: false, tagged: false, held: false, whole: false, exact: None, accepted: false, _only_effects: () })
}

/// Construct C2 (D1): the licence of code run ahead where a structural proof (`Proof`, built only here from its checked
/// evidence) shows PostgreSQL 18.6 runs the same code to the same first error. Code that acts is denied: the proof is
/// about where the code runs and what it raises, and the order of acts against the rest of the statement still shows.
/// Inside `deny_all`: granted as outside (a proof holds for the plan-order run too). No recheck, no tag: the error is
/// PostgreSQL's own.
///
/// The sites that ask it, each with what the run-ahead tracer (gravity_driver `ahead_trace`) reads of it. A site whose
/// pipelines `pg_demand` never gates is judged Demanded before any licence is read: there the exact licence types a
/// strategy that held none, and the trace shows no key move.
/// - lowerv4 ops/ctescan/lower.rs `object`, a non-quiet first-fetch CTE's producers run ahead (FirstFetch): never gated,
///   since FirstFetch's premise (PostgreSQL runs the CTE at the statement's first fetch) is what keeps them ungated.
/// - lowerv4 ops/ctescan/lower.rs `object`, a non-quiet first-fetch CTE's parallel fill (FirstFetch): never gated, as
///   above.
/// - lowerv4 ops/hashjoin/probe.rs `ahead_covered`, a covered outer run before the build (PgFailsToo): never gated, an
///   outer PostgreSQL skips on an empty build being a skip, not a node it may never run.
/// - lowerv4 ops/mergejoin/full_whole.rs, a FULL merge's covered inner fillers (PgFailsToo): never gated, a FULL join's
///   inner being read after an empty outer too.
/// - lowerv4 ops/hashjoin/probe.rs `make`, a shared CTE's raising eager build (PgFailsToo, `Proof::pg_fails_too_shared`):
///   gated where its join is, so its runs are read and count as AHEAD-EXACT.
pub fn exact(e: Effects, p: Proof) -> Result<Licence, Denied> {
    if e.acts {
        return Err(Denied);
    }
    Ok(Licence { departure: Departure::Ahead, recheck: false, tagged: false, held: false, whole: false, exact: Some(p), accepted: false, _only_effects: () })
}

/// `exact` for a proof that orders the code's acts as well (`Proof::subplan_first`): PostgreSQL runs the cited code
/// before anything else of the statement, so code that acts is granted too (its acts come first in both engines, and
/// what the caller runs before it is its own check: lowerv4 `certain_first`'s quiet `others`). Any other proof is
/// `exact`'s. The site: lowerv4 ops/subplan/per_row.rs `certain_first`, the kept fill of a SubPlan a Values outer's first
/// row evaluates first (PgRunsFirst), stamped for the run-ahead tracer only where `whole_read` denies it: the fill runs
/// as it ran, AHEAD-EXACT counts it.
pub fn exact_first(e: Effects, p: Proof) -> Result<Licence, Denied> {
    if !p.orders_acts {
        return exact(e, p);
    }
    Ok(Licence { departure: Departure::Ahead, recheck: false, tagged: false, held: false, whole: false, exact: Some(p), accepted: false, _only_effects: () })
}

/// Construct C2, the accepted class: early work (code run ahead of PostgreSQL's demand) that can only raise an error
/// PostgreSQL never hits is accepted as a class, licensed by name with its reason (an error PostgreSQL never
/// hits, from early work; rows never differ), and fixed only where it costs perf. So it is granted to code that does not
/// act, whatever it may raise, and only for `Ahead`: a strategy that skips evaluations or runs them again keeps `licence`'s
/// rule.
/// - Code that acts: denied (an act run ahead is no error; acting code that runs ahead asks `volatile_free`).
/// - `user_code` (the code or what feeds it calls user code that may write: `user_code_may_write`): denied, as by
///   `held_error`; a non-VOLATILE SQL or PL/pgSQL function cannot write, and an extra call's NOTICE is free.
/// - Inside `deny_all` / `deny_all_bounded`: granted as outside (no re-run checks the plan-order run either).
///
/// No recheck, no tag, no held error: the licence is held for the tracer (`Licence::accepted`, program
/// `LicenceStamp::Accepted`), which counts the run apart and never as a key, and codegen reads nothing of it.
pub fn accepted(e: Effects, user_code: bool) -> Result<Licence, Denied> {
    if e.acts || user_code {
        return Err(Denied);
    }
    Ok(Licence { departure: Departure::Ahead, recheck: false, tagged: false, held: false, whole: false, exact: None, accepted: true, _only_effects: () })
}

/// Construct C2, the accepted class on its acting side: code run ahead of PostgreSQL's demand whose acts are extra
/// VOLATILE calls, which are free (an extra call and its side effects, even a call PostgreSQL makes zero
/// times), whatever it may raise (an error PostgreSQL never hits, from early work: the accepted class). A run ahead is a
/// difference only where an effect is read inside the same statement, so the evidence is the effects home's `call_local`
/// over `unit` (the code the strategy runs ahead, plan subtrees or expressions with what they read), read fail closed:
/// - the statement is there and writes nothing (no write node, no data-modifying CTE: a trigger, rule or default may read
///   what an act moved);
/// - every act of `unit` is a call-local builtin's (`types_core::quiet::CALL_LOCAL_ACT`: nextval, random,
///   gen_random_uuid, clock_timestamp ..): a user function in the unit may read what it wrote, and is denied;
/// - nowhere in the statement a user function, a call that reads or sets what a call-local act moves (currval, lastval,
///   setval, setseed ..) or a read of a sequence relation;
/// - no row lock anywhere in the statement (a planned row mark or a LockRows: a lock is no call's act).
///
/// Another call-local call elsewhere (a random draw, a clock read, another nextval of any sequence) sees at most a gap,
/// which is free too. Inside `deny_all` / `deny_all_bounded`: granted as outside. Code that does not act
/// asks `accepted`. No recheck, no tag: the licence is held for the tracer (`Licence::accepted`, program
/// `LicenceStamp::Accepted`), counted apart and never a key, and codegen reads nothing of it.
pub fn volatile_free<'p>(cx: &super::Cx<'_, 'p>, unit: impl IntoIterator<Item = Node<'p>>) -> Result<Licence, Denied> {
    let s = cx.stmt.ok_or(Denied)?;
    if super::call_local(cx, unit).is_none() || locks(s) {
        return Err(Denied);
    }
    Ok(Licence { departure: Departure::Ahead, recheck: false, tagged: false, held: false, whole: false, exact: None, accepted: true, _only_effects: () })
}

/// A row lock anywhere in statement `s`: a planned row mark (FOR UPDATE / SHARE / KEY SHARE) or a LockRows node in its
/// tree or a subplan's.
fn locks<'p>(s: &'p PlannedStmt<'p>) -> bool {
    let mut lock = s.rowMarks.iter().next().is_some();
    crate::rerun::each_plan_node(s, &mut |n| lock |= n.node_tag() == NodeTag::T_LockRows);
    lock
}

/// `f`, the lowering of a statement that `acts` (no plan-order re-run can repeat it) and holds a recheck licence
/// (`recheck`): `statement_acts` and `statement_rechecks` answer inside it (facts::order's D2597 scope). Restored when `f`
/// returns or unwinds (nested lowerings included).
pub fn lowering<T>(acts: bool, recheck: bool, f: impl FnOnce() -> T) -> T {
    struct Restore((bool, bool));
    impl Drop for Restore {
        fn drop(&mut self) {
            STMT.with(|s| s.set(self.0));
        }
    }
    let _r = Restore(STMT.with(|s| s.replace((acts, recheck))));
    if acts {
        ACTED.with(|a| a.set(true));
    }
    f()
}

/// Inside `lowering` of a statement that acts.
pub fn statement_acts() -> bool {
    STMT.with(Cell::get).0
}

/// Inside `lowering` of a statement holding a recheck licence.
pub fn statement_rechecks() -> bool {
    STMT.with(Cell::get).1
}

const FULL: u8 = 1;
const BOUNDED: u8 = 2;

/// Inside `deny_all` or `deny_all_bounded`.
pub fn denying() -> bool {
    DENY.with(Cell::get) != 0
}

/// Inside `deny_all_bounded`: the plan-order run answers a read that stops at a count (a cursor's FETCH n, a protocol
/// Execute's row limit), where PostgreSQL's lazy nodes may never reach the rows a family's form reads ahead (D2316).
pub fn denying_bounded() -> bool {
    DENY.with(Cell::get) == BOUNDED
}

fn deny_as<T>(how: u8, f: impl FnOnce() -> T) -> T {
    struct Restore(u8);
    impl Drop for Restore {
        fn drop(&mut self) {
            DENY.with(|d| d.set(self.0));
        }
    }
    let _r = Restore(DENY.with(|d| d.replace(how)));
    f()
}

/// `f` with every licence denied: the lowering it runs emits each family's PostgreSQL form (the plan-order program) for
/// a read taken to its end. The flag is restored when `f` returns or unwinds.
pub fn deny_all<T>(f: impl FnOnce() -> T) -> T {
    deny_as(FULL, f)
}

/// `deny_all` for a read that stops at a count (`denying_bounded`).
pub fn deny_all_bounded<T>(f: impl FnOnce() -> T) -> T {
    deny_as(BOUNDED, f)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Departure; 4] = [Departure::Ahead, Departure::Fewer, Departure::More, Departure::Skip];

    #[test]
    fn unobservable_code_holds_a_licence_everywhere() {
        assert!(unobservable(Effects::NONE).is_some_and(|l| !l.recheck() && !l.tagged() && l.departure() == Departure::Ahead));
        assert!(deny_all(|| unobservable(Effects::NONE)).is_some(), "the plan-order run grants it too");
        assert!(unobservable(Effects::RAISES).is_none(), "code that may raise asks licence or licence_unchecked");
    }

    /// Construct C2: the Ahead-only quiet licence grants code that neither acts nor raises whatever its rows; the
    /// licence of every other departure keeps `Effects::NONE`.
    #[test]
    fn quiet_ahead_frees_same_rows_for_an_ahead_departure_only() {
        let rows_differ = Effects { acts: false, may_raise: false, same_rows: false };
        let l = quiet_ahead(rows_differ).expect("neither acts nor raises, other rows run to run: granted");
        assert!(l.departure() == Departure::Ahead && !l.recheck() && !l.tagged() && !l.held_error() && !l.whole_read());
        assert_eq!(quiet_ahead(rows_differ), unobservable(Effects::NONE), "unobservable's fields");
        assert!(deny_all(|| quiet_ahead(rows_differ)).is_some(), "the plan-order run grants it too");
        assert!(quiet_ahead(Effects::NONE).is_some());
        assert!(quiet_ahead(Effects { may_raise: true, ..rows_differ }).is_none(), "the same code, if it may raise");
        assert!(quiet_ahead(Effects::RAISES).is_none(), "code that may raise asks licence, held_error or whole_read");
        assert!(quiet_ahead(Effects::ACTS).is_none(), "code that acts");
        assert!(quiet_ahead(Effects::UNKNOWN).is_none(), "unknown code acts");
        let volatile = Effects::of_found(super::super::found::Found { volatility: super::super::Volatility::Volatile, ..super::super::found::Found::CLEAN });
        assert!(volatile.acts && quiet_ahead(volatile).is_none(), "a VOLATILE call acts (random(), clock_timestamp()): denied");
        assert!(unobservable(rows_differ).is_none(), "the licence a re-run or a cache holds keeps same_rows");
    }

    #[test]
    fn the_held_error_rule() {
        let l = held_error(Effects::RAISES, false, None).expect("code that may raise and does nothing else");
        assert!(l.held_error() && !l.recheck() && !l.tagged() && l.departure() == Departure::Ahead);
        assert_eq!(held_error(Effects::ACTS, false, None), Err(Denied), "code that acts");
        assert_eq!(held_error(Effects::UNKNOWN, false, None), Err(Denied), "unknown code acts");
        assert_eq!(held_error(Effects::RAISES, true, None), Err(Denied), "user code may RAISE NOTICE");
        assert!(deny_all(|| held_error(Effects::RAISES, false, None)).is_ok_and(|l| l.held_error()), "the plan-order run's own held forms");
        assert!(lowering(true, false, || held_error(Effects::RAISES, false, None)).is_ok(), "the error is raised at PostgreSQL's point");
        for l in [licence(Effects::RAISES, Departure::Ahead), licence_unchecked(Effects::RAISES, false), unobservable(Effects::NONE).ok_or(Denied)] {
            assert!(!l.expect("granted").held_error(), "no other licence is a held error's");
        }
    }

    #[test]
    fn the_whole_read_rule() {
        let l = whole_read(Effects::RAISES).expect("code that may raise and does not act");
        assert!(l.whole_read() && !l.recheck() && !l.tagged() && !l.held_error() && l.departure() == Departure::Ahead);
        assert!(whole_read(Effects::NONE).is_ok_and(|l| l.whole_read()));
        assert_eq!(whole_read(Effects::ACTS), Err(Denied), "code that acts shows its order");
        assert_eq!(whole_read(Effects::UNKNOWN), Err(Denied), "unknown code acts");
        assert!(deny_all(|| whole_read(Effects::RAISES)).is_ok(), "the plan-order run reads whole too");
        for l in [licence(Effects::RAISES, Departure::Ahead), licence_unchecked(Effects::RAISES, false), held_error(Effects::RAISES, false, None), unobservable(Effects::NONE).ok_or(Denied)] {
            assert!(!l.expect("granted").whole_read(), "no other licence records the whole-read guard");
        }
    }

    #[test]
    fn a_held_error_covers_acts_only_with_the_call_local_proof() {
        use super::super::found::Found;
        use super::super::read::local_in;
        let acts = Effects { acts: true, may_raise: true, same_rows: false };
        let nextval = Found { volatility: super::super::Volatility::Volatile, ..Found::CLEAN };
        let local = local_in(nextval, Some(nextval)).expect("nextval alone is granted");
        assert!(held_error(acts, false, Some(local)).is_ok_and(|l| l.held_error() && !l.recheck() && !l.tagged()));
        assert_eq!(held_error(acts, false, None), Err(Denied), "acts with no proof");
        assert_eq!(held_error(acts, true, Some(local)), Err(Denied), "user code stays denied");
        assert_eq!(local_in(nextval, None), None, "no statement (or one that writes): fail closed");
        assert!(local_in(nextval, Some(nextval)).is_some(), "another nextval in the statement, of any sequence: its place against the unit's is free");
        assert_eq!(local_in(nextval, Some(Found { local_state: true, ..nextval })), None, "currval, lastval, setval or setseed in the statement");
        assert_eq!(local_in(nextval, Some(Found { user_call: true, ..nextval })), None, "a user call in the statement");
        let past = Found { acts_past_call: true, ..Found::CLEAN };
        assert_eq!(local_in(past, Some(past)), None, "setval / setseed / pg_advisory_lock in the unit act past their call");
        assert_eq!(local_in(Found { user_call: true, ..Found::CLEAN }, Some(Found::CLEAN)), None, "a user call in the unit");
        assert!(local_in(Found::CLEAN, Some(nextval)).is_some(), "a unit that does not act minds no nextval");
        assert_eq!(local_in(Found::UNKNOWN, Some(Found::CLEAN)), None, "an unknown node acts past its call");
    }

    /// Construct C2: the whole-read licence grants code that acts only through call-local builtins, with the proof.
    #[test]
    fn a_whole_read_covers_acts_only_with_the_call_local_proof() {
        use super::super::found::Found;
        use super::super::read::local_in;
        let acts = Effects { acts: true, may_raise: true, same_rows: false };
        let nextval = Found { volatility: super::super::Volatility::Volatile, ..Found::CLEAN };
        let local = local_in(nextval, Some(nextval)).expect("nextval alone is granted");
        assert!(whole_read_local(acts, Some(local)).is_ok_and(|l| l.whole_read() && !l.recheck() && !l.tagged()));
        assert_eq!(whole_read_local(acts, None), Err(Denied), "acts with no proof");
        assert_eq!(whole_read(acts), Err(Denied), "whole_read is the form with no proof");
        assert!(deny_all(|| whole_read_local(acts, Some(local))).is_ok(), "the plan-order run grants it as outside");
    }

    /// Construct C2 (D1): the exact licence grants code that does not act, raising or not, inside the plan-order run too,
    /// with no recheck and no tag, and records its proof; code that acts is denied whatever the proof.
    #[test]
    fn the_exact_rule() {
        let p = Proof { kind: ProofKind::FirstFetch, node: 7, orders_acts: false };
        for e in [Effects::NONE, Effects::RAISES, Effects { acts: false, may_raise: true, same_rows: false }] {
            let l = exact(e, p).expect("does not act: granted");
            assert!(l.exact() == Some(p) && !l.recheck() && !l.tagged() && !l.held_error() && !l.whole_read() && l.departure() == Departure::Ahead);
            assert!(deny_all(|| exact(e, p)).is_ok_and(|l| l.exact() == Some(p)), "the plan-order run grants it too");
        }
        assert_eq!(exact(Effects::ACTS, p), Err(Denied), "code that acts shows the order of its acts");
        assert_eq!(exact(Effects::UNKNOWN, p), Err(Denied), "unknown code acts");
        for l in [licence(Effects::RAISES, Departure::Ahead), licence_unchecked(Effects::RAISES, false), held_error(Effects::RAISES, false, None), whole_read(Effects::RAISES), unobservable(Effects::NONE).ok_or(Denied)] {
            assert_eq!(l.expect("granted").exact(), None, "no other licence carries a proof");
        }
        assert_eq!((p.kind(), p.node()), (ProofKind::FirstFetch, 7));
    }

    /// Construct C2, the accepted class: code that does not act is granted whatever it may raise, inside the
    /// plan-order run and an acting statement too, Ahead only, with no recheck, tag, held error, guard or proof; code that
    /// acts and user code that may write are denied; no other licence is accepted.
    #[test]
    fn the_accepted_rule() {
        for e in [Effects::NONE, Effects::RAISES, Effects { acts: false, may_raise: true, same_rows: false }] {
            let l = accepted(e, false).expect("does not act: granted");
            assert!(l.accepted() && l.departure() == Departure::Ahead);
            assert!(!l.recheck() && !l.tagged() && !l.held_error() && !l.whole_read() && l.exact().is_none(), "held for the tracer only");
            assert!(deny_all(|| accepted(e, false)).is_ok_and(|l| l.accepted()), "the plan-order run grants it too");
            assert!(lowering(true, false, || accepted(e, false)).is_ok(), "an acting statement: the code itself does not act");
        }
        assert_eq!(accepted(Effects::ACTS, false), Err(Denied), "code that acts asks volatile_free");
        assert_eq!(accepted(Effects::UNKNOWN, false), Err(Denied), "unknown code acts");
        assert_eq!(accepted(Effects { acts: true, may_raise: false, same_rows: true }, false), Err(Denied), "an act with no error");
        assert_eq!(accepted(Effects::RAISES, true), Err(Denied), "user code that may write");
        let p = Proof { kind: ProofKind::PgRunsFirst, node: 3, orders_acts: true };
        let others = [licence(Effects::RAISES, Departure::Ahead), licence_unchecked(Effects::RAISES, false), held_error(Effects::RAISES, false, None), whole_read(Effects::RAISES), unobservable(Effects::NONE).ok_or(Denied), quiet_ahead(Effects::NONE).ok_or(Denied), exact(Effects::RAISES, p), exact_first(Effects::ACTS, p)];
        for l in others {
            assert!(!l.expect("granted").accepted(), "no other licence is the accepted class");
        }
    }

    #[test]
    fn the_licence_rule() {
        for d in ALL {
            let l = licence(Effects::NONE, d).expect("no effects: granted");
            assert!(!l.recheck() && l.departure() == d);
            assert_eq!(licence(Effects::ACTS, d), Err(Denied), "{d:?}: code that acts is denied");
            assert_eq!(licence(Effects::UNKNOWN, d), Err(Denied), "{d:?}: unknown code acts");
        }
        assert!(licence(Effects::RAISES, Departure::Ahead).is_ok_and(|l| l.recheck()));
        assert!(licence(Effects::RAISES, Departure::More).is_ok_and(|l| l.recheck()));
        assert!(licence(Effects::RAISES, Departure::Fewer).is_ok_and(|l| !l.recheck()));
        assert_eq!(licence(Effects::RAISES, Departure::Skip), Err(Denied), "a skipped evaluation would lose its error");
    }

    #[test]
    fn deny_all_denies_and_restores() {
        assert!(!denying());
        for d in ALL {
            assert_eq!(deny_all(|| licence(Effects::NONE, d)), Err(Denied));
        }
        assert!(deny_all(|| deny_all(denying) && denying()), "nested scopes");
        assert!(!denying(), "deny_all restores");
        let r = std::panic::catch_unwind(|| deny_all(|| panic!("unwinds")));
        assert!(r.is_err() && !denying(), "deny_all restores on unwind");
    }

    /// `DECISIONS.md` 4 (D) b: code that may raise runs ahead with no recheck, tagged; code with no effects untagged; code
    /// that acts and user code are denied, an acting statement's read side granted; the plan-order run grants it as outside.
    #[test]
    fn the_unchecked_ahead_rule() {
        let l = licence_unchecked(Effects::RAISES, false).expect("may raise, does not act: granted");
        assert!(l.tagged() && !l.recheck() && l.departure() == Departure::Ahead);
        assert!(licence_unchecked(Effects::NONE, false).is_ok_and(|l| !l.tagged() && !l.recheck()));
        assert_eq!(licence_unchecked(Effects::ACTS, false), Err(Denied), "code that acts");
        assert_eq!(licence_unchecked(Effects::UNKNOWN, false), Err(Denied), "unknown code acts");
        assert_eq!(licence_unchecked(Effects::RAISES, true), Err(Denied), "user code stays exact");
        assert!(lowering(true, false, || licence_unchecked(Effects::RAISES, false)).is_ok_and(|l| l.tagged()), "an acting statement's read side");
        assert!(lowering(false, false, || licence_unchecked(Effects::RAISES, false)).is_ok_and(|l| l.tagged()));
        assert!(deny_all(|| licence_unchecked(Effects::RAISES, false)).is_ok_and(|l| l.tagged()), "the plan-order run");
        assert!(deny_all_bounded(|| licence_unchecked(Effects::NONE, false)).is_ok_and(|l| !l.tagged()));
        assert_eq!(deny_all(|| licence_unchecked(Effects::ACTS, false)), Err(Denied), "acting code, in the plan-order run");
        assert_eq!(deny_all(|| licence_unchecked(Effects::RAISES, true)), Err(Denied), "user code, in the plan-order run");
        assert_eq!(deny_all(|| licence(Effects::RAISES, Departure::Ahead)), Err(Denied), "the recheck licence stays denied there");
        assert!(licence(Effects::RAISES, Departure::Ahead).is_ok_and(|l| l.recheck() && !l.tagged()), "a recheck stays a recheck");
    }

    /// `grants` counts each licence `licence` grants, never a denial nor `licence_unchecked`'s (the plan-order run grants
    /// those too, so they never send an admission to it).
    #[test]
    fn grants_count_the_granted_licences() {
        let g0 = grants();
        assert!(licence(Effects::RAISES, Departure::Ahead).is_ok());
        assert_eq!(grants(), g0.wrapping_add(1), "a grant");
        assert!(licence(Effects::ACTS, Departure::Ahead).is_err() && deny_all(|| licence(Effects::RAISES, Departure::Ahead)).is_err());
        assert!(licence_unchecked(Effects::RAISES, false).is_ok());
        assert_eq!(grants(), g0.wrapping_add(1), "denials and unchecked licences are not counted");
    }

    /// D2316: a bounded plan-order run denies every licence as `deny_all` does and reads as bounded; a full one nested in
    /// it does not; each scope restores the one around it.
    #[test]
    fn deny_all_bounded_is_a_plan_order_run_that_stops() {
        assert!(!denying_bounded());
        assert_eq!(deny_all_bounded(|| licence(Effects::NONE, Departure::Ahead)), Err(Denied));
        assert!(deny_all_bounded(|| denying() && denying_bounded() && deny_all(|| denying() && !denying_bounded()) && denying_bounded()));
        assert!(deny_all(|| !denying_bounded()));
        assert!(!denying() && !denying_bounded(), "restored");
    }
}

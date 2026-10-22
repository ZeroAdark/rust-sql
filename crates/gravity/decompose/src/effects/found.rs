//! F5's facts of a plan subtree or an expression (`Found`, walked by decompose rerun.rs), its golden tokens, and the
//! per-call and per-node rules that walk reads. Each site keeps its own answer for an oid the facts cannot name.
use gravity_program::ArenaRef;
use types_nodes::list::NodeList;
use types_nodes::plannodes::PlannedStmt;
use types_nodes::primnodes::SubPlan;

use crate::catalog::Facts;
use crate::effects::{FuncProps, Parallel, Volatility};

/// What a walk found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
    /// The most volatile call (`contain_volatile_functions` / `contain_mutable_functions` over the whole subtree).
    pub volatility: Volatility,
    /// No call or node that can raise on some input: every function leakproof (a builtin: or raise-free by
    /// `effects::quiet_call`, or a call whose arguments `effects::quiet_args` passes) and strict, no I/O or
    /// domain coercion, no subscript, MIN / MAX, XML / JSON node, no scalar / row / MULTIEXPR sublink that may see a second row, no LIMIT /
    /// OFFSET / frame offset that is not a non-negative constant, no row lock, write or run-time partition pruning.
    pub raise_free: bool,
    /// No node whose rows may differ between two runs with the same params: a SampleScan (its seed is drawn per scan
    /// without REPEATABLE), a WorkTableScan of a recursion outside the subtree, a NamedTuplestoreScan, LockRows (it
    /// re-fetches the newest version), a TID scan's CURRENT OF, a write (a data-modifying CTE's ModifyTable), a foreign /
    /// custom scan or any node the walk does not know.
    pub rescan_stable: bool,
    /// D1035: the worst parallel hazard (`max_parallel_hazard` over the subtree, which pgrust's planner
    /// never runs: planner lib.rs `assess_parallel = false`): each call's `proparallel` (a user function's, an unknown
    /// oid's and any node the walk does not know: Unsafe), and a scan of a temporary relation (Restricted: PostgreSQL
    /// never plans one parallel, `set_rel_consider_parallel`) or of one the facts cannot name (Unsafe).
    pub parallel: Parallel,
    /// F4 (D1906): a call of a user function (oid >= FIRST_NORMAL_OID) or of one the facts cannot name.
    pub user_call: bool,
    /// C2 (the held-error allowlist): a node that acts past its own call, i.e. what makes `volatility` VOLATILE but a call
    /// of a `types_core::quiet::CALL_LOCAL_ACT` builtin: a VOLATILE call of any other function, a call the facts cannot
    /// name, a domain CHECK, any node the walk does not know. `Effects::acts` without it: every act is call-local.
    pub acts_past_call: bool,
    /// A call of a `CALL_LOCAL_STATE` builtin (currval, lastval, setval, setseed ..): it reads or sets the state a
    /// call-local act moves, so none runs ahead in its statement.
    pub local_state: bool,
    /// D4144: a VOLATILE call whose place among the statement's calls shows in a value: user code, a sequence or clock
    /// call (`CALL_LOCAL_ACT` but its random draws, `CALL_LOCAL_DRAW`), a call that reads or sets their state
    /// (`CALL_LOCAL_STATE`), or a call the facts cannot name (lowerv4 volatile.rs `Call::Order`); a node the walk does not
    /// know sets it as every field (`UNKNOWN`). A SubPlan's call reads a data-modifying CTE as its store
    /// (`rerun::of_subplan_stored`), so that CTE's write sets none there.
    pub order_call: bool,
    /// D3816: a call that acts (read.rs `call_acts`: a VOLATILE one not in the pinned non-acting list, or one the facts
    /// cannot name). Only the statement's acting set reads it (read.rs `of_statement`); `volatility` still
    /// counts the listed ones, so every other reading (what may cache, skip a rerun or run once) takes them as VOLATILE.
    pub acts: bool,
    /// D4316 / D4323: some node may run user code that writes, whatever its volatility (`effects::user_code_may_write` at
    /// each call the walk reads, an aggregate's support functions too, and `effects::node_may_write` at each expression
    /// node: a user type's I/O, a user domain's cast, a builtin that runs its own query). A STABLE or IMMUTABLE user
    /// function may call a VOLATILE writer, which `acts` (read from declared volatility) does not show. A node the walk
    /// does not know and a domain CHECK set it.
    pub may_write: bool,
}

impl Found {
    pub const CLEAN: Found = Found { volatility: Volatility::Immutable, raise_free: true, rescan_stable: true, parallel: Parallel::Safe, user_call: false, acts_past_call: false, local_state: false, order_call: false, acts: false, may_write: false };
    /// A coercion to a domain: PostgreSQL 18.6 runs every CHECK of the domain chain at each one (ExecEvalConstraintCheck),
    /// and a CHECK may call user or VOLATILE code. One whose constraints the facts cannot read (`Facts::domain_checks`;
    /// D3796: the F4 walk reads those it can) fails closed: it acts, calls user code and may raise.
    pub const DOMAIN_CHECK: Found = Found { volatility: Volatility::Volatile, raise_free: false, rescan_stable: true, parallel: Parallel::Safe, user_call: true, acts_past_call: true, local_state: false, order_call: true, acts: true, may_write: true };
    pub const UNKNOWN: Found = Found { volatility: Volatility::Volatile, raise_free: false, rescan_stable: false, parallel: Parallel::Unsafe, user_call: true, acts_past_call: true, local_state: false, order_call: true, acts: true, may_write: true };

    pub(crate) fn add(&mut self, o: Found) {
        self.parallel = self.parallel.max(o.parallel);
        self.volatility = self.volatility.max(o.volatility);
        self.raise_free &= o.raise_free;
        self.rescan_stable &= o.rescan_stable;
        self.user_call |= o.user_call;
        self.acts_past_call |= o.acts_past_call;
        self.local_state |= o.local_state;
        self.order_call |= o.order_call;
        self.acts |= o.acts;
        self.may_write |= o.may_write;
    }
}

impl Found {
    /// C2 (the held-error allowlist): a call of `oid` whose pg_proc row is `p`. A VOLATILE call acts past its call unless
    /// the builtin is a `types_core::quiet::CALL_LOCAL_ACT` row; a `CALL_LOCAL_STATE` call is marked.
    pub(crate) fn local_marks(&mut self, oid: u32, p: &FuncProps) {
        self.acts_past_call |= p.volatility == Volatility::Volatile && !types_core::quiet::call_local_act(oid);
        self.local_state |= types_core::quiet::call_local_state(oid);
        let (act, draw) = (types_core::quiet::call_local_act(oid), types_core::quiet::call_local_draw(oid));
        self.order_call |= p.volatility == Volatility::Volatile && (crate::effects::user_code(oid) || act && !draw || types_core::quiet::call_local_state(oid));
    }

    /// A node that reads the session's state at each run (CURRENT_DATE, CURRENT_USER, ..: an SQLValueFunction): stable
    /// (`contain_mutable_functions`), never raising.
    pub fn stable_node(&mut self) {
        self.volatility = self.volatility.max(Volatility::Stable);
    }
}

/// D3374: SubPlan `s` (`f`: rerun.rs `of_subplan`'s facts for it) may run where PostgreSQL skips it, unseen but for its
/// own sublink's second row (lowerv4 holds that one for an interpreter island's read): no user function (its code may
/// raise or write, whatever its marking), nothing VOLATILE, and raise-free, or else its test, its args and its plan each
/// raise-free (rerun.rs `of_expr` / `of_plan`: a nested SubPlan's own raise counts, a row lock raises, a data-modifying
/// CTE is unknown).
pub fn sub_plan_quiet_but_rows<'p>(stmt: &'p PlannedStmt<'p>, facts: &dyn Facts, arena: ArenaRef, s: &'p SubPlan<'p>, f: Found) -> bool {
    if f.user_call || f.volatility == Volatility::Volatile {
        return false;
    }
    if f.raise_free {
        return true;
    }
    let Some(root) = crate::rerun::subplan_root(stmt, s.plan_id) else { return false };
    s.testexpr.into_iter().chain(s.args.iter()).all(|n| crate::rerun::of_expr(stmt, facts, arena, n).raise_free) && crate::rerun::of_plan(stmt, facts, arena, root).raise_free
}

/// F5's raise-free answer for a call of `oid` whose pg_proc row is `p`, over `args` and the node's input collation where
/// the node has them: the effects home's raise-free rules (`quiet_call`: a leakproof builtin, or one whose rule needs no
/// argument, types_core::quiet::call; `quiet_args` over the arguments: an integer `%` / `/` by a constant, a privilege
/// text; `quiet_pattern`: a builtin LIKE / ILIKE / regex match over a good constant pattern, as `of_expr` reads
/// it; heldout rnd-3226), an argument rule only over a strict function. A `quiet_call` builtin raises on no input, a
/// NULL one too, as the home's `quiet_node` reads it (pg_stat_get_activity is not strict: its NULL lists every backend).
/// A user function counts only as the builtin it runs (`effects::body`), never by its
/// own LEAKPROOF mark: its body may raise or act whatever the mark says (`effects::leakproof`). Over arguments, the home's
/// collation bound too (`effects::collation_set`): collatable arguments of conflicting implicit collations leave the
/// input collation indeterminate, and texteq / text_lt .. raise 42P22 at their first non-null input. `quiet_call` is
/// `effects::quiet_call(facts, oid)`, which the caller may keep per function (rerun.rs `Walk::call`). D3788: `list`, `args`
/// are a ScalarArrayOpExpr's (a scalar and an array): `quiet_pattern_list` reads its constant pattern list, the scalar
/// rules (`quiet_args`, `quiet_pattern`) never apply. D3979: `quiet_collated` over the input collation too, as
/// `quiet_node` reads a call: a case map under the default, C or POSIX collation.
pub fn call_raise_free(facts: &dyn Facts, quiet_call: bool, p: &FuncProps, oid: u32, args: Option<(&NodeList<'_>, u32)>, list: bool) -> bool {
    if list {
        let listed = args.is_some_and(|(a, coll)| crate::effects::quiet_pattern_list(facts, oid, a, coll) && crate::effects::collation_set(oid, coll, a));
        return (quiet_call || listed) && p.strict;
    }
    let by_args = args.is_some_and(|(a, coll)| crate::effects::quiet_args(oid, a) || crate::effects::quiet_pattern(facts, oid, a, coll) || crate::effects::quiet_collated(facts, oid, coll));
    (quiet_call || by_args && p.strict) && args.is_none_or(|(a, coll)| crate::effects::collation_set(oid, coll, a))
}

/// The F5 tokens of a golden line: ` vol=s` / ` vol=v` (immutable prints none), ` raise-free`.
pub fn dump_f5(out: &mut dyn core::fmt::Write, volatility: Volatility, raise_free: bool) -> core::fmt::Result {
    if volatility != Volatility::Immutable {
        write!(out, " vol={}", volatility.letter())?;
    }
    if raise_free {
        write!(out, " raise-free")?;
    }
    Ok(())
}

//! FACTS-1b (M8-plan §7.1): the catalog properties of a function or an operator the facts read (`rescan.rs` F1 / F5, `join.rs`
//! F2), answered by `Facts::func` / `Facts::oper`. The default is the builtin catalog (`procfacts.rs`, generated from
//! PostgreSQL 18.6's .dat files with the BKI column defaults); an oid it does not hold (a user function, or one initdb's SQL
//! scripts created) has no properties, and every fact that reads one is false: taken as volatile, non-strict, not leakproof,
//! hashing in no opfamily.

pub(crate) mod procfacts;
/// The agg family's sibling file of the home: its moved planning-half decisions.
pub mod agg;

// The effects home (v5 interfaces §2.2): what running some code may do, read from the catalog-generated table above (or
// `pg_proc` on the server, through `Facts::func`), and the licence a departure from PostgreSQL's executor needs.
mod fold;
pub mod found;
mod licence;
mod read;
/// The scan family's sibling file of the home (construct bundle scan-01).
pub(crate) mod scan;
pub mod setop;
mod sort;
mod user;

pub use licence::{accepted, deny_all, deny_all_bounded, denying, denying_bounded, exact, exact_first, grants, held_error, licence, licence_unchecked, lowering, quiet_ahead, statement_acts, statement_rechecks, take_acted, unobservable, volatile_free, whole_read, whole_read_local, Denied, Departure, Licence, Proof, ProofKind};
pub use read::{bit_index_class, body, call_local, CallLocal, divisor_class, each_list_text, unit_class, json_is_quiet, leakproof, numeric_typmod_class, of_call, of_expr, of_list, of_ops, of_plan, of_statement, of_statement_acts, pattern_good, pattern_list, pattern_list_good, pattern_operand, pattern_quiet, plan_word, pool_safe, pure, quiet_args, quiet_call, quiet_pairs, quiet_node, quiet_pattern, quiet_pattern_list, row_compare_quiet, sample_class, text_arg_class, xml_quiet, Cx, Scope, PLAIN};
pub use fold::{raises_by_order, scan_raises};
pub use sort::{reads_raising_sort, sort_raises};

pub use read::agg_quiet;
pub use read::quiet_collated;
pub use read::like_image_quiet;
/// A call's input collation is set (or nothing it reads is collatable): the one collation rule every raise-free reading
/// asks for a call, a ScalarArrayOpExpr or a GREATEST / LEAST ("could not determine which collation" otherwise).
pub use read::collation_set;
/// ops/window.rs D1354: an expression acts (its VOLATILE reading), with no raise reading.
pub use read::expr_acts;
/// D3816: the acting set's one answer to "a call acts" (F5's `Found::acts` and lowerv4's acting reading ask it).
pub use read::call_acts;
pub use read::{of_join, of_source};
pub use read::agg_oid_quiet;
pub use read::{num_bounded, num_digits};
/// One node's own facts, its children apart (a reader that walks the children itself).
pub use read::of_node;
/// D2044: a call that is user code (its raise can leave SPI, GUC or snapshot state behind).
pub use read::{runs_named_code, user_code};
/// The one answer to "a call runs user code" (F5 and the lowering's readings ask it).
pub use user::{code_may_write, node_may_write, runs_user_code, user_code_may_write};

/// OID-24: builtin `funcid` is not VOLATILE but runs a query of its own over the relations its arguments name, so the call
/// runs whatever those relations call (a view's nextval, a user function in its target list): table_to_xml and its
/// schema_ / database_ / _and_xmlschema siblings (`types_core::quiet::RUNS_OWN_QUERY`). PostgreSQL 18.6's other builtins that run a
/// query through SPI are VOLATILE (query_ / cursor_to_xml*, ts_stat, ts_rewrite over a query text), so their volatility
/// already says so, or read only catalogs (pg_get_viewdef / pg_get_ruledef, schema_ / database_to_xmlschema).
/// tests/effects_home.rs derives that split from pg_proc.dat and the reference sources: a builtin that gains an SPI call,
/// or a list gone stale, fails the decompose tests.
pub fn runs_own_query(funcid: u32) -> bool {
    types_core::quiet::runs_own_query(funcid)
}

/// OID-24: builtin `funcid` is not VOLATILE and runs a statement of its own through SPI over the catalogs alone
/// (pg_get_viewdef / pg_get_ruledef, schema_ / database_to_xmlschema: `types_core::quiet::CATALOG_QUERY`). No user code
/// runs, so `runs_own_query` and the user-code answers say no; a caller that must know whether a call runs a statement
/// inside its own (D4583) reads this beside them. tests/effects_home.rs pins it to that test's `CATALOG_ONLY`.
pub fn runs_catalog_query(funcid: u32) -> bool {
    types_core::quiet::catalog_query(funcid)
}

/// What a call of aggregate (or window function) `oid` runs beyond its own pg_proc row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggCalls {
    /// Nothing more: a builtin, whose pg_proc.dat row states its volatility by hand.
    Own,
    /// A user aggregate's support functions: CREATE AGGREGATE stores its own row IMMUTABLE whatever they do.
    Support(crate::catalog::AggSupport),
    /// A user aggregate the catalog has no pg_aggregate row for (unknown), or a user window function that is not one.
    NoRow,
}

pub fn agg_calls(f: &dyn crate::catalog::Facts, oid: u32) -> AggCalls {
    if oid < crate::catalog::typefacts::FIRST_NORMAL_OID {
        return AggCalls::Own;
    }
    f.agg_support(oid).map_or(AggCalls::NoRow, AggCalls::Support)
}

/// One more call of aggregate `oid`'s final function on a state it already finalized (a top's order pass, then the emit):
/// a builtin's own pg_proc row, a user aggregate's finalfn (none: nothing runs), unknown without a pg_aggregate row.
pub fn of_final(f: &dyn crate::catalog::Facts, oid: u32) -> Effects {
    match agg_calls(f, oid) {
        AggCalls::Own => of_call(f, oid),
        AggCalls::Support(s) if s.final_ == 0 => Effects::NONE,
        AggCalls::Support(s) => of_call(f, s.final_),
        AggCalls::NoRow => Effects::UNKNOWN,
    }
}

/// Every call aggregate `oid` makes over a run, its fold and its final: a builtin's own pg_proc row, a user aggregate's
/// pg_aggregate support functions, unknown without a pg_aggregate row.
pub fn of_agg(f: &dyn crate::catalog::Facts, oid: u32) -> Effects {
    match agg_calls(f, oid) {
        AggCalls::Own => of_call(f, oid),
        AggCalls::Support(s) => s.each().fold(Effects::NONE, |e, g| e.and(of_call(f, g))),
        AggCalls::NoRow => Effects::UNKNOWN,
    }
}

/// The aggregates' own ORDER BY / DISTINCT sorts in `n` (errorhold, D1602): `ACTS` when one sorts with user code, a user
/// operator (USING: its opfamily's comparator, which the expression walk never sees: it reads the operator's oprcode only)
/// or a sort column of a type past FirstNormalObjectId whose compare acts (`of_compare`: a user type's comparators are a
/// user's; AGC-17: an enum's are initdb's enum_cmp / enum_eq and a domain's its base's, so they do not count; a record
/// sorts with record_cmp over its members' built-in comparators, so it does not count); else `NONE` (the group's own
/// sort, run as the group is handed on).
pub fn of_agg_sorts(f: &dyn crate::catalog::Facts, n: types_nodes::Node<'_>) -> Effects {
    use crate::catalog::typefacts::FIRST_NORMAL_OID;
    let mut hit = false;
    crate::expr::walk(n, &mut |x| {
        if let Some(r) = x.as_aggref() {
            let col = |i: u32| r.args.iter().filter_map(|t| t.as_target_entry()).find(|t| t.ressortgroupref == i).and_then(|t| crate::catalog::expr_type::type_of(t.expr)).is_some_and(|t| t >= FIRST_NORMAL_OID && of_compare(f, t).acts);
            hit |= r.aggorder.iter().chain(r.aggdistinct.iter()).filter_map(|c| c.as_sort_group_clause()).any(|c| c.sortop >= FIRST_NORMAL_OID || c.eqop >= FIRST_NORMAL_OID || col(c.tleSortGroupRef));
        }
        !hit
    });
    if hit { Effects::ACTS } else { Effects::NONE }
}

/// A table AM may run operator `opno` on every row (`rows_zone_form`): its function is an IMMUTABLE LEAKPROOF builtin.
pub fn am_pushable(opno: u32, opfuncid: u32) -> bool {
    let func = if opfuncid != 0 { Some(opfuncid) } else { builtin_oper(opno).map(|p| p.code) };
    func.and_then(builtin_func).is_some_and(|f| f.volatility == Volatility::Immutable && f.leakproof)
}

/// A scan of index `indexid` keyed by operators `opnos`: the operators' functions when every key column's operator class
/// reaches only initdb functions (D2063's `IndexOpc`, all key columns: skip scan reads key-less leading ones) and each
/// operator is an initdb one; else unknown (a user's support function, or a user operator, runs per probe).
pub fn of_index(f: &dyn crate::catalog::Facts, indexid: u32, opnos: impl IntoIterator<Item = u32>) -> Effects {
    if !f.index_opc(indexid).all() {
        return Effects::UNKNOWN;
    }
    opnos.into_iter().fold(Effects::NONE, |e, op| {
        e.and(match f.oper(op) {
            Some(p) if op < crate::catalog::typefacts::FIRST_NORMAL_OID && p.code != 0 && !p.poly => of_call(f, p.code),
            _ => Effects::UNKNOWN,
        })
    })
}

/// A key, a sort or a min / max over type `typid` that compares and hashes by the type's default opclasses' procs
/// (`TypeFacts` eq / cmp / hash; an enum's are initdb's enum_eq / enum_cmp / hashenum): those procs' calls when the builtin
/// type table holds the type (a domain: its base) or it is an enum (AGC-20), a named composite's record procs over its
/// columns' (AGC-20), and the type is no pseudo-type (the catalog's typtype; a source without a catalog row: the bootstrap catalog's, where the table's
/// pseudo-types are record and anyarray), a record's or anyarray's procs calling each member type's (a user's, maybe); else
/// unknown (a user type's procs are a user's functions).
pub fn of_type(f: &dyn crate::catalog::Facts, typid: u32) -> Effects {
    of_procs(f, typid, true)
}

/// A sort, a merge or an ordered-set final that only compares values of type `typid`: `of_type` over the btree procs
/// (eq / cmp) alone. The hash proc is no part of a compare, and PostgreSQL marks no builtin hash function leakproof
/// (hashint4 included), so asking `of_type` there would call every type's compare one that may raise.
pub fn of_compare(f: &dyn crate::catalog::Facts, typid: u32) -> Effects {
    of_procs(f, typid, false)
}

fn of_procs(f: &dyn crate::catalog::Facts, typid: u32, hash: bool) -> Effects {
    of_procs_at(f, typid, hash, 0)
}

/// `of_procs` at nesting `depth` of composite columns (at most 8 levels, as lowerv4's `type_free`; deeper: unknown).
fn of_procs_at(f: &dyn crate::catalog::Facts, typid: u32, hash: bool, depth: u8) -> Effects {
    crate::walk::stack_check(); // D3748
    let Some(t) = crate::catalog::typefacts::facts_of(f, typid) else { return Effects::UNKNOWN };
    let pseudo = crate::catalog::pseudo_type(f, typid);
    // AGC-20: a named composite (a domain over one keys as its base) compares by initdb's record_eq / btrecordcmp and
    // hashes by hash_record (`quiet::RECORD_KEY_PROCS`), which call each live column's own default procs (rowtypes.c; a
    // dropped column skipped): those calls and the columns' effects. A column type nothing reads (a user type's), a
    // descriptor that cannot be read or nesting past 8 levels: unknown. The anonymous record is a pseudo-type and stays
    // unknown.
    if !pseudo && f.type_row(t.base).is_some_and(|b| b.typtype == b'c') {
        let mut cols = Effects::NONE;
        if depth >= 8 || !f.row_cols(t.base, &mut |a| cols = cols.and(of_procs_at(f, a.typid, hash, depth + 1))) {
            return Effects::UNKNOWN;
        }
        let [eq, cmp, h] = types_core::quiet::RECORD_KEY_PROCS.map(|r| r.0);
        return [eq, cmp, if hash { h } else { 0 }].into_iter().filter(|&p| p != 0).fold(cols, |e, p| e.and(of_call(f, p)));
    }
    // AGC-20: an enum (a domain over one keys as its base, `facts_of`) is no row of the builtin type table, but every enum
    // compares and hashes by initdb's enum_eq, enum_cmp and hashenum over pg_enum's sort order (`quiet::ENUM_KEY_PROCS`)
    let enum_ = t.image.is_enum();
    let [eq, cmp, h] = if enum_ { types_core::quiet::ENUM_KEY_PROCS.map(|r| r.0) } else { [t.eq, t.cmp, t.hash] };
    let known = (crate::catalog::typefacts::row_of(t.base).is_some() || enum_) && !pseudo;
    let procs = [eq, cmp, if hash { h } else { 0 }];
    if !known || eq == 0 || procs.iter().any(|&p| p >= crate::catalog::typefacts::FIRST_NORMAL_OID) {
        return Effects::UNKNOWN;
    }
    procs.into_iter().filter(|&p| p != 0).fold(Effects::NONE, |e, p| e.and(of_call(f, p)))
}

/// Whether a sort or DISTINCT clause's ordering operator runs user code beyond its column type's own comparator: an
/// operator of another family (not `facts::order::default_order`) that is a user's or calls a user's function. A builtin
/// operator of a builtin non-default family (text_pattern_ops' `~<~`) compares by builtin code. No sortop (a hashed
/// DISTINCT) and an operator the source never saw are user code.
pub fn user_order(f: &dyn crate::catalog::Facts, sortop: u32) -> bool {
    use crate::catalog::typefacts::FIRST_NORMAL_OID;
    !crate::facts::order::default_order(f, sortop) && (sortop == 0 || sortop >= FIRST_NORMAL_OID || f.oper(sortop).is_none_or(|o| o.code >= FIRST_NORMAL_OID))
}

/// D2719 / D2640 (group lane): whether a group key of type `typid` under equality operator `op` compares by user code or
/// by a built-in other than the type's own `=` (`Facts::key_eq` User or Other) where the operator is a user's:
/// PostgreSQL groups by the operator's procedure there, which the key's image does not stand for (a streamed sorted
/// group calls it, decompose ops/agg `KeyDecl::user_eq`). No operator, or a built-in one, is not.
pub fn user_group_eq(f: &dyn crate::catalog::Facts, op: u32, typid: u32) -> bool {
    op >= crate::catalog::typefacts::FIRST_NORMAL_OID && crate::catalog::keysem::eq_not_image(f, op, typid)
}

/// D2036: hash clause `c` (a hash join's, or a catalog nest's in its form) is under no builtin hashable equality
/// (join.rs `builtin_hash_eq`), or (D2725) under a builtin one across two types whose key images do not pair
/// (`images_pair`: float4 = float8, date = timestamp), so its keys are the hash value of each side (join.rs `user_keys`)
/// and the probe runs it on each candidate whose images are equal (lowerv4 keys.rs `confirms`), as PG's bucket scan runs it
/// on each hash-equal tuple.
pub fn user_hash_clause(facts: &dyn crate::catalog::Facts, c: types_nodes::Node<'_>) -> bool {
    c.as_op_expr().is_some_and(|o| crate::join::hash_valued(facts, o))
}

/// D2013 slice 2, Road A (main 2026-10-04T20:45:39Z; the user's VOLATILE ruling of 2026-10-04: counts free, at least one
/// call per used evaluation, never cached across rows): whether code `e` PostgreSQL evaluates inside a nest inner's index
/// key or run-time pruning step may move onto the nest's outer row (decompose facts/hoist.rs), where it runs once per
/// outer row and only that row's keys and pruning read it. Code that acts passes: only its call count moves, and each row
/// whose value is used makes its own call. Code that may raise passes only where PostgreSQL evaluates it once per outer row
/// too (`exact`: a plain inner index walk's runtime keys, at every rescan); an arm's key or a pruning step PostgreSQL may
/// never evaluate for a row (a pruned arm, an arm past a LIMIT), where the outer row's error would be a new one.
pub fn hoist_ok(e: Effects, exact: bool) -> bool {
    exact || !e.may_raise
}

/// D4569: whether a SELECT's union arms may each carry a copy of a nest (decompose arm_nests.rs `per_arm`) whose inner's
/// rows are the same at every rescan (F1 `invariant`), whose inner F5 reads raise-free with at most a STABLE call
/// (`raise_free`, `volatility`), and whose join qual's conjuncts have the effects `qual`: the inner is calm, so its one
/// fill run ahead of every arm shows nothing (lowerv4 nest.spool's calm eager form), and the qual does not act, so no
/// order of the arms' rows shows. A qual that may raise is copied too (a data error does not constrain the evaluation
/// order): each copy runs it over its own arm's pairs, the pairs PostgreSQL's one nest runs it over, so an error the
/// copies raise is one PostgreSQL raises, perhaps from another pair (`occurred_on BETWEEN released_on AND released_on +
/// 6`: date_pli may raise). The raising inner stays out: its fill runs ahead of every arm, and its licences are lowerv4's
/// held-error forms.
pub fn union_nest_quiet(invariant: bool, raise_free: bool, volatility: Volatility, qual: Effects) -> bool {
    invariant && raise_free && volatility != Volatility::Volatile && !qual.acts
}

/// What running some code may do. `UNKNOWN` is the worst case on every field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Effects {
    /// Running it once more, once fewer or at another moment can be seen: a VOLATILE call in any node that calls one, a
    /// function the catalog cannot answer, a write, a row lock. STABLE and IMMUTABLE calls do not act (`DECISIONS.md` 3).
    pub acts: bool,
    /// It may raise on some input.
    pub may_raise: bool,
    /// Two runs with the same parameters return the same rows.
    pub same_rows: bool,
}

impl Effects {
    pub const NONE: Effects = Effects { acts: false, may_raise: false, same_rows: true };
    pub const RAISES: Effects = Effects { acts: false, may_raise: true, same_rows: true };
    pub const ACTS: Effects = Effects { acts: true, may_raise: false, same_rows: false };
    pub const UNKNOWN: Effects = Effects { acts: true, may_raise: true, same_rows: false };

    pub const fn and(self, o: Effects) -> Effects {
        Effects { acts: self.acts || o.acts, may_raise: self.may_raise || o.may_raise, same_rows: self.same_rows && o.same_rows }
    }
}

/// The one answer for what running a SubPlan step may do (ops/subplan `SubPlanDesc::answer`): its `Effects`, whether it
/// runs user code (`Found::user_call`) and whether it makes a call whose place among the statement's calls shows
/// (`Found::order_call`). `UNKNOWN`, every field at its worst, is the answer for a SubPlan no step names (fail closed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubEffects {
    pub effects: Effects,
    pub user_code: bool,
    pub order_call: bool,
}

impl SubEffects {
    pub const NONE: SubEffects = SubEffects { effects: Effects::NONE, user_code: false, order_call: false };
    pub const UNKNOWN: SubEffects = SubEffects { effects: Effects::UNKNOWN, user_code: true, order_call: true };

    pub const fn and(self, o: SubEffects) -> SubEffects {
        SubEffects { effects: self.effects.and(o.effects), user_code: self.user_code || o.user_code, order_call: self.order_call || o.order_call }
    }
}

/// `pg_proc.provolatile`, ordered: a set of calls is as volatile as its most volatile member.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Volatility {
    Immutable,
    Stable,
    Volatile,
}

impl Volatility {
    pub const fn letter(self) -> char {
        match self {
            Volatility::Immutable => 'i',
            Volatility::Stable => 's',
            Volatility::Volatile => 'v',
        }
    }
}

/// `pg_proc.proparallel`, ordered: the planner's parallel hazard (`max_parallel_hazard`), a set of calls as hazardous as
/// its worst member. D1035 step 2 (V4R-444): pgrust's planner never assesses it (planner lib.rs `assess_parallel = false`,
/// Gravity decision 6), so decompose folds it itself (rescan.rs F5) for an InitPlan's eager evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Parallel {
    Safe,
    Restricted,
    Unsafe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FuncProps {
    pub volatility: Volatility,
    pub strict: bool,
    /// `proleakproof`: PostgreSQL's promise that the function raises nothing that depends on its arguments.
    pub leakproof: bool,
    /// `proparallel` as stored (D1035 step 2); `pool` is what a call to it is taken as.
    pub parallel: Parallel,
}

impl FuncProps {
    /// A pg_proc row's provolatile and proparallel letters with its proisstrict and proleakproof (a plan capture's
    /// `-- func` line, corpus.rs); none for a letter pg_proc never holds.
    pub fn of_letters(volatility: &str, strict: bool, leakproof: bool, parallel: &str) -> Option<FuncProps> {
        let volatility = match volatility {
            "i" => Volatility::Immutable,
            "s" => Volatility::Stable,
            "v" => Volatility::Volatile,
            _ => return None,
        };
        let parallel = match parallel {
            "s" => Parallel::Safe,
            "r" => Parallel::Restricted,
            "u" => Parallel::Unsafe,
            _ => return None,
        };
        Some(FuncProps { volatility, strict, leakproof, parallel })
    }

    /// A call to `funcid` on a pool: a builtin's `proparallel`; a user function's is taken as Unsafe whatever its mark
    /// (main 09-28: conservative, its InitPlan stays lazy).
    pub fn pool(&self, funcid: u32) -> Parallel {
        if funcid < crate::catalog::typefacts::FIRST_NORMAL_OID { self.parallel } else { Parallel::Unsafe }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperProps {
    /// `oprcode`: the function the operator calls.
    pub code: u32,
    pub canhash: bool,
    pub canmerge: bool,
    /// The first hash opfamily holding the operator as its strategy-1 `=` (`get_op_hash_functions`' pick); 0 = none.
    pub hash_family: u32,
    /// A polymorphic or record input (array / record / range equality): whether it hashes depends on the actual type.
    pub poly: bool,
    /// `oprcom`: the operator with the inputs swapped (`a < b` is `b > a`); 0 = none.
    pub commutator: u32,
    /// `oprnegate`: the operator answering NOT of this one on non-NULL inputs (`a <> b` is NOT `a = b`); 0 = none.
    pub negator: u32,
}

/// A builtin function's properties (`procfacts::PROCS`); none for an oid outside the builtin catalog.
pub fn builtin_func(oid: u32) -> Option<FuncProps> {
    let oid = u16::try_from(oid).ok()?;
    let i = crate::effects::procfacts::PROCS.binary_search_by_key(&oid, |r| r.0).ok()?;
    let f = crate::effects::procfacts::PROCS[i].1;
    let volatility = match f & 3 {
        0 => Volatility::Immutable,
        1 => Volatility::Stable,
        _ => Volatility::Volatile,
    };
    let parallel = match f >> 4 & 3 {
        0 => Parallel::Safe,
        1 => Parallel::Restricted,
        _ => Parallel::Unsafe,
    };
    Some(FuncProps { volatility, strict: f & 4 != 0, leakproof: f & 8 != 0, parallel })
}

/// D3577: builtin `oid` is a LANGUAGE sql function (`procfacts::PROCS` bit 6): initdb's `select $2 + $1` commutators
/// (`interval_pl_timestamp`, `integer_pl_date`, ...) and the like, which PostgreSQL runs through the SQL function manager
/// (or inlines); false for an oid outside the builtin catalog.
pub fn builtin_sql_body(oid: u32) -> bool {
    let Ok(oid) = u16::try_from(oid) else { return false };
    let procs = crate::effects::procfacts::PROCS;
    procs.binary_search_by_key(&oid, |r| r.0).is_ok_and(|i| procs[i].1 & 64 != 0)
}

/// A pattern match's form (DC-14, the constant-pattern rule `read::quiet_pattern` applies).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pattern {
    /// LIKE / NOT LIKE over text, name or bpchar (like.c MatchText; bpchar's and the like() aliases call textlike).
    Like,
    /// ILIKE / NOT ILIKE (like.c Generic_Text_IC_like).
    ILike,
    /// `~` / `!~` (`Regex(false)`) and `~*` / `!~*` (`Regex(true)`): regexp.c RE_compile_and_execute with REG_ADVANCED
    /// (and REG_ICASE). SIMILAR TO is `~` over similar_to_escape's result, a constant once the planner folds it.
    Regex(bool),
}

/// Builtin `funcid`'s pattern match form (`procfacts::PATTERNS`, by its C function's name); none for any other function,
/// a user function whatever its name included.
pub fn builtin_pattern(funcid: u32) -> Option<Pattern> {
    let oid = u16::try_from(funcid).ok()?;
    let i = crate::effects::procfacts::PATTERNS.binary_search_by_key(&oid, |r| r.0).ok()?;
    match crate::effects::procfacts::PATTERNS[i].1 {
        1 => Some(Pattern::Like),
        2 => Some(Pattern::ILike),
        3 => Some(Pattern::Regex(false)),
        _ => Some(Pattern::Regex(true)),
    }
}

/// The source class of a builtin SQL `extract(field FROM x)` (`procfacts::EXTRACTS`: pg_proc's `extract` rows, by their
/// argument type): what bounds each field's value (agg keys.rs `extract_int4`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtractSource {
    Date,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
    Interval,
}

/// Builtin `funcid`'s `extract` source (`procfacts::EXTRACTS`); none for any other function, a user function whatever its
/// name included.
pub fn builtin_extract(funcid: u32) -> Option<ExtractSource> {
    let oid = u16::try_from(funcid).ok()?;
    let i = crate::effects::procfacts::EXTRACTS.binary_search_by_key(&oid, |r| r.0).ok()?;
    Some(match crate::effects::procfacts::EXTRACTS[i].1 {
        1 => ExtractSource::Date,
        2 => ExtractSource::Time,
        3 => ExtractSource::TimeTz,
        4 => ExtractSource::Timestamp,
        5 => ExtractSource::TimestampTz,
        _ => ExtractSource::Interval,
    })
}

/// A builtin operator's properties (`procfacts::OPERS`); none for an oid outside the builtin catalog. One load of
/// `OPER_ROW`, no search: normalize asks it of compares the front walk keys (`commute`, `detext`'s negator).
pub fn builtin_oper(oid: u32) -> Option<OperProps> {
    let row = *OPER_ROW.get(usize::try_from(oid).ok()?)?;
    let (_, code, fam, f, com, neg) = *crate::effects::procfacts::OPERS.get(usize::from(row).checked_sub(1)?)?;
    Some(OperProps {
        code: code as u32,
        canhash: f & 1 != 0,
        canmerge: f & 2 != 0,
        hash_family: fam as u32,
        poly: f & 4 != 0,
        commutator: com as u32,
        negator: neg as u32,
    })
}

/// `procfacts::OPERS`' row of each operator oid, as 1 + its index (0: no builtin operator), built at compile time from the
/// sorted table (one row per oid): the row its binary search found, for every oid.
static OPER_ROW: [u16; OPER_OIDS] = oper_rows();
const OPER_OIDS: usize = crate::effects::procfacts::OPERS[crate::effects::procfacts::OPERS.len() - 1].0 as usize + 1;

const fn oper_rows() -> [u16; OPER_OIDS] {
    let t = crate::effects::procfacts::OPERS;
    let mut rows = [0u16; OPER_OIDS];
    let mut i = 0;
    while i < t.len() {
        rows[t[i].0 as usize] = (i + 1) as u16;
        i += 1;
    }
    rows
}

/// A builtin btree comparison operator's place `(opfamily, lefttype, righttype, strategy)` (`procfacts::BTREE`, the first
/// family holding it); none for an operator in no builtin btree family. One load of `BTREE_ROW`, no search: the front walk
/// asks it of each compare it keys, two to three times (normalize's `retype_class`, `implied_class_of`).
pub fn builtin_btree_member(opno: u32) -> Option<(u32, u32, u32, u8)> {
    let row = *BTREE_ROW.get(usize::try_from(opno).ok()?)?;
    let &(_, fam, l, r, s) = crate::effects::procfacts::BTREE.get(usize::from(row).checked_sub(1)?)?;
    Some((fam as u32, l as u32, r as u32, s))
}

/// `procfacts::BTREE`'s first row of each operator oid, as 1 + its index (0: the operator is in no builtin btree family),
/// built at compile time from the sorted table: the row its binary search found (`partition_point`), for every oid.
static BTREE_ROW: [u16; BTREE_OIDS] = btree_rows();
const BTREE_OIDS: usize = crate::effects::procfacts::BTREE[crate::effects::procfacts::BTREE.len() - 1].0 as usize + 1;

const fn btree_rows() -> [u16; BTREE_OIDS] {
    let t = crate::effects::procfacts::BTREE;
    let mut rows = [0u16; BTREE_OIDS];
    let mut i = t.len();
    // from the end, so an operator in two families keeps its first row
    while i > 0 {
        i -= 1;
        rows[t[i].0 as usize] = (i + 1) as u16;
    }
    rows
}

/// The builtin btree family's operator for `(lefttype, righttype, strategy)` (`get_opfamily_member`); none when it has none.
pub fn builtin_btree_op(family: u32, left: u32, right: u32, strategy: u8) -> Option<u32> {
    let row = (u16::try_from(family).ok()?, u16::try_from(left).ok()?, u16::try_from(right).ok()?, strategy);
    crate::effects::procfacts::BTREE.iter().find(|r| (r.1, r.2, r.3, r.4) == row).map(|r| r.0 as u32)
}

/// A builtin type's `(typinput, typoutput)` (`procfacts::TYPES`, array types through array_in / array_out); none for a type
/// outside the builtin catalog (a user type or domain: its I/O calls are unknown).
pub fn builtin_type_io(typid: u32) -> Option<(u32, u32)> {
    let typid = u16::try_from(typid).ok()?;
    let i = crate::effects::procfacts::TYPES.binary_search_by_key(&typid, |r| r.0).ok()?;
    let (_, input, output) = crate::effects::procfacts::TYPES[i];
    Some((input as u32, output as u32))
}

/// `get_promoted_array_type` of a builtin type (`procfacts::PROMOTED`): its array type, else itself when it is an array;
/// none outside the builtin catalog or for a type with no array (PostgreSQL raises "could not find array type").
pub fn builtin_promoted_array(typid: u32) -> Option<u32> {
    let typid = u16::try_from(typid).ok()?;
    let i = crate::effects::procfacts::PROMOTED.binary_search_by_key(&typid, |r| r.0).ok()?;
    Some(crate::effects::procfacts::PROMOTED[i].1 as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{AggSupport, AttFact, ExprFp, Facts, IndexFact, RelFact};

    /// `builtin_btree_member`'s table is the binary search's answer for every oid: the first row of an operator in two
    /// families, none past the table's last operator or between its operators.
    #[test]
    fn btree_member_by_table_is_the_searched_row() {
        let t = procfacts::BTREE;
        let searched = |opno: u32| {
            let op = u16::try_from(opno).ok()?;
            let &(o, fam, l, r, s) = t.get(t.partition_point(|r| r.0 < op))?;
            (o == op).then_some((fam as u32, l as u32, r as u32, s))
        };
        let last = u32::from(t[t.len() - 1].0);
        for opno in (0..=last + 70).chain([70_000, u32::MAX]) {
            assert_eq!(builtin_btree_member(opno), searched(opno), "{opno}");
        }
        assert!(t.windows(2).any(|w| w[0].0 == w[1].0), "the table holds an operator in two families");
    }

    /// `builtin_oper`'s table is the binary search's answer for every oid (the table holds each operator once).
    #[test]
    fn oper_by_table_is_the_searched_row() {
        let t = procfacts::OPERS;
        assert!(t.windows(2).all(|w| w[0].0 < w[1].0), "sorted, one row per operator");
        let searched = |oid: u32| {
            let oid = u16::try_from(oid).ok()?;
            let (_, code, fam, f, com, neg) = t[t.binary_search_by_key(&oid, |r| r.0).ok()?];
            Some((code, fam, f & 7, com, neg))
        };
        let last = u32::from(t[t.len() - 1].0);
        for oid in (0..=last + 70).chain([70_000, u32::MAX]) {
            let got = builtin_oper(oid).map(|o| (o.code as u16, o.hash_family as u16, (o.canhash as u8) | ((o.canmerge as u8) << 1) | ((o.poly as u8) << 2), o.commutator as u16, o.negator as u16));
            assert_eq!(got, searched(oid), "{oid}");
        }
    }

    /// D2044: user code is a function past FIRST_NORMAL_OID (a PL/pgSQL, SQL or extension function); a builtin is not.
    #[test]
    fn user_code_is_a_function_past_first_normal_oid() {
        for builtin in [1317, 16383] {
            assert!(!user_code(builtin), "{builtin}");
        }
        for user in [16384, 70000] {
            assert!(user_code(user), "{user}");
        }
    }

    /// D3979: lower / upper / initcap (text) raise on no input under the default, C or POSIX collation (`quiet_collated`
    /// over `types_core::quiet::case_map_quiet`); an unset or another collation keeps them raising, as their own mark does.
    #[test]
    fn case_maps_are_quiet_by_their_collation() {
        let no = &crate::catalog::NoFacts;
        for f in [870, 871, 872] {
            for c in [100, 950, 951] {
                assert!(read::quiet_collated(no, f, c), "{f} under {c}");
            }
            assert!(!read::quiet_collated(no, f, 0) && !read::quiet_collated(no, f, 12345), "{f}: unset, another collation");
            assert!(!read::quiet_call(no, f), "{f}: not quiet apart from its collation");
        }
        assert!(!read::quiet_collated(no, 3848, 100), "lower(anyrange): not the rule's");
        assert!(!read::quiet_collated(no, 20100, 100), "a user function the facts run no builtin for");
    }

    /// Arguments of different implicit collations leave a call's input collation indeterminate (0): a collation-aware
    /// builtin raises at its first non-null pair, so no collation is set. A COLLATE clause, or no collatable argument, sets it;
    /// a builtin that reads no collation (`types_core::quiet::COLLATION_FREE`) needs none.
    #[test]
    fn collation_set_needs_a_set_collation_over_collatable_arguments() {
        use types_nodes::primnodes::Var;
        use types_nodes::{Node, NodeList};
        let ctx = mcx::MemoryContext::new("collation-set");
        let m = ctx.mcx();
        let v = |coll: u32, t: u32| Node::mk(m, Var { vartype: t, varcollid: coll, ..Default::default() }).unwrap();
        let (c, posix) = (950, 951);
        let text2 = NodeList::make2(m, v(c, 25), v(posix, 25)).unwrap();
        let (texteq, text_lt) = (67, 740);
        assert!(!read::collation_set(texteq, 0, &text2), "C vs POSIX: indeterminate");
        assert!(!read::collation_set(text_lt, 0, &text2), "text_lt: varstr_cmp's check_collation_set");
        assert!(!read::collation_set(0, 0, &text2), "a comparison by the type's family (MinMaxExpr)");
        assert!(read::collation_set(texteq, c, &text2), "a COLLATE clause sets it");
        let int2 = NodeList::make2(m, v(0, 23), v(0, 23)).unwrap();
        assert!(read::collation_set(texteq, 0, &int2), "no collatable argument");
        for (f, name) in [(2160, "~<~"), (2161, "~<=~"), (2163, "~>=~"), (2164, "~>~"), (2166, "bttext_pattern_cmp"), (1258, "||")] {
            assert!(read::collation_set(f, 0, &text2), "{name} over text reads no collation");
        }
        let bp2 = NodeList::make2(m, v(c, 1042), v(posix, 1042)).unwrap();
        assert!(!read::collation_set(1048, 0, &bp2), "bpchareq: bpchar's collation-aware equality");
        for (f, name) in [(2174, "~<~"), (2175, "~<=~"), (2177, "~>=~"), (2178, "~>~"), (2180, "btbpchar_pattern_cmp")] {
            assert!(read::collation_set(f, 0, &bp2), "{name} over bpchar reads no collation");
        }
    }

    /// User aggregates 20000 (finalfn 20010, VOLATILE), 20001 (finalfn 20011, IMMUTABLE), 20002 (no finalfn); 20003 has
    /// no pg_aggregate row.
    struct Aggs;

    impl Facts for Aggs {
        fn rel(&self, _: u32) -> Option<RelFact> {
            None
        }
        fn att(&self, _: u32, _: i16) -> Option<AttFact> {
            None
        }
        fn index(&self, _: u32) -> Option<IndexFact> {
            None
        }
        fn indexes_of(&self, _: u32, _: &mut dyn FnMut(IndexFact)) {}
        fn index_expr(&self, _: u32, _: u8) -> Option<ExprFp> {
            None
        }
        fn func(&self, funcid: u32) -> Option<FuncProps> {
            let volatility = match funcid {
                20010 => Volatility::Volatile,
                20011 => Volatility::Immutable,
                _ => return builtin_func(funcid),
            };
            Some(FuncProps { volatility, strict: false, leakproof: false, parallel: Parallel::Safe })
        }
        fn agg_support(&self, aggfnoid: u32) -> Option<AggSupport> {
            let final_ = match aggfnoid {
                20000 => 20010,
                20001 => 20011,
                20002 => 0,
                _ => return None,
            };
            Some(AggSupport { trans: 20020, final_, combine: 0, serial: 0, deserial: 0, mtrans: 0, minv: 0, mfinal: 0 })
        }
    }

    /// A source that compiles every regular expression (`true`) or none (`false`, a fixture's answer).
    struct Compiles(bool);

    impl Facts for Compiles {
        fn rel(&self, _: u32) -> Option<RelFact> {
            None
        }
        fn att(&self, _: u32, _: i16) -> Option<AttFact> {
            None
        }
        fn index(&self, _: u32) -> Option<IndexFact> {
            None
        }
        fn indexes_of(&self, _: u32, _: &mut dyn FnMut(IndexFact)) {}
        fn index_expr(&self, _: u32, _: u8) -> Option<ExprFp> {
            None
        }
        fn regex_compiles(&self, _: &[u8], _: bool, _: u32) -> bool {
            self.0
        }
    }

    /// DC-14: a constant-pattern match is quiet once its pattern is known good at lowering; the forms by the builtin's C
    /// function. Does not apply: a LIKE pattern ending in its escape, a regex the source cannot compile (a fixture), a
    /// nondeterministic or unknown collation, a NULL or non-constant pattern, a function that is no pattern match (bytea's
    /// LIKE, texteq), and the two temp-schema functions are quiet with no argument rule.
    #[test]
    fn a_constant_pattern_match_is_quiet_once_its_pattern_is_good() {
        use types_nodes::primnodes::{Param, ParamKind};
        use types_nodes::{Node, NodeList};
        assert_eq!([858, 1636, 1252, 1241, 1658].map(builtin_pattern), [Some(Pattern::Like), Some(Pattern::ILike), Some(Pattern::Regex(false)), Some(Pattern::Regex(true)), Some(Pattern::Regex(false))]);
        assert_eq!([2005, 67, 20_000].map(builtin_pattern), [None, None, None]);
        let ctx = mcx::MemoryContext::new("dc14-pattern");
        let m = ctx.mcx();
        let subject = || Node::mk(m, Param { paramkind: ParamKind::PARAM_EXTERN, paramid: 1, paramtype: 19, paramtypmod: -1, paramcollid: 950, location: -1 }).unwrap();
        let args = |pat: &[u8]| NodeList::make2(m, subject(), crate::normalize::text_const(m, pat, 950).unwrap()).unwrap();
        let (c, yes, no) = (950, &Compiles(true), &Compiles(false));
        assert!(quiet_pattern(no, 858, &args(b"pg\\_%"), c), "name LIKE: the escape pairs up");
        assert!(!quiet_pattern(no, 858, &args(b"abc\\"), c), "does not apply: a LIKE pattern ending in its escape");
        assert!(quiet_pattern(no, 1635, &args(b"PG%"), c), "name ILIKE under C");
        assert!(quiet_pattern(yes, 79, &args(b"^(pg_catalog)$"), c), "name ~: the server compiles it");
        assert!(!quiet_pattern(no, 79, &args(b"^(pg_catalog)$"), c), "does not apply: a source that cannot compile it");
        assert!(!quiet_pattern(yes, 1635, &args(b"PG%"), 20_000), "does not apply: an unknown (user) collation");
        assert!(!quiet_pattern(yes, 858, &args(b"pg%"), 0), "does not apply: no collation (PostgreSQL cannot determine one)");
        assert_eq!(pattern_operand(858, &args(b"pg%")).map(|(f, p)| (f, p.to_vec())), Some((Pattern::Like, b"pg%".to_vec())), "the class's operand");
        assert!(!quiet_pattern(yes, 2005, &args(b"x"), c) && !quiet_pattern(yes, 67, &args(b"x"), c), "not a pattern match");
        let var = NodeList::make2(m, subject(), subject()).unwrap();
        assert!(!quiet_pattern(yes, 79, &var, c), "does not apply: a pattern that is no constant");
        assert!(read::quiet_call(no, 2854) && read::quiet_call(no, 2855), "pg_my_temp_schema / pg_is_other_temp_schema");
    }

    /// A one-dimensional text array's image as construct_md_array lays it out (`each_list_text`'s reading): the header,
    /// a NULL bitmap where an element is NULL (`dataoffset` then MAXALIGNed past it), each element a 4-byte-header varlena
    /// at int alignment; `ndim` 0 for no element.
    fn text_array_image(elems: &[Option<&[u8]>]) -> Vec<u8> {
        let n = elems.len();
        let nulls = elems.iter().any(Option::is_none);
        let data = if nulls { (24 + n.div_ceil(8) + 7) & !7 } else { 24 };
        let mut v = vec![0u8; if n == 0 { 16 } else { data }]; // alloc: Cold
        let w = |v: &mut Vec<u8>, at: usize, x: u32| v[at..at + 4].copy_from_slice(&x.to_ne_bytes());
        w(&mut v, 4, u32::from(n != 0));
        w(&mut v, 8, if nulls { data as u32 } else { 0 });
        w(&mut v, 12, 25);
        if n != 0 {
            w(&mut v, 16, n as u32);
            w(&mut v, 20, 1);
        }
        for (i, e) in elems.iter().enumerate() {
            if nulls && e.is_some() {
                v[24 + i / 8] |= 1 << (i % 8);
            }
        }
        for e in elems.iter().flatten() {
            v.resize(v.len().next_multiple_of(4), 0);
            v.extend_from_slice(&(((4 + e.len()) as u32) << 2).to_le_bytes());
            v.extend_from_slice(e);
        }
        v.resize(v.len().next_multiple_of(4), 0);
        let total = v.len() as u32;
        w(&mut v, 0, total << 2);
        v
    }

    /// D3788 (DC-14 over a list): a pattern match over a constant text array is quiet once every non-NULL element is a
    /// good pattern (pgwatch's `nspname NOT LIKE ALL (ARRAY['pg\_%', 'information_schema', '\_timescaledb%'])`).
    /// Does not apply: an element ending in its escape, a regex the source cannot compile, a nondeterministic or no
    /// collation, a non-constant list, another element type or a function that is no pattern match. A NULL element is
    /// never matched; an empty list matches nothing.
    #[test]
    fn a_pattern_match_over_a_constant_list_is_quiet_once_every_pattern_is_good() {
        use types_nodes::primnodes::{Param, ParamKind};
        use types_nodes::{Node, NodeList};
        let ctx = mcx::MemoryContext::new("d3788-pattern-list");
        let m = ctx.mcx();
        let subject = || Node::mk(m, Param { paramkind: ParamKind::PARAM_EXTERN, paramid: 1, paramtype: 19, paramtypmod: -1, paramcollid: 950, location: -1 }).unwrap();
        let list = |elems: &[Option<&[u8]>], elemtype: u32| {
            let mut image = text_array_image(elems);
            image[12..16].copy_from_slice(&elemtype.to_ne_bytes());
            let words: Vec<u32> = image.chunks(4).map(|w| u32::from_ne_bytes([w[0], w[1], w[2], w[3]])).collect();
            let words = mcx::slice_borrow_in(m, &words).unwrap();
            let k = Node::mk_const(m, 1009, -1, 950, -1, datum::Datum::from_usize(words.as_ptr() as usize), false, false).unwrap();
            NodeList::make2(m, subject(), k).unwrap()
        };
        let (c, yes, no) = (950, &Compiles(true), &Compiles(false));
        fn e(b: &'static [u8]) -> Option<&'static [u8]> {
            Some(b)
        }
        let pgwatch = [e(b"pg\\_%"), e(b"information_schema"), e(b"\\_timescaledb%")];
        assert!(quiet_pattern_list(no, 859, &list(&pgwatch, 25), c), "name NOT LIKE ALL: every escape pairs up");
        assert!(quiet_pattern_list(no, 858, &list(&[e(b"a%"), None, e(b"b\\\\")], 25), c), "a NULL element is skipped; a doubled escape pairs");
        assert!(quiet_pattern_list(no, 858, &list(&[], 25), c), "an empty list matches nothing");
        assert!(!quiet_pattern_list(no, 858, &list(&[e(b"a%"), e(b"abc\\")], 25), c), "does not apply: an element ending in its escape");
        assert!(quiet_pattern_list(yes, 79, &list(&[e(b"^pg_"), e(b"x$")], 25), c), "name ~ ANY: the server compiles each");
        assert!(!quiet_pattern_list(no, 79, &list(&[e(b"^pg_")], 25), c), "does not apply: a source that cannot compile it");
        assert!(!quiet_pattern_list(yes, 858, &list(&pgwatch, 25), 0), "does not apply: no collation");
        assert!(!quiet_pattern_list(yes, 858, &list(&pgwatch, 23), c), "does not apply: another element type");
        assert!(!quiet_pattern_list(yes, 67, &list(&pgwatch, 25), c), "does not apply: texteq is no pattern match");
        let var = NodeList::make2(m, subject(), subject()).unwrap();
        assert!(!quiet_pattern_list(yes, 858, &var, c), "does not apply: a list that is no constant");
        let image = text_array_image(&[e(b"ab"), None, e(b"cde")]);
        let mut seen = Vec::new(); // alloc: Cold
        assert_eq!(each_list_text(&image[4..], &mut |p| { seen.push(p.to_vec()); true }), Some(true));
        assert_eq!(seen, [b"ab".to_vec(), b"cde".to_vec()], "the non-NULL elements in order");
        assert_eq!(each_list_text(&image[4..image.len() - 4], &mut |_| true), None, "a length past the image");
        let mut two = text_array_image(&[e(b"a")]);
        two[4..8].copy_from_slice(&2u32.to_ne_bytes());
        assert_eq!(each_list_text(&two[4..], &mut |_| true), None, "two dimensions");
    }

    #[test]
    fn of_final_reads_the_final_function() {
        // sum(int4) (2108): a builtin's own IMMUTABLE row.
        assert!(!of_final(&Aggs, 2108).acts);
        assert!(of_final(&Aggs, 20000).acts, "a VOLATILE finalfn acts");
        assert_eq!(of_final(&Aggs, 20001), Effects::RAISES, "an IMMUTABLE user finalfn may raise, never acts");
        assert_eq!(of_final(&Aggs, 20002), Effects::NONE, "no finalfn: nothing runs");
        assert_eq!(of_final(&Aggs, 20003), Effects::UNKNOWN, "no pg_aggregate row: unknown");
    }

    #[test]
    fn of_agg_reads_every_support_function() {
        assert!(!of_agg(&Aggs, 2108).acts);
        assert!(of_agg(&Aggs, 20000).acts, "a VOLATILE finalfn acts");
        assert!(of_agg(&Aggs, 20002).acts, "an unknown transfn acts though no finalfn runs");
        assert_eq!(of_agg(&Aggs, 20003), Effects::UNKNOWN, "no pg_aggregate row: unknown");
    }

    /// Indexes 30000 (one key column, initdb opclass), 30001 (two key columns, the second a user's opclass), any other
    /// unknown; operator 40000 is a user's (oprcode 20010, VOLATILE).
    struct Idx;

    impl Facts for Idx {
        fn rel(&self, _: u32) -> Option<RelFact> {
            None
        }
        fn att(&self, _: u32, _: i16) -> Option<AttFact> {
            None
        }
        fn index(&self, _: u32) -> Option<IndexFact> {
            None
        }
        fn indexes_of(&self, _: u32, _: &mut dyn FnMut(IndexFact)) {}
        fn index_expr(&self, _: u32, _: u8) -> Option<ExprFp> {
            None
        }
        fn index_opc(&self, indexid: u32) -> crate::catalog::IndexOpc {
            match indexid {
                30000 => crate::catalog::IndexOpc { builtin: 0b1, nkeys: 1, known: true },
                30001 => crate::catalog::IndexOpc { builtin: 0b01, nkeys: 2, known: true },
                _ => crate::catalog::IndexOpc::UNKNOWN,
            }
        }
        fn func(&self, funcid: u32) -> Option<FuncProps> {
            Aggs.func(funcid)
        }
        fn oper(&self, opno: u32) -> Option<OperProps> {
            match opno {
                40000 => Some(OperProps { code: 20010, canhash: false, canmerge: false, hash_family: 0, poly: false, commutator: 0, negator: 0 }),
                _ => builtin_oper(opno),
            }
        }
    }

    #[test]
    fn of_index_reads_opclasses_and_key_operators() {
        // int4eq (96) over a user table's index on an initdb opclass: the user index itself does not act.
        assert!(!of_index(&Idx, 30000, [96]).acts);
        assert!(!of_index(&Idx, 30000, []).acts, "no key operator: the opclass alone");
        assert_eq!(of_index(&Idx, 30001, [96]), Effects::UNKNOWN, "a user opclass on a key column: unknown");
        assert_eq!(of_index(&Idx, 30002, [96]), Effects::UNKNOWN, "no opclass facts: unknown");
        assert_eq!(of_index(&Idx, 30000, [40000]), Effects::UNKNOWN, "a user operator: unknown");
    }
    /// Types: jsonb (3802) a base type, record (2249) a pseudo-type, 50000 a user base type, 50001 a domain over jsonb, 50002
    /// an enum, 50003 a domain over it; composites 50004 (int4, the enum), 50005 (int4, the user type), 50006 a domain over
    /// 50004, 50007 (50004, text), 50008 one whose descriptor cannot be read, 50009 a (malformed) one holding itself.
    struct Tys;

    impl Facts for Tys {
        fn rel(&self, _: u32) -> Option<RelFact> {
            None
        }
        fn att(&self, _: u32, _: i16) -> Option<AttFact> {
            None
        }
        fn index(&self, _: u32) -> Option<IndexFact> {
            None
        }
        fn indexes_of(&self, _: u32, _: &mut dyn FnMut(IndexFact)) {}
        fn index_expr(&self, _: u32, _: u8) -> Option<ExprFp> {
            None
        }
        fn type_row(&self, typid: u32) -> Option<crate::catalog::TypeRow> {
            let (typlen, typtype, base) = match typid {
                3802 => (-1, b'b', 0),
                2249 => (-1, b'p', 0),
                50000 => (-1, b'b', 0),
                50001 => (-1, b'd', 3802),
                50002 => (4, b'e', 0),
                50003 => (4, b'd', 50002),
                50004 | 50005 | 50007 | 50008 | 50009 => (-1, b'c', 0),
                50006 => (-1, b'd', 50004),
                _ => return None,
            };
            Some(crate::catalog::TypeRow { typlen, byval: false, typtype, base })
        }
        fn row_cols(&self, typid: u32, f: &mut dyn FnMut(AttFact)) -> bool {
            let cols: &[u32] = match typid {
                50004 => &[23, 50002],
                50005 => &[23, 50000],
                50007 => &[50004, 25],
                50009 => &[50009],
                _ => return false,
            };
            cols.iter().for_each(|&t| f(AttFact::plain(t, -1, -1, false, false)));
            true
        }
    }

    #[test]
    fn of_type_reads_the_types_procs() {
        assert!(!of_type(&Tys, 3802).acts, "jsonb's eq / cmp / hash are initdb IMMUTABLE functions");
        assert!(!of_type(&Tys, 50001).acts, "a domain compares as its base");
        assert_eq!(of_type(&Tys, 2249), Effects::UNKNOWN, "record's procs call each column type's");
        assert_eq!(of_type(&Tys, 50000), Effects::UNKNOWN, "a user type's procs are a user's");
        assert_eq!(of_type(&Tys, 60000), Effects::UNKNOWN, "an unknown type");
        assert!(!of_type(&Tys, 23).acts, "int4 with no catalog row: the bootstrap catalog's IMMUTABLE procs, no pseudo-type");
        assert_eq!(of_type(&Tys, 2277), Effects::UNKNOWN, "anyarray with no catalog row stays a pseudo-type");
        assert_eq!(of_compare(&Tys, 23), Effects::NONE, "int4eq and btint4cmp are leakproof builtins; hashint4 is not, and no compare calls it");
        assert!(of_type(&Tys, 23).may_raise, "pg_proc marks hashint4 not leakproof");
        assert_eq!(of_compare(&Tys, 2249), Effects::UNKNOWN, "record's compare calls each column type's");
        assert!(!of_type(&Tys, 50002).acts, "AGC-20: an enum compares and hashes by initdb's enum_eq / enum_cmp / hashenum");
        assert_eq!(of_compare(&Tys, 50003), of_compare(&Tys, 50002), "a domain over an enum compares as the enum");
        assert_ne!(of_compare(&Tys, 50002), Effects::UNKNOWN, "an enum's compare is known");
        assert!(!of_type(&Tys, 50004).acts, "AGC-20: a named composite of builtin columns: record_eq / btrecordcmp / hash_record over theirs");
        assert_eq!(of_compare(&Tys, 50006), of_compare(&Tys, 50004), "a domain over a composite compares as the composite");
        assert!(!of_compare(&Tys, 50007).acts, "a composite of a composite of builtin columns");
        assert_eq!(of_type(&Tys, 50005), Effects::UNKNOWN, "a composite with a user-typed column calls that type's procs");
        assert_eq!(of_compare(&Tys, 50008), Effects::UNKNOWN, "a composite whose columns cannot be read");
        assert_eq!(of_compare(&Tys, 50009), Effects::UNKNOWN, "nesting past 8 levels");
    }

    /// KeySem (LANGUAGE internal; equivclass#53): user functions 20100 (`AS 'int8eq'`), 20101 (`AS 'int8lt'`), 20103 (`AS
    /// 'hashint8'`) and 20104 (`AS 'btint8cmp'`) are LANGUAGE internal, IMMUTABLE STRICT, not marked leakproof, and run
    /// builtins 467, 469, 449 and 351; 20102 is a SQL function. Operators by what a node naming them alone runs: 40100 is
    /// int8alias1's `<` (oprcode 20101; its btree family has no comparator for its type), 40101 a `=` over the SQL
    /// function, 40102 a hashable `=` (20100, hash procs 20103), 40103 a `<` whose family's comparator is 20104.
    struct Alias;

    impl Facts for Alias {
        fn rel(&self, _: u32) -> Option<RelFact> {
            None
        }
        fn att(&self, _: u32, _: i16) -> Option<AttFact> {
            None
        }
        fn index(&self, _: u32) -> Option<IndexFact> {
            None
        }
        fn indexes_of(&self, _: u32, _: &mut dyn FnMut(IndexFact)) {}
        fn index_expr(&self, _: u32, _: u8) -> Option<ExprFp> {
            None
        }
        fn func(&self, funcid: u32) -> Option<FuncProps> {
            match funcid {
                20100..=20104 => Some(FuncProps { volatility: Volatility::Immutable, strict: true, leakproof: false, parallel: Parallel::Safe }),
                _ => builtin_func(funcid),
            }
        }
        fn builtin_body(&self, funcid: u32) -> Option<u32> {
            match funcid {
                20100 => Some(467),
                20101 => Some(469),
                20103 => Some(449),
                20104 => Some(351),
                _ => None,
            }
        }
        fn op_calls(&self, opno: u32) -> Option<crate::catalog::OpCalls> {
            let (code, order, hash) = match opno {
                40100 => (20101, Some([0, 0]), None),
                40101 => (20102, None, None),
                40102 => (20100, None, Some([20103, 20103])),
                40103 => (20101, Some([0, 20104]), None),
                _ => return None,
            };
            Some(crate::catalog::OpCalls { code, order, hash })
        }
    }

    #[test]
    fn a_language_internal_function_raises_as_its_builtin() {
        assert_eq!((body(&Alias, 20100), body(&Alias, 467), body(&Alias, 20102)), (Some(467), Some(467), None));
        assert!(quiet_call(&Alias, 20100) && leakproof(&Alias, 20100), "int8eq is a leakproof builtin, whatever 20100's own mark");
        assert_eq!(of_call(&Alias, 20100), Effects::NONE, "IMMUTABLE by its own row, quiet as int8eq");
        assert_eq!(of_call(&Alias, 20103), Effects::RAISES, "hashint8 is no leakproof builtin");
        assert_eq!(of_call(&Alias, 20102), Effects::RAISES, "does not apply: a SQL function keeps its own volatility and may raise");
        assert!(!quiet_call(&crate::catalog::NoFacts, 20100), "does not apply: a source that reads no catalog knows no alias");
        let ctx = mcx::MemoryContext::new("keysem-langint");
        let m = ctx.mcx();
        let arena = gravity_program::Arena::new(1 << 12);
        let clause = |eqop, sortop| types_nodes::Node::mk(m, types_nodes::parsenodes::SortGroupClause { tleSortGroupRef: 1, eqop, sortop, reverse_sort: false, nulls_first: false, hashable: false }).unwrap();
        let walk = |n| Effects::of_found(crate::rerun::of_expr_alone(&Alias, arena.handle(), n));
        assert_eq!(walk(clause(0, 40100)), Effects::RAISES, "int8alias1's `<`: int8lt's facts, and the sort raises at its first call");
        assert_eq!(walk(clause(0, 40103)), Effects::NONE, "a `<` over int8lt with btint8cmp as its family's comparator");
        assert_eq!(walk(clause(40102, 0)), Effects::RAISES, "a hashable `=`: int8eq and hashint8, no act");
        assert_eq!(walk(clause(40101, 0)), Effects::UNKNOWN, "does not apply: an operator over a SQL function stays unknown");
        assert_eq!(Effects::of_found(crate::rerun::of_expr_alone(&crate::catalog::NoFacts, arena.handle(), clause(0, 40100))), Effects::UNKNOWN, "does not apply: no catalog");
    }

    /// `of_node` judges one node, its children apart, by the walk's per-node rules, and fails closed: a node the walk does
    /// not know acts and may raise, and so does a SubPlan read with no statement. Folding it over every node of an
    /// expression gives the whole walk's answer.
    #[test]
    fn of_node_judges_one_node_and_fails_closed() {
        use types_nodes::primnodes::{CoerceToDomain, CoercionForm, FuncExpr, NextValueExpr, Param, ParamKind, SubPlan};
        use types_nodes::{Node, NodeList};
        let ctx = mcx::MemoryContext::new("of-node");
        let m = ctx.mcx();
        let arena = gravity_program::Arena::new(1 << 12);
        let (f, cx) = (&crate::catalog::NoFacts, Cx::without_stmt(&crate::catalog::NoFacts, arena.handle()));
        let param = || Node::mk(m, Param { paramkind: ParamKind::PARAM_EXTERN, paramid: 1, paramtype: 23, paramtypmod: -1, paramcollid: 0, location: -1 }).unwrap();
        let call = |funcid: u32, args| Node::mk(m, FuncExpr { funcid, funcresulttype: 23, args, ..Default::default() }).unwrap();
        // random() (VOLATILE), int4eq (leakproof, strict), int4pl (strict, raises on overflow), an oid no catalog holds
        let random = call(1598, NodeList::nil());
        let eq = call(65, NodeList::make2(m, random, param()).unwrap());
        assert!(read::of_node(&cx, random).acts, "a VOLATILE call acts");
        assert_eq!(read::of_node(&cx, eq), Effects::NONE, "int4eq itself: its VOLATILE argument is a child");
        assert!(read::of_expr(&cx, eq, &PLAIN).acts, "the whole expression acts");
        let pl = call(177, NodeList::make2(m, param(), param()).unwrap());
        assert_eq!(read::of_node(&cx, pl), Effects::RAISES, "int4pl may raise, never acts");
        assert_eq!(read::of_node(&cx, call(70_000, NodeList::nil())), Effects::UNKNOWN, "an unknown function");
        assert_eq!(read::of_node(&cx, param()), Effects::NONE, "a parameter calls nothing");
        let domain = Node::mk(m, CoerceToDomain { arg: param(), resulttype: 70_001, resulttypmod: -1, resultcollid: 0, coercionformat: CoercionForm::COERCE_IMPLICIT_CAST, location: -1 }).unwrap();
        assert_eq!(read::of_node(&cx, domain), Effects::UNKNOWN, "a domain coercion runs its CHECKs: fail closed, it acts and may raise");
        let nextval = Node::mk(m, NextValueExpr { seqid: 70_002, typeId: 20 }).unwrap();
        assert_eq!(read::of_node(&cx, nextval), Effects::UNKNOWN, "a node the walk does not know: fail closed");
        let sub = Node::mk(m, SubPlan { plan_id: 1, ..Default::default() }).unwrap();
        assert_eq!(read::of_node(&cx, sub), Effects::UNKNOWN, "a SubPlan with no statement: its plan is out of reach");
        for n in [eq, pl, domain] {
            let mut each = Effects::NONE;
            crate::expr::walk(n, &mut |x| {
                each = each.and(read::of_node(&cx, x));
                true
            });
            assert_eq!(each, Effects::of_found(crate::rerun::of_expr_alone(f, arena.handle(), n)), "the fold is the walk");
        }
    }

    /// D3796: a domain's constraints as the facts hand them (`Facts::domain_checks`): 70_010 CHECK (VALUE >= 0) by int4ge,
    /// 70_011 NOT NULL only, 70_012 a CHECK calling random(), 70_013 one calling a function no catalog holds, 70_014 one
    /// coercing to 70_014 itself, 70_015 over 70_010 (its chain's CHECK); any other domain unread.
    struct Doms<'m> {
        ge: types_nodes::Node<'m>,
        random: types_nodes::Node<'m>,
        unknown: types_nodes::Node<'m>,
        itself: types_nodes::Node<'m>,
    }

    impl Facts for Doms<'_> {
        fn rel(&self, _: u32) -> Option<RelFact> {
            None
        }
        fn att(&self, _: u32, _: i16) -> Option<AttFact> {
            None
        }
        fn index(&self, _: u32) -> Option<IndexFact> {
            None
        }
        fn indexes_of(&self, _: u32, _: &mut dyn FnMut(IndexFact)) {}
        fn index_expr(&self, _: u32, _: u8) -> Option<ExprFp> {
            None
        }
        fn domain_checks(&self, typid: u32, f: &mut dyn for<'n> FnMut(Option<types_nodes::Node<'n>>)) -> bool {
            match typid {
                70_010 | 70_015 => f(Some(self.ge)),
                70_011 => f(None),
                70_012 => f(Some(self.random)),
                70_013 => f(Some(self.unknown)),
                70_014 => f(Some(self.itself)),
                _ => return false,
            }
            true
        }
    }

    /// D3796: the F4 walk reads a domain coercion by the constraints the facts hand it: any constraint may raise, a CHECK
    /// calls what its expression calls; a domain the facts cannot read, or one met again inside its own CHECK, fails
    /// closed.
    #[test]
    fn a_domain_coercion_reads_its_constraints() {
        use types_nodes::primnodes::{CoerceToDomain, CoerceToDomainValue, CoercionForm, FuncExpr, Param, ParamKind};
        use types_nodes::{Node, NodeList};
        let ctx = mcx::MemoryContext::new("domains");
        let m = ctx.mcx();
        let arena = gravity_program::Arena::new(1 << 12);
        let value = || Node::mk(m, CoerceToDomainValue { typeId: 23, typeMod: -1, collation: 0, location: -1 }).unwrap();
        let param = || Node::mk(m, Param { paramkind: ParamKind::PARAM_EXTERN, paramid: 1, paramtype: 23, paramtypmod: -1, paramcollid: 0, location: -1 }).unwrap();
        let call = |funcid: u32, args| Node::mk(m, FuncExpr { funcid, funcresulttype: 16, args, ..Default::default() }).unwrap();
        let domain = |resulttype: u32, arg| Node::mk(m, CoerceToDomain { arg, resulttype, resulttypmod: -1, resultcollid: 0, coercionformat: CoercionForm::COERCE_IMPLICIT_CAST, location: -1 }).unwrap();
        let ge = call(150, NodeList::make2(m, value(), param()).unwrap());
        let random = call(1598, NodeList::nil());
        let unknown = call(70_000, NodeList::make1(m, value()).unwrap());
        let itself = domain(70_014, value());
        let f = Doms { ge, random, unknown, itself };
        let walk = |typid: u32| Effects::of_found(crate::rerun::of_expr_alone(&f, arena.handle(), domain(typid, param())));
        assert_eq!(walk(70_010), Effects::RAISES, "a CHECK by a builtin IMMUTABLE comparison: may raise, never acts");
        assert_eq!(walk(70_011), Effects::RAISES, "a NOT NULL: may raise, never acts");
        assert_eq!(walk(70_015), Effects::RAISES, "the chain's CHECK");
        assert!(walk(70_012).acts, "a CHECK calling random() acts");
        assert_eq!(walk(70_013), Effects::UNKNOWN, "a CHECK calling a function no catalog holds");
        assert_eq!(walk(70_014), Effects::UNKNOWN, "a CHECK coercing to its own domain: fail closed");
        assert_eq!(walk(70_016), Effects::UNKNOWN, "a domain the facts cannot read: fail closed");
        let found = crate::rerun::of_expr_alone(&f, arena.handle(), domain(70_010, param()));
        assert!(!found.user_call, "int4ge is no user code");
    }

    /// u9 (m6_sem_call_counts 90): text_pattern_ops' `~<~` 2314 is a non-default family's builtin operator calling
    /// builtin bttext_pattern_lt, so no user code; a default family's `<` neither; no sortop and a user operator are.
    #[test]
    fn a_builtin_non_default_family_is_no_user_code() {
        let f = crate::catalog::NoFacts;
        assert!(!user_order(&f, 2314) && !user_order(&f, 97));
        assert!(user_order(&f, 0) && user_order(&f, 70_000));
    }
}

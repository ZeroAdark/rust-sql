//! The effects AGREEMENT property test: every decider against the effects home's own reader, over every builtin.
//! The class it closes is two deciders of the effects home answering one question differently about the same code
//! (`build_class::side` vs `hides_calls`, D3566 / D3587; drain.rs `index_acts` vs decompose `of_source`, D3831; decompose
//! `quiet_node` vs a shared list, D3847). Lint R1 keeps raw catalog reads inside the home; it cannot see two readers in the
//! home disagree.
//!
//! The subjects are every PostgreSQL 18.6 builtin of the reference catalog (`.dat` files): a function (pg_proc.dat) as a
//! FuncExpr over columns of its declared argument types, an aggregate as an Aggref (pg_aggregate.dat's kind and direct
//! arguments), a window function as a WindowFunc; an operator (pg_operator.dat) as an OpExpr, an `=` also as IS DISTINCT
//! FROM and NULLIF, a boolean one also as `op ANY (array)` where its right type has an array type; every base, range and
//! multirange type's and their arrays' I/O coercion to text and from text; every function and operator again with each
//! argument a constant of each class decompose `quiet_args` reads (and NULL), under that constant's collation; every
//! comparison of two collatable types again over two columns of different implicit collations (C and POSIX: the input
//! collation is indeterminate); every function and operator with a collatable argument again under the default
//! collation and under C (its columns', input and result collations as parse_collate sets them: the collation-gated
//! rules); the leaf and structural node kinds; and the plan steps the pipeline readers read apart
//! (a nested loop, a LIMIT, a spool write, a drain).
//!
//! The questions. The effects home's own reader answers each one (the reference): decompose `effects::of_expr` for an
//! expression (its `quiet` rule for the raise, its F5 walk for the acts, F5's `user_call` for the user code) and
//! `effects::of_ops` for a plan step.
//! - RAISE: the code may raise on some input;
//! - ACTS: it makes a VOLATILE call or one the catalog cannot answer;
//! - USER: it calls user code or code the facts cannot name;
//! - CALLS: ACTS or USER (what makes a hash join side a user function's);
//! - QUIET: neither RAISE nor CALLS.
//! - LOUD: RAISE or ACTS (nestloop.rs `loud`'s question).
//!
//! Each decider in `DECIDERS` answers one of them, over the expression or over a pipeline that filters on it (or runs
//! the step) and reads no source of its own. A decider whose answer is not the reference's on a subject is a
//! disagreement, named `decider QUESTION direction subject`: `permissive` when the decider reads the code as safer than
//! the home does (the WRONG direction: D3566 / D3587 / D2784 / D2820), `conservative` the other way (a refusal or a
//! slower form, never a wrong result). `KNOWN` lists today's, each with the ruling that makes it so (a D-id or a design
//! rule) or the bug it is. A new disagreement fails the test, its row printed ready to paste: name the ruling or book the
//! bug. A listed one that no longer disagrees fails it too: delete it. `CEILING` is the list's size, so a row is added
//! only with its ruling and a raised ceiling in the same change, where review sees it.
//! Two rules narrow what counts: a decider in `ONE_WAY`, documented as failing closed, counts only its permissive
//! disagreements (each stays a lint/EFFECTS-DEBT row until it is deleted onto the home); and a disagreement an `INHERITS`
//! line names is its `KNOWN` row's, read the same way by a decider built on that row's reading (it comes back as NEW when
//! the row is deleted).
//!
//! The body is one `#[cfg(test)]` item: the gravity rules and the determinism lint read a `mod x;` file as
//! production code and skip only `#[cfg(test)]` items.
#[cfg(test)]
mod sweep {
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::path::PathBuf;

    use datum::Datum;

    use gravity_decompose::corpus::FixtureFacts;
    use gravity_decompose::effects::{self, Cx, Volatility, PLAIN};
    use gravity_decompose::facts::nest::NestFacts;
    use gravity_decompose::join::JoinDesc;
    use gravity_decompose::{Bound, Op, PipelineGraph, PipelineNode, Source};
    use gravity_expr::catalog::{DomainFacts, FuncFacts, IoFacts, TextFacts, TypeFacts};
    use gravity_expr::Catalog;
    use gravity_fn::{Shim, Tier};
    use gravity_program::{Arena, ArenaBox, ArenaRef, End, Pipeline, Reason};
    use mcx::{Mcx, MemoryContext};
    use types_core::Oid;
    use types_error::{PgError, PgResult};
    use types_nodes::jointype::JoinType;
    use types_nodes::list::{NodeList, OidList};
    use types_nodes::plannodes::{Plan, PlannedStmt, Result as ResultPlan};
    use types_nodes::primnodes::{
        Aggref, ArrayExpr, BoolExpr, BoolExprType, CoalesceExpr, CoerceToDomain, CoerceViaIO, CoercionForm, DistinctExpr, FuncExpr, MinMaxExpr, MinMaxOp,
        NextValueExpr, NullIfExpr, NullTest, NullTestType, OpExpr, Param, ParamKind, RelabelType, RowExpr, SQLValueFunction, SQLValueFunctionOp,
        ScalarArrayOpExpr, TargetEntry, Var, WindowFunc,
    };
    use types_nodes::{CmdType, Node};

    use crate::graph::Gx;
    use crate::ops::hashjoin::build_class::{own_side, side_by, Side};

    /// The question a decider answers.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Q {
        Raise,
        Acts,
        User,
        Calls,
        Quiet,
        /// RAISE or ACTS (a reader that asks 'may raise or call a VOLATILE function').
        Loud,
    }

    impl Q {
        fn name(self) -> &'static str {
            match self {
                Q::Raise => "RAISE",
                Q::Acts => "ACTS",
                Q::User => "USER",
                Q::Calls => "CALLS",
                Q::Quiet => "QUIET",
                Q::Loud => "LOUD",
            }
        }
    }

    /// The reference's answers for one subject.
    #[derive(Clone, Copy)]
    struct Ref {
        raise: bool,
        acts: bool,
        user: bool,
    }

    impl Ref {
        fn of(self, q: Q) -> bool {
            match q {
                Q::Raise => self.raise,
                Q::Acts => self.acts,
                Q::User => self.user,
                Q::Calls => self.acts || self.user,
                Q::Quiet => !self.raise && !self.acts && !self.user,
                Q::Loud => self.raise || self.acts,
            }
        }
    }

    /// One subject's world: a graph of one pipeline (a breaker's rows, no source of its own) that filters on the subject's
    /// expression or runs its step, the effects facts and the catalog over it, and for an expression a Result plan over it.
    struct World<'w> {
        gx: Gx<'w, 'static>,
        arena: ArenaRef,
        expr: Option<Node<'static>>,
        stmt: Option<&'static PlannedStmt<'static>>,
        /// The same graph with the subject's statement (`graph.stmt`): the statement readers' world.
        sx: Gx<'w, 'static>,
    }

    /// A decider's answer to its question for the world's subject; none where it does not apply (an expression reader over a
    /// plan step).
    type Decide = fn(&World<'_>) -> Option<bool>;

    /// Every decider asked, by name, with the question it answers. The expression readers read the subject's expression; the
    /// pipeline readers read pipeline 0.
    const DECIDERS: &[(&str, Q, Decide)] = &[
        // decompose: the F5 walk (rerun.rs: what `of_node`, `of_plan` and the graph's facts read) and `of_plan` over a Result
        ("decompose.f5", Q::Raise, |w| w.expr.map(|n| !gravity_decompose::rerun::of_expr_alone(w.gx.facts, w.arena, n).raise_free)),
        ("decompose.of_plan", Q::Raise, |w| w.stmt.and_then(|s| s.planTree).map(|r| effects::of_plan(&Cx::new(w.stmt.unwrap(), w.gx.facts, w.arena), r).may_raise)),
        ("decompose.of_plan", Q::Acts, |w| w.stmt.and_then(|s| s.planTree).map(|r| effects::of_plan(&Cx::new(w.stmt.unwrap(), w.gx.facts, w.arena), r).acts)),
        // lowerv4's expression readers
        ("lowerv4.raise_free", Q::Raise, |w| w.expr.map(|n| !crate::effects::raise_free(&w.gx, n))),
        ("lowerv4.raise_free_each", Q::Raise, |w| w.expr.map(|n| !crate::effects::raise_free_each(&w.gx, n, &|_| false))),
        ("lowerv4.merge.pure", Q::Raise, |w| w.expr.map(|n| !crate::effects::visible::merge::pure(&w.gx, n))),
        ("lowerv4.nestloop.quiet_in", Q::Raise, |w| w.expr.map(|n| !crate::effects::visible::nestloop::quiet_in(&w.gx, n, false))),
        ("lowerv4.pgrc.quiet_qual", Q::Raise, |w| w.expr.map(|n| !crate::effects::visible::pgrc::quiet_qual(&w.gx, n))),
        ("lowerv4.volatile", Q::Acts, |w| w.expr.map(|n| crate::effects::volatile(&w.gx, n))),
        ("lowerv4.nestloop.acts", Q::Acts, |w| w.expr.map(|n| crate::effects::visible::nestloop::acts(&w.gx, w.arena, n))),
        ("lowerv4.srf.volatile", Q::Acts, |w| w.expr.map(|n| crate::effects::visible::srf::volatile(&w.gx, w.arena, n))),
        ("lowerv4.dict.call_acts", Q::Acts, |w| w.expr.and_then(|n| n.as_func_expr().map(|f| f.funcid)).map(|f| crate::effects::visible::dict::call_acts(w.gx.facts, f))),
        ("lowerv4.topn.order_volatile", Q::Acts, |w| w.expr.map(|n| crate::effects::visible::topn::order_volatile(&w.gx, n))),
        ("lowerv4.user_fn", Q::User, |w| w.expr.map(|n| crate::effects::user_fn(&w.gx, n))),
        ("lowerv4.user_or_volatile", Q::Calls, |w| w.expr.map(|n| crate::effects::user_or_volatile(&w.gx, n))),
        ("lowerv4.side_calls", Q::Calls, |w| w.expr.map(|n| crate::effects::visible::hashjoin::side_calls(&w.gx, n))),
        ("lowerv4.side_calls_body", Q::Calls, |w| w.expr.map(|n| crate::effects::visible::hashjoin::side_calls_body(&w.gx, n))),
        ("lowerv4.lockrows.calm_call", Q::Calls, |w| w.expr.map(|n| !crate::effects::visible::lockrows::calm_call(&w.gx, n))),
        ("lowerv4.expr_quiet", Q::Quiet, |w| w.expr.map(|n| crate::effects::expr_quiet(&w.gx, n))),
        // lowerv4's pipeline readers
        ("lowerv4.own_side", Q::Calls, |w| Some(own_side(&w.gx, 0) == Side::UserFn)),
        ("lowerv4.own_side", Q::Quiet, |w| Some(own_side(&w.gx, 0) == Side::Safe)),
        ("lowerv4.side_by", Q::Calls, |w| Some(side_by(&w.gx, 0, usize::MAX, false) == Side::UserFn)),
        ("lowerv4.side_by", Q::Quiet, |w| Some(side_by(&w.gx, 0, usize::MAX, false) == Side::Safe)),
        ("lowerv4.pipeline_effects", Q::Raise, |w| Some(crate::effects::pipeline_effects(&w.gx, w.arena, 0, false).may_raise)),
        ("lowerv4.pipeline_effects", Q::Acts, |w| Some(crate::effects::pipeline_effects(&w.gx, w.arena, 0, false).acts)),
        ("lowerv4.op_raise_free", Q::Raise, |w| Some(!crate::effects::op_raise_free(&w.gx, &w.gx.graph.pipelines[0].ops[0], None))),
        // more lowerv4 readers: an expression reader's sum and the readings its consumers combine
        ("lowerv4.nestloop.loud", Q::Loud, |w| w.expr.map(|n| crate::effects::visible::nestloop::loud(&w.gx, &w.gx.graph.pipelines[0], n))),
        ("lowerv4.side_class", Q::Calls, |w| w.expr.map(|n| crate::effects::visible::hashjoin::side_calls(&w.gx, n) || crate::effects::visible::hashjoin::hidden(&w.gx, n))),
        ("lowerv4.ahead_side", Q::Calls, |w| w.expr.map(|_| side_by(&w.gx, 0, usize::MAX, false) == Side::UserFn || crate::effects::hides_calls(&w.gx, 0, false))),
        ("lowerv4.licence_side", Q::Calls, |w| w.expr.map(|_| side_by(&w.gx, 0, usize::MAX, false) == Side::UserFn || crate::effects::hides_user_code(&w.gx, 0))),
        ("lowerv4.statement_user_code", Q::User, |w| w.stmt.map(|_| crate::effects::statement_user_code(&w.sx, w.arena))),
        ("lowerv4.merge.ops_can_raise", Q::Raise, |w| Some(crate::effects::visible::merge::ops_can_raise(&w.gx, &w.gx.graph.pipelines[0].ops[..]))),
        ("lowerv4.material.rows_quiet", Q::Quiet, |w| Some(crate::effects::visible::material::rows_quiet(&w.gx, &w.gx.graph.pipelines[0].ops[..]))),
        ("lowerv4.merge.quiet", Q::Quiet, |w| Some(crate::effects::visible::merge::quiet(&w.gx, 0, usize::MAX))),
        ("lowerv4.drain.pipe_acts", Q::Acts, |w| Some(crate::effects::visible::drain::pipe_acts(&w.gx, &w.gx.graph.pipelines[0], 0, 0))),
        ("lowerv4.ops_raise_free", Q::Raise, |w| Some(!crate::effects::ops_raise_free(&w.gx, &w.gx.graph.pipelines[0].ops[..]))),
        ("lowerv4.fed_user_code", Q::User, |w| Some(crate::effects::fed_user_code(&w.gx, 0))),
        // one-sided: a comparison it reads as quiet must not raise; it says nothing of the others
        ("lowerv4.errorhold.quiet_cmp", Q::Raise, |w| w.expr.and_then(|n| crate::effects::visible::errorhold::quiet_cmp(&w.gx, n).then_some(false))),
    ];

    /// A ruled disagreement: each `decider QUESTION direction` of `deciders` on each subject of `subjects`, and the ruling that
    /// makes it so (a D-id or a design rule; an id this test booked is a bug to fix, its row deleted with the fix). `id` names
    /// the row for `INHERITS`.
    struct Known {
        id: &'static str,
        deciders: &'static [&'static str],
        subjects: &'static [&'static str],
        ruling: &'static str,
    }

    /// Today's disagreements (the first run: trunk 846c59db7c5 with gravfix3#2630, D3897). A decider or a subject is only
    /// deleted from a rule.
    const KNOWN: &[Known] = &[
        Known {
            id: "D1236",
            deciders: &["lowerv4.raise_free RAISE permissive", "lowerv4.raise_free_each RAISE permissive", "lowerv4.merge.pure RAISE permissive", "lowerv4.nestloop.quiet_in RAISE permissive", "lowerv4.pgrc.quiet_qual RAISE permissive", "lowerv4.op_raise_free RAISE permissive", "lowerv4.expr_quiet QUIET permissive", "lowerv4.own_side QUIET permissive", "lowerv4.side_by QUIET permissive"],
            subjects: &[
                "node MinMaxExpr greatest 16", "node MinMaxExpr greatest 20", "node MinMaxExpr greatest 23", "node MinMaxExpr greatest 25",
                "node MinMaxExpr greatest 700", "node MinMaxExpr greatest 701", "node MinMaxExpr greatest 1043", "node MinMaxExpr greatest 1082",
                "node MinMaxExpr greatest 1184", "node MinMaxExpr greatest 1186", "node MinMaxExpr greatest 1700", "node MinMaxExpr greatest 2950",
            ],
            ruling: "D1236 (MERGE-3b PM2): lowering's one raise-free walk (raise_free.rs, under every reading here) reads GREATEST / LEAST over a type whose compare the home reads pure (`cmp_pure`: effects::of_compare), its collation set, as raise-free: PostgreSQL 18.6's ExecEvalMinMax only calls that compare. The home's quiet_node does not list a MinMaxExpr (and F5 reads every one as raising). C1 (a) moves the rule into quiet_node.",
        },
        Known {
            id: "D449",
            deciders: &["lowerv4.merge.pure RAISE conservative", "lowerv4.nestloop.quiet_in RAISE conservative", "lowerv4.expr_quiet QUIET conservative"],
            subjects: &[
                "agg 2100 avg(int8)", "agg 2101 avg(int4)", "agg 2102 avg(int2)", "agg 2107 sum(int8)", "agg 2108 sum(int4)", "agg 2109 sum(int2)",
                "agg 2115 max(int8)", "agg 2116 max(int4)", "agg 2117 max(int2)", "agg 2118 max(oid)", "agg 2119 max(float4)", "agg 2120 max(float8)",
                "agg 2122 max(date)", "agg 2123 max(time)", "agg 2124 max(timetz)", "agg 2125 max(money)", "agg 2126 max(timestamp)",
                "agg 2127 max(timestamptz)", "agg 2128 max(interval)", "agg 2130 max(numeric)", "agg 2131 min(int8)", "agg 2132 min(int4)",
                "agg 2133 min(int2)", "agg 2134 min(oid)", "agg 2135 min(float4)", "agg 2136 min(float8)", "agg 2138 min(date)", "agg 2139 min(time)",
                "agg 2140 min(timetz)", "agg 2141 min(money)", "agg 2142 min(timestamp)", "agg 2143 min(timestamptz)", "agg 2144 min(interval)",
                "agg 2146 min(numeric)", "agg 2147 count(any)", "agg 2148 variance(int8)", "agg 2149 variance(int4)", "agg 2150 variance(int2)",
                "agg 2154 stddev(int8)", "agg 2155 stddev(int4)", "agg 2156 stddev(int2)", "agg 2236 bit_and(int2)", "agg 2237 bit_or(int2)",
                "agg 2238 bit_and(int4)", "agg 2239 bit_or(int4)", "agg 2240 bit_and(int8)", "agg 2241 bit_or(int8)", "agg 2517 bool_and(bool)",
                "agg 2518 bool_or(bool)", "agg 2519 every(bool)", "agg 2641 var_samp(int8)", "agg 2642 var_samp(int4)", "agg 2643 var_samp(int2)",
                "agg 2712 stddev_samp(int8)", "agg 2713 stddev_samp(int4)", "agg 2714 stddev_samp(int2)", "agg 2718 var_pop(int8)",
                "agg 2719 var_pop(int4)", "agg 2720 var_pop(int2)", "agg 2724 stddev_pop(int8)", "agg 2725 stddev_pop(int4)", "agg 2726 stddev_pop(int2)",
                "agg 2797 max(tid)", "agg 2798 min(tid)", "agg 2803 count()", "agg 2818 regr_count(float8,float8)", "agg 4189 max(pg_lsn)",
                "agg 4190 min(pg_lsn)", "agg 5099 max(xid8)", "agg 5100 min(xid8)", "agg 6164 bit_xor(int2)", "agg 6165 bit_xor(int4)",
                "agg 6166 bit_xor(int8)",
            ],
            ruling: "D449 / D1486: the row-form readings (merge.rs `pure`, which expr_quiet asks through ops_can_raise; nestloop.rs `quiet_in`, the union arms' reading) take no aggregate as a leaf: its transitions ran in the pipelines that fold into the slot it reads. The home's quiet_node reads a builtin aggregate `agg_quiet` passes as raise-free.",
        },
        Known {
            id: "D1902",
            deciders: &["decompose.f5 RAISE permissive", "decompose.of_plan RAISE permissive"],
            subjects: &["node Param exec"],
            ruling: "D1902: the home's expression reader reads a PARAM_EXEC as raise-free only where its owner binds it (Scope::bound); F5 reads the parameter as a leaf and the InitPlan or SubPlan that sets it as a plan of its own (of_plan walks the statement's).",
        },
        Known {
            id: "D2107",
            deciders: &["lowerv4.own_side QUIET conservative", "lowerv4.side_by QUIET conservative", "lowerv4.op_raise_free RAISE conservative"],
            subjects: &["step nest (inner of no op, quiet facts)"],
            ruling: "D2107 (accepted, main 2026-10-11T06:08:10Z): own_side and side_by read every nest step as raising (its embedded inner runs per outer row where the nest does), and op_raise_free reads a nest by its F5 facts only under drain's TAILSTOP reading (`tail`); the home's of_ops reads the nest's inner, parameters and qual (of_join), all quiet here.",
        },
        Known {
            id: "user_fn-calls",
            deciders: &["lowerv4.user_fn USER permissive"],
            subjects: &["node CoerceToDomain (a domain no catalog row answers)", "node NextValueExpr"],
            ruling: "user_fn's calls reading reads a call of a user function: a NextValueExpr runs nextval_internal, builtin code, and a domain cast's CHECKs are read by volatile_check / check_acts (D1899, D1907) and by user_fn's Hidden reading (D1906). F5 reads both as unknown code (fail closed; D3796 for a domain whose CHECKs the facts cannot read).",
        },
        Known {
            id: "T6j",
            deciders: &["lowerv4.own_side CALLS permissive", "lowerv4.side_by CALLS permissive"],
            subjects: &["step limit 5"],
            ruling: "T6j: of_ops reads an op it does not model as unknown (it acts: fail closed); own_side and side_by read a Limit as raising, never as a call (its bounds here are constants).",
        },
        Known {
            id: "D3897",
            deciders: &["lowerv4.own_side CALLS permissive", "lowerv4.own_side QUIET permissive", "lowerv4.side_by CALLS permissive", "lowerv4.side_by QUIET permissive"],
            subjects: &["step drain right"],
            ruling: "D3897: a RIGHT / FULL drain hands on the build's rows by their match bits and evaluates nothing (its join's keys and qual are the probe's), so own_side reads the side as its source's; of_ops reads a drain as unknown (T6j: of_join's hash probe, drain, cache and spool).",
        },
        Known {
            id: "D2731",
            deciders: &["lowerv4.own_side CALLS permissive", "lowerv4.own_side QUIET permissive", "lowerv4.side_by CALLS permissive", "lowerv4.side_by QUIET permissive"],
            subjects: &["step spool"],
            ruling: "D2731: a Material's spool write moves the rows its pipeline made and evaluates nothing (own_side reads the side as its source's); of_ops reads a spool as unknown (T6j).",
        },
        Known {
            id: "W4b-RowX",
            deciders: &["lowerv4.raise_free_each RAISE permissive", "lowerv4.side_by QUIET permissive"],
            subjects: &["fn 2091 array_lower(anyarray,int4)", "fn 2092 array_upper(anyarray,int4)"],
            ruling: "W4b-RowX: side_by's PG-quiet reading (raise_free_each) takes PostgreSQL 18.6's quiet array reads: array_lower / array_upper answer NULL out of range.",
        },
        Known {
            id: "W4b-RowX-const",
            deciders: &["lowerv4.raise_free_each RAISE permissive", "lowerv4.side_by QUIET permissive"],
            subjects: &[
                "fn 2091 array_lower(anyarray,int4) $2=-1", "fn 2091 array_lower(anyarray,int4) $2=-2147483648",
                "fn 2091 array_lower(anyarray,int4) $2=0", "fn 2091 array_lower(anyarray,int4) $2=2",
                "fn 2091 array_lower(anyarray,int4) $2=NULL", "fn 2092 array_upper(anyarray,int4) $2=-1",
                "fn 2092 array_upper(anyarray,int4) $2=-2147483648", "fn 2092 array_upper(anyarray,int4) $2=0",
                "fn 2092 array_upper(anyarray,int4) $2=2", "fn 2092 array_upper(anyarray,int4) $2=NULL",
            ],
            ruling: "W4b-RowX (effects debt), the same reading over constant arguments: array_lower / array_upper answer NULL out of range (PostgreSQL 18.6 arrayfuncs.c), whatever the dimension.",
        },
    ];

    /// The number of (decider, subject) pairs `KNOWN` lists: it only goes down, with the rows.
    const CEILING: usize = 368;

    /// ONE-WAY: a decider its doc says fails closed counts only its permissive rows; a
    /// conservative one refuses more or runs slower, never serves wrong. Each is still a second home: its lint/EFFECTS-DEBT
    /// row stays (lint RV keeps it) until the decider is deleted onto the home's reader, and its line here goes with it.
    /// (decider, where its doc says so, its EFFECTS-DEBT row)
    const ONE_WAY: &[(&str, &str, &str)] = &[
        (
            "lowerv4.nestloop.loud",
            "nestloop.rs `loud` / `own`: an unknown node or function counts, a user one is never quiet, any node but a column, a constant, a plain parameter or a structural one counts",
            "lowerv4.src.effects.visible.nestloop.rs@loud",
        ),
        (
            "lowerv4.drain.pipe_acts",
            "drain.rs `acts` (volatile.rs `Reading::spool`): a visible effect of any kind (user code, a NOTICE, its own row's act), every domain cast and every node the walk cannot see",
            "lowerv4.src.effects.visible.drain.rs@acts",
        ),
    ];

    /// INHERITS: a decider built on a reading a `KNOWN` row rules disagrees the same way on that
    /// row's subjects; there it is covered by the row, not new, and does not count against `CEILING`. (the row's id, each
    /// `decider QUESTION direction` it covers): deleting the row (its fix) brings these back as NEW.
    const INHERITS: &[(&str, &[&str])] = &[
        (
            "D1236",
            &["lowerv4.merge.ops_can_raise RAISE permissive", "lowerv4.ops_raise_free RAISE permissive", "lowerv4.merge.quiet QUIET permissive", "lowerv4.material.rows_quiet QUIET permissive"],
        ),
        ("D449", &["lowerv4.merge.ops_can_raise RAISE conservative", "lowerv4.material.rows_quiet QUIET conservative"]),
        ("D1902", &["lowerv4.nestloop.loud LOUD permissive"]),
        ("D2107", &["lowerv4.ops_raise_free RAISE conservative"]),
        ("user_fn-calls", &["lowerv4.fed_user_code USER permissive"]),
        (
            "T6j",
            &["lowerv4.merge.ops_can_raise RAISE permissive", "lowerv4.merge.quiet QUIET permissive", "lowerv4.material.rows_quiet QUIET permissive", "lowerv4.drain.pipe_acts ACTS permissive"],
        ),
        ("D3897", &["lowerv4.merge.ops_can_raise RAISE permissive", "lowerv4.merge.quiet QUIET permissive", "lowerv4.drain.pipe_acts ACTS permissive"]),
        ("D2731", &["lowerv4.merge.ops_can_raise RAISE permissive", "lowerv4.merge.quiet QUIET permissive", "lowerv4.drain.pipe_acts ACTS permissive"]),
    ];

    #[test]
    fn every_decider_agrees_with_the_effects_home_or_is_listed() {
        let cat = Dat::load();
        let facts = FixtureFacts::default();
        let m = mcx();
        let subjects = subjects(m, &cat);
        assert!(subjects.len() > 5000, "only {} subjects: the reference catalog's rows were not read", subjects.len());
        let mut found = BTreeSet::new();
        let mut asked = vec![0usize; DECIDERS.len()]; // alloc: Cold
        for s in &subjects {
            ask(s, &cat, &facts, m, &mut asked, &mut found);
        }
        let idle: Vec<&str> = DECIDERS.iter().zip(&asked).filter(|(_, k)| **k == 0).map(|(d, _)| d.0).collect(); // alloc: Cold
        assert!(idle.is_empty(), "deciders that answered no subject: {idle:?}");
        let mut known: BTreeMap<String, &str> = BTreeMap::new();
        let mut ids = BTreeSet::new();
        for k in KNOWN {
            assert!(ids.insert(k.id), "two KNOWN rows have the id `{}`", k.id);
            for d in k.deciders {
                for s in k.subjects {
                    assert!(known.insert(format!("{d} {s}"), k.ruling).is_none(), "KNOWN lists `{d} {s}` twice");
                }
            }
        }
        assert_eq!(known.len(), CEILING, "KNOWN lists {} disagreements, CEILING says {CEILING}: the list only shrinks, and CEILING with it", known.len());
        for &(d, doc, row) in ONE_WAY {
            assert!(DECIDERS.iter().any(|x| x.0 == d), "ONE_WAY lists `{d}` ({doc}; effects debt {row}), which DECIDERS does not ask: delete its line");
        }
        // `decider QUESTION direction subject` -> the id of the KNOWN row it inherits
        let mut inherited: BTreeMap<String, &str> = BTreeMap::new();
        for &(id, ds) in INHERITS {
            let k = KNOWN.iter().find(|k| k.id == id).unwrap_or_else(|| panic!("INHERITS names `{id}`, which is no KNOWN row's id"));
            for d in ds {
                let dir = d.rsplit(' ').next().unwrap_or_default();
                assert!(k.deciders.iter().any(|x| x.rsplit(' ').next() == Some(dir)), "INHERITS reads `{d}` under `{id}`, which rules no {dir} reading");
                for s in k.subjects {
                    let key = format!("{d} {s}");
                    assert!(!known.contains_key(&key), "`{key}` is both KNOWN and inherited from `{id}`");
                    assert!(inherited.insert(key, id).is_none(), "`{d} {s}` is inherited twice");
                }
            }
        }
        let new: Vec<&String> = found.iter().filter(|k| !known.contains_key(k.as_str()) && !inherited.contains_key(k.as_str())).collect(); // alloc: Cold
        let mut gone: Vec<String> = known.keys().filter(|k| !found.contains(*k)).cloned().collect(); // alloc: Cold
        for &(id, ds) in INHERITS {
            for d in ds {
                if !inherited.iter().any(|(k, i)| *i == id && found.contains(k) && k.starts_with(d) && k.as_bytes().get(d.len()) == Some(&b' ')) {
                    gone.push(format!("INHERITS {id}: {d}"));
                }
            }
        }
        // the new ones as rows to paste, per `decider QUESTION direction`
        let mut by: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for k in &new {
            let mut it = k.splitn(4, ' ');
            let d = it.by_ref().take(3).collect::<Vec<_>>().join(" ");
            by.entry(d).or_default().extend(it.next());
        }
        let rows: Vec<String> = by.iter().map(|(d, ss)| format!("    Known {{ id: \"<id>\", deciders: &[\"{d}\"], subjects: &[{}], ruling: \"<ruling>\" }},", ss.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", "))).collect(); // alloc: Cold
        assert!(
            new.is_empty() && gone.is_empty(),
            "effects deciders disagree with the effects home ({} subjects, {} deciders).\nNEW ({}): name the ruling that makes each so, or book the bug it is (an INHERITS line where a KNOWN row rules the reading it is built on):\n{}\nNO LONGER DISAGREEING ({}): delete them from KNOWN (lower CEILING) or INHERITS:\n{}",
            subjects.len(),
            DECIDERS.len(),
            new.len(),
            rows.join("\n"),
            gone.len(),
            gone.join("\n")
        );
    }

    /// Asks every decider about subject `s` beside the home's reference reading; a disagreement goes into `found` as
    /// `decider QUESTION direction subject`, and `asked` counts the subjects each decider answered.
    fn ask(s: &Subject, cat: &Dat, facts: &FixtureFacts, m: Mcx<'static>, asked: &mut [usize], found: &mut BTreeSet<String>) {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 20);
        let a = arena.handle();
        let (op, expr) = match &s.what {
            What::Expr(n) => (Op::Filter(*n), Some(*n)),
            What::Step(f) => (f(a, m), None),
        };
        let g = graph(a, op);
        let cx = Cx::without_stmt(facts, a);
        let reference = match expr {
            Some(n) => {
                let e = effects::of_expr(&cx, n, &PLAIN);
                Ref { raise: e.may_raise, acts: e.acts, user: gravity_decompose::rerun::of_expr_alone(facts, a, n).user_call }
            }
            None => {
                let e = effects::of_ops(&cx, &g.pipelines[0].ops[..], &PLAIN);
                Ref { raise: e.may_raise, acts: e.acts, user: false }
            }
        };
        let stmt = expr.map(|n| statement(m, n));
        let mut g2 = graph(a, match &s.what { What::Expr(n) => Op::Filter(*n), What::Step(f) => f(a, m) });
        g2.stmt = stmt;
        let w = World { gx: Gx { graph: &g, facts, catalog: cat, over: None, mcx: rig.mcx() }, arena: a, expr, stmt, sx: Gx { graph: &g2, facts, catalog: cat, over: None, mcx: rig.mcx() } };
        for (i, &(name, q, decide)) in DECIDERS.iter().enumerate() {
            match catch_unwind(AssertUnwindSafe(|| decide(&w))) {
                Ok(None) => {}
                Ok(Some(v)) => {
                    asked[i] += 1;
                    if v != reference.of(q) {
                        // the decider reads the code as safer than the home does (the WRONG direction), or as less safe
                        let permissive = if q == Q::Quiet { v } else { !v };
                        if !permissive && ONE_WAY.iter().any(|o| o.0 == name) {
                            continue;
                        }
                        let dir = if permissive { "permissive" } else { "conservative" };
                        found.insert(format!("{name} {} {dir} {}", q.name(), s.name));
                    }
                }
                Err(_) => {
                    found.insert(format!("{name} {} {} PANIC", q.name(), s.name));
                }
            }
        }
    }

    /// D3954 and D3955, which the sweep found: nestloop.rs `own` reads a comparison over an indeterminate collation as loud
    /// (PostgreSQL 18.6 raises at its first input), and merge.rs `ops_can_raise` reads a nested loop step by its inner.
    #[test]
    fn d3954_d3955_loud_and_ops_can_raise_agree_with_the_home() {
        let cat = Dat::load();
        let facts = FixtureFacts::default();
        let m = mcx();
        let pins = [
            "op 98 text = text C vs POSIX",
            "op 664 text < text C vs POSIX",
            "op 3877 text ^@ text C vs POSIX",
            "step nest (inner filtering on int4 / int4, raising facts)",
            "step nest (inner projecting random(), VOLATILE facts)",
        ];
        let mut found = BTreeSet::new();
        let mut asked = vec![0usize; DECIDERS.len()]; // alloc: Cold
        let mut seen = 0;
        for s in subjects(m, &cat).iter().filter(|s| pins.contains(&s.name.as_str())) {
            seen += 1;
            ask(s, &cat, &facts, m, &mut asked, &mut found);
        }
        assert_eq!(seen, pins.len(), "a pinned subject is no longer built: {pins:?}");
        let back: Vec<&String> = found.iter().filter(|k| k.starts_with("lowerv4.nestloop.loud ") || k.starts_with("lowerv4.merge.ops_can_raise ")).collect(); // alloc: Cold
        assert!(back.is_empty(), "D3954 / D3955 are back: {back:?}");
    }

    /// The Result plan over `n` (`decompose.of_plan` reads it), its statement a SELECT.
    fn statement(m: Mcx<'static>, n: Node<'static>) -> &'static PlannedStmt<'static> {
        let mut tl = NodeList::nil();
        tl.lappend(m, entry(m, n, 1)).expect("tlist");
        let root = Node::mk(m, ResultPlan { plan: Plan { targetlist: tl, ..Default::default() }, resconstantqual: None }).expect("result");
        Box::leak(Box::new(PlannedStmt { commandType: CmdType::CMD_SELECT, planTree: Some(root), ..Default::default() })) // alloc: Cold
    }

    /// One pipeline over a breaker's rows (no producer: what feeds it is no subject's), running `op`, into the sink.
    fn graph(a: ArenaRef, op: Op<'static>) -> PipelineGraph<'static> {
        let mut g = PipelineGraph::new(a);
        let mut p = PipelineNode::new(a, 1, Source::Breaker { slot: 0 }, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
        p.push(op);
        g.pipelines.push(p);
        g.root = 0;
        g
    }

    fn mcx() -> Mcx<'static> {
        let cx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("effects agreement"))); // alloc: Cold
        cx.mcx()
    }

    enum What {
        Expr(Node<'static>),
        /// A plan step, made in the subject's arena.
        Step(fn(ArenaRef, Mcx<'static>) -> Op<'static>),
    }

    struct Subject {
        name: String,
        what: What,
    }

    fn var(m: Mcx<'static>, t: Oid, at: i16) -> Node<'static> {
        Node::mk(m, Var { varno: 1, varattno: at, vartype: t, vartypmod: -1, ..Default::default() }).expect("var")
    }

    /// A column of each type in `ts`.
    fn cols(m: Mcx<'static>, ts: &[Oid]) -> NodeList<'static> {
        let mut l = NodeList::nil();
        for (i, &t) in ts.iter().enumerate() {
            l.lappend(m, var(m, t, i as i16 + 1)).expect("cols");
        }
        l
    }

    /// Representative constants of type `t`, NULL included: the divisor (0, -1, 2), length (INT32_MIN), unit ('year',
    /// 'fortnight'), privilege ('SELECT') and empty-text classes decompose `quiet_args` reads.
    fn consts(m: Mcx<'static>, t: Oid) -> Vec<(String, Node<'static>)> {
        let mut out = Vec::new(); // alloc: Cold
        let int = |v: i64, len: i32, d: Datum| (v.to_string(), Node::mk_const(m, t, -1, 0, len, d, false, true).expect("int const"));
        match t {
            21 => for v in [0i16, -1, 2] { out.push(int(v.into(), 2, Datum::from_i16(v))) },
            23 => for v in [0i32, -1, 2, i32::MIN] { out.push(int(v.into(), 4, Datum::from_i32(v))) },
            20 => for v in [0i64, -1, 2] { out.push(int(v, 8, Datum::from_i64(v))) },
            TEXT => for s in ["year", "SELECT", "fortnight", ""] {
                out.push((format!("'{s}'"), gravity_decompose::normalize::text_const(m, s.as_bytes(), DEFAULT_COLLATION).expect("text const")))
            },
            _ => return out,
        }
        let len = match t { 21 => 2, 23 => 4, 20 => 8, _ => -1 };
        out.push(("NULL".into(), Node::mk_const(m, t, -1, if t == TEXT { DEFAULT_COLLATION } else { 0 }, len, Datum::from_usize(0), true, t != 25).expect("null const")));
        out
    }

    /// The columns of `cols`, with argument `at` replaced by `c`.
    fn cols_with(m: Mcx<'static>, ts: &[Oid], at: usize, c: Node<'static>) -> NodeList<'static> {
        let mut l = NodeList::nil();
        for (i, &t) in ts.iter().enumerate() {
            l.lappend(m, if i == at { c } else { var(m, t, i as i16 + 1) }).expect("cols_with");
        }
        l
    }

    fn entry(m: Mcx<'static>, e: Node<'static>, resno: i16) -> Node<'static> {
        Node::mk(m, TargetEntry { expr: e, resno, resname: None, ressortgroupref: 0, resorigtbl: 0, resorigcol: 0, resjunk: false }).expect("entry")
    }

    /// An aggregate's arguments: a target entry over a column of each type in `ts`.
    fn entries(m: Mcx<'static>, ts: &[Oid], from: usize) -> NodeList<'static> {
        let mut l = NodeList::nil();
        for (i, &t) in ts.iter().enumerate() {
            l.lappend(m, entry(m, var(m, t, (from + i) as i16 + 1), i as i16 + 1)).expect("entries");
        }
        l
    }

    fn oids(m: Mcx<'static>, ts: &[Oid]) -> OidList<'static> {
        let mut l = OidList::nil();
        for &t in ts {
            l.lappend(m, t).expect("oids");
        }
        l
    }

    const BOOL: Oid = 16;
    const INT4: Oid = 23;
    const INT8: Oid = 20;
    const TEXT: Oid = 25;
    const RECORD: Oid = 2249;
    /// pg_collation.dat: the database default, C and POSIX.
    const DEFAULT_COLLATION: Oid = 100;
    const C_COLLATION: Oid = 950;
    const POSIX_COLLATION: Oid = 951;
    /// A domain (or sequence) no catalog row answers: a user's.
    const USER_OBJECT: Oid = 16_500;

    /// Every subject: the reference catalog's builtins, then the node kinds and plan steps.
    fn subjects(m: Mcx<'static>, cat: &Dat) -> Vec<Subject> {
        let mut out = Vec::new(); // alloc: Cold
        let mut push = |name: String, n: Node<'static>| out.push(Subject { name, what: What::Expr(n) });
        let sig = |ts: &[Oid]| ts.iter().map(|t| cat.type_name(*t)).collect::<Vec<_>>().join(",");
        for (&oid, p) in &cat.procs {
            let name = format!("{oid} {}({})", p.name, sig(&p.args[..]));
            match p.kind {
                b'f' => {
                    let n = Node::mk(m, FuncExpr { funcid: oid, funcresulttype: p.ret, funcretset: p.facts.retset, args: cols(m, &p.args), ..Default::default() }).expect("func");
                    push(format!("fn {name}"), n);
                }
                b'a' => {
                    let (kind, direct) = cat.aggs.get(&oid).copied().unwrap_or((b'n' as i8, 0));
                    let direct = direct.min(p.args.len());
                    let a = Aggref {
                        aggfnoid: oid,
                        aggtype: p.ret,
                        aggargtypes: oids(m, &p.args),
                        aggdirectargs: cols(m, &p.args[..direct]),
                        args: entries(m, &p.args[direct..], direct),
                        aggstar: p.args.is_empty(),
                        aggkind: kind,
                        ..Default::default()
                    };
                    push(format!("agg {name}"), Node::mk(m, a).expect("agg"));
                }
                b'w' => {
                    let n = Node::mk(m, WindowFunc { winfnoid: oid, wintype: p.ret, args: cols(m, &p.args), winref: 1, ..Default::default() }).expect("win");
                    push(format!("win {name}"), n);
                }
                _ => {} // a procedure: CALL, never an expression
            }
        }
        for (&oid, o) in &cat.opers {
            let ts: Vec<Oid> = [o.left, o.right].into_iter().filter(|&t| t != 0).collect(); // alloc: Cold
            let name = format!("{oid} {} {} {}", cat.type_name(o.left), o.name, cat.type_name(o.right));
            let n = Node::mk(m, OpExpr { opno: oid, opfuncid: o.code, opresulttype: o.result, args: cols(m, &ts), ..Default::default() }).expect("op");
            push(format!("op {name}"), n);
            if o.name == "=" && o.result == BOOL && o.left != 0 {
                let d = Node::mk(m, DistinctExpr { opno: oid, opfuncid: o.code, opresulttype: BOOL, args: cols(m, &ts), ..Default::default() }).expect("distinct");
                push(format!("distinct {name}"), d);
                let x = Node::mk(m, NullIfExpr { opno: oid, opfuncid: o.code, opresulttype: o.left, args: cols(m, &ts), ..Default::default() }).expect("nullif");
                push(format!("nullif {name}"), x);
            }
            if let (true, true, Some(arr)) = (o.result == BOOL, o.left != 0, cat.array_of(o.right)) {
                let x = ScalarArrayOpExpr { opno: oid, opfuncid: o.code, useOr: true, args: cols(m, &[o.left, arr]), ..Default::default() };
                push(format!("any {name}[]"), Node::mk(m, x).expect("saop"));
            }
        }
        // each function and operator argument as a constant of each representative value, under its own collation
        for (&oid, p) in &cat.procs {
            if p.kind != b'f' {
                continue;
            }
            let name = format!("{oid} {}({})", p.name, sig(&p.args[..]));
            for i in 0..p.args.len() {
                for (label, c) in consts(m, p.args[i]) {
                    let args = cols_with(m, &p.args, i, c);
                    let inputcollid = if p.args[i] == TEXT { DEFAULT_COLLATION } else { 0 };
                    let n = Node::mk(m, FuncExpr { funcid: oid, funcresulttype: p.ret, funcretset: p.facts.retset, args, inputcollid, ..Default::default() }).expect("func");
                    push(format!("fn {name} ${}={label}", i + 1), n);
                }
            }
        }
        for (&oid, o) in &cat.opers {
            let ts: Vec<Oid> = [o.left, o.right].into_iter().filter(|&t| t != 0).collect(); // alloc: Cold
            let name = format!("{oid} {} {} {}", cat.type_name(o.left), o.name, cat.type_name(o.right));
            for i in 0..ts.len() {
                for (label, c) in consts(m, ts[i]) {
                    let inputcollid = if ts[i] == TEXT { DEFAULT_COLLATION } else { 0 };
                    let n = Node::mk(m, OpExpr { opno: oid, opfuncid: o.code, opresulttype: o.result, args: cols_with(m, &ts, i, c), inputcollid, ..Default::default() }).expect("op");
                    push(format!("op {name} ${}={label}", i + 1), n);
                }
            }
            // two collatable columns of different implicit collations (C, POSIX): input collation indeterminate
            if ts.len() == 2 && ts.iter().all(|&t| matches!(t, 19 | 25 | 1042 | 1043)) {
                let mut args = NodeList::nil();
                for (i, (&t, coll)) in ts.iter().zip([C_COLLATION, POSIX_COLLATION]).enumerate() {
                    args.lappend(m, Node::mk(m, Var { varno: 1, varattno: i as i16 + 1, vartype: t, vartypmod: -1, varcollid: coll, ..Default::default() }).expect("var")).expect("args");
                }
                let n = Node::mk(m, OpExpr { opno: oid, opfuncid: o.code, opresulttype: o.result, args, inputcollid: 0, ..Default::default() }).expect("op");
                push(format!("op {name} C vs POSIX"), n);
            }
        }
        // each call with a collatable argument again under one collation, as parse_collate sets it (its columns', its input
        // and its result's): the database default and C, where the collation-gated rules read
        let collatable = |t: &Oid| cat.types.get(t).is_some_and(|f| f.collation != 0);
        let under = |ts: &[Oid], c: Oid| {
            let mut l = NodeList::nil();
            for (i, t) in ts.iter().enumerate() {
                let varcollid = if collatable(t) { c } else { 0 };
                l.lappend(m, Node::mk(m, Var { varno: 1, varattno: i as i16 + 1, vartype: *t, vartypmod: -1, varcollid, ..Default::default() }).expect("var")).expect("cols");
            }
            l
        };
        for (c, label) in [(DEFAULT_COLLATION, "default"), (C_COLLATION, "C")] {
            for (&oid, p) in &cat.procs {
                if p.kind != b'f' || !p.args.iter().any(collatable) {
                    continue;
                }
                let funccollid = if collatable(&p.ret) { c } else { 0 };
                let f = FuncExpr { funcid: oid, funcresulttype: p.ret, funcretset: p.facts.retset, funccollid, inputcollid: c, args: under(&p.args, c), ..Default::default() };
                push(format!("fn {oid} {}({}) collate {label}", p.name, sig(&p.args[..])), Node::mk(m, f).expect("func"));
            }
            for (&oid, o) in &cat.opers {
                let ts: Vec<Oid> = [o.left, o.right].into_iter().filter(|&t| t != 0).collect(); // alloc: Cold
                if !ts.iter().any(collatable) {
                    continue;
                }
                let opcollid = if collatable(&o.result) { c } else { 0 };
                let x = OpExpr { opno: oid, opfuncid: o.code, opresulttype: o.result, opcollid, inputcollid: c, args: under(&ts, c), ..Default::default() };
                push(format!("op {oid} {} {} {} collate {label}", cat.type_name(o.left), o.name, cat.type_name(o.right)), Node::mk(m, x).expect("op"));
            }
        }
        for (&t, f) in &cat.types {
            if !matches!(f.typtype as u8, b'b' | b'r' | b'm') {
                continue;
            }
            let io = |arg: Node<'static>, to: Oid| CoerceViaIO { arg, resulttype: to, resultcollid: 0, coerceformat: CoercionForm::COERCE_EXPLICIT_CAST, location: -1 };
            push(format!("io {}->text", cat.type_name(t)), Node::mk(m, io(var(m, t, 1), TEXT)).expect("io out"));
            push(format!("io text->{}", cat.type_name(t)), Node::mk(m, io(var(m, TEXT, 1), t)).expect("io in"));
        }
        nodes(m, &mut push);
        for (name, f) in STEPS {
            out.push(Subject { name: format!("step {name}"), what: What::Step(*f) });
        }
        out
    }

    /// The leaf and structural node kinds over columns.
    fn nodes(m: Mcx<'static>, push: &mut dyn FnMut(String, Node<'static>)) {
        let mk = |n: PgResult<Node<'static>>| n.expect("node");
        for t in [BOOL, INT4, TEXT, RECORD] {
            push(format!("node Var {t}"), var(m, t, 1));
        }
        for (k, kind) in [("extern", ParamKind::PARAM_EXTERN), ("exec", ParamKind::PARAM_EXEC)] {
            push(format!("node Param {k}"), mk(Node::mk(m, Param { paramkind: kind, paramid: 1, paramtype: INT4, paramtypmod: -1, ..Default::default() })));
        }
        push("node NullTest".into(), mk(Node::mk(m, NullTest { arg: Some(var(m, INT4, 1)), nulltesttype: NullTestType::IS_NULL, ..Default::default() })));
        push("node BoolExpr AND".into(), mk(Node::mk(m, BoolExpr { boolop: BoolExprType::AND_EXPR, args: cols(m, &[BOOL, BOOL]), ..Default::default() })));
        push("node CoalesceExpr".into(), mk(Node::mk(m, CoalesceExpr { coalescetype: INT4, args: cols(m, &[INT4, INT4]), ..Default::default() })));
        let relabel = RelabelType { arg: var(m, INT4, 1), resulttype: 26, resulttypmod: -1, resultcollid: 0, relabelformat: CoercionForm::COERCE_IMPLICIT_CAST, location: -1 };
        push("node RelabelType int4->oid".into(), mk(Node::mk(m, relabel)));
        for (t, multi) in [(INT4, false), (INT4, true)] {
            let a = ArrayExpr { array_typeid: 1007, element_typeid: t, elements: cols(m, &[t, t]), multidims: multi, ..Default::default() };
            push(format!("node ArrayExpr multidims={multi}"), mk(Node::mk(m, a)));
        }
        let row = RowExpr { args: cols(m, &[INT4, TEXT]), row_typeid: RECORD, row_format: CoercionForm::COERCE_IMPLICIT_CAST, colnames: NodeList::nil(), location: -1 };
        push("node RowExpr".into(), mk(Node::mk(m, row)));
        for t in [INT4, INT8, TEXT, 700, 701, 1700, 1082, 1184, 1186, 1043, 16, 2950, 3802, 114, 1007] {
            let g = MinMaxExpr { minmaxtype: t, op: MinMaxOp::IS_GREATEST, args: cols(m, &[t, t]), ..Default::default() };
            push(format!("node MinMaxExpr greatest {t}"), mk(Node::mk(m, g)));
        }
        use SQLValueFunctionOp as S;
        let svf = [
            S::SVFOP_CURRENT_DATE, S::SVFOP_CURRENT_TIME, S::SVFOP_CURRENT_TIME_N, S::SVFOP_CURRENT_TIMESTAMP, S::SVFOP_CURRENT_TIMESTAMP_N, S::SVFOP_LOCALTIME,
            S::SVFOP_LOCALTIME_N, S::SVFOP_LOCALTIMESTAMP, S::SVFOP_LOCALTIMESTAMP_N, S::SVFOP_CURRENT_ROLE, S::SVFOP_CURRENT_USER, S::SVFOP_USER,
            S::SVFOP_SESSION_USER, S::SVFOP_CURRENT_CATALOG, S::SVFOP_CURRENT_SCHEMA,
        ];
        for op in svf {
            let name = format!("node SQLValueFunction {}", op as u32);
            push(name, mk(Node::mk(m, SQLValueFunction { op, typmod: -1, ..Default::default() })));
        }
        push("node NextValueExpr".into(), mk(Node::mk(m, NextValueExpr { seqid: USER_OBJECT, typeId: INT8 })));
        let dom = CoerceToDomain { arg: var(m, INT4, 1), resulttype: USER_OBJECT, resulttypmod: -1, resultcollid: 0, coercionformat: CoercionForm::COERCE_IMPLICIT_CAST, location: -1 };
        push("node CoerceToDomain (a domain no catalog row answers)".into(), mk(Node::mk(m, dom)));
    }

    /// The plan steps the pipeline readers read apart from expressions.
    const STEPS: &[(&str, fn(ArenaRef, Mcx<'static>) -> Op<'static>)] = &[
        ("nest (inner of no op, quiet facts)", |a, _| nest(a, Vec::new(), Volatility::Immutable, true)),
        ("nest (inner filtering on int4 / int4, raising facts)", |a, m| {
            let code = effects::builtin_oper(528).map_or(0, |o| o.code); // int4 / int4
            let div = Node::mk(m, OpExpr { opno: 528, opfuncid: code, opresulttype: INT4, args: cols(m, &[INT4, INT4]), ..Default::default() }).expect("int4div");
            nest(a, vec![Op::Filter(div)], Volatility::Immutable, false)
        }),
        ("nest (inner projecting random(), VOLATILE facts)", |a, m| {
            let random = Node::mk(m, FuncExpr { funcid: 1598, funcresulttype: 701, ..Default::default() }).expect("random");
            let mut l = NodeList::nil();
            l.lappend(m, random).expect("tlist");
            nest(a, vec![Op::Project(Box::leak(Box::new(l)))], Volatility::Volatile, true) // alloc: Cold
        }),
        ("limit 5", |_, _| Op::Limit { count: Bound::Const(5), offset: Bound::All, ties: None }),
        ("spool", |_, _| Op::Join(JoinDesc::Spool)),
        ("drain right", |_, _| Op::Join(JoinDesc::Drain { matches: 1, jointype: JoinType::JOIN_RIGHT })),
    ];

    /// An inner nested loop without parameters or join qual over an embedded inner (a constant's rows) running `ops`, with
    /// the F5 facts `volatility` and `raise_free`.
    fn nest(a: ArenaRef, ops: Vec<Op<'static>>, volatility: Volatility, raise_free: bool) -> Op<'static> {
        let nil: &'static NodeList<'static> = Box::leak(Box::new(NodeList::nil())); // alloc: Cold
        let mut inner = PipelineNode::new(a, 2, Source::Const, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
        ops.into_iter().for_each(|op| inner.push(op));
        let facts = NestFacts { node: 1, invariant: true, volatility, raise_free, eq: None };
        Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, params: nil, inner: ArenaBox::new_in(inner, a), qual: nil, unique: false, inner_node: 2, facts })
    }

    /// The reference catalog (crates/postgres-18.6-reference .dat files) as lowering's `Catalog` reads it: pg_proc's rows,
    /// pg_type's (arrays as genbki makes them from `array_type_oid`), pg_operator's (each `oprcode` the builtin table's),
    /// pg_aggregate's kinds. What a unit test has no syscache for answers an error, as the guard tests' catalogs do.
    struct Dat {
        procs: BTreeMap<Oid, Proc>,
        types: BTreeMap<Oid, TypeFacts>,
        names: HashMap<Oid, String>,
        arrays: HashMap<Oid, Oid>,
        opers: BTreeMap<Oid, Oper>,
        /// pg_aggregate.dat by aggfnoid: aggkind and aggnumdirectargs.
        aggs: HashMap<Oid, (i8, usize)>,
    }

    struct Proc {
        name: String,
        kind: u8,
        ret: Oid,
        args: Vec<Oid>,
        facts: FuncFacts,
    }

    struct Oper {
        name: String,
        left: Oid,
        right: Oid,
        result: Oid,
        code: Oid,
    }

    /// The reference catalog's `.dat` file `file`: one key -> value map per row (quoted values with backslash escapes, `#`
    /// lines skipped), as decompose's tests/effects_home.rs reads pg_proc.dat.
    fn dat(file: &str) -> Vec<BTreeMap<String, String>> {
        let dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset: run under cargo test")).join("../../postgres-18.6-reference/src/include/catalog");
        let text = std::fs::read_to_string(dir.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        let (mut rows, mut cur, mut key) = (Vec::new(), None::<BTreeMap<String, String>>, String::new()); // alloc: Cold
        let mut it = text.chars().peekable();
        while let Some(c) = it.next() {
            let Some(row) = cur.as_mut() else {
                match c {
                    '#' => while it.next_if(|&c| c != '\n').is_some() {},
                    '{' => (cur, key) = (Some(BTreeMap::new()), String::new()), // alloc: Cold
                    _ => {}
                }
                continue;
            };
            match c {
                '\'' => {
                    let mut v = String::new(); // alloc: Cold
                    while let Some(c) = it.next() {
                        match c {
                            '\\' => v.extend(it.next()),
                            '\'' => break,
                            c => v.push(c),
                        }
                    }
                    row.insert(std::mem::take(&mut key), v);
                }
                '}' => rows.extend(cur.take()),
                ',' => key.clear(),
                c if c.is_ascii_alphanumeric() || c == '_' => key.push(c),
                _ => {}
            }
        }
        rows
    }

    fn oid(s: &str) -> Oid {
        s.parse().unwrap_or_else(|_| panic!("not an oid: {s}"))
    }

    impl Dat {
        fn load() -> Dat {
            let (mut types, mut names, mut by_name, mut arrays) = (BTreeMap::new(), HashMap::new(), HashMap::new(), HashMap::new());
            let mut elems = Vec::new(); // alloc: Cold
            for r in dat("pg_type.dat") {
                let t = oid(&r["oid"]);
                let letter = |k: &str, d: u8| r.get(k).and_then(|v| v.bytes().next()).unwrap_or(d);
                let len = match r.get("typlen").map(String::as_str) {
                    Some("NAMEDATALEN") => 64,
                    Some("SIZEOF_POINTER") => 8,
                    Some(s) => s.parse().unwrap_or(-1),
                    None => -1,
                };
                let byval = matches!(r.get("typbyval").map(String::as_str), Some("t" | "FLOAT8PASSBYVAL"));
                let collation = match r.get("typcollation").map(String::as_str) {
                    Some("default") => 100,
                    Some("C") => 950,
                    _ => 0,
                };
                let f = TypeFacts { len, byval, align: letter("typalign", b'i') as i8, typtype: letter("typtype", b'b') as i8, collation, elem: 0 };
                types.insert(t, f);
                // a true array's element (get_element_type: the array subscript handler); a fixed-length type's typelem is not one
                if r.get("typsubscript").is_some_and(|s| s == "array_subscript_handler") {
                    elems.extend(r.get("typelem").map(|e| (t, e.clone())));
                }
                let name = r["typname"].clone();
                if let Some(a) = r.get("array_type_oid") {
                    let a = oid(a);
                    let align = if f.align == b'd' as i8 { b'd' as i8 } else { b'i' as i8 };
                    types.insert(a, TypeFacts { len: -1, byval: false, align, typtype: b'b' as i8, collation, elem: t });
                    arrays.insert(t, a);
                    names.insert(a, format!("_{name}"));
                    by_name.insert(format!("_{name}"), a);
                }
                by_name.insert(name.clone(), t);
                names.insert(t, name);
            }
            for (t, e) in elems {
                let e = by_name[&e];
                types.get_mut(&t).expect("the row").elem = e;
            }
            let ty = |n: &str| by_name.get(n).copied().unwrap_or_else(|| panic!("no pg_type.dat row named {n}"));
            let mut procs = BTreeMap::new();
            for r in dat("pg_proc.dat") {
                let args: Vec<Oid> = r.get("proargtypes").map_or_else(Vec::new, |s| s.split_whitespace().map(ty).collect()); // alloc: Cold
                let letter = |k: &str, d: u8| r.get(k).and_then(|v| v.bytes().next()).unwrap_or(d);
                let prolang = match r.get("prolang").map(String::as_str) {
                    None | Some("internal") => 12,
                    Some("c") => 13,
                    Some("sql") => 14,
                    Some(_) => 0,
                };
                let poly = |t: &Oid| names[t] == "record" || names[t] == "internal" || names[t].starts_with("any");
                let facts = FuncFacts {
                    strict: letter("proisstrict", b't') == b't',
                    retset: letter("proretset", b'f') == b't',
                    nargs: args.len() as i16,
                    volatility: Some(letter("provolatile", b'i')),
                    parallel: Some(letter("proparallel", b's')),
                    node_arg: args.iter().any(poly) || r.get("provariadic").is_some_and(|v| v == "any"),
                    leakproof: Some(letter("proleakproof", b'f') == b't'),
                    prolang: Some(prolang),
                    body: 0,
                };
                procs.insert(oid(&r["oid"]), Proc { name: r["proname"].clone(), kind: letter("prokind", b'f'), ret: ty(r["prorettype"].as_str()), args, facts });
            }
            let mut opers = BTreeMap::new();
            for r in dat("pg_operator.dat") {
                let o = oid(&r["oid"]);
                let t = |k: &str| r.get(k).filter(|v| v.as_str() != "0").map_or(0, |n| ty(n.as_str()));
                let code = effects::builtin_oper(o).map_or(0, |p| p.code);
                opers.insert(o, Oper { name: r["oprname"].clone(), left: t("oprleft"), right: t("oprright"), result: t("oprresult"), code });
            }
            // pg_aggregate.dat names its function by signature, `name(type,..)`, or by a name no other pg_proc row has
            let sig = |p: &Proc| format!("{}({})", p.name, p.args.iter().map(|t| names[t].as_str()).collect::<Vec<_>>().join(","));
            let by_sig: HashMap<String, Oid> = procs.iter().map(|(&o, p)| (sig(p), o)).collect();
            let mut aggs = HashMap::new();
            for r in dat("pg_aggregate.dat") {
                let f = &r["aggfnoid"];
                let fnoid = by_sig.get(f.as_str()).copied().or_else(|| {
                    let mut named = procs.iter().filter(|(_, p)| p.kind == b'a' && &p.name == f);
                    named.next().map(|(&o, _)| o).filter(|_| named.next().is_none())
                });
                let fnoid = fnoid.unwrap_or_else(|| panic!("pg_aggregate.dat's {f}: no pg_proc.dat row"));
                let kind = r.get("aggkind").and_then(|k| k.bytes().next()).unwrap_or(b'n') as i8;
                aggs.insert(fnoid, (kind, r.get("aggnumdirectargs").map_or(0, |n| n.parse().expect("aggnumdirectargs"))));
            }
            Dat { procs, types, names, arrays, opers, aggs }
        }

        fn type_name(&self, t: Oid) -> String {
            if t == 0 {
                return "-".into();
            }
            self.names.get(&t).cloned().unwrap_or_else(|| t.to_string())
        }

        fn array_of(&self, t: Oid) -> Option<Oid> {
            self.arrays.get(&t).copied()
        }
    }

    fn no(what: &str) -> Box<PgError> {
        Box::new(PgError::error(format!("agreement catalog: no {what}"))) // alloc: Cold
    }

    impl Catalog<'static> for Dat {
        fn func(&self, f: Oid) -> PgResult<FuncFacts> {
            self.procs.get(&f).map(|p| p.facts).ok_or_else(|| no("function"))
        }
        fn opcode(&self, o: Oid) -> PgResult<Oid> {
            self.opers.get(&o).map(|x| x.code).filter(|&c| c != 0).ok_or_else(|| no("operator"))
        }
        fn op_input_types(&self, o: Oid) -> PgResult<(Oid, Oid)> {
            self.opers.get(&o).map(|x| (x.left, x.right)).ok_or_else(|| no("operator"))
        }
        fn btree_cmp_proc(&self, _: Oid, _: Oid, _: Oid) -> PgResult<Oid> {
            Err(no("btree proc"))
        }
        fn opclass(&self, _: Oid) -> PgResult<(Oid, Oid)> {
            Err(no("opclass"))
        }
        fn type_facts(&self, t: Oid) -> PgResult<TypeFacts> {
            self.types.get(&t).copied().ok_or_else(|| no("type"))
        }
        fn type_cmp_proc(&self, _: Oid) -> PgResult<Oid> {
            Err(no("type compare"))
        }
        fn base_type(&self, t: Oid, _: &mut i32) -> PgResult<Oid> {
            Ok(t)
        }
        fn domain(&self, _: Oid) -> PgResult<DomainFacts<'static>> {
            Err(no("domain"))
        }
        fn io(&self, _: Oid) -> PgResult<IoFacts> {
            Err(no("io"))
        }
        fn text(&self, _: Oid) -> PgResult<TextFacts> {
            Err(no("text"))
        }
        fn tier(&self, f: Oid) -> Tier {
            gravity_fn::tier_of(f)
        }
        fn shim(&self, f: Oid) -> Option<&'static Shim> {
            gravity_fn::shim_of(f)
        }
    }
}

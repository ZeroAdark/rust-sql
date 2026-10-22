//! The shared user-function guard (design REV 2; the user's review-loop
//! change (1), 09-29): ONE predicate and ONE refusal name, `USER_FN`, that a row calls at its NEW admission points (no
//! served path calls it). This half (G1) answers whether a node's keys reach a support function that is not built in: the
//! comparison, equality, hash, in_range and partition-pruning functions PostgreSQL 18.6's executor resolves for the node,
//! found as the executor finds them (through the operator's opfamily, never by the operator's OID: D1371-R6 S2). `Ok`
//! means no user code is reached, not the key's semantics: a row still compares with the resolved FUNCTION 1, collation
//! and NULLS order (D2051). G2 adds `acts()` and `volatile_order()` to this file.
use gravity_decompose::catalog::typefacts::FIRST_NORMAL_OID;
use types_core::catalog::{RECORDARRAYOID, RECORDOID};
use gravity_decompose::ops::shared::agg::{AGG_HASHED, AGG_MIXED, AGG_SORTED};
use gravity_decompose::expr::kids;
use gravity_decompose::catalog::expr_type::type_of;
use gravity_decompose::catalog::IndexAm;
use gravity_decompose::graph::KeyForm;
use gravity_decompose::ops::shared::scan::BitmapPlan;
use gravity_decompose::{IndexKey, Op};
use gravity_expr::catalog::Support;
use gravity_expr::Catalog;
use types_nodes::nodes_enums::LimitOption;
use types_nodes::primnodes::{Aggref, SubscriptingRef, AGGKIND_NORMAL};
use types_nodes::{Node, NodeTag};
use mcx::{CollectIn, GrowIn, PgVec};

use crate::graph::Gx;
use crate::admit::refusal::ShapeWhy;

/// The one raise-free walk and its named readings (`raise_free_by` is its `rtfilter` reading).
pub(crate) mod raise_free;
/// The one USER-FN walk and its named readings.
pub(crate) mod user_fn;

/// The visible half: may the user see a change of row order, of call order or count, or of which error (order_visible,
/// calls_visible, error_visible). This file keeps the primitives they read.
pub mod visible;

/// Fold order: a group's aggregates order-free, its min / max member kept on any walk (moved from graph.rs).
pub mod fold;

/// The one VOLATILE walk and its named readings.
pub(crate) mod volatile;
/// The lowering-end agreement check of the three expression readers (off unless `pgrust.gravity_effects_agree` asks).
pub(crate) mod end_check;

/// C1 (b): node `n`'s effects answers (`gx.fx(n)`) and the home's one producer of them, `Fx::ask`.
pub(crate) mod fx;

/// The effects agreement property test: every decider of the home against its own reader, over every builtin and node kind.
#[cfg(test)]
mod agreement;

/// The guard's one name (D2113, a refusal name): every `Err` is this.
pub const USER_FN: ShapeWhy = ShapeWhy("a newly served shape that would call a support function that is not built in, or move a VOLATILE or user call off PostgreSQL's order or count (D2113)");

/// S4 / D2093: a DISPATCH function's components are read from the type cache at lowering time, and an exact-type DEFAULT
/// opclass created later (for int4[], int4range, an enum, a composite) changes them without retiring the cached program.
/// Until the D2093 row keys the program by the front epoch, every DISPATCH function refuses; GUARD-DISPATCH-OK flips this
/// line (order board line 22).
pub const DISPATCH_OK: bool = false;

/// What PostgreSQL's executor resolves for a key (REV 2 lines 11-16).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyUse {
    /// The ordering family's FUNCTION 2, else FUNCTION 1 (PrepareSortSupportFromOrderingOp): both must be built in.
    Sort,
    /// The merge family's FUNCTION 2, else FUNCTION 1, at the clause's member types (MJExamineQuals).
    Merge,
    /// The operator's `oprcode` (execTuplesMatchPrepare).
    Eq,
    /// Both inputs' hash functions (`get_op_hash_functions`) and the operator's `oprcode` (execTuplesHashPrepare).
    Hash,
    /// The WindowAgg's in_range functions, the pruning steps' cmpfns, a write's partition routing: given in `fns`.
    InRange,
    Prune,
    Route,
}

/// One key as the node holds it (its own arrays, mergeFamilies, hash clauses; never decompose's key images).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    /// The sort, equality or hash operator the node names.
    pub op: u32,
    /// The key's actual input types: a DISPATCH function's components are theirs (0: unknown, refused).
    pub lefttype: u32,
    pub righttype: u32,
    /// Merge: the clause's mergeFamilies entry; 0 elsewhere (the family is resolved from `op`).
    pub family: u32,
    pub coll: u32,
}

/// Whether `keys`, used as `use_`, and `fns` (functions the caller resolved: in_range, cmpfns, routing) call only built-in
/// support functions. A DISPATCH function in `fns` is refused (its components' type is not known here: D1483).
pub fn support(gx: &Gx<'_, '_>, use_: KeyUse, keys: &[Key], fns: &[u32]) -> Result<(), ShapeWhy> {
    verdict(fns_ok(fns) && keys.iter().all(|k| key_ok(gx.catalog, DISPATCH_OK, use_, k)))
}

/// `support` over what `node` itself resolves, the KeyUse taken from the node (S1). A plan node: Sort, IncrementalSort (+ Eq
/// of its presorted keys), a sorted SetOp, MergeAppend and Gather Merge sort; MergeJoin merges; Group, Unique, a sorted
/// Agg (head and every chain Agg, with each chain phase's Sort), WindowAgg and LIMIT WITH TIES compare equal; a hashed Agg
/// or SetOp, HashJoin, Memoize and RecursiveUnion hash; an Agg also answers for each Aggref it runs. An expression: an
/// Aggref (aggorder: Sort, + Eq when ordered-set / hypothetical: mode(), dense_rank(); aggdistinct: Sort + Eq), a hashed
/// SubPlan (nodeSubplan.c:986-997), a hashed ScalarArrayOpExpr. Pruning: a PartitionPruneInfo, PartitionedRelPruneInfo
/// or step (cmpfns). Any other node answers `Ok`, which is no complete check: RowCompareExpr, MinMaxExpr, index order-by
/// operators and quals, amprocs and every function an expression calls are acts()' (G2).
/// The pruning steps of an Append / MergeAppend live in the statement's `partPruneInfos`: pass that node too.
pub fn support_of(gx: &Gx<'_, '_>, node: Node<'_>) -> Result<(), ShapeWhy> {
    support_in(gx.catalog, node)
}

/// `support_of` over catalog `c` alone, for a reader that runs before the graph's `Gx` exists (group's regroup overlay).
pub(crate) fn support_in(c: &dyn Catalog<'_>, node: Node<'_>) -> Result<(), ShapeWhy> {
    verdict(node_ok(c, DISPATCH_OK, node))
}

/// D1899: a cast in `n` to a domain whose CHECK constraints (its base domains' with them) make a VOLATILE call. PG's
/// contain_volatile_functions reads no domain CHECK, so check_hashjoinable takes such a cast into a hash clause, and
/// ExecScanHashBucket runs the clause again for every candidate whose hash value matches: the CHECK runs once more per
/// candidate. Inside a CHECK it fails closed: a call the catalog cannot read, a node the walk cannot see, a domain the
/// catalog cannot read or one met again inside its own checks.
pub(crate) fn volatile_check(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    gx.fx(n).volatile_check()
}

/// Whether domain `t`'s CHECK constraints (its base domains' with them, as the catalog lists them) hold one that `read`
/// reads as true; a domain the catalog cannot read, or one met again inside its own checks (`doms`), counts (fail closed).
/// The one domain step of `volatile_check` (D1899) and `check_acts` (D1907): they differ only in how a CHECK is read.
fn domain_checks<'p, 'm>(gx: &Gx<'_, 'p>, t: u32, doms: &mut PgVec<'m, u32>, read: &mut dyn FnMut(Node<'p>, &mut PgVec<'m, u32>) -> bool) -> bool {
    let Some(d) = gx.catalog.domain(t).ok().filter(|_| !doms.contains(&t)) else { return true };
    doms.push_in(t);
    let v = d.constraints.iter().any(|c| matches!(c, gravity_expr::catalog::DomainConstraint::Check { expr, .. } if read(*expr, doms)));
    doms.pop();
    v
}

/// D1907: a cast in `n` to a domain whose CHECK acts by effects' reader (`effects::of_expr(..).acts`: a VOLATILE call or
/// one the catalog cannot answer, a SubPlan, any node the walk does not know), a nested domain's CHECK with it: the
/// inline forms run the check at every open, while effects reads a domain coercion as one that may raise, not one that
/// acts. `volatile_check`'s walk with that reading of a CHECK (`domain_checks`), failing closed the same way.
pub(crate) fn check_acts<'p>(gx: &Gx<'_, 'p>, cx: &gravity_decompose::effects::Cx<'_, 'p>, n: Node<'p>) -> bool {
    gx.fx(n).check_acts(cx)
}

fn verdict(ok: bool) -> Result<(), ShapeWhy> {
    if ok {
        Ok(())
    } else {
        Err(USER_FN)
    }
}

/// Functions resolved by the caller (0: none): built in and no DISPATCH function.
fn fns_ok(fns: &[u32]) -> bool {
    fns.iter().all(|&f| builtin_fn(f))
}

/// One function resolved by the caller (0: none) is built in and no DISPATCH function (drain's run-time pruning steps ask
/// it for their support functions: overfit OIDW-29).
pub(crate) fn builtin_fn(f: u32) -> bool {
    f == 0 || f < FIRST_NORMAL_OID && dispatch(f).is_none()
}

/// Whether built-in function `f` may emit a NOTICE or WARNING per call though it is not VOLATILE (a VOLATILE call acts by
/// its class; a user function by its OID): the one table's `types_core::quiet::NOTICE`.
pub(crate) fn may_notice(f: u32) -> bool {
    types_core::quiet::notice(f)
}

fn key_ok(c: &dyn Catalog<'_>, d: bool, use_: KeyUse, k: &Key) -> bool {
    let sorted = |q: &dyn Fn(i16) -> Support| match (c.support(q(2)), c.support(q(1))) {
        (Ok(f2), Ok(f1)) => (f1 != 0 || f2 != 0) && [f1, f2].iter().all(|&f| f == 0 || built_in(c, d, f, k.lefttype, 0)),
        _ => false,
    };
    let eq = || c.opcode(k.op).is_ok_and(|f| built_in(c, d, f, k.lefttype, 0));
    match use_ {
        KeyUse::Sort => sorted(&|num| Support::Sort { op: k.op, num }),
        KeyUse::Merge => sorted(&|num| Support::Merge { op: k.op, family: k.family, num }),
        KeyUse::Eq => eq(),
        KeyUse::Hash => eq() && [(false, k.lefttype), (true, k.righttype)].iter().all(|&(rhs, t)| c.support(Support::Hash { op: k.op, rhs }).is_ok_and(|f| built_in(c, d, f, t, 0))),
        KeyUse::InRange | KeyUse::Prune | KeyUse::Route => true,
    }
}

/// `f` is built in when it is a catalog function below FIRST_NORMAL_OID (typefacts.rs:69) that is no DISPATCH function,
/// or (once `d`, DISPATCH_OK) a DISPATCH function whose every component over type `t` is built in.
fn built_in(c: &dyn Catalog<'_>, d: bool, f: u32, t: u32, depth: u8) -> bool {
    crate::stack_check();
    f != 0 && f < FIRST_NORMAL_OID && dispatch(f).is_none_or(|r| d && depth < 8 && reach(c, r, t, depth + 1))
}

// PG 18.6's built-in functions that reach a component type's function through the type cache, or a type or query named
// at run time: the one table's `types_core::quiet::DISPATCH`.
use types_core::quiet::{dispatch, Reach, Via};
use Reach::{Elem, Field, Io, Named, Range};
use Via::{Cmp, Eq as E, Hash as H};

/// Every component DISPATCH function `r` calls over actual type `t` (a domain's base) is built in.
fn reach(c: &dyn Catalog<'_>, r: Reach, t: u32, depth: u8) -> bool {
    let q = |s: Support| c.support(s).ok().filter(|&x| x != 0);
    let fun = |v: Via, ct: u32| match v {
        Cmp => c.type_cmp_proc(ct).ok().filter(|&x| x != 0),
        E => c.type_eq_opr(ct).ok().filter(|&x| x != 0).and_then(|o| c.opcode(o).ok()),
        H(ext) => q(Support::TypeHash { typid: ct, ext }),
    };
    let comp = |v: Via, ct: u32| ct != 0 && fun(v, ct).is_some_and(|f| built_in(c, true, f, ct, depth));
    let Ok(t) = c.base_type(t, &mut -1) else { return false };
    match r {
        Elem(v) => c.type_facts(t).is_ok_and(|x| comp(v, x.elem)),
        Field(_) if matches!(t, 0 | RECORDOID | RECORDARRAYOID) => false,
        Field(v) => q(Support::Natts(t)).is_some_and(|n| (1..=n.min(1600) as i16).all(|a| c.support(Support::Field { typid: t, attno: a }).is_ok_and(|ft| ft == 0 || comp(v, ft)))),
        Range(v, multi) => {
            let Some(r) = (if multi { q(Support::MultirangeRange(t)) } else { Some(t) }) else { return false };
            let sub = q(Support::RangeSub(r)).unwrap_or(0);
            match v {
                H(_) => comp(v, sub),
                _ => q(Support::RangeCmp(r)).is_some_and(|f| built_in(c, true, f, sub, depth)) && c.support(Support::RangeCanonical(r)).is_ok_and(|f| f == 0 || built_in(c, true, f, r, depth)),
            }
        }
        Io | Named => false,
    }
}

/// D2065: whether `n` calls a function or operator over a range or multirange value whose type's comparison function (its
/// rngsubopc's FUNCTION 1) or canonical function is not built in. PostgreSQL's range functions compare bounds and elements
/// through the type cache (rangetypes.c range_cmp_bounds: `rng_cmp_proc_finfo`), so `r @> x` over such a range runs that
/// user function, VOLATILE or not, at each call: a call the side classes, which read a call's own function, do not see.
/// A user type the catalog cannot answer for counts; a built-in type is not asked.
pub fn range_user_cmp(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    gx.fx(n).range_user_cmp()
}

/// D2115 x D1911: whether pipeline `k` computes an aggregate argument's SubPlan that acts or may raise (decompose
/// `SubPlanDesc::effects`) in a statement with a Limit whose count may be zero (`SubPlanDesc::stops`). At a zero count PG
/// runs none of it (ExecLimit never pulls its subplan), so a strategy that runs such a SubPlan ahead of the Limit's check
/// makes a call or an error the user sees (result/gate.rs `SPOOL_ZERO` asks it for a grouping-sets spool walk).
pub(crate) fn arg_subplan_loud(gx: &Gx<'_, '_>, k: usize) -> bool {
    use gravity_decompose::ops::shared::subplan::SubSite;
    gx.pipelines()[k].ops.iter().any(|o| matches!(o, Op::SubPlan(d) if d.stops && d.site == SubSite::AggArg && (d.effects().acts || d.effects().may_raise)))
}

/// The type of column `idx` (1-based) of the input `src` reads: its outer child's target list, else its own.
fn col_type(src: Node<'_>, idx: i16) -> u32 {
    let at = |n: Option<Node<'_>>| n.and_then(|n| n.as_plan()).and_then(|p| usize::try_from(idx).ok()?.checked_sub(1).filter(|&i| i < p.targetlist.len()).map(|i| p.targetlist.nth(i))).and_then(type_of);
    let p = src.as_plan();
    at(p.and_then(|p| p.lefttree)).or_else(|| at(Some(src))).unwrap_or(0)
}

fn node_ok(c: &dyn Catalog<'_>, d: bool, n: Node<'_>) -> bool {
    crate::stack_check();
    // each key of parallel arrays, its type from `src`'s input
    let arr = |u: KeyUse, src: Node<'_>, ops: &[u32], cols: &[i16], colls: &[u32]| {
        ops.iter().enumerate().all(|(i, &op)| {
            let t = cols.get(i).map_or(0, |&a| col_type(src, a));
            key_ok(c, d, u, &Key { op, lefttype: t, righttype: t, family: 0, coll: colls.get(i).copied().unwrap_or(0) })
        })
    };
    // an OpExpr clause's key: its operator and its two arguments' types
    let clause = |x: Node<'_>, op: u32, family: u32, coll: u32| {
        x.as_op_expr().filter(|o| o.args.len() == 2 && (op == 0 || o.opno == op)).map(|o| Key { op: o.opno, lefttype: type_of(o.args.nth(0)).unwrap_or(0), righttype: type_of(o.args.nth(1)).unwrap_or(0), family, coll })
    };
    match n.node_tag() {
        NodeTag::T_Sort => n.as_sort().is_some_and(|s| arr(KeyUse::Sort, n, s.sortOperators, s.sortColIdx, s.collations)),
        NodeTag::T_IncrementalSort => n.as_incremental_sort().is_some_and(|x| {
            let s = &x.sort;
            let pre = usize::try_from(x.nPresortedCols).unwrap_or(0).min(s.sortOperators.len());
            let eqs = s.sortOperators[..pre].iter().enumerate().all(|(i, &op)| {
                let t = s.sortColIdx.get(i).map_or(0, |&a| col_type(n, a));
                c.support(Support::EqOfSort(op)).is_ok_and(|e| e != 0 && key_ok(c, d, KeyUse::Eq, &Key { op: e, lefttype: t, righttype: t, family: 0, coll: 0 }))
            });
            arr(KeyUse::Sort, n, s.sortOperators, s.sortColIdx, s.collations) && eqs
        }),
        // PG 18.6 nodeSetOp.c:637-660: SETOP_HASHED (1) hashes by its equality operators, SETOP_SORTED sorts by its
        // ordering operators (PrepareSortSupportFromOrderingOp: the family's FUNCTION 2 / 1, not oprcode)
        NodeTag::T_SetOp => n.as_set_op().is_some_and(|s| arr(if s.strategy == 1 { KeyUse::Hash } else { KeyUse::Sort }, n, s.cmpOperators, s.cmpColIdx, s.cmpCollations)),
        NodeTag::T_MergeAppend => n.as_merge_append().is_some_and(|s| arr(KeyUse::Sort, n, s.sortOperators, s.sortColIdx, s.collations)),
        NodeTag::T_GatherMerge => n.as_gather_merge().is_some_and(|s| arr(KeyUse::Sort, n, s.sortOperators, s.sortColIdx, s.collations)),
        NodeTag::T_Group => n.as_group().is_some_and(|s| arr(KeyUse::Eq, n, s.grpOperators, s.grpColIdx, s.grpCollations)),
        NodeTag::T_Unique => n.as_unique().is_some_and(|s| arr(KeyUse::Eq, n, s.uniqOperators, s.uniqColIdx, s.uniqCollations)),
        NodeTag::T_Agg => {
            // the head and each chain Agg by strategy (their keys read the head's input), each chain phase's Sort
            // (nodeAgg.c ExecInitAgg: the Sort node's arrays), then every Aggref the Agg runs
            let one = |x: Node<'_>, chained: bool| {
                x.as_agg().is_some_and(|a| {
                    let uses: &[KeyUse] = match a.aggstrategy {
                        AGG_SORTED => &[KeyUse::Eq],
                        AGG_HASHED => &[KeyUse::Hash],
                        AGG_MIXED => &[KeyUse::Eq, KeyUse::Hash],
                        _ => &[],
                    };
                    let sort = x.as_plan().and_then(|p| p.lefttree).filter(|_| chained).and_then(|s| s.as_sort());
                    uses.iter().all(|&u| arr(u, n, a.grpOperators, a.grpColIdx, a.grpCollations)) && sort.is_none_or(|s| arr(KeyUse::Sort, n, s.sortOperators, s.sortColIdx, s.collations))
                })
            };
            let mut ok = n.as_agg().is_some_and(|a| one(n, false) && a.chain.iter().all(|x| one(x, true)));
            for x in n.as_plan().into_iter().flat_map(|p| p.targetlist.iter().chain(p.qual.iter())) {
                let mut each = true;
                let known = each_aggref(x, &mut |r| each &= node_ok(c, d, r));
                ok &= known && each;
            }
            ok
        }
        NodeTag::T_WindowAgg => n.as_window_agg().is_some_and(|w| {
            arr(KeyUse::Eq, n, w.partOperators, w.partColIdx, w.partCollations) && arr(KeyUse::Eq, n, w.ordOperators, w.ordColIdx, w.ordCollations) && fns_ok(&[w.startInRangeFunc, w.endInRangeFunc])
        }),
        NodeTag::T_Limit => n.as_limit().is_some_and(|l| l.limitOption != LimitOption::LIMIT_OPTION_WITH_TIES || arr(KeyUse::Eq, n, l.uniqOperators, l.uniqColIdx, l.uniqCollations)),
        NodeTag::T_MergeJoin => n.as_merge_join().is_some_and(|j| {
            j.mergeclauses.len() == j.mergeFamilies.len()
                && j.mergeclauses.iter().zip(j.mergeFamilies).enumerate().all(|(i, (x, &fam))| clause(x, 0, fam, j.mergeCollations.get(i).copied().unwrap_or(0)).is_some_and(|k| key_ok(c, d, KeyUse::Merge, &k)))
        }),
        NodeTag::T_HashJoin => n.as_hash_join().is_some_and(|j| {
            j.hashclauses.len() == j.hashoperators.len() && j.hashclauses.iter().zip(j.hashoperators.iter()).all(|(x, op)| clause(x, op, 0, 0).is_some_and(|k| key_ok(c, d, KeyUse::Hash, &k)))
        }),
        NodeTag::T_Memoize => n.as_memoize().is_some_and(|m| {
            m.hashOperators.len() == m.param_exprs.len()
                && m.hashOperators.iter().zip(m.param_exprs.iter()).all(|(&op, e)| {
                    let t = type_of(e).unwrap_or(0);
                    key_ok(c, d, KeyUse::Hash, &Key { op, lefttype: t, righttype: t, family: 0, coll: 0 })
                })
        }),
        NodeTag::T_RecursiveUnion => n.as_recursive_union().is_some_and(|r| arr(KeyUse::Hash, n, r.dupOperators, r.dupColIdx, r.dupCollations)),
        // nodeSubplan.c:986-997: per OpExpr of the test, its opfuncid, the hash functions of its operator and the
        // right-hand type's `=` (`get_compatible_hash_operators`)
        NodeTag::T_SubPlan => n.as_sub_plan().is_some_and(|s| {
            let op_ok = |x: Node<'_>| {
                clause(x, 0, 0, 0).is_some_and(|k| {
                    let rhs = c.support(Support::HashRhsEq(k.op)).unwrap_or(0);
                    fns_ok(&[x.as_op_expr().map_or(0, |o| o.opfuncid)]) && key_ok(c, d, KeyUse::Hash, &k) && rhs != 0 && key_ok(c, d, KeyUse::Eq, &Key { op: rhs, lefttype: k.righttype, ..k })
                })
            };
            let and = s.testexpr.and_then(|t| t.as_bool_expr()).filter(|b| b.boolop == types_nodes::primnodes::BoolExprType::AND_EXPR);
            !s.useHashTable || s.testexpr.is_some_and(|t| and.map_or_else(|| op_ok(t), |b| b.args.iter().all(op_ok)))
        }),
        NodeTag::T_ScalarArrayOpExpr => n.as_scalar_array_op_expr().is_some_and(|s| s.hashfuncid == 0 || fns_ok(&[s.hashfuncid, s.opfuncid, s.negfuncid])),
        NodeTag::T_Aggref => n.as_aggref().is_some_and(|r| {
            let t = |i: u32| r.args.iter().filter_map(|x| x.as_target_entry()).find(|x| x.ressortgroupref == i).and_then(|x| type_of(x.expr)).unwrap_or(0);
            let by = |x: Node<'_>, eq: bool| {
                x.as_sort_group_clause().is_some_and(|s| {
                    let k = Key { op: s.sortop, lefttype: t(s.tleSortGroupRef), righttype: t(s.tleSortGroupRef), family: 0, coll: r.inputcollid };
                    key_ok(c, d, KeyUse::Sort, &k) && (!eq || key_ok(c, d, KeyUse::Eq, &Key { op: s.eqop, ..k }))
                })
            };
            r.aggorder.iter().all(|x| by(x, r.aggkind != AGGKIND_NORMAL)) && r.aggdistinct.iter().all(|x| by(x, true))
        }),
        NodeTag::T_PartitionPruneStepOp => n.as_partition_prune_step_op().is_some_and(|s| fns_ok(s.cmpfns.as_slice())),
        NodeTag::T_PartitionedRelPruneInfo => n.as_partitioned_rel_prune_info().is_some_and(|r| r.initial_pruning_steps.iter().chain(r.exec_pruning_steps.iter()).all(|s| node_ok(c, d, s))),
        NodeTag::T_PartitionPruneInfo => n.as_partition_prune_info().is_some_and(|p| p.prune_infos.iter().all(|l| l.as_list().is_some_and(|l| l.iter().all(|r| node_ok(c, d, r))))),
        _ => true,
    }
}

/// Calls `f` on each Aggref under `n` (not into one: its arguments hold none it runs), a JsonValueExpr's `raw_expr`
/// included (decompose expr.rs `run_aggrefs`); false when some node on the way is one `kids` does not know (refused).
fn each_aggref<'p>(n: Node<'p>, f: &mut dyn FnMut(Node<'p>)) -> bool {
    crate::stack_check();
    if n.as_aggref().is_some() {
        f(n);
        return true;
    }
    let raw = n.as_json_value_expr().filter(|j| j.formatted_expr.is_some()).and_then(|j| j.raw_expr);
    let mut ok = raw.is_none_or(|r| each_aggref(r, f));
    let known = kids(n, &mut |k| ok &= each_aggref(k, f));
    ok && known
}

// Moved from ops/hashjoin (build_class.rs, strategies/rtfilter.rs) unchanged, v5 hashjoin landing 2: the may-raise /
// may-call predicates their callers in agg, result, scan, sort and table share. Landing 8 replaces them with `of_ops`.

/// D1005: a VOLATILE call anywhere in `n`, the fail-closed union of the VOLATILE readings (T2): a call D1871's nodes make
/// (`calls`: an operator's function counts in every node that calls one) whose function pg_proc marks VOLATILE or cannot
/// answer, a cast to a domain whose CHECKs make one (D1899), or a range function over a range whose comparison is user
/// code (D2065): `volatile::Reading::any` and `range_user_cmp`.
pub(crate) fn volatile(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    gx.fx(n).volatile_any()
}

/// USEROP (ops/shared/one_batch.rs): a run-time refusal of this statement falls through to the row executor (exec runner
/// `falls_at_run`: a SELECT with no row lock and no VOLATILE call in any program). decompose's acting set cannot say when
/// a sort is keyed by a SQL comparator: it reads that comparator as unknown code. Here every pipeline reads a scan, a
/// breaker or a constant and runs only filters, projections, limits, sorts and hash probes and builds over expressions
/// with no VOLATILE call (`volatile`), every aggregate is built in over such arguments, and there is no embedded inner.
/// Anything else fails closed.
pub(crate) fn falls_at_run(gx: &Gx<'_, '_>) -> bool {
    use gravity_decompose::join::JoinDesc;
    use gravity_decompose::Source;
    let v = |n: Node<'_>| volatile(gx, n);
    let l = |x: &types_nodes::list::NodeList<'_>| x.iter().any(v);
    let ps = gx.pipelines();
    let op = |o: &Op<'_>| match o {
        Op::Filter(n) | Op::Gate(n) => !v(*n),
        Op::Project(x) => !l(x),
        o if crate::frame::iface::holds_limit(core::slice::from_ref(o)) => true,
        Op::Sort(s) => !l(s.tlist),
        Op::Join(JoinDesc::Probe { keys, clauses, qual, .. }) => !l(keys) && !l(clauses) && !l(qual),
        Op::Join(JoinDesc::Build { keys, .. }) => !l(keys),
        _ => false,
    };
    let groups = gx.graph.groups.iter().chain(gx.graph.streams.iter());
    ps.inners().is_empty()
        && (0..ps.len()).all(|q| matches!(ps[q].source, Source::Range { .. } | Source::Breaker { .. } | Source::Const) && ps[q].ops.iter().all(op))
        && groups.flat_map(|g| g.aggs.iter()).all(|a| a.fnoid < FIRST_NORMAL_OID && !v(a.node) && !a.filter.is_some_and(v))
}

/// Function `func` is IMMUTABLE (`pg_proc.provolatile` 'i'), so one verdict per distinct argument stands for every row's
/// call (a hash join's hoisted call, ops/hashjoin/strategies/hoist.rs: the verdict kept in place of its code). A function
/// the catalog cannot answer is not.
pub(crate) fn immutable_call(gx: &Gx<'_, '_>, func: u32) -> bool {
    gx.catalog.func(func).is_ok_and(|f| f.volatility == Some(b'i'))
}

/// D1871: the functions node `n` itself calls (0: none): a FuncExpr's; an OpExpr's, a DistinctExpr's and a NullIfExpr's
/// operator function; a ScalarArrayOpExpr's operator function and its hashed form's hash and negator functions
/// (contain_volatile_functions' check_functions_in_node reads all of them). D3921: a NextValueExpr (an identity column's
/// default) reads as `nextval(regclass)`, whose draw it makes per evaluation (nextval_internal), as PostgreSQL 18.6's
/// contain_volatile_functions_walker counts it ("NextValueExpr is volatile").
fn calls(n: Node<'_>) -> [Option<u32>; 3] {
    if let Some(o) = n.as_scalar_array_op_expr() {
        return [Some(o.opfuncid), Some(o.hashfuncid).filter(|&f| f != 0), Some(o.negfuncid).filter(|&f| f != 0)];
    }
    let f = n.as_func_expr().map(|f| f.funcid)
        .or_else(|| n.as_op_expr().map(|o| o.opfuncid))
        .or_else(|| n.as_distinct_expr().map(|o| o.opfuncid))
        .or_else(|| n.as_null_if_expr().map(|o| o.opfuncid))
        .or_else(|| (n.node_tag() == NodeTag::T_NextValueExpr).then_some(gravity_decompose::catalog::casts::NEXTVAL));
    [f, None, None]
}

/// W4b-RowX (code review rounds 2 and 3): whether `n` (a list: each member, as a gate's implicit AND) holds a value of a
/// type whose support procs reach user code (a builtin over it runs the type's output, comparison and hash procs, and v4's
/// hash keys compare by type: D2065 / D2068): a type past FirstNormalObjectId that `type_free` does not read free (AGC-17:
/// an enum, a domain over a free type, an array of one and a composite of them run initdb's enum_*, the base's,
/// array_* and record_* procs over their members' own; a user base or range type, a pseudo-type member and a runaway
/// nesting reach user code, as does a type the effects facts hold no row for (fail closed, before any catalog read); a
/// domain's CHECK runs only where a CoerceToDomain does, its own reader), a node the walk cannot
/// see, or a builtin that runs a query of its own (`effects::runs_own_query`, STABLE: its own query runs what the
/// relations it names call, a view's nextval: table_, schema_, database_to_xml[_and_xmlschema]).
/// An aggregate's ORDER BY and DISTINCT clauses hold no value (the values they sort are its argument entries, walked): a
/// SortGroupClause reads as its sort and equality operators, builtins or a user's code (D1902: an ordered aggregate over a
/// rerun set's join, `string_agg(.. ORDER BY ..)`, no longer makes the statement loud).
pub(crate) fn user_typed(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    gx.fx(n).user_typed()
}

/// D2107: nest `j`'s embedded inner (`NestFacts`: F5 over the inner plan, D1336) or its join qual makes a VOLATILE call,
/// or its qual calls a user function: the inner runs per outer row where the nest does, so a side holding the nest calls
/// as a filter's does (hashjoin build_class.rs `own_side_by`).
pub(crate) fn nest_calls(gx: &Gx<'_, '_>, j: &gravity_decompose::join::JoinDesc<'_>) -> bool {
    use gravity_decompose::effects::Volatility;
    matches!(j, gravity_decompose::join::JoinDesc::Nest { facts, qual, .. } if facts.volatility == Volatility::Volatile || qual.iter().any(|n| user_or_volatile(gx, n)))
}

/// D3261 (rerun/union_reads.rs `folds` under `every_read`): `n` is an Aggref of a builtin plain aggregate (PostgreSQL's
/// own, below FIRST_NORMAL_OID; no ordered-set or hypothetical-set kind), so its transition and final functions are
/// initdb's: over arguments of builtin types they call no user code, and a numeric sum's overflow is all they may show.
pub(crate) fn builtin_plain_agg(n: Node<'_>) -> bool {
    n.as_aggref().is_some_and(|a| a.aggkind == AGGKIND_NORMAL && a.aggfnoid < FIRST_NORMAL_OID)
}

/// A non-builtin function or operator anywhere in `n` (as far as the walker sees; D1871: every operator node, `calls`).
pub(crate) fn user_fn(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    gx.fx(n).user_calls()
}

/// A non-builtin or VOLATILE function or operator anywhere in `n` (`user_fn`, then `volatile`): a call PostgreSQL makes
/// only on the rows it reaches, so a step holding one runs nowhere PG's fetches do not.
pub(crate) fn user_or_volatile(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    gx.fx(n).user_or_volatile()
}

/// Whether `n` raises nothing on any input (D237: a row PG would not have evaluated must not raise here): the raise-free
/// walk's shared leaf set (raise_free.rs `leaf`: columns, constants, extern parameters (a bound word, as the planner's
/// quiet_qual takes a Param: D598), relabels, AND / OR / NOT, IS [NOT] NULL / TRUE, CASE, COALESCE, ROW, COLLATE, a row
/// conversion, GREATEST / LEAST over a pure compare, a start-time SQLValueFunction) and the effects home's call rule: a
/// call `quiet_call` passes (a leakproof builtin, the numeric comparisons, the integer bitwise operators, textcat, jsonb's
/// field / element accessors and `?`: `types_core::quiet::call`; a scalar-array op by its operator, the array a kid), an
/// integer modulus or division by a constant it cannot raise on and the other `quiet_args` classes (the divisor's class
/// keyed by the effects home's `divisor_class`), a constant pattern match `quiet_pattern` passes; an aggregate the fold
/// form takes (raise_free.rs `Agg::Fold`: the home's `agg_quiet` with no ORDER BY, DISTINCT or direct argument).
pub(crate) fn raise_free(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    gx.fx(n).quiet()
}

/// D1902: `raise_free` with the PARAM_EXEC ids `quiet` names read as bound words (a rerun set owner's, set before it runs),
/// and under `arr` (W4b-RowX, `join_build_class::side_by`'s reading only) PG 18.6's quiet array reads: array_lower /
/// array_upper (NULL out of range), `quiet_fetch`, and a no-op array coercion (int2vector::int2[]: an element
/// CaseTestExpr, copied as is).
pub(crate) fn raise_free_by(gx: &Gx<'_, '_>, n: Node<'_>, quiet: &dyn Fn(i32) -> bool, arr: bool) -> bool {
    gx.fx(n).quiet_by(quiet, arr)
}

/// D2849 (decompose join.rs `KeyRaise`): `n` is a hash join key whose hash PostgreSQL raises on (`raising_keys`): its own
/// value raises nothing, but every reader that asks whether the key raises asks for the join's hashing of it.
pub(crate) fn raising_key(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    gx.graph.raising_keys.iter().any(|k| k.ptr_eq(n))
}

/// D2849: pipeline `k` hashes a key whose hash raises 42P22 (`raising_key`, a Build's or a Probe's). That error is no data
/// error, so no licence to run ahead unchecked covers it (DECISIONS 4 (D) b): `Px::ahead_unchecked` denies it.
pub(crate) fn hashes_raising_key(gx: &Gx<'_, '_>, k: usize) -> bool {
    use gravity_decompose::join::JoinDesc;
    !gx.graph.raising_keys.is_empty() && gx.pipelines()[k].ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Build { keys, .. } | JoinDesc::Probe { keys, .. }) if keys.iter().any(|n| raising_key(gx, n))))
}

/// Main 10-03 22:21Z (per expression): `raise_free_by`'s PG-quiet reading (`arr`) for one expression, a list member by
/// member (a gate's implicit AND). An expression can raise because of a user type only when it calls that type's
/// functions or casts or compares through it, so one that holds a value of a user type (`user_typed`: a row-reading xml
/// builtin too, or a node the walk cannot see) reads as trunk (`raise_free_by` without `arr`), and any other reads PG
/// 18.6's quiet array reads whatever the statement holds elsewhere (a generate_series over int4 arguments next to a user
/// range in another expression reads quiet).
pub(crate) fn raise_free_each(gx: &Gx<'_, '_>, n: Node<'_>, quiet: &dyn Fn(i32) -> bool) -> bool {
    gx.fx(n).quiet_each(quiet)
}

/// W4b-RowX: a true array's element fetch (`type_facts.elem` is get_element_type: IsTrueArrayType, array_subscript_handler;
/// a NULL or out-of-range subscript reads NULL), never a slice, an assignment or another handler's (jsonb's, point's raw).
fn quiet_fetch(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    let fetch = |r: &SubscriptingRef<'_>| r.refassgnexpr.is_none() && r.reflowerindexpr.is_nil() && r.refelemtype != 0;
    n.as_subscripting_ref().is_some_and(|r| fetch(r) && gx.catalog.type_facts(r.refcontainertype).is_ok_and(|t| t.elem == r.refelemtype))
}

/// W4b-RowX (`join_build_class::side_by` only, over an initdb index: its opclass's order procs are PG's own): an index key
/// a btree scan reads without raising: `column op value` under a leakproof builtin operator, or IS [NOT] NULL, quiet value
/// (`raise_free_each`: per expression).
pub(crate) fn quiet_key(gx: &Gx<'_, '_>, k: &IndexKey<'_>, quiet: &dyn Fn(i32) -> bool) -> bool {
    let op = || gx.catalog.opcode(k.opno).is_ok_and(|f| gravity_decompose::effects::leakproof(gx.facts, f));
    (matches!(k.form, KeyForm::Null | KeyForm::NotNull) || k.form == KeyForm::Op && op()) && gx.fx(k.value).quiet_each(quiet)
}

/// V4R-147: a DISTINCT ON key's equality (the Unique's `uniqOperators`, decompose's `Op::FirstOfRun`), which PG's Unique
/// calls on every row it reads: a builtin operator whose function PG marks leakproof raises on no input.
pub(crate) fn eq_raise_free(gx: &Gx<'_, '_>, opno: u32) -> bool {
    gx.catalog.opcode(opno).is_ok_and(|f| gravity_decompose::effects::leakproof(gx.facts, f))
}

/// errorhold (D1904): sort `sd` raises on no input: every key orders by an initdb operator whose function raises on no
/// input (`eq_raise_free`: its default btree family's support is PG's own; a key of a non-default family or with no order
/// proc, which raises at the Sort's first call, W8-EQUIVCLASS-SORTOP, does not pass), its target list is quiet (`rf`), and
/// its bound, if any, is a constant (a LIMIT expression may raise). Asked by join/build_class.rs `own_side_by` under its
/// PG-quiet reading only.
pub(crate) fn sort_quiet(gx: &Gx<'_, '_>, sd: &gravity_decompose::ops::shared::sort::SortDesc<'_>, rf: &dyn Fn(Node<'_>) -> bool) -> bool {
    use gravity_decompose::graph::Bound;
    use gravity_decompose::ops::shared::sort::SortForm;
    let bound = |b: &Bound<'_>| !matches!(b, Bound::Expr(_));
    let form = match &sd.form {
        SortForm::Runs => true,
        SortForm::TopK { count, offset, .. } | SortForm::Walk { count, offset, .. } => bound(count) && bound(offset),
    };
    form && sd.tlist.iter().all(rf) && sort_keys_quiet(gx, &sd.keys)
}

/// `sort_quiet`'s key test: every key orders by an initdb operator whose function raises on no input, in its default btree
/// family, with an order proc (a MergeAppend's merge keys read so too, the tracer's stamp: visible hashjoin.rs `stamp_side`).
pub(crate) fn sort_keys_quiet(gx: &Gx<'_, '_>, keys: &[gravity_decompose::ops::shared::sort::SortKey]) -> bool {
    keys.iter().all(|k| k.opno < FIRST_NORMAL_OID && !k.own_family && k.no_proc.is_none() && eq_raise_free(gx, k.opno))
}

/// errorhold (D1904): merge join `j` raises on no input: its families are initdb ones (their btree support is PG's own) and
/// its merge clauses and join qual are quiet (`rf`, which reads a value of a user type as trunk does).
pub(crate) fn merge_quiet(j: &gravity_decompose::join::JoinDesc<'_>, rf: &dyn Fn(Node<'_>) -> bool) -> bool {
    let gravity_decompose::join::JoinDesc::Merge { clauses, qual, families, .. } = j else { return false };
    families.iter().all(|&f| f < FIRST_NORMAL_OID) && clauses.iter().chain(qual.iter()).all(rf)
}

/// D2063 (V4R-578): a scan of index `indexid` keyed by `keys` reaches only initdb functions that cannot raise: a system
/// index, or every key column's operator class and each key operator an initdb one whose function raises on no input
/// (decompose's `effects::of_index`; facts unknown: fail closed). Its keys' values are the reader's (`quiet_key`). D3831:
/// an `IS [NOT] NULL` key (a min / max InitPlan's `col IS NOT NULL`) names no operator (its opno is 0): the AM tests the
/// key's null flag and calls nothing, so it adds no call (as decompose's `of_source` and merge.rs `builtin_ix`).
pub(crate) fn initdb_index(gx: &Gx<'_, '_>, indexid: u32, keys: &[IndexKey<'_>]) -> bool {
    indexid < FIRST_NORMAL_OID || {
        let e = gravity_decompose::effects::of_index(gx.facts, indexid, keys.iter().filter(|k| !matches!(k.form, KeyForm::Null | KeyForm::NotNull)).map(|k| k.opno));
        !e.acts && !e.may_raise
    }
}

/// `f` holds for every index of bitmap plan `p` (its id, its keys, its access method).
pub(crate) fn bitmap_all(p: &BitmapPlan<'_>, f: &dyn Fn(u32, &[IndexKey<'_>], IndexAm) -> bool) -> bool {
    crate::stack_check();
    match p {
        BitmapPlan::Index { indexid, keys, iam, .. } => f(*indexid, keys, *iam),
        BitmapPlan::And { plans, .. } | BitmapPlan::Or { plans, .. } => plans.iter().all(|x| bitmap_all(x, f)),
    }
}


/// `op` is a set function whose functions neither act nor may raise (decompose `effects::of_call`; builtin column types),
/// over arguments `raise_free_each` reads quietly with the PARAM_EXEC ids `bound` names (a no-op array coercion too:
/// int2vector::int2[]), its target list too: an unnest of a nest parameter's array (rerun/inner_probe.rs's nest-set build
/// side, regress create_table_like#464). In a FROM clause or a target list (a ProjectSet: D1018, create_function_sql#92's
/// information_schema._pg_expandarray, `quiet::sql_body`), one step either way, read by its raises only.
pub(crate) fn srf_quiet(gx: &Gx<'_, '_>, op: &Op<'_>, bound: &dyn Fn(i32) -> bool) -> bool {
    let Op::Srf(d) = op else { return false };
    let call = |f: u32| {
        let e = gravity_decompose::effects::of_call(gx.facts, f);
        !e.acts && !e.may_raise
    };
    d.functions.iter().all(|f| call(f.funcid) && f.coltypes.iter().all(|t| t < FIRST_NORMAL_OID) && f.args.iter().all(|a| gx.fx(a).quiet_each(bound)))
        && d.tlist.iter().all(|n| srf_call(d, n) || gx.fx(n).quiet_each(bound))
}

/// Target-list entry `n` of set function step `d` is one of its own calls (a ProjectSet lists them among its entries; the
/// call and its arguments are `srf_quiet`'s `call` and argument terms).
pub(crate) fn srf_call(d: &gravity_decompose::ops::shared::srf::SrfDesc<'_>, n: Node<'_>) -> bool {
    n.as_target_entry().map_or(n, |t| t.expr).as_func_expr().is_some_and(|f| f.funcretset && d.functions.iter().any(|g| g.funcid == f.funcid))
}

/// Every op of `ops` is a filter, gate or projection that `raise_free` passes (`op_raise_free` with no tail reading).
pub(crate) fn ops_raise_free(gx: &Gx<'_, '_>, ops: &[Op<'_>]) -> bool {
    ops.iter().all(|op| op_raise_free(gx, op, None))
}

/// D3148 (join.domain, hashjoin strategies/domain.rs): whether group `g`'s keyed hash end may leave unfolded the rows of a
/// group that probe `j` of pipeline `q` (the group's emit) drops. Such a group is never finalised nor emitted, so no op of
/// `q` before `j` may raise (its HAVING and projection), nor any aggregate's transition or final (`agg_quiet`, a builtin
/// plain aggregate), nor what a skipped row would evaluate before its fold: the arguments and the FILTER. The group keys
/// are still imaged for every row, before the admission, as without it.
pub(crate) fn domain_cut_free(gx: &Gx<'_, '_>, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>, q: usize, j: usize) -> bool {
    let pre = &gx.pipelines()[q].ops[..j];
    let agg = |a: &gravity_decompose::ops::shared::agg::AggDecl<'_>| {
        a.node.as_aggref().is_some_and(|r| gravity_decompose::effects::agg_quiet(r) && r.args.iter().all(|n| raise_free(gx, n)) && r.aggfilter.is_none_or(|f| raise_free(gx, f)))
    };
    pre.iter().all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_))) && ops_raise_free(gx, pre) && g.aggs.iter().all(agg) && g.aggs.iter().all(|a| a.filter.is_none_or(|f| raise_free(gx, f)))
}

/// Op `o` raises on no row. A filter, gate or projection whose expressions `raise_free` passes, under either reading.
/// `tail` is drain's TAILSTOP reading of a tail op (`Some(subs)`): there a projection item or qual that is a SubPlan the
/// pipeline's own op evaluates (`subs`) reads that op's value (the op's arm tests it: `SELECT .., EXISTS (..)` into a
/// sink), and a Limit with constant bounds, a hash Probe's keys and quals, a Nest by its F5 `raise_free` fact with its
/// quals and parameters, and a SubPlan by its F5 fact pass too. Any other op may raise.
pub(crate) fn op_raise_free(gx: &Gx<'_, '_>, o: &Op<'_>, tail: Option<&[i32]>) -> bool {
    use gravity_decompose::graph::Bound;
    use gravity_decompose::join::JoinDesc;
    let own = |n: Node<'_>| tail.is_some_and(|subs| n.as_target_entry().map_or(n, |t| t.expr).as_sub_plan().is_some_and(|s| subs.contains(&s.plan_id)));
    let rf = |n: Node<'_>| own(n) || gx.fx(n).quiet();
    match o {
        Op::Filter(n) | Op::Gate(n) => rf(*n),
        Op::Project(l) => l.iter().all(rf),
        _ if tail.is_none() => false,
        Op::Limit { count, offset, .. } => [count, offset].into_iter().all(|b| matches!(*b, Bound::All) || matches!(*b, Bound::Const(v) if v >= 0)),
        Op::Join(JoinDesc::Probe { keys, qual, .. }) => keys.iter().chain(qual.iter()).all(rf),
        Op::Join(JoinDesc::Nest { params, qual, facts, .. }) => facts.raise_free && qual.iter().all(rf) && params.iter().all(|p| p.as_nest_loop_param().is_some_and(|p| rf(p.paramval))),
        Op::SubPlan(s) => s.raise_free,
        _ => false,
    }
}

/// D3245 (join.rtfilter.multi): `ops_raise_free` over the ops before a later hash probe of a pipeline, where an earlier
/// hash probe passes too when its keys and quals raise on no row, its output row is the probe row's (INNER, LEFT, SEMI,
/// ANTI; no marks, not the catalog form), and `build_holds_none(i, slot)` (the caller's: its build holds no error for
/// the probe's first row and does not run at it). Every row such a probe passes on is its probe row with columns added,
/// so it keeps each scan column the later probe keys on: a granule none of whose rows the later probe can keep runs no
/// op here that a user could see.
pub(crate) fn ops_raise_free_past_probes(gx: &Gx<'_, '_>, ops: &[Op<'_>], build_holds_none: impl Fn(usize, u8) -> bool) -> bool {
    use gravity_decompose::join::JoinDesc;
    use types_nodes::jointype::JoinType::{JOIN_ANTI, JOIN_INNER, JOIN_LEFT, JOIN_SEMI};
    ops.iter().enumerate().all(|(i, op)| match op {
        Op::Join(JoinDesc::Probe { slot, jointype: JOIN_INNER | JOIN_LEFT | JOIN_SEMI | JOIN_ANTI, marks: false, catalog: false, .. }) => {
            op_raise_free(gx, op, Some(&[])) && build_holds_none(i, *slot)
        }
        _ => op_raise_free(gx, op, None),
    })
}

/// D449's rule (props merge.rs `ops_can_raise`) on one expression: it raises nothing on any input and calls no user or
/// VOLATILE function. lockrows' EvalPlanQual rules (ops/lockrows/rerun.rs `admit`) ask it of what a recheck reruns or skips.
pub(crate) fn expr_quiet(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    gx.fx(n).quiet_calm()
}

/// Pipeline `p` from op `from` up to op `to` (its source too when `from` is 0 and it is not a COPY mark's stored row,
/// `copied`): `expr_quiet` over its filters, gates, projections and hash join keys, clauses and quals. A range, breaker or
/// const source is quiet, an index or bitmap one when its keys and recheck quals are. D4196: a nest with no parameters
/// whose join qual is `expr_quiet` and whose inner pipeline is quiet by this reading (its rescans read the same rows
/// and hand each joined row on, raising and calling nothing). Any other source or op may raise.
/// lockrows' rerun.rs `admit` (RERUN_FIRST, BUILD_FIRST, a build that may be empty) asks it.
pub(crate) fn span_quiet(gx: &Gx<'_, '_>, p: &gravity_decompose::PipelineNode<'_>, from: usize, to: usize, copied: bool) -> bool {
    crate::stack_check(); // a nest's inner pipeline is read by this reading too
    use gravity_decompose::{join::JoinDesc, Source};
    let pure = |n: Node<'_>| gx.fx(n).quiet_calm();
    let src = from > 0
        || copied
        || match &p.source {
            Source::Range { .. } | Source::Breaker { .. } | Source::Const => true,
            Source::Index { keys, recheck, .. } => keys.iter().all(|k| pure(k.value)) && recheck.iter().all(pure),
            Source::Bitmap { plan, recheck, .. } => bitmap_all(plan, &|_, keys, _| keys.iter().all(|k| pure(k.value))) && recheck.iter().all(pure),
            _ => false,
        };
    src && p.ops[from.min(p.ops.len())..to.min(p.ops.len())].iter().all(|o| match o {
        Op::Filter(n) | Op::Gate(n) => pure(*n),
        Op::Project(l) => l.iter().all(pure),
        Op::Join(JoinDesc::Probe { keys, clauses, qual, .. }) => keys.iter().chain(clauses.iter()).chain(qual.iter()).all(pure),
        Op::Join(JoinDesc::Build { keys, .. }) => keys.iter().all(pure),
        Op::Join(JoinDesc::Nest { params, qual, inner, .. }) => params.is_nil() && qual.iter().all(pure) && span_quiet(gx, inner, 0, inner.ops.len(), false),
        _ => false,
    })
}

/// lockrows REFFAM (ops/lockrows/rerun.rs RERUN_FIRST): pipeline `p` over a range source or a settled union's reader, its
/// ops only filters, projections, Append markers (an arm's, a feed's, the reader's union), a build's keys and (lockrows
/// J182: a merge join's sorted input) a full sort whose every key `sort_free` passes, none calling a user or VOLATILE
/// function. A recheck reruns such a producer over a REFERENCE mark's refetched row, the very version
/// that passed these expressions when the statement read it, so what may raise raised nothing then and raises nothing now;
/// a call is still a call PG may not make.
pub(crate) fn span_calls_free(gx: &Gx<'_, '_>, p: &gravity_decompose::PipelineNode<'_>) -> bool {
    use gravity_decompose::{ops::shared::append::AppendDesc, Source};
    let calm = |n: Node<'_>| !user_or_volatile(gx, n);
    let union = matches!(p.source, Source::Breaker { .. }) && matches!(p.ops.first(), Some(Op::Append(AppendDesc::Union { prune: None, .. })));
    (matches!(p.source, Source::Range { .. }) || union)
        && p.ops.iter().all(|o| match o {
            Op::Filter(n) => calm(*n),
            Op::Project(l) => l.iter().all(calm),
            Op::Append(AppendDesc::Arm { .. } | AppendDesc::Feed { .. }) => true,
            Op::Append(AppendDesc::Union { prune: None, .. }) => union,
            Op::Join(gravity_decompose::join::JoinDesc::Build { keys, .. }) => keys.iter().all(calm),
            Op::Sort(d) => matches!(d.form, gravity_decompose::ops::shared::sort::SortForm::Runs) && d.tlist.iter().all(calm) && d.keys.iter().all(|k| sort_key_type(d.tlist, k.resno).is_some_and(|t| sort_free(gx.catalog, k.opno, t))),
            _ => false,
        })
}

/// D3437 (drain/arms.rs `arm_run`): `n` raises nothing and calls no user or VOLATILE function (the node's Effects:
/// `quiet`, `user_or_volatile`), an aggregate read in the fold form (raise_free.rs `Agg::Fold`: a builtin aggregate whose
/// transitions and final raise on no input is a leaf, decompose's `agg_quiet`), as merge.rs `pipe_given` reads a group's
/// emit: a count's or a sum's group is no raise.
pub(crate) fn fold_quiet(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    let fx = gx.fx(n);
    fx.quiet() && !fx.user_or_volatile()
}

/// D3437 (drain/arms.rs `arm_run`): arm pipeline `q`'s own work raises nothing and calls no user or VOLATILE function
/// (`fold_quiet`): a breaker, const or range source, an arm marker with no pruning, filters, gates and projections (its
/// child's finals among them), a sort that `sort_quiet` passes over such a list, and its end's aggregates and FILTERs.
pub(crate) fn arm_quiet(gx: &Gx<'_, '_>, q: usize) -> bool {
    use gravity_decompose::{ops::shared::append::AppendDesc, Source};
    let p = &gx.pipelines()[q];
    let calm = |n: Node<'_>| fold_quiet(gx, n);
    let aggs = gx.group_at(q).is_none_or(|g| g.aggs.iter().all(|a| calm(a.node) && a.filter.is_none_or(calm)));
    aggs && matches!(p.source, Source::Breaker { .. } | Source::Const | Source::Range { .. })
        && p.ops.iter().all(|op| match op {
            Op::Filter(n) | Op::Gate(n) => calm(*n),
            Op::Project(l) => l.iter().all(calm),
            Op::Append(AppendDesc::Arm { prune: None, .. }) => true,
            Op::Sort(d) => sort_quiet(gx, d, &calm),
            _ => false,
        })
}

/// The type of the sort key at `resno` of target list `tlist`.
fn sort_key_type(tlist: &types_nodes::list::NodeList<'_>, resno: i16) -> Option<u32> {
    tlist.iter().filter_map(|n| n.as_target_entry()).find(|t| t.resno == resno).and_then(|t| type_of(t.expr))
}

/// D1107's eager refill (rerun/spool/form.rs `refilled`): a reader's FACTS-1b facts (F5 raise-free, the most volatile
/// call) let an enclosing nest's set run the fill at each re-open, where PostgreSQL rewinds over an unchanged parameter, and
/// at a re-open whose nest reads no outer row, where PostgreSQL never runs the child: raise-free with no VOLATILE call
/// (C1: STABLE code may run any number of times, at any time, and returns the same rows over the same arguments in one
/// statement, as effects/visible/material.rs `calm_fill` reads it).
pub(crate) fn refill_quiet(raise_free: bool, volatility: gravity_decompose::effects::Volatility) -> bool {
    raise_free && volatility != gravity_decompose::effects::Volatility::Volatile
}

/// D4101 (S3 rkeep ra-hx3/g8-hagg-lat-exists, g7-setop-lat-exists): whether op `j` of pipeline `k` (a nest loop or a
/// SubPlan) declares its inner quiet: it raises nothing and calls nothing VOLATILE (its FACTS-1b facts, `refill_quiet`),
/// so neither a fill run ahead of PG's first fetch nor the order a kept table hands its records on can show
/// (rerun/inner_once.rs `kept_emit`, lifetime/nestloop.rs `setop_kept`). A nest's join qual too (each conjunct's Effects,
/// as `expr_quiet` reads them): the nest's facts read its inner plan only, and the qual runs per inner record in the
/// table's order, where a SEMI / ANTI nest stops at its first match (a SubPlan's facts read its test).
pub(crate) fn owner_quiet(gx: &Gx<'_, '_>, k: usize, j: usize) -> bool {
    use gravity_decompose::join::JoinDesc;
    match gx.pipelines().get(k).and_then(|p| p.ops.get(j)) {
        Some(Op::Join(JoinDesc::Nest { facts, qual, .. })) => refill_quiet(facts.raise_free, facts.volatility) && qual.iter().all(|n| gx.fx(n).quiet_calm()),
        Some(Op::SubPlan(d)) => refill_quiet(d.raise_free, d.volatility),
        _ => false,
    }
}

/// errorhold (D1108 row 2, rerun/spool/form.rs `spool_fill`): a reader's FACTS-1b facts that let a plan Material's fill
/// that may raise run ahead of PostgreSQL's first fetch under the recheck licence: invariant (every rescan reads the same
/// rows) and IMMUTABLE, not raise-free (a raise-free one takes the plain eager form).
pub(crate) fn licensed_fill(invariant: bool, raise_free: bool, volatility: gravity_decompose::effects::Volatility) -> bool {
    invariant && !raise_free && volatility == gravity_decompose::effects::Volatility::Immutable
}

/// `DECISIONS.md` 4 (D) b (rerun/spool/form.rs `unchecked_arm`): a reader's FACTS-1b facts under which a plan Material's
/// fill that no form in PostgreSQL's order takes may fill up front unchecked: invariant (one fill serves every rescan), not
/// raise-free, and calling nothing VOLATILE.
pub(crate) fn unchecked_fill(invariant: bool, raise_free: bool, volatility: gravity_decompose::effects::Volatility) -> bool {
    invariant && !raise_free && volatility != gravity_decompose::effects::Volatility::Volatile
}

/// D1108 row 3 (rerun/spool/form.rs `whole_first`): a reader's FACTS-1b facts that no other eager form of a plan
/// Material's fill takes: the child may raise or calls VOLATILE code, and it is not errorhold's licensed fill. Such a fill
/// runs whole at the nest's first outer row, as PostgreSQL's first inner scan pulls it, or not at all.
pub(crate) fn deferred_fill(invariant: bool, raise_free: bool, volatility: gravity_decompose::effects::Volatility) -> bool {
    (!raise_free || volatility == gravity_decompose::effects::Volatility::Volatile) && !licensed_fill(invariant, raise_free, volatility)
}

/// D3378 (rerun/inner_once.rs `kept_run_lazy`): a nest's FACTS-1b facts under which its inner's kept sort run fills at the
/// nest's first outer row, where PostgreSQL sorts it at that rescan: the inner may raise and calls nothing VOLATILE (a
/// raise-free one keeps `kept_run`'s eager fill, `refill_quiet`).
pub(crate) fn waited_fill(raise_free: bool, volatility: gravity_decompose::effects::Volatility) -> bool {
    !raise_free && volatility != gravity_decompose::effects::Volatility::Volatile
}

/// D3378 (rerun/inner_once.rs `kept_run_lazy`): every raise of pipeline `x` comes at a row it reads, none when the program
/// binds. A builtin pattern match over a constant pattern decompose's `quiet_pattern` (DC-14) does not read quiet (a LIKE
/// ending in its escape) is compiled at bind, where PostgreSQL raises at the first row it matches, so a fill deferred to
/// the nest's first outer row would still raise under an empty outer. Read over a scan, a Result or a breaker source,
/// filters, gates, projections, sorts and the aggregates the pipeline folds; any other source or op reads as not (fail
/// closed).
pub(crate) fn raises_at_rows(gx: &Gx<'_, '_>, x: usize) -> bool {
    raises_at_rows_with(gx, x, false)
}

/// D3897 (rerun/spool/lazy.rs `drain_wait`): `raises_at_rows` with a hash probe's keys and qual and a build's keys read
/// too, each evaluated at a row it reads.
pub(crate) fn raises_at_rows_joined(gx: &Gx<'_, '_>, x: usize) -> bool {
    raises_at_rows_with(gx, x, true)
}

fn raises_at_rows_with(gx: &Gx<'_, '_>, x: usize, joins: bool) -> bool {
    use gravity_decompose::join::JoinDesc;
    use gravity_decompose::Source;
    let p = &gx.pipelines()[x];
    let src = matches!(p.source, Source::Range { .. } | Source::Const | Source::Breaker { .. });
    let ops = p.ops.iter().all(|op| match op {
        Op::Filter(n) | Op::Gate(n) => !bind_pattern(gx, *n),
        Op::Project(l) => !l.iter().any(|n| bind_pattern(gx, n)),
        Op::Sort(_) => true,
        Op::Join(JoinDesc::Probe { keys, qual, .. }) if joins => !keys.iter().chain(qual.iter()).any(|n| bind_pattern(gx, n)),
        Op::Join(JoinDesc::Build { keys, .. }) if joins => !keys.iter().any(|n| bind_pattern(gx, n)),
        Op::Join(JoinDesc::Drain { .. }) if joins => true,
        _ => false,
    });
    let aggs = gx.group_at(x).is_none_or(|g| g.aggs.iter().all(|a| !bind_pattern(gx, a.node) && a.filter.is_none_or(|f| !bind_pattern(gx, f))));
    src && ops && aggs
}

/// A node of `n` is a builtin pattern match over a constant pattern that may raise (`raises_at_rows`: decompose's
/// `pattern_operand` with `quiet_pattern` false). A node the child walk does not know reads as one (fail closed). D3907:
/// an aggregate's DISTINCT or ORDER BY clause names its operators and an argument's place, no expression of its own.
fn bind_pattern(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    crate::stack_check();
    if n.node_tag() == NodeTag::T_SortGroupClause {
        return false;
    }
    let call = n.as_func_expr().map(|x| (x.funcid, &x.args, x.inputcollid)).or_else(|| n.as_op_expr().map(|o| (o.opfuncid, &o.args, o.inputcollid)));
    let own = call.is_some_and(|(f, args, coll)| gravity_decompose::effects::pattern_operand(f, args).is_some() && !gravity_decompose::effects::quiet_pattern(gx.facts, f, args, coll));
    let mut kid = false;
    let known = kids(n, &mut |c| kid = kid || bind_pattern(gx, c));
    own || kid || !known
}

/// errorhold (D1108 row 2): pipeline `x` or a pipeline feeding it (join/build_class.rs `feed`) may call user code (a
/// function past FIRST_NORMAL_OID, D2044: a PL raise can leave SPI state no plan-order re-run holds, D2573): in its source
/// or ops (`user_code_of`) or in a group it produces (`group_user_code`).
pub(crate) fn fed_user_code(gx: &Gx<'_, '_>, x: usize) -> bool {
    let ps = gx.pipelines();
    let src = if let gravity_decompose::Source::Breaker { slot } = ps[x].source { Some(slot) } else { None };
    let feeders = crate::ops::hashjoin::build_class::feed(gx, x, ps[x].desc.consumes.iter().copied().chain(src));
    core::iter::once(x).chain(feeders).any(|y| user_code_of(gx, y, &ps[y]) || group_user_code(gx, y))
}

/// D3893 (`fed_user_code`): pipeline `p` (pipeline `at`, or a nest's or nested SubPlan's inner embedded in it, whose slots
/// `at` consumes: decompose join.rs `embed`) calls user code in an expression of its source or ops (a scan's keys,
/// recheck and quals, a sample's method, a FROM function, a LIMIT's bounds, a window's functions, offsets and run
/// condition, a pruning's steps), through an operator it compares by (a sort, merge join, Memoize, window, DISTINCT ON,
/// merge emit or set operation), or in an embedded inner. A union's emit reads its arms (`fed_user_code` of each, by
/// `arms_at`); what only moves rows calls nothing: an arm's feed, a spool write, a hash join's drain of the build rows no
/// probe matched. A hashed SubPlan whose table `at` consumes is read through its builder (a feeder), a built one's end is
/// its own pipeline's (siblings AGA-28 H1). A write or row lock (its triggers and EvalPlanQual), a recursive union (seed
/// and term run as their own pipelines) unless its own flags show it calls none (D4470: `IterDesc::calm`, `term_volatile`,
/// its dedup operators), a table function (its columns' input functions) and a SubPlan whose plan lives elsewhere read as
/// user code (fail closed).
fn user_code_of(gx: &Gx<'_, '_>, at: usize, p: &gravity_decompose::PipelineNode<'_>) -> bool {
    crate::stack_check();
    use gravity_decompose::join::JoinDesc;
    use gravity_decompose::ops::shared::append::AppendDesc;
    use gravity_decompose::ops::shared::setop::SetDesc;
    use gravity_decompose::ops::shared::sort::SortForm;
    use gravity_decompose::ops::shared::subplan::SubForm;
    use gravity_decompose::{Bound, Source};
    crate::stack_check();
    let any = |l: &types_nodes::list::NodeList<'_>| l.iter().any(|n| gx.fx(n).user_calls());
    let user_op = |o: u32| o >= FIRST_NORMAL_OID;
    let eq_keys = |k: &[gravity_decompose::ops::shared::sort::EqKey]| k.iter().any(|k| user_op(k.opno));
    let bound = |b: &Bound<'_>| matches!(b, Bound::Expr(n) if gx.fx(*n).user_calls());
    let prune = |d: &Option<gravity_decompose::ops::shared::append::PruneDesc<'_>>| d.as_ref().is_some_and(|d| prune_user_code(gx, d));
    let arms = |slot: u8| arms_at(gx, at, slot, &|q| fed_user_code(gx, q));
    let src = match &p.source {
        Source::Range { .. } | Source::Const | Source::Breaker { .. } | Source::Named { .. } => false,
        Source::Index { keys, recheck, .. } => keys.iter().any(|k| gx.fx(k.value).user_calls()) || any(recheck),
        Source::Bitmap { plan, recheck, .. } => bitmap_user_code(gx, plan) || any(recheck),
        Source::Values { lists, .. } => any(lists),
        Source::Tid { quals, .. } => any(quals),
        Source::Sample { sample, .. } => user_op(sample.handler) || sample.args.iter().chain(sample.repeatable).any(|n| gx.fx(n).user_calls()),
        Source::TableFunc { .. } => true,
    };
    src || p.ops.iter().any(|op| {
        if let Some((count, offset)) = crate::frame::iface::limit_bounds(op) {
            return bound(count) || bound(offset);
        }
        match op {
            Op::Filter(n) | Op::Gate(n) => gx.fx(*n).user_calls(),
            Op::Project(l) => any(l),
            Op::Srf(d) => d.functions.iter().any(|f| gx.fx(f.node).user_calls()) || any(d.tlist),
            Op::Sort(s) => s.keys.iter().any(|k| user_op(k.opno) || k.own_family) || any(s.tlist) || matches!(&s.form, SortForm::TopK { count, offset, .. } | SortForm::Walk { count, offset, .. } if bound(count) || bound(offset)),
            Op::Window(w) => {
                let offsets = [w.plan.startOffset, w.plan.endOffset];
                w.functions.iter().any(|f| user_op(f.func.winfnoid) || gx.fx(f.node).user_calls()) || any(w.tlist) || any(w.run_condition) || offsets.into_iter().flatten().any(|n| gx.fx(n).user_calls())
                    || w.plan.partOperators.iter().chain(w.plan.ordOperators.iter()).chain([&w.plan.startInRangeFunc, &w.plan.endInRangeFunc]).any(|&o| user_op(o))
            }
            Op::FirstOfRun(t) => eq_keys(&t.keys),
            Op::GroupRun(i) => gx.graph.streams.get(*i as usize).is_none_or(|g| group_calls(gx, g)),
            Op::SetOp(SetDesc::Identity { keys }) => eq_keys(keys),
            Op::SetOp(SetDesc::Set { keys, tlist, .. }) => eq_keys(keys) || any(tlist),
            // D4470 (#530): a recursive union's seed and terms run as their own pipelines, which `feed` may not name: the
            // union is read by its own flags (decompose `IterDesc`): a calm seed and once-run term inputs (`calm`), a term
            // that calls no user or VOLATILE function (`term_volatile`), built-in dedup operators. Else user code
            Op::SetOp(SetDesc::Iter(it)) => !it.calm || it.term_volatile || eq_keys(&it.dedup),
            Op::Write(_) | Op::Lock(_) => true,
            Op::Join(JoinDesc::Nest { inner, qual, params, .. }) => any(qual) || any(params) || user_code_of(gx, at, inner),
            Op::Join(JoinDesc::Probe { keys, clauses, qual, .. }) => any(keys) || any(clauses) || any(qual),
            Op::Join(JoinDesc::Build { keys, .. }) => any(keys),
            Op::Join(JoinDesc::Merge { clauses, qual, families, .. }) => any(clauses) || any(qual) || families.iter().any(|&f| user_op(f)),
            Op::Join(JoinDesc::Memo { keys, ops, binary, .. }) => any(keys) || !binary && ops.iter().any(|&o| user_op(o)),
            Op::Join(JoinDesc::Drain { .. } | JoinDesc::Spool) => false,
            Op::Append(AppendDesc::Feed { .. }) => false,
            Op::Append(AppendDesc::Arm { prune: d, .. }) => prune(d),
            Op::Append(AppendDesc::Union { slot, prune: d, .. }) => prune(d) || arms(*slot),
            Op::Append(AppendDesc::Merge { slot, keys, prune: d, .. }) => prune(d) || keys.iter().any(|k| user_op(k.opno) || k.own_family) || arms(*slot),
            Op::SubPlan(d) => d.test.is_some_and(|n| gx.fx(n).user_calls()) || any(d.args) || match &d.form {
                SubForm::Hashed { slot } => !gx.pipelines()[at].desc.consumes.contains(slot),
                SubForm::Build => false,
                SubForm::Nested { inner } => user_code_of(gx, at, inner),
            },
            // a LIMIT, read above by its bounds
            _ => true,
        }
    })
}

/// D3893 (`user_code_of`): `f` holds for an arm of the union a reader in pipeline `at` (its emit, or a nest inner embedded
/// in it) reads through `slot`: a pipeline before `at` whose `Feed` writes `slot`, back to the slot's previous reader (a
/// slot is reused after its life). build_class `feed` names only the nearest arm (`life_of`) for an embedded reader.
fn arms_at(gx: &Gx<'_, '_>, at: usize, slot: u8, f: &dyn Fn(usize) -> bool) -> bool {
    let ps = gx.pipelines();
    let reads = |q: usize| ps[q].desc.consumes.contains(&slot) || matches!(ps[q].source, gravity_decompose::Source::Breaker { slot: s } if s == slot);
    let from = (0..at).rev().find(|&q| reads(q)).map_or(0, |q| q + 1);
    (from..at).any(|q| crate::rerun::union_arms::feed_of(&ps[q]) == Some(slot) && f(q))
}

/// D3893 (`user_code_of`): run-time pruning `d` calls user code: a step's key expression does, or a step compares through
/// a support function past FIRST_NORMAL_OID (a user operator class's); a prune info of another kind reads as one (fail
/// closed). A combine step only combines its steps' results.
fn prune_user_code(gx: &Gx<'_, '_>, d: &gravity_decompose::ops::shared::append::PruneDesc<'_>) -> bool {
    let step = |s: Node<'_>| s.as_partition_prune_step_op().is_some_and(|o| o.exprs.iter().any(|n| gx.fx(n).user_calls()) || o.cmpfns.iter().any(|f| f >= FIRST_NORMAL_OID));
    let mut infos = d.info.prune_infos.iter().flat_map(|l| l.as_list().into_iter().flat_map(|l| l.iter()));
    infos.any(|r| r.as_partitioned_rel_prune_info().is_none_or(|r| r.initial_pruning_steps.iter().chain(r.exec_pruning_steps.iter()).any(|s| step(s))))
}

/// D1108 (`fed_user_code`, beside `user_code_of`, which reads a pipeline's source and ops only): a group
/// pipeline `x` produces (its breaker's, or a GroupRun's stream) calls user code (`group_calls`).
pub(crate) fn group_user_code(gx: &Gx<'_, '_>, x: usize) -> bool {
    let life = gravity_program::LifeId(x as u16);
    gx.graph.groups.iter().chain(gx.graph.streams.iter()).filter(|g| g.producer == life).any(|g| group_calls(gx, g))
}

/// `group_user_code`: group `g` calls user code: an aggregate past FIRST_NORMAL_OID (a PL transition or final function
/// may raise), an aggregate's arguments or FILTER, or a grouping key's expression.
fn group_calls(gx: &Gx<'_, '_>, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>) -> bool {
    g.aggs.iter().any(|a| a.fnoid >= FIRST_NORMAL_OID || gx.fx(a.node).user_calls() || a.filter.is_some_and(|n| gx.fx(n).user_calls())) || g.keys.iter().any(|k| gx.fx(k.expr.node()).user_calls())
}

/// D1108 (`user_code_of`'s Bitmap source): a bitmap heap scan's index arms, read as the Index source's keys
/// are (each key's value), And / Or arms all of theirs.
fn bitmap_user_code(gx: &Gx<'_, '_>, plan: &gravity_decompose::ops::shared::scan::BitmapPlan<'_>) -> bool {
    crate::stack_check();
    use gravity_decompose::ops::shared::scan::BitmapPlan;
    match plan {
        BitmapPlan::Index { keys, .. } => keys.iter().any(|k| gx.fx(k.value).user_calls()),
        BitmapPlan::And { plans, .. } | BitmapPlan::Or { plans, .. } => plans.iter().any(|p| bitmap_user_code(gx, p)),
    }
}

/// G2(b) (rerun/union_arms.rs `drained`): a nest between a settled union and the MERGE / UPDATE / DELETE the union feeds
/// acts nowhere through its inner when the inner's FACTS-1b most volatile call (F5) is below VOLATILE. Whether the inner
/// raises stays its own form's question (D1108: filled ahead only when raise-free, else run per outer row).
pub(crate) fn nest_inner_calm(volatility: gravity_decompose::effects::Volatility) -> bool {
    volatility != gravity_decompose::effects::Volatility::Volatile
}

/// nestloop D1018 (rerun/inner_probe.rs `ok`): a nest join among a nest inner's steps before its kept hash probe (the
/// inner's own key lookup feeding the probe side). PostgreSQL's rescans run it, then the probe against the kept table, as
/// the inner does, when its inner raises nothing (F5 `raise_free` over the inner plan) and, unless `acts` (the count-free
/// reading, `ok`'s `calm`), calls nothing VOLATILE; its join qual and the outer values it binds as parameters pass `q`.
pub(crate) fn nest_step_quiet(op: &Op<'_>, q: &dyn Fn(Node<'_>) -> bool, acts: bool) -> bool {
    let Op::Join(gravity_decompose::join::JoinDesc::Nest { params, qual, facts, .. }) = op else { return false };
    facts.raise_free && (acts || nest_inner_calm(facts.volatility)) && qual.iter().all(q) && params.iter().all(|p| p.as_nest_loop_param().is_some_and(|p| q(p.paramval)))
}

/// drain (gen mt-0076 group, hashjoin/drain/spool_admit.rs `op_acts`): whether window op `op` may act: the WindowAgg's
/// projection or a function's argument or FILTER calls a VOLATILE function, one of its functions (the WindowFunc node),
/// its run condition or a frame offset may call what acts (`a`), or a partition or order equality operator is a user's
/// (`user_op`). The ranking functions over built-in keys (row_number, rank, dense_rank, ..) call nothing; any other op acts.
pub(crate) fn window_acts(op: &Op<'_>, a: &dyn Fn(Node<'_>) -> bool, user_op: &dyn Fn(u32) -> bool) -> bool {
    let Op::Window(w) = op else { return true };
    let offsets = [w.plan.startOffset, w.plan.endOffset];
    w.volatile_args || w.volatile_proj || w.functions.iter().any(|f| a(f.node)) || w.run_condition.iter().any(a) || offsets.into_iter().flatten().any(a)
        || w.plan.partOperators.iter().chain(w.plan.ordOperators.iter()).any(|&o| user_op(o))
}

/// D1906 (fail closed; hashjoin probe.rs `opaque`): whether pipeline `b`'s side (`b` and its feeders, all `side_by` reads)
/// holds what the side class reads as Raises without seeing into it, so a VOLATILE call there reads as one: a SubPlan of
/// any volatility (its plan runs per row), a LIMIT or OFFSET expression, a window (`windows`: one `window_calls` passes
/// does not), a streamed group (D1071) and every other op but a filter, gate, projection, hash probe or build, set function
/// or constant LIMIT, what moves rows (a union's feed, arm or emit with no run-time pruning, a spool write) and a sort,
/// merge join, ordered emit or nest (its embedded inner read through, `nest_inner_calls`) whose target list, clauses,
/// operators and inner call nothing (a nest's parameters read by their values, a Memoize by its keys and operators:
/// D3781), and a SubPlan whose arguments, test and inner call nothing (`sub_quiet`) (D3566); a TID or TABLESAMPLE source (a rescan draws its sample again); a node kind the class does not read as
/// a call (`hidden`) in what it reads; a feeder's group whose aggregate, arguments or FILTER call (its fold, at the
/// breaker, which the class does not read); D2065: a function or operator over a range whose comparison (rngsubopc
/// FUNCTION 1) or canonical function is user code. `DECISIONS.md` 4 (D) b: a part runs ahead unchecked only where this is
/// false (`Px::ahead_unchecked` reads user code by the side class).
pub(crate) fn hides_calls(gx: &Gx<'_, '_>, b: usize, windows: bool) -> bool {
    hides_in(gx, b, windows, true, true)
}

/// D3566 (`licence_unchecked`'s user code): `hides_calls` with a WindowAgg read into, but a TABLESAMPLE source hides a call
/// only through a method that is not built in or arguments that call: its draw runs no user code (a rescan drawing its
/// sample again is the re-run rules' question, which `hides_calls` keeps for a kept build).
pub(crate) fn hides_user_code(gx: &Gx<'_, '_>, b: usize) -> bool {
    hides_in(gx, b, true, false, true)
}

/// D3788 (build_class `lazy_group_by`'s raise-only rule): `hides_user_code` over pipeline `b`'s own ops and source, not
/// what feeds it: a pipeline the program runs between a lazy group and its reader, whose feeders run where they run
/// whichever order the group takes (each one between is asked in turn), and a member of the group itself.
pub(crate) fn own_hides_user_code(gx: &Gx<'_, '_>, b: usize) -> bool {
    hides_in(gx, b, true, false, false)
}

/// D3587 (`Px::run_ahead`): the statement calls user code somewhere: decompose rerun's F4 reading (`Found::user_call`)
/// over its plan tree and every subplan: a function, operator, aggregate (its support too) or type I/O past
/// FIRST_NORMAL_OID, a domain check, or a call or node the facts cannot name. A graph mapped without its statement
/// does (fail closed). Where it does not, nothing `hides_user_code` reads coarsely (a node it does not look into) can
/// run user code.
pub(crate) fn statement_user_code(gx: &Gx<'_, '_>, arena: gravity_program::ArenaRef) -> bool {
    let Some(s) = gx.graph.stmt else { return true };
    // D3586: answered once per lowering under one facts (memo.rs): `Px::run_ahead` asks it per pipeline, and a walk of the
    // whole plan per arm made a pruned Append's lowering quadratic in its arms
    let (key, facts) = (crate::memo::key_of(&gx.graph.pipelines), gx.facts as *const _ as *const () as usize);
    if let Some(Some((f, user))) = crate::memo::read(key, |m| m.user_code) {
        if f == facts {
            return user;
        }
    }
    let user = s.planTree.into_iter().chain(s.subplans.iter().flatten()).any(|r| gravity_decompose::rerun::of_plan(s, gx.facts, arena, r).user_call);
    crate::memo::write(key, |m| m.user_code = Some((facts, user)));
    user
}

/// `hides_calls` and `hides_user_code`: `draws`, a TABLESAMPLE source hides a call (a rescan draws its sample again);
/// `fed`, the pipelines feeding `b` (build_class `feed`) are read with it (`own_hides_user_code` reads `b` alone).
fn hides_in(gx: &Gx<'_, '_>, b: usize, windows: bool, draws: bool, fed: bool) -> bool {
    use gravity_decompose::graph::{Bound, Source};
    use gravity_decompose::join::JoinDesc;
    use gravity_decompose::ops::shared::append::AppendDesc;
    let p = &gx.pipelines()[b];
    let mut all = if fed { crate::ops::hashjoin::build_class::feed(gx, b, p.desc.consumes.iter().copied().chain(if let Source::Breaker { slot } = p.source { Some(slot) } else { None })) } else { PgVec::new_in(gx.mcx) };
    all.push_in(b);
    // D2065: a range function over a range whose comparison is user code calls it (effects.rs `range_user_cmp`). D3566: a
    // SubPlan's own parts are read (`sub_quiet`): a node naming a SubPlan whose op in these pipelines calls nothing passes
    // D3587: a user function LANGUAGE internal over a builtin runs PostgreSQL's own code (`side_calls_body`)
    let parts = |n: Node<'_>| visible::hashjoin::side_calls_body(gx, n) || visible::hashjoin::hidden_parts(gx, n) || range_user_cmp(gx, n);
    let mut quiet: PgVec<'_, i32> = PgVec::new_in(gx.mcx);
    for &x in &all {
        for op in gx.pipelines()[x].ops.iter() {
            if let Op::SubPlan(d) = op {
                if sub_quiet(gx, x, d, &parts, &quiet[..], windows, draws) {
                    quiet.push_in(d.plan_id);
                }
            }
        }
    }
    let named = |n: Node<'_>| !sub_refs_in(n, &quiet[..]);
    let hidden = |n: Node<'_>| visible::hashjoin::hidden_parts(gx, n) || range_user_cmp(gx, n) || named(n);
    let calls = |n: Node<'_>| visible::hashjoin::side_calls_body(gx, n) || hidden(n);
    let user_key = |k: &gravity_decompose::ops::shared::sort::SortKey| op_user_code(gx, k.opno);
    let hides = |x: usize, op: &Op<'_>| {
        if let Some((count, offset)) = crate::frame::iface::limit_bounds(op) {
            return matches!(count, Bound::Expr(_)) || matches!(offset, Bound::Expr(_));
        }
        match op {
            Op::Filter(n) | Op::Gate(n) => hidden(*n),
            Op::Project(l) => l.iter().any(|n| hidden(n)),
            Op::Join(JoinDesc::Probe { keys, qual, .. }) => keys.iter().chain(qual.iter()).any(|n| hidden(n)),
            Op::Join(JoinDesc::Build { keys, .. }) => keys.iter().any(|n| hidden(n)),
            Op::Srf(d) => d.functions.iter().any(|f| hidden(f.node)),
            Op::Window(_) if windows => window_calls(op, &calls),
            // D3566: what moves rows and evaluates nothing (a union's feed, arm marker or emit with no run-time pruning, a
            // Material's spool write, D3893: a hash join's drain of the build rows no probe matched) hides no call; a sort
            // evaluates its target list and compares through its keys' operators (`op_user_code`); a merge join its clauses,
            // join qual and families' support; an ordered emit its keys' operators
            Op::Append(AppendDesc::Feed { .. } | AppendDesc::Arm { prune: None, .. } | AppendDesc::Union { prune: None, .. }) | Op::Join(JoinDesc::Spool | JoinDesc::Drain { .. }) => false,
            Op::Append(AppendDesc::Merge { keys, prune: None, .. }) => keys.iter().any(user_key),
            Op::Sort(sd) => !sort_settled(sd) || sd.tlist.iter().any(|n| calls(n)) || sd.keys.iter().any(user_key),
            Op::Join(JoinDesc::Merge { clauses, qual, families, .. }) => families.iter().any(|&f| f >= FIRST_NORMAL_OID) || clauses.iter().chain(qual.iter()).any(|n| calls(n)),
            Op::Join(JoinDesc::Nest { inner, qual, params, .. }) => qual.iter().any(|n| calls(n)) || params_call(params, &calls) || nest_inner_calls(gx, x, inner, &parts, &quiet[..], windows, draws),
            Op::Join(JoinDesc::Memo { keys, ops, colls, binary, .. }) => memo_calls(gx, keys, ops, colls, *binary, &calls),
            Op::SubPlan(d) => !quiet.contains(&d.plan_id),
            // D3587: a recursive union moves its generations' rows and calls only a UNION's dedup equality operators
            // (`op_user_code`); what its seed and terms call is read by the union's own flags (fail closed): the term's
            // user or VOLATILE calls (`term_volatile`) and a seed or once-run term input that is not quiet (`early_quiet`)
            Op::SetOp(gravity_decompose::ops::shared::setop::SetDesc::Iter(it)) => it.term_volatile || !it.early_quiet || it.dedup.iter().any(|k| op_user_code(gx, k.opno)),
            _ => true,
        }
    };
    let src = |s: &Source<'_>| match s {
        Source::Values { lists, .. } => lists.iter().filter_map(|l| l.as_list()).flat_map(|l| l.iter()).any(|n| hidden(n)),
        Source::TableFunc { func: f, .. } => {
            let cols = [&f.colexprs, &f.coldefexprs, &f.colvalexprs].into_iter().flat_map(|l| l.iter().flatten());
            f.docexpr.into_iter().chain(f.rowexpr).chain(f.passingvalexprs.iter()).chain(cols).any(|n| hidden(n))
        }
        Source::Index { keys, recheck, .. } => keys.iter().map(|k| k.value).chain(recheck.iter()).any(|n| hidden(n)),
        Source::Bitmap { recheck, .. } => recheck.iter().any(|n| hidden(n)),
        Source::Tid { .. } => true,
        Source::Sample { sample, .. } => draws || sample.handler >= FIRST_NORMAL_OID || sample.args.iter().chain(sample.repeatable).any(|n| hidden(n)),
        _ => false,
    };
    all.into_iter().any(|x| {
        let p = &gx.pipelines()[x];
        let aggs = gx.group_at(x).is_some_and(|g| g.aggs.iter().any(|a| gravity_decompose::effects::user_code(a.fnoid) || calls(a.node) || a.filter.is_some_and(calls)));
        p.ops.iter().any(|op| hides(x, op)) || src(&p.source) || aggs
    })
}

/// D3781 (`hides_calls`, `nest_inner_calls`): a nest join's parameters, read by their values. A NestLoopParam hands the
/// outer row's `paramval` to the inner's PARAM_EXEC slot (ExecNestLoop) and evaluates nothing else, so it calls what
/// `calls` sees in that value; a node of another kind reads as a call (fail closed).
fn params_call(params: &types_nodes::list::NodeList<'_>, calls: &dyn Fn(Node<'_>) -> bool) -> bool {
    params.iter().any(|p| p.as_nest_loop_param().is_none_or(|p| calls(p.paramval)))
}

/// D3781 (`hides_calls`, `nest_inner_calls`): a Memoize (nest.memo) evaluates its cache keys and hashes and compares them
/// through their hash equality operators (`ops`: MemoHash_hash / MemoHash_equal call their hash and equality procs), or
/// by datum image (`binary`), and calls nothing else. It calls what `calls` sees in a key, or user code through an
/// operator (`op_user_code`). A key whose hash raises 42P22 (a collatable type under no collation, as D2849's hash join
/// keys) or whose type the facts do not know reads as a call too: no unchecked licence covers that error.
fn memo_calls(gx: &Gx<'_, '_>, keys: &types_nodes::list::NodeList<'_>, ops: &[u32], colls: &[u32], binary: bool, calls: &dyn Fn(Node<'_>) -> bool) -> bool {
    use gravity_decompose::catalog::typefacts::facts_of;
    let no_coll = |(n, &c): (Node<'_>, &u32)| c == 0 && type_of(n).and_then(|t| facts_of(gx.facts, t)).is_none_or(|f| f.collatable);
    keys.iter().any(|n| calls(n)) || !binary && (ops.iter().any(|&o| op_user_code(gx, o)) || keys.iter().zip(colls).any(no_coll))
}

/// D3566 (`hides_calls`): SubPlan op `d` calls nothing `parts` sees in its arguments and test, and a nested one nothing
/// in its embedded inner (`nest_inner_calls`, a SubPlan inside read the same way); a SubPlan node they name must be one
/// of `quiet` (read before it). A hashed or built one's set is the pipeline that builds it, read as a feeder.
fn sub_quiet(gx: &Gx<'_, '_>, at: usize, d: &gravity_decompose::ops::shared::subplan::SubPlanDesc<'_>, parts: &dyn Fn(Node<'_>) -> bool, quiet: &[i32], windows: bool, draws: bool) -> bool {
    use gravity_decompose::ops::shared::subplan::SubForm;
    let calls = |n: Node<'_>| parts(n) || !sub_refs_in(n, quiet);
    !d.args.iter().any(|n| calls(n))
        && !d.test.is_some_and(|n| calls(n))
        && match &d.form {
            SubForm::Nested { inner } => !nest_inner_calls(gx, at, inner, parts, quiet, windows, draws),
            SubForm::Build | SubForm::Hashed { .. } => true,
        }
}

/// D3566 (`hides_calls`): every SubPlan node in `n` names one of `quiet`.
fn sub_refs_in(n: Node<'_>, quiet: &[i32]) -> bool {
    crate::stack_check();
    let mut ok = n.as_sub_plan().is_none_or(|s| quiet.contains(&s.plan_id));
    gravity_decompose::expr::kids(n, &mut |c| ok = ok && sub_refs_in(c, quiet));
    ok
}

/// D3587 (`hides_calls`): operator `opno`, named alone by a sort or an ordered emit, runs user code: it is a user's and a
/// support function it runs (`Facts::op_calls`: its code, an ordering operator's comparator and sortsupport, a hashable
/// one's hash procs) is neither a builtin nor a user function LANGUAGE internal over one (`effects::body`, KeySem); one
/// the facts do not know is user code. A family with no order proc for the type raises at the sort's first call, which
/// is no call.
pub(crate) fn op_user_code(gx: &Gx<'_, '_>, opno: u32) -> bool {
    opno >= FIRST_NORMAL_OID && !gx.facts.op_calls(opno).is_some_and(|c| c.each().all(|p| gravity_decompose::effects::body(gx.facts, p).is_some()))
}

/// D3566 (`hides_calls`): nest inner `p` (embedded in pipeline `at`), run per outer row, which the side class reads only by
/// its F5 volatility, calls what `calls` sees in its source, filters, projections, probes and builds, a Memoize
/// (`memo_calls`), a nest inside it (its parameters by their values, `params_call`: D3781), and as `hides_calls` reads
/// them its sorts, merge joins, windows (`windows`), ordered emits and recursive unions; D3893: a union it reads with no
/// run-time pruning calls what any arm does, each read as an inner (`arm_calls`: the side class reads only the nearest
/// arm, build_class `feed`). What moves rows calls nothing (a union's feed or arm marker, a spool write, a hash join's
/// drain); any other op reads as a call (fail closed).
fn nest_inner_calls(gx: &Gx<'_, '_>, at: usize, p: &gravity_decompose::graph::PipelineNode<'_>, parts: &dyn Fn(Node<'_>) -> bool, outer: &[i32], windows: bool, draws: bool) -> bool {
    crate::stack_check(); // D3832
    use gravity_decompose::graph::{Bound, Source};
    use gravity_decompose::join::JoinDesc;
    use gravity_decompose::ops::shared::append::AppendDesc;
    let mut quiet: PgVec<'_, i32> = mcx::to_vec_in(outer, gx.mcx);
    for op in p.ops.iter() {
        if let Op::SubPlan(d) = op {
            if sub_quiet(gx, at, d, parts, &quiet[..], windows, draws) {
                quiet.push_in(d.plan_id);
            }
        }
    }
    let calls = |n: Node<'_>| parts(n) || !sub_refs_in(n, &quiet[..]);
    let keys = |k: &[IndexKey<'_>]| k.iter().any(|x| calls(x.value));
    let src = match &p.source {
        Source::Range { .. } | Source::Breaker { .. } | Source::Const | Source::Named { .. } => false,
        Source::Values { lists, .. } => lists.iter().filter_map(|l| l.as_list()).flat_map(|l| l.iter()).any(|n| calls(n)),
        Source::Index { keys: k, recheck, .. } => keys(k) || recheck.iter().any(|n| calls(n)),
        Source::Bitmap { plan, recheck, .. } => !bitmap_all(plan, &|_, k, _| !keys(k)) || recheck.iter().any(|n| calls(n)),
        // a sample's method and arguments (a user method is user code); `draws`, a rescan per outer row draws again
        Source::Sample { sample, .. } => draws || sample.handler >= FIRST_NORMAL_OID || sample.args.iter().chain(sample.repeatable).any(|n| calls(n)),
        _ => true,
    };
    src || p.ops.iter().any(|op| {
        if let Some((count, offset)) = crate::frame::iface::limit_bounds(op) {
            return matches!(count, Bound::Expr(_)) || matches!(offset, Bound::Expr(_));
        }
        match op {
            Op::Filter(n) | Op::Gate(n) => calls(*n),
            Op::Project(l) => l.iter().any(|n| calls(n)),
            Op::Join(JoinDesc::Probe { keys, qual, .. }) => keys.iter().chain(qual.iter()).any(|n| calls(n)),
            Op::Join(JoinDesc::Build { keys, .. }) => keys.iter().any(|n| calls(n)),
            Op::Join(JoinDesc::Nest { inner, qual, params, .. }) => qual.iter().any(|n| calls(n)) || params_call(params, &calls) || nest_inner_calls(gx, at, inner, parts, &quiet[..], windows, draws),
            Op::Join(JoinDesc::Memo { keys, ops, colls, binary, .. }) => memo_calls(gx, keys, ops, colls, *binary, &calls),
            Op::Join(JoinDesc::Merge { clauses, qual, families, .. }) => families.iter().any(|&f| f >= FIRST_NORMAL_OID) || clauses.iter().chain(qual.iter()).any(|n| calls(n)),
            Op::Sort(sd) => !sort_settled(sd) || sd.tlist.iter().any(|n| calls(n)) || sd.keys.iter().any(|k| op_user_code(gx, k.opno)),
            Op::Window(_) if windows => window_calls(op, &calls),
            Op::SubPlan(d) => !quiet.contains(&d.plan_id),
            Op::SetOp(gravity_decompose::ops::shared::setop::SetDesc::Iter(it)) => it.term_volatile || !it.early_quiet || it.dedup.iter().any(|k| op_user_code(gx, k.opno)),
            Op::Append(AppendDesc::Feed { .. } | AppendDesc::Arm { prune: None, .. }) | Op::Join(JoinDesc::Spool | JoinDesc::Drain { .. }) => false,
            Op::Append(AppendDesc::Union { slot, prune: None, .. }) => arms_at(gx, at, *slot, &|q| arm_calls(gx, q, parts, &quiet[..], windows, draws)),
            Op::Append(AppendDesc::Merge { slot, keys, prune: None, .. }) => keys.iter().any(|k| op_user_code(gx, k.opno)) || arms_at(gx, at, *slot, &|q| arm_calls(gx, q, parts, &quiet[..], windows, draws)),
            // D3743: a set function the inner calls per outer row (UNNEST(ARRAY[..]) beside pg_stat_activity, the
            // exporter's buckets): its calls with their arguments and a ProjectSet's plain entries, read as a filter's
            Op::Srf(d) => d.functions.iter().any(|f| calls(f.node)) || d.tlist.iter().any(|n| calls(n)),
            _ => true,
        }
    })
}

/// D3893 (`nest_inner_calls`): union arm `q`, read by a nest inner, calls what `calls` sees in it or in a pipeline feeding
/// it, each read as an inner is, or in a group they produce (an aggregate past FIRST_NORMAL_OID, its arguments or FILTER).
fn arm_calls(gx: &Gx<'_, '_>, q: usize, parts: &dyn Fn(Node<'_>) -> bool, quiet: &[i32], windows: bool, draws: bool) -> bool {
    let ps = gx.pipelines();
    let src = if let gravity_decompose::Source::Breaker { slot } = ps[q].source { Some(slot) } else { None };
    let calls = |n: Node<'_>| parts(n) || !sub_refs_in(n, quiet);
    let aggs = |x: usize| gx.group_at(x).is_some_and(|g| g.aggs.iter().any(|a| gravity_decompose::effects::user_code(a.fnoid) || calls(a.node) || a.filter.is_some_and(calls)));
    let feeders = crate::ops::hashjoin::build_class::feed(gx, q, ps[q].desc.consumes.iter().copied().chain(src));
    core::iter::once(q).chain(feeders).any(|x| nest_inner_calls(gx, x, &ps[x], parts, quiet, windows, draws) || aggs(x))
}

/// `hides_calls` and `nest_inner_calls`: sort `sd`'s bounds are settled (a full sort, or a top-k or walk whose count and
/// offset are no expression).
fn sort_settled(sd: &gravity_decompose::ops::shared::sort::SortDesc<'_>) -> bool {
    use gravity_decompose::ops::shared::sort::SortForm;
    use gravity_decompose::Bound;
    match &sd.form {
        SortForm::Runs => true,
        SortForm::TopK { count, offset, .. } | SortForm::Walk { count, offset, .. } => !matches!(count, Bound::Expr(_)) && !matches!(offset, Bound::Expr(_)),
    }
}

/// `DECISIONS.md` 4 (D) b (`hides_calls`): WindowAgg op `op` may call user or VOLATILE code: `window_acts`
/// with `calls` reading its expressions, and a partition or order operator that is not built in.
pub(crate) fn window_calls(op: &Op<'_>, calls: &dyn Fn(Node<'_>) -> bool) -> bool {
    window_acts(op, calls, &|o| o >= gravity_decompose::catalog::typefacts::FIRST_NORMAL_OID)
}

/// with#170 / #171 (hashjoin build_class.rs `own_side_by`): WindowAgg `d` raises on no input and calls nothing `q` rejects,
/// the raise-free side of `window_acts` (and stronger than rerun/union_reads.rs `raise_only`, which lets an argument
/// raise): no frame offset (PG checks an offset's sign and NULL), partition and order keys of scalar builtin types under
/// initdb operators (their builtin comparisons raise nothing), a run condition `q` passes, and functions that raise nothing:
/// row_number, rank, dense_rank, percent_rank and cume_dist, or a plain builtin aggregate the effects home reads quiet
/// (`agg_oid_quiet`, the rule an Aggref takes) over arguments and a FILTER `q` passes. A key over an anonymous record or
/// anyarray is not scalar: record_eq / record_cmp and array_eq reach each column's or element's own comparison (user code,
/// or "could not identify an equality operator"). A collatable key needs a collation: texteq under 0 raises (D1292).
pub(crate) fn window_quiet(d: &gravity_decompose::ops::window::WindowDesc<'_>, q: &dyn Fn(Node<'_>) -> bool) -> bool {
    use gravity_decompose::ops::window::{WinFn, WinFnKind as K};
    use gravity_decompose::catalog::typefacts::{row_of, FIRST_NORMAL_OID};
    const ANYARRAYOID: u32 = 2277;
    let no_offset = d.plan.startOffset.is_none() && d.plan.endOffset.is_none();
    let scalar = |k: &gravity_decompose::ops::shared::sort::EqKey, coll: Option<&u32>| {
        k.typid != ANYARRAYOID && !gravity_decompose::catalog::anonymous_record(k.typid) && k.opno < FIRST_NORMAL_OID
            && row_of(k.typid).is_some_and(|t| !t.collatable || coll.is_some_and(|&c| c != 0))
    };
    let keys = d.partition.iter().enumerate().all(|(i, k)| scalar(k, d.plan.partCollations.get(i)))
        && d.order.iter().enumerate().all(|(i, k)| scalar(k, d.plan.ordCollations.get(i)))
        && d.plan.partOperators.iter().chain(d.plan.ordOperators.iter()).all(|&o| o < FIRST_NORMAL_OID);
    let quiet_fn = |f: &WinFn<'_>| match f.kind {
        K::RowNumber | K::Rank | K::DenseRank | K::PercentRank | K::CumeDist => true,
        K::Aggregate => {
            let typmod = || f.func.args.iter().next().is_some_and(gravity_decompose::effects::num_bounded); // AGC-23: the digit bound, not a bare typmod'd column
            f.func.winagg && gravity_decompose::effects::agg_oid_quiet(f.func.winfnoid, typmod) && f.func.args.iter().all(q) && f.func.aggfilter.is_none_or(q)
        }
        _ => false,
    };
    no_offset && keys && d.run_condition.iter().all(q) && d.functions.iter().all(quiet_fn)
}

/// aggtrans T6c: the pipeline feeding pipeline `k`'s Breaker when it ends in a Sort PostgreSQL runs incrementally (presorted
/// keys: an Incremental Sort, no bound) over a table or index scan's rows and no group drains it. PostgreSQL streams the
/// scan through it group by group, so a fold ahead of its order runs that pipeline's whole input ahead too: it is a
/// `fold_ahead` feed (its ops, its scan, the Sort's comparators).
pub(crate) fn incremental_feed(gx: &Gx<'_, '_>, k: usize) -> Option<usize> {
    use gravity_decompose::{ops::shared::sort::SortForm, Source};
    let Source::Breaker { slot } = &gx.pipelines()[k].source else { return None };
    let q = gx.life_of(k, *slot)?.0 as usize;
    let x = gx.pipelines().get(q)?;
    let sort = matches!(x.ops.last(), Some(Op::Sort(s)) if s.presorted > 0 && matches!(s.form, SortForm::Runs));
    (sort && gx.drained_group(q).is_none() && matches!(x.source, Source::Range { .. } | Source::Index { .. } | Source::Const)).then_some(q)
}

/// D3637: spooled drain `d` (probe `k`) hands its drained records on in record order where PostgreSQL goes
/// bucket by bucket, so an end folding them with a money or interval sum or average (`visible::drain::sums_end`: the end
/// is order-free but for those) adds another order's partial sums: the value is the multiset's in any order, an overflow
/// that order's. Each such call folds under the batch's recheck licence (`fold_ahead`, the drain and its end its feeds:
/// an overflow before any row left re-runs the statement in plan order). Where no licence is granted (the plan-order run,
/// a writing statement) it folds in the spool's order when PostgreSQL reads the group whole (`PipelineGraph::whole`):
/// every row then reaches PostgreSQL's fold too, and an overflow is the answer of some order of them, as a sorted group's
/// ties (ops/agg/trans/hosted.rs `own_order`, D3374). True when the drain has no such end; false = A2END.
pub(crate) fn drain_sums(px: &mut crate::frame::px::Px<'_, '_>, k: usize, d: usize) -> bool {
    use gravity_decompose::effects::{denying, denying_bounded};
    use gravity_decompose::ops::shared::agg_class::Class;
    let gx = px.gx;
    let Some((e, g)) = crate::effects::visible::drain::sums_end(gx, d) else { return true };
    let feeds: PgVec<'_, (usize, usize)> = [k, d, e].iter().enumerate().filter(|&(i, q)| ![k, d, e][..i].contains(q)).map(|(_, &q)| (q, 0)).collect_in(gx.mcx);
    let sums = || g.aggs.iter().filter(|a| matches!(a.class, Class::SumMoney | Class::SumInterval | Class::AvgInterval));
    if sums().all(|a| fold_ahead(px, core::slice::from_ref(a), &feeds[..]).is_some()) {
        return true;
    }
    let writes = gx.pipelines().iter().any(|p| p.ops.iter().any(|o| matches!(o, Op::Write(_))));
    (denying() && !denying_bounded() || writes) && gx.graph.whole.contains(&g.node_id)
}

/// aggtrans T6 (the batch re-run adopted; errorhold's licence): a grouping folding ahead of PostgreSQL's order (a sorted
/// group's keyed end folds every group's rows before it hands the first on, a host-sorted aggregate's transitions run at
/// its group's final, a final runs before another aggregate's sorted transitions). The batch: each call of `aggs`
/// (function, arguments, FILTER, direct arguments, a user aggregate's support functions: `of_expr`) with its own ORDER BY
/// / DISTINCT sort, and the ops up to `n` of each feed `(k, n)`; an aggregate reaching user code PostgreSQL calls by type
/// (its sorts' support functions, `node_ok`; an argument type not `type_free`: a comparator, an output function) acts.
/// Construct C2: the licence the batch folds ahead under. Denied (`None`): the caller refuses by name. Code that may
/// raise is granted with a recheck marking the feeds: an
/// error before any row left re-runs the statement in plan order (exec `runner::recheck`); one after it is raised
/// finalizing a group PostgreSQL reaches too (D439's pull cap, which the caller declares), by calls it makes there as
/// well, so it raises there too (`DECISIONS.md` 4 (D)). aggtrans T6b: a feed's index scan streams its rows in PostgreSQL
/// too, so the scan's own calls run ahead with the batch: its operator classes' support and its key operators
/// (`of_index`: unknown, so acting, for a user one), its key values and its recheck quals. T6c: a feed's Sort counts its
/// keys' comparators (an incremental Sort's input pipeline is a feed: PG streams it). T6d / T6e: `type_free` argument
/// types; the aggregate's own sorts and a feed's are read by the functions PG resolves for them, a DISPATCH comparator
/// (btarraycmp, btrecordcmp, range_cmp) by its components through the type cache (`sort_free`).
pub(crate) fn fold_ahead(px: &mut crate::frame::px::Px<'_, '_>, aggs: &[gravity_decompose::ops::shared::agg::AggDecl<'_>], feeds: &[(usize, usize)]) -> Option<gravity_decompose::effects::Licence> {
    use gravity_decompose::effects::{licence, of_expr, of_ops, of_source, Cx, Departure, Effects, PLAIN};
    let (gx, arena) = (px.gx, px.arena);
    let typed = |d: &gravity_decompose::ops::shared::agg::AggDecl<'_>| node_ok(gx.catalog, true, d.node) && d.node.as_aggref().is_some_and(|r| arg_values(r).all(|e| value_free(gx.catalog, e, 0)));
    // group lane, USEROP #530 (kit K1.idxq): the statement's root pipeline seeks its index once, where PostgreSQL's first
    // fetch pulls the plan's outer chain down to that scan; not with a hash probe in it (PostgreSQL may build first and never
    // pull the outer) or a one-time gate (PostgreSQL then pulls nothing below it). A merge inner, a union arm, a nest inner or
    // a pipeline under a breaker may never be pulled by PostgreSQL (an empty outer, a LIMIT above), so it is not `once`
    let once = |k: usize| {
        let probe_or_gate = |o: &Op<'_>| matches!(o, Op::Join(gravity_decompose::join::JoinDesc::Probe { .. }) | Op::Gate(_));
        k == gx.graph.root as usize && !gx.pipelines()[k].ops.iter().any(probe_or_gate)
    };
    let scan = |k: usize| match &gx.pipelines()[k].source {
        s @ gravity_decompose::graph::Source::Index { .. } => user_btree_scan(gx, arena, s, once(k)).unwrap_or_else(|| of_source(&Cx::without_stmt(gx.facts, arena), s, &PLAIN)),
        _ => Effects::NONE,
    };
    // a feed's Sort (T6c: an incremental one PG streams) calls its keys' comparators (`sort_compare`)
    let op = |o: &Op<'_>| match o {
        Op::Sort(s) if matches!(s.form, gravity_decompose::ops::shared::sort::SortForm::Runs) => sort_compare(gx, s),
        o => of_ops(&Cx::without_stmt(gx.facts, arena), core::slice::from_ref(o), &PLAIN),
    };
    // aggtrans T6j: a merge join's inner is a run its producer spooled whole before the probe's first row (decompose
    // sort::spool_run) where PG reads it as the merge asks: that pipeline's work runs ahead too, a feed of the batch
    let mut feeds: PgVec<'_, (usize, usize)> = mcx::to_vec_in(feeds, gx.mcx);
    let mut i = 0;
    while i < feeds.len() {
        let (k, n) = feeds[i];
        for o in &gx.pipelines()[k].ops[..n] {
            let Op::Join(gravity_decompose::join::JoinDesc::Merge { slot, .. }) = o else { continue };
            let Some(q) = gx.life_of(k, *slot).map(|l| l.0 as usize) else { return None };
            if !feeds.iter().any(|f| f.0 == q) {
                feeds.push_in((q, gx.pipelines()[q].ops.len()));
            }
        }
        i += 1;
    }
    let ops = feeds.iter().fold(Effects::NONE, |e, &(k, n)| gx.pipelines()[k].ops[..n].iter().fold(e.and(scan(k)), |e, o| e.and(op(o))));
    let call = |d: &gravity_decompose::ops::shared::agg::AggDecl<'_>| of_expr(&Cx::without_stmt(gx.facts, arena), d.node, &PLAIN);
    let own = aggs.iter().fold(ops, |e, d| e.and(call(d)).and(if typed(d) { Effects::NONE } else { Effects::ACTS }));
    let Ok(l) = licence(px.batch_effects(own), Departure::Ahead) else { return None };
    // construct C2 (b): asked with every feed's effects, so each feed holds it for the run-ahead tracer too
    for &(k, _) in &feeds {
        px.licensed(k, &l);
        px.hold(k, &l);
    }
    Some(l)
}

/// T6c (`fold_ahead`), D2117 (ops/subplan/per_row.rs `feed_effects`): the calls full sort `s` makes comparing its rows, its
/// keys' comparators: a default btree family's over a type the builtin table knows (`of_compare`), else built-in ones that
/// may raise (`sort_free`); else unknown. Its target list's expressions are not read here (a projection op computes them).
pub(crate) fn sort_compare(gx: &Gx<'_, '_>, s: &gravity_decompose::ops::shared::sort::SortDesc<'_>) -> gravity_decompose::effects::Effects {
    use gravity_decompose::effects::Effects;
    s.keys.iter().fold(Effects::NONE, |e, x| {
        let t = s.tlist.iter().filter_map(|n| n.as_target_entry()).find(|t| t.resno == x.resno).and_then(|t| type_of(t.expr));
        e.and(match t.filter(|_| gx.facts.key(gravity_decompose::catalog::KeyReq::sort(x.opno)).is_some_and(|o| o.default_family)).map(|t| (t, gravity_decompose::effects::of_compare(gx.facts, t))) {
            Some((t, c)) if c.acts && sort_free(gx.catalog, x.opno, t) => Effects::RAISES,
            Some((_, c)) => c,
            None => Effects::UNKNOWN,
        })
    })
}

/// group lane, USEROP #530 (kit K1.idxeqc / K1.idxq): the calls btree index scan `s` of a heap relation makes per row when
/// an operator class of its index is a user's (`of_index` reads the whole index as unknown, so acting, there): with no key
/// at all, none (a full walk of the leaf pages calls no support function; btree skip scan runs only for an omitted
/// leading column under a later key); with keys `column op value` on a leading prefix of the index's columns (no column
/// skipped), each key's operator function (`_bt_checkkeys`) and the btree FUNCTION 1 of the user operator's family
/// (`_bt_first`'s descent: the catalog's `ordering_cmp_proc`), each read by its volatility (`of_call`: a user comparator
/// declared IMMUTABLE may raise; a VOLATILE one acts), plus the key values and recheck quals. `None` (the caller's
/// `of_index` answer stands) for any other scan: a built-in operator in a user family (its family is not the operator's
/// only one), a non-ordering key operator, a row compare, ANY, a NULL test, an expression column.
/// Kit K1.idxq (#530): when the scan seeks `once` (the root pipeline's source, `fold_ahead`), FUNCTION 1 is left out. nbtree calls
/// it only in `_bt_first` (the descent's insertion scan key; `_bt_preprocess_keys` compares keys there too), at the first
/// `index_getnext` after the seek's `index_rescan`, before any row. The source seeks with those AM calls at its first pull
/// (hosts ops/scan/index_source.rs `seek`), where PostgreSQL's IndexScan first pulls: a raise there comes before any row in
/// both, and never without a pull (LIMIT 0, a cursor never fetched). Any key operator of the family serves then (an index
/// qual's operator is a member of its column's family; `=`, `<=` and `>=` keys have no `ordering_cmp_proc`). A re-seek per
/// outer row (a rerun set), or a seek PostgreSQL may never make (a merge inner, a union arm, a pipeline under a breaker),
/// keeps FUNCTION 1 and the ordering-operator limit.
fn user_btree_scan(gx: &Gx<'_, '_>, arena: gravity_program::ArenaRef, s: &gravity_decompose::graph::Source<'_>, once: bool) -> Option<gravity_decompose::effects::Effects> {
    use gravity_decompose::effects::{of_call, of_expr, of_list, Cx, Effects, PLAIN};
    let gravity_decompose::graph::Source::Index { am: gravity_decompose::catalog::Am::Heap, iam: IndexAm::Btree, indexid, keys, recheck, .. } = s else { return None };
    if gx.facts.index_opc(*indexid).all() {
        return None; // D2063's facts answer it as built in: `of_index` reads it
    }
    let cx = Cx::without_stmt(gx.facts, arena);
    let prefix = keys.iter().all(|k| (1..k.col).all(|c| keys.iter().any(|x| x.col == c)));
    let mut e = of_list(&cx, recheck, &PLAIN);
    for k in keys.iter() {
        let code = gx.catalog.opcode(k.opno).ok().filter(|&f| f != 0)?;
        if k.form != KeyForm::Op || k.opno < FIRST_NORMAL_OID || k.attno <= 0 || !prefix {
            return None;
        }
        let seek = if once { Effects::NONE } else { of_call(gx.facts, gx.catalog.ordering_cmp_proc(k.opno).ok().filter(|&f| f != 0)?) };
        e = e.and(of_call(gx.facts, code)).and(seek).and(of_expr(&cx, k.value, &PLAIN));
    }
    Some(e)
}

/// KeySem's group-key equality reads (`keysem::eq`) through the lowering's catalog: the live one, or the rig's fixture.
pub(crate) struct EqReads<'a, 'p>(pub &'a dyn Catalog<'p>);

impl gravity_fn::typesem::keysem::eq::EqReads for EqReads<'_, '_> {
    fn opcode(&self, op: u32) -> Option<u32> {
        self.0.opcode(op).ok().filter(|&f| f != 0)
    }
    fn type_eq_opr(&self, typid: u32) -> Option<u32> {
        self.0.type_eq_opr(typid).ok().filter(|&o| o != 0)
    }
    fn prolang(&self, proc: u32) -> Option<u32> {
        self.0.func(proc).ok().and_then(|f| f.prolang)
    }
}

/// aggtrans T6e: a sort by `op` over type `t` calls only built-in code: the family's support functions, a DISPATCH one's
/// components read through the type cache. Read at lowering: a later CREATE OPERATOR CLASS .. DEFAULT moves the front
/// epoch and retires the program (D2063; e2e corr_d1605_d2093_opclass_retire).
fn sort_free(c: &dyn Catalog<'_>, op: u32, t: u32) -> bool {
    key_ok(c, true, KeyUse::Sort, &Key { op, lefttype: t, righttype: t, family: 0, coll: 0 })
}

/// aggtrans T6f: the types of call `r`'s direct and aggregated arguments as PG passes them (a `record` parameter's value
/// keeps its row type, which `aggargtypes` does not show); 0 for one `type_of` cannot read.
pub(crate) fn arg_types<'a, 'p>(r: &'a Aggref<'p>) -> impl Iterator<Item = u32> + use<'a, 'p> {
    arg_values(r).map(|e| type_of(e).unwrap_or(0))
}

fn arg_values<'a, 'p>(r: &'a Aggref<'p>) -> impl Iterator<Item = Node<'p>> + use<'a, 'p> {
    let agg = r.args.iter().filter_map(|t| t.as_target_entry()).filter(|t| !t.resjunk).map(|t| t.expr);
    r.aggdirectargs.iter().chain(agg)
}

/// D3504: value `e` reaches no user code by type (`type_free`); a ROW(..) constructor's anonymous record names its columns
/// in the expression, and record_out, composite_to_json, btrecordcmp and hash_record call each column's own type's code,
/// so it reaches none when no column does (effects/visible/group.rs `user_sort` reads its sort the same way).
fn value_free(c: &dyn Catalog<'_>, e: Node<'_>, depth: u8) -> bool {
    crate::stack_check();
    match e.as_row_expr() {
        Some(r) if r.row_typeid == types_core::catalog::RECORDOID => depth < 8 && r.args.iter().all(|a| value_free(c, a, depth + 1)),
        Some(r) => type_free(c, r.row_typeid, depth),
        None => type_of(e).is_some_and(|t| type_free(c, t, depth)),
    }
}

/// aggtrans T6f: type `t`'s default btree comparator (the type cache's; a domain's is its base's) is built-in code, a
/// DISPATCH one's components read as `sort_free` reads them (record_larger, array_larger and a key sort compare by it).
pub(crate) fn cmp_free(c: &dyn Catalog<'_>, t: u32) -> bool {
    c.type_cmp_proc(t).is_ok_and(|f| built_in(c, true, f, t, 0))
}

/// aggtrans T6d / T6e: a value of type `t` reaches no user code by type (output, comparison, hash: record_out / btrecordcmp
/// / hash_record call each column's, array_out / btarraycmp / hash_array the element's): a built-in type, an enum, a domain
/// over one, an array of one, a composite of them (dropped columns skipped), at most 8 levels deep.
pub(crate) fn type_free(c: &dyn Catalog<'_>, t: u32, depth: u8) -> bool {
    crate::stack_check();
    let Some((b, f)) = c.base_type(t, &mut -1).ok().and_then(|b| Some((b, c.type_facts(b).ok()?))).filter(|_| depth < 8) else { return false };
    let field = |a: i16| c.support(Support::Field { typid: b, attno: a }).is_ok_and(|ft| ft == 0 || type_free(c, ft, depth + 1));
    match f.typtype as u8 {
        _ if f.elem != 0 => type_free(c, f.elem, depth + 1),
        b'e' => true,
        b'c' => c.support(Support::Natts(b)).is_ok_and(|n| n > 0 && (1..=n.min(1600) as i16).all(field)),
        b'b' | b'r' | b'm' => b < FIRST_NORMAL_OID,
        _ => false,
    }
}

/// aggtrans overfit OIDW-6 / OIDW-7: a value of type `t` names no columns or elements of its own: a pseudo-type (`record`,
/// `anyarray`: the catalog's typtype) or an array of one (`record[]`). Its comparison and output functions call each
/// member's, of a type only the value holds (a user's, maybe). A type the catalog cannot read is one (fail closed).
pub(crate) fn rowless(c: &dyn Catalog<'_>, t: u32) -> bool {
    let pseudo = |typtype: i8| typtype == b'p' as i8;
    c.type_facts(t).map_or(true, |f| pseudo(f.typtype) || (f.elem != 0 && c.type_facts(f.elem).map_or(true, |e| pseudo(e.typtype))))
}

/// D2824 (`still`'s JSON arm): whether JSON collect `r` parses a json argument's text again per row: jsonb_agg (a jsonb
/// result: datum_to_jsonb reads JSONTYPE_JSON text with the json parser, a `\u0000` escape raises), not json_agg (a json
/// result: datum_to_json_internal appends that text as is).
pub(crate) fn json_reparsed(r: &Aggref<'_>) -> bool {
    r.aggtype != types_core::JSONOID
}

/// aggtrans overfit OIDW-7: a value of type `t` holds json text, which jsonb_agg parses again per row (a `\u0000` escape
/// raises): PostgreSQL's json_categorize_type JSONTYPE_JSON (through a domain: it reads the base type), an array of such
/// (array_to_json reads each element by its category) or, AGC-22, a composite with such a column (composite_to_jsonb reads
/// each column by its category; dropped columns skipped), at most 8 levels deep; a type the catalog cannot read is one
/// (fail closed).
pub(crate) fn json_text(c: &dyn Catalog<'_>, t: u32) -> bool {
    json_text_in(c, t, 0)
}

fn json_text_in(c: &dyn Catalog<'_>, t: u32, depth: u8) -> bool {
    crate::stack_check();
    let Some((b, f)) = c.base_type(t, &mut -1).ok().and_then(|b| Some((b, c.type_facts(b).ok()?))).filter(|_| depth < 8) else { return true };
    let field = |a: i16| c.support(Support::Field { typid: b, attno: a }).map_or(true, |ft| ft != 0 && json_text_in(c, ft, depth + 1));
    match f.typtype as u8 {
        _ if b == types_core::JSONOID => true,
        _ if f.elem != 0 => json_text_in(c, f.elem, depth + 1),
        b'c' => c.support(Support::Natts(b)).map_or(true, |n| (1..=n.min(1600) as i16).any(field)),
        _ => false,
    }
}

/// aggtrans T6k (main's perf debt 20:20Z: ordered aggregates in D1289 groups at 1.3x / 2.1x rowexec; fold/hosted.rs
/// `arena_word`'s agg.arena bit, the effects home's read of the plan's order fields): whether group `g` feeds its host-sorted
/// aggregates in the order PostgreSQL's Agg reads them (G10's `fed_in_order`: the drain of the Sort PostgreSQL put under its
/// Agg, through projections alone, every pipeline width 1 under the serial arena) and one of them is aggpresorted:
/// PostgreSQL then runs its transfn per row in that order, so the host advances it per row too (rows tied on every Sort key
/// in an order PostgreSQL leaves free, as G10's riders and D1428's final sort). An ORDER BY aggregate PostgreSQL sorts
/// itself (not aggpresorted) keeps its run and final sort. (b): with no such feed, a group whose every aggpresorted ORDER BY
/// aggregate orders by its group key columns alone (`on_keys`): all of a group's rows tie on it, so whatever order they
/// come in is a tie order. Every aggpresorted ORDER BY aggregate of `g` must be `still` (the caller's: #530's array or
/// string collect, JSON over built-in types but json, no user code by type or comparator): its transitions raise and act
/// on nothing, so running them as the rows come, also in groups PG's GroupAggregate never reaches (a LIMIT above, a later
/// group's raise: the keyed table reads every row before its first group), shows nothing PG does not; a user or raising
/// one keeps the final-time fold (D1371), which runs only for the groups the emit hands on. (c): or the group reads
/// PostgreSQL's own scan order with no Sort between (hosted.rs `scan_order`: a btree index scan, ties in heap TID order),
/// the order PostgreSQL's Agg reads and presorted by. A float sum or average (`SumF32`, `SumF64`, `AvgF64`: float4pl /
/// float8pl / float8_accum, whose only raise is the out-of-range check of the row's result) and an array_agg over arrays
/// (`ArrayOfArrays`: array_agg_array_transfn, whose raises are a null, an empty or a mismatched array of the row, built-in
/// code over the input's images alone) ride too, in a plain group fed in PostgreSQL's order ((a) or (c)) with no Limit in
/// the program: PostgreSQL's Agg reads every row of its one group before its one output row, so its transfn raises at the
/// same row as the host's; only a Limit over the Agg (LIMIT 0 asks it for no row) leaves PostgreSQL's rows unread.
pub(crate) fn ordered_fed(gx: &Gx<'_, '_>, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>, still: &dyn Fn(&gravity_decompose::ops::shared::agg::AggDecl<'_>) -> bool) -> bool {
    use gravity_decompose::ops::shared::agg::{AggDecl, GroupForm};
    use gravity_decompose::ops::shared::agg_class::{Class, Collect};
    let presorted = |d: &&AggDecl<'_>| crate::ops::shared::agg::sort_cols(d) && d.node.as_aggref().is_some_and(|r| r.aggpresorted);
    let form = matches!(g.form, GroupForm::Plain | GroupForm::Sorted) && g.set.is_none() && g.two_pass.is_none() && g.hints.iter().all(Option::is_none);
    let in_order = crate::ops::group::regroup::fed_in_order(gx.graph, g) || form && crate::ops::agg::trans::hosted::scan_order(gx, g);
    let once = g.form == GroupForm::Plain && in_order && !gx.pipelines().iter().any(|p| crate::frame::iface::holds_limit(&p.ops));
    let rides = |d: &AggDecl<'_>| still(d) || once && matches!(d.class, Class::SumF32 | Class::SumF64 | Class::AvgF64 | Class::Collect(Collect::ArrayOfArrays));
    let mut fed = g.aggs.iter().filter(presorted).peekable();
    fed.peek().is_some() && g.aggs.iter().filter(presorted).all(rides) && (in_order || form && fed.all(|d| on_keys(gx.facts, g, d)))
}

/// D3374 (D1371's presorted user aggregates, T6k (d)): whether group `g` advances its aggpresorted ORDER BY aggregates
/// per row whatever their transitions do (a user transfn's NOTICE or write, a raise). PostgreSQL's Agg reads its rows
/// once (no Limit in the program) from the drain of the full Sort it put under it (presorted.rs `full_feed`: that Sort
/// reads its whole input first; an incremental one reads it between the Agg's rows), so every row meets its per-row
/// transition (ExecBuildAggTrans: each aggregate's FILTER, arguments and transfn in aggno order; a presorted call's ORDER BY
/// columns are not evaluated), as the end or the stream steps its folds per row (trans/plain.rs `lower`; the host's
/// `fed_row` calls the transfn at once). The calls, NOTICEs, writes and first error are then PostgreSQL's for the same rows.
/// A plain group runs each final after the last row in aggno order, as PostgreSQL; a streamed sorted group (D1071) hands
/// each group on, its finals, HAVING and projection, when the next group's first row comes, as agg_retrieve_direct (a
/// keyed end that does not stream runs every row's transitions before its first group leaves, which a cursor's FETCH n
/// would see: it keeps D1371's final-time fold). Every host-sorted aggregate must be such a call (none sorts and folds at its final, D1371's order
/// rule; no DISTINCT, D1596's), its ORDER BY's resjunk columns `calm` (the end evaluates them per row, PostgreSQL does
/// not) and every projection between the Sort's drain and the end `calm` (it runs ahead of the row's transitions). Rows
/// tied on every Sort key come in an order PostgreSQL leaves free (DECISIONS 5).
pub(crate) fn ordered_fed_acting(gx: &Gx<'_, '_>, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>) -> bool {
    use gravity_decompose::ops::shared::agg::{AggDecl, GroupForm};
    let calm = |n: Node<'_>| raise_free(gx, n) && !user_or_volatile(gx, n) && !user_typed(gx, n);
    let fed = |d: &AggDecl<'_>| d.mods.distinct == 0 && crate::ops::shared::agg::sort_cols(d) && d.node.as_aggref().is_some_and(|r| r.aggpresorted);
    let junk = |d: &AggDecl<'_>| d.node.as_aggref().is_some_and(|r| r.args.iter().filter_map(|t| t.as_target_entry()).filter(|t| t.resjunk).all(|t| calm(t.expr)));
    let (ps, k) = (gx.pipelines(), g.producer.0 as usize);
    let stream = gx.streams().position(|s| core::ptr::eq(s, g));
    let form = matches!(g.form, GroupForm::Plain | GroupForm::Sorted) && g.set.is_none() && g.two_pass.is_none() && g.hints.iter().all(Option::is_none);
    // D1371 (v0.4 refusals): or every fed call orders by its group's key columns or by constants alone (`on_keys`,
    // `const_order`): all of a group's rows tie on its ORDER BY, so any order they come in is a tie order (DECISIONS 5)
    // and PostgreSQL, which needs no sort for it (aggpresorted), advances it per row as they come
    let ties = form && g.aggs.iter().filter(|d| fed(d)).all(|d| on_keys(gx.facts, g, d) || const_order(d));
    // D1371 (v0.4 refusals): or a CteScan over a CTE whose store fills from a full Sort's drain (PG 17+ hands the CTE's
    // pathkeys up, so PostgreSQL presorted the call by the CTE's own Sort): the reader's pull (ops/ctescan, one row per
    // pull where the CTE calls user or VOLATILE code) runs the CTE's per-row target list as PostgreSQL's CteScan does,
    // one row ahead of that row's transitions
    let cte = || match ps.get(k).map(|p| &p.source) {
        Some(gravity_decompose::Source::Breaker { slot }) => gx.life_of(k, *slot).map(|l| l.0 as usize).is_some_and(|o| o < gx.graph.pipelines.len() && gx.spool_kind(o) == crate::face::kinds::KindId::NestCte && crate::ops::group::presorted::sorted_fill(gx.graph, o)),
        _ => false,
    };
    let once = form && (crate::ops::group::presorted::full_feed(gx.graph, g) || ties || cte()) && !ps.iter().any(|p| crate::frame::iface::holds_limit(&p.ops));
    let upto = |p: &gravity_decompose::PipelineNode<'_>| stream.and_then(|i| p.ops.iter().position(|o| matches!(o, Op::GroupRun(r) if *r as usize == i))).unwrap_or(p.ops.len());
    // a filter there (`full_feed`: a SubqueryScan's qual over the ordered subquery) is PostgreSQL's per-row qual below the
    // Agg, evaluated per row before the row's transitions as the pipeline does, whatever it calls
    // (and, in the tie case, a FROM function's rows: PG's FunctionScan reads the call whole at its first fetch, a
    // calm call shows nothing there)
    let srf = |d: &gravity_decompose::ops::shared::srf::SrfDesc<'_>| {
        matches!(d.place, gravity_decompose::ops::shared::srf::SrfPlace::From { .. }) && d.functions.iter().all(|f| calm(f.node)) && d.tlist.iter().all(calm)
    };
    let ops = ps.get(k).is_some_and(|p| p.ops[..upto(p)].iter().all(|o| match o {
        Op::Project(l) => l.iter().all(calm),
        Op::Filter(_) => true,
        Op::Srf(d) => ties && srf(d),
        _ => false,
    }));
    let held = g.form == GroupForm::Plain || stream.is_some();
    // D1371 (v0.4 refusals): beside the fed calls, an ORDER BY or DISTINCT aggregate PostgreSQL sorts itself (not
    // aggpresorted) keeps its run and folds at the final, where PostgreSQL's finalize_aggregates runs its
    // process_ordered_aggregate_* (folds_ahead puts every sorted fold before the finals where a final may act); per row
    // PostgreSQL only evaluates its arguments and FILTER into its sort, which must then call no user or VOLATILE code
    // (their calls would interleave with the fed transfns' in aggno order)
    let still = |n: Node<'_>| !user_or_volatile(gx, n) && !user_typed(gx, n) && !gravity_decompose::expr::has_subplan(n);
    let own = |d: &AggDecl<'_>| !fed(d) && d.node.as_aggref().is_some_and(|r| r.args.iter().chain(r.aggdirectargs.iter()).all(still) && r.aggfilter.is_none_or(still));
    once && ops && held && g.aggs.iter().any(fed) && g.aggs.iter().filter(|d| crate::ops::shared::agg::sorted(d)).all(|d| fed(d) && junk(d) || own(d))
}

/// D3374 (an acting window argument SubPlan, decompose subplan.rs `attach_window_args`): whether the one acting SubPlan
/// on fill `fill` of the window reader `q` keeps PostgreSQL 18.6's calls. Decompose placed it there because PostgreSQL
/// evaluates its argument once per stored row in the buffer's order (an aggregate with no FILTER whose frame head never
/// moves, every row read: `frame_reads_each_once`); the fill evaluates it per row in that order on the leader (the caller
/// makes the fill serial). The calls are PostgreSQL's when nothing else the statement runs among them acts or raises:
/// - every row is read: no Limit in the program, `q` not an InitPlan's or a rerun set's, its rows handed to the statement's
///   sink or to a full Sort drained there (the caller serves the program only to a reader that takes every row);
/// - the fill's source hands rows on quietly: a breaker's run (what fed it ran whole before), or a scan whose per-row calls
///   and its producers' show nothing; its other ops and every argument cell outside a fill SubPlan are calm
///   (`calm_outside`); the window's functions, keys and run condition quiet (`window_quiet`); `q`'s other ops quiet;
/// - every other pipeline (outside the fill's feeders when its source is a breaker, the SubPlan's own rerun set and a
///   Sort's drain past `q`) runs nothing with effects (`pipeline_effects`).
/// Rows tied on every key come in an order PostgreSQL leaves free (DECISIONS 5), as for D1371's presorted calls. The
/// first reason it fails is a refusal name.
pub(crate) fn window_fill_exact(gx: &Gx<'_, '_>, arena: gravity_program::ArenaRef, q: usize, fill: usize) -> Result<(), &'static str> {
    use gravity_abi::breaker::BreakerKind;
    use gravity_decompose::effects::{of_source, Cx, Effects, PLAIN};
    use gravity_decompose::ops::shared::subplan::{SubForm, SubSite};
    use gravity_decompose::Source;
    use crate::graph::EndKind;
    const WHY: &str = "an acting window argument SubPlan beside code of the statement that acts or may raise, or under a reader that may stop before the last row (D3374)";
    let quiet = |e: Effects| !e.acts && !e.may_raise;
    let ps = gx.pipelines();
    let (Some(fp), Some(qp)) = (ps.get(fill), ps.get(q)) else { return Err(WHY) };
    let subs = || fp.ops.iter().filter_map(|o| if let Op::SubPlan(d) = o { Some(d) } else { None });
    let mut acting = subs().filter(|d| d.effects().acts);
    let (Some(s), None) = (acting.next(), acting.next()) else { return Err(WHY) };
    if s.site != SubSite::Row || s.conditional || !matches!(s.form, SubForm::Nested { .. }) {
        return Err(WHY);
    }
    let id = s.plan_id;
    let placed = |pid: i32| subs().any(|d| d.plan_id == pid);
    let cx = Cx::without_stmt(gx.facts, arena);
    let rerun = |x: usize| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(x as u16)));
    let init = |x: usize| gx.graph.init_plans.iter().any(|i| i.pipeline as usize == x);
    let sink = |x: usize| matches!(gx.end_kind(x), EndKind::Sink) && !init(x) && !rerun(x);
    let drain = (q + 1..ps.len()).find(|&r| matches!(ps[r].source, Source::Breaker { slot } if ps[q].desc.produces() == Some(slot)));
    let pulled = !init(q) && !rerun(q) && !rerun(fill) && match gx.end_kind(q) {
        EndKind::Sink => sink(q),
        EndKind::Breaker(BreakerKind::Sort) => drain.is_some_and(sink) && !gx.cte_write(q),
        _ => false,
    };
    if !pulled || ps.iter().any(|p| crate::frame::iface::holds_limit(&p.ops)) {
        return Err(WHY);
    }
    // the fill: its source, its other ops and SubPlans
    let breaker = matches!(fp.source, Source::Breaker { .. });
    let ops = fp.ops.iter().all(|o| match o {
        Op::SubPlan(d) => d.plan_id == id || quiet(d.effects()),
        o => quiet(op_effects(gx, &cx, o)),
    });
    if !(breaker || quiet(of_source(&cx, &fp.source, &PLAIN))) || !ops {
        return Err(WHY);
    }
    // the window op, its argument cells (the fill's end evaluates them) and the reader's other ops
    let Some(d) = qp.ops.iter().find_map(|o| if let Op::Window(w) = o { Some(w) } else { None }) else { return Err(WHY) };
    let calm = |n: Node<'_>| gx.fx(n).calm_outside(arena, &placed);
    let reader = qp.ops.iter().all(|o| match o {
        Op::Window(_) => true,
        Op::SubPlan(x) => quiet(x.effects()),
        o => quiet(op_effects(gx, &cx, o)),
    });
    if d.volatile_args || d.volatile_proj || !window_quiet(d, &calm) || !reader {
        return Err(WHY);
    }
    // every other pipeline: the fill's feeders ran whole before its first row when its source is a breaker (else they
    // are read too), the SubPlan's own rerun set is its evaluation, a Sort's drain runs after the last call
    let mut feeds: PgVec<'_, usize> = PgVec::new_in(gx.mcx);
    if breaker {
        let mut todo = producers(gx, fill);
        while let Some(x) = todo.pop() {
            if !feeds.contains(&x) {
                feeds.push_in(x);
                todo.extend_in(producers(gx, x));
            }
        }
    }
    let own = |x: usize| gx.graph.reruns.iter().any(|r| r.plan_id == id && r.pipelines.contains(&(x as u16)));
    let others = (0..ps.len()).filter(|&x| x != fill && x != q && Some(x) != drain && !feeds.contains(&x) && !own(x));
    for x in others {
        if !quiet(pipeline_effects(gx, arena, x, false)) {
            return Err(WHY);
        }
    }
    Ok(())
}

/// D2117 / D3374: what graph pipeline `x` runs, through its producers when `follow`: its source's per-row calls (none
/// for a breaker's run), its ops (a full Sort by its compares and target list), its end (a sink, a full Sort, a plain
/// aggregate or a Materialize show nothing; any other end is unknown) and its group's calls (a keyed one unknown).
pub(crate) fn pipeline_effects(gx: &Gx<'_, '_>, arena: gravity_program::ArenaRef, x: usize, follow: bool) -> gravity_decompose::effects::Effects {
    use gravity_abi::breaker::BreakerKind;
    use gravity_decompose::effects::{of_expr, of_source, Cx, Effects, PLAIN};
    use gravity_decompose::Source;
    let cx = Cx::without_stmt(gx.facts, arena);
    let (mut todo, mut seen, mut e) = (mcx::pg_vec![in gx.mcx; x], PgVec::new_in(gx.mcx), Effects::NONE);
    while let Some(q) = todo.pop() {
        if seen.contains(&q) {
            continue;
        }
        seen.push_in(q);
        let p = &gx.pipelines()[q];
        e = e.and(if let Source::Breaker { .. } = p.source { Effects::NONE } else { of_source(&cx, &p.source, &PLAIN) });
        let full = |o: &Op<'_>| matches!(o, Op::Sort(s) if matches!(s.form, gravity_decompose::ops::shared::sort::SortForm::Runs));
        e = p.ops.iter().fold(e, |e, o| e.and(op_effects(gx, &cx, o)));
        let quiet_end = match gx.end_kind(q) {
            crate::graph::EndKind::Breaker(BreakerKind::Sort) => p.ops.iter().any(full),
            crate::graph::EndKind::Breaker(b) => matches!(b, BreakerKind::PlainAgg | BreakerKind::Materialize),
            crate::graph::EndKind::Sink => true,
            crate::graph::EndKind::RowOp(_) => false,
        };
        e = e.and(if quiet_end { Effects::NONE } else { Effects::UNKNOWN });
        if let Some(g) = gx.group_at(q) {
            e = e.and(if g.keys.is_empty() { Effects::NONE } else { Effects::UNKNOWN });
            for d in g.aggs.iter() {
                e = e.and(of_expr(&cx, d.node, &PLAIN)).and(d.filter.map_or(Effects::NONE, |f| of_expr(&cx, f, &PLAIN)));
            }
        }
        if follow {
            todo.extend_in(producers(gx, q));
        }
    }
    e
}

/// `pipeline_effects`' reading of one op: decompose's `of_ops` (filters, gates, projections, nested loops; any other op
/// unknown), a full Sort by its keys' comparators (`sort_compare`) and its target list read as code.
fn op_effects<'p>(gx: &Gx<'_, 'p>, cx: &gravity_decompose::effects::Cx<'_, 'p>, o: &Op<'p>) -> gravity_decompose::effects::Effects {
    use gravity_decompose::effects::{of_expr, of_ops, Effects, PLAIN};
    match o {
        // its target list too, read as code (a projection op before it computes what is not a column)
        Op::Sort(s) if matches!(s.form, gravity_decompose::ops::shared::sort::SortForm::Runs) => s.tlist.iter().fold(sort_compare(gx, s), |e, n| {
            e.and(n.as_target_entry().map_or(Effects::UNKNOWN, |t| of_expr(cx, t.expr, &PLAIN)))
        }),
        o => of_ops(cx, core::slice::from_ref(o), &PLAIN),
    }
}

/// D2117 / D3374: the pipelines whose rows pipeline `q` reads (each consumed slot's and its breaker source's producer,
/// `Gx::life_of`).
pub(crate) fn producers<'g>(gx: &Gx<'g, '_>, q: usize) -> PgVec<'g, usize> {
    let p = &gx.pipelines()[q];
    let src = if let gravity_decompose::Source::Breaker { slot } = p.source { Some(slot) } else { None };
    p.desc.consumes.iter().copied().chain(src).filter_map(|s| gx.life_of(q, s)).map(|l| l.0 as usize).collect_in(gx.mcx)
}

/// USEROP K10.ao (main's D1371 ruling 2026-10-04T17:10Z, case (a)): whether PG presorted host-sorted `d` of group `g`
/// (Aggref `aggpresorted`) through the Sort it put under the Agg, whose drain `g` reads (G10's `fed_in_order`). That Sort
/// called `d`'s comparator over the rows in both engines, adjacent pairs included, so the host's re-sort of each run at the
/// final adds calls only (count-free, 10-04) and its raises are the argument-order class the ruling frees.
pub(crate) fn resorts_presorted(gx: &Gx<'_, '_>, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>, d: &gravity_decompose::ops::shared::agg::AggDecl<'_>) -> bool {
    d.node.as_aggref().is_some_and(|r| r.aggpresorted) && crate::ops::group::regroup::fed_in_order(gx.graph, g)
}

/// D1371 (v0.4 refusals): whether every ORDER BY entry of `d` sorts a constant (`ORDER BY 1` over a VARIADIC array):
/// every row ties on it.
fn const_order(d: &gravity_decompose::ops::shared::agg::AggDecl<'_>) -> bool {
    let Some(r) = d.node.as_aggref() else { return false };
    let mut entries = r.aggorder.iter().map(|n| n.as_sort_group_clause());
    !r.aggorder.is_nil() && entries.all(|c| c.is_some_and(|c| {
        let t = r.args.iter().filter_map(|n| n.as_target_entry()).find(|t| t.ressortgroupref == c.tleSortGroupRef);
        t.is_some_and(|t| t.expr.as_relabel_type().map_or(t.expr, |x| x.arg).as_const().is_some())
    }))
}

/// T6k (b): whether every ORDER BY entry of `d` sorts a group key column of `g` (an `OUTER_VAR` its keys read, through
/// RelabelType) by its type's default btree ordering (`default_order`): the operator family of the key's equality, so rows of
/// one group are equal under it.
fn on_keys(facts: &dyn gravity_decompose::catalog::Facts, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>, d: &gravity_decompose::ops::shared::agg::AggDecl<'_>) -> bool {
    let Some(r) = d.node.as_aggref() else { return false };
    let key = |n: Node<'_>| {
        let n = n.as_relabel_type().map_or(n, |x| x.arg);
        n.as_var().is_some_and(|v| v.varno == types_nodes::primnodes::OUTER_VAR && v.varlevelsup == 0 && v.varattno > 0 && g.keys.iter().any(|k| k.resno == v.varattno))
    };
    let mut entries = r.aggorder.iter().map(|n| n.as_sort_group_clause());
    !r.aggorder.is_nil() && entries.all(|c| c.is_some_and(|c| {
        let t = r.args.iter().filter_map(|n| n.as_target_entry()).find(|t| t.ressortgroupref == c.tleSortGroupRef);
        t.is_some_and(|t| key(t.expr)) && gravity_decompose::facts::order::default_order(facts, c.sortop)
    }))
}

/// Whether every nest of pipeline `k` re-runs an inner that raises nothing (decompose F5's `raise_free` over the inner,
/// its computed index keys included). A nest re-runs its inner per outer row, so a raising inner (a key computed from the
/// outer row, D1016 / D2070) raises at the first outer row the walk reaches, which the m8_d1016 witnesses pin to
/// PostgreSQL's order: heap.pages' reversal (Decision 11) asks this. A filter or join qual that only may raise keeps the
/// reversal (which error is free, DECISIONS 4 (a)).
pub(crate) fn nests_raise_free(gx: &Gx<'_, '_>, k: usize) -> bool {
    use gravity_decompose::join::JoinDesc;
    gx.pipelines()[k].ops.iter().all(|op| !matches!(op, Op::Join(JoinDesc::Nest { facts, .. }) if !facts.raise_free))
}

/// D2662: whether heap.pages may walk pipeline `k`'s pages backwards (Decision 11) once `visible::order_visible` answered
/// `None`. A reversed walk evaluates the rows after PostgreSQL's first raising row, which PostgreSQL never reaches, so
/// which error is raised and which calls run follow the walk. The walk's own code (`build_class::side_by` over its ops,
/// and the group's aggregate arguments and FILTERs, read as side_by reads them: a parameter a nest or a per-row SubPlan
/// binds is a bound word) raising nothing reverses freely; code that may raise departs `Ahead` under a recheck licence
/// (`Px::run_ahead`: whole before any row leaves, nothing in it or feeding it is user code, the statement acts nowhere),
/// so an error re-runs the statement in plan order, whose walk is forward and raises PostgreSQL's first error. A user
/// function or a value of a user type never runs for a row PostgreSQL does not reach (a STABLE PL's NOTICE, its SPI
/// state: D2573): the walk keeps PostgreSQL's order and stops at its first raising row.
/// Construct C2: the licence the reversed walk holds (`effects::unobservable` for code raising nothing, `Px::run_ahead`'s
/// recheck licence for code that may raise); `None` keeps PostgreSQL's order.
pub(crate) fn walk_reversible(px: &mut crate::frame::px::Px<'_, '_>, k: usize) -> Option<gravity_decompose::effects::Licence> {
    use crate::ops::hashjoin::build_class::{quiet_statement, side_by, Side};
    let gx = px.gx;
    let (bound, quiet) = (|id: i32| gx.graph.params.iter().any(|x| x.0 == id), quiet_statement(gx));
    let rf = |n: Node<'_>| if quiet { raise_free_each(gx, n, &bound) } else { raise_free_by(gx, n, &bound, false) };
    let of = |n: Node<'_>| if user_fn(gx, n) || user_typed(gx, n) { Side::UserFn } else if rf(n) { Side::Safe } else { Side::Raises };
    let args = |d: &gravity_decompose::ops::shared::agg::AggDecl<'_>| {
        let r = d.node.as_aggref();
        let entries = r.into_iter().flat_map(|r| r.aggdirectargs.iter().chain(r.args.iter().map(|t| t.as_target_entry().map_or(t, |t| t.expr))));
        entries.chain(d.filter).fold(Side::Safe, |w, n| w.max(of(n)))
    };
    let side = gx.group_at(k).map_or(Side::Safe, |g| g.aggs.iter().fold(Side::Safe, |w, d| w.max(args(d))));
    match side.max(side_by(gx, k, usize::MAX, false)) {
        Side::Safe => gravity_decompose::effects::unobservable(gravity_decompose::effects::Effects::NONE),
        Side::Raises => px.run_ahead(k, gravity_decompose::effects::Effects::RAISES),
        Side::UserFn => None,
    }
}

/// D3378 (m8_d1018_kept_run K6 / K6b): graph pipeline `q` and every pipeline feeding it raise nothing and call nothing
/// VOLATILE or user-written: no group among them, and `side_by`'s Safe (PG 18.6's quiet reading expression by expression,
/// a sort of quiet keys too) in a statement whose user code every expression shows (`quiet_statement`). Run ahead of the
/// place PostgreSQL runs it, such a fill shows nothing: rerun/inner_once.rs `kept_run`'s run filled ahead of the owner,
/// and D2117's kept feeder ahead of a SubPlan's first evaluation (subplan/per_row.rs), as code with no effects departs
/// from nothing.
pub(crate) fn fill_quiet(gx: &crate::graph::Gx<'_, '_>, q: usize) -> bool {
    use crate::ops::hashjoin::build_class::{feed, quiet_statement, side_by, Side};
    use gravity_decompose::Source;
    let ps = gx.pipelines();
    if q >= ps.len() {
        return false;
    }
    let src = if let Source::Breaker { slot } = ps[q].source { Some(slot) } else { None };
    let mut group = feed(gx, q, ps[q].desc.consumes.iter().copied().chain(src));
    group.push_in(q);
    group.iter().all(|&x| x < ps.len() && gx.group_at(x).is_none()) && quiet_statement(gx) && side_by(gx, q, usize::MAX, false) == Side::Safe
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use gravity_decompose::{NoFacts, PipelineGraph};
    use gravity_expr::catalog::{DomainFacts, FuncFacts, IoFacts, Support, TextFacts, TypeFacts};
    use gravity_expr::Catalog;
    use gravity_fn::{Shim, Tier};
    use gravity_program::Arena;
    use mcx::{Mcx, MemoryContext};
    use types_core::Oid;
    use types_error::{PgError, PgResult};
    use types_nodes::list::{NodeList, OidList};
    use types_nodes::nodes_enums::LimitOption;
    use types_nodes::parsenodes::SortGroupClause;
    use types_nodes::plannodes::{
        Agg, GatherMerge, Group, Hash, HashJoin, IncrementalSort, Limit, Memoize, MergeAppend, MergeJoin, PartitionPruneInfo, PartitionPruneStepOp, PartitionedRelPruneInfo, Plan, RecursiveUnion, Scan, SeqScan, SetOp, Sort, Unique, WindowAgg,
    };
    use types_nodes::primnodes::{Aggref, BoolExpr, BoolExprType, OpExpr, ScalarArrayOpExpr, SubPlan, TargetEntry, Var, AGGKIND_HYPOTHETICAL, AGGKIND_NORMAL, AGGKIND_ORDERED_SET};
    use types_nodes::Node;

    use super::{cmp_free, key_ok, node_ok, sort_free, support, support_of, type_free, Key, KeyUse, DISPATCH_OK, USER_FN};
    use crate::graph::Gx;

    /// A catalog of tables (a unit test has no syscache): operators' oprcode, `Support` answers, type-cache rows and
    /// element types; anything else is an error (the guard refuses it).
    #[derive(Default)]
    struct Cat {
        opcode: HashMap<Oid, Oid>,
        support: HashMap<Support, Oid>,
        cmp: HashMap<Oid, Oid>,
        eq: HashMap<Oid, Oid>,
        elem: HashMap<Oid, Oid>,
        base: HashMap<Oid, Oid>,
        typtype: HashMap<Oid, u8>,
    }

    fn no(what: &str) -> Box<PgError> {
        Box::new(PgError::error(format!("test catalog: no {what}"))) // alloc: Cold
    }

    impl Catalog<'static> for Cat {
        fn func(&self, _: Oid) -> PgResult<FuncFacts> {
            Err(no("func"))
        }
        fn opcode(&self, o: Oid) -> PgResult<Oid> {
            self.opcode.get(&o).copied().ok_or_else(|| no("operator"))
        }
        fn op_input_types(&self, _: Oid) -> PgResult<(Oid, Oid)> {
            Err(no("op types"))
        }
        fn btree_cmp_proc(&self, _: Oid, _: Oid, _: Oid) -> PgResult<Oid> {
            Err(no("btree proc"))
        }
        fn opclass(&self, _: Oid) -> PgResult<(Oid, Oid)> {
            Err(no("opclass"))
        }
        fn type_facts(&self, t: Oid) -> PgResult<TypeFacts> {
            let typtype = self.typtype.get(&t).copied().unwrap_or(b'b') as i8;
            Ok(TypeFacts { len: -1, byval: false, align: b'i' as i8, typtype, collation: 0, elem: self.elem.get(&t).copied().unwrap_or(0) })
        }
        fn type_cmp_proc(&self, t: Oid) -> PgResult<Oid> {
            Ok(self.cmp.get(&t).copied().unwrap_or(0))
        }
        fn type_eq_opr(&self, t: Oid) -> PgResult<Oid> {
            Ok(self.eq.get(&t).copied().unwrap_or(0))
        }
        fn base_type(&self, t: Oid, _: &mut i32) -> PgResult<Oid> {
            Ok(self.base.get(&t).copied().unwrap_or(t))
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
        fn support(&self, q: Support) -> PgResult<Oid> {
            Ok(self.support.get(&q).copied().unwrap_or(0))
        }
    }

    // PG 18.6 oids: int4 < 97 / = 96 (btint4cmp 351, btint4sortsupport 3130, int4eq 65, hashint4 450); text < 664 / = 98
    // (bttextcmp 360, bttextsortsupport 3255, texteq 67, hashtext 400); enum < 3518 / = 3516 (enum_cmp 3514, enum_eq 3508,
    // hashenum 3515); box < 504 / = 503 (box_eq 128; box has no built-in btree family); anyrange < 3884 / = 3882 (range_cmp
    // 3870, range_sortsupport 6391, range_eq 3855, hash_range 3902); anyarray < 1072 / = 1070 (btarraycmp 382, array_eq 744,
    // hash_array 626); int4range 3904 (int4range_canonical 3914), int4multirange 4451, int4[] 1007, real[] 1021, json[] 199.
    // User objects from 16384 up: a plpgsql FUNCTION 1 16501 / 16601, a user btree family 16700, a user < 16600 and = 16602.
    fn cat() -> Cat {
        let mut c = Cat::default();
        c.opcode.extend([(96, 65), (98, 67), (3516, 3508), (503, 128), (3882, 3855), (1070, 744), (16602, 16603), (410, 467)]);
        let s = &mut c.support;
        for (op, f1, f2) in [(97, 351, 3130), (664, 360, 3255), (3518, 3514, 0), (504, 16501, 0), (3884, 3870, 6391), (1072, 382, 0), (16600, 16601, 0), (412, 0, 3136)] {
            s.insert(Support::Sort { op, num: 1 }, f1);
            s.insert(Support::Sort { op, num: 2 }, f2);
        }
        for (op, f) in [(96, 450), (98, 400), (3516, 3515), (3882, 3902), (1070, 626), (410, 0)] {
            s.insert(Support::Hash { op, rhs: false }, f);
            s.insert(Support::Hash { op, rhs: true }, f);
        }
        s.insert(Support::Hash { op: 16602, rhs: false }, 450);
        s.insert(Support::Hash { op: 16602, rhs: true }, 16604);
        s.extend([(Support::Merge { op: 96, family: 1976, num: 1 }, 351), (Support::Merge { op: 96, family: 1976, num: 2 }, 3130), (Support::Merge { op: 96, family: 16700, num: 1 }, 16701)]);
        s.extend([(Support::EqOfSort(97), 96), (Support::EqOfSort(504), 503), (Support::EqOfSort(16600), 16602), (Support::HashRhsEq(96), 96), (Support::HashRhsEq(3882), 3882)]);
        // the type cache, read once DISPATCH_OK: int4range over int4, a user range 16900 whose rngsubopc FUNCTION 1 is
        // 16601, int4multirange, a composite 16950 (int4, a dropped column, text), one 16960 over the user range
        s.extend([(Support::RangeCmp(3904), 351), (Support::RangeSub(3904), 23), (Support::RangeCanonical(3904), 3914), (Support::TypeHash { typid: 23, ext: false }, 450)]);
        s.extend([(Support::RangeCmp(16900), 16601), (Support::RangeSub(16900), 23), (Support::MultirangeRange(4451), 3904)]);
        s.extend([(Support::Natts(16950), 3), (Support::Field { typid: 16950, attno: 1 }, 23), (Support::Field { typid: 16950, attno: 3 }, 25), (Support::Natts(16960), 1), (Support::Field { typid: 16960, attno: 1 }, 16900)]);
        c.cmp.extend([(23, 351), (25, 360), (700, 356), (114, 16800), (16900, 3870), (16999, 382)]);
        c.eq.extend([(23, 96)]);
        c.elem.extend([(1007, 23), (1021, 700), (199, 114), (16999, 16999)]);
        c.base.extend([(16990, 23)]);
        c
    }

    fn key(op: Oid, t: Oid) -> Key {
        Key { op, lefttype: t, righttype: t, family: 0, coll: 0 }
    }

    fn ok(c: &Cat, d: bool, u: KeyUse, k: Key) -> bool {
        key_ok(c, d, u, &k)
    }

    fn mcx() -> Mcx<'static> {
        let cx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("guard user_fn"))); // alloc: Cold
        cx.mcx()
    }

    /// One target entry per type in `ts` (a key's type is its input's column).
    fn tlist(m: Mcx<'static>, ts: &[Oid]) -> NodeList<'static> {
        let mut tl = NodeList::nil();
        for (i, &t) in ts.iter().enumerate() {
            let var = Node::mk(m, Var { vartype: t, varattno: i as i16 + 1, ..Default::default() }).expect("var");
            tl.lappend(m, Node::mk(m, TargetEntry { expr: var, resno: i as i16 + 1, resname: None, ressortgroupref: i as u32 + 1, resorigtbl: 0, resorigcol: 0, resjunk: false }).expect("tle")).expect("tl");
        }
        tl
    }

    fn input(m: Mcx<'static>, ts: &[Oid]) -> Node<'static> {
        Node::mk(m, SeqScan { scan: Scan { plan: Plan { targetlist: tlist(m, ts), ..Default::default() }, ..Default::default() }, ..Default::default() }).expect("scan")
    }

    fn over(m: Mcx<'static>, ts: &[Oid]) -> Plan<'static> {
        Plan { lefttree: Some(input(m, ts)), ..Default::default() }
    }

    fn opexpr(m: Mcx<'static>, opno: Oid, t: Oid) -> Node<'static> {
        let v = || Node::mk(m, Var { vartype: t, ..Default::default() }).expect("var");
        Node::mk(m, OpExpr { opno, opfuncid: 0, opresulttype: 16, args: NodeList::make2(m, v(), v()).expect("args"), ..Default::default() }).expect("op")
    }

    fn sgc(sortop: Oid, eqop: Oid) -> SortGroupClause {
        SortGroupClause { tleSortGroupRef: 1, eqop, sortop, ..Default::default() }
    }

    /// D1371-R6 S2 / T1-T2: the family get_ordering_op_properties picks decides, never the operator's OID: box's built-in
    /// `<` in a user btree family (FUNCTION 1 plpgsql) refuses; int4's `<`, also in a user family, sorts in the built-in one.
    #[test]
    fn the_family_decides_never_the_operator_oid() {
        let c = cat();
        assert!(!ok(&c, false, KeyUse::Sort, key(504, 603)), "T1: box < (a built-in operator) in a user family");
        assert!(ok(&c, false, KeyUse::Sort, key(97, 23)), "T2: int4 < resolves to integer_ops");
        assert!(!ok(&c, false, KeyUse::Sort, key(16600, 23)), "a user < over int4");
        assert!(ok(&c, false, KeyUse::Sort, key(3518, 16500)) && ok(&c, false, KeyUse::Eq, key(3516, 16500)) && ok(&c, false, KeyUse::Hash, key(3516, 16500)), "an enum: enum_cmp / enum_eq / hashenum, no DISPATCH");
        assert!(ok(&c, false, KeyUse::Sort, key(97, 16990)), "a domain over int4 sorts by int4's family");
        assert!(ok(&c, false, KeyUse::Sort, key(664, 25)) && ok(&c, false, KeyUse::Hash, key(98, 25)), "text");
    }

    /// Sort needs FUNCTION 1 and 2 built in where present, and one of them (an operator no family orders by refuses); the
    /// FUNCTION-2-only family (412: F1 0, F2 built in) serves.
    #[test]
    fn a_sort_key_needs_every_present_function_built_in() {
        let mut c = cat();
        assert!(!ok(&c, false, KeyUse::Sort, key(9999, 23)), "no ordering family");
        assert!(ok(&c, false, KeyUse::Sort, key(412, 20)), "FUNCTION 2 alone");
        c.support.insert(Support::Sort { op: 97, num: 2 }, 16502);
        assert!(!ok(&c, false, KeyUse::Sort, key(97, 23)), "a user FUNCTION 2 beside a built-in FUNCTION 1");
        c.support.insert(Support::Sort { op: 97, num: 2 }, 3130);
        c.support.insert(Support::Sort { op: 97, num: 1 }, 16502);
        assert!(!ok(&c, false, KeyUse::Sort, key(97, 23)), "a user FUNCTION 1 beside a built-in FUNCTION 2");
    }

    #[test]
    fn eq_hash_and_merge_read_their_own_functions() {
        let c = cat();
        assert!(!ok(&c, false, KeyUse::Eq, key(16602, 23)), "a user = (its oprcode)");
        assert!(!ok(&c, false, KeyUse::Eq, key(9999, 23)), "an operator the catalog cannot read");
        assert!(!ok(&c, false, KeyUse::Hash, key(16602, 23)), "a user right-hand hash function");
        assert!(!ok(&c, false, KeyUse::Hash, key(410, 20)), "no hash function");
        assert!(ok(&c, false, KeyUse::Merge, Key { family: 1976, ..key(96, 23) }), "integer_ops");
        assert!(!ok(&c, false, KeyUse::Merge, Key { family: 16700, ..key(96, 23) }), "int4 = in a user merge family");
        for u in [KeyUse::InRange, KeyUse::Prune, KeyUse::Route] {
            assert!(ok(&c, false, u, key(9999, 0)), "{u:?} reads fns, not keys");
        }
    }

    /// Functions a caller resolved (cmpfns, in_range, routing): a user one or any DISPATCH one refuses (D1483), 0 is none.
    #[test]
    fn caller_functions_refuse_user_and_dispatch_ones() {
        assert!(super::fns_ok(&[351, 0, 4135]));
        assert!(!super::fns_ok(&[351, 16601]), "a user FUNCTION 1 in cmpfns");
        for f in [382, 3870, 626, 6192, 744] {
            assert!(!super::fns_ok(&[f]), "DISPATCH {f}");
        }
    }

    /// S4 / D2093: DISPATCH_OK is false, so a range, array or record key refuses whatever its components: int4range,
    /// int4[] and real[] included. The flip is GUARD-DISPATCH-OK's, after the D2093 row lands.
    #[test]
    fn dispatch_keys_refuse_while_dispatch_ok_is_false() {
        assert!(!DISPATCH_OK, "D2093 has not landed: GUARD-DISPATCH-OK flips this");
        let c = cat();
        assert!(!ok(&c, DISPATCH_OK, KeyUse::Sort, key(3884, 3904)), "int4range (range_cmp / range_sortsupport)");
        assert!(!ok(&c, DISPATCH_OK, KeyUse::Hash, key(3882, 3904)), "int4range hashed (hash_range, range_eq)");
        assert!(!ok(&c, DISPATCH_OK, KeyUse::Sort, key(1072, 1007)), "int4[] (btarraycmp)");
        assert!(!ok(&c, DISPATCH_OK, KeyUse::Sort, key(1072, 1021)), "real[]");
        assert!(!ok(&c, DISPATCH_OK, KeyUse::Eq, key(1070, 1007)), "int4[] = (array_eq)");
    }

    /// Once DISPATCH_OK: each component the dispatcher reads through the type cache decides (T3-T6b): built-in element,
    /// subtype and field functions serve; a user one anywhere below refuses, record and a runaway nesting too.
    #[test]
    fn dispatch_components_decide_once_dispatch_ok() {
        let c = cat();
        assert!(ok(&c, true, KeyUse::Sort, key(3884, 3904)) && ok(&c, true, KeyUse::Hash, key(3882, 3904)), "int4range");
        assert!(!ok(&c, true, KeyUse::Sort, key(3884, 16900)), "T5: a user range whose rngsubopc FUNCTION 1 is user code");
        assert!(ok(&c, true, KeyUse::Sort, key(1072, 1007)) && ok(&c, true, KeyUse::Sort, key(1072, 1021)), "int4[], real[]");
        assert!(ok(&c, true, KeyUse::Eq, key(1070, 1007)) && ok(&c, true, KeyUse::Hash, key(1070, 1007)), "int4[] = / hashed");
        assert!(!ok(&c, true, KeyUse::Sort, key(1072, 199)), "T4: json[] under a user DEFAULT json opclass");
        // anymultirange < 2862 (multirange_cmp 4273), record < 2990 (btrecordcmp 2987)
        let mut r = cat();
        r.support.extend([(Support::Sort { op: 2862, num: 1 }, 4273), (Support::Sort { op: 2990, num: 1 }, 2987)]);
        assert!(ok(&r, true, KeyUse::Sort, key(2862, 4451)), "int4multirange: its range's components");
        assert!(ok(&r, true, KeyUse::Sort, key(2990, 16950)), "a composite of int4, a dropped column, text");
        assert!(!ok(&r, true, KeyUse::Sort, key(2990, 16960)), "a composite over the user range");
        assert!(!ok(&r, true, KeyUse::Sort, key(2990, 2249)), "record: its fields are not the catalog's");
        assert!(!ok(&c, true, KeyUse::Sort, key(1072, 16999)), "an array type nested in itself: the depth cap");
        assert!(!ok(&c, true, KeyUse::Sort, key(1072, 0)), "an unknown key type");
    }

    /// lockrows J182: `span_calls_free`'s Sort arm passes a key only where `sort_free` does. A record key over a composite
    /// holding a type with no btree opclass (point: no comparator in the type cache, where btrecordcmp raises "could not
    /// identify a comparison function" at run time) stays refused, and so does `record`; a composite of int4 and text passes.
    #[test]
    fn a_sort_key_over_a_composite_of_a_type_without_opclass_stays_refused() {
        let mut r = cat();
        r.support.extend([(Support::Sort { op: 2990, num: 1 }, 2987), (Support::Natts(16970), 1), (Support::Field { typid: 16970, attno: 1 }, 600)]);
        assert!(!super::sort_free(&r, 2990, 16970), "a composite of point (no btree opclass)");
        assert!(!super::sort_free(&r, 2990, 2249), "record");
        assert!(super::sort_free(&r, 2990, 16950), "a composite of int4, a dropped column, text");
        assert!(super::sort_free(&r, 97, 23), "int4 <");
    }

    /// Main 19:38:26Z (AGC-17): the by-type reader (`type_free`) and hashjoin#1157's by-function one (decompose
    /// `effects::runs_user_code`) agree over a type's procs: a type is free exactly when none of its procs (an array's element's too) is
    /// user code. An enum (enum_in / enum_out / enum_eq / enum_cmp / hashenum), a domain over int4 (int4's), an array of a
    /// user base type (array_* and the element's own) and a user base type (its own procs, past FirstNormalObjectId).
    #[test]
    fn type_free_agrees_with_user_call_over_the_types_procs() {
        let mut c = cat();
        c.typtype.extend([(16500, b'e'), (16980, b'b')]);
        c.elem.insert(16974, 16980);
        c.base.insert(16990, 23);
        let user = |procs: &[u32]| procs.iter().any(|&p| gravity_decompose::effects::runs_user_code(&NoFacts, p));
        let rows: [(u32, &[u32]); 4] = [
            (16500, &[3504, 3505, 3508, 3514, 3515]),
            (16990, &[42, 43, 65, 351, 450]),
            (16974, &[750, 751, 744, 382, 626, 16981, 16982]),
            (16980, &[16981, 16982, 16983]),
        ];
        for (t, procs) in rows {
            assert_eq!(type_free(&c, t, 0), !user(procs), "type {t}: type_free and user_call over its procs");
        }
        assert!(type_free(&c, 16500, 0) && type_free(&c, 16990, 0) && !type_free(&c, 16974, 0) && !type_free(&c, 16980, 0), "the sample spans both answers");
    }

    /// aggtrans T6e: fold_ahead's by-type reading. Built-in types, enums, domains over them, arrays of them and composites
    /// of them reach no user code; a user base or range type, a pseudo-type, record, a composite holding one and a runaway
    /// nesting do. Sorts read their DISPATCH components: text[] and a composite of int4 and text sort by built-in code.
    #[test]
    fn type_free_reads_elements_columns_and_bases() {
        let mut c = cat();
        c.typtype.extend([(16500, b'e'), (16950, b'c'), (16960, b'c'), (16900, b'r'), (2249, b'p'), (2277, b'p'), (16970, b'c'), (16980, b'b')]);
        c.elem.extend([(1009, 25), (16971, 16950), (16972, 16500), (16973, 16900), (2277, 2283)]);
        c.typtype.insert(2283, b'p');
        c.base.extend([(16992, 1009), (16993, 16950)]);
        c.support.extend([(Support::Natts(16970), 2), (Support::Field { typid: 16970, attno: 1 }, 16971), (Support::Field { typid: 16970, attno: 2 }, 16992)]);
        for t in [23, 25, 1009, 3904, 16500, 16950, 16970, 16971, 16972, 16990, 16992, 16993] {
            assert!(type_free(&c, t, 0), "{t}");
        }
        for t in [16900, 16960, 16973, 16980, 2249, 2277, 16999] {
            assert!(!type_free(&c, t, 0), "{t}");
        }
        c.support.extend([(Support::Sort { op: 2990, num: 1 }, 2987)]);
        assert!(sort_free(&c, 1072, 1007) && sort_free(&c, 2990, 16950) && !sort_free(&c, 2990, 16960) && !sort_free(&c, 1072, 199), "int4[], a composite, one over a user range, json[] under a user opclass");
        c.cmp.extend([(1007, 382), (199, 382), (16950, 2987), (16960, 2987), (16500, 3514), (2249, 2987), (16990, 351)]);
        assert!(cmp_free(&c, 23) && cmp_free(&c, 1007) && cmp_free(&c, 16950) && cmp_free(&c, 16500) && cmp_free(&c, 16990), "int4, int4[], a composite, an enum, a domain over int4");
        assert!(!cmp_free(&c, 199) && !cmp_free(&c, 16960) && !cmp_free(&c, 2249) && !cmp_free(&c, 600), "json[] under a user opclass, a composite over a user range, record, a type with no comparator");
        // AGC-22: json text through a domain, an array and a composite's column (jsonb_agg parses it again per row)
        c.elem.insert(199, 114);
        c.base.insert(16994, 114);
        c.typtype.insert(16995, b'c');
        c.support.extend([(Support::Natts(16995), 2), (Support::Field { typid: 16995, attno: 1 }, 23), (Support::Field { typid: 16995, attno: 2 }, 16994)]);
        assert!(super::json_text(&c, 114) && super::json_text(&c, 199) && super::json_text(&c, 16994) && super::json_text(&c, 16995), "json, json[], a domain over json, a composite holding one");
        assert!(!super::json_text(&c, 3802) && !super::json_text(&c, 1009) && !super::json_text(&c, 16950) && !super::json_text(&c, 16970), "jsonb, text[], composites of int4 / text / arrays");
    }

    /// D1906 follow-up (record output over built-in columns): `visible::hashjoin::hidden`'s CoerceViaIO reads through a
    /// source type whose output runs only built-in code (`type_free`): a composite of int4 and text cast to text is
    /// built-in code; a composite over a user range, a user base type, and a cast into the composite (its input) still
    /// count, and `walk` alone (no `free`) reads every user type's output as hidden.
    #[test]
    fn a_cast_to_text_of_builtin_record_output_is_not_hidden() {
        use super::user_fn::{walk, walk_by, Reading};
        use types_nodes::primnodes::{CoerceViaIO, CoercionForm};
        let (mut c, m) = (cat(), mcx());
        c.typtype.extend([(16950, b'c'), (16960, b'c'), (16900, b'r'), (16980, b'b')]);
        let cast = |from: Oid, to: Oid| n(m, CoerceViaIO { arg: n(m, Var { vartype: from, ..Default::default() }), resulttype: to, resultcollid: 0, coerceformat: CoercionForm::COERCE_EXPLICIT_CAST, location: -1 });
        let free = |t: u32| type_free(&c, t, 0);
        let hid = |x: Node<'static>| walk_by(&NoFacts, x, &Reading::hidden(), &free);
        assert!(!hid(cast(16950, 25)), "a composite of int4 and text cast to text");
        assert!(hid(cast(16960, 25)) && hid(cast(16980, 25)), "a composite over a user range, a user base type");
        assert!(hid(cast(25, 16950)), "text cast to the composite (its input runs each column's)");
        assert!(walk(&NoFacts, cast(16950, 25), &Reading::hidden()), "walk alone");
    }

    fn n<T: types_nodes::NodeVariant<'static>>(m: Mcx<'static>, x: T) -> Node<'static> {
        Node::mk(m, x).expect("node")
    }

    /// S1: the KeyUse comes from the node. A sorted SetOp / MergeAppend / Gather Merge / IncrementalSort over the box key
    /// (box's built-in operators in a user family) refuses; a hashed SetOp compares by its = and hash functions.
    #[test]
    fn each_node_takes_its_key_use_from_the_node() {
        let (c, m) = (cat(), mcx());
        let sorted = |op, t, strategy| n(m, SetOp { plan: over(m, &[t]), strategy, numCols: 1, cmpColIdx: &[1], cmpOperators: Box::leak(Box::new([op])), cmpCollations: &[0], cmpNullsFirst: &[false], ..Default::default() }); // alloc: Cold
        assert!(!node_ok(&c, false, sorted(504, 603, 0)), "SETOP_SORTED over box");
        assert!(node_ok(&c, false, sorted(97, 23, 0)), "SETOP_SORTED over int4");
        assert!(node_ok(&c, false, sorted(96, 23, 1)) && !node_ok(&c, false, sorted(503, 603, 1)), "SETOP_HASHED: int4 = serves, box = has no hash function");
        let ma = |op, t| n(m, MergeAppend { plan: Plan { targetlist: tlist(m, &[t]), ..Default::default() }, numCols: 1, sortColIdx: &[1], sortOperators: Box::leak(Box::new([op])), collations: &[0], nullsFirst: &[false], ..Default::default() }); // alloc: Cold
        assert!(!node_ok(&c, false, ma(504, 603)) && node_ok(&c, false, ma(97, 23)), "MergeAppend");
        let gm = |op| n(m, GatherMerge { plan: over(m, &[603]), numCols: 1, sortColIdx: &[1], sortOperators: Box::leak(Box::new([op])), collations: &[0], nullsFirst: &[false], ..Default::default() }); // alloc: Cold
        assert!(!node_ok(&c, false, gm(504)) && node_ok(&c, false, gm(97)), "Gather Merge");
        let sort = |ops: &'static [Oid], pre| {
            let s = Sort { plan: over(m, &[23, 23]), numCols: ops.len() as i32, sortColIdx: &[1, 2], sortOperators: ops, collations: &[0, 0], nullsFirst: &[false, false], gravity_rewrite: None };
            n(m, IncrementalSort { sort: s, nPresortedCols: pre })
        };
        assert!(node_ok(&c, false, sort(&[97, 97], 1)), "IncrementalSort over int4");
        assert!(!node_ok(&c, false, sort(&[97, 504], 1)), "a user sort family in a later key");
        let mut e = cat();
        e.support.insert(Support::EqOfSort(97), 16602);
        assert!(!node_ok(&e, false, sort(&[97, 97], 1)), "IncrementalSort: the presorted key's = is user code");
        assert!(node_ok(&e, false, sort(&[97, 97], 0)), "no presorted key: no = read (does not apply)");
        let plain = |op| n(m, Sort { plan: over(m, &[23]), numCols: 1, sortColIdx: &[1], sortOperators: Box::leak(Box::new([op])), collations: &[0], nullsFirst: &[false], gravity_rewrite: None }); // alloc: Cold
        assert!(node_ok(&c, false, plain(97)) && !node_ok(&c, false, plain(16600)), "Sort");
    }

    #[test]
    fn joins_subplans_and_array_ops_read_their_clauses() {
        let (c, m) = (cat(), mcx());
        let mj = |fam: &'static [Oid]| {
            let j = MergeJoin { mergeclauses: NodeList::make1(m, opexpr(m, 96, 23)).unwrap(), mergeFamilies: fam, mergeCollations: &[0], mergeReversals: &[false], mergeNullsFirst: &[false], ..Default::default() };
            n(m, j)
        };
        assert!(node_ok(&c, false, mj(&[1976])) && !node_ok(&c, false, mj(&[16700])), "MergeJoin: the clause's merge family");
        let hj = |op| n(m, HashJoin { hashclauses: NodeList::make1(m, opexpr(m, op, 23)).unwrap(), hashoperators: OidList::make1(m, op).unwrap(), ..Default::default() });
        assert!(node_ok(&c, false, hj(96)) && !node_ok(&c, false, hj(16602)), "HashJoin");
        let memo = |op| n(m, Memoize { numKeys: 1, hashOperators: Box::leak(Box::new([op])), collations: &[0], param_exprs: NodeList::make1(m, n(m, Var { vartype: 23, ..Default::default() })).unwrap(), ..Default::default() }); // alloc: Cold
        assert!(node_ok(&c, false, memo(96)) && !node_ok(&c, false, memo(16602)), "Memoize");
        let ru = |op| n(m, RecursiveUnion { plan: over(m, &[23]), numCols: 1, dupColIdx: &[1], dupOperators: Box::leak(Box::new([op])), dupCollations: &[0], ..Default::default() }); // alloc: Cold
        assert!(node_ok(&c, false, ru(96)) && !node_ok(&c, false, ru(16602)), "RecursiveUnion");
        let sp = |op, hashed, and: bool| {
            let t = if and { n(m, BoolExpr { boolop: BoolExprType::AND_EXPR, args: NodeList::make2(m, opexpr(m, 96, 23), opexpr(m, op, 23)).unwrap(), location: -1 }) } else { opexpr(m, op, 23) };
            n(m, SubPlan { testexpr: Some(t), useHashTable: hashed, ..Default::default() })
        };
        assert!(node_ok(&c, false, sp(96, true, false)) && node_ok(&c, false, sp(96, true, true)), "hashed SubPlan over int4");
        assert!(!node_ok(&c, false, sp(16602, true, false)) && !node_ok(&c, false, sp(16602, true, true)), "hashed SubPlan over a user =");
        assert!(node_ok(&c, false, sp(16602, false, false)), "a SubPlan that does not hash resolves nothing here (does not apply)");
        let saop = |h, f| n(m, ScalarArrayOpExpr { opno: 96, opfuncid: f, hashfuncid: h, ..Default::default() });
        assert!(node_ok(&c, false, saop(450, 65)) && !node_ok(&c, false, saop(16604, 65)) && !node_ok(&c, false, saop(626, 744)), "hashed SAOP: hashint4 / a user hash / hash_array");
        assert!(node_ok(&c, false, saop(0, 16603)), "a SAOP that does not hash (does not apply)");
    }

    #[test]
    fn aggregates_windows_limits_and_pruning() {
        let (c, m) = (cat(), mcx());
        let kind = |sortop, eqop, distinct: bool, aggkind| {
            let cl = NodeList::make1(m, n(m, sgc(sortop, eqop))).unwrap();
            let arg = n(m, TargetEntry { expr: n(m, Var { vartype: 23, ..Default::default() }), resno: 1, resname: None, ressortgroupref: 1, resorigtbl: 0, resorigcol: 0, resjunk: false });
            let (aggorder, aggdistinct) = if distinct { (NodeList::nil(), cl) } else { (cl, NodeList::nil()) };
            n(m, Aggref { aggfnoid: 2803, args: NodeList::make1(m, arg).unwrap(), aggorder, aggdistinct, aggkind, ..Default::default() })
        };
        let aggref = |sortop, eqop, distinct| kind(sortop, eqop, distinct, AGGKIND_NORMAL);
        assert!(node_ok(&c, false, aggref(97, 96, false)) && !node_ok(&c, false, aggref(16600, 96, false)), "aggorder");
        assert!(!node_ok(&c, false, aggref(97, 16602, true)), "aggdistinct: its = too");
        assert!(node_ok(&c, false, aggref(97, 16602, false)), "a plain aggregate's aggorder reads no = (does not apply)");
        // mode() (orderedsetaggs.c:1063) and dense_rank() WITHIN GROUP (:1351) compare by the clause's eqop: a user = beside
        // a built-in FUNCTION 1 refuses; built-in ones serve
        for k in [AGGKIND_ORDERED_SET, AGGKIND_HYPOTHETICAL] {
            assert!(!node_ok(&c, false, kind(97, 16602, false, k)) && node_ok(&c, false, kind(97, 96, false, k)), "aggkind {k}");
        }
        let agg = |strategy, op, tl: Option<Node<'static>>| {
            let mut p = over(m, &[23]);
            p.targetlist = tl.map_or(NodeList::nil(), |x| NodeList::make1(m, n(m, TargetEntry { expr: x, resno: 1, resname: None, ressortgroupref: 0, resorigtbl: 0, resorigcol: 0, resjunk: false })).unwrap());
            n(m, Agg { plan: p, aggstrategy: strategy, numCols: 1, grpColIdx: &[1], grpOperators: Box::leak(Box::new([op])), grpCollations: &[0], ..Default::default() }) // alloc: Cold
        };
        assert!(node_ok(&c, false, agg(1, 96, None)) && node_ok(&c, false, agg(2, 96, None)), "sorted / hashed over int4");
        assert!(!node_ok(&c, false, agg(1, 16602, None)) && !node_ok(&c, false, agg(2, 16602, None)) && !node_ok(&c, false, agg(3, 16602, None)), "a user =");
        assert!(node_ok(&c, false, agg(0, 16602, None)), "AGG_PLAIN compares no key (does not apply)");
        assert!(!node_ok(&c, false, agg(0, 96, Some(aggref(16600, 96, false)))), "the Agg answers for its Aggrefs");
        let mut chained = Agg { plan: over(m, &[23]), aggstrategy: 1, numCols: 1, grpColIdx: &[1], grpOperators: &[96], grpCollations: &[0], ..Default::default() };
        let phase = |op| n(m, Agg { plan: Plan { lefttree: Some(n(m, Sort { numCols: 1, sortColIdx: &[1], sortOperators: Box::leak(Box::new([op])), collations: &[0], nullsFirst: &[false], ..Default::default() })), ..Default::default() }, aggstrategy: 1, numCols: 1, grpColIdx: &[1], grpOperators: &[96], grpCollations: &[0], ..Default::default() }); // alloc: Cold
        chained.chain = NodeList::make1(m, phase(97)).unwrap();
        assert!(node_ok(&c, false, n(m, chained)), "a chain phase sorting by int4");
        let mut chained = Agg { plan: over(m, &[23]), aggstrategy: 1, numCols: 1, grpColIdx: &[1], grpOperators: &[96], grpCollations: &[0], ..Default::default() };
        chained.chain = NodeList::make1(m, phase(16600)).unwrap();
        assert!(!node_ok(&c, false, n(m, chained)), "a chain phase's Sort by a user family");
        let win = |op, f| n(m, WindowAgg { plan: over(m, &[23]), partNumCols: 1, partColIdx: &[1], partOperators: Box::leak(Box::new([op])), partCollations: &[0], startInRangeFunc: f, ..Default::default() }); // alloc: Cold
        assert!(node_ok(&c, false, win(96, 4135)) && !node_ok(&c, false, win(16602, 0)) && !node_ok(&c, false, win(96, 16605)), "WindowAgg: partition = and in_range");
        let lim = |o, op| n(m, Limit { plan: over(m, &[23]), limitOption: o, uniqNumCols: 1, uniqColIdx: &[1], uniqOperators: Box::leak(Box::new([op])), uniqCollations: &[0], ..Default::default() }); // alloc: Cold
        assert!(!node_ok(&c, false, lim(LimitOption::LIMIT_OPTION_WITH_TIES, 16602)) && node_ok(&c, false, lim(LimitOption::LIMIT_OPTION_COUNT, 16602)), "WITH TIES alone compares");
        let grp = |op| n(m, Group { plan: over(m, &[23]), numCols: 1, grpColIdx: &[1], grpOperators: Box::leak(Box::new([op])), grpCollations: &[0] }); // alloc: Cold
        let uniq = |op| n(m, Unique { plan: over(m, &[23]), numCols: 1, uniqColIdx: &[1], uniqOperators: Box::leak(Box::new([op])), uniqCollations: &[0] }); // alloc: Cold
        assert!(node_ok(&c, false, grp(96)) && !node_ok(&c, false, grp(16602)) && node_ok(&c, false, uniq(96)) && !node_ok(&c, false, uniq(16602)), "Group / Unique");
        let step = |f| n(m, PartitionPruneStepOp { cmpfns: OidList::make1(m, f).unwrap(), ..Default::default() });
        let rel = |f| n(m, PartitionedRelPruneInfo { exec_pruning_steps: NodeList::make1(m, step(f)).unwrap(), ..Default::default() });
        let info = |f| n(m, PartitionPruneInfo { prune_infos: NodeList::make1(m, Node::mk_list(m, NodeList::make1(m, rel(f)).unwrap()).unwrap()).unwrap(), ..Default::default() });
        assert!(node_ok(&c, false, step(351)) && !node_ok(&c, false, step(16601)) && !node_ok(&c, false, step(382)), "K3: cmpfns");
        assert!(node_ok(&c, false, info(351)) && !node_ok(&c, false, info(16601)), "K3: through the prune info");
    }

    /// A built-in that runs a type's input function or a query named by a run-time argument (pg_input_is_valid /
    /// pg_input_error_info over a domain whose CHECK calls nextval, table_to_xml over a view, ts_stat) is never built in,
    /// DISPATCH_OK or not; the schema-only xml functions read the catalog alone (does not apply).
    #[test]
    fn run_time_named_builtins_are_never_built_in() {
        let c = cat();
        for f in [6210, 6211, 2923, 2929, 2933, 2935, 2936, 2938, 3689, 3690, 3685] {
            assert!(!super::fns_ok(&[f]) && !super::built_in(&c, true, f, 25, 0), "{f}");
        }
        assert!(super::fns_ok(&[2926, 2934, 2937, 3684]) && super::built_in(&c, true, 2926, 2205, 0), "the xmlschema-only functions, ts_rewrite(tsquery, tsquery, tsquery)");
    }

    /// A node that resolves no support function answers Ok whatever its expressions call (acts() is G2's): a scan.
    #[test]
    fn a_node_that_resolves_nothing_answers_ok() {
        let (c, m) = (Cat::default(), mcx());
        assert!(node_ok(&c, false, input(m, &[16900])), "a scan of a user-range column");
        assert!(node_ok(&c, false, n(m, Hash::default())), "the Hash node: its HashJoin reads the functions");
    }

    #[test]
    fn the_pub_faces_answer_user_fn_by_name() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let m = mcx();
        let catalog: &'static dyn Catalog<'static> = Box::leak(Box::new(cat())); // alloc: Cold
        let arena: &'static Arena = Box::leak(Box::new(Arena::new(1 << 12))); // alloc: Cold
        let g: &'static PipelineGraph = Box::leak(Box::new(PipelineGraph::new(arena.handle()))); // alloc: Cold
        let gx = Gx { graph: g, facts: &NoFacts, catalog, over: None, mcx: rig.mcx() };
        assert_eq!(support(&gx, KeyUse::Sort, &[key(504, 603)], &[]), Err(USER_FN));
        assert_eq!(support(&gx, KeyUse::Prune, &[], &[16601]), Err(USER_FN));
        assert_eq!(support(&gx, KeyUse::InRange, &[], &[6210]), Err(USER_FN));
        assert_eq!(support(&gx, KeyUse::Sort, &[key(97, 23)], &[351]), Ok(()));
        let s = Node::mk(m, Sort { numCols: 1, sortColIdx: &[1], sortOperators: &[504], collations: &[0], nullsFirst: &[false], ..Default::default() }).unwrap();
        assert_eq!(support_of(&gx, s), Err(USER_FN));
        assert!(USER_FN.0.contains("(D2113)"));
    }

    /// drain (gen mt-0076 group): `window_acts`, one case per act kind (a VOLATILE function argument or FILTER, a VOLATILE
    /// projection, a window function that may act, its run condition, a start or end frame offset, a user partition or
    /// order =); row_number() over int4 keys with none of them acts nowhere.
    #[test]
    fn a_window_acts_per_kind() {
        use gravity_decompose::ops::window::{WinFn, WinFnKind, WindowDesc};
        use gravity_decompose::ops::shared::window::{Exclude, FrameBound, FrameCursor, FrameMode};
        use gravity_decompose::Op;
        use gravity_program::ArenaVec;
        use types_nodes::primnodes::WindowFunc;
        let m = mcx();
        let arena: &'static Arena = Box::leak(Box::new(Arena::new(1 << 12))); // alloc: Cold
        let (quiet_fn, acting_fn) = (n(m, WindowFunc { winfnoid: 3100, wintype: 20, ..Default::default() }), n(m, WindowFunc { winfnoid: 16501, wintype: 20, ..Default::default() }));
        let (quiet, acting) = (n(m, Var { vartype: 23, ..Default::default() }), n(m, Var { vartype: 23, ..Default::default() }));
        let a = |x: Node<'_>| x.ptr_eq(acting) || x.ptr_eq(acting_fn);
        let user_op = |o: Oid| o >= 16384;
        let cursor = FrameCursor { mode: FrameMode::Rows, start: FrameBound::UnboundedPreceding, end: FrameBound::CurrentRow, exclude: Exclude::None, needs: 0 };
        // (VOLATILE arguments, VOLATILE projection, the function, the run condition, the start / end offsets, partition =, order =)
        let win = |args: bool, proj: bool, f: Node<'static>, run: Node<'static>, offsets: [Option<Node<'static>>; 2], part: Oid, ord: Oid| {
            let plan: &'static WindowAgg<'static> = Box::leak(Box::new(WindowAgg { partNumCols: 1, partColIdx: &[1], partOperators: Box::leak(Box::new([part])), partCollations: &[0], ordNumCols: 1, ordColIdx: &[1], ordOperators: Box::leak(Box::new([ord])), ordCollations: &[0], startOffset: offsets[0], endOffset: offsets[1], ..Default::default() })); // alloc: Cold
            let mut functions = ArenaVec::new_in(arena.handle());
            functions.push(WinFn { resno: 2, node: f, func: f.as_window_func().expect("a WindowFunc"), kind: WinFnKind::RowNumber, cursor });
            let run_condition: &'static NodeList<'static> = Box::leak(Box::new(NodeList::make1(m, run).expect("run"))); // alloc: Cold
            let (partition, order) = (ArenaVec::new_in(arena.handle()), ArenaVec::new_in(arena.handle()));
            Op::Window(WindowDesc { winref: 1, partition, order, frame: cursor, functions, natts: 1, run_condition, top: true, tlist: &plan.plan.targetlist, plan, volatile_args: args, volatile_proj: proj, whole: false, arms_capped: false, capped: None })
        };
        let acts = |o: Op<'static>| super::window_acts(&o, &a, &user_op);
        assert!(!acts(win(false, false, quiet_fn, quiet, [Some(quiet), Some(quiet)], 96, 97)), "row_number() OVER (PARTITION BY int4 ORDER BY int4)");
        assert!(acts(win(true, false, quiet_fn, quiet, [None, None], 96, 97)), "a VOLATILE argument or FILTER");
        assert!(acts(win(false, true, quiet_fn, quiet, [None, None], 96, 97)), "a VOLATILE projection");
        assert!(acts(win(false, false, acting_fn, quiet, [None, None], 96, 97)), "the window function (its WindowFunc node) may act");
        assert!(acts(win(false, false, quiet_fn, acting, [None, None], 96, 97)), "the run condition may act");
        assert!(acts(win(false, false, quiet_fn, quiet, [Some(acting), None], 96, 97)), "the start offset may act");
        assert!(acts(win(false, false, quiet_fn, quiet, [None, Some(acting)], 96, 97)), "the end offset may act");
        assert!(acts(win(false, false, quiet_fn, quiet, [None, None], 16602, 97)), "a user partition =");
        assert!(acts(win(false, false, quiet_fn, quiet, [None, None], 96, 16600)), "a user order operator");
    }

    /// with#170 / #171: `window_quiet`, one case per rule (a frame offset, a user partition or order operator, a run
    /// condition the caller rejects, a function that may raise, a plain builtin aggregate quiet or not, a record or record[]
    /// key, a text key with and without its collation).
    #[test]
    fn a_window_is_quiet_per_kind() {
        use gravity_decompose::ops::shared::sort::EqKey;
        use gravity_decompose::ops::window::{WinFn, WinFnKind, WindowDesc};
        use gravity_decompose::ops::shared::window::{Exclude, FrameBound, FrameCursor, FrameMode};
        use gravity_abi::breaker::KeyKind;
        use gravity_program::ArenaVec;
        use types_nodes::primnodes::WindowFunc;
        let m = mcx();
        let arena: &'static Arena = Box::leak(Box::new(Arena::new(1 << 12))); // alloc: Cold
        let rank = n(m, WindowFunc { winfnoid: 3100, wintype: 20, ..Default::default() });
        let lag = n(m, WindowFunc { winfnoid: 3106, wintype: 23, ..Default::default() });
        let min = n(m, WindowFunc { winfnoid: 2132, wintype: 23, winagg: true, ..Default::default() });
        let sum = n(m, WindowFunc { winfnoid: 2114, wintype: 1700, winagg: true, ..Default::default() });
        let (fine, loud) = (n(m, Var { vartype: 23, ..Default::default() }), n(m, Var { vartype: 23, ..Default::default() }));
        let q = |x: Node<'_>| !x.ptr_eq(loud);
        let cursor = FrameCursor { mode: FrameMode::Rows, start: FrameBound::UnboundedPreceding, end: FrameBound::CurrentRow, exclude: Exclude::None, needs: 0 };
        // (the function and its kind, the run condition, the start offset, partition =, the partition key's type and collation)
        let win = |f: Node<'static>, kind: WinFnKind, run: Node<'static>, start: Option<Node<'static>>, part: Oid, (typid, coll): (Oid, Oid)| {
            let plan: &'static WindowAgg<'static> = Box::leak(Box::new(WindowAgg { partNumCols: 1, partColIdx: &[1], partOperators: Box::leak(Box::new([part])), partCollations: Box::leak(Box::new([coll])), ordNumCols: 1, ordColIdx: &[1], ordOperators: Box::leak(Box::new([97])), ordCollations: &[0], startOffset: start, ..Default::default() })); // alloc: Cold
            let mut functions = ArenaVec::new_in(arena.handle());
            functions.push(WinFn { resno: 2, node: f, func: f.as_window_func().expect("a WindowFunc"), kind, cursor });
            let run_condition: &'static NodeList<'static> = Box::leak(Box::new(NodeList::make1(m, run).expect("run"))); // alloc: Cold
            let (mut partition, order) = (ArenaVec::new_in(arena.handle()), ArenaVec::new_in(arena.handle()));
            partition.push(EqKey { resno: 1, opno: part, kind: KeyKind::Fp128, typid });
            WindowDesc { winref: 1, partition, order, frame: cursor, functions, natts: 1, run_condition, top: true, tlist: &plan.plan.targetlist, plan, volatile_args: false, volatile_proj: false, whole: false, arms_capped: false, capped: None }
        };
        let quiet = |d: WindowDesc<'static>| super::window_quiet(&d, &q);
        assert!(quiet(win(rank, WinFnKind::RowNumber, fine, None, 96, (23, 0))), "row_number() OVER (PARTITION BY int4 ORDER BY int4)");
        assert!(quiet(win(min, WinFnKind::Aggregate, fine, None, 96, (23, 0))), "min(int4) OVER (..): a plain builtin aggregate the effects home reads quiet");
        assert!(!quiet(win(rank, WinFnKind::RowNumber, fine, Some(fine), 96, (23, 0))), "a frame offset (PG checks its sign and NULL)");
        assert!(!quiet(win(rank, WinFnKind::RowNumber, fine, None, 16602, (23, 0))), "a user partition =");
        assert!(!quiet(win(rank, WinFnKind::RowNumber, loud, None, 96, (23, 0))), "a run condition the caller rejects");
        assert!(!quiet(win(lag, WinFnKind::Lag, fine, None, 96, (23, 0))), "lag: its offset argument may raise");
        assert!(!quiet(win(sum, WinFnKind::Aggregate, fine, None, 96, (23, 0))), "sum(numeric) over no typmod may overflow");
        assert!(!quiet(win(rank, WinFnKind::RowNumber, fine, None, 2988, (2249, 0))), "PARTITION BY ROW(..): record_eq reaches each column's own =");
        assert!(!quiet(win(rank, WinFnKind::RowNumber, fine, None, 1070, (2287, 0))), "a record[] key: array_eq over record_eq");
        assert!(!quiet(win(rank, WinFnKind::RowNumber, fine, None, 98, (25, 0))), "a text key under no collation: texteq raises");
        assert!(quiet(win(rank, WinFnKind::RowNumber, fine, None, 98, (25, 100))), "a text key under its collation");
    }
}

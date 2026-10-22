"""R12: lowering and exec's admission read derived plan properties only from facts, effects and typesem (PLAN-v5 §3.2;
DECOMPOSER-v5 §3; closed-ir.md §4). Not in decompose (the plan reader), hosts, gravity_expr, fn, or the homes; inside
decompose only (b)'s `.oper(` applies. The rule cannot see properties derived from structure with no forbidden name,
or names Gravity shares with PG (nulls_first, sortop, parallel_safe, unique, collations, typid, typmod): review, the
facts oracle and typesem's boundary variants cover those."""
import re
from common import G, L, home, in_any, in_src, is_lowering

ID = "R12"
TITLE = "lowering reads a PG order / uniqueness / key-class / rescan field, a property reader or a plan-node accessor"
INSTEAD = "facts::of(g, node), effects, typesem"
_EXEMPT = home("effects", "inputs", "typesem", "facts", "catalog", "rerun", "dict", "table", "iface") + L("frame/")
_ADMIT = [G + "exec/src/admit/", G + "exec/src/ops/*/admit.rs", G + "exec/src/ops/*/admit/*"]


def SCOPE(p):
    return in_src(p) and (is_lowering(p) or in_any(p, _ADMIT) or p.startswith(G + "decompose/src/"))


def is_home(p):
    return in_any(p, _EXEMPT)


_FIELDS = ("sortOperators sortColIdx nullsFirst ordOperators ordColIdx partOperators cmpNullsFirst mergeNullsFirst "
           "mergeReversals mergeCollations indexorderdir aggpresorted gravity_sort_operators gravity_nulls_first "
           "gravity_sort_col_idx inner_unique uniqOperators uniqColIdx grpOperators dupOperators hashOperators "
           "hashoperators hashclauses mergeFamilies cmpOperators eqop extParam allParam chgParam parParam setParam "
           "execparamids keyparamids parallel_aware parallelModeNeeded").split()          # 35 (37 less hashfuncid, negfuncid)
_ACC = ("planned_stmt result project_set seq_scan sample_scan tid_scan tid_range_scan index_scan index_only_scan "
        "bitmap_and bitmap_or bitmap_index_scan bitmap_heap_scan function_scan table_func_scan cte_scan "
        "named_tuplestore_scan work_table_scan recursive_union values_scan foreign_scan sort incremental_sort material "
        "memoize group unique append merge_append gather gather_merge partition_prune_info partitioned_rel_prune_info "
        "partition_prune_step_op partition_prune_step_combine append_rel_info subquery_scan set_op agg window_agg "
        "modify_table limit nest_loop_param nest_loop merge_join hash_join hash lock_rows plan_row_mark "
        "plan_inval_item plan").split()
_ENUMS = {"GravityRewrite", "RowMarkType", "PARTPRUNE_COMBINE_UNION", "PARTPRUNE_COMBINE_INTERSECT"}
A = re.compile(r"\.(?:" + "|".join(_FIELDS) + r")\b(?!\s*\()")
B = re.compile(r"\.oper\s*\(|\bbuiltin_oper\s*\(|\.opcode\s*\(|\btype_facts\s*\(")
C = re.compile(r"\.as_(?:" + "|".join(_ACC) + r")\s*\(")
_OPER = re.compile(r"\.oper\s*\(")
_PN = re.compile(r"\bplannodes::(\{[^}]*\}|\*|\w+)")


def check(src, ctx):
    if src.path.startswith(G + "decompose/src/"):
        return src.hits(_OPER)
    out = src.hits(A) + src.hits(B) + src.hits(C)
    out += src.hits(_PN, keep=lambda t, m: not {n.strip().split(" as ")[0].split("::")[0]
                                                for n in m.group(1).strip("{}").split(",") if n.strip()} <= _ENUMS)
    return sorted(set(out))


PLANTED = [(G + "lowerv4/src/ops/mergejoin/planted.rs",
            "pub fn rev(m: &MergeJoin) -> bool { m.mergeReversals.iter().next().is_some() }\n"),
           (G + "lowerv4/src/ops/sort/planted.rs", "pub fn s(n: Node) -> bool { n.as_sort().is_some() }\n"),
           (G + "decompose/src/ops/hashjoin.rs", "pub fn k(f: &Facts, o: u32) -> bool { f.oper(o).is_ok() }\n")]
CLEAN = [(G + "lowerv4/src/ops/mergejoin/clean.rs",
          "use types_nodes::plannodes::RowMarkType;\npub fn rev(f: &NodeFacts) -> bool { f.order.is_some() }\n"),
         (G + "decompose/src/ops/sort/mod.rs", "pub fn s(n: Node) -> bool { n.as_sort().is_some() }\n")]


use ::types_nodes::node_tree::Node;
use ::types_nodes::NodeTag;

/// C's `ExecSupportsBackwardScan` (execAmi.c:503+), DEMOTED to a
/// scroll-POLICY oracle (backward-execution wave B10, cursors inc-2 §6
/// rider row 3 "execami demotion + rename").
///
/// What changed: in C this predicate answers "can the EXECUTOR run this
/// plan backwards?" and gates both the planner's Material wrap (deleted,
/// B3) and the implicit-SCROLL default for cursors declared without
/// SCROLL/NO SCROLL. Our executor NEVER runs backwards (the run seam
/// refuses backward entry - deletion-prep B1; backward cursor reads are
/// served by the portal tuplestore). What remains is the POLICY use: which
/// cursors get CURSOR_OPT_SCROLL by default - a user-visible SQL contract
/// (whether FETCH BACKWARD on an undeclared cursor succeeds or raises the
/// no-scroll error) that must stay byte-identical to C. So the plan-shape
/// walk below mirrors C's answer set EXACTLY and is consulted only by the
/// two cursor-open policy probes (portalcmds PerformCursorOpen, SPI
/// SPI_cursor_open_internal), never by the executor.
pub fn plan_implicit_scroll_ok(node: Option<Node<'_>>) -> bool {
    let Some(node) = node else { return false };
    let plan = node.as_plan().expect("plan-tree node has a Plan prefix");
    if plan.parallel_aware {
        return false;
    }
    match node.node_tag() {
        NodeTag::T_Result => match plan.lefttree {
            Some(outer) => plan_implicit_scroll_ok(Some(outer)),
            None => false,
        },
        // C: IndexSupportsBackwardScan(indexid) — the index AM's
        // amcanbackward (btree/hash yes; gist/spgist/gin/brin/bloom/hnsw no).
        NodeTag::T_IndexScan => {
            index_supports_backward_scan(node.as_index_scan().expect("T_IndexScan").indexid)
        }
        NodeTag::T_IndexOnlyScan => index_supports_backward_scan(
            node.as_index_only_scan().expect("T_IndexOnlyScan").indexid,
        ),
        NodeTag::T_SeqScan
        | NodeTag::T_TidScan
        | NodeTag::T_TidRangeScan
        | NodeTag::T_FunctionScan
        | NodeTag::T_ValuesScan
        | NodeTag::T_CteScan
        | NodeTag::T_Material
        | NodeTag::T_Sort => true,
        NodeTag::T_Append => {
            let a = node.as_append().expect("T_Append");
            // With async, tuples may be interleaved, so can't back up.
            a.nasyncplans == 0
                && a.appendplans
                    .iter()
                    .all(|p| plan_implicit_scroll_ok(Some(p)))
        }
        NodeTag::T_SubqueryScan => {
            plan_implicit_scroll_ok(node.as_subquery_scan().expect("T_SubqueryScan").subplan)
        }
        NodeTag::T_LockRows | NodeTag::T_Limit => plan_implicit_scroll_ok(plan.lefttree),
        _ => false,
    }
}

pub(crate) fn cursor_plan_rewind_replays_seam(pstmt: &::types_nodes::plannodes::PlannedStmt<'_>) -> bool {
    !plan_implicit_scroll_ok(pstmt.planTree)
}

/// `ExecSupportsBackwardScan` as pquery's PortalStart consults it: C's
/// plan-shape answer (`plan_implicit_scroll_ok`) narrowed by the port's
/// True ⇒ the portal is NOT store-armed: it hands the executor C's
/// REWIND|BACKWARD eflags and FETCH BACKWARD runs the plan backward
/// (volatile functions re-evaluate, as in C); false ⇒ the portal store
/// stands in for the Material node C's planner would add.
pub fn plan_supports_backward_scan(pstmt: &::types_nodes::plannodes::PlannedStmt<'_>) -> bool {
    if !plan_implicit_scroll_ok(pstmt.planTree) {
        return false;
    }
    if !::syscache_seams::lookup_pg_class_ls_shape::is_installed() {
        return true;
    }
    !pstmt.rtable.iter().filter_map(|n| n.as_range_tbl_entry()).any(|rte| {
        rte.rtekind == ::types_nodes::parsenodes::RTEKind::RTE_RELATION
            && matches!(::lsyscache::get_rel_relam(rte.relid), Ok(relam) if relam_is_columnar(relam))
    })
}

fn relam_is_columnar(relam: ::types_core::Oid) -> bool {
    const HEAP_TABLE_AM_OID: ::types_core::Oid = 2;
    relam != ::types_core::InvalidOid
        && relam != HEAP_TABLE_AM_OID
        && matches!(
            ::syscache_seams::pg_am_amname::call(relam),
            Ok(Some(ref n)) if n == "pgrcolumnar"
        )
}

/// `IndexSupportsBackwardScan` (execAmi.c:603): does the index's AM support
/// backward scanning? C reads pg_class.relam through the syscache and asks
/// the AM routine for `amcanbackward`; the closed AM set answers statically
/// by handler kind: only btree and hash (nbtree.c bthandler / hash.c
/// hashhandler set it) — every other in-tree AM (gist, spgist, gin, brin)
/// and extension AM (bloom, hnsw) leaves it false. So an undeclared cursor
/// over a GiST index scan is NO SCROLL: FETCH BACKWARD raises "cursor can
/// only scan forward" (55000).
fn index_supports_backward_scan(indexid: ::types_core::Oid) -> bool {
    // Seamless unit-fixture worlds (no syscache installed) carry no relam to
    // consult; they keep the btree answer the fixtures were written against.
    if !::syscache_seams::lookup_pg_class_ls_shape::is_installed() {
        return true;
    }
    match ::lsyscache::get_rel_relam(indexid) {
        // The AM's handler answers, so a CREATE ACCESS METHOD ... HANDLER
        // bthandler index scrolls like btree (execAmi.c:617 amcanbackward).
        Ok(relam) => matches!(
            ::indexam::IndexAmKind::from_relam(relam),
            ::indexam::IndexAmKind::Btree | ::indexam::IndexAmKind::Hash
        ),
        // C: elog(ERROR, "cache lookup failed for relation %u") — a planned
        // index always exists; answer NO SCROLL rather than widen the policy.
        Err(_) => false,
    }
}

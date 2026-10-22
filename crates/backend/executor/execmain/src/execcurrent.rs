
use ::types_core::Oid;
use ::types_error::{PgError, PgResult, ERRCODE_INVALID_CURSOR_STATE, ERRCODE_UNDEFINED_CURSOR};
use ::types_portal::PortalStrategy;
use ::types_tuple::itemptr::ItemPointerData;

pub(crate) fn exec_current_of_seam(
    cursor_name: Option<&str>,
    cursor_param: i32,
    _table_oid: Oid,
    _table_name: &str,
) -> PgResult<Option<ItemPointerData>> {
    let Some(cursor_name) = cursor_name else {
        // fetch_cursor_param_value: REFCURSOR params only arrive via the
        // plpgsql columnref hooks, which are structurally absent.
        panic!("execCurrentOf (execCurrent.c): cursor_param {cursor_param}; plpgsql lane");
    };
    let Some(portal) = ::portalmem::GetPortalByName(Some(cursor_name)) else {
        return Err(Box::new(
            PgError::error(format!("cursor \"{cursor_name}\" does not exist"))
                .with_sqlstate(ERRCODE_UNDEFINED_CURSOR),
        ));
    };
    let (strategy, query_desc) = {
        let p = portal.borrow();
        (p.strategy, p.queryDesc)
    };
    if strategy != PortalStrategy::PORTAL_ONE_SELECT {
        return Err(invalid_cursor_state(format!(
            "cursor \"{cursor_name}\" is not a SELECT query"
        )));
    }
    if query_desc.is_null() {
        return Err(invalid_cursor_state(format!(
            "cursor \"{cursor_name}\" is held from a previous transaction"
        )));
    }
    Err(Box::new(PgError::error(format!(
        "execCurrentOf: cursor \"{cursor_name}\" holds a query desc Gravity's entry did not answer for"
    ))))
}

fn plan_has_capturable_scan(
    node: Option<::types_nodes::node_tree::Node<'_>>,
    rtable: &::types_nodes::NodeList<'_>,
) -> bool {
    use ::types_nodes::NodeTag;
    let Some(node) = node else {
        return false;
    };
    let plan = node.as_plan().expect("plan-tree node has a Plan prefix");
    match node.node_tag() {
        NodeTag::T_SeqScan => true,
        NodeTag::T_SampleScan
        | NodeTag::T_IndexScan
        | NodeTag::T_IndexOnlyScan
        | NodeTag::T_TidScan
        | NodeTag::T_TidRangeScan
        | NodeTag::T_BitmapHeapScan => true,
        NodeTag::T_Append => {
            let a = node.as_append().expect("T_Append");
            a.appendplans
                .iter()
                .any(|p| plan_has_capturable_scan(Some(p), rtable))
        }
        NodeTag::T_Result | NodeTag::T_Limit => plan_has_capturable_scan(plan.lefttree, rtable),
        NodeTag::T_SubqueryScan => plan_has_capturable_scan(
            node.as_subquery_scan().expect("T_SubqueryScan").subplan,
            rtable,
        ),
        _ => false,
    }
}

/// Seam impl (execmain_seams::cursor_plan_current_of_eligible, escalation
/// EX-CA-1): the §4.1 eligibility answer for PortalStart's arming decision.
pub(crate) fn cursor_plan_current_of_eligible_seam(
    pstmt: &::types_nodes::plannodes::PlannedStmt<'_>,
    scroll: bool,
) -> bool {
    // A SCROLL plan C would top with Material (planner.c:460-463) is never
    (!scroll || crate::execami::plan_implicit_scroll_ok(pstmt.planTree))
        && plan_has_capturable_scan(pstmt.planTree, &pstmt.rtable)
}

/// Seam impl (execmain_seams::cursor_plan_capture_batch_fill; SE-R41,
/// notes/se-r41-retire.md §3.1): is this ELIGIBLE plan the batch store-fill
/// shape — a bare T_SeqScan top (the exact `PlanStateNode::SeqScan` gate of
/// `cursor_store_batch_fill`; no Result/Limit/Append/SubqueryScan wrapper)?
/// TRUE moves the §4.2 capture inside the run (per accepted row, at the
/// emit surface — settle-safe by ordering), which is what lets the portal
/// drop the D-CA-2 fence eflags and lets the lane batch fill own the heap
/// fill cell. The caller only probes when the eligibility answer was TRUE.
pub(crate) fn cursor_plan_capture_batch_fill_seam(
    pstmt: &::types_nodes::plannodes::PlannedStmt<'_>,
) -> bool {
    use ::types_nodes::NodeTag;
    let Some(node) = pstmt.planTree else {
        return false;
    };
    node.node_tag() == NodeTag::T_SeqScan
}

#[track_caller]
#[cold]
#[inline(never)]
fn invalid_cursor_state(msg: String) -> Box<PgError> {
    Box::new(PgError::error(msg).with_sqlstate(ERRCODE_INVALID_CURSOR_STATE))
}


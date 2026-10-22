//! Rung 14 contract C5, `WHERE CURRENT OF` (docs/gravity/entry.md §11). A CURRENT-OF-eligible SCROLL cursor's store fill
//! arms a tid sidecar on its tuplestore receiver (pquery `fill_portal_store_to`): one `(tableoid, ctid)` per stored row,
//! which `exec_current_of`'s store-armed arm reads at `portalPos - 1`. The incumbent captures it from its scan state; a
//! served program has none, so D421 (V4R-82) serves the statement over a copy of the plan whose root emits two more
//! columns, the capturable scan's `ctid` and `tableoid`, threaded through the wrappers eligibility admits (Result, Limit,
//! SubqueryScan, Append: execcurrent.rs `plan_has_capturable_scan`). run.rs strips them from each row into the sidecar;
//! `current` answers `exec_current_of` from the sidecar as the store-armed arm does. An index-only scan emits the index
//! entry's heap tid (C5b-2); a named portal with row marks is C5b's (`current_rowmark`). D2281: a cursor with no store
//! (not SCROLL) is served over the same copy; run.rs keeps its last row's identity, which `current` reads instead.
use datum::Datum;
use execmain_seams::{cursor_plan_current_of_eligible, cursor_store_fill_enabled};
use mcx::Mcx;
use pquery::ActivePortal;
use types_core::catalog::{OIDOID, TIDOID};
use types_core::Oid;
use types_error::{PgError, PgResult, ERRCODE_INVALID_CURSOR_STATE};
use types_nodes::parsenodes::CURSOR_OPT_SCROLL;
use types_nodes::plannodes::{Append, BitmapHeapScan, ForeignScan, IndexOnlyScan, IndexScan, PlannedStmt, SampleScan, SeqScan, SubqueryScan, TidRangeScan, TidScan};
use types_nodes::primnodes::{Const, TargetEntry, Var, OUTER_VAR};
use types_nodes::{Node, NodeList, NodeTag};
use types_portal::PortalStrategy;
use types_tuple::ItemPointerData;

pub const WHY: &str = "entry: CURRENT OF-eligible scroll cursor (the store fill captures tids from the incumbent's scan)";

/// C5b: a named portal with row marks is served (WHERE CURRENT OF reads the lock host's curCtid: `current_rowmark`). Over a
/// plan that prunes at executor start, PG's es_rowmarks holds only the marks of es_unpruned_relids: the entry runs that
/// initial pruning at the portal's start (`unpruned`) and `current_rowmark` reads the same set. A LockRows over a sort /
/// material product is lowering's to decide, as for any other portal (a refusal stays where lowering raises it; the lock's
/// recheck over another pipeline's product is D652's). D2604: a family's lock over a sort is lowering's too (lockrows
/// marks.rs: each child's mark active on its own leaf's rows alone), so `current_rowmark` answers the current row's leaf.


/// The identity columns the plan copy's root emits after the statement's own: `ctid`, then `tableoid`.
pub const IDENTITY: usize = 2;

/// What the entry does with a statement at ExecutorStart under C5.
pub enum Cursor {
    /// Not a CURRENT-OF-eligible scroll cursor's statement.
    Plain,
    /// Refused by name: the statement's error (entry `removed`).
    Refused(&'static str),
    /// Served over `identity_plan`'s copy.
    Identity,
}

pub fn cursor(pstmt: &PlannedStmt<'static>) -> Cursor {
    let Some(portal) = ActivePortal() else { return Cursor::Plain };
    let p = portal.borrow();
    if p.strategy != PortalStrategy::PORTAL_ONE_SELECT || !p.queryDesc.is_null() || p.stmts.is_null() {
        return Cursor::Plain;
    }
    let Some(first) = pquery::stmt_list::resolve(p.stmts).first() else { return Cursor::Plain };
    if !core::ptr::eq(first, pstmt) {
        return Cursor::Plain;
    }
    // D2281: every named cursor (DECLARE's, PL/pgSQL's, a named protocol portal): a scroll cursor's store keeps one
    // identity per row, any other cursor's serve the last row's (run.rs `last_tid`)
    if p.name.is_empty() {
        return Cursor::Plain;
    }
    if !(cursor_plan_current_of_eligible::is_installed() && cursor_plan_current_of_eligible::call(pstmt, p.cursorOptions & CURSOR_OPT_SCROLL != 0)) {
        return Cursor::Plain;
    }
    // D2281's last-row identity is the scan's ctid lane, and a store-filled scroll cursor's (D3377) too: the copy's scan
    // reads `ctid` and `tableoid`. PGRCSERVE M3: over another table AM that scan takes the rows road (its ctid the AM's
    // TID, pgrcolumnar's stable ID, which WHERE CURRENT OF's TID scan fetches), but not under an invisible portal
    // (EXECUTE's: no CURRENT OF names it, its native road stays); an AM this server does not carry keeps the plain serve
    let heap = heap_only(pstmt, pstmt.planTree, !p.visible);
    let stored = p.cursorOptions & CURSOR_OPT_SCROLL != 0 && cursor_store_fill_enabled::is_installed() && cursor_store_fill_enabled::call();
    if !heap && !stored {
        return Cursor::Plain;
    }
    if !shape(pstmt.planTree) {
        return Cursor::Refused(WHY);
    }
    Cursor::Identity
}

/// D3580: `cursor`'s plan half for a named protocol portal the runner serves (no query desc, no active portal there; a
/// protocol portal is never a scroll cursor): `Identity` when WHERE CURRENT OF can name the plan's scan row, which the
/// runner reads from its run's scan (runner `portal_current_of`); `Plain` when it cannot (no capturable scan, another
/// table AM: execCurrentOf's own errors), `Refused` for a shape whose row no scan names alone.
pub fn plan_cursor(pstmt: &PlannedStmt<'static>) -> Cursor {
    if !(cursor_plan_current_of_eligible::is_installed() && cursor_plan_current_of_eligible::call(pstmt, false)) {
        return Cursor::Plain;
    }
    if !heap_only(pstmt, pstmt.planTree, false) {
        return Cursor::Plain;
    }
    if !shape(pstmt.planTree) {
        return Cursor::Refused(WHY);
    }
    Cursor::Identity
}

fn scanrelid(n: Node<'_>) -> Option<u32> {
    Some(match n.node_tag() {
        NodeTag::T_SeqScan => n.as_variant::<SeqScan>()?.scan.scanrelid,
        NodeTag::T_SampleScan => n.as_variant::<SampleScan>()?.scan.scanrelid,
        NodeTag::T_IndexScan => n.as_variant::<IndexScan>()?.scan.scanrelid,
        // C5b-2: an index-only scan's `ctid` is the index entry's heap tid (execCurrentOf's `xs_heaptid`, the source's RowId lane)
        NodeTag::T_IndexOnlyScan => n.as_variant::<IndexOnlyScan>()?.scan.scanrelid,
        NodeTag::T_TidScan => n.as_variant::<TidScan>()?.scan.scanrelid,
        NodeTag::T_TidRangeScan => n.as_variant::<TidRangeScan>()?.scan.scanrelid,
        NodeTag::T_BitmapHeapScan => n.as_variant::<BitmapHeapScan>()?.scan.scanrelid,
        _ => return None,
    })
}

/// The outer input of a plan node (`outerPlan`).
fn lefttree<'a>(n: Node<'a>) -> Option<Node<'a>> {
    n.as_plan().and_then(|p| p.lefttree)
}

/// Every row of the plan comes from a scan that can emit its heap identity (an Append: every arm).
fn shape(n: Option<Node<'_>>) -> bool {
    stack_depth_core::check_stack_depth_or_panic();
    let Some(n) = n else { return false };
    match n.node_tag() {
        NodeTag::T_Result | NodeTag::T_Limit => shape(lefttree(n)),
        NodeTag::T_SubqueryScan => shape(n.as_variant::<SubqueryScan>().and_then(|s| s.subplan)),
        // an arm with no capturable scan (eligibility's `any`) carries no identity: a projecting one emits a NULL `ctid`
        // and `tableoid` 0, the (0, 0) the incumbent's capture stores for a row no scan of the spine holds
        NodeTag::T_Append => n.as_variant::<Append>().is_some_and(|a| a.appendplans.iter().any(|c| shape(Some(c))) && a.appendplans.iter().all(|c| shape(Some(c)) || nullable(c))),
        _ => scanrelid(n).is_some(),
    }
}

/// An Append arm with no capturable scan that can carry a constant identity (`no_identity`): a projecting node, or a
/// Sort / Material / Limit / Unique (whose target list mirrors its input's) over one.
fn nullable(n: Node<'_>) -> bool {
    stack_depth_core::check_stack_depth_or_panic();
    projects(n) || (passes(n) && lefttree(n).is_some_and(nullable))
}

/// A node whose target list is its input's columns (createplan.c's non-projecting kinds) and whose rows do not depend on
/// a column it does not name: two more columns pass through it.
fn passes(n: Node<'_>) -> bool {
    matches!(n.node_tag(), NodeTag::T_Sort | NodeTag::T_IncrementalSort | NodeTag::T_Material | NodeTag::T_Limit | NodeTag::T_Unique)
}

/// A plan node that evaluates its own target list (createplan.c `is_projection_capable_plan`), so an identity of two
/// constants can be appended to it: the projecting kinds an Append arm with no capturable scan commonly is, and a scan
/// below a node execcurrent's search does not pass (a Sort's input: no identity of the root row).
fn projects(n: Node<'_>) -> bool {
    scanrelid(n).is_some()
        || matches!(
            n.node_tag(),
            NodeTag::T_SubqueryScan
                | NodeTag::T_Result
                | NodeTag::T_Agg
                | NodeTag::T_Group
                | NodeTag::T_WindowAgg
                | NodeTag::T_NestLoop
                | NodeTag::T_MergeJoin
                | NodeTag::T_HashJoin
                | NodeTag::T_FunctionScan
                | NodeTag::T_ValuesScan
                | NodeTag::T_CteScan
                | NodeTag::T_WorkTableScan
                | NodeTag::T_NamedTuplestoreScan
                | NodeTag::T_TableFuncScan
        )
}

/// Every scan `shape` accepted reads a relation of a table AM this server carries (heap, or another on the rows road:
/// PGRCSERVE M3; heap alone when `heap`), whose rows carry the TIDs D2281's last-row identity reads.
fn heap_only(pstmt: &PlannedStmt<'_>, n: Option<Node<'_>>, heap: bool) -> bool {
    stack_depth_core::check_stack_depth_or_panic();
    let Some(n) = n else { return false };
    match n.node_tag() {
        NodeTag::T_Result | NodeTag::T_Limit => heap_only(pstmt, lefttree(n), heap),
        NodeTag::T_SubqueryScan => heap_only(pstmt, n.as_variant::<SubqueryScan>().and_then(|s| s.subplan), heap),
        NodeTag::T_Append => n.as_variant::<Append>().is_some_and(|a| a.appendplans.iter().filter(|c| shape(Some(*c))).all(|c| heap_only(pstmt, Some(c), heap))),
        _ => scanrelid(n)
            .and_then(|r| pstmt.rtable.iter().nth(r as usize - 1))
            .and_then(|e| e.as_range_tbl_entry())
            .is_some_and(|e| lsyscache::get_rel_relam(e.relid).is_ok_and(|am| am == HEAP_TABLE_AM_OID || (!heap && tableam_vocab::TableAm::of_relam(am).is_some()))),
    }
}

const HEAP_TABLE_AM_OID: Oid = 2;

/// A copy of `pstmt` in `mcx` whose root emits the capturable scan's `ctid` and `tableoid` after its own columns;
/// `None` when a level cannot thread them (`shape` said it could: a defensive answer, the caller refuses).
pub fn identity_plan(mcx: Mcx<'static>, pstmt: &PlannedStmt<'static>) -> PgResult<Option<&'static PlannedStmt<'static>>> {
    let copy = copyfuncs::copy_utility_planned_stmt(mcx, pstmt)?;
    let Some(root) = copy.planTree else { return Ok(None) };
    Ok(add(mcx, root, false)?.map(|_| copy))
}

fn var<'m>(mcx: Mcx<'m>, varno: i32, varattno: i16, vartype: Oid) -> PgResult<Node<'m>> {
    Node::mk(mcx, Var { varno, varattno, vartype, vartypmod: -1, varnosyn: varno.max(0) as u32, varattnosyn: varattno, location: -1, ..Var::default() })
}

/// Thread the identity through `n`: the resno of its `ctid` column (`tableoid` the next).
/// `junk` (D2708): `n` is a SubqueryScan's subplan, so its two new entries are resjunk. A whole-row Var of the subquery
/// (`SELECT row_to_json(ss) FROM (..) ss`) is the subplan's non-junk entries (C `ExecInitWholeRowVar`'s junk filter,
/// decompose subplan.rs `record_row`), so the row keeps the statement's own columns; the scan above reads the two by resno.
fn add(mcx: Mcx<'static>, n: Node<'static>, junk: bool) -> PgResult<Option<i16>> {
    stack_depth_core::check_stack_depth()?;
    let Some(plan) = n.as_plan() else { return Ok(None) };
    let (varno, at) = match n.node_tag() {
        NodeTag::T_Result | NodeTag::T_Limit => match plan.lefttree {
            Some(c) => match add(mcx, c, false)? {
                Some(r) => (OUTER_VAR, r),
                None => return Ok(None),
            },
            None => return Ok(None),
        },
        NodeTag::T_SubqueryScan => {
            let s = n.as_variant::<SubqueryScan>().expect("T_SubqueryScan");
            match s.subplan.map(|c| add(mcx, c, true)).transpose()?.flatten() {
                Some(r) => (s.scan.scanrelid as i32, r),
                None => return Ok(None),
            }
        }
        NodeTag::T_Append => {
            let mut at = None;
            for c in n.as_variant::<Append>().expect("T_Append").appendplans.iter() {
                let arm = if shape(Some(c)) { add(mcx, c, false)? } else if nullable(c) { no_identity(mcx, c)? } else { None };
                match (arm, at) {
                    (None, _) => return Ok(None),
                    (Some(r), Some(a)) if r != a => return Ok(None),
                    (Some(r), _) => at = Some(r),
                }
            }
            match at {
                Some(r) => (OUTER_VAR, r),
                None => return Ok(None),
            }
        }
        _ => match scanrelid(n) {
            Some(rel) => (rel as i32, 0),
            None => return Ok(None),
        },
    };
    // a scan's own system columns; a wrapper's the child's two new columns
    let (ctid, toid) = if at == 0 { (var(mcx, varno, -1, TIDOID)?, var(mcx, varno, -6, OIDOID)?) } else { (var(mcx, varno, at, TIDOID)?, var(mcx, varno, at + 1, OIDOID)?) };
    let base = plan.targetlist.len() as i16;
    let mut tl = NodeList::with_capacity(mcx, plan.targetlist.len() + IDENTITY)?;
    for te in plan.targetlist.iter() {
        tl.lappend(mcx, te)?;
    }
    for (k, (expr, name)) in [(ctid, "ctid"), (toid, "tableoid")].into_iter().enumerate() {
        tl.lappend(mcx, Node::mk(mcx, TargetEntry { expr, resno: base + 1 + k as i16, resname: Some(name), ressortgroupref: 0, resorigtbl: 0, resorigcol: 0, resjunk: junk })?)?;
    }
    super::raw::set_targetlist(n, tl);
    Ok(Some(base + 1))
}

/// An Append arm with no capturable scan (`shape` false, `nullable`): a NULL `ctid` and `tableoid` 0 after its own columns,
/// at the position its siblings' identities take (equal target lists), made by its first projecting node and passed up
/// through the Sort / Material / Limit / Unique above it as their input's columns; the resno of the `ctid` column.
fn no_identity<'m>(mcx: Mcx<'m>, n: Node<'m>) -> PgResult<Option<i16>> {
    stack_depth_core::check_stack_depth()?;
    let Some(plan) = n.as_plan() else { return Ok(None) };
    let (ctid, toid) = if projects(n) {
        (
            Node::mk(mcx, Const { consttype: TIDOID, consttypmod: -1, constcollid: 0, constlen: 6, constvalue: Datum::null(), constisnull: true, constbyval: false, location: -1 })?,
            Node::mk(mcx, Const { consttype: OIDOID, consttypmod: -1, constcollid: 0, constlen: 4, constvalue: Datum::from_oid(0), constisnull: false, constbyval: true, location: -1 })?,
        )
    } else {
        let Some(c) = plan.lefttree.filter(|_| passes(n)) else { return Ok(None) };
        let Some(at) = no_identity(mcx, c)? else { return Ok(None) };
        if at != plan.targetlist.len() as i16 + 1 {
            return Ok(None);
        }
        (var(mcx, OUTER_VAR, at, TIDOID)?, var(mcx, OUTER_VAR, at + 1, OIDOID)?)
    };
    let base = plan.targetlist.len() as i16;
    let mut tl = NodeList::with_capacity(mcx, plan.targetlist.len() + IDENTITY)?;
    for te in plan.targetlist.iter() {
        tl.lappend(mcx, te)?;
    }
    for (k, (expr, name)) in [(ctid, "ctid"), (toid, "tableoid")].into_iter().enumerate() {
        tl.lappend(mcx, Node::mk(mcx, TargetEntry { expr, resno: base + 1 + k as i16, resname: Some(name), ressortgroupref: 0, resorigtbl: 0, resorigcol: 0, resjunk: false })?)?;
    }
    super::raw::set_targetlist(n, tl);
    Ok(Some(base + 1))
}

/// The scan of `table_oid` the plan's root row comes from, as execcurrent's `search_plan_tree` walks the plan state:
/// Result / Limit / SubqueryScan to their input, an Append to its one matching arm (none on two).
fn scans(pstmt: &PlannedStmt<'_>, n: Option<Node<'_>>, table_oid: Oid) -> bool {
    stack_depth_core::check_stack_depth_or_panic();
    let Some(n) = n else { return false };
    match n.node_tag() {
        NodeTag::T_Result | NodeTag::T_Limit => scans(pstmt, lefttree(n), table_oid),
        NodeTag::T_SubqueryScan => scans(pstmt, n.as_variant::<SubqueryScan>().and_then(|s| s.subplan), table_oid),
        NodeTag::T_Append => n.as_variant::<Append>().is_some_and(|a| a.appendplans.iter().filter(|c| scans(pstmt, Some(*c), table_oid)).count() == 1),
        _ => scanrelid(n).and_then(|r| pstmt.rtable.iter().nth(r as usize - 1)).and_then(|e| e.as_range_tbl_entry()).is_some_and(|e| e.relid == table_oid),
    }
}

/// execCurrentOf without FOR UPDATE / SHARE over a cursor Gravity serves: no scan of `table_oid` that execcurrent's
/// `search_plan_tree` could reach (Result / Limit / SubqueryScan to their input, any Append arm, a scan node of the
/// table), so PG's own error for certain; a reachable one is the typed refusal's (the position is M8's). D3723: a
/// MergeAppend stops the search, as in execCurrent.c (its inputs are all active, so none is known to hold the cursor's
/// row): a NO SCROLL cursor over one is not simply updatable either.
pub fn no_scan_of(pstmt: &PlannedStmt<'_>, table_oid: Oid) -> bool {
    fn reaches(pstmt: &PlannedStmt<'_>, n: Option<Node<'_>>, table_oid: Oid) -> bool {
        stack_depth_core::check_stack_depth_or_panic();
        let Some(n) = n else { return false };
        let rel = match n.node_tag() {
            NodeTag::T_Result | NodeTag::T_Limit => return reaches(pstmt, lefttree(n), table_oid),
            NodeTag::T_SubqueryScan => return reaches(pstmt, n.as_variant::<SubqueryScan>().and_then(|s| s.subplan), table_oid),
            NodeTag::T_Append => return n.as_variant::<Append>().is_some_and(|a| a.appendplans.iter().any(|c| reaches(pstmt, Some(c), table_oid))),
            NodeTag::T_MergeAppend => return false,
            NodeTag::T_IndexOnlyScan => n.as_variant::<IndexOnlyScan>().map(|s| s.scan.scanrelid),
            NodeTag::T_ForeignScan => n.as_variant::<ForeignScan>().map(|s| s.scan.scanrelid),
            _ => scanrelid(n),
        };
        rel.filter(|&r| r > 0).and_then(|r| pstmt.rtable.iter().nth(r as usize - 1)).and_then(|e| e.as_range_tbl_entry()).is_some_and(|e| e.relid == table_oid)
    }
    pstmt.rowMarks.is_nil() && !reaches(pstmt, pstmt.planTree, table_oid)
}

/// PG's error for a cursor with no simply updatable scan of the table.
pub fn not_simply_updatable(cursor_name: &str, table_name: &str) -> Box<PgError> {
    invalid(format!("cursor \"{cursor_name}\" is not a simply updatable scan of table \"{table_name}\"")) // alloc: Cold
}

fn invalid(msg: String) -> Box<PgError> {
    Box::new(PgError::error(msg).with_sqlstate(ERRCODE_INVALID_CURSOR_STATE)) // alloc: Cold
}

/// `exec_current_of` for a portal served over the identity copy (`pstmt` the statement's own plan): the store-armed
/// arm of execcurrent.rs over the sidecar, its errors verbatim.
/// `last`: the serve's last row `(tableoid, packed tid)`, read when the portal has no store (D2281).
pub fn current(portal: &types_portal::Portal<'static>, pstmt: &PlannedStmt<'_>, last: Option<(u32, u64)>, cursor_name: &str, table_oid: Oid, table_name: &str) -> PgResult<Option<ItemPointerData>> {
    let (at_start, at_end, tid_store, pos) = {
        let p = portal.borrow();
        (p.atStart, p.atEnd, p.cursorTidStore, p.portalPos)
    };
    if !scans(pstmt, pstmt.planTree, table_oid) {
        return Err(invalid(format!("cursor \"{cursor_name}\" is not a simply updatable scan of table \"{table_name}\""))); // alloc: Cold
    }
    if at_start || at_end {
        return Err(invalid(format!("cursor \"{cursor_name}\" is not positioned on a row"))); // alloc: Cold
    }
    let row = if tid_store.is_null() { last } else if pos == 0 { None } else { tuplestore::hold::tidstore_get(tid_store, (pos - 1) as i64)? };
    let Some(row) = row else {
        return Err(invalid(format!("cursor \"{cursor_name}\" is not a simply updatable scan of table \"{table_name}\""))); // alloc: Cold
    };
    answer(row, cursor_name, table_oid, table_name)
}

/// D2798: `current` while the cursor's own run evaluates the user code that asks (its desc in use): the row the identity
/// program published before that row's quals and projection (`Serve::published`), the scan slot execCurrentOf reads
/// then; not the store, whose rows end before this one, nor the last emitted row. Nothing published (a source with no
/// locator) is no row Gravity can name: the typed refusal, never another row.
pub fn current_running(portal: &types_portal::Portal<'static>, pstmt: &PlannedStmt<'_>, published: Option<(u32, u64)>, cursor_name: &str, table_oid: Oid, table_name: &str) -> PgResult<Option<ItemPointerData>> {
    let (at_start, at_end) = {
        let p = portal.borrow();
        (p.atStart, p.atEnd)
    };
    if !scans(pstmt, pstmt.planTree, table_oid) {
        return Err(not_simply_updatable(cursor_name, table_name));
    }
    if at_start || at_end {
        return Err(invalid(format!("cursor \"{cursor_name}\" is not positioned on a row"))); // alloc: Cold
    }
    match published {
        Some(row) => answer(row, cursor_name, table_oid, table_name),
        None => Err(unsupported(cursor_name)),
    }
}

/// The served cursor's typed refusal of WHERE CURRENT OF: a position this entry cannot name.
pub fn unsupported(cursor_name: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("cursor \"{cursor_name}\" is served by Gravity: WHERE CURRENT OF is not supported on it yet")).with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED)) // alloc: Cold
}

/// The current row `(tableoid, block << 16 | offset)` as execCurrentOf's answer for `table_oid`.
fn answer((oid, packed): (u32, u64), cursor_name: &str, table_oid: Oid, table_name: &str) -> PgResult<Option<ItemPointerData>> {
    if oid != table_oid {
        // another inheritance child produced the current row
        return Ok(None);
    }
    let tid = ItemPointerData::new((packed >> 16) as u32, (packed & 0xffff) as u16);
    if tid.ip_posid == 0 {
        return Err(invalid(format!("cursor \"{cursor_name}\" is not a simply updatable scan of table \"{table_name}\""))); // alloc: Cold
    }
    Ok(Some(tid))
}

/// C5b: `exec_current_of` for a served portal with row marks: execCurrentOf's es_rowmarks arm, its checks in its order.
/// The marks are the plan's that InitPlan keeps in es_rowmarks: no parent mark, and a relation's only when it is in
/// es_unpruned_relids (`unpruned`: the portal start's initial pruning; None, no pruning at start: the unprunable set);
/// `locked` is the lock host's curCtid (`Locker::current`: the row the last lock left, the newest version when it followed
/// the update chain). A lock of another leaf means another inheritance child produced the current row: `None`, PG's false.
pub fn current_rowmark(portal: &types_portal::Portal<'static>, pstmt: &PlannedStmt<'_>, unpruned: Option<&[i32]>, locked: gravity_abi::ScanPos, cursor_name: &str, table_oid: Oid, table_name: &str) -> PgResult<Option<ItemPointerData>> {
    let mut found = 0;
    for rc in pstmt.rowMarks.iter().filter_map(|n| n.as_plan_row_mark()) {
        if rc.isParent || !rc.markType.requires_row_share_lock() {
            continue;
        }
        let Some(rte) = pstmt.rtable.iter().nth(rc.rti as usize - 1).and_then(|e| e.as_range_tbl_entry()) else { continue };
        let kept = match unpruned {
            Some(u) => u.contains(&(rc.rti as i32)),
            None => pstmt.unprunableRelids.is_member(rc.rti as i32),
        };
        if rte.rtekind == types_nodes::parsenodes::RTEKind::RTE_RELATION && !kept {
            continue;
        }
        if rte.relid == table_oid {
            found += 1;
            if found > 1 {
                return Err(invalid(format!("cursor \"{cursor_name}\" has multiple FOR UPDATE/SHARE references to table \"{table_name}\""))); // alloc: Cold
            }
        }
    }
    if found == 0 {
        return Err(invalid(format!("cursor \"{cursor_name}\" does not have a FOR UPDATE/SHARE reference to table \"{table_name}\""))); // alloc: Cold
    }
    let (at_start, at_end) = {
        let p = portal.borrow();
        (p.atStart, p.atEnd)
    };
    if at_start || at_end {
        return Err(invalid(format!("cursor \"{cursor_name}\" is not positioned on a row"))); // alloc: Cold
    }
    if !locked.is_valid() || locked.rel != table_oid || locked.off == 0 {
        return Ok(None);
    }
    Ok(Some(ItemPointerData::new(locked.block, locked.off)))
}

/// One served row's identity columns (`ctid` datum, `tableoid`) as the sidecar's `(tableoid, block << 16 | offset)`.
pub fn pack(ctid: u64, tableoid: u64) -> (u32, u64) {
    if ctid == 0 {
        return (tableoid as u32, 0);
    }
    let t = super::raw::tid(ctid);
    (tableoid as u32, ((types_tuple::itemptr::ItemPointerGetBlockNumberNoCheck(t) as u64) << 16) | t.ip_posid as u64)
}

/// D424 (M1-FLAGS census accounting): a FETCH / MOVE that runs a cursor's program logs one verdict line in a statement's
/// shape, so the served-diff credits the DECLARE from the log. True when the statement at hand is a FETCH / MOVE and the
/// active portal is a visible named cursor (DECLARE's; not the unnamed / protocol portals). The query string may hold
/// several statements (a simple-query `DECLARE c CURSOR FOR ..; FETCH 5 FROM c`, whose debug query string is the whole
/// text): any of them that is a FETCH / MOVE counts (CAND-02 / CAND-M42: the first word alone was DECLARE, so the cursor's
/// FETCH logged no verdict line and the rig read the statement as never offered).
fn portal_fetch() -> bool {
    let Some(portal) = ActivePortal() else { return false };
    {
        let p = portal.borrow();
        if !p.visible || p.name.is_empty() {
            return false;
        }
    }
    elog::with_debug_query_string(|q| q.is_some_and(has_fetch))
}

/// Whether a statement of the query string `q` starts with FETCH or MOVE: each `;`-separated statement's first word, the
/// separators found outside quotes, comments and dollar quotes.
fn has_fetch(q: &str) -> bool {
    let is_fetch = |s: &str| matches!(first_word(s), Some(w) if w.eq_ignore_ascii_case("fetch") || w.eq_ignore_ascii_case("move"));
    let b = q.as_bytes();
    let (mut i, mut start) = (0, 0);
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => {
                let quote = b[i];
                i += 1;
                while i < b.len() && b[i] != quote {
                    i += 1;
                }
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = q[i + 2..].find("*/").map_or(b.len(), |e| i + 2 + e + 1);
            }
            b'$' => {
                // a dollar quote `$tag$ .. $tag$` (tag: letters, digits, `_`; not a `$1` parameter)
                let tag_end = q[i + 1..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).map(|e| i + 1 + e);
                if let Some(e) = tag_end.filter(|&e| b[e] == b'$' && !b.get(i + 1).is_some_and(u8::is_ascii_digit)) {
                    let tag = &q[i..=e];
                    i = q[e + 1..].find(tag).map_or(b.len(), |f| e + 1 + f + tag.len() - 1);
                }
            }
            b';' => {
                if is_fetch(&q[start..i]) {
                    return true;
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    start < b.len() && is_fetch(&q[start..])
}

/// The first SQL word of `q`: leading blanks and `--` / `/* */` comments skipped.
fn first_word(mut q: &str) -> Option<&str> {
    loop {
        q = q.trim_start();
        if let Some(r) = q.strip_prefix("--") {
            q = r.split_once('\n').map_or("", |(_, r)| r);
        } else if let Some(r) = q.strip_prefix("/*") {
            q = r.split_once("*/").map_or("", |(_, r)| r);
        } else {
            break;
        }
    }
    let n = q.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(q.len());
    (n > 0).then(|| &q[..n])
}

/// The line itself, under the report knob as every verdict line: `served=` / `fallthrough=` are the session's counts
/// (a FETCH line counts nothing: the DECLARE's verdict is the statement's), then the FETCH text. `last` is the desc's
/// mark of the statement it last logged (its start timestamp): one line per FETCH / MOVE statement, as a store fill may
/// run the program twice under one FETCH (the target row, then the step past it). The portal test comes first (cheap:
/// no FETCH, no env read); the knob is the backend's own env read.
fn fetch_line(last: Option<&mut i64>, msg: core::fmt::Arguments<'_>) {
    if !portal_fetch() || !crate::inputs::knobs::seam::report_on() {
        return;
    }
    if let Some(last) = last {
        let start = xact::GetCurrentStatementStartTimestamp();
        if core::mem::replace(last, start) == start {
            return;
        }
    }
    let (served, fallthrough) = crate::with_backend(|b| (b.served, b.fallthrough)).unwrap_or((0, 0));
    let line = elog::with_debug_query_string(|q| format!("{msg} served={served} fallthrough={fallthrough} :: {}", q.unwrap_or("").replace('\n', " "))); // alloc: Cold
    let _ = elog::ereport(types_error::LOG).errmsg(line).errhidestmt(true).errhidecontext(true).finish(types_error::ErrorLocation::new("gravity", 0, "report"));
}

/// A FETCH / MOVE ran the served program: `rows` it produced this call.
pub fn fetch_served(last: &mut i64, shape: &str, rows: u64, admit: &str) {
    fetch_line(Some(last), format_args!("gravity: served {shape} rows={rows} admit={admit} via=portal:fetch"));
}

#[cfg(test)]
mod has_fetch_tests {
    use super::has_fetch;

    #[test]
    fn finds_a_fetch_in_any_statement() {
        assert!(has_fetch("FETCH 5 FROM c"));
        assert!(has_fetch("  -- x\n/* y */ move next in c"));
        assert!(has_fetch("/*v5p:ad-0001#1*/ DECLARE cur1 CURSOR FOR SELECT id FROM ord ORDER BY id; FETCH 24 FROM cur1"));
        assert!(has_fetch("DECLARE c CURSOR FOR SELECT 1; FETCH 5 FROM c; FETCH 2 FROM c"));
        assert!(!has_fetch("DECLARE c CURSOR FOR SELECT 1"));
        assert!(!has_fetch("COMMIT"));
        assert!(!has_fetch("SELECT 'a; fetch 1 from c' FROM t"));
        assert!(!has_fetch("SELECT $q$; fetch 1 from c$q$, $$; move c$$"));
        assert!(!has_fetch("SELECT \"x;fetch\" FROM t -- ; fetch 1 from c"));
        assert!(!has_fetch("SELECT 1 /* ; fetch 1 from c */"));
        assert!(has_fetch("SELECT $1; FETCH 1 FROM c"));
        assert!(has_fetch("SELECT 'é;'; FETCH 1 FROM c"));
    }
}

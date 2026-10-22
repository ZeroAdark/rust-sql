use std::rc::Rc;

use tcop_dest::DestReceiver;
use types_dest::CommandDest;
use types_error::PgResult;
use types_nodes::nodes_enums::CmdType;
use types_nodes::plannodes::PlannedStmt;
use types_portal::{CachedPlanHandle, ParamListHandle, QueryDescHandle, QueryEnvHandle};
use types_scan::sdir::ScanDirection;
use types_core::instrument::{
    AggregateInstrumentation, BitmapHeapScanInstrumentation, HashInstrumentation,
    IncrementalSortInfo, Instrumentation, MemoizeInstrumentation, TuplesortInstrumentation,
    TuplestoreInstrumentation,
};
use types_snapshot::SnapshotData;
use types_tuple::TupleDescData;

pub type Snapshot = Rc<SnapshotData<'static>>;

seam_core::seam!(
    // Retention contract: caller keeps plannedstmt/source_text alive until free_query_desc (C's raw-pointer rule).
    pub fn create_query_desc<'p, 'a, 's>(
        plannedstmt: &'p PlannedStmt<'a>,
        source_text: &'s str,
        snapshot: Option<Snapshot>,
        crosscheck_snapshot: Option<Snapshot>,
        dest: CommandDest,
        params: ParamListHandle,
        query_env: QueryEnvHandle,
        instrument_options: i32,
    ) -> PgResult<QueryDescHandle>
);

seam_core::seam!(
    pub fn free_query_desc(query_desc: QueryDescHandle)
);

seam_core::seam!(
    // Cached-plan handle for the next create_query_desc on this backend (no C counterpart).
    pub fn note_cplan_for_query_desc(cplan: CachedPlanHandle)
);

seam_core::seam!(
    // Abort-path reclamation: C frees the QueryDesc with the portal context and never runs ExecutorEnd.
    pub fn release_query_desc(query_desc: QueryDescHandle)
);

seam_core::seam!(
    pub fn executor_start(query_desc: QueryDescHandle, eflags: i32) -> PgResult<()>
);

seam_core::seam!(
    pub fn executor_run<'d, 'mcx>(
        query_desc: QueryDescHandle,
        direction: ScanDirection,
        count: u64,
        dest: &'d mut DestReceiver<'mcx>,
    ) -> PgResult<()>
);

seam_core::seam!(
    pub fn executor_finish(query_desc: QueryDescHandle) -> PgResult<()>
);

seam_core::seam!(
    // false = the normal ExecutorEnd + FreeQueryDesc ran instead; the caller's cleanup hook is consumed either way.
    pub fn executor_finish_and_park(query_desc: QueryDescHandle) -> PgResult<bool>
);

seam_core::seam!(
    // false = param shape mismatch; caller sheds the retained executor and builds fresh via ExecutorStart.
    pub fn executor_rearm(
        query_desc: QueryDescHandle,
        snapshot: Option<Snapshot>,
        params: ParamListHandle,
    ) -> PgResult<bool>
);

seam_core::seam!(
    pub fn executor_rewind(query_desc: QueryDescHandle) -> PgResult<()>
);

seam_core::seam!(
    pub fn executor_end(query_desc: QueryDescHandle) -> PgResult<()>
);

seam_core::tap!(pub fn tap_executor_start(h: QueryDescHandle));
seam_core::tap!(pub fn tap_executor_run(h: QueryDescHandle));
seam_core::tap!(pub fn tap_executor_finish(h: QueryDescHandle));
seam_core::tap!(pub fn tap_executor_end(h: QueryDescHandle) -> PgResult<()>);
// _leave taps: C consumers wrap standard_ExecutorRun/Finish in PG_TRY to
// track nesting depth; the seam guarantees the leave fires on the error path
// too (PG_FINALLY parity).
seam_core::tap!(pub fn tap_executor_run_leave(h: QueryDescHandle));
seam_core::tap!(pub fn tap_executor_finish_leave(h: QueryDescHandle));
seam_core::tap!(pub fn tap_executor_hooks_active() -> bool);

pub struct HookQd<'a> {
    pub plannedstmt: &'static PlannedStmt<'static>,
    pub source_text: &'static str,
    pub params: ParamListHandle,
    pub instrument_options: &'a mut i32,
    pub totaltime: &'a mut Option<Box<Instrumentation>>,
    pub total_processed: u64,
    pub parallel_workers_to_launch: i32,
    pub parallel_workers_launched: i32,
    pub explain_only: bool,
}

seam_core::seam!(
    pub fn with_hook_qd<'f>(query_desc: QueryDescHandle, f: &'f mut dyn FnMut(&mut HookQd<'_>))
);

seam_core::seam!(
    pub fn query_desc_es_processed(query_desc: QueryDescHandle) -> u64
);

seam_core::seam!(
    pub fn query_desc_snapshot(query_desc: QueryDescHandle) -> Option<Snapshot>
);

seam_core::seam!(
    pub fn query_desc_result_tupdesc(
        query_desc: QueryDescHandle,
    ) -> Option<Rc<TupleDescData<'static>>>
);

seam_core::seam!(
    pub fn query_desc_operation(query_desc: QueryDescHandle) -> CmdType
);

seam_core::seam!(
    pub fn query_desc_instrument(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<Instrumentation>
);

pub struct TrigInstrReport {
    /// Which EState list the relation came from: 0 = es_opened_result_relations, 1 = es_tuple_routing_result_relations, 2 = es_trig_target_relations (C reports them in that order).
    pub kind: u8,
    pub relname: String,
    pub triggers: Vec<(String, types_core::Oid, Instrumentation)>,
}

seam_core::seam!(
    pub fn query_desc_trigger_instrument(query_desc: QueryDescHandle) -> Vec<TrigInstrReport>
);

seam_core::seam!(
    // None = no initial pruning.
    pub fn query_desc_prune_result(
        query_desc: QueryDescHandle,
        part_prune_index: i32,
    ) -> Option<Vec<i32>>
);

seam_core::seam!(
    pub fn query_desc_rti_unpruned(query_desc: QueryDescHandle, rti: i32) -> Option<bool>
);

seam_core::seam!(
    pub fn query_desc_fdw_batch_size(query_desc: QueryDescHandle, rti: u32) -> Option<i32>
);

seam_core::seam!(
    pub fn query_desc_agg_instrument(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<AggregateInstrumentation>
);

seam_core::seam!(
    pub fn query_desc_sort_instrument(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<TuplesortInstrumentation>
);

seam_core::seam!(
    pub fn query_desc_incsort_instrument(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<IncrementalSortInfo>
);

seam_core::seam!(
    pub fn query_desc_index_instrument(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<u64>
);

seam_core::seam!(
    pub fn query_desc_hash_instrument(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<HashInstrumentation>
);

seam_core::seam!(
    pub fn query_desc_tuplestore_instrument(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<TuplestoreInstrumentation>
);

seam_core::seam!(
    pub fn query_desc_memoize_instrument(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<MemoizeInstrumentation>
);

seam_core::seam!(
    pub fn query_desc_bitmap_instrument(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<BitmapHeapScanInstrumentation>
);

seam_core::seam!(
    pub fn query_desc_index_searches(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<u64>
);

seam_core::seam!(
    pub fn query_desc_jit_instr(query_desc: QueryDescHandle) -> (i32, i32, u64)
);

seam_core::seam!(
    pub fn query_desc_runtime_ea_refusals(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<Vec<(&'static str, &'static str)>>
);

seam_core::seam!(
    pub fn query_desc_runtime_ea_pipeline(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<Vec<types_core::instrument::RuntimeEaPipeline>>
);

seam_core::seam!(
    pub fn query_desc_engine_events(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<Vec<(types_core::instrument::EngineKindWire, &'static str, &'static str)>>
);

seam_core::seam!(
    pub fn query_desc_gravity_node(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<types_core::instrument::GravityNodeRows>
);

seam_core::seam!(
    pub fn query_desc_gravity_report(
        query_desc: QueryDescHandle,
    ) -> Option<types_core::instrument::GravityReport>
);

seam_core::seam!(
    pub fn query_desc_gravity_heap_fetches(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<u64>
);

seam_core::seam!(
    pub fn query_desc_workers_launched(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<i32>
);

seam_core::seam!(
    pub fn query_desc_merge_instrument(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<(f64, f64, f64)>
);

seam_core::seam!(
    pub fn query_desc_worker_instrument(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<Vec<Instrumentation>>
);

seam_core::seam!(
    pub fn query_desc_worker_sort_instrument(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<Vec<(i32, TuplesortInstrumentation)>>
);

seam_core::seam!(
    pub fn query_desc_worker_bitmap_instrument(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<Vec<(i32, BitmapHeapScanInstrumentation)>>
);

seam_core::seam!(
    pub fn query_desc_worker_memoize_instrument(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<Vec<(i32, MemoizeInstrumentation)>>
);

seam_core::seam!(
    pub fn query_desc_worker_incsort_instrument(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<Vec<(i32, IncrementalSortInfo)>>
);

seam_core::seam!(
    pub fn query_desc_worker_agg_instrument(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
    ) -> Option<Vec<(i32, AggregateInstrumentation)>>
);

seam_core::seam!(
    pub fn query_desc_foreign_explain<'e>(
        query_desc: QueryDescHandle,
        plan_node_id: i32,
        flags: types_nodes::FdwExplainFlags,
        emit: &'e mut dyn FnMut(&str, types_nodes::FdwExplainProp<'_>) -> PgResult<()>,
    ) -> PgResult<()>
);

seam_core::seam!(
    pub fn exec_clean_type_from_tl<'p, 'a>(
        pstmt: &'p PlannedStmt<'a>,
    ) -> PgResult<Rc<TupleDescData<'static>>>
);

seam_core::seam!(
    // None = valid cursor not on a row of this table.
    pub fn exec_current_of<'a>(
        cursor_name: Option<&'a str>,
        cursor_param: i32,
        table_oid: types_core::Oid,
        table_name: &'a str,
    ) -> PgResult<Option<types_tuple::ItemPointerData>>
);

seam_core::seam!(
    pub fn exec_check_permissions<'p, 'a>(
        perm_infos: &'p types_nodes::NodeList<'a>,
    ) -> PgResult<()>
);

seam_core::seam!(
    // false ⇒ the per-table walk can never succeed: no capture, error arms only.
    pub fn cursor_plan_current_of_eligible<'p, 'a>(
        pstmt: &'p PlannedStmt<'a>,
        scroll: bool,
    ) -> bool
);

seam_core::seam!(
    // does a rewind of this SCROLL portal replay its cursor store? True
    // exactly where C's planner wraps a SCROLL plan in Material
    // (planner.c:444-451, !ExecSupportsBackwardScan of the top plan), whose
    // rewind replays instead of re-running the plan.
    pub fn cursor_plan_rewind_replays<'p, 'a>(pstmt: &'p PlannedStmt<'a>) -> bool
);

seam_core::seam!(
    // false ⇒ the portal store stands in for C's Material wrap.
    pub fn plan_supports_backward_scan<'p, 'a>(pstmt: &'p PlannedStmt<'a>) -> bool
);

seam_core::seam!(
    pub fn cursor_capture_current(
        query_desc: QueryDescHandle,
    ) -> PgResult<Option<(types_core::Oid, u64)>>
);

seam_core::seam!(
    pub fn cursor_store_fill_enabled() -> bool
);

seam_core::seam!(
    pub fn cursor_store_armed_note()
);

seam_core::seam!(
    pub fn cursor_plan_capture_batch_fill<'p, 'a>(
        pstmt: &'p PlannedStmt<'a>,
    ) -> bool
);

pub mod p8ctx;


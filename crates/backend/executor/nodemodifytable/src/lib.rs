//! What the server keeps of nodeModifyTable.c / execMain.c's per-row write checks now that Gravity's entry serves every
//! write: ExecConstraints (NOT NULL and CHECK, with C's error text and Failing-row detail), ExecComputeStoredGenerated
//! and the virtual-generated NOT NULL checks, the view WITH CHECK OPTION violation, the generated-column extra updated
//! columns, and the FDW-modify routine registry. Their callers are COPY FROM, the logical replication apply worker,
//! EXPLAIN (the FDW-modify explain hook), postgres_fdw and Gravity's write hosts. The ModifyTable row node itself
//! (ExecInitModifyTable / ExecModifyTable / ExecEndModifyTable with its INSERT / UPDATE / DELETE / MERGE / ON CONFLICT
//! arms, RETURNING, row triggers and EvalPlanQual) was deleted with the row executor (rowexec deletion E4).
#![allow(non_snake_case)]

use datum::Datum;
use execexpr::{EvalSlots, ExprState};
use executils::{EStateData, ExecSlotId};
use mcx::PgBox;
use types_core::Oid;
use types_error::{PgError, PgResult, ERRCODE_CHECK_VIOLATION, ERRCODE_NOT_NULL_VIOLATION};
use types_nodes::plannodes::ModifyTable;
use types_rel::Relation;
use types_slot::SlotData;

// The FDW-modify half of C's FdwRoutine (fdwapi.h), installed per FdwKind by
// the provider's init_seams (nodeforeignscan's FdwExecRoutine pattern). Not
// installed = the provider has no ExecForeign{Insert,Update,Delete}
// (CheckValidResultRel's 0A000 arm). Batch insert / direct modify unmodeled.
pub struct FdwModifyRoutine {
    /// BeginForeignModify: decode fdwPrivLists[list_index], open the remote
    /// connection, return ri_FdwState (None under EXPLAIN_ONLY).
    pub begin: for<'mcx> fn(
        &'mcx ModifyTable<'mcx>,
        u32,   // rti
        usize, // list_index into fdwPrivLists
        &mut EStateData<'mcx>,
        i32, // eflags
    ) -> PgResult<Option<Box<dyn core::any::Any>>>,
    /// ExecForeignInsert/Update/Delete: true = a remote row was affected
    /// ("do nothing" = false). `slot` is the new-tuple slot (INSERT/UPDATE)
    /// or the RETURNING slot (DELETE); RETURNING data is stored into it.
    pub exec_insert: for<'mcx> fn(
        &mut dyn core::any::Any,
        &mut EStateData<'mcx>,
        ExecSlotId, // slot
        ExecSlotId, // plan slot
    ) -> PgResult<bool>,
    pub exec_update: for<'mcx> fn(
        &mut dyn core::any::Any,
        &mut EStateData<'mcx>,
        ExecSlotId,
        ExecSlotId,
    ) -> PgResult<bool>,
    pub exec_delete: for<'mcx> fn(
        &mut dyn core::any::Any,
        &mut EStateData<'mcx>,
        ExecSlotId,
        ExecSlotId,
    ) -> PgResult<bool>,
    /// EndForeignModify: deallocate the prepared statement, release the
    /// connection.
    pub end: fn(Box<dyn core::any::Any>) -> PgResult<()>,
    /// ExplainForeignModify; None = nothing extra to print (C's NULL slot).
    pub explain: Option<FdwModifyExplain>,
    /// Flush provider-buffered batch inserts (C ExecPendingInserts' per-rel
    /// half); None = provider never buffers.
    pub flush: Option<for<'mcx> fn(&mut dyn core::any::Any, &mut EStateData<'mcx>) -> PgResult<()>>,
    /// GetForeignModifyBatchSize over ri_FdwState; None = 1.
    pub batch_size: Option<fn(&dyn core::any::Any) -> i32>,
    /// Make `exec_insert` send each row at once: C's CIM_SINGLE COPY calls
    /// ExecForeignInsert and never ExecForeignBatchInsert. None = the
    /// provider never buffers.
    pub disable_batching: Option<fn(&mut dyn core::any::Any)>,
    /// BeginForeignInsert: a routed foreign partition's insert state; None =
    /// the provider routes with no ri_FdwState (C's NULL callback).
    pub begin_insert: Option<FdwBeginInsert>,
}

/// BeginForeignInsert's arguments: the plan, the routed foreign leaf, the RT
/// index whose entry (copied, relid swapped) describes the leaf and carries
/// the perminfo, the RT index the leaf's RETURNING/WCO Vars use, those lists
/// translated to leaf attnos, and whether the leaf is also an UPDATE subplan
/// result relation (postgresBeginForeignInsert's aux_fmstate guard).
pub type FdwBeginInsert = for<'mcx> fn(
    &'mcx ModifyTable<'mcx>,
    &mut EStateData<'mcx>,
    &Relation<'mcx>,
    u32,
    u32,
    &types_nodes::list::NodeList<'mcx>,
    &types_nodes::list::NodeList<'mcx>,
    bool,
) -> PgResult<Option<Box<dyn core::any::Any>>>;

/// ExplainForeignModify surface: (fdwPrivLists[j], relid, has_wco, flags,
/// emit). Plan-level — callable from EXPLAIN without executor state.
pub type FdwModifyExplain = for<'mcx> fn(
    &types_nodes::NodeList<'mcx>,
    Oid,
    bool,
    types_nodes::FdwExplainFlags,
    &mut dyn FnMut(&str, types_nodes::FdwExplainProp<'_>) -> PgResult<()>,
) -> PgResult<()>;

pub fn fdw_modify_explain(kind: types_nodes::FdwKind) -> Option<FdwModifyExplain> {
    fdw_modify_routine(kind).and_then(|r| r.explain)
}

static FDW_MODIFY_ROUTINES: [core::sync::atomic::AtomicPtr<FdwModifyRoutine>;
    types_nodes::NUM_FDW_KINDS] = [const { core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()) };
    types_nodes::NUM_FDW_KINDS];

pub fn install_fdw_modify_routine(kind: types_nodes::FdwKind, routine: &'static FdwModifyRoutine) {
    FDW_MODIFY_ROUTINES[kind.index()].store(
        routine as *const FdwModifyRoutine as *mut FdwModifyRoutine,
        core::sync::atomic::Ordering::Release,
    );
}

pub fn fdw_modify_routine(kind: types_nodes::FdwKind) -> Option<&'static FdwModifyRoutine> {
    let p = FDW_MODIFY_ROUTINES[kind.index()].load(core::sync::atomic::Ordering::Acquire);
    // SAFETY: install stores a &'static FdwModifyRoutine; never unset.
    unsafe { p.as_ref() }
}

// CheckValidResultRel's foreign-table arm (execMain.c:1123): the FDW must
// supply the operation's ExecForeign* callback and IsForeignRelUpdatable
// must allow it. COPY FROM into a foreign table checks CMD_INSERT here
// (copyfrom.c:921).
pub fn check_valid_foreign_result_rel<'mcx>(
    mcx: mcx::Mcx<'mcx>,
    rel: &Relation<'mcx>,
    operation: types_nodes::nodes_enums::CmdType,
    kind: types_nodes::FdwKind,
) -> PgResult<()> {
    use types_nodes::nodes_enums::CmdType;
    // MERGE on a foreign target is rejected in parse analysis / createplan,
    // so the per-operation arms are exhaustive here (C's default: elog).
    let (verb, not_allowed) = match operation {
        CmdType::CMD_INSERT => ("insert into", "does not allow inserts"),
        CmdType::CMD_UPDATE => ("update", "does not allow updates"),
        CmdType::CMD_DELETE => ("delete from", "does not allow deletes"),
        _ => panic!("CheckValidResultRel (execMain.c): {operation:?} on a foreign table"),
    };
    if fdw_modify_routine(kind).is_none() {
        return Err(Box::new(
            PgError::error(format!(
                "cannot {verb} foreign table \"{}\"",
                String::from_utf8_lossy(rel.rd_rel.relname.name_str())
            ))
            .with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED),
        ));
    }
    if let Some(mask) =
        foreigncmds_seams::fdw_is_foreign_rel_updatable::call(mcx, kind, rel.rd_id)?
    {
        if mask & (1 << operation as i32) == 0 {
            return Err(Box::new(
                PgError::error(format!(
                    "foreign table \"{}\" {not_allowed}",
                    String::from_utf8_lossy(rel.rd_rel.relname.name_str())
                ))
                .with_sqlstate(types_error::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE),
            ));
        }
    }
    Ok(())
}

pub struct GeneratedExpr<'mcx> {
    attnum: usize,
    state: PgBox<'mcx, ExprState<'mcx>>,
}

pub struct VirtualNnExpr<'mcx> {
    attnum: usize,
    state: PgBox<'mcx, ExprState<'mcx>>,
}

pub struct CheckExpr<'mcx> {
    name: mcx::PgString<'mcx>,
    state: Option<PgBox<'mcx, ExprState<'mcx>>>,
}

// (The lane-admission shape probe `mt_lane_shape_refusal` — sole caller
// lanev2/dml.rs — was DELETED at p72 D-2 with the DML hosting island.)

/// ExecGetExtraUpdatedCols' leg of ExecGetAllUpdatedCols (execUtils.c) —
/// ExecInitStoredGenerated(CMD_UPDATE)'s ri_extraUpdatedCols
/// (execExpr.c): every generated column whose expression depends on an
/// updated column joins `cols` (encoded attno - FirstLowInvalidHeapAttributeNumber);
/// with a BEFORE ROW UPDATE trigger (which may change any column) every
/// generated column does. Shared with the logical apply worker's
/// ExecSimpleRelationUpdate (execReplication.c), whose updatedCols are the
/// remote-changed columns (worker.c:2606-2626).
pub fn add_generated_extra_updated_cols<'mcx>(
    mcx: mcx::Mcx<'mcx>,
    rel: &Relation<'mcx>,
    trig_update_before_row: bool,
    cols: &mut types_nodes::Bitmapset<'mcx>,
) -> PgResult<()> {
    const FLIHAN: i32 = types_tuple::htup::FirstLowInvalidHeapAttributeNumber;
    let has_generated = rel
        .rd_att
        .constr
        .as_deref()
        .is_some_and(|c| c.has_generated_stored || c.has_generated_virtual);
    if !has_generated {
        return Ok(());
    }
    let skip_by_deps = !trig_update_before_row;
    let constr = rel.rd_att.constr.as_deref().expect("checked above");
    for i in 0..rel.rd_att.natts as usize {
        if rel.rd_att.attr(i).attgenerated == 0 {
            continue;
        }
        if skip_by_deps {
            let adbin = constr
                .defval
                .iter()
                .find(|d| d.adnum == (i + 1) as i16)
                .and_then(|d| d.adbin.as_ref())
                .unwrap_or_else(|| {
                    panic!(
                        "no generation expression found for column number {} of table \"{}\"",
                        i + 1,
                        String::from_utf8_lossy(rel.rd_rel.relname.name_str())
                    )
                });
            let expr = readfuncs::stringToNode(mcx, adbin.as_str())?;
            let mut attrs_used = types_nodes::Bitmapset::empty();
            vars::var::pull_varattnos(mcx, expr, 1, &mut attrs_used)?;
            if !cols.overlap(&attrs_used) {
                continue;
            }
        }
        cols.add_member(mcx, (i + 1) as i32 - FLIHAN)?;
    }
    Ok(())
}



// ExecComputeStoredGenerated + ExecInitGenerated (nodeModifyTable.c). The
// slot must be virtual: retained by-ref values point at subplan/projection
// memory that survives the clear+restore (C datumCopies instead).
pub fn exec_compute_stored_generated<'mcx>(
    mcx: mcx::Mcx<'mcx>,
    generated_exprs: &mut Option<mcx::PgVec<'mcx, GeneratedExpr<'mcx>>>,
    updated_cols: Option<&types_nodes::Bitmapset<'mcx>>,
    rel: &Relation<'mcx>,
    slot: &mut SlotData<'mcx>,
) -> PgResult<()> {
    exec_compute_stored_generated_impl::<false>(mcx, mcx, generated_exprs, updated_cols, rel, slot)
}

pub fn exec_compute_stored_generated_in_row<'mcx>(
    mcx: mcx::Mcx<'mcx>,
    row_mcx: mcx::Mcx<'_>,
    generated_exprs: &mut Option<mcx::PgVec<'mcx, GeneratedExpr<'mcx>>>,
    rel: &Relation<'mcx>,
    slot: &mut SlotData<'mcx>,
) -> PgResult<()> {
    exec_compute_stored_generated_impl::<true>(mcx, row_mcx, generated_exprs, None, rel, slot)
}

// Executor form (ExecComputeStoredGenerated): the expressions and the
// by-ref copies of every column evaluate in the per-tuple context
// (GetPerTupleMemoryContext, reset per row), the slot is then materialized
// into `mcx` — nothing per row is left in es_query_cxt.
pub fn exec_compute_stored_generated_in<'mcx>(
    mcx: mcx::Mcx<'mcx>,
    row_mcx: mcx::Mcx<'_>,
    generated_exprs: &mut Option<mcx::PgVec<'mcx, GeneratedExpr<'mcx>>>,
    updated_cols: Option<&types_nodes::Bitmapset<'mcx>>,
    rel: &Relation<'mcx>,
    slot: &mut SlotData<'mcx>,
) -> PgResult<()> {
    exec_compute_stored_generated_impl::<true>(mcx, row_mcx, generated_exprs, updated_cols, rel, slot)
}

// `updated_cols` is ExecInitGenerated's UPDATE filter (nodeModifyTable.c:496):
// Some(ExecGetUpdatedCols) skips generated columns depending on no target
// column; None compiles every stored generated column.
fn exec_compute_stored_generated_impl<'mcx, const ROW: bool>(
    mcx: mcx::Mcx<'mcx>,
    row_mcx: mcx::Mcx<'_>,
    generated_exprs: &mut Option<mcx::PgVec<'mcx, GeneratedExpr<'mcx>>>,
    updated_cols: Option<&types_nodes::Bitmapset<'mcx>>,
    rel: &Relation<'mcx>,
    slot: &mut SlotData<'mcx>,
) -> PgResult<()> {
    let constr = rel.rd_att.constr.as_deref().expect("caller checked");
    if generated_exprs.is_none() {
        let mut compiled: mcx::PgVec<'mcx, GeneratedExpr<'mcx>> = mcx::PgVec::new_in(mcx);
        for i in 0..rel.rd_att.natts as usize {
            // nodeModifyTable.c:496: only STORED generated columns are
            // prepared (and computed); a VIRTUAL column's expression never
            // runs here and its slot value stays NULL.
            if rel.rd_att.attr(i).attgenerated != STORED_GEN {
                continue;
            }
            let adbin = constr
                .defval
                .iter()
                .find(|d| d.adnum == (i + 1) as i16)
                .and_then(|d| d.adbin.as_ref())
                .unwrap_or_else(|| {
                    panic!(
                        "no generation expression found for column number {} of table \"{}\"",
                        i + 1,
                        String::from_utf8_lossy(rel.rd_rel.relname.name_str())
                    )
                });
            // cookDefault coerced the stored tree to the column type, so
            // build_column_default's re-coercion is a no-op; skipped.
            let node = readfuncs::stringToNode(mcx, adbin.as_str())?;
            if let Some(cols) = updated_cols {
                let mut attrs_used = types_nodes::Bitmapset::empty();
                vars::var::pull_varattnos(mcx, node, 1, &mut attrs_used)?;
                if !cols.overlap(&attrs_used) {
                    continue;
                }
            }
            let node = clauses::eval_const_expressions(mcx, node)?;
            nodes_core::fix_opfuncids(node)?;
            let mut state = execexpr::exec_init_expr(mcx, Some(node), execexpr::ParamBind::NONE)?
                .expect("generation expr");
            state.arm_result_mcx(mcx);
            compiled.push(GeneratedExpr { attnum: i, state });
        }
        *generated_exprs = Some(compiled);
    }

    exectuples::slot_getallattrs(slot);
    let exprs = generated_exprs.as_mut().expect("just built");
    let mut results: mcx::PgVec<'_, (usize, Datum, bool)> = mcx::PgVec::new_in(row_mcx);
    results
        .try_reserve_exact(exprs.len())
        .map_err(|_| Box::new(mcx.oom(exprs.len() * 24)))?;
    for ge in exprs.iter_mut() {
        let mut slots = EvalSlots {
            scan: Some(slot),
            inner: None,
            outer: None,
        };
        if ROW {
            // SAFETY: row results are owned by the slot before return; caches retain mcx.
            unsafe { ge.state.arm_result_mcx_raw(row_mcx) };
        }
        let result = execexpr::exec_eval_expr(&mut ge.state, &mut slots);
        if ROW {
            ge.state.arm_result_mcx(mcx);
        }
        let r = result?;
        results.push((ge.attnum, r.value, r.isnull));
    }
    let natts = rel.rd_att.natts as usize;
    let mut values: mcx::PgVec<'_, Datum> = mcx::vec_with_capacity_in(row_mcx, natts)?;
    let mut nulls: mcx::PgVec<'_, bool> = mcx::vec_with_capacity_in(row_mcx, natts)?;
    {
        let base = slot.base_mut();
        values.extend(base.tts_values.iter().copied());
        nulls.extend(base.tts_isnull.iter().copied());
    }
    for &(attnum, value, isnull) in results.iter() {
        values[attnum] = value;
        nulls[attnum] = isnull;
    }
    for i in 0..natts {
        let att = rel.rd_att.attr(i);
        if !nulls[i] && !att.attbyval {
            values[i] = copy_by_ref_datum(row_mcx, values[i], att.attlen)?;
        }
    }
    exectuples::exec_clear_tuple(slot, mcx);
    let base = slot.base_mut();
    for i in 0..natts {
        base.tts_values[i] = values[i];
        base.tts_isnull[i] = nulls[i];
    }
    exectuples::exec_store_virtual_tuple(slot);
    if ROW {
        if let Err(error) = exectuples::exec_materialize_slot(slot, mcx) {
            exectuples::exec_clear_tuple(slot, mcx);
            let base = slot.base_mut();
            base.tts_values.fill(Datum::null());
            base.tts_isnull.fill(true);
            return Err(error);
        }
    }
    Ok(())
}

fn copy_by_ref_datum<'mcx>(mcx: mcx::Mcx<'mcx>, d: Datum, attlen: i16) -> PgResult<Datum> {
    let p = d.as_usize() as *const u8;
    let size = match attlen {
        // SAFETY: non-null by-ref datum points at a live varlena image.
        -1 => unsafe { types_tuple::varatt::varsize_any(p) },
        // SAFETY: cstring datum is NUL-terminated.
        -2 => unsafe {
            let mut n = 0usize;
            while *p.add(n) != 0 {
                n += 1;
            }
            n + 1
        },
        l => l as usize,
    };
    let mut buf: mcx::PgVec<'mcx, u8> = mcx::vec_with_capacity_in(mcx, size)?;
    // SAFETY: size bytes are readable per the attlen contract above.
    unsafe {
        core::ptr::copy_nonoverlapping(p, buf.as_mut_ptr(), size);
        buf.set_len(size);
    }
    Ok(Datum::from_usize(buf.leak().as_ptr() as usize))
}

#[track_caller]
#[cold]
#[inline(never)]
pub fn view_wco_violation<'mcx>(
    mcx: mcx::Mcx<'mcx>,
    relname: &str,
    rel: &Relation<'mcx>,
    slot: &mut SlotData<'mcx>,
    root_rel: Option<&Relation<'mcx>>,
    modified_cols: Option<&types_nodes::Bitmapset<'mcx>>,
) -> Box<PgError> {
    let mut e = PgError::error(format!(
        "new row violates check option for view \"{relname}\""
    ))
    .with_sqlstate(types_error::ERRCODE_WITH_CHECK_OPTION_VIOLATION);
    if let Ok(Some(desc)) = root_slot_value_description(mcx, rel, slot, root_rel, modified_cols) {
        e = e.with_detail(format!("Failing row contains {desc}."));
    }
    Box::new(e)
}

// ExecConstraints (execMain.c): NOT NULL + CHECK arms live.
// GetPerTupleMemoryContext(estate): the row-scoped context mt_row_prologue
// resets before the next child row.
fn per_tuple_mcx<'mcx>(estate: &mut EStateData<'mcx>) -> mcx::Mcx<'mcx> {
    let per_tuple: core::ptr::NonNull<mcx::MemoryContext> =
        core::ptr::NonNull::from(estate.get_per_tuple_memory().context());
    // SAFETY: the per-tuple ExprContext lives in the estate for the whole query.
    unsafe { per_tuple.as_ref() }.mcx()
}

pub fn exec_constraints<'mcx>(
    mcx: mcx::Mcx<'mcx>,
    check_exprs: &mut Option<mcx::PgVec<'mcx, CheckExpr<'mcx>>>,
    virtual_nn_exprs: &mut Option<mcx::PgVec<'mcx, VirtualNnExpr<'mcx>>>,
    rel: &Relation<'mcx>,
    slot: &mut SlotData<'mcx>,
    root_rel: Option<&Relation<'mcx>>,
    modified_cols: Option<&types_nodes::Bitmapset<'mcx>>,
) -> PgResult<()> {
    exec_constraints_impl::<false>(
        mcx,
        mcx,
        check_exprs,
        virtual_nn_exprs,
        rel,
        slot,
        root_rel,
        modified_cols,
    )
}

pub fn exec_constraints_in_row<'mcx>(
    mcx: mcx::Mcx<'mcx>,
    row_mcx: mcx::Mcx<'_>,
    check_exprs: &mut Option<mcx::PgVec<'mcx, CheckExpr<'mcx>>>,
    virtual_nn_exprs: &mut Option<mcx::PgVec<'mcx, VirtualNnExpr<'mcx>>>,
    rel: &Relation<'mcx>,
    slot: &mut SlotData<'mcx>,
    root_rel: Option<&Relation<'mcx>>,
    modified_cols: Option<&types_nodes::Bitmapset<'mcx>>,
) -> PgResult<()> {
    exec_constraints_impl::<true>(
        mcx,
        row_mcx,
        check_exprs,
        virtual_nn_exprs,
        rel,
        slot,
        root_rel,
        modified_cols,
    )
}

fn exec_constraints_impl<'mcx, const ROW: bool>(
    mcx: mcx::Mcx<'mcx>,
    row_mcx: mcx::Mcx<'_>,
    check_exprs: &mut Option<mcx::PgVec<'mcx, CheckExpr<'mcx>>>,
    virtual_nn_exprs: &mut Option<mcx::PgVec<'mcx, VirtualNnExpr<'mcx>>>,
    rel: &Relation<'mcx>,
    slot: &mut SlotData<'mcx>,
    root_rel: Option<&Relation<'mcx>>,
    modified_cols: Option<&types_nodes::Bitmapset<'mcx>>,
) -> PgResult<()> {
    if let Some(constr) = rel.rd_att.constr.as_deref() {
        if constr.has_not_null {
            exec_not_null_constraints(mcx, rel, slot, root_rel, modified_cols)?;
            if constr.has_generated_virtual {
                if let Some(i) = exec_rel_gen_virtual_notnull(mcx, row_mcx, virtual_nn_exprs, rel, slot)? {
                    return Err(not_null_violation(
                        mcx,
                        rel,
                        slot,
                        i,
                        root_rel,
                        modified_cols,
                    ));
                }
            }
        }
        // execMain.c:2030: gate on pg_class.relchecks, not on the rows the
        // relcache found (ExecRelCheck reports the difference).
        if constr.relchecks > 0 {
            if let Some(failed) = exec_rel_check::<ROW>(mcx, row_mcx, check_exprs, rel, slot)? {
                return Err(check_violation(
                    mcx,
                    rel,
                    slot,
                    failed,
                    root_rel,
                    modified_cols,
                ));
            }
        }
    }
    Ok(())
}

// ExecRelCheck (execMain.c): compile once into check_exprs, evaluate with the
// slot as the scan tuple; ExecCheck semantics (NULL result passes). Returns
// the failing constraint's index.
fn exec_rel_check<'mcx, const ROW: bool>(
    mcx: mcx::Mcx<'mcx>,
    row_mcx: mcx::Mcx<'_>,
    check_exprs: &mut Option<mcx::PgVec<'mcx, CheckExpr<'mcx>>>,
    rel: &Relation<'mcx>,
    slot: &mut SlotData<'mcx>,
) -> PgResult<Option<usize>> {
    let constr = rel.rd_att.constr.as_deref().expect("caller checked");
    // execMain.c:1796-1798: CheckNNConstraintFetch let a short pg_constraint
    // scan pass with a WARNING; enforcing an incomplete CHECK set is an ERROR.
    let ncheck = constr.check.len() as i32;
    if ncheck != constr.relchecks as i32 {
        return Err(Box::new(PgError::error(format!(
            "{} pg_constraint record(s) missing for relation \"{}\"",
            constr.relchecks as i32 - ncheck,
            String::from_utf8_lossy(rel.rd_rel.relname.name_str()),
        ))));
    }
    if check_exprs.is_none() {
        let mut compiled: mcx::PgVec<'mcx, CheckExpr<'mcx>> = mcx::PgVec::new_in(mcx);
        compiled
            .try_reserve_exact(constr.check.len())
            .map_err(|_| {
                Box::new(mcx.oom(constr.check.len() * core::mem::size_of::<CheckExpr<'_>>()))
            })?;
        for c in constr.check.iter() {
            let name = c.ccname.as_ref().expect("ccname").clone_in(mcx)?;
            if !c.ccenforced {
                compiled.push(CheckExpr { name, state: None });
                continue;
            }
            let ccbin = c.ccbin.as_ref().expect("ccbin");
            let mut node = readfuncs::stringToNode(mcx, ccbin.as_str())?;
            if constr.has_generated_virtual {
                // execMain.c:1818 expand_generated_columns_in_expr.
                node = expand_generated_columns_in_expr(mcx, node, rel, 1)?.unwrap_or(node);
            }
            // C ExecPrepareExpr: expression_planner before init — the stored
            // ccbin is parse output (CollateExpr survives until const-fold).
            node = clauses::eval_const_expressions(mcx, node)?;
            nodes_core::fix_opfuncids(node)?;
            let mut state = execexpr::exec_init_expr(mcx, Some(node), execexpr::ParamBind::NONE)?
                .expect("check constraint expr");
            // Whole-row/composite steps return by-ref datums (C evaluates in
            // the per-tuple context).
            state.arm_result_mcx(mcx);
            compiled.push(CheckExpr {
                name,
                state: Some(state),
            });
        }
        *check_exprs = Some(compiled);
    }
    for (i, ce) in check_exprs
        .as_mut()
        .expect("just built")
        .iter_mut()
        .enumerate()
    {
        let Some(state) = ce.state.as_deref_mut() else {
            continue;
        };
        let mut slots = EvalSlots {
            scan: Some(slot),
            inner: None,
            outer: None,
        };
        if ROW {
            // SAFETY: only the CHECK Boolean escapes; persistent frames are restored below.
            unsafe { state.arm_result_mcx_raw(row_mcx) };
        }
        let result = execexpr::exec_eval_expr(state, &mut slots);
        if ROW {
            state.arm_result_mcx(mcx);
        }
        let r = result?;
        if !r.isnull && !r.value.as_bool() {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

// ExecConstraints (execMain.c), NOT NULL arm (ReportNotNullViolationError).
fn exec_not_null_constraints<'mcx>(
    mcx: mcx::Mcx<'mcx>,
    rel: &Relation<'mcx>,
    slot: &mut SlotData<'mcx>,
    root_rel: Option<&Relation<'mcx>>,
    modified_cols: Option<&types_nodes::Bitmapset<'mcx>>,
) -> PgResult<()> {
    for i in 0..rel.rd_att.natts as usize {
        let att = rel.rd_att.attr(i);
        if att.attgenerated == VIRTUAL_GEN {
            continue;
        }
        if att.attnotnull && exectuples::slot_attisnull(slot, i as i32 + 1) {
            return Err(not_null_violation(
                mcx,
                rel,
                slot,
                i,
                root_rel,
                modified_cols,
            ));
        }
    }
    Ok(())
}

const VIRTUAL_GEN: i8 = types_core::catalog::ATTRIBUTE_GENERATED_VIRTUAL as i8;
const STORED_GEN: i8 = types_core::catalog::ATTRIBUTE_GENERATED_STORED as i8;

// build_generation_expression (rewriteHandler.c:4520), adbin-direct copy: the
// rewrite_handler home is unreachable (planner -> execmain -> this crate
// cycle) and cookDefault stored a coerced tree, so re-coercion is a no-op.
fn build_generation_expression<'mcx>(
    mcx: mcx::Mcx<'mcx>,
    rel: &Relation<'mcx>,
    attrno: usize,
) -> PgResult<types_nodes::Node<'mcx>> {
    let att = rel.rd_att.attr(attrno - 1);
    let constr = rel.rd_att.constr.as_deref().expect("caller checked");
    let adbin = constr
        .defval
        .iter()
        .find(|d| d.adnum == attrno as i16)
        .and_then(|d| d.adbin.as_ref())
        .unwrap_or_else(|| {
            panic!(
                "no generation expression found for column number {} of table \"{}\"",
                attrno,
                String::from_utf8_lossy(rel.rd_rel.relname.name_str())
            )
        });
    let expr = readfuncs::stringToNode(mcx, adbin.as_str())?;
    if att.attcollation != 0 && att.attcollation != nodes_core::node_funcs::expr_collation(expr) {
        return types_nodes::Node::mk(
            mcx,
            types_nodes::primnodes::CollateExpr {
                arg: expr,
                collOid: att.attcollation,
                location: -1,
            },
        );
    }
    Ok(expr)
}

// expand_generated_columns_in_expr (rewriteHandler.c:4493): Vars naming a
// virtual generated column of rel at varno become the generation expression.
fn expand_generated_columns_in_expr<'mcx>(
    mcx: mcx::Mcx<'mcx>,
    node: types_nodes::Node<'mcx>,
    rel: &Relation<'mcx>,
    varno: i32,
) -> PgResult<Option<types_nodes::Node<'mcx>>> {
    if let Some(v) = node.as_var() {
        if v.varlevelsup != 0 || v.varno != varno {
            return Ok(None);
        }
        if v.varattno == 0 {
            // ReplaceVarsFromTargetList whole-row arm (rewriteManip.c:1801):
            // a named-rowtype whole-row Var becomes a RowExpr over per-field
            // Vars (dropped columns as NULL int4 consts, expandRTE shape),
            // each field then replaced so virtual columns expand.
            let mut args = types_nodes::list::NodeList::nil();
            for i in 0..rel.rd_att.natts as usize {
                let att = rel.rd_att.attr(i);
                let field = if att.attisdropped {
                    types_nodes::Node::mk_const(
                        mcx,
                        types_core::catalog::INT4OID,
                        -1,
                        0,
                        4,
                        datum::Datum::null(),
                        true,
                        true,
                    )?
                } else if att.attgenerated == VIRTUAL_GEN {
                    debug_assert!(varno == 1, "generation expression Vars are varno 1");
                    build_generation_expression(mcx, rel, i + 1)?
                } else {
                    types_nodes::Node::mk_var(
                        mcx,
                        varno,
                        (i + 1) as i16,
                        att.atttypid,
                        att.atttypmod,
                        att.attcollation,
                        0,
                    )?
                };
                args.lappend(mcx, field)?;
            }
            return Ok(Some(types_nodes::Node::mk(
                mcx,
                types_nodes::RowExpr {
                    args,
                    row_typeid: v.vartype,
                    row_format: types_nodes::CoercionForm::COERCE_IMPLICIT_CAST,
                    colnames: types_nodes::list::NodeList::nil(),
                    location: v.location,
                },
            )?));
        }
        if v.varattno < 0 || rel.rd_att.attr(v.varattno as usize - 1).attgenerated != VIRTUAL_GEN {
            return Ok(None);
        }
        let e = build_generation_expression(mcx, rel, v.varattno as usize)?;
        debug_assert!(varno == 1, "generation expression Vars are varno 1");
        return Ok(Some(e));
    }
    clauses::walker::expression_tree_mutator(mcx, node, &mut |n| {
        expand_generated_columns_in_expr(mcx, n, rel, varno)
    })
}

// ExecRelGenVirtualNotNull (execMain.c:2098): NullTest(IS NOT NULL) over the
// generation expression per virtual not-null column; compiled once.
pub fn exec_rel_gen_virtual_notnull<'mcx>(
    mcx: mcx::Mcx<'mcx>,
    row_mcx: mcx::Mcx<'_>,
    virtual_nn_exprs: &mut Option<mcx::PgVec<'mcx, VirtualNnExpr<'mcx>>>,
    rel: &Relation<'mcx>,
    slot: &mut SlotData<'mcx>,
) -> PgResult<Option<usize>> {
    if virtual_nn_exprs.is_none() {
        let mut compiled: mcx::PgVec<'mcx, VirtualNnExpr<'mcx>> = mcx::PgVec::new_in(mcx);
        for i in 0..rel.rd_att.natts as usize {
            let att = rel.rd_att.attr(i);
            if !(att.attnotnull && att.attgenerated == VIRTUAL_GEN) {
                continue;
            }
            let arg = build_generation_expression(mcx, rel, i + 1)?;
            let nulltest = types_nodes::Node::mk(
                mcx,
                types_nodes::primnodes::NullTest {
                    arg: Some(arg),
                    nulltesttype: types_nodes::primnodes::NullTestType::IS_NOT_NULL,
                    argisrow: false,
                    location: -1,
                },
            )?;
            let nulltest = clauses::eval_const_expressions(mcx, nulltest)?;
            nodes_core::fix_opfuncids(nulltest)?;
            let mut state =
                execexpr::exec_init_expr(mcx, Some(nulltest), execexpr::ParamBind::NONE)?
                    .expect("virtual not-null expr");
            state.arm_result_mcx(mcx);
            compiled.push(VirtualNnExpr { attnum: i, state });
        }
        *virtual_nn_exprs = Some(compiled);
    }
    exectuples::slot_getallattrs(slot);
    for e in virtual_nn_exprs.as_mut().expect("just built").iter_mut() {
        let mut slots = EvalSlots {
            scan: Some(slot),
            inner: None,
            outer: None,
        };
        // SAFETY: only the NullTest Boolean escapes; the generated value
        // dies with the row (execMain.c:2139 evaluates in the per-tuple
        // econtext); the persistent frame is restored below.
        unsafe { e.state.arm_result_mcx_raw(row_mcx) };
        let result = execexpr::exec_eval_expr(&mut e.state, &mut slots);
        e.state.arm_result_mcx(mcx);
        let r = result?;
        if !r.isnull && !r.value.as_bool() {
            return Ok(Some(e.attnum));
        }
    }
    Ok(None)
}

#[cold]
#[inline(never)]
fn schema_name_of(mcx: mcx::Mcx<'_>, rel: &Relation<'_>) -> String {
    lsyscache::misc::get_namespace_name(mcx, rel.rd_rel.relnamespace)
        .ok()
        .flatten()
        .map(|s| s.as_str().to_owned())
        .unwrap_or_default()
}

#[track_caller]
#[cold]
#[inline(never)]
fn not_null_violation<'mcx>(
    mcx: mcx::Mcx<'mcx>,
    rel: &Relation<'mcx>,
    slot: &mut SlotData<'mcx>,
    attidx: usize,
    root_rel: Option<&Relation<'mcx>>,
    modified_cols: Option<&types_nodes::Bitmapset<'mcx>>,
) -> Box<PgError> {
    let att = rel.rd_att.attr(attidx);
    let col = String::from_utf8_lossy(att.attname.name_str()).into_owned();
    let table = String::from_utf8_lossy(rel.rd_rel.relname.name_str()).into_owned();
    let mut e = PgError::error(format!(
        "null value in column \"{col}\" of relation \"{table}\" violates \
         not-null constraint"
    ))
    .with_sqlstate(ERRCODE_NOT_NULL_VIOLATION)
    // C execMain.c:2219 (18.6): the ereport lives in ReportNotNullViolationError (wire F/L/R)
    .with_location("execMain.c", 2219, "ReportNotNullViolationError")
    .with_schema_name(schema_name_of(mcx, rel))
    .with_table_name(table);
    if let Ok(Some(desc)) = root_slot_value_description(mcx, rel, slot, root_rel, modified_cols) {
        e = e.with_detail(format!("Failing row contains {desc}."));
    }
    e.column_name = Some(col);
    Box::new(e)
}

// ExecConstraints' ri_RootResultRelInfo leg: a routed leaf's failing row is
// reported in the root's rowtype (execMain.c reverse attrmap); modified_cols
// is numbered in the description relation's (root's) attnos.
fn root_slot_value_description<'mcx>(
    mcx: mcx::Mcx<'mcx>,
    rel: &Relation<'mcx>,
    slot: &mut SlotData<'mcx>,
    root_rel: Option<&Relation<'mcx>>,
    modified_cols: Option<&types_nodes::Bitmapset<'mcx>>,
) -> PgResult<Option<String>> {
    match root_rel {
        Some(root) if root.rd_id != rel.rd_id => {
            let map = tupdesc::build_attrmap_by_name_if_req(mcx, &rel.rd_att, &root.rd_att, false)?;
            execpartition::slot_value_description(mcx, root, slot, modified_cols, map.as_deref())
        }
        _ => execpartition::slot_value_description(mcx, rel, slot, modified_cols, None),
    }
}

#[track_caller]
#[cold]
#[inline(never)]
fn check_violation<'mcx>(
    mcx: mcx::Mcx<'mcx>,
    rel: &Relation<'mcx>,
    slot: &mut SlotData<'mcx>,
    failed: usize,
    root_rel: Option<&Relation<'mcx>>,
    modified_cols: Option<&types_nodes::Bitmapset<'mcx>>,
) -> Box<PgError> {
    let constr = rel.rd_att.constr.as_deref().expect("has checks");
    let ccname = constr.check[failed]
        .ccname
        .as_ref()
        .map(|s| s.as_str().to_owned())
        .unwrap_or_default();
    let table = String::from_utf8_lossy(rel.rd_rel.relname.name_str()).into_owned();
    let mut e = PgError::error(format!(
        "new row for relation \"{table}\" violates check constraint \"{ccname}\""
    ))
    .with_sqlstate(ERRCODE_CHECK_VIOLATION)
    // C execMain.c: the check-constraint ereport lives in ExecConstraints
    .with_funcname("ExecConstraints")
    .with_schema_name(schema_name_of(mcx, rel))
    .with_table_name(table)
    .with_constraint_name(ccname);
    if let Ok(Some(desc)) = root_slot_value_description(mcx, rel, slot, root_rel, modified_cols) {
        e = e.with_detail(format!("Failing row contains {desc}."));
    }
    Box::new(e)
}

// CheckExpr / GeneratedExpr / VirtualNnExpr: their caller (COPY FROM, the logical apply worker, Gravity's write hosts)
// owns and releases each one's state; the bundle is forgettable once it has.
mcx::forget_safe_struct!(
    CheckExpr<'_> { name; state },
    GeneratedExpr<'_> { attnum; state },
    VirtualNnExpr<'_> { attnum; state },
);

// (The WAVE-9 WS-AG fusion-D1a append region — mt_rowchain_shape_mask +
// the five mt_ins_* chain seams, sole driver the stitched row chain in
// lanev2/dml.rs — was DELETED at p72 D-2 with the DML hosting island.)

#[cfg(test)]
mod copy_row_tests;

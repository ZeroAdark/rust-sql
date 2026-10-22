//! pglpoint S1: a plain point SELECT after planning. The planner's IndexScan over a unique btree of a heap table, equality
//! on every key column against a Const, a target list of the table's columns and constants: run in ExecutorStart /
//! ExecutorRun's order (the permission checks, the relations opened as ExecGetRangeTableRelation / ExecInitIndexScan open
//! them, the scan keys, the RowDescription, the probe, a DataRow per row, SELECT n) with no Gravity program. Anything
//! else is `Verdict::Not` / `Declined`: Gravity serves it unchanged. A plan S5 served from its memo runs over the pieces its
//! memo entry keeps (keep.rs `ProbeKept`: the parked scan, its keys and slot, the out columns).
use gravity_program::{Cmd, OutCol};
use mcx::{Mcx, PgVec};
use tcop_dest::DestReceiver;
use types_core::Oid;
use types_error::PgResult;
use types_nodes::list::NodeList;
use types_nodes::nodes_enums::CmdType;
use types_nodes::parsenodes::RTEKind;
use types_nodes::plannodes::{IndexScan, PlannedStmt};
use types_nodes::primnodes::TargetEntry;
use types_portal::params::ParamExternData;
use types_portal::QueryCompletion;
use types_rel::{NoLock, Relation, LOCKMODE};
use types_slot::SlotData;

use super::keep::{self, Kept, KeptBox, Made, ProbeKept};
use super::out::{cstring, out_col};
use super::run::{exec_in, with_run};
use super::{probe, rel, skip};

/// What hook B makes of a planned statement.
pub(super) enum Verdict<'p> {
    /// Not an index scan statement: no word about it.
    Not,
    /// An index scan statement S1 does not take, and why.
    Declined(&'static str),
    Take(Shape<'p>),
}

/// The statement S1 runs: its scan, its table and the RTE's lock mode (ExecInitIndexScan's for the index).
pub(super) struct Shape<'p> {
    pub scan: &'p IndexScan<'p>,
    pub relid: Oid,
    pub lockmode: LOCKMODE,
}

/// The statement-level and plan-level test, pointer checks only (the relations' own tests are `relation_refusal`).
pub(super) fn shape<'p>(p: &PlannedStmt<'p>) -> Verdict<'p> {
    let Some(scan) = p.planTree.and_then(|n| n.as_index_scan()) else {
        return Verdict::Not;
    };
    let plain = p.commandType == CmdType::CMD_SELECT && p.utilityStmt.is_none() && p.canSetTag && !p.hasReturning && !p.hasModifyingCTE;
    let empty = p.rowMarks.is_nil() && p.subplans.is_nil() && p.paramExecTypes.is_nil() && p.resultRelations.is_nil() && !p.parallelModeNeeded;
    if !(plain && empty) {
        return Verdict::Declined("statement");
    }
    let Some(rte) = (p.rtable.len() == 1).then(|| p.rtable.nth(0).as_range_tbl_entry()).flatten() else {
        return Verdict::Declined("range table");
    };
    if rte.rtekind != RTEKind::RTE_RELATION || rte.inh || !rte.securityQuals.is_nil() || scan.scan.scanrelid != 1 {
        return Verdict::Declined("range table");
    }
    let plan = &scan.scan.plan;
    let bare = plan.lefttree.is_none() && plan.righttree.is_none() && plan.initPlan.is_nil() && plan.qual.is_nil() && scan.indexorderby.is_nil();
    if !bare || scan.indexqual.is_nil() {
        return Verdict::Declined("plan");
    }
    if plan.targetlist.is_nil() || !plan.targetlist.iter().all(|t| t.as_target_entry().is_some_and(entry)) {
        return Verdict::Declined("target list");
    }
    Verdict::Take(Shape { scan, relid: rte.relid, lockmode: rte.rellockmode })
}

/// A target entry S1 projects: a column of the scanned table (not a system column or the whole row) or a constant, with
/// a name the wire's out row holds and a type it has a form for (no cstring, as S0a and the lowering's `col_of`).
fn entry(te: &TargetEntry<'_>) -> bool {
    let named = !te.resjunk && te.resname.is_none_or(|n| n.len() < gravity_program::program::NAME_LEN) && !cstring(te);
    let column = te.expr.as_var().is_some_and(|v| v.varno == 1 && v.varattno > 0 && v.varlevelsup == 0);
    named && (column || te.expr.as_const().is_some())
}

/// The table's and the index's tests (`None`: S1 takes them): `rel::table_refusal`, the index the table's, and a point
/// probe's (`probe::index_refusal`, the keys' values from `params`, their properties into `ops`).
pub(super) fn relation_refusal(heap: &Relation<'_>, index: &Relation<'_>, s: &Shape<'_>, params: &[ParamExternData], ops: &mut probe::KeyOps) -> PgResult<Option<&'static str>> {
    if let Some(why) = rel::table_refusal(heap) {
        return Ok(Some(why));
    }
    if !rel::index_of(heap, index) {
        return Ok(Some("index of another table"));
    }
    probe::index_refusal(index, &s.scan.indexqual, params, ops)
}

/// Hook B for an index scan statement: `None` leaves it to Gravity, unchanged. `hit`: S5 served the plan from its memo
/// slot, which keeps the run's pieces (keep.rs `ProbeKept`).
pub(super) fn serve(pstmt: &PlannedStmt<'_>, dest: &mut DestReceiver<'_>, hit: Option<skip::Hit>) -> Option<PgResult<Option<QueryCompletion>>> {
    let s = match shape(pstmt) {
        Verdict::Not => return None,
        Verdict::Declined(why) => return declined(why),
        Verdict::Take(s) => s,
    };
    // `strict` raises only where hook A built a plan Gravity would then run (S0a); S1 serves the planner's own plan, so a
    // decline here is a word in the log, in every mode
    if let Some(why) = super::runtime_refusal(dest.mydest()).or_else(|| xact::IsolationIsSerializable().then_some("serializable")) {
        return declined(why);
    }
    // D4572: a slot whose pieces found no room runs as with pgrust.point_keep off (`skip::Taken::NoRoom`)
    let taken = hit.map(|h| (h, skip::take_kept(&h)));
    let r = with_run(|mcx, wire| match taken {
        Some((h, skip::Taken::Kept(kb))) if keep::probe_ids(&kb) == Some((s.relid, s.scan.indexid)) => run_kept(pstmt, &s, mcx, wire, h, kb, None).map(Ok),
        Some((h, skip::Taken::Kept(_) | skip::Taken::Make)) => make_and_run(pstmt, &s, mcx, wire, h),
        Some((_, skip::Taken::NoRoom)) | None => run_in(pstmt, &s, mcx, wire),
    });
    let r = match r {
        Ok(Err(why)) => return declined(why),
        Ok(Ok(qc)) => Ok(qc),
        Err(e) => Err(e),
    };
    if super::reporting() {
        super::report(&match &r {
            Ok(qc) => format!("served select rows={}", qc.nprocessed), // alloc: Cold
            Err(_) => "served select error".to_string(),                // alloc: Cold
        });
    }
    Some(r.map(Some))
}

#[cold]
fn declined(why: &str) -> Option<PgResult<Option<QueryCompletion>>> {
    if super::reporting() {
        super::report(&format!("declined select ({why})")); // alloc: Cold
    }
    None
}

/// ExecGetRangeTableRelation's open of the table (the parser's lock held) and ExecInitIndexScan's of the index
/// (`index_lockmode`), the table's and the index's tests (`Err` with the reason: nothing was sent, Gravity serves it), then
/// ExecutorStart's checks and the keys' EXECUTE gates (`probe::key_gates`), then ExecutorRun: the RowDescription, the probe,
/// one DataRow per row built in the run context (`send_row_in`), SELECT n.
fn run_in<'m>(pstmt: &PlannedStmt<'_>, s: &Shape<'_>, mcx: Mcx<'m>, wire: &mut gravity_wire::WireNode) -> PgResult<Result<QueryCompletion, &'static str>> {
    let heap = table::table_open(mcx, s.relid, NoLock)?;
    let index = indexam::index_open(mcx, s.scan.indexid, index_lockmode(s))?;
    let mut ops: probe::KeyOps = [(0, 0); types_core::fmgr::INDEX_MAX_KEYS as usize];
    let r = match relation_refusal(&heap, &index, s, &[], &mut ops) {
        Ok(Some(why)) => Ok(Err(why)),
        Ok(None) => exec_in(pstmt, mcx, |_| probe_rows(s, mcx, wire, &heap, &index, &ops)).map(|n| Ok(gravity_wire::completion(Cmd::Select, n))),
        Err(e) => Err(e),
    };
    let closed = indexam::index_close(index, NoLock).and_then(|()| table::table_close(heap, NoLock));
    let r = r?;
    closed?;
    Ok(r)
}

/// The pieces a memo slot keeps for its SELECT (`ProbeKept::make`: `run_in`'s opens and tests, what they read recorded
/// into the slot's reads), then the run over them.
fn make_and_run(pstmt: &PlannedStmt<'_>, s: &Shape<'_>, mcx: Mcx<'_>, wire: &mut gravity_wire::WireNode, hit: skip::Hit) -> PgResult<Result<QueryCompletion, &'static str>> {
    let (made, deps) = gravity_deps::recording(|| ProbeKept::make(s, skip::MEMO_OWNER));
    match made? {
        Err(why) => Ok(Err(why)),
        Ok(k) => run_kept(pstmt, s, mcx, wire, hit, keep::boxed(Made::Probe(k), skip::MEMO_OWNER)?, Some(deps)).map(Ok),
    }
}

/// A SELECT over the memo slot's kept pieces: ExecInitIndexScan's index lock where hook A's left none, then ExecutorStart's
/// checks and the snapshot around the run (`ProbeKept::run`); the pieces back into the slot when it returns, dropped when
/// it raises. The first run (`deps`: the making's reads) makes the scan, its reads recorded with the making's.
fn run_kept(pstmt: &PlannedStmt<'_>, s: &Shape<'_>, mcx: Mcx<'_>, wire: &mut gravity_wire::WireNode, hit: skip::Hit, mut kb: KeptBox, deps: Option<gravity_deps::Deps>) -> PgResult<QueryCompletion> {
    if super::reporting() {
        super::report(if deps.is_some() { "keep made" } else { "keep used" });
    }
    let mode = index_lockmode(s);
    if mode != NoLock {
        lmgr_seams::lock_relation_oid::call(s.scan.indexid, mode)?;
    }
    let mut run = || {
        kb.with_mut(|kin| -> PgResult<u64> {
            let Kept::Probe(k) = &mut kin.kept else {
                return Err(keep::internal("a point SELECT's memo entry keeps another statement's pieces"));
            };
            let k: &mut ProbeKept = k; // the pieces' fields borrowed apart, not through the box each time
            exec_in(pstmt, mcx, |_| k.run(s, mcx, wire))
        })
    };
    let (n, deps) = match deps {
        None => (run()?, None),
        Some(mut d) => {
            let (n, first) = gravity_deps::recording(&mut run);
            d.absorb(&first);
            (n?, Some(d))
        }
    };
    skip::put_kept(&hit, kb, deps.as_ref());
    Ok(gravity_wire::completion(Cmd::Select, n))
}

/// ExecInitIndexScan's index_open(indexid, rellockmode) takes the RTE's lock on the index; where this transaction already
/// holds it in that mode, C's LockAcquire is a local lock table hit (LOCKACQUIRE_ALREADY_CLEAR: the count raised, no
/// invalidation read) and the lock stays to the transaction's end either way. S1 asks the same table (LockHeldByMe, the
/// exact mode) and opens with NoLock on a hit, the lock call's walk saved; on a miss it takes the lock as C does.
/// Hook B's one caller is exec_simple_query with the plan pg_plan_queries made in that call (tcop simple_query.rs, the
/// `simple` seam; execute.rs `simple_at` with no kept snapshot), and get_relation_info opened every valid index of the
/// table with the RTE's rellockmode, closed with NoLock (plancat.rs), so the probe hits. S1x's Bind runs a cached plan:
/// AcquireExecutorLocks locked the range table's relations only, so a transaction's first Bind misses and the index is
/// locked here: never read unlocked.
pub(super) fn index_lockmode(s: &Shape<'_>) -> LOCKMODE {
    if lmgr_seams::check_relation_locked_by_me::call(s.scan.indexid, s.lockmode, false) {
        NoLock
    } else {
        s.lockmode
    }
}

fn probe_rows<'m>(s: &Shape<'_>, mcx: Mcx<'m>, wire: &mut gravity_wire::WireNode, heap: &Relation<'m>, index: &Relation<'m>, ops: &probe::KeyOps) -> PgResult<u64> {
    probe::key_gates(&s.scan.indexqual)?;
    let tlist: &NodeList<'_> = &s.scan.scan.plan.targetlist;
    let n = tlist.len();
    let mut cols: PgVec<'m, OutCol> = PgVec::new_in(mcx);
    cols.try_reserve_exact(n).map_err(|_| mcx.oom(n * core::mem::size_of::<OutCol>()))?;
    for t in tlist.iter() {
        cols.push(out_col(t.as_target_entry().expect("checked by shape"))?);
    }
    wire.prepare_cols(&cols, None)?;
    wire.reset();
    wire.send_rowdesc()?;
    let mut values: PgVec<'m, u64> = PgVec::new_in(mcx);
    let mut isnull: PgVec<'m, u8> = PgVec::new_in(mcx);
    values.try_reserve_exact(n).map_err(|_| mcx.oom(n * 8))?;
    isnull.try_reserve_exact(n).map_err(|_| mcx.oom(n))?;
    probe::run(mcx, heap, index, &s.scan.indexqual, ops, |slot| {
        project(tlist, slot, &mut values, &mut isnull);
        wire.send_row_in(&values, &isnull, mcx)
    })
}

/// ExecProject over the scanned row: each entry's column (slot_getattr) or constant, into `values` / `isnull`.
pub(super) fn project(tlist: &NodeList<'_>, slot: &mut SlotData<'_>, values: &mut PgVec<'_, u64>, isnull: &mut PgVec<'_, u8>) {
    values.clear();
    isnull.clear();
    for t in tlist.iter() {
        let te = t.as_target_entry().expect("checked by shape");
        let (v, null) = match te.expr.as_var() {
            Some(var) => {
                let mut null = false;
                let d = exectuples::slot_getattr(slot, var.varattno as i32, &mut null);
                (d.as_u64(), null)
            }
            None => {
                let c = te.expr.as_const().expect("checked by shape");
                (c.constvalue.as_u64(), c.constisnull)
            }
        };
        values.push(v);
        isnull.push(u8::from(null));
    }
}

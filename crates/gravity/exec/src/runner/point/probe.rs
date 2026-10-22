//! pglpoint S1: the unique-key probe, nodeIndexscan.c's road for the one shape the point path takes: a valid, immediate,
//! unique btree of a heap table, equality on every key column against a Const (or, S1x, the portal's parameter). The keys
//! are ExecIndexBuildScanKeys'; the scan is the index AM's own begin / rescan / getnext / end, so HOT chains, visibility,
//! kill_prior_tuple, the second getnext and the AM's pgstat counts (idx_scan, idx_tup_read, idx_tup_fetch) are
//! PostgreSQL's; the heap tuple lands in a buffer heap slot of the table's.
use std::rc::Rc;

use mcx::{Mcx, PgVec};
use types_core::Oid;
use datum::Datum;
use types_error::PgResult;
use types_nodes::list::NodeList;
use types_nodes::primnodes::{Const, Param, ParamKind, INDEX_VAR};
use types_nodes::Node;
use types_portal::params::ParamExternData;
use types_rel::Relation;
use types_scan::scankey::{ScanKeyData, StrategyNumber, SK_ISNULL};
use types_scan::sdir::ScanDirection;
use types_slot::SlotData;
use types_snapshot::SnapshotData;

/// btree's BTEqualStrategyNumber.
const BT_EQUAL: i32 = 3;

/// Each key's btree strategy and right-hand type (ExecIndexBuildScanKeys' get_op_opfamily_properties) in `indexqual`'s
/// order: `index_refusal` looks them up once, `Scan::begin` builds the keys from them.
pub(super) type KeyOps = [(i32, Oid); types_core::fmgr::INDEX_MAX_KEYS as usize];

/// One clause of a point probe: `index column attno <op> value`.
#[derive(Clone, Copy)]
pub(super) struct Key<'p> {
    pub attno: i16,
    pub opno: Oid,
    pub opfuncid: Oid,
    pub collid: Oid,
    pub value: Value<'p>,
}

/// A key's comparison value: a constant, or a parameter of the portal's (a generic plan's PARAM_EXTERN: a run-time key of
/// ExecIndexBuildScanKeys, evaluated by ExecIndexEvalRuntimeKeys before the first fetch).
#[derive(Clone, Copy)]
pub(super) enum Value<'p> {
    Const(&'p Const),
    Param(&'p Param),
}

impl Value<'_> {
    /// The value and its null flag: a parameter's from `params` (`None`: no value for it here, or not of the type the
    /// plan was made for: the run is not the point path's, and Gravity's raises PostgreSQL's error).
    pub(super) fn get(&self, params: &[ParamExternData]) -> Option<(Datum, bool)> {
        match self {
            Value::Const(c) => Some((c.constvalue, c.constisnull)),
            Value::Param(p) => {
                let prm = params.get(usize::try_from(p.paramid).ok()?.checked_sub(1)?)?;
                (prm.ptype != 0 && prm.ptype == p.paramtype).then_some((prm.value, prm.isnull))
            }
        }
    }
}

/// The column side of an index clause: the index column (a RelabelType over it allowed: the planner's binary-compatible
/// key, e.g. a varchar column under text's operator).
fn index_col(n: Node<'_>) -> Option<i16> {
    let n = n.as_relabel_type().map_or(n, |r| r.arg);
    n.as_var().filter(|v| v.varno == INDEX_VAR && v.varattno > 0).map(|v| v.varattno)
}

/// Clause `i` of `indexqual` as a point key: an OpExpr over two arguments, the index column then a Const or an extern
/// Param. `None` for anything else (an IN list, a row comparison, IS NULL, an expression argument).
pub(super) fn key_of<'p>(clause: Node<'p>) -> Option<Key<'p>> {
    let op = clause.as_op_expr()?;
    if op.args.len() != 2 {
        return None;
    }
    let attno = index_col(op.args.nth(0))?;
    let arg = op.args.nth(1);
    let value = match arg.as_const() {
        Some(c) => Value::Const(c),
        None => Value::Param(arg.as_param().filter(|p| p.paramkind == ParamKind::PARAM_EXTERN)?),
    };
    Some(Key { attno, opno: op.opno, opfuncid: op.opfuncid, collid: op.inputcollid, value })
}

/// Why `index` is not a point probe's for `indexqual` (`None`: it is): a valid, immediate, unique btree with no expression
/// key and no predicate, each key column under exactly one clause whose operator is the column's opfamily equality, in
/// a collation the column's (none, the default, C or POSIX), whose value `params` gives (`Value::get`); `ops` gets each
/// key's strategy and right-hand type.
pub(super) fn index_refusal(index: &Relation<'_>, indexqual: &NodeList<'_>, params: &[ParamExternData], ops: &mut KeyOps) -> PgResult<Option<&'static str>> {
    const BTREE_AM_OID: Oid = 403;
    let Some(form) = index.rd_index.as_ref() else {
        return Ok(Some("not an index"));
    };
    if index.rd_rel.relam != BTREE_AM_OID {
        return Ok(Some("not btree"));
    }
    if !form.indisunique || !form.indimmediate || !form.indisvalid || !form.indisready || form.indcheckxmin {
        return Ok(Some("index not unique, immediate and valid"));
    }
    if form.has_indpred || form.indexprs_src.is_some() {
        return Ok(Some("partial or expression index"));
    }
    let nkeys = form.indnkeyatts as usize;
    let mut seen = [false; types_core::fmgr::INDEX_MAX_KEYS as usize];
    if nkeys == 0 || nkeys > seen.len() || indexqual.len() != nkeys {
        return Ok(Some("key count"));
    }
    for (i, clause) in indexqual.iter().enumerate() {
        let Some(k) = key_of(clause) else {
            return Ok(Some("key shape"));
        };
        if k.value.get(params).is_none() {
            return Ok(Some("parameter"));
        }
        let col = k.attno as usize;
        if col > nkeys || seen[col - 1] || form.indkey.get(col - 1).is_none_or(|&a| a <= 0) {
            return Ok(Some("key columns"));
        }
        seen[col - 1] = true;
        let coll = index.rd_indcollation.get(col - 1).copied().unwrap_or(0);
        if !plain_collation(coll) || !plain_collation(k.collid) {
            return Ok(Some("collation"));
        }
        let (strategy, _, right) = gravity_fn::typesem::op_opfamily_properties(k.opno, index.rd_opfamily[col - 1], false)?;
        if strategy != BT_EQUAL {
            return Ok(Some("not equality"));
        }
        ops[i] = (strategy, right);
    }
    Ok(None)
}

/// Every key of `indexqual` (`index_refusal` passed it for the plan) has a value in `params` of the type the plan was made
/// for (`Value::get`): a kept probe's per-portal half of `index_refusal`'s "parameter" test (pkeep.rs).
pub(super) fn params_fit(indexqual: &NodeList<'_>, params: &[ParamExternData]) -> bool {
    indexqual.iter().all(|clause| key_of(clause).is_some_and(|k| k.value.get(params).is_some()))
}

/// ExecInitIndexScan's ExecInitQual(indexqualorig): ExecInitFunc's EXECUTE gate (and the function-execute object access
/// hook) on each key's operator function, at every run (ExecutorStart: S1's statement, S1x's Bind) after the permission
/// checks and before any message. A role with SELECT on the table and no EXECUTE on the operator's function (int4eq, say)
/// gets PostgreSQL's 'permission denied for function' (D4541). `indexqual` passed `index_refusal`.
pub(super) fn key_gates(indexqual: &NodeList<'_>) -> PgResult<()> {
    for clause in indexqual.iter() {
        let k = key_of(clause).expect("checked by index_refusal");
        if k.opfuncid != 0 {
            gravity_expr::nodes::func::execute_gate(k.opfuncid)?;
        }
    }
    Ok(())
}

/// No collation, the database default, C or POSIX.
fn plain_collation(c: Oid) -> bool {
    const DEFAULT_COLLATION_OID: Oid = 100;
    const C_COLLATION_OID: Oid = 950;
    const POSIX_COLLATION_OID: Oid = 951;
    matches!(c, 0 | DEFAULT_COLLATION_OID | C_COLLATION_OID | POSIX_COLLATION_OID)
}

/// An index scan of the probe's, from ExecInitIndexScan to ExecEndIndexScan: the scan keys built (ExecIndexBuildScanKeys:
/// strategy and subtype from the column's opfamily, the clause's function and collation, the value; a NULL value's key
/// matches nothing), the scan begun under `snap` (ExecInitIndexScan's es_snapshot) and a slot of the table's kind
/// (table_slot_create). `rescan` gives the AM the keys, `next` fetches a row into `slot` (ExecScanFetch's interrupt check
/// first; the tuple's buffer stays pinned until the next fetch or `end`), `end` clears the slot and ends the scan.
pub(super) struct Scan<'m> {
    scan: indexam::IndexScanDescData<'m>,
    pub slot: SlotData<'m>,
    keys: PgVec<'m, ScanKeyData>,
}

impl<'m> Scan<'m> {
    pub(super) fn begin(
        mcx: Mcx<'m>,
        heap: &Relation<'m>,
        index: &Relation<'m>,
        indexqual: &NodeList<'_>,
        params: &[ParamExternData],
        ops: &KeyOps,
        snap: Rc<SnapshotData<'m>>,
    ) -> PgResult<Scan<'m>> {
        let n = indexqual.len();
        let mut keys: PgVec<'m, ScanKeyData> = PgVec::new_in(mcx);
        keys.try_reserve_exact(n).map_err(|_| mcx.oom(n * core::mem::size_of::<ScanKeyData>()))?;
        for (clause, &(strategy, right)) in indexqual.iter().zip(ops.iter()) {
            let k = key_of(clause).expect("checked by index_refusal");
            let (value, isnull) = k.value.get(params).ok_or_else(no_value)?;
            let mut sk = ScanKeyData::empty();
            sk.sk_flags = if isnull { SK_ISNULL } else { 0 };
            sk.sk_attno = k.attno;
            sk.sk_strategy = strategy as StrategyNumber;
            sk.sk_subtype = right;
            sk.sk_collation = k.collid;
            fmgr_core::fmgr_info_into(k.opfuncid, &mut sk.sk_func)?;
            sk.sk_argument = value;
            keys.push(sk);
        }
        let scan = indexam::index_beginscan(mcx, heap, index, snap, n as i32, 0)?;
        let slot = tableam::table_slot_create(mcx, heap)?;
        Ok(Scan { scan, slot, keys })
    }

    /// The slot is of `heap`'s row type (its descriptor the relation's own): a kept scan serves only such a statement or
    /// portal (pkeep.rs: a relcache entry rebuilt since the scan was made has another descriptor).
    pub(super) fn fits(&self, heap: &Relation<'m>) -> bool {
        self.slot.base().tts_tupleDescriptor.as_ref().is_some_and(|d| Rc::ptr_eq(d, &heap.rd_att))
    }

    pub(super) fn rescan(&mut self) -> PgResult<()> {
        indexam::index_rescan(&mut self.scan, Some(&self.keys[..]), None)
    }

    pub(super) fn next(&mut self, mcx: Mcx<'m>) -> PgResult<bool> {
        // ExecScanFetch's CHECK_FOR_INTERRUPTS before each fetch (a cancel, statement_timeout, a terminate)
        cancelpoll::interrupts::check()?;
        indexam::index_getnext_slot(mcx, &mut self.scan, ScanDirection::ForwardScanDirection, &mut self.slot)
    }

    pub(super) fn end(mut self, mcx: Mcx<'m>) -> PgResult<()> {
        exectuples::exec_clear_tuple(&mut self.slot, mcx);
        indexam::index_endscan(self.scan)
    }

    /// A scan kept across statements (keep.rs `ProbeKept`), at its statement's end: the slot cleared and the scan parked
    /// (indexam's `index_parkscan`: no pin, no relation, no snapshot left), for the next statement to `rearm`.
    pub(super) fn park(&mut self, mcx: Mcx<'m>) -> PgResult<()> {
        exectuples::exec_clear_tuple(&mut self.slot, mcx);
        indexam::index_parkscan(&mut self.scan)
    }

    /// A parked scan for a new statement: the per-execution part of index_beginscan over the statement's relations and
    /// snapshot (`index_rearmscan`), then ExecIndexBuildScanKeys' values from the statement's clauses (`indexqual`, the
    /// kept scan's shape: each key's strategy, subtype, collation and function stay the build's) and `params` (a portal's,
    /// pkeep.rs; a statement's keys are its constants, `&[]`). `rescan` follows.
    pub(super) fn rearm(&mut self, heap: &Relation<'m>, index: &Relation<'m>, indexqual: &NodeList<'_>, params: &[ParamExternData], snap: Rc<SnapshotData<'m>>) -> PgResult<()> {
        indexam::index_rearmscan(&mut self.scan, heap, index, snap)?;
        for (sk, clause) in self.keys.iter_mut().zip(indexqual.iter()) {
            let k = key_of(clause).ok_or_else(no_value)?;
            let (value, isnull) = k.value.get(params).ok_or_else(no_value)?;
            sk.sk_flags = if isnull { SK_ISNULL } else { 0 };
            sk.sk_argument = value;
        }
        Ok(())
    }
}

/// The probe of a statement (S1): a `Scan` under the active snapshot (the statement's, which `run::exec_in` pushed), every
/// row the AM returns handed to `row` while its buffer is pinned (a unique point probe finds at most one; the loop runs to
/// the AM's end, as IndexNext does), the scan ended on every exit. Returns the rows handed over.
pub(super) fn run<'m>(
    mcx: Mcx<'m>,
    heap: &Relation<'m>,
    index: &Relation<'m>,
    indexqual: &NodeList<'_>,
    ops: &KeyOps,
    mut row: impl FnMut(&mut SlotData<'m>) -> PgResult<()>,
) -> PgResult<u64> {
    let mut scan = Scan::begin(mcx, heap, index, indexqual, &[], ops, snapmgr::GetActiveSnapshot())?;
    let mut rows = 0u64;
    let r = scan.rescan().and_then(|()| loop {
        if !scan.next(mcx)? {
            return Ok(());
        }
        row(&mut scan.slot)?;
        rows += 1;
    });
    let ended = scan.end(mcx);
    r.and(ended).map(|()| rows)
}

#[cold]
fn no_value() -> Box<types_error::PgError> {
    Box::new(types_error::PgError::error("point path: a probe key with no value").with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR)) // alloc: Cold
}

use std::cell::RefCell;

use datum::Datum;
use mcx::{Mcx, MemoryContext, PgVec};
use types_core::catalog::{
    ACCESS_METHOD_OPERATOR_RELATION_ID, ACCESS_METHOD_PROCEDURE_RELATION_ID, ATTRIBUTE_RELATION_ID, CONSTRAINT_RELATION_ID,
    INDEX_RELATION_ID, OPERATOR_CLASS_RELATION_ID, OPERATOR_FAMILY_RELATION_ID, RELATION_RELATION_ID,
};
use types_core::{InvalidOid, Oid};
use types_error::{PgError, PgResult, ERROR, FATAL};
use types_rel::RelationData;
use types_storage::{
    RelFileLocatorBackend, SharedInvalRelmapMsg, SharedInvalSmgrMsg, SharedInvalidationMessage,
};
use types_tuple::HeapTupleData;

use crate::registration::{
    self, prepare_inplace_invalidation_state, prepare_invalidation_state,
};
use crate::{
    with_state, RelSyncCallbackFunction, RelcacheCallbackFunction, RelcacheCallbackItem,
    RelsyncCallbackItem, SyscacheCallbackFunction, SyscacheCallbackItem, CALLBACKS,
    MAX_RELCACHE_CALLBACKS, MAX_RELSYNC_CALLBACKS, MAX_SYSCACHE_CALLBACKS, SYS_CACHE_SIZE,
};

fn MyDatabaseId() -> Oid {
    init_small::globals::MyDatabaseId()
}

#[track_caller]
#[cold]
#[inline(never)]
fn err_invalid_cache_id(level: types_error::ErrorLevel, cacheid: i32) -> Box<PgError> {
    Box::new(PgError::new(level, format!("invalid cache ID: {cacheid}")))
}

#[track_caller]
#[cold]
#[inline(never)]
fn err_out_of_slots(list: &str) -> Box<PgError> {
    Box::new(PgError::new(FATAL, format!("out of {list} slots")))
}

thread_local! {
    static TUPLE_INVAL_SCRATCH: RefCell<MemoryContext> =
        RefCell::new(MemoryContext::new("PrepareToInvalidateCacheTuple scratch"));
}

// Reset-per-acquisition scratch; a re-entrant invalidation inside the seam
// falls back to a fresh context so the buffer is never aliased across nesting.
fn with_tuple_inval_scratch<R>(f: impl for<'s> FnOnce(Mcx<'s>) -> PgResult<R>) -> PgResult<R> {
    TUPLE_INVAL_SCRATCH.with(|cell| match cell.try_borrow_mut() {
        Ok(mut ctx) => {
            ctx.reset();
            f(ctx.mcx())
        }
        Err(_) => {
            let ctx = MemoryContext::new("PrepareToInvalidateCacheTuple (reentrant)");
            f(ctx.mcx())
        }
    })
}

fn cache_invalidate_heap_tuple_common(
    relation: &RelationData<'_>,
    tuple: &HeapTupleData<'_>,
    newtuple: Option<&HeapTupleData<'_>>,
    use_inplace: bool,
) -> PgResult<()> {
    if miscinit_seams::is_bootstrap_processing_mode::call() {
        return Ok(());
    }

    if !catalog_seams::is_catalog_relation::call(relation) {
        return Ok(());
    }
    if catalog_seams::is_toast_relation::call(relation) {
        return Ok(());
    }

    let tuple_rel_id = relation.rd_id;
    let snapshots_only = syscache_seams::relation_invalidates_snapshots_only::call(tuple_rel_id);

    with_tuple_inval_scratch(|scratch| {
        // The catcache walk may lazily init a catcache and re-enter via
        // CallSyscacheCallbacks, so it runs BEFORE the state borrow (C order
        // differs: prepare_callback first); its Copy requests replay under it.
        let reqs = if snapshots_only {
            PgVec::new_in(scratch)
        } else {
            catcache_seams::prepare_to_invalidate_cache_tuple::call(
                scratch, relation, tuple, newtuple,
            )?
        };
        // D3866: the row-stamped caches whose program stamps this transaction's commit moves
        let touched = if reqs.iter().any(|r| l2cache::row_stamped(r.cache_id))
            && changes_committed_row(tuple, newtuple, use_inplace)
        {
            reqs.iter().fold(0u128, |m, r| m | crate::eoxact::stamp_bit(r.cache_id))
        } else {
            0
        };

        let rel_target: Option<(Oid, Oid)> = if tuple_rel_id == RELATION_RELATION_ID {
            let classtup = syscache_seams::pg_class_shape::call(tuple);
            let database_id = if classtup.relisshared { InvalidOid } else { MyDatabaseId() };
            Some((classtup.oid, database_id))
        } else if tuple_rel_id == ATTRIBUTE_RELATION_ID {
            // KLUGE ALERT (C): always MyDatabaseId, even for shared rels.
            Some((syscache_seams::pg_attribute_attrelid::call(tuple), MyDatabaseId()))
        } else if tuple_rel_id == INDEX_RELATION_ID {
            Some((syscache_seams::pg_index_indexrelid::call(tuple), MyDatabaseId()))
        } else if tuple_rel_id == CONSTRAINT_RELATION_ID {
            syscache_seams::pg_constraint_fk_target::call(tuple)
                .map(|conrelid| (conrelid, MyDatabaseId()))
        } else {
            None
        };

        // C3 S5 (D3889): a pg_class row update that changed only the relation's storage and size statistics registers
        // its messages under the storage scope (`REL_SCOPE_STORAGE`)
        let storage_only = tuple_rel_id == RELATION_RELATION_ID
            && !use_inplace
            && newtuple.is_some_and(|n| pg_class_storage_only(relation, tuple, n));
        let scope = if storage_only { crate::relcache_scope() | crate::REL_SCOPE_STORAGE } else { crate::relcache_scope() };
        crate::with_relcache_scope(scope, || with_state(|state| {
            let mcx = state.mcx;
            let info = if use_inplace {
                prepare_inplace_invalidation_state(state)
            } else {
                prepare_invalidation_state(state)?
            };

            if snapshots_only {
                let database_id = if catalog_seams::is_shared_relation::call(tuple_rel_id) {
                    InvalidOid
                } else {
                    MyDatabaseId()
                };
                registration::register_snapshot_invalidation(
                    mcx, state, info, database_id, tuple_rel_id,
                )?;
            } else {
                for req in reqs.iter() {
                    registration::register_catcache_invalidation(
                        mcx, state, info, req.cache_id, req.hash_value, req.db_id,
                    )?;
                }
                state.stamp_touched |= touched;
                // D3888: the family a support catalog row names, both images (`PreCommit_Inval` names its relations)
                for t in core::iter::once(tuple).chain(newtuple) {
                    if let Some(f) = support_family(relation, t).filter(|f| !state.support_families.contains(f)) {
                        state.support_families.push(f);
                    }
                }
            }

            match rel_target {
                Some((relation_id, database_id)) => registration::register_relcache_invalidation(
                    mcx, state, info, database_id, relation_id,
                ),
                None => Ok(()),
            }
        }))
    })
}

/// D3888: the operator family support catalog row `t` of `relation` names: pg_amop's and pg_amproc's family, pg_opclass's,
/// pg_opfamily's own OID. An index's relcache entry (rd_opfamily, rd_support) and a partition key are built from the
/// family's rows, and no relcache message of theirs covers those rows.
fn support_family(relation: &RelationData<'_>, t: &HeapTupleData<'_>) -> Option<Oid> {
    let attnum = match relation.rd_id {
        ACCESS_METHOD_OPERATOR_RELATION_ID | ACCESS_METHOD_PROCEDURE_RELATION_ID => 2,
        OPERATOR_CLASS_RELATION_ID => 6,
        OPERATOR_FAMILY_RELATION_ID => 1,
        _ => return None,
    };
    let mut null = false;
    // SAFETY: `t` is a row of `relation`, read under its descriptor (the relation the tuple invalidation names).
    let d = unsafe { types_tuple::heap_getattr(t, attnum, relation.descr(), &mut null) };
    (!null).then(|| d.as_oid())
}

/// pg_class's columns (PostgreSQL 18 `pg_class.h`) a relation's storage and its size statistics alone change: a new
/// relfilenode (TRUNCATE, REINDEX, CLUSTER / VACUUM FULL's swap), the pages, tuples and all-visible / all-frozen counts,
/// the freeze horizons.
const PG_CLASS_STORAGE_ATTS: [i32; 7] = [8, 10, 11, 12, 13, 30, 31];
const ANUM_PG_CLASS_RELAM: i32 = 7;
const ANUM_PG_CLASS_RELKIND: i32 = 18;
const HEAP_TABLE_AM_OID: Oid = 2;
const BTREE_AM_OID: Oid = 403;

/// C3 S5 (D3889): update `old` -> `new` of a pg_class row changes no column but the storage ones
/// (`PG_CLASS_STORAGE_ATTS`), of a heap relation, a btree index or a sequence (another access method's catalog-held
/// state is not audited: its updates stay whole). Every other column compares by image (a toasted or otherwise
/// re-encoded equal value compares unequal: whole, never a missed change).
fn pg_class_storage_only(relation: &RelationData<'_>, old: &HeapTupleData<'_>, new: &HeapTupleData<'_>) -> bool {
    let desc = relation.descr();
    let get = |t: &HeapTupleData<'_>, attnum: i32| {
        let mut null = false;
        // SAFETY: `t` is a pg_class row and `desc` pg_class's descriptor (the relation the tuple invalidation names).
        let d = unsafe { types_tuple::heap_getattr(t, attnum, desc, &mut null) };
        (!null).then_some(d)
    };
    let am = get(new, ANUM_PG_CLASS_RELAM).map_or(InvalidOid, |d| d.as_oid());
    let sequence = get(new, ANUM_PG_CLASS_RELKIND).is_some_and(|d| d.as_i8() == types_rel::RELKIND_SEQUENCE as i8);
    if !(am == HEAP_TABLE_AM_OID || am == BTREE_AM_OID || sequence) {
        return false;
    }
    (1..=desc.natts).filter(|a| !PG_CLASS_STORAGE_ATTS.contains(a)).all(|a| {
        let att = desc.attr(a as usize - 1);
        match (get(old, a), get(new, a)) {
            (None, None) => true,
            (Some(x), Some(y)) => image_eq(x, y, att.attbyval, att.attlen),
            _ => false,
        }
    })
}

/// `datumIsEqual`: the two images compare byte for byte (no detoasting).
fn image_eq(x: Datum, y: Datum, byval: bool, len: i16) -> bool {
    if byval {
        return x == y;
    }
    let size = |d: Datum| match len {
        n if n > 0 => Some(n as usize),
        // SAFETY: a non-null by-reference varlena column of a live tuple.
        -1 => Some(unsafe { types_tuple::varatt::varsize_any(d.as_usize() as *const u8) }),
        _ => None,
    };
    match (size(x), size(y)) {
        (Some(a), Some(b)) if a == b => {
            // SAFETY: both images live in their tuples for `a` bytes.
            unsafe { core::slice::from_raw_parts(x.as_usize() as *const u8, a) == core::slice::from_raw_parts(y.as_usize() as *const u8, a) }
        }
        _ => false,
    }
}

/// D3866: whether this tuple invalidation changes a catalog row committed before this transaction, so that its commit
/// moves a row-stamped cache's program stamp (l2cache `ROW_STAMPED_CAT_IDS`): an update of such a row, an in-place
/// update, or an invalidation a command forces on an unchanged row (ALTER DOMAIN's of the domain's pg_type row). A row
/// this transaction created (an insert, either version of an update of it) changes nothing another backend read, and
/// one it deleted names an object no statement can name again (until the OID counter wraps, which the stamp mixes:
/// l2cache `oid_wraps`). Without the transaction seam (a unit build): every row counts as changed.
fn changes_committed_row(
    tuple: &HeapTupleData<'_>,
    newtuple: Option<&HeapTupleData<'_>>,
    use_inplace: bool,
) -> bool {
    use types_tuple::htup::{HEAP_XMAX_INVALID, HEAP_XMAX_IS_LOCKED_ONLY, HEAP_XMAX_IS_MULTI};
    let current = xact_seams::transaction_id_is_current_transaction_id::call;
    if use_inplace || !xact_seams::transaction_id_is_current_transaction_id::is_installed() {
        return true;
    }
    let h = tuple.t_data();
    if current(h.xmin()) {
        return false;
    }
    if newtuple.is_some() {
        return true;
    }
    let xmax = h.xmax_raw();
    let deleted_here = xmax != types_core::xact::InvalidTransactionId
        && h.t_infomask & (HEAP_XMAX_INVALID | HEAP_XMAX_IS_MULTI) == 0
        && !HEAP_XMAX_IS_LOCKED_ONLY(h.t_infomask)
        && current(xmax);
    !deleted_here
}

pub fn CacheInvalidateHeapTuple(
    relation: &RelationData<'_>,
    tuple: &HeapTupleData<'_>,
    newtuple: Option<&HeapTupleData<'_>>,
) -> PgResult<()> {
    cache_invalidate_heap_tuple_common(relation, tuple, newtuple, false)
}

pub fn CacheInvalidateHeapTupleInplace(
    relation: &RelationData<'_>,
    key_equivalent_tuple: &HeapTupleData<'_>,
) -> PgResult<()> {
    cache_invalidate_heap_tuple_common(relation, key_equivalent_tuple, None, true)
}

pub fn CacheInvalidateCatalog(catalogId: Oid) -> PgResult<()> {
    let database_id = if catalog_seams::is_shared_relation::call(catalogId) {
        InvalidOid
    } else {
        MyDatabaseId()
    };

    with_state(|state| {
        let mcx = state.mcx;
        let info = prepare_invalidation_state(state)?;
        registration::register_catalog_invalidation(mcx, state, info, database_id, catalogId)
    })
}

pub fn CacheInvalidateRelcache(relation: &RelationData<'_>) -> PgResult<()> {
    let relation_id = relation.rd_id;
    let database_id = if relation.rd_rel.relisshared {
        InvalidOid
    } else {
        MyDatabaseId()
    };

    with_state(|state| {
        let mcx = state.mcx;
        let info = prepare_invalidation_state(state)?;
        registration::register_relcache_invalidation(mcx, state, info, database_id, relation_id)
    })
}

pub fn CacheInvalidateRelcacheAll() -> PgResult<()> {
    with_state(|state| {
        let mcx = state.mcx;
        let info = prepare_invalidation_state(state)?;
        registration::register_relcache_invalidation(mcx, state, info, InvalidOid, InvalidOid)
    })
}

pub fn CacheInvalidateRelcacheByTuple(classTuple: &HeapTupleData<'_>) -> PgResult<()> {
    let classtup = syscache_seams::pg_class_shape::call(classTuple);
    let relation_id = classtup.oid;
    let database_id = if classtup.relisshared { InvalidOid } else { MyDatabaseId() };

    with_state(|state| {
        let mcx = state.mcx;
        let info = prepare_invalidation_state(state)?;
        registration::register_relcache_invalidation(mcx, state, info, database_id, relation_id)
    })
}

pub fn CacheInvalidateRelcacheByRelid(relid: Oid) -> PgResult<()> {
    if cache_invalidate_relcache_by_relid_if_exists(relid)? {
        Ok(())
    } else {
        Err(err_cache_lookup_failed(relid))
    }
}

/// The operator-family scan does not lock each returned relation. A concurrent
/// DROP can remove one before this lookup; DROP sends its own invalidation and
/// there is no surviving support entry to refresh. Other lookup errors remain
/// errors, and ordinary callers retain the strict public entry point above.
pub(crate) fn cache_invalidate_relcache_by_relid_if_exists(relid: Oid) -> PgResult<bool> {
    let classtup = match syscache_seams::lookup_pg_class_by_relid::call(relid)? {
        Some(shape) => shape,
        None => return Ok(false),
    };
    let relation_id = classtup.oid;
    let database_id = if classtup.relisshared { InvalidOid } else { MyDatabaseId() };

    with_state(|state| {
        let mcx = state.mcx;
        let info = prepare_invalidation_state(state)?;
        registration::register_relcache_invalidation(mcx, state, info, database_id, relation_id)
    })?;
    Ok(true)
}

#[track_caller]
#[cold]
#[inline(never)]
fn err_cache_lookup_failed(relid: Oid) -> Box<PgError> {
    Box::new(PgError::error(format!("cache lookup failed for relation {relid}")))
}

pub fn CacheInvalidateRelSync(relid: Oid) -> PgResult<()> {
    with_state(|state| {
        let mcx = state.mcx;
        let info = prepare_invalidation_state(state)?;
        registration::register_relsync_invalidation(mcx, state, info, MyDatabaseId(), relid)
    })
}

pub fn CacheInvalidateRelSyncAll() -> PgResult<()> {
    CacheInvalidateRelSync(InvalidOid)
}

// Nontransactional, sent immediately; three ProcNumber bytes travel in union
// padding, hence the MAX_BACKENDS_BITS bound.
pub fn CacheInvalidateSmgr(rlocator: RelFileLocatorBackend) -> PgResult<()> {
    const { assert!(crate::MAX_BACKENDS_BITS <= 23) };

    let msg = SharedInvalidationMessage::Smgr(SharedInvalSmgrMsg {
        backend_hi: (rlocator.backend >> 16) as i8,
        backend_lo: (rlocator.backend & 0xffff) as u16,
        rlocator: rlocator.locator,
    });
    sinval_seams::send_shared_invalid_messages::call(&[msg])
}

pub fn CacheInvalidateRelmap(databaseId: Oid) -> PgResult<()> {
    let msg = SharedInvalidationMessage::Relmap(SharedInvalRelmapMsg { dbId: databaseId });
    sinval_seams::send_shared_invalid_messages::call(&[msg])
}

pub fn CacheRegisterSyscacheCallback(
    cacheid: i32,
    func: SyscacheCallbackFunction,
    arg: Datum,
) -> PgResult<()> {
    if cacheid < 0 || cacheid >= SYS_CACHE_SIZE as i32 {
        return Err(err_invalid_cache_id(FATAL, cacheid));
    }
    CALLBACKS.with(|cell| {
        let t = &mut *cell.borrow_mut();
        // D4682: a registration already in the table is not added again. The table lives as long as the thread and is
        // never unregistered, while a cache that registers when it is first built is built again by each session a
        // reused client thread runs (launch_backend cpool); a duplicate would run the same callback twice per message
        // and, session after session, run the table out of slots.
        if t.syscache_list[..t.syscache_count].iter().flatten().any(|c| {
            c.id == cacheid as i16 && core::ptr::fn_addr_eq(c.function, func) && c.arg == arg
        }) {
            return Ok(());
        }
        if t.syscache_count >= MAX_SYSCACHE_CALLBACKS {
            return Err(err_out_of_slots("syscache_callback_list"));
        }

        let count = t.syscache_count as i16;
        let head = crate::SYSCACHE_LINKS.with(|links| links[cacheid as usize].get());
        if head == 0 {
            crate::SYSCACHE_LINKS.with(|links| links[cacheid as usize].set(count + 1));
        } else {
            let mut i = (head - 1) as usize;
            while t.syscache_list[i].expect("linked slot populated").link > 0 {
                i = (t.syscache_list[i].expect("linked slot populated").link - 1) as usize;
            }
            t.syscache_list[i].as_mut().expect("linked slot populated").link = count + 1;
        }

        t.syscache_list[t.syscache_count] = Some(SyscacheCallbackItem {
            id: cacheid as i16,
            link: 0,
            function: func,
            arg,
        });
        t.syscache_count += 1;
        Ok(())
    })
}

pub fn CacheRegisterRelcacheCallback(func: RelcacheCallbackFunction, arg: Datum) -> PgResult<()> {
    CALLBACKS.with(|cell| {
        let t = &mut *cell.borrow_mut();
        // D4682: as CacheRegisterSyscacheCallback, a registration already in the table is not added again.
        if t.relcache_list[..t.relcache_count]
            .iter()
            .flatten()
            .any(|c| core::ptr::fn_addr_eq(c.function, func) && c.arg == arg)
        {
            return Ok(());
        }
        if t.relcache_count >= MAX_RELCACHE_CALLBACKS {
            return Err(err_out_of_slots("relcache_callback_list"));
        }
        t.relcache_list[t.relcache_count] = Some(RelcacheCallbackItem { function: func, arg });
        t.relcache_count += 1;
        Ok(())
    })
}

pub fn CacheRegisterRelSyncCallback(func: RelSyncCallbackFunction, arg: Datum) -> PgResult<()> {
    CALLBACKS.with(|cell| {
        let t = &mut *cell.borrow_mut();
        // D4682: as CacheRegisterSyscacheCallback, a registration already in the table is not added again.
        if t.relsync_list[..t.relsync_count]
            .iter()
            .flatten()
            .any(|c| core::ptr::fn_addr_eq(c.function, func) && c.arg == arg)
        {
            return Ok(());
        }
        if t.relsync_count >= MAX_RELSYNC_CALLBACKS {
            return Err(err_out_of_slots("relsync_callback_list"));
        }
        t.relsync_list[t.relsync_count] = Some(RelsyncCallbackItem { function: func, arg });
        t.relsync_count += 1;
        Ok(())
    })
}

#[inline]
pub fn CallSyscacheCallbacks(cacheid: i32, hashvalue: u32) -> PgResult<()> {
    if cacheid < 0 || cacheid >= SYS_CACHE_SIZE as i32 {
        return Err(err_invalid_cache_id(ERROR, cacheid));
    }

    let head = crate::SYSCACHE_LINKS.with(|links| links[cacheid as usize].get());
    if head > 0 {
        call_syscache_chain(head, cacheid, hashvalue);
    }
    Ok(())
}

#[inline(never)]
fn call_syscache_chain(head: i16, cacheid: i32, hashvalue: u32) {
    let mut i = head as i32 - 1;
    while i >= 0 {
        let ccitem = CALLBACKS
            .with(|c| c.borrow().syscache_list[i as usize])
            .expect("linked slot populated");
        debug_assert_eq!(ccitem.id, cacheid as i16);
        (ccitem.function)(ccitem.arg, cacheid, hashvalue);
        // C re-reads ccitem->link after the callback (it may have registered).
        i = CALLBACKS.with(|c| {
            c.borrow().syscache_list[i as usize]
                .expect("linked slot populated")
                .link
        }) as i32
            - 1;
    }
}

pub fn CallRelSyncCallbacks(relid: Oid) -> PgResult<()> {
    let mut i = 0usize;
    while let Some(ccitem) = CALLBACKS.with(|c| {
        let t = c.borrow();
        if i < t.relsync_count { t.relsync_list[i] } else { None }
    }) {
        (ccitem.function)(ccitem.arg, relid);
        i += 1;
    }
    Ok(())
}

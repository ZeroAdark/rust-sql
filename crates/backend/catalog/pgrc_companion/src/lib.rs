//! Ring slot `n` of pgrcolumnar table `p` is the heap `pg_toast.pgrc_seg_<p>_<n>`: the parent's columns at the parent's attnums.
use mcx::Mcx;
use pgrc_format::dml::Tombstone;
use pgrc_front_seams::{TombHit, TombInsert, TombState};
use tableam_vocab::{LockTupleMode, Snapshot};
use types_core::{AttrNumber, CommandId, InvalidOid, Oid, COLLATION_RELATION_ID, DEFAULT_COLLATION_OID, INDEX_MAX_KEYS, PG_TOAST_NAMESPACE, RELATION_RELATION_ID, TYPE_RELATION_ID};
use types_error::{PgError, PgResult};
use types_rel::{AccessExclusiveLock, AccessShareLock, NoLock, Relation, RowExclusiveLock, RELKIND_RELATION};
use types_scan::scankey::{BTEqualStrategyNumber, ScanKeyData};
use types_slot::SlotData;
use types_tuple::HeapTupleData;

const INT4OID: Oid = 23;
const INT8OID: Oid = 20;
const INT8_BTREE_OPS_OID: Oid = 3124;
const F_INT8EQ: Oid = 467;

pub fn segment_name(parent: Oid, slot: u16) -> String {
    format!("pgrc_seg_{parent}_{slot}")
}

fn namespace_of(parent_ns: Oid) -> Oid {
    match catalog_namespace::isTempOrTempToastNamespace(parent_ns) {
        true => catalog_namespace::GetTempToastNamespace(),
        false => PG_TOAST_NAMESPACE,
    }
}

pub fn segment_relid(parent: Oid, slot: u16) -> PgResult<Option<Oid>> {
    let oid = lsyscache::get_relname_relid(&segment_name(parent, slot), namespace_of(lsyscache::get_rel_namespace(parent)?))?;
    Ok((oid != InvalidOid).then_some(oid))
}

/// Idempotent; the caller holds a parent lock that serializes creation.
pub fn create_segment<'mcx>(mcx: Mcx<'mcx>, parent: Oid, slot: u16) -> PgResult<Oid> {
    if let Some(oid) = segment_relid(parent, slot)? {
        return Ok(oid);
    }
    let rel = table::table_open(mcx, parent, NoLock)?;
    let out = create_guts(mcx, &rel, slot);
    rel.close(NoLock)?;
    out
}

fn create_guts<'mcx>(mcx: Mcx<'mcx>, rel: &types_rel::Relation<'mcx>, slot: u16) -> PgResult<Oid> {
    debug_assert!(tableam_vocab::is_pgrcolumnar_am_oid(rel.rd_rel.relam));
    if rel.rd_rel.relisshared {
        return Err(Box::new(PgError::error("pgrcolumnar does not support shared relations".to_string()).with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED)));
    }
    let natts = rel.rd_att.natts as usize;
    let mut desc = tupdesc::CreateTemplateTupleDesc(mcx, natts as i32)?;
    let mut dropped: Vec<AttrNumber> = Vec::new();
    for i in 0..natts {
        let att = rel.rd_att.attr(i);
        let attnum = (i + 1) as AttrNumber;
        if att.attisdropped {
            tupdesc::TupleDescInitEntry(&mut desc, attnum, Some(&format!("pgrc_dropped_{attnum}")), INT4OID, -1, 0)?;
            dropped.push(attnum);
            continue;
        }
        let name = String::from_utf8_lossy(att.attname.name_str()).into_owned();
        tupdesc::TupleDescInitEntry(&mut desc, attnum, Some(&name), att.atttypid, att.atttypmod, att.attndims as i32)?;
    }
    let oid = catalog_heap::heap_create_with_catalog(
        mcx,
        &catalog_heap::HeapCreateParams {
            relname: &segment_name(rel.rd_id, slot),
            relid: InvalidOid,
            reltypeid: InvalidOid,
            relnamespace: namespace_of(rel.rd_rel.relnamespace),
            reltablespace: rel.rd_rel.reltablespace,
            ownerid: rel.rd_rel.relowner,
            accessmtd: tableam_vocab::HEAP_TABLE_AM_OID,
            relkind: RELKIND_RELATION,
            relpersistence: rel.rd_rel.relpersistence,
            reloftype: InvalidOid,
            shared: false,
            mapped: false,
            allow_system_table_mods: true,
            reloptions: None,
            relrewrite: InvalidOid,
            use_user_acl: false,
            is_internal: true,
        },
        &desc,
    )?;
    xact::CommandCounterIncrement()?;
    for &attnum in &dropped {
        catalog_heap::RemoveAttributeById(mcx, oid, attnum)?;
    }
    // D4785: the columns depend on their types and collations AUTO (heap_create_with_catalog recorded them NORMAL): the
    // segment is an internal part of its parent, so a DROP TYPE / DOMAIN reports the parent's column alone
    pg_depend::deleteDependencyRecordsForClass(mcx, RELATION_RELATION_ID, oid, TYPE_RELATION_ID, pg_depend::DependencyType::Normal)?;
    pg_depend::deleteDependencyRecordsForClass(mcx, RELATION_RELATION_ID, oid, COLLATION_RELATION_ID, pg_depend::DependencyType::Normal)?;
    for (i, att) in desc.attrs.iter().enumerate().take(natts) {
        let attnum = (i + 1) as AttrNumber;
        if dropped.contains(&attnum) {
            continue;
        }
        let col = pg_depend::ObjectAddress::sub_set(RELATION_RELATION_ID, oid, attnum as i32);
        pg_depend::recordDependencyOn(mcx, &col, &pg_depend::ObjectAddress::set(TYPE_RELATION_ID, att.atttypid), pg_depend::DependencyType::Auto)?;
        if att.attcollation != InvalidOid && att.attcollation != DEFAULT_COLLATION_OID {
            pg_depend::recordDependencyOn(mcx, &col, &pg_depend::ObjectAddress::set(COLLATION_RELATION_ID, att.attcollation), pg_depend::DependencyType::Auto)?;
        }
    }
    let base = pg_depend::ObjectAddress::set(RELATION_RELATION_ID, rel.rd_id);
    let seg = pg_depend::ObjectAddress::set(RELATION_RELATION_ID, oid);
    pg_depend::recordDependencyOn(mcx, &seg, &base, pg_depend::DependencyType::Internal)?;
    xact::CommandCounterIncrement()?;
    catalog_toasting::NewRelationCreateToastTable(mcx, oid, None)?;
    xact::CommandCounterIncrement()?;
    Ok(oid)
}

pub fn reset_segment<'mcx>(mcx: Mcx<'mcx>, relid: Oid) -> PgResult<()> {
    let rel = table::table_open(mcx, relid, AccessExclusiveLock)?;
    catalog_index::RelationSetNewRelfilenumber(mcx, &rel, rel.rd_rel.relpersistence)?;
    let toast = rel.rd_rel.reltoastrelid;
    rel.close(NoLock)?;
    if toast != InvalidOid {
        let t = table::table_open(mcx, toast, AccessExclusiveLock)?;
        catalog_index::RelationSetNewRelfilenumber(mcx, &t, t.rd_rel.relpersistence)?;
        t.close(NoLock)?;
    }
    catalog_index::reindex_relation(mcx, relid, catalog_index::REINDEX_REL_PROCESS_TOAST, &catalog_index::ReindexParams::default(), &mut |_| {})?;
    xact::CommandCounterIncrement()
}

pub use reorderbuffer_seams::CompanionKind;

fn canonical_u32(digits: &[u8]) -> Option<u32> {
    let n: u32 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    (n.to_string().as_bytes() == digits).then_some(n)
}

/// Temp companions (the temp toast namespace) are never logically logged, so only `pg_toast` is asked about.
pub fn kind_of(relnamespace: Oid, relname: &[u8], relkind: u8) -> Option<(Oid, CompanionKind)> {
    if relnamespace != PG_TOAST_NAMESPACE || relkind != RELKIND_RELATION {
        return None;
    }
    if let Some(rest) = relname.strip_prefix(b"pgrc_tomb_") {
        let (parent, slot) = match rest.iter().position(|&b| b == b'_') {
            None => (canonical_u32(rest)?, None),
            Some(at) => (canonical_u32(&rest[..at])?, Some(canonical_u32(&rest[at + 1..])?)),
        };
        let named = slot.is_none_or(|s| s >= 1 && s < u32::from(pgrc_format::manifest::TOMB_SLOTS));
        return named.then_some((parent, CompanionKind::Tombstone));
    }
    let rest = relname.strip_prefix(b"pgrc_seg_")?;
    let at = rest.iter().position(|&b| b == b'_')?;
    let parent = canonical_u32(&rest[..at])?;
    let slot = u16::try_from(canonical_u32(&rest[at + 1..])?).ok()?;
    Some((parent, CompanionKind::Segment { slot }))
}

/// Another session's temp relations never reach VACUUM FULL's and CLUSTER's rewrites.
pub fn companion_parent(relnamespace: Oid, relname: &[u8], relkind: u8) -> Option<Oid> {
    let ns = match catalog_namespace::isTempToastNamespace(relnamespace) {
        true => PG_TOAST_NAMESPACE,
        false => relnamespace,
    };
    kind_of(ns, relname, relkind).map(|(parent, _)| parent)
}

pub fn tombstone_name(parent: Oid) -> String {
    tomb_name(parent, 0)
}

pub fn tomb_name(parent: Oid, slot: u8) -> String {
    match slot {
        0 => format!("pgrc_tomb_{parent}"),
        s => format!("pgrc_tomb_{parent}_{s}"),
    }
}

pub fn tombstone_relid(parent: Oid) -> PgResult<Option<Oid>> {
    tomb_relid(parent, 0)
}

pub fn tomb_relid(parent: Oid, slot: u8) -> PgResult<Option<Oid>> {
    let oid = lsyscache::get_relname_relid(&tomb_name(parent, slot), namespace_of(lsyscache::get_rel_namespace(parent)?))?;
    Ok((oid != InvalidOid).then_some(oid))
}

pub fn tomb_relids(parent: Oid) -> PgResult<Vec<Oid>> {
    (0..pgrc_format::manifest::TOMB_SLOTS).filter_map(|s| tomb_relid(parent, s).transpose()).collect()
}

/// Idempotent; the caller holds a parent lock that serializes creation.
pub fn create_tombstone<'mcx>(mcx: Mcx<'mcx>, parent: Oid) -> PgResult<Oid> {
    create_tomb(mcx, parent, 0)
}

pub fn create_tomb<'mcx>(mcx: Mcx<'mcx>, parent: Oid, slot: u8) -> PgResult<Oid> {
    if let Some(oid) = tomb_relid(parent, slot)? {
        return Ok(oid);
    }
    let rel = table::table_open(mcx, parent, NoLock)?;
    let out = create_tomb_guts(mcx, &rel, slot);
    rel.close(NoLock)?;
    out
}

fn create_tomb_guts<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, slot: u8) -> PgResult<Oid> {
    if rel.rd_rel.relisshared {
        return Err(Box::new(PgError::error("pgrcolumnar does not support shared relations".to_string()).with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED)));
    }
    const _: () = assert!(pgrc_format::dml::TOMBSTONE_NATTS == 2);
    let mut desc = tupdesc::CreateTemplateTupleDesc(mcx, 2)?;
    for (i, name) in ["rowident", "succ"].into_iter().enumerate() {
        tupdesc::TupleDescInitEntry(&mut desc, i as AttrNumber + 1, Some(name), INT8OID, -1, 0)?;
        desc.attr_mut(i).attnotnull = true;
    }
    let name = tomb_name(rel.rd_id, slot);
    let oid = catalog_heap::heap_create_with_catalog(
        mcx,
        &catalog_heap::HeapCreateParams {
            relname: &name,
            relid: InvalidOid,
            reltypeid: InvalidOid,
            relnamespace: namespace_of(rel.rd_rel.relnamespace),
            reltablespace: rel.rd_rel.reltablespace,
            ownerid: rel.rd_rel.relowner,
            accessmtd: tableam_vocab::HEAP_TABLE_AM_OID,
            relkind: RELKIND_RELATION,
            relpersistence: rel.rd_rel.relpersistence,
            reloftype: InvalidOid,
            shared: false,
            mapped: false,
            allow_system_table_mods: true,
            reloptions: None,
            relrewrite: InvalidOid,
            use_user_acl: false,
            is_internal: true,
        },
        &desc,
    )?;
    xact::CommandCounterIncrement()?;
    let base = pg_depend::ObjectAddress::set(RELATION_RELATION_ID, rel.rd_id);
    let tomb = pg_depend::ObjectAddress::set(RELATION_RELATION_ID, oid);
    pg_depend::recordDependencyOn(mcx, &tomb, &base, pg_depend::DependencyType::Internal)?;
    xact::CommandCounterIncrement()?;
    let trel = table::table_open(mcx, oid, AccessExclusiveLock)?;
    let mut attnums = [0 as AttrNumber; INDEX_MAX_KEYS as usize];
    attnums[0] = 1;
    let zeros = [0; INDEX_MAX_KEYS as usize];
    let mut info = execindexing::IndexInfo {
        ii_NumIndexAttrs: 1,
        ii_AmCache: None,
        ii_NumIndexKeyAttrs: 1,
        ii_IndexAttrNumbers: attnums,
        ii_Expressions: types_nodes::NodeList::nil(),
        ii_ExpressionsState: mcx::PgVec::new_in(mcx),
        ii_Predicate: types_nodes::NodeList::nil(),
        ii_PredicateState: None,
        ii_Unique: true,
        ii_NullsNotDistinct: false,
        ii_ReadyForInserts: true,
        ii_Summarizing: false,
        ii_Concurrent: false,
        ii_BrokenHotChain: false,
        ii_UniqueOps: zeros,
        ii_UniqueProcs: zeros,
        ii_UniqueStrats: [0; INDEX_MAX_KEYS as usize],
        ii_HasExclusion: false,
        ii_ExclusionOps: zeros,
        ii_ExclusionProcs: zeros,
        ii_ExclusionStrats: [0; INDEX_MAX_KEYS as usize],
        ii_WithoutOverlaps: false,
        ii_CheckedUnchanged: false,
        ii_IndexUnchanged: false,
        ii_ParallelWorkers: 0,
    };
    let out = catalog_index::index_create(
        mcx,
        &trel,
        format!("{name}_rowident").as_bytes(),
        InvalidOid,
        &mut info,
        &[b"rowident".as_slice()],
        catalog_index::BTREE_AM_OID,
        rel.rd_rel.reltablespace,
        &[InvalidOid],
        &[INT8_BTREE_OPS_OID],
        &[0],
        &catalog_index::IndexCreateExtra {
            flags: 0,
            constr_flags: 0,
            allow_system_table_mods: true,
            is_internal: true,
            parent_index_relid: InvalidOid,
            parent_constraint_id: InvalidOid,
            reloptions: None,
            opclass_options: None,
            stattargets: None,
            old_number: types_core::InvalidRelFileNumber,
        },
    );
    trel.close(NoLock)?;
    out?;
    xact::CommandCounterIncrement()?;
    Ok(oid)
}

fn tomb_index<'mcx>(mcx: Mcx<'mcx>, tomb: &Relation<'mcx>, lock: types_rel::LOCKMODE) -> PgResult<Relation<'mcx>> {
    match relcache_seams::relation_get_index_list::call(mcx, tomb.rd_id)?.first() {
        Some(&idx) => indexam::index_open(mcx, idx, lock),
        None => Err(Box::new(PgError::error(format!("pgrcolumnar: tombstone relation {} has no index", tomb.rd_id)).with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED))),
    }
}

/// The live tombstones for `rowident` other than `skip`; a decided one first, since a speculative one may still lose.
/// A tombstone outranks a LOCK row (a writer moving a LOCK row to its tombstone shows both under `SnapshotDirty`).
fn probe_skip<'mcx>(mcx: Mcx<'mcx>, tomb: &Relation<'mcx>, rowident: u64, snap: &Snapshot<'mcx>, skip: Option<&types_tuple::ItemPointerData>) -> PgResult<Option<TombHit>> {
    let key = int8_key(datum::Datum::from_i64(rowident as i64), 0)?;
    let mut found: Option<(u8, TombHit)> = None;
    walk(mcx, tomb, &key, snap, skip, &mut |_, rank, hit| {
        if found.as_ref().is_none_or(|(r, _)| rank < *r) {
            found = Some((rank, hit));
        }
        rank != 0
    })?;
    Ok(found.map(|(_, h)| h))
}

fn int8_key(arg: datum::Datum, flags: i32) -> PgResult<ScanKeyData> {
    let mut key = ScanKeyData::empty();
    key.sk_flags = flags;
    key.sk_attno = 1;
    key.sk_strategy = BTEqualStrategyNumber;
    key.sk_func = fmgr_seams::fmgr_info::call(F_INT8EQ)?;
    key.sk_argument = arg;
    Ok(key)
}

/// `each(rowident, rank, hit)` until it returns false. Rank 0: a decided tombstone (final); 1: a speculative one; 2: a LOCK row.
fn walk<'mcx>(
    mcx: Mcx<'mcx>,
    tomb: &Relation<'mcx>,
    key: &ScanKeyData,
    snap: &Snapshot<'mcx>,
    skip: Option<&types_tuple::ItemPointerData>,
    each: &mut dyn FnMut(u64, u8, TombHit) -> bool,
) -> PgResult<()> {
    let snap = snap.clone().ok_or_else(|| Box::new(PgError::error("pgrcolumnar: tombstone probe without a snapshot".to_string())))?;
    let idx = tomb_index(mcx, tomb, AccessShareLock)?;
    let mut slot = tableam::table_slot_create(mcx, tomb)?;
    let mut scan = indexam::index_beginscan(mcx, tomb, &idx, snap.clone(), 1, 0)?;
    indexam::index_rescan(&mut scan, Some(std::slice::from_ref(key)), None)?;
    let res = (|| -> PgResult<()> {
        while indexam::index_getnext_slot(mcx, &mut scan, types_scan::ScanDirection::ForwardScanDirection, &mut slot)? {
            let tup: &HeapTupleData<'_> = match &slot {
                SlotData::BufferHeap(b) => b.base.tuple.as_ref(),
                SlotData::Heap(h) => h.tuple.as_ref(),
                _ => None,
            }
            .ok_or_else(|| Box::new(PgError::error("pgrcolumnar: tombstone slot holds no tuple".to_string())))?;
            if skip.is_some_and(|s| types_tuple::ItemPointerEquals(s, &tup.t_self)) {
                continue;
            }
            let (mut v, mut n) = ([datum::Datum::null(); 2], [false; 2]);
            types_tuple::heap_deform_tuple(tup, &tomb.rd_att, &mut v, &mut n);
            let t = Tombstone::from_datums([v[0].as_i64(), v[1].as_i64()]).map_err(|e| Box::new(PgError::error(format!("pgrcolumnar: {e}")).with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED)))?;
            let hdr = tup.t_data();
            let xmin = hdr.xmin_raw();
            let spec_token = if hdr.is_speculative() { hdr.speculative_token() } else { 0 };
            let state = match xact::TransactionIdIsCurrentTransactionId(xmin) {
                true => TombState::Own,
                false if snap.dirty_xmin.get() != 0 => TombState::InProgress,
                false => TombState::Committed,
            };
            let own_unfrozen = t.is_lock() && state == TombState::Own && !hdr.xmin_frozen();
            let hit = TombHit { tid: tup.t_self, succ: t.succ, xmin, cmin: hdr.raw_command_id(), state, spec_token, xmax: snap.dirty_xmax.get(), own_unfrozen };
            let rank = match (t.is_lock(), spec_token) {
                (true, _) => 2,
                (false, 0) => 0,
                (false, _) => 1,
            };
            if !each(t.rowident, rank, hit) {
                break;
            }
        }
        Ok(())
    })();
    indexam::index_endscan(scan)?;
    exectuples::exec_clear_tuple(&mut slot, mcx);
    // a probe's index lock ends with the probe (a retired heap's reset takes AccessExclusiveLock on both)
    indexam::index_close(idx, AccessShareLock)?;
    res
}

pub fn tomb_probe<'mcx>(mcx: Mcx<'mcx>, tomb: &Relation<'mcx>, rowident: u64, snap: &Snapshot<'mcx>) -> PgResult<Option<TombHit>> {
    probe_skip(mcx, tomb, rowident, snap, None)
}

/// One `= ANY` key over all of `rowidents`, so btree reads each leaf once and descends again only across a
/// gap; each ID's hit is the one [`probe_skip`] picks, from the same tuples in the same order.
pub fn tomb_probe_many<'mcx>(mcx: Mcx<'mcx>, tomb: &Relation<'mcx>, rowidents: &[u64], snap: &Snapshot<'mcx>, out: &mut dyn FnMut(u64, TombHit)) -> PgResult<()> {
    let arr;
    let key = match rowidents {
        [] => return Ok(()),
        [one] => int8_key(datum::Datum::from_i64(*one as i64), 0)?,
        many => {
            debug_assert!(many.windows(2).all(|w| w[0] < w[1]), "tomb_probe_many: IDs not ascending and distinct");
            let mut elems: mcx::PgVec<'mcx, datum::Datum> = mcx::vec_with_capacity_in(mcx, many.len())?;
            elems.extend(many.iter().map(|&r| datum::Datum::from_i64(r as i64)));
            arr = datum::array_build::construct_array_image(mcx, &elems[..], INT8OID, 8, true, b'd')?;
            int8_key(datum::Datum::from_usize(arr.as_ptr() as usize), types_scan::scankey::SK_SEARCHARRAY)?
        }
    };
    let mut cur: Option<(u64, u8, TombHit)> = None;
    walk(mcx, tomb, &key, snap, None, &mut |r, rank, hit| {
        if cur.as_ref().is_some_and(|(c, _, _)| *c == r) {
            let (_, best, h) = cur.as_mut().expect("the same row's entry");
            if rank < *best {
                *best = rank;
                *h = hit;
            }
        } else if let Some((c, _, h)) = cur.replace((r, rank, hit)) {
            out(c, h);
        }
        true
    })?;
    if let Some((c, _, h)) = cur {
        out(c, h);
    }
    Ok(())
}

/// A won insert confirms only while `current` still says `tomb` takes new tombstones; otherwise it is
/// super-deleted and the caller retries on the heap that does.
pub fn tomb_insert<'mcx>(mcx: Mcx<'mcx>, tomb: &Relation<'mcx>, t: Tombstone, cid: CommandId, current: &mut dyn FnMut() -> PgResult<bool>) -> PgResult<TombInsert> {
    insert(mcx, tomb, t, cid, None, current)
}

/// The LOCK row `(rowident, SUCC_LOCK)`, frozen and locked in `mode` by this transaction once won.
pub fn tomb_insert_lock<'mcx>(mcx: Mcx<'mcx>, tomb: &Relation<'mcx>, rowident: u64, cid: CommandId, mode: LockTupleMode, current: &mut dyn FnMut() -> PgResult<bool>) -> PgResult<TombInsert> {
    insert(mcx, tomb, Tombstone { rowident, succ: pgrc_format::rowid::SUCC_LOCK }, cid, Some(mode), current)
}

/// The lock bits `heap_lock_tuple` gives a tuple with no xmax: infomask, infomask2.
fn lock_bits(mode: LockTupleMode) -> (u16, u16) {
    use types_tuple::{HEAP_KEYS_UPDATED, HEAP_XMAX_EXCL_LOCK, HEAP_XMAX_KEYSHR_LOCK, HEAP_XMAX_LOCK_ONLY, HEAP_XMAX_SHR_LOCK};
    match mode {
        LockTupleMode::LockTupleKeyShare => (HEAP_XMAX_LOCK_ONLY | HEAP_XMAX_KEYSHR_LOCK, 0),
        LockTupleMode::LockTupleShare => (HEAP_XMAX_LOCK_ONLY | HEAP_XMAX_SHR_LOCK, 0),
        LockTupleMode::LockTupleNoKeyExclusive => (HEAP_XMAX_LOCK_ONLY | HEAP_XMAX_EXCL_LOCK, 0),
        LockTupleMode::LockTupleExclusive => (HEAP_XMAX_LOCK_ONLY | HEAP_XMAX_EXCL_LOCK, HEAP_KEYS_UPDATED),
    }
}

fn insert<'mcx>(mcx: Mcx<'mcx>, tomb: &Relation<'mcx>, t: Tombstone, cid: CommandId, lock: Option<LockTupleMode>, current: &mut dyn FnMut() -> PgResult<bool>) -> PgResult<TombInsert> {
    let idx = tomb_index(mcx, tomb, RowExclusiveLock)?;
    let xid = xact::GetCurrentTransactionId()?;
    let token = lmgr::SpeculativeInsertionLockAcquire(xid)?;
    let values = t.datums().map(datum::Datum::from_i64);
    let mut tup = heaptuple::heap_form_tuple(mcx, &tomb.rd_att, &values, &[false; 2])?;
    tup.t_tableOid = tomb.rd_id;
    tup.t_data_mut().set_speculative_token(token);
    heapam::heap_insert(tomb, &mut tup, cid, heapam::hio::HEAP_INSERT_SPECULATIVE | heapam::hio::HEAP_INSERT_NO_LOGICAL, None)?;
    let tid = tup.t_self;
    mark(tomb, &tid);
    let unique = indexam::index_insert(mcx, &idx, &values[..1], &[false], &tid, tomb, types_nbtree::IndexUniqueCheck::UNIQUE_CHECK_PARTIAL, false, &mut None)?;
    let lost = match unique {
        true => None,
        false => {
            let dirty = Some(std::rc::Rc::new(types_snapshot::SnapshotData::sentinel(mcx, types_snapshot::SnapshotType::SNAPSHOT_DIRTY)));
            probe_skip(mcx, tomb, t.rowident, &dirty, Some(&tid))?.map(|h| h.xmin)
        }
    };
    // After the unique check, before the confirm. A switch that retired this heap committed before a writer on the
    // new heap probed this one, so either that probe saw this speculative tuple, or this check sees the switch.
    let out = match lost {
        None if current()? => TombInsert::Won(tid),
        None => TombInsert::Retry,
        Some(x) => TombInsert::Lost(x),
    };
    match out {
        TombInsert::Won(_) => {
            // frozen while still speculative; never before the win, since a super-deleted frozen tuple would stay
            // visible. An exclusive lock is the insert itself: an abort (or a rolled-back subtransaction) takes the
            // row with it. Weaker locks are shared, so they freeze.
            if let Some(mode) = lock.filter(|m| *m != LockTupleMode::LockTupleExclusive) {
                let (im, im2) = lock_bits(mode);
                pruneheap::heap_freeze_own_insert(tomb, &tid, xid, im, im2)?;
            }
            heapam::heap_finish_speculative(tomb, &tid)?
        }
        _ => heapam::heap_abort_speculative(tomb, &tid)?,
    }
    mark(tomb, &tid);
    lmgr::SpeculativeInsertionLockRelease(xid)?;
    indexam::index_close(idx, NoLock)?;
    Ok(out)
}

/// A heap no-key update (`rowident` is unchanged). Not HOT: the new version's index entry, checked unique (the old
/// version is this transaction's update, invisible to the check).
pub fn tomb_update<'mcx>(mcx: Mcx<'mcx>, tomb: &Relation<'mcx>, otid: types_tuple::ItemPointerData, t: Tombstone, cid: CommandId) -> PgResult<types_tuple::ItemPointerData> {
    let values = t.datums().map(datum::Datum::from_i64);
    let mut tup = heaptuple::heap_form_tuple(mcx, &tomb.rd_att, &values, &[false; 2])?;
    tup.t_tableOid = tomb.rd_id;
    let mut tmfd = tableam_vocab::TM_FailureData::default();
    let mut mode = LockTupleMode::LockTupleNoKeyExclusive;
    let mut indexes = tableam_vocab::TU_UpdateIndexes::TU_None;
    let res = heapam::heap_update(tomb, &otid, &mut tup, cid, None, true, &mut tmfd, &mut mode, &mut indexes)?;
    if res != tableam_vocab::TM_Result::TM_Ok {
        return Err(Box::new(PgError::error(format!("pgrcolumnar: update of a locked LOCK row of tombstone relation {} failed: {res:?}", tomb.rd_id))));
    }
    let tid = tup.t_self;
    mark(tomb, &otid);
    mark(tomb, &tid);
    if indexes != tableam_vocab::TU_UpdateIndexes::TU_None {
        let idx = tomb_index(mcx, tomb, RowExclusiveLock)?;
        let out = indexam::index_insert(mcx, &idx, &values[..1], &[false], &tid, tomb, types_nbtree::IndexUniqueCheck::UNIQUE_CHECK_YES, false, &mut None);
        indexam::index_close(idx, NoLock)?;
        out?;
    }
    Ok(tid)
}

/// This (sub)transaction's unfrozen LOCK row at `tid` becomes tombstone `t` in place (its rowident, and so its index
/// entry, unchanged): one XLOG_HEAP_INPLACE record.
pub fn tomb_overwrite<'mcx>(mcx: Mcx<'mcx>, tomb: &Relation<'mcx>, tid: types_tuple::ItemPointerData, t: Tombstone, cid: CommandId) -> PgResult<()> {
    let mut data = [0u8; 16];
    data[0..8].copy_from_slice(&(t.rowident as i64).to_ne_bytes());
    data[8..16].copy_from_slice(&(t.succ as i64).to_ne_bytes());
    heapam::inplace::heap_overwrite_own_insert(mcx, tomb, tid, &data, cid)?;
    mark(tomb, &tid);
    Ok(())
}

fn mark(rel: &Relation<'_>, tid: &types_tuple::ItemPointerData) {
    let l = rel.rd_locator.get();
    pgrc_marks::written((l.spcOid, l.dbOid, l.relNumber), types_tuple::itemptr::ItemPointerGetBlockNumberNoCheck(tid));
}

pub fn init_seams() {
    pgrc_front_seams::create_segment::set(create_segment);
    pgrc_front_seams::segment_relid::set(segment_relid);
    pgrc_front_seams::reset_segment::set(reset_segment);
    pgrc_front_seams::create_tombstone::set(create_tombstone);
    pgrc_front_seams::tombstone_relid::set(tombstone_relid);
    pgrc_front_seams::create_tomb::set(create_tomb);
    pgrc_front_seams::tomb_relid::set(tomb_relid);
    pgrc_front_seams::tomb_relids::set(tomb_relids);
    pgrc_front_seams::tomb_probe::set(tomb_probe);
    pgrc_front_seams::tomb_probe_many::set(tomb_probe_many);
    pgrc_front_seams::tomb_insert::set(tomb_insert);
    pgrc_front_seams::tomb_insert_lock::set(tomb_insert_lock);
    pgrc_front_seams::tomb_update::set(tomb_update);
    pgrc_front_seams::tomb_overwrite::set(tomb_overwrite);
    reorderbuffer_seams::pgrc_companion_of::set(kind_of);
}

#[cfg(test)]
mod tests {
    use super::*;
    use types_rel::{RELKIND_INDEX, RELKIND_TOASTVALUE};

    const PUBLIC: Oid = 2200;

    #[test]
    fn kind_of_names_both_companions_and_nothing_else() {
        let seg = segment_name(16384, 3);
        let tomb = tombstone_name(16384);
        assert_eq!(kind_of(PG_TOAST_NAMESPACE, seg.as_bytes(), RELKIND_RELATION), Some((16384, CompanionKind::Segment { slot: 3 })));
        assert_eq!(kind_of(PG_TOAST_NAMESPACE, tomb.as_bytes(), RELKIND_RELATION), Some((16384, CompanionKind::Tombstone)));
        assert_eq!(tomb_name(16384, 0), tomb);
        assert_eq!(tomb_name(16384, 1), "pgrc_tomb_16384_1");
        assert_eq!(kind_of(PG_TOAST_NAMESPACE, tomb_name(16384, 1).as_bytes(), RELKIND_RELATION), Some((16384, CompanionKind::Tombstone)));
        assert_eq!(kind_of(PUBLIC, tomb_name(16384, 1).as_bytes(), RELKIND_RELATION), None);
        assert_eq!(kind_of(PG_TOAST_NAMESPACE, segment_name(u32::MAX, u16::MAX).as_bytes(), RELKIND_RELATION), Some((u32::MAX, CompanionKind::Segment { slot: u16::MAX })));
        assert_eq!(kind_of(PUBLIC, seg.as_bytes(), RELKIND_RELATION), None);
        assert_eq!(kind_of(PG_TOAST_NAMESPACE, seg.as_bytes(), RELKIND_TOASTVALUE), None);
        assert_eq!(kind_of(PG_TOAST_NAMESPACE, tomb.as_bytes(), RELKIND_INDEX), None);
        assert_eq!(kind_of(PG_TOAST_NAMESPACE, b"pg_toast_16384", RELKIND_TOASTVALUE), None);
        for bad in [
            &b"pgrc_seg_16384"[..], b"pgrc_seg_16384_", b"pgrc_seg__3", b"pgrc_seg_16384_3_1", b"pgrc_seg_16384_65536",
            b"pgrc_seg_016384_3", b"pgrc_seg_16384_+3", b"pgrc_seg_4294967296_0", b"pgrc_tomb_", b"pgrc_tomb_x1",
            b"pgrc_tomb_16384_0", b"pgrc_tomb_16384_2", b"pgrc_tomb_16384_01", b"pgrc_tomb_16384_", b"pgrc_tomb__1",
            b"pgrc_tomb_16384_1_1", b"pgrc_tomb_ 1", b"pgrc_segx_1_2",
        ] {
            assert_eq!(kind_of(PG_TOAST_NAMESPACE, bad, RELKIND_RELATION), None, "{}", String::from_utf8_lossy(bad));
        }
    }
}

use relcache::schemapg::CLASS_OID_INDEX_ID;
use relcache_build_seams::ScannedPgClass;
use types_core::{InvalidOid, Oid, RELATION_RELATION_ID};
use types_error::{PgError, PgResult, FATAL};
use types_rel::{AccessShareLock, FormData_pg_class};
use types_tuple::{HeapTupleData, TupleDescData};

use crate::{getattr, name_from, oid_key, CatalogRow};

const Anum_pg_class_oid: i32 = 1;
const Anum_pg_class_reloptions: i32 = 33;
const CLASS_FIXED_NATTS: i32 = 31;

#[track_caller]
#[cold]
#[inline(never)]
fn no_database() -> Box<PgError> {
    Box::new(PgError::new(
        FATAL,
        "cannot read pg_class without having selected a database",
    ))
}

// ScanPgRelation (relcache.c).
pub(crate) fn scan_pg_relation(
    mcx: mcx::Mcx<'_>,
    target_rel_id: Oid,
    index_ok: bool,
    force_non_historic: bool,
) -> PgResult<Option<ScannedPgClass>> {
    if init_small::globals::MyDatabaseId() == InvalidOid {
        return Err(no_database());
    }
    let snapshot = if force_non_historic {
        snapmgr::RegisterSnapshot(Some(&snapmgr::GetNonHistoricCatalogSnapshot(
            RELATION_RELATION_ID,
        )?))?
    } else {
        None
    };
    let rel = table::table_open(mcx, RELATION_RELATION_ID, AccessShareLock)?;
    let keys = [oid_key(Anum_pg_class_oid, target_rel_id)];
    let mut scan = genam::systable_beginscan(
        mcx,
        &rel,
        CLASS_OID_INDEX_ID,
        index_ok && relcache::criticalRelcachesBuilt(),
        snapshot.clone(),
        &keys,
    )?;
    let decoded = match genam::systable_getnext(mcx, &mut scan)? {
        Some(tup) => Some(decode(mcx, rel.descr(), tup, target_rel_id)?),
        None => None,
    };
    genam::systable_endscan(mcx, scan)?;
    rel.close(AccessShareLock)?;
    snapmgr::UnregisterSnapshot(snapshot.as_ref());
    Ok(decoded)
}

pub(crate) fn decode(
    scratch: mcx::Mcx<'_>,
    td: &TupleDescData<'_>,
    tup: &HeapTupleData<'_>,
    relid: Oid,
) -> PgResult<ScannedPgClass> {
    let row = CatalogRow::new(td, tup, CLASS_FIXED_NATTS);
    let form = FormData_pg_class {
        relname: name_from(tup, row.req(2)?),
        relnamespace: row.req(3)?.as_oid(),
        reltype: row.req(4)?.as_oid(),
        relowner: row.req(6)?.as_oid(),
        relam: row.req(7)?.as_oid(),
        relfilenode: row.req(8)?.as_oid(),
        reltablespace: row.req(9)?.as_oid(),
        relpages: row.req(10)?.as_i32(),
        reltuples: row.req(11)?.as_f32(),
        relallvisible: row.req(12)?.as_i32(),
        reltoastrelid: row.req(14)?.as_oid(),
        relhasindex: row.req(15)?.as_bool(),
        relisshared: row.req(16)?.as_bool(),
        relpersistence: row.req(17)?.as_u8(),
        relkind: row.req(18)?.as_u8(),
        relhassubclass: row.req(23)?.as_bool(),
        relrowsecurity: row.req(24)?.as_bool(),
        relispopulated: row.req(26)?.as_bool(),
        relreplident: row.req(27)?.as_u8(),
        relispartition: row.req(28)?.as_bool(),
        relfrozenxid: row.req(30)?.as_transaction_id(),
        relminmxid: row.req(31)?.as_transaction_id(),
    };
    let _ = relid;
    let (opts_datum, opts_null) = getattr(td, tup, Anum_pg_class_reloptions);
    let options = reloptions::extractRelOptions(
        scratch,
        form.relkind,
        form.relam,
        if opts_null { None } else { Some(opts_datum) },
    )?;
    let relnatts = row.req(19)?.as_i16();
    let relchecks = row.req(20)?.as_i16();
    let relhasrules = row.req(21)?.as_bool();
    let relhastriggers = row.req(22)?.as_bool();
    let relrewrite = row.req(29)?.as_oid();
    Ok(ScannedPgClass { form, relchecks, relnatts, relhastriggers, relhasrules, relrewrite, options })
}

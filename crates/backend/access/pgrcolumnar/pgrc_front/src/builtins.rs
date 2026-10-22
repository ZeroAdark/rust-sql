
use datum::Datum;
use mcx::Mcx;
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_fmgr::{FmgrBuiltin, FmgrInfo, FunctionCallInfoBaseData as Fcinfo};
use pgrc_format::dml::Tombstone;
use pgrc_format::manifest::SegState;
use pgrc_front_seams::TombInsert;
use tableam_vocab::VacuumParams;
use types_rel::{AccessShareLock, NoLock, Relation, RelationData, ShareUpdateExclusiveLock};
use types_storage::buf::BufferAccessStrategy;

use crate::{compile, maint, reclaim, ring, rotate, seal};

pub const PGRC_FLUSH_FOID: Oid = 9023;

pub const PGRC_SEGMENTS_FOID: Oid = 9024;
pub const PGRC_SEGMENT_ROWS_FOID: Oid = 9025;
pub const PGRC_TOMB_INSERT_FOID: Oid = 9026;
pub const PGRC_TOMB_PROBE_FOID: Oid = 9027;
pub const PGRC_ROWIDENT_FOID: Oid = 9028;
pub const PGRC_FETCH_FOID: Oid = 9029;
pub const PGRC_COMPACT_FOID: Oid = 9030;
pub const PGRC_COMPACT_STATS_FOID: Oid = 9031;
pub const PGRC_TOMBSTONES_FOID: Oid = 9039;
pub const PGRC_VIEWS_FOID: Oid = 9040;
pub const PGRC_ROWID_STATS_FOID: Oid = 9041;
pub const PGRC_ROWID_RESERVE_FOID: Oid = 9042;
pub const PGRC_SCAN_STATS_FOID: Oid = 9043;
pub const PGRC_IXFETCH_STATS_FOID: Oid = 9044;
pub const PGRC_FORGET_ROW_IDS_FOID: Oid = 9045;
pub const PGRC_ASSIGN_ROW_IDS_FOID: Oid = 9046;

pub const FRONT_BUILTINS: &[FmgrBuiltin] = &[
    FmgrBuiltin { foid: PGRC_FLUSH_FOID, name: "pgrc_flush", nargs: 1, strict: true, retset: false, func: fc_pgrc_flush },
    FmgrBuiltin { foid: PGRC_SEGMENTS_FOID, name: "pgrc_segments", nargs: 1, strict: true, retset: false, func: fc_pgrc_segments },
    FmgrBuiltin { foid: PGRC_SEGMENT_ROWS_FOID, name: "pgrc_segment_rows", nargs: 1, strict: true, retset: false, func: fc_pgrc_segment_rows },
    FmgrBuiltin { foid: PGRC_TOMB_INSERT_FOID, name: "pgrc_tomb_insert", nargs: 3, strict: true, retset: false, func: fc_pgrc_tomb_insert },
    FmgrBuiltin { foid: PGRC_TOMB_PROBE_FOID, name: "pgrc_tomb_probe", nargs: 3, strict: true, retset: false, func: fc_pgrc_tomb_probe },
    FmgrBuiltin { foid: PGRC_ROWIDENT_FOID, name: "pgrc_rowident", nargs: 2, strict: true, retset: false, func: fc_pgrc_rowident },
    FmgrBuiltin { foid: PGRC_FETCH_FOID, name: "pgrc_fetch", nargs: 2, strict: true, retset: false, func: fc_pgrc_fetch },
    FmgrBuiltin { foid: PGRC_COMPACT_FOID, name: "pgrc_compact", nargs: 2, strict: true, retset: false, func: fc_pgrc_compact },
    FmgrBuiltin { foid: PGRC_COMPACT_STATS_FOID, name: "pgrc_compact_stats", nargs: 1, strict: true, retset: false, func: fc_pgrc_compact_stats },
    FmgrBuiltin { foid: PGRC_TOMBSTONES_FOID, name: "pgrc_tombstones", nargs: 1, strict: true, retset: false, func: fc_pgrc_tombstones },
    FmgrBuiltin { foid: PGRC_VIEWS_FOID, name: "pgrc_views", nargs: 1, strict: true, retset: false, func: fc_pgrc_views },
    FmgrBuiltin { foid: PGRC_ROWID_STATS_FOID, name: "pgrc_rowid_stats", nargs: 1, strict: true, retset: false, func: fc_pgrc_rowid_stats },
    FmgrBuiltin { foid: PGRC_ROWID_RESERVE_FOID, name: "pgrc_rowid_reserve", nargs: 2, strict: true, retset: false, func: fc_pgrc_rowid_reserve },
    FmgrBuiltin { foid: PGRC_SCAN_STATS_FOID, name: "pgrc_scan_stats", nargs: 1, strict: true, retset: false, func: fc_pgrc_scan_stats },
    FmgrBuiltin { foid: PGRC_IXFETCH_STATS_FOID, name: "pgrc_ixfetch_stats", nargs: 0, strict: false, retset: false, func: fc_pgrc_ixfetch_stats },
    FmgrBuiltin { foid: PGRC_FORGET_ROW_IDS_FOID, name: "pgrc_forget_row_ids", nargs: 1, strict: true, retset: false, func: fc_pgrc_forget_row_ids },
    FmgrBuiltin { foid: PGRC_ASSIGN_ROW_IDS_FOID, name: "pgrc_assign_row_ids", nargs: 1, strict: true, retset: false, func: fc_pgrc_assign_row_ids },
];

pub fn fc_pgrc_ixfetch_stats(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let mcx = fcinfo.result_mcx();
    Ok(types_fmgr::varlena_result(varlena::cstring_to_text(mcx, crate::ixfetch::stats_text().as_bytes())?))
}

pub(crate) fn flush_locked<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<u32> {
    rotate::rotate(mcx, rel, 1)?;
    let (sealed, _) = seal::seal_all(mcx, rel)?;
    compile::compile(mcx, rel, true)?;
    reclaim::reclaim(mcx, rel)?;
    reclaim::settle(rel)?;
    Ok(sealed)
}

pub fn vacuum<'mcx>(mcx: Mcx<'mcx>, parent: &RelationData<'mcx>, params: &VacuumParams, bstrategy: BufferAccessStrategy) -> PgResult<()> {
    vacuum_heaps(mcx, parent, params, bstrategy)
}

/// Every row committed before the VACUUM started is sealed, or the step errors (lock_timeout,
/// statement_timeout, a deadlock): the VACUUM never returns success with them left in the front.
pub fn vacuum_flush_step<'mcx>(mcx: Mcx<'mcx>, parent: &RelationData<'mcx>, st: &mut tableam_seams::PgrcVacFlush) -> PgResult<bool> {
    if procarray::MyProcIsInLazyVacuum() {
        return Err(Box::new(PgError::error("pgrcolumnar: a VACUUM flush step ran with PROC_IN_VACUUM set")));
    }
    let rel = table::table_open(mcx, parent.rd_id, NoLock)?;
    let out = (|| {
        if !crate::is_pgrc_table(&rel) {
            return Ok(false);
        }
        pgrc_am::ingest::defer_if_busy(&rel)?;
        maint::lock(rel.rd_id)?;
        match st.phase {
            0 => {
                rotate::rotate(mcx, &rel, 1)?;
                st.targets = ring::newest(&rel)?
                    .map(|g| ring::records(&g).iter().filter(|r| r.state == SegState::Sealing).map(|r| (r.relid, r.part_no)).collect())
                    .unwrap_or_default();
                injection_point::injection_point("pgrc-vacuum-flushed")?;
                st.phase = 1;
                Ok(true)
            }
            1 => {
                match seal::seal_one_waiting(mcx, &rel, &st.targets)? {
                    true => st.sealed += 1,
                    false => st.phase = 2,
                }
                Ok(true)
            }
            _ => {
                compile::compile(mcx, &rel, true)?;
                reclaim::reclaim(mcx, &rel)?;
                reclaim::settle(&rel)?;
                Ok(false)
            }
        }
    })();
    rel.close(NoLock)?;
    out
}

/// No xid and no catalog update, so it runs the same with `PROC_IN_VACUUM` set and in a cluster past its xid stop limit
/// (the emergency VACUUM).
pub fn freeze<'mcx>(mcx: Mcx<'mcx>, parent: &RelationData<'mcx>, oldest_xmin: types_core::TransactionId) -> PgResult<tableam_seams::PgrcPassFacts> {
    let rel = table::table_open(mcx, parent.rd_id, NoLock)?;
    let out = pgrc_am::vacuum::freeze(&rel, oldest_xmin).and_then(|relfrozenxid_cap| {
        // advisory: a table whose facts cannot be read keeps the pass's figures, and its VACUUM (the freeze) completes
        let relstats = match pgrc_am::footer::footer_size(&rel) {
            Ok(size) => size.map(|(rows, bytes)| (bytes.div_ceil(types_core::BLCKSZ as u64) as types_core::BlockNumber, rows as f64)),
            // P304: a table whose storage is gone (its directory removed: 58P01) is the error, as C's VACUUM of a
            // relation whose file is missing
            Err(e) if e.sqlstate() == types_error::ERRCODE_UNDEFINED_FILE => return Err(e),
            Err(e) => {
                elog::elog(types_error::WARNING, format!("pgrcolumnar: VACUUM of \"{}\" leaves its size out of pg_class: {}", rel.name(), e.message()))?;
                None
            }
        };
        Ok(tableam_seams::PgrcPassFacts { relfrozenxid_cap, relstats })
    });
    rel.close(NoLock)?;
    out
}

fn vacuum_heaps<'mcx>(mcx: Mcx<'mcx>, parent: &RelationData<'mcx>, params: &VacuumParams, bstrategy: BufferAccessStrategy) -> PgResult<()> {
    let rel = table::table_open(mcx, parent.rd_id, NoLock)?;
    let out = compile::vacuum(mcx, &rel, params, bstrategy.clone()).and_then(|_| ring::newest(&rel));
    rel.close(NoLock)?;
    let Some(g) = out? else { return Ok(()) };
    for r in g.segments.iter().filter(|r| r.state != SegState::Sealed) {
        if !lmgr::ConditionalLockRelationOid(r.relid, ShareUpdateExclusiveLock)? {
            continue;
        }
        let seg = table::table_open(mcx, r.relid, NoLock)?;
        let res = tableam_seams::table_relation_vacuum::call(mcx, &seg, params, bstrategy.clone());
        seg.close(NoLock)?;
        res?;
    }
    Ok(())
}

pub fn fc_pgrc_flush(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let mcx = fcinfo.result_mcx();
    let rel = table::table_open(mcx, fcinfo.arg_oid(0), AccessShareLock)?;
    let out = match crate::is_pgrc_table(&rel) {
        true => maint::lock(rel.rd_id).and_then(|()| flush_locked(mcx, &rel)),
        false => Err(Box::new(PgError::error(format!("pgrc_flush: \"{}\" is not a pgrcolumnar table", rel.name())))),
    };
    rel.close(NoLock)?;
    Ok(Datum::from_i32(out? as i32))
}

pub fn fc_pgrc_segments(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    segment_count(fcinfo, "pgrc_segments", ring::unsealed_blocks)
}

pub fn fc_pgrc_views(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    segment_count(fcinfo, "pgrc_views", |_, rel| {
        let l = rel.rd_locator.get();
        Ok(pgrc_am::catalog::live_views((l.spcOid, l.dbOid, l.relNumber as u64)) as u64)
    })
}

pub fn fc_pgrc_segment_rows(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    segment_count(fcinfo, "pgrc_segment_rows", ring::unsealed_rows)
}

fn segment_count(fcinfo: &mut Fcinfo, name: &str, count: for<'m> fn(Mcx<'m>, &Relation<'m>) -> PgResult<u64>) -> PgResult<Datum> {
    let mcx = fcinfo.result_mcx();
    let rel = table::table_open(mcx, fcinfo.arg_oid(0), AccessShareLock)?;
    let out = match crate::is_pgrc_table(&rel) {
        true => count(mcx, &rel),
        false => Err(Box::new(PgError::error(format!("{name}: \"{}\" is not a pgrcolumnar table", rel.name())))),
    };
    rel.close(NoLock)?;
    Ok(Datum::from_i64(out? as i64))
}

pub fn fc_pgrc_tomb_insert(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let t = Tombstone { rowident: fcinfo.arg_i64(1) as u64, succ: fcinfo.arg_i64(2) as u64 };
    let won = with_tombstones(fcinfo, "pgrc_tomb_insert", &mut |_mcx, tombs| {
        let cid = xact_seams::get_current_command_id::call(true)?;
        Ok(matches!(tombs.insert(t, cid)?, TombInsert::Won(_)).then_some(1))
    })?;
    Ok(Datum::from_bool(won.is_some()))
}

pub fn fc_pgrc_tomb_probe(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let (r, dirty) = (fcinfo.arg_i64(1) as u64, fcinfo.arg_bool(2));
    let succ = with_tombstones(fcinfo, "pgrc_tomb_probe", &mut |mcx, tombs| {
        let snap = match dirty {
            true => Some(std::rc::Rc::new(types_snapshot::SnapshotData::sentinel(mcx, types_snapshot::SnapshotType::SNAPSHOT_DIRTY))),
            false => Some(snapmgr::GetActiveSnapshot()),
        };
        for tomb in tombs.heaps() {
            if let Some(h) = pgrc_front_seams::tomb_probe::call(mcx, tomb, r, &snap)? {
                return Ok(Some(h.succ as i64));
            }
        }
        Ok(None)
    })?;
    fcinfo.isnull = succ.is_none();
    Ok(Datum::from_i64(succ.unwrap_or(0)))
}

pub fn fc_pgrc_tombstones(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    if !superuser_seams::superuser::call()? {
        return Err(Box::new(PgError::error("pgrc_tombstones: must be superuser".to_string()).with_sqlstate(types_error::ERRCODE_INSUFFICIENT_PRIVILEGE)));
    }
    let mcx = fcinfo.result_mcx();
    let rel = table::table_open(mcx, fcinfo.arg_oid(0), AccessShareLock)?;
    let out = (|| -> PgResult<i64> {
        if !crate::is_pgrc_table(&rel) {
            return Err(Box::new(PgError::error(format!("pgrc_tombstones: \"{}\" is not a pgrcolumnar table", rel.name()))));
        }
        let heaps = crate::tombs::open_read(mcx, rel.rd_id, &crate::tombs::probe_slots(&crate::tombs::newest_state(&rel)?))?;
        let snap = Some(snapmgr::GetActiveSnapshot());
        let mut n = 0i64;
        let res = heaps.iter().try_for_each(|h| {
            let mut hs = heapam::heap_beginscan(mcx, &h.rel, snap.clone(), 0, mcx::PgVec::new_in(mcx), None, tableam_vocab::SO_TYPE_SEQSCAN | tableam_vocab::SO_ALLOW_PAGEMODE)?;
            let res = (|| -> PgResult<()> {
                while let Some(tup) = heapam::heap_getnext(&mut hs, types_scan::sdir::ScanDirection::ForwardScanDirection)? {
                    let (mut v, mut nulls) = ([datum::Datum::null(); 2], [false; 2]);
                    types_tuple::heap_deform_tuple(tup, &h.rel.rd_att, &mut v, &mut nulls);
                    n += i64::from(v[1].as_i64() as u64 != pgrc_format::rowid::SUCC_LOCK);
                }
                Ok(())
            })();
            heapam::heap_endscan(hs)?;
            res
        });
        crate::tombs::close_all(heaps)?;
        res.map(|()| n)
    })();
    rel.close(NoLock)?;
    Ok(Datum::from_i64(out?))
}

fn with_tombstones(fcinfo: &mut Fcinfo, name: &str, f: &mut dyn for<'a, 'm> FnMut(Mcx<'m>, &mut crate::tombs::Writer<'a, 'm>) -> PgResult<Option<i64>>) -> PgResult<Option<i64>> {
    if !superuser_seams::superuser::call()? {
        return Err(Box::new(PgError::error(format!("{name}: must be superuser")).with_sqlstate(types_error::ERRCODE_INSUFFICIENT_PRIVILEGE)));
    }
    let mcx = fcinfo.result_mcx();
    let rel = table::table_open(mcx, fcinfo.arg_oid(0), AccessShareLock)?;
    let out = match crate::is_pgrc_table(&rel) {
        true => crate::tombs::Writer::open(mcx, &rel).and_then(|mut t| {
            let out = f(mcx, &mut t);
            t.close()?;
            out
        }),
        false => Err(Box::new(PgError::error(format!("{name}: \"{}\" is not a pgrcolumnar table", rel.name())))),
    };
    rel.close(NoLock)?;
    out
}

fn arg_tid(fcinfo: &Fcinfo, n: usize) -> types_tuple::ItemPointerData {
    // SAFETY: the caller's argument n is a tid.
    let t = unsafe { *fcinfo.arg_tid(n) };
    let half = |i: usize| u16::from_ne_bytes([t[i], t[i + 1]]);
    let mut tid = types_tuple::ItemPointerData::default();
    types_tuple::ItemPointerSet(&mut tid, (half(0) as u32) << 16 | half(2) as u32, half(4));
    tid
}

pub fn fc_pgrc_fetch(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let tid = arg_tid(fcinfo, 1);
    let mcx = fcinfo.result_mcx();
    let rel = table::table_open(mcx, fcinfo.arg_oid(0), AccessShareLock)?;
    let mut slot = exectuples::make_tuple_table_slot(mcx, types_slot::TupleSlotKind::Virtual, Some(rel.rd_att.clone()));
    let out = crate::fetch::fetch_row_version(mcx, &rel, &tid, &Some(snapmgr::GetActiveSnapshot()), &mut slot);
    exectuples::exec_clear_tuple(&mut slot, mcx);
    rel.close(NoLock)?;
    Ok(Datum::from_bool(out?))
}

pub fn fc_pgrc_rowident(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let tid = arg_tid(fcinfo, 1);
    let mcx = fcinfo.result_mcx();
    let rel = table::table_open(mcx, fcinfo.arg_oid(0), AccessShareLock)?;
    let out = crate::dml::rowident_of(mcx, &rel, &tid);
    rel.close(NoLock)?;
    Ok(Datum::from_i64(out? as i64))
}

/// Every kind but `run`, `runpass` and `member` takes VACUUM's privilege rule ([`compact_permitted`]) before the
/// maintenance lock.
pub fn fc_pgrc_compact(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    // SAFETY: argument 1 is a non-null text (strict).
    let kind = String::from_utf8_lossy(unsafe { fcinfo.arg_varlena_packed(1)? }.data()).into_owned();
    if (kind == "run" || kind == "runpass" || kind == "member") && !superuser_seams::superuser::call()? {
        return Err(Box::new(PgError::error(format!("pgrc_compact: kind \"{kind}\": must be superuser")).with_sqlstate(types_error::ERRCODE_INSUFFICIENT_PRIVILEGE)));
    }
    let force = match kind.as_str() {
        "auto" | "run" | "runpass" | "member" => None,
        "concat" => Some(crate::compact::Force::Concat),
        "sorted" => Some(crate::compact::Force::Sorted),
        "dv" => Some(crate::compact::Force::Dv),
        k => return Err(Box::new(PgError::error(format!("pgrc_compact: unknown kind \"{k}\" (auto, concat, sorted, dv, run, runpass, member)")).with_sqlstate(types_error::ERRCODE_INVALID_PARAMETER_VALUE))),
    };
    let mcx = fcinfo.result_mcx();
    let rel = table::table_open(mcx, fcinfo.arg_oid(0), AccessShareLock)?;
    let out = match crate::is_pgrc_table(&rel) {
        true if kind == "run" => maint::lock_maint(rel.rd_id).and_then(|()| crate::compact::run_all(mcx, &rel)).map(|o| o.and_then(|o| o.members.first().copied()).map_or(0, |n| n as i32)),
        true if kind == "runpass" => maint::lock_maint(rel.rd_id).and_then(|()| crate::compact::run_pass(mcx, &rel, false)).map(|o| o.and_then(|r| r.out.members.first().copied()).map_or(0, |n| n as i32)),
        true if kind == "member" => maint::lock_maint(rel.rd_id).and_then(|()| crate::compact::member_pass_forced(mcx, &rel)).map(|o| o.and_then(|m| m.to.or(m.retired.first().copied())).map_or(0, |n| n as i32)),
        true => compact_permitted(rel.rd_id, &rel.name()).and_then(|()| maint::lock_maint(rel.rd_id)).and_then(|()| crate::compact::run_one(mcx, &rel, force)).map(|o| o.and_then(|(_, o)| o.part_no).map_or(0, |n| n as i32)),
        false => Err(Box::new(PgError::error(format!("pgrc_compact: \"{}\" is not a pgrcolumnar table", rel.name())))),
    };
    rel.close(NoLock)?;
    Ok(Datum::from_i32(out?))
}

/// `pgrc_compact`'s privilege rule, VACUUM's (`vacuum_is_permitted_for_relation`): the database's owner (a pgrc table
/// is never shared), or a role with MAINTAIN on the table.
fn compact_permitted(relid: Oid, relname: &str) -> PgResult<()> {
    let roleid = miscinit_seams::get_user_id::call();
    if aclchk_seams::object_ownercheck::call(types_core::catalog::DATABASE_RELATION_ID, init_small::globals::MyDatabaseId(), roleid)?
        || aclchk_seams::pg_class_aclcheck_ext::call(relid, roleid, types_nodes::parsenodes::ACL_MAINTAIN)?.0 == 0
    {
        return Ok(());
    }
    Err(Box::new(PgError::error(format!("permission denied to compact \"{relname}\"")).with_sqlstate(types_error::ERRCODE_INSUFFICIENT_PRIVILEGE)))
}

pub fn fc_pgrc_rowid_stats(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let mcx = fcinfo.result_mcx();
    let rel = table::table_open(mcx, fcinfo.arg_oid(0), AccessShareLock)?;
    let out = match crate::is_pgrc_table(&rel) {
        true => pgrc_am::rowid_alloc::marks(&rel).and_then(|(next, high)| {
            let dir = pgrc_am::rowid_alloc::dir_of(&rel);
            let e = pgrc_am::rowid_alloc::extents(&dir)?;
            let maps = pgrc_am::mapcache::stats_text(ring::newest(&rel)?.as_deref(), &dir);
            let (hits, misses) = crate::pathcache::view_counts();
            Ok(format!("{} {} {maps} view_hits {hits} view_misses {misses}", pgrc_am::rowid_alloc::stats_text(next, high, &e), pgrc_am::rowid_alloc::renumber_text(&rel)))
        }),
        false => Err(Box::new(PgError::error(format!("pgrc_rowid_stats: \"{}\" is not a pgrcolumnar table", rel.name())))),
    };
    rel.close(NoLock)?;
    Ok(types_fmgr::varlena_result(varlena::cstring_to_text(mcx, out?.as_bytes())?))
}

pub fn fc_pgrc_rowid_reserve(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    if !superuser_seams::superuser::call()? {
        return Err(Box::new(PgError::error("pgrc_rowid_reserve: must be superuser".to_string()).with_sqlstate(types_error::ERRCODE_INSUFFICIENT_PRIVILEGE)));
    }
    let n = fcinfo.arg_i64(1);
    if n <= 0 || n as u64 > pgrc_format::rowid::ROWID_LIMIT {
        return Err(Box::new(PgError::error(format!("pgrc_rowid_reserve: {n} is not between 1 and 2^40")).with_sqlstate(types_error::ERRCODE_INVALID_PARAMETER_VALUE)));
    }
    let mcx = fcinfo.result_mcx();
    let rel = table::table_open(mcx, fcinfo.arg_oid(0), types_rel::RowExclusiveLock)?;
    let out = match crate::is_pgrc_table(&rel) {
        true => pgrc_am::rowid_alloc::reserve(&rel, n as u64),
        false => Err(Box::new(PgError::error(format!("pgrc_rowid_reserve: \"{}\" is not a pgrcolumnar table", rel.name())).with_sqlstate(types_error::ERRCODE_WRONG_OBJECT_TYPE))),
    };
    rel.close(NoLock)?;
    Ok(Datum::from_i64(out?.lo as i64))
}

/// D4826 (a test hook): each part of the table whose rows have one range of IDs of their own (a bulk part's implicit
/// map) loses it, as a part written before row IDs (IX0) has none; a metadata-only publish, the part files untouched.
/// Superuser, under AccessExclusiveLock, on a table with no index (its entries name the IDs). Returns the parts changed.
pub fn fc_pgrc_forget_row_ids(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    if !superuser_seams::superuser::call()? {
        return Err(Box::new(PgError::error("pgrc_forget_row_ids: must be superuser".to_string()).with_sqlstate(types_error::ERRCODE_INSUFFICIENT_PRIVILEGE)));
    }
    let mcx = fcinfo.result_mcx();
    let rel = table::table_open(mcx, fcinfo.arg_oid(0), types_rel::AccessExclusiveLock)?;
    let out = match crate::is_pgrc_table(&rel) {
        true if rel.rd_rel.relhasindex => Err(Box::new(PgError::error(format!("pgrc_forget_row_ids: \"{}\" has an index", rel.name())).with_sqlstate(types_error::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE))),
        true => forget_row_ids(&rel),
        false => Err(Box::new(PgError::error(format!("pgrc_forget_row_ids: \"{}\" is not a pgrcolumnar table", rel.name())).with_sqlstate(types_error::ERRCODE_WRONG_OBJECT_TYPE))),
    };
    rel.close(NoLock)?;
    Ok(Datum::from_i32(out? as i32))
}

fn forget_row_ids(rel: &Relation<'_>) -> PgResult<u32> {
    let Some(gen) = ring::newest(rel)? else { return Ok(0) };
    let own = |i: usize| gen.ids_of(i).len() == 1 && gen.manifest.parts[i].flags & pgrc_format::manifest::PART_FLAG_ROWMAP == 0 && gen.run_member(i).is_none();
    let parts: Vec<u32> = (0..gen.manifest.parts.len()).filter(|&i| own(i)).map(|i| gen.manifest.parts[i].part_no).collect();
    if parts.is_empty() {
        return Ok(0);
    }
    let mut edit = |_: Option<&pgrc_format::manifest::Manifest>, b: &mut pgrc_write::plan::PlanBuilder<'_>| -> pgrc_write::WriteResult<()> {
        parts.iter().for_each(|&p| b.set_ranges(p, Vec::new()));
        Ok(())
    };
    pgrc_am::ingest::publish_ext(rel, &mut edit)?;
    Ok(parts.len() as u32)
}

/// D4826 (d): each part of the table written before row IDs (IX0: no range and no map, not a run member) gets a range of
/// fresh IDs of its own (`rowid_alloc::reserve`, durable, block-aligned), its implicit map (row i is ID lo + i): a
/// metadata-only publish, the part files untouched (VACUUM FULL renumbers by rewriting each part). Superuser, under
/// AccessExclusiveLock; an index of the table is the caller's to REINDEX. Returns the parts given IDs.
pub fn fc_pgrc_assign_row_ids(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    if !superuser_seams::superuser::call()? {
        return Err(Box::new(PgError::error("pgrc_assign_row_ids: must be superuser".to_string()).with_sqlstate(types_error::ERRCODE_INSUFFICIENT_PRIVILEGE)));
    }
    let mcx = fcinfo.result_mcx();
    let rel = table::table_open(mcx, fcinfo.arg_oid(0), types_rel::AccessExclusiveLock)?;
    let out = match crate::is_pgrc_table(&rel) {
        true => assign_row_ids(&rel),
        false => Err(Box::new(PgError::error(format!("pgrc_assign_row_ids: \"{}\" is not a pgrcolumnar table", rel.name())).with_sqlstate(types_error::ERRCODE_WRONG_OBJECT_TYPE))),
    };
    rel.close(NoLock)?;
    Ok(Datum::from_i32(out? as i32))
}

fn assign_row_ids(rel: &Relation<'_>) -> PgResult<u32> {
    let Some(gen) = ring::newest(rel)? else { return Ok(0) };
    let idless = |i: usize| gen.manifest.parts[i].rows > 0 && gen.ids_of(i).is_empty() && gen.manifest.parts[i].flags & pgrc_format::manifest::PART_FLAG_ROWMAP == 0 && gen.run_member(i).is_none();
    let mut ranges: Vec<(u32, pgrc_format::manifest::IdRanges)> = Vec::new();
    // bounded("other: the parts of one manifest, one durable reserve each")
    for i in (0..gen.manifest.parts.len()).filter(|&i| idless(i)) {
        let r = pgrc_am::rowid_alloc::reserve(rel, gen.manifest.parts[i].rows)?;
        ranges.push((gen.manifest.parts[i].part_no, vec![(r.lo, r.hi)]));
    }
    if ranges.is_empty() {
        return Ok(0);
    }
    let mut edit = |_: Option<&pgrc_format::manifest::Manifest>, b: &mut pgrc_write::plan::PlanBuilder<'_>| -> pgrc_write::WriteResult<()> {
        ranges.iter().for_each(|(p, r)| b.set_ranges(*p, r.clone()));
        Ok(())
    };
    pgrc_am::ingest::publish_ext(rel, &mut edit)?;
    Ok(ranges.len() as u32)
}

pub fn fc_pgrc_compact_stats(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let mcx = fcinfo.result_mcx();
    let rel = table::table_open(mcx, fcinfo.arg_oid(0), AccessShareLock)?;
    let out = match crate::is_pgrc_table(&rel) {
        true => crate::compact::stats(&rel),
        false => Err(Box::new(PgError::error(format!("pgrc_compact_stats: \"{}\" is not a pgrcolumnar table", rel.name())))),
    };
    rel.close(NoLock)?;
    Ok(types_fmgr::varlena_result(varlena::cstring_to_text(mcx, out?.as_bytes())?))
}

/// Statements SQE serves are not counted, in pg_stat or here (its engine bridge is not hooked), until SQE is removed.
pub fn fc_pgrc_scan_stats(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let mcx = fcinfo.result_mcx();
    let rel = table::table_open(mcx, fcinfo.arg_oid(0), AccessShareLock)?;
    let out = match crate::is_pgrc_table(&rel) {
        true => Ok(pgrc_am::scanstats::stats_text(rel.rd_id)),
        false => Err(Box::new(PgError::error(format!("pgrc_scan_stats: \"{}\" is not a pgrcolumnar table", rel.name())))),
    };
    rel.close(NoLock)?;
    Ok(types_fmgr::varlena_result(varlena::cstring_to_text(mcx, out?.as_bytes())?))
}

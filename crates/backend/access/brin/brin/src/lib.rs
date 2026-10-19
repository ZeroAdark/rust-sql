//! brin.c: the BRIN index access method (build, insert, bitmap scan).
//! Summarize/desummarize SQL paths and vacuum live in brin_funcs/brin_build.
//! Loud lanes: parallel build. Autosummarize enqueues autovacuum work items
//! (AutoVacuumRequestWork). Support procs outside the in-core families run
//! through fmgr with C's pointer arguments.
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(clippy::too_many_arguments)]

use ::bufmgr_seams::{
    buffer_get_page, lock_buffer, read_buffer, release_buffer, BUFFER_LOCK_SHARE,
    BUFFER_LOCK_UNLOCK,
};
use ::datum::Datum;
use ::mcx::{vec_with_capacity_in, Mcx, MemoryContext, PgVec};
use ::types_brin::*;
use ::types_core::{BlockNumber, Buffer, ForkNumber, InvalidBuffer};
use ::types_error::PgResult;
use ::types_rel::{AccessShareLock, Relation};
use ::types_relscan::{relation_get_index_scan, IndexScanDescData, IndexScanOpaque};
use ::types_scan::scankey::{ScanKeyData, SK_ISNULL, SK_SEARCHNULL, SK_SEARCHNOTNULL};
use ::types_storage::bufpage::PageRef;
use ::types_tuple::itemptr::{ItemPointerData, ItemPointerGetBlockNumber};
use ::types_tuple::{CompactAttribute, FormData_pg_attribute, TupleDescData};

use brin_pageops::{
    brinGetTupleForHeapBlock, brinRevmapInitialize, brinRevmapTerminate,
    brin_can_do_samepage_update, brin_doupdate,
};
use brin_tuple::{brin_deform_tuple, brin_form_tuple, brin_new_memtuple};

// C CHECK_FOR_INTERRUPTS (heapam's gated-helper shape): brin.c CFIs every
// range in bringetbitmap and every brininsert retry.
#[inline(never)]
fn process_interrupts() -> PgResult<()> {
    postgres_seams::check_for_interrupts::call()
}

#[inline(always)]
fn check_for_interrupts() -> PgResult<()> {
    if init_small::globals::InterruptPending() {
        return process_interrupts();
    }
    Ok(())
}

#[cold]
#[inline(never)]
fn missing_support_function(procnum: u16, attnum: usize, index: &str) -> Box<::types_error::PgError> {
    Box::new(::types_error::PgError::error(format!(
        "missing support function {procnum} for attribute {attnum} of index \"{index}\""
    )))
}

// Takes the desc so per-row callers don't hoist the index name's loads.
#[cold]
#[inline(never)]
fn missing_proc(bdesc: &BrinDesc<'_>, procnum: u16, attnum: usize) -> Box<::types_error::PgError> {
    missing_support_function(procnum, attnum, &bdesc.bd_index_name)
}

/// A pg_amproc entry naming the in-core proc of its slot (directly or as a
/// LANGUAGE internal alias) dispatches natively; any other runs through fmgr.
/// `natives` is the slot's proc in the minmax, minmax-multi, bloom and inclusion
/// families.
fn resolve_proc(
    oid: ::types_core::Oid,
    natives: [::types_core::Oid; 4],
    options: Option<&[u8]>,
) -> PgResult<BrinProc> {
    if oid == ::types_core::InvalidOid {
        return Ok(BrinProc::Missing);
    }
    let oid = if oid >= ::types_core::catalog::FirstNormalObjectId
        && fmgr_seams::internal_builtin_oid::is_installed()
    {
        fmgr_seams::internal_builtin_oid::call(oid)?.unwrap_or(oid)
    } else {
        oid
    };
    Ok(match natives.iter().position(|&c| c == oid) {
        Some(0) => BrinProc::MinMax,
        Some(1) => BrinProc::MinMaxMulti,
        Some(2) => BrinProc::Bloom,
        Some(3) => BrinProc::Inclusion,
        _ => BrinProc::Fmgr(BrinFmgrProc {
            oid,
            finfo: core::cell::RefCell::new(None),
            options: options.map(|o| Box::new(fmgr::OpclassOptions(o.into()))),
        }),
    })
}

/// index_getprocinfo for a non-core proc: resolved once, cached on the desc,
/// and invoked in place (fn_extra persists across calls as in C).
fn with_fmgr<R>(
    p: &BrinFmgrProc,
    f: impl FnOnce(&mut fmgr::FmgrInfo) -> PgResult<R>,
) -> PgResult<R> {
    let mut slot = p.finfo.try_borrow_mut().map_err(|_| recursive_support_call(p.oid))?;
    if slot.is_none() {
        let mut finfo = fmgr_core::fmgr_info(p.oid)?;
        if let Some(o) = p.options.as_deref() {
            // SAFETY: the options box and this cache both live in `p`.
            unsafe { finfo.set_opclass_options(o) };
        }
        *slot = Some(finfo);
    }
    f(slot.as_mut().expect("resolved above"))
}

// The heavy in-core add_value arms stay out of add_values_to_range so their
// hoisted constants and saved registers don't land on every family's row path.
#[inline(never)]
fn minmax_multi_add_value(
    mcx: Mcx<'_>,
    bdesc: &BrinDesc<'_>,
    bval: &mut BrinValues,
    value: Datum,
    isnull: bool,
    colloid: ::types_core::Oid,
) -> PgResult<bool> {
    brin_minmax_multi::brin_minmax_multi_add_value(mcx, bdesc, bval, value, isnull, colloid)
}

#[inline(never)]
fn inclusion_add_value(
    mcx: Mcx<'_>,
    bdesc: &BrinDesc<'_>,
    bval: &mut BrinValues,
    value: Datum,
    isnull: bool,
    colloid: ::types_core::Oid,
) -> PgResult<bool> {
    brin_inclusion::brin_inclusion_add_value(mcx, bdesc, bval, value, isnull, colloid)
}

// Extension support procs (C's FunctionCallN with pointer arguments) stay
// outlined so the in-core arms keep their codegen.
#[inline(never)]
fn fmgr_add_value(
    p: &BrinFmgrProc,
    bdesc: &BrinDesc<'_>,
    keyno: usize,
    mcx: Mcx<'_>,
    bval: &mut BrinValues,
    value: Datum,
    isnull: bool,
) -> PgResult<bool> {
    let bvalp: *mut BrinValues = bval;
    with_fmgr(p, |fi| {
        Ok(fmgr::function_call4_coll_in(
            fi,
            bdesc.bd_indcollation[keyno],
            mcx,
            ptr_datum(bdesc),
            Datum::from_usize(bvalp as usize),
            value,
            Datum::from_bool(isnull),
        )?
        .as_bool())
    })
}

#[inline(never)]
fn fmgr_consistent(
    p: &BrinFmgrProc,
    bdesc: &BrinDesc<'_>,
    keyno: usize,
    mcx: Mcx<'_>,
    bval: &BrinValues,
    keys: &[&ScanKeyData],
) -> PgResult<bool> {
    with_fmgr(p, |fi| {
        if fi.fn_nargs >= 4 {
            return Ok(fmgr::function_call4_coll_in(
                fi,
                keys[0].sk_collation,
                mcx,
                ptr_datum(bdesc),
                ptr_datum(bval),
                ptr_datum(keys.as_ptr()),
                Datum::from_i32(keys.len() as i32),
            )?
            .as_bool());
        }
        for key in keys {
            let add = fmgr::function_call3_coll_in(
                fi,
                key.sk_collation,
                mcx,
                ptr_datum(bdesc),
                ptr_datum(bval),
                ptr_datum(*key),
            )?;
            if !add.as_bool() {
                return Ok(false);
            }
        }
        Ok(true)
    })
}

#[inline(never)]
fn fmgr_union(
    p: &BrinFmgrProc,
    bdesc: &BrinDesc<'_>,
    keyno: usize,
    mcx: Mcx<'_>,
    col_a: &mut BrinValues,
    col_b: &BrinValues,
) -> PgResult<()> {
    let col_ap: *mut BrinValues = col_a;
    with_fmgr(p, |fi| {
        fmgr::function_call3_coll_in(
            fi,
            bdesc.bd_indcollation[keyno],
            mcx,
            ptr_datum(bdesc),
            Datum::from_usize(col_ap as usize),
            ptr_datum(col_b),
        )
    })?;
    Ok(())
}

#[cold]
#[inline(never)]
fn recursive_support_call(oid: ::types_core::Oid) -> Box<::types_error::PgError> {
    Box::new(::types_error::PgError::error(format!(
        "recursive call of BRIN support function {oid}"
    )))
}

#[inline]
fn ptr_datum<T>(p: *const T) -> Datum {
    Datum::from_usize(p as usize)
}

/// BrinGetPagesPerRange / BrinGetAutoSummarize
pub fn brin_get_pages_per_range(rel: &Relation<'_>) -> BlockNumber {
    match rel.rd_options.as_ref().and_then(|o| o.brin()) {
        Some(o) => o.pages_per_range as BlockNumber,
        None => BRIN_DEFAULT_PAGES_PER_RANGE,
    }
}

fn brin_get_auto_summarize(rel: &Relation<'_>) -> bool {
    match rel.rd_options.as_ref().and_then(|o| o.brin()) {
        Some(o) => o.autosummarize,
        None => false,
    }
}

pub fn brin_build_desc<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<BrinDesc<'mcx>> {
    let tupdesc = rel.rd_att.clone();
    let natts = tupdesc.natts as usize;

    // C: options ride each support fn's flinfo via index_getprocinfo; the
    // direct-dispatch port pins them on the column info instead (uninstalled
    // seam = unit-test paths, where no opclass has options).
    let attoptions = if indexam_seams::relation_get_index_att_options::is_installed() {
        Some(indexam_seams::relation_get_index_att_options::call(rel)?)
    } else {
        None
    };

    let mut info: Vec<BrinColInfo> = Vec::with_capacity(natts);
    let mut totalstored = 0usize;
    for keyno in 0..natts {
        let opfamily = rel.rd_opfamily[keyno];
        let opcintype = rel.rd_opcintype[keyno];
        let opcinfo_proc = lsyscache::get_opfamily_proc(
            opfamily,
            opcintype,
            opcintype,
            BRIN_PROCNUM_OPCINFO as i16,
        )?;
        // brin.c:1608 index_getprocinfo + FunctionCall1: a LANGUAGE internal
        // alias of an in-core opcInfo proc dispatches to that proc's body.
        let opcinfo_proc = if opcinfo_proc >= ::types_core::catalog::FirstNormalObjectId
            && fmgr_seams::internal_builtin_oid::is_installed()
        {
            fmgr_seams::internal_builtin_oid::call(opcinfo_proc)?.unwrap_or(opcinfo_proc)
        } else {
            opcinfo_proc
        };
        let options: Option<Box<[u8]>> =
            attoptions.as_ref().and_then(|a| a.get(keyno)).and_then(|o| o.as_ref()).cloned();
        // brin.c:1611: the opcInfo proc receives the index attribute's type
        // (opckeytype when the opclass declares STORAGE), not rd_opcintype.
        let atttypid = tupdesc.attr(keyno).atttypid;
        let mut col = match opcinfo_proc {
            F_BRIN_MINMAX_OPCINFO => brin_minmax::brin_minmax_opcinfo(atttypid),
            F_BRIN_MINMAX_MULTI_OPCINFO => {
                brin_minmax_multi::brin_minmax_multi_opcinfo(atttypid)
            }
            F_BRIN_INCLUSION_OPCINFO => brin_inclusion::brin_inclusion_opcinfo(atttypid),
            F_BRIN_BLOOM_OPCINFO => brin_bloom::brin_bloom_opcinfo(atttypid),
            other => {
                let mut finfo = fmgr_core::fmgr_info(other)?;
                let call_options = options.as_ref().map(|o| fmgr::OpclassOptions(o.clone()));
                if let Some(o) = call_options.as_ref() {
                    // SAFETY: `call_options` outlives this one call.
                    unsafe { finfo.set_opclass_options(o) };
                }
                let d = fmgr_core::function_call1_coll_in(
                    &mut finfo,
                    ::types_core::InvalidOid,
                    mcx,
                    Datum::from_oid(atttypid),
                )?;
                // SAFETY: a non-core opcInfo proc returns brin_opcinfo_into_datum.
                unsafe { brin_opcinfo_from_datum(d) }
            }
        };
        if let Some(inc) = col.inclusion.as_mut() {
            inc.options = options.as_ref().map(|o| Box::new(fmgr::OpclassOptions(o.clone())));
        }
        col.oi_opclass_options = options;
        debug_assert!((col.oi_nstored as usize) <= BRIN_MAX_NSTORED);
        totalstored += col.oi_nstored as usize;
        info.push(col);
    }

    let mut attrs: PgVec<'mcx, FormData_pg_attribute> = vec_with_capacity_in(mcx, totalstored)?;
    let mut compact: PgVec<'mcx, CompactAttribute> = vec_with_capacity_in(mcx, totalstored)?;
    for keyno in 0..natts {
        for i in 0..info[keyno].oi_nstored as usize {
            let mut att = tupdesc.attr(keyno).clone();
            att.attnum = (attrs.len() + 1) as i16;
            att.atttypmod = -1;
            att.attndims = 0;
            att.attnotnull = false;
            let stored_typid = info[keyno].oi_typids[i];
            if stored_typid != att.atttypid {
                // TupleDescInitEntry over the opclass's summary type; only the
                // fill/deform-relevant pg_type fields matter for the disk desc.
                match stored_typid {
                    PG_BRIN_MINMAX_MULTI_SUMMARYOID | PG_BRIN_BLOOM_SUMMARYOID => {
                        att.attlen = -1;
                        att.attbyval = false;
                        att.attalign = types_tuple::TYPALIGN_INT;
                        att.attstorage = types_tuple::TYPSTORAGE_EXTENDED;
                        att.attcollation = types_core::DEFAULT_COLLATION_OID;
                    }
                    types_core::BOOLOID => {
                        att.attlen = 1;
                        att.attbyval = true;
                        att.attalign = types_tuple::TYPALIGN_CHAR;
                        att.attstorage = types_tuple::TYPSTORAGE_PLAIN;
                        att.attcollation = 0;
                    }
                    other => {
                        let (typlen, typbyval, typalign) = lsyscache::get_typlenbyvalalign(other)?;
                        att.attlen = typlen;
                        att.attbyval = typbyval;
                        att.attalign = typalign;
                        att.attstorage = lsyscache::get_typstorage(other)?;
                        att.attcollation = lsyscache::get_typcollation(other)?;
                    }
                }
                att.atttypid = stored_typid;
            } else {
                // TupleDescInitEntry reads pg_type.typstorage, not the heap
                // column's attstorage (ALTER ... SET STORAGE must not change
                // the index's packing/compress eligibility).
                att.attstorage = lsyscache::get_typstorage(stored_typid)?;
            }
            att.attcompression = 0;
            compact.push(CompactAttribute::populate_from(&att));
            attrs.push(att);
        }
    }
    let disktdesc = TupleDescData {
        natts: totalstored as i32,
        tdtypeid: types_core::RECORDOID,
        tdtypmod: -1,
        tdrefcount: -1,
        constr: None,
        compact_attrs: compact,
        attrs,
    };

    let amsupport = rel.rd_support.len() / natts.max(1);
    let mut procs: Vec<BrinColProcs> = Vec::with_capacity(natts);
    for keyno in 0..natts {
        let options = info[keyno].oi_opclass_options.as_deref();
        let proc_oid = |procnum: u16| {
            rel.rd_support
                .get(keyno * amsupport + procnum as usize - 1)
                .copied()
                .unwrap_or(::types_core::InvalidOid)
        };
        procs.push(BrinColProcs {
            add_value: resolve_proc(
                proc_oid(BRIN_PROCNUM_ADDVALUE),
                [
                    F_BRIN_MINMAX_ADD_VALUE,
                    F_BRIN_MINMAX_MULTI_ADD_VALUE,
                    F_BRIN_BLOOM_ADD_VALUE,
                    F_BRIN_INCLUSION_ADD_VALUE,
                ],
                options,
            )?,
            consistent: resolve_proc(
                proc_oid(BRIN_PROCNUM_CONSISTENT),
                [
                    F_BRIN_MINMAX_CONSISTENT,
                    F_BRIN_MINMAX_MULTI_CONSISTENT,
                    F_BRIN_BLOOM_CONSISTENT,
                    F_BRIN_INCLUSION_CONSISTENT,
                ],
                options,
            )?,
            union: resolve_proc(
                proc_oid(BRIN_PROCNUM_UNION),
                [
                    F_BRIN_MINMAX_UNION,
                    F_BRIN_MINMAX_MULTI_UNION,
                    F_BRIN_BLOOM_UNION,
                    F_BRIN_INCLUSION_UNION,
                ],
                options,
            )?,
        });
    }

    Ok(BrinDesc {
        bd_tupdesc: tupdesc,
        bd_disktdesc: disktdesc,
        bd_totalstored: totalstored,
        bd_opfamily: rel.rd_opfamily.iter().copied().collect(),
        bd_opcintype: rel.rd_opcintype.iter().copied().collect(),
        bd_indcollation: rel.rd_indcollation.iter().copied().collect(),
        bd_pages_per_range: brin_get_pages_per_range(rel),
        bd_info: info,
        bd_procs: procs,
        bd_index_name: rel.name().to_owned(),
        bd_support: rel.rd_support.iter().copied().collect(),
    })
}

pub struct BrinStatsData {
    pub pagesPerRange: BlockNumber,
    pub revmapNumPages: BlockNumber,
}

pub fn brinGetStats(index: &Relation<'_>) -> PgResult<BrinStatsData> {
    let metabuffer = read_buffer::call(index, BRIN_METAPAGE_BLKNO)?;
    lock_buffer::call(metabuffer, BUFFER_LOCK_SHARE)?;
    // SAFETY: pinned + share locked.
    let metadata =
        brin_meta_read(unsafe { &PageRef::from_raw(buffer_get_page::call(metabuffer)) });
    lock_buffer::call(metabuffer, BUFFER_LOCK_UNLOCK)?;
    release_buffer::call(metabuffer)?;
    Ok(BrinStatsData {
        pagesPerRange: metadata.pagesPerRange,
        revmapNumPages: metadata.lastRevmapPage.wrapping_sub(1),
    })
}

fn initialize_brin_insertstate<'mcx>(
    mcx: Mcx<'mcx>,
    idxRel: &Relation<'mcx>,
) -> PgResult<BrinInsertState<'mcx>> {
    let bis_desc = brin_build_desc(mcx, idxRel)?;
    let (bis_rmAccess, bis_pages_per_range) = brinRevmapInitialize(idxRel)?;
    Ok(BrinInsertState { bis_rmAccess, bis_desc, bis_pages_per_range })
}

// brininsert's autosummarize request (brin.c): register the previous page
// range with autovacuum's work-item queue. The queue is best-effort: an
// unrecorded request (work-item array full) only LOGs, and the range stays
// summarizable through VACUUM and brin_summarize_new_values().
fn request_range_summarization(
    idxRel: &Relation<'_>,
    lastPageRange: BlockNumber,
) -> PgResult<()> {
    let recorded = autovacuum_seams::auto_vacuum_request_work::call(
        autovacuum_seams::AVW_BRIN_SUMMARIZE_RANGE,
        idxRel.rd_id,
        lastPageRange,
    );
    if !recorded {
        elog_seams::ereport::call(
            ::types_error::PgError::new(
                ::types_error::LOG,
                format!(
                    "request for BRIN range summarization for index \"{}\" page {} was not recorded",
                    idxRel.name(),
                    lastPageRange
                ),
            )
            .with_sqlstate(::types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED)
            .with_location("brin.c", 416, "brininsert"),
        )?;
    }
    Ok(())
}

/// brininsert. `amcache` is C's indexInfo->ii_AmCache slot, owned by the
/// executor's per-command index state.
pub fn brininsert<'mcx>(
    mcx: Mcx<'mcx>,
    idxRel: &Relation<'mcx>,
    values: &[Datum],
    nulls: &[bool],
    heaptid: &ItemPointerData,
    amcache: &mut Option<BrinInsertState<'mcx>>,
) -> PgResult<bool> {
    let autosummarize = brin_get_auto_summarize(idxRel);

    if amcache.is_none() {
        *amcache = Some(initialize_brin_insertstate(mcx, idxRel)?);
    }
    let bistate = amcache.as_ref().expect("initialized above");
    let revmap = &bistate.bis_rmAccess;
    let bdesc = &bistate.bis_desc;
    let pagesPerRange = bistate.bis_pages_per_range;

    let origHeapBlk = ItemPointerGetBlockNumber(heaptid);
    let heapBlk = (origHeapBlk / pagesPerRange) * pagesPerRange;

    let mut buf: Buffer = InvalidBuffer;

    loop {
        // C brin.c:386: CFI at the top of every insert retry.
        check_for_interrupts()?;

        // First tuple of a new page range: ask for summarization of the
        // previous one (C brininsert's autosummarize block).
        if autosummarize && heapBlk > 0 && heapBlk == origHeapBlk && heaptid.ip_posid == 1 {
            let lastPageRange = heapBlk - 1;
            let probecxt = MemoryContext::new_bump("brininsert autosummarize probe");
            let mut lastPageTuple: PgVec<'_, u8> = PgVec::new_in(probecxt.mcx());
            let got = brinGetTupleForHeapBlock(
                idxRel,
                revmap,
                lastPageRange,
                &mut buf,
                &mut lastPageTuple,
                BUFFER_LOCK_SHARE,
                true,
            )?;
            if got.is_none() {
                request_range_summarization(idxRel, lastPageRange)?;
            } else {
                lock_buffer::call(buf, BUFFER_LOCK_UNLOCK)?;
            }
        }

        let tupcxt = MemoryContext::new_bump("brininsert cxt");
        let cmcx = tupcxt.mcx();
        let mut brtup: PgVec<'_, u8> = PgVec::new_in(cmcx);
        let got = brinGetTupleForHeapBlock(
            idxRel,
            revmap,
            heapBlk,
            &mut buf,
            &mut brtup,
            BUFFER_LOCK_SHARE,
            true,
        )?;

        let Some(off) = got else {
            break;
        };

        let mut dtup = brin_new_memtuple(bdesc);
        brin_deform_tuple(bdesc, &brtup, &mut dtup)?;

        let need_insert = add_values_to_range(bdesc, &mut dtup, values, nulls)?;

        if !need_insert {
            lock_buffer::call(buf, BUFFER_LOCK_UNLOCK)?;
        } else {
            let newtup = brin_form_tuple(cmcx, bdesc, heapBlk, &mut dtup)?;
            let samepage = brin_can_do_samepage_update(buf, brtup.len(), newtup.len());
            lock_buffer::call(buf, BUFFER_LOCK_UNLOCK)?;

            if !brin_doupdate(
                idxRel,
                pagesPerRange,
                revmap,
                heapBlk,
                buf,
                off,
                &brtup,
                &newtup,
                samepage,
            )? {
                continue;
            }
        }

        break;
    }

    if buf != InvalidBuffer {
        release_buffer::call(buf)?;
    }

    Ok(false)
}

pub fn brininsertcleanup(amcache: &mut Option<BrinInsertState<'_>>) -> PgResult<()> {
    let Some(bistate) = amcache.take() else {
        return Ok(());
    };
    brinRevmapTerminate(bistate.bis_rmAccess)
}

pub fn brinbeginscan<'mcx>(
    mcx: Mcx<'mcx>,
    r: &Relation<'mcx>,
    nkeys: i32,
    norderbys: i32,
) -> PgResult<IndexScanDescData<'mcx>> {
    let (bo_rmAccess, bo_pagesPerRange) = brinRevmapInitialize(r)?;
    let bo_bdesc = brin_build_desc(mcx, r)?;
    let opaque =
        ::mcx::alloc_in(mcx, BrinOpaque { bo_pagesPerRange, bo_rmAccess, bo_bdesc })?;
    relation_get_index_scan(
        mcx,
        r,
        nkeys,
        norderbys,
        IndexScanOpaque::Brin(opaque),
        xact::TransactionStartedDuringRecovery(),
    )
}

pub fn bringetbitmap(
    scan: &mut IndexScanDescData<'_>,
    tbm: &mut tidbitmap::TIDBitmap<'_>,
) -> PgResult<i64> {
    if scan.index_rel().pgstat_enabled.get() {
        scan.xs_pgstat_index_scans += 1;
    }
    scan.xs_nsearches += 1;

    let idxRel = scan.index_rel().alias();
    let scan_mcx = *scan.keyData.allocator();
    let nkeys_total = scan.numberOfKeys.max(0) as usize;

    let IndexScanOpaque::Brin(opaque) = &mut scan.opaque else {
        unreachable!("bringetbitmap on non-BRIN opaque")
    };
    let bdesc = &opaque.bo_bdesc;
    let natts = bdesc.natts();
    let mut totalpages: i64 = 0;

    let heapOid = idxRel.rd_index.as_ref().expect("index").indrelid;
    let heapRel = table::table_open(scan_mcx, heapOid, AccessShareLock)?;
    let nblocks = bufmgr_seams::relation_get_number_of_blocks_in_fork::call(
        &heapRel,
        ForkNumber::MAIN_FORKNUM,
    )?;
    heapRel.close(AccessShareLock)?;

    let mut keys: Vec<Vec<&ScanKeyData>> =
        (0..natts).map(|_| Vec::with_capacity(nkeys_total)).collect();
    let mut nullkeys: Vec<Vec<&ScanKeyData>> =
        (0..natts).map(|_| Vec::with_capacity(nkeys_total)).collect();
    for key in scan.keyData.iter().take(nkeys_total) {
        let keyattno = key.sk_attno as usize;
        debug_assert!(
            (key.sk_flags & SK_ISNULL) != 0
                || key.sk_collation == bdesc.bd_tupdesc.attr(keyattno - 1).attcollation
        );
        if keys[keyattno - 1].is_empty() && nullkeys[keyattno - 1].is_empty() {
            match &bdesc.bd_procs[keyattno - 1].consistent {
                BrinProc::Missing => {
                    return Err(missing_proc(bdesc, BRIN_PROCNUM_CONSISTENT, keyattno))
                }
                BrinProc::Fmgr(p) => {
                    with_fmgr(p, |_| Ok(()))?
                }
                _ => {}
            }
        }
        if key.sk_flags & SK_ISNULL != 0 {
            nullkeys[keyattno - 1].push(key);
        } else {
            keys[keyattno - 1].push(key);
        }
    }

    let mut dtup = brin_new_memtuple(bdesc);

    let mut per_range = MemoryContext::new_bump("bringetbitmap cxt");
    let mut buf: Buffer = InvalidBuffer;
    let mut btup: PgVec<'_, u8> = PgVec::new_in(scan_mcx);

    // u64 iteration: heapBlk could wrap for tables near 2^32 pages.
    let mut heapBlk: u64 = 0;
    while heapBlk < nblocks as u64 {
        // C brin.c:748: CFI once per page range in the whole-index walk.
        check_for_interrupts()?;

        per_range.reset();

        let gottuple = brinGetTupleForHeapBlock(
            &idxRel,
            &opaque.bo_rmAccess,
            heapBlk as BlockNumber,
            &mut buf,
            &mut btup,
            BUFFER_LOCK_SHARE,
            false,
        )?
        .is_some();

        let addrange = if !gottuple {
            true
        } else {
            brin_deform_tuple(bdesc, &btup, &mut dtup)?;
            if dtup.bt_placeholder {
                true
            } else {
                let mut addrange = true;
                for attno in 1..=natts {
                    if keys[attno - 1].is_empty() && nullkeys[attno - 1].is_empty() {
                        continue;
                    }

                    if dtup.bt_empty_range {
                        addrange = false;
                        break;
                    }

                    let bval = &dtup.bt_columns[attno - 1];

                    if bdesc.bd_info[attno - 1].oi_regular_nulls
                        && !check_null_keys(bval, &nullkeys[attno - 1])
                    {
                        addrange = false;
                        break;
                    }

                    if keys[attno - 1].is_empty() {
                        continue;
                    }

                    if bval.bv_allnulls {
                        addrange = false;
                        break;
                    }

                    match &bdesc.bd_procs[attno - 1].consistent {
                        BrinProc::MinMax => {
                            for key in &keys[attno - 1] {
                                addrange = brin_minmax::brin_minmax_consistent(
                                    per_range.mcx(),
                                    bdesc,
                                    bval,
                                    key,
                                )?;
                                if !addrange {
                                    break;
                                }
                            }
                        }
                        // Multi-key consistent (C fn_nargs >= 4): all keys at
                        // once, collation from the first key.
                        BrinProc::MinMaxMulti => {
                            addrange = brin_minmax_multi::brin_minmax_multi_consistent(
                                per_range.mcx(),
                                bdesc,
                                bval,
                                &keys[attno - 1],
                                keys[attno - 1][0].sk_collation,
                            )?;
                        }
                        // C's bloom consistent takes the key array (fn_nargs
                        // >= 4) but tests keys independently; per-key calls
                        // are equivalent.
                        BrinProc::Bloom => {
                            for key in &keys[attno - 1] {
                                addrange = brin_bloom::brin_bloom_consistent(
                                    per_range.mcx(),
                                    bdesc,
                                    bval,
                                    key,
                                )?;
                                if !addrange {
                                    break;
                                }
                            }
                        }
                        BrinProc::Inclusion => {
                            for key in &keys[attno - 1] {
                                addrange = brin_inclusion::brin_inclusion_consistent(
                                    per_range.mcx(),
                                    bdesc,
                                    bval,
                                    key,
                                )?;
                                if !addrange {
                                    break;
                                }
                            }
                        }
                        BrinProc::Fmgr(p) => {
                            addrange = fmgr_consistent(
                                p,
                                bdesc,
                                attno - 1,
                                per_range.mcx(),
                                bval,
                                &keys[attno - 1],
                            )?;
                        }
                        BrinProc::Missing => unreachable!("consistent proc checked at key setup"),
                    }
                    if !addrange {
                        break;
                    }
                }
                addrange
            }
        };

        if addrange {
            let last = core::cmp::min(nblocks as u64, heapBlk + opaque.bo_pagesPerRange as u64);
            for pageno in heapBlk..last {
                tbm.add_page(pageno as BlockNumber)?;
                totalpages += 1;
            }
        }

        heapBlk += opaque.bo_pagesPerRange as u64;
    }

    if buf != InvalidBuffer {
        release_buffer::call(buf)?;
    }

    Ok(totalpages * 10)
}

pub fn brinrescan(
    scan: &mut IndexScanDescData<'_>,
    scankey: Option<&[ScanKeyData]>,
) -> PgResult<()> {
    if let Some(keys) = scankey {
        if scan.numberOfKeys > 0 {
            scan.keyData.clear();
            for k in keys.iter().take(scan.numberOfKeys as usize) {
                scan.keyData.push(k.clone());
            }
        }
    }
    Ok(())
}

pub fn brinendscan(scan: &mut IndexScanDescData<'_>) -> PgResult<()> {
    let IndexScanOpaque::Brin(opaque) = &mut scan.opaque else {
        unreachable!("brinendscan on non-BRIN opaque")
    };
    let rm = core::mem::replace(
        &mut opaque.bo_rmAccess,
        BrinRevmap {
            rm_pagesPerRange: 0,
            rm_lastRevmapPage: core::cell::Cell::new(0),
            rm_metaBuf: InvalidBuffer,
            rm_currBuf: core::cell::Cell::new(InvalidBuffer),
        },
    );
    release_buffer::call(rm.rm_metaBuf)?;
    if rm.rm_currBuf.get() != InvalidBuffer {
        release_buffer::call(rm.rm_currBuf.get())?;
    }
    Ok(())
}

pub fn add_values_to_range(
    bdesc: &BrinDesc<'_>,
    dtup: &mut BrinMemTuple,
    values: &[Datum],
    nulls: &[bool],
) -> PgResult<bool> {
    let empty_range_at_entry = dtup.bt_empty_range;
    let mut modified = empty_range_at_entry;

    let BrinMemTuple { bt_context, bt_columns, .. } = dtup;
    let mcx = bt_context.mcx();

    // Per-row loop: slices hoisted to natts so the index checks leave the loop.
    let natts = bdesc.natts();
    let bt_columns = &mut bt_columns[..natts];
    let (info, procs) = (&bdesc.bd_info[..natts], &bdesc.bd_procs[..natts]);
    let colls = &bdesc.bd_indcollation[..natts];
    let (values, nulls) = (&values[..natts], &nulls[..natts]);

    for keyno in 0..natts {
        let bval = &mut bt_columns[keyno];

        let has_nulls = !empty_range_at_entry && (bval.bv_hasnulls || bval.bv_allnulls);

        if info[keyno].oi_regular_nulls && nulls[keyno] {
            if !bval.bv_hasnulls {
                bval.bv_hasnulls = true;
                modified = true;
            }
            continue;
        }

        let result = match &procs[keyno].add_value {
            BrinProc::MinMax => brin_minmax::brin_minmax_add_value(
                mcx,
                bdesc,
                bval,
                values[keyno],
                nulls[keyno],
                colls[keyno],
            )?,
            BrinProc::MinMaxMulti => minmax_multi_add_value(
                mcx,
                bdesc,
                bval,
                values[keyno],
                nulls[keyno],
                colls[keyno],
            )?,
            BrinProc::Bloom => brin_bloom::brin_bloom_add_value(
                mcx,
                bdesc,
                bval,
                values[keyno],
                nulls[keyno],
                colls[keyno],
            )?,
            BrinProc::Inclusion => inclusion_add_value(
                mcx,
                bdesc,
                bval,
                values[keyno],
                nulls[keyno],
                colls[keyno],
            )?,
            BrinProc::Fmgr(p) => fmgr_add_value(p, bdesc, keyno, mcx, bval, values[keyno], nulls[keyno])?,
            BrinProc::Missing => {
                return Err(missing_proc(bdesc, BRIN_PROCNUM_ADDVALUE, keyno + 1))
            }
        };
        modified |= result;

        if has_nulls && !(bval.bv_hasnulls || bval.bv_allnulls) {
            debug_assert!(modified);
            bval.bv_hasnulls = true;
        }
    }

    debug_assert!(!dtup.bt_empty_range || modified);
    dtup.bt_empty_range = false;

    Ok(modified)
}

fn check_null_keys(bval: &BrinValues, nullkeys: &[&ScanKeyData]) -> bool {
    for key in nullkeys {
        debug_assert!(key.sk_attno as u16 == bval.bv_attno);
        if key.sk_flags & SK_ISNULL == 0 {
            continue;
        }
        if key.sk_flags & SK_SEARCHNULL != 0 {
            if !bval.bv_allnulls && !bval.bv_hasnulls {
                return false;
            }
        } else if key.sk_flags & SK_SEARCHNOTNULL != 0 {
            if bval.bv_allnulls {
                return false;
            }
        } else {
            return false;
        }
    }
    true
}

/// union_tuples (summarize_range's merge step).
pub fn union_tuples(bdesc: &BrinDesc<'_>, a: &mut BrinMemTuple, b: &[u8]) -> PgResult<()> {
    let mut db = brin_new_memtuple(bdesc);
    brin_deform_tuple(bdesc, b, &mut db)?;

    if db.bt_empty_range {
        return Ok(());
    }

    let BrinMemTuple { bt_context, bt_columns, bt_empty_range, .. } = a;
    let mcx = bt_context.mcx();

    if *bt_empty_range {
        for keyno in 0..bdesc.natts() {
            let col_a = &mut bt_columns[keyno];
            let col_b = &db.bt_columns[keyno];
            col_a.bv_allnulls = col_b.bv_allnulls;
            col_a.bv_hasnulls = col_b.bv_hasnulls;
            if col_b.bv_allnulls {
                continue;
            }
            for i in 0..bdesc.bd_info[keyno].oi_nstored as usize {
                let (byval, len) = disk_attr_for(bdesc, keyno, i);
                col_a.bv_values[i] = brin_tuple::datum_copy(mcx, col_b.bv_values[i], byval, len)?;
            }
        }
        *bt_empty_range = false;
        return Ok(());
    }

    for keyno in 0..bdesc.natts() {
        let col_a = &mut bt_columns[keyno];
        let col_b = &db.bt_columns[keyno];

        if bdesc.bd_info[keyno].oi_regular_nulls {
            let b_has_nulls = col_b.bv_hasnulls || col_b.bv_allnulls;
            if !col_a.bv_allnulls && b_has_nulls {
                col_a.bv_hasnulls = true;
            }
            if col_b.bv_allnulls {
                continue;
            }
            if col_a.bv_allnulls {
                col_a.bv_allnulls = false;
                col_a.bv_hasnulls = true;
                for i in 0..bdesc.bd_info[keyno].oi_nstored as usize {
                    let (byval, len) = disk_attr_for(bdesc, keyno, i);
                    col_a.bv_values[i] =
                        brin_tuple::datum_copy(mcx, col_b.bv_values[i], byval, len)?;
                }
                continue;
            }
        }

        match &bdesc.bd_procs[keyno].union {
            BrinProc::MinMax => brin_minmax::brin_minmax_union(
                mcx,
                bdesc,
                bdesc.bd_indcollation[keyno],
                col_a,
                col_b,
            )?,
            BrinProc::MinMaxMulti => brin_minmax_multi::brin_minmax_multi_union(
                mcx,
                bdesc,
                bdesc.bd_indcollation[keyno],
                col_a,
                col_b,
            )?,
            BrinProc::Bloom => brin_bloom::brin_bloom_union(
                mcx,
                bdesc,
                bdesc.bd_indcollation[keyno],
                col_a,
                col_b,
            )?,
            BrinProc::Inclusion => brin_inclusion::brin_inclusion_union(
                mcx,
                bdesc,
                bdesc.bd_indcollation[keyno],
                col_a,
                col_b,
            )?,
            BrinProc::Fmgr(p) => fmgr_union(p, bdesc, keyno, mcx, col_a, col_b)?,
            BrinProc::Missing => {
                return Err(missing_proc(bdesc, BRIN_PROCNUM_UNION, keyno + 1))
            }
        }
    }
    Ok(())
}

fn disk_attr_for(bdesc: &BrinDesc<'_>, keyno: usize, i: usize) -> (bool, i16) {
    let mut stored = 0usize;
    for k in 0..keyno {
        stored += bdesc.bd_info[k].oi_nstored as usize;
    }
    let att = bdesc.bd_disktdesc.compact_attr(stored + i);
    (att.attbyval, att.attlen)
}

#[cfg(test)]
mod support_proc_dispatch_tests {
    use std::rc::Rc;

    use ::fmgr::{FmgrInfo, FunctionCallInfoBaseData as Fcinfo};
    use ::types_tuple::FormData_pg_attribute;

    use super::*;

    const EXT_ADD_VALUE: ::types_core::Oid = 70001;

    fn desc<'mcx>(mcx: Mcx<'mcx>, add_value: BrinProc) -> BrinDesc<'mcx> {
        let att = FormData_pg_attribute {
            attnum: 1,
            atttypid: ::types_core::INT4OID,
            attlen: 4,
            attbyval: true,
            attalign: types_tuple::TYPALIGN_INT,
            ..Default::default()
        };
        let mk = || {
            let mut attrs = PgVec::new_in(mcx);
            attrs.push(att);
            let mut compact = PgVec::new_in(mcx);
            compact.push(CompactAttribute::populate_from(&att));
            TupleDescData {
                natts: 1,
                tdtypeid: types_core::RECORDOID,
                tdtypmod: -1,
                tdrefcount: -1,
                constr: None,
                compact_attrs: compact,
                attrs,
            }
        };
        BrinDesc {
            bd_tupdesc: Rc::new(mk()),
            bd_disktdesc: mk(),
            bd_totalstored: 1,
            bd_opfamily: vec![0],
            bd_opcintype: vec![::types_core::INT4OID],
            bd_indcollation: vec![4242],
            bd_pages_per_range: 2,
            bd_info: vec![brin_minmax::brin_minmax_opcinfo(::types_core::INT4OID)],
            bd_procs: vec![BrinColProcs {
                add_value,
                consistent: BrinProc::MinMax,
                union: BrinProc::MinMax,
            }],
            bd_index_name: "ext_idx".to_owned(),
            bd_support: vec![],
        }
    }

    // C proc-2 protocol: (BrinDesc *, BrinValues *, value, isnull) under the
    // index collation; the proc owns the summary update.
    fn ext_add_value(_f: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
        assert_eq!(fcinfo.get_collation(), 4242);
        // SAFETY: protocol above; the desc and column outlive the call.
        let bdesc = unsafe { &*(fcinfo.arg(0).as_usize() as *const BrinDesc<'_>) };
        assert_eq!(bdesc.bd_index_name, "ext_idx");
        // SAFETY: as above.
        let column = unsafe { &mut *(fcinfo.arg(1).as_usize() as *mut BrinValues) };
        assert_eq!(column.bv_attno, 1);
        assert!(!fcinfo.arg_bool(3));
        let v = fcinfo.arg(2).as_i32();
        if !column.bv_allnulls && column.bv_values[0].as_i32() >= v {
            return Ok(Datum::from_bool(false));
        }
        column.bv_values[0] = Datum::from_i32(v);
        column.bv_allnulls = false;
        Ok(Datum::from_bool(true))
    }

    fn resolved(f: ::fmgr::PGFunction, oid: ::types_core::Oid) -> BrinProc {
        BrinProc::Fmgr(BrinFmgrProc {
            oid,
            finfo: core::cell::RefCell::new(Some(FmgrInfo::new(f, oid, 4, false, false))),
            options: None,
        })
    }

    #[test]
    fn extension_add_value_runs_through_fmgr() {
        let cxt = MemoryContext::new_bump("brin dispatch test");
        let bdesc = desc(cxt.mcx(), resolved(ext_add_value, EXT_ADD_VALUE));
        let mut dtup = brin_tuple::brin_new_memtuple_fallible(&bdesc).unwrap();
        assert!(add_values_to_range(&bdesc, &mut dtup, &[Datum::from_i32(7)], &[false]).unwrap());
        assert!(!dtup.bt_empty_range);
        assert!(!dtup.bt_columns[0].bv_allnulls);
        assert_eq!(dtup.bt_columns[0].bv_values[0].as_i32(), 7);
        assert!(!add_values_to_range(&bdesc, &mut dtup, &[Datum::from_i32(5)], &[false]).unwrap());
        assert!(add_values_to_range(&bdesc, &mut dtup, &[Datum::from_i32(9)], &[false]).unwrap());
        assert_eq!(dtup.bt_columns[0].bv_values[0].as_i32(), 9);
    }

    #[test]
    fn missing_add_value_fails_like_index_getprocinfo() {
        let cxt = MemoryContext::new_bump("brin dispatch test");
        let bdesc = desc(cxt.mcx(), BrinProc::Missing);
        let mut dtup = brin_tuple::brin_new_memtuple_fallible(&bdesc).unwrap();
        let err = add_values_to_range(&bdesc, &mut dtup, &[Datum::from_i32(7)], &[false]).unwrap_err();
        assert_eq!(
            err.message(),
            "missing support function 2 for attribute 1 of index \"ext_idx\""
        );
    }

    #[test]
    fn resolve_proc_keeps_core_procs_native_by_slot() {
        let add = [
            F_BRIN_MINMAX_ADD_VALUE,
            F_BRIN_MINMAX_MULTI_ADD_VALUE,
            F_BRIN_BLOOM_ADD_VALUE,
            F_BRIN_INCLUSION_ADD_VALUE,
        ];
        assert!(matches!(resolve_proc(0, add, None).unwrap(), BrinProc::Missing));
        assert!(matches!(resolve_proc(F_BRIN_MINMAX_ADD_VALUE, add, None).unwrap(), BrinProc::MinMax));
        assert!(matches!(
            resolve_proc(F_BRIN_MINMAX_MULTI_ADD_VALUE, add, None).unwrap(),
            BrinProc::MinMaxMulti
        ));
        assert!(matches!(resolve_proc(F_BRIN_BLOOM_ADD_VALUE, add, None).unwrap(), BrinProc::Bloom));
        assert!(matches!(
            resolve_proc(F_BRIN_INCLUSION_ADD_VALUE, add, None).unwrap(),
            BrinProc::Inclusion
        ));
        // A core proc of another slot is not this slot's native proc.
        match resolve_proc(F_BRIN_INCLUSION_UNION, add, None).unwrap() {
            BrinProc::Fmgr(p) => assert_eq!(p.oid, F_BRIN_INCLUSION_UNION),
            _ => panic!("misplaced core proc must not dispatch natively"),
        }
    }
}

#[cfg(test)]
mod autosummarize_request_tests {
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, MutexGuard, Once};

    use ::types_core::{Oid, INVALID_PROC_NUMBER, RELPERSISTENCE_PERMANENT};
    use ::types_rel::pg_class::{FormData_pg_class, RELKIND_INDEX};
    use ::types_rel::{LockInfoData, LockRelId, RelationData};
    use ::types_tuple::NameData;

    use super::*;

    // The seams are process-global and set-once: one recorder impl serves
    // every test, with per-test behavior (queue full or not) behind a lock.
    static INSTALL: Once = Once::new();
    static QUEUE_HAS_ROOM: AtomicBool = AtomicBool::new(true);
    static REQUESTS: Mutex<Vec<(i32, Oid, BlockNumber)>> = Mutex::new(Vec::new());
    static LOGS: Mutex<Vec<(::types_error::ErrorLevel, String, Option<::types_error::SqlState>)>> =
        Mutex::new(Vec::new());
    static SERIALIZE: Mutex<()> = Mutex::new(());

    fn install_recorders() -> MutexGuard<'static, ()> {
        INSTALL.call_once(|| {
            autovacuum_seams::auto_vacuum_request_work::set(|av_type, relation_id, blkno| {
                REQUESTS.lock().unwrap().push((av_type, relation_id, blkno));
                QUEUE_HAS_ROOM.load(Ordering::Relaxed)
            });
            elog_seams::ereport_msg::set(|elevel, msg, _detail| {
                LOGS.lock().unwrap().push((elevel, msg, None));
                Ok(())
            });
            elog_seams::ereport::set(|err| {
                LOGS.lock()
                    .unwrap()
                    .push((err.level(), err.message().to_string(), Some(err.sqlstate())));
                Ok(())
            });
        });
        let guard = SERIALIZE.lock().unwrap();
        REQUESTS.lock().unwrap().clear();
        LOGS.lock().unwrap().clear();
        guard
    }

    fn index_rel<'mcx>(mcx: Mcx<'mcx>, oid: Oid, name: &str) -> Relation<'mcx> {
        let mut relname = NameData::default();
        relname.namestrcpy(name);
        let rd_rel = FormData_pg_class {
            relname,
            relnamespace: 2200,
            reltype: 0,
            relowner: 10,
            relam: 3580, // BRIN_AM_OID
            relfilenode: oid,
            reltablespace: 0,
            relpages: 0,
            reltuples: -1.0,
            relallvisible: 0,
            reltoastrelid: 0,
            relhasindex: false,
            relisshared: false,
            relpersistence: RELPERSISTENCE_PERMANENT,
            relkind: RELKIND_INDEX,
            relhassubclass: false,
            relrowsecurity: false,
            relispopulated: true,
            relreplident: b'n',
            relispartition: false,
            relfrozenxid: 0,
            relminmxid: 0,
        };
        let data = RelationData {
            rd_locator: Default::default(),
            rd_smgr: Default::default(),
            rd_id: oid,
            rd_backend: INVALID_PROC_NUMBER,
            rd_islocaltemp: false,
            rd_isvalid: core::cell::Cell::new(true),
            rd_createSubid: core::cell::Cell::new(0),
            rd_newRelfilelocatorSubid: core::cell::Cell::new(0),
            rd_firstRelfilelocatorSubid: core::cell::Cell::new(0),
            rd_droppedSubid: core::cell::Cell::new(0),
            rd_lockInfo: LockInfoData {
                lockRelId: LockRelId { relId: oid, dbId: 5 },
            },
            rd_rel,
            rd_att: Rc::new(TupleDescData {
                natts: 0,
                tdtypeid: 0,
                tdtypmod: -1,
                tdrefcount: -1,
                constr: None,
                compact_attrs: PgVec::new_in(mcx),
                attrs: PgVec::new_in(mcx),
            }),
            rd_index: None,
            rd_opcintype: PgVec::new_in(mcx),
            rd_opfamily: PgVec::new_in(mcx),
            rd_indoption: PgVec::new_in(mcx),
            rd_indcollation: PgVec::new_in(mcx),
            rd_options: None,
            pgstat_enabled: core::cell::Cell::new(false),
            pgstat_link: core::cell::Cell::new((0, core::ptr::null_mut())),
            rd_amcache: Default::default(),
            rd_amcache_hash: Default::default(),
            rd_amcache_gin: Default::default(),
            rd_amcache_spgist: Default::default(),
            rd_support: PgVec::new_in(mcx),
            rd_supportinfo: Default::default(),
            rd_opcoptions: Default::default(),
            rd_indexlist: Default::default(),
            rd_trigdesc: Default::default(),
            rd_hastriggers: false,
            rd_hasrules: false,
        };
        Relation::open(data, None)
    }

    // Recorded request: the work item carries the index oid and the previous
    // range's start block, and nothing is LOGged (the engagement witness for
    // the ported AutoVacuumRequestWork wiring).
    #[test]
    fn recorded_request_passes_oid_and_block_and_stays_silent() {
        let _guard = install_recorders();
        QUEUE_HAS_ROOM.store(true, Ordering::Relaxed);

        let cxt = MemoryContext::new_bump("autosummarize test");
        let rel = index_rel(cxt.mcx(), 50042, "summary_idx");
        request_range_summarization(&rel, 41).unwrap();

        assert_eq!(
            REQUESTS.lock().unwrap().as_slice(),
            &[(autovacuum_seams::AVW_BRIN_SUMMARIZE_RANGE, 50042, 41)]
        );
        assert!(LOGS.lock().unwrap().is_empty(), "recorded request must not LOG");
    }

    // Unrecorded request (work-item array full): C's LOG arm, byte-matching
    // brin.c's message.
    #[test]
    fn unrecorded_request_takes_c_log_arm() {
        let _guard = install_recorders();
        QUEUE_HAS_ROOM.store(false, Ordering::Relaxed);

        let cxt = MemoryContext::new_bump("autosummarize test");
        let rel = index_rel(cxt.mcx(), 50043, "summary_idx");
        request_range_summarization(&rel, 7).unwrap();
        QUEUE_HAS_ROOM.store(true, Ordering::Relaxed);

        assert_eq!(
            REQUESTS.lock().unwrap().as_slice(),
            &[(autovacuum_seams::AVW_BRIN_SUMMARIZE_RANGE, 50043, 7)]
        );
        let logs = LOGS.lock().unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].0, ::types_error::LOG);
        assert_eq!(
            logs[0].1,
            "request for BRIN range summarization for index \"summary_idx\" page 7 was not recorded"
        );
        assert_eq!(logs[0].2, Some(::types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED));
    }
}

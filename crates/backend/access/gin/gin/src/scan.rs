//! ginscan.c: scan setup.

use ::bufmgr_seams as bm;
use ::datum::Datum;
use ::gin_vocab::*;
use ::mcx::{Mcx, PgBox, PgVec};
use ::types_core::{InvalidBuffer, OffsetNumber};
use ::types_error::PgResult;
use ::types_rel::Relation;
use ::types_relscan::{relation_get_index_scan, IndexScanDescData, IndexScanOpaque};
use ::types_scan::scankey::{InvalidStrategy, ScanKeyData, StrategyNumber, SK_ISNULL};
use ::types_tuple::itemptr::FirstOffsetNumber;

use crate::insert::cached_gin_state;
use crate::util::{ginCompareEntries, ginGetStats};
use crate::vec_append;

/// ginbeginscan.
pub fn ginbeginscan<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    nkeys: i32,
    norderbys: i32,
) -> PgResult<IndexScanDescData<'mcx>> {
    debug_assert!(norderbys == 0);
    let state = cached_gin_state(rel)?;
    let so = mcx::alloc_in(
        mcx,
        GinScanOpaqueData {
            ginstate: Some(state),
            work: Some(GinScanWork::new()),
            isVoidRes: false,
        },
    )?;
    relation_get_index_scan(
        mcx,
        rel,
        nkeys,
        norderbys,
        IndexScanOpaque::Gin(so),
        xact::TransactionStartedDuringRecovery(),
    )
}

/// ginFillScanEntry: dedup + create.
#[allow(clippy::too_many_arguments)]
fn fill_scan_entry(
    state: &GinState,
    work: &mut GinScanState<'_>,
    attnum: OffsetNumber,
    strategy: StrategyNumber,
    search_mode: i32,
    query_key: Datum,
    query_orig: Datum,
    query_category: GinNullCategory,
    is_partial_match: bool,
    has_extra_data: bool,
) -> PgResult<u32> {
    if !has_extra_data && work.entries.len() < 100 {
        for (i, prev) in work.entries.iter().enumerate() {
            if !prev.hasExtraData
                && prev.isPartialMatch == is_partial_match
                && prev.strategy == strategy
                && prev.searchMode == search_mode
                && prev.attnum == attnum
                && ginCompareEntries(
                    state,
                    attnum,
                    prev.queryKey,
                    prev.queryCategory,
                    query_key,
                    query_category,
                )? == 0
            {
                return Ok(i as u32);
            }
        }
    }

    let kcx = work.kcx();
    // SAFETY: as above.
    let mut entry = GinScanEntryData::new(kcx)?;
    entry.queryKey = query_key;
    entry.queryOrig = query_orig;
    entry.queryCategory = query_category;
    entry.isPartialMatch = is_partial_match;
    entry.hasExtraData = has_extra_data;
    entry.strategy = strategy;
    entry.searchMode = search_mode;
    entry.attnum = attnum;
    entry.buffer = InvalidBuffer;
    item_pointer_set_min(&mut entry.curItem);
    entry.offset = 0;

    let id = work.entries.len() as u32;
    work.entries.push(mcx::alloc_in(kcx, entry)?);
    Ok(id)
}

fn add_hidden_entry(
    state: &GinState,
    work: &mut GinScanState<'_>,
    key_idx: usize,
    category: GinNullCategory,
) -> PgResult<()> {
    let (attnum, search_mode) = {
        let key = &work.keys[key_idx];
        (key.attnum, key.searchMode)
    };
    let id = fill_scan_entry(
        state,
        work,
        attnum,
        InvalidStrategy,
        search_mode,
        Datum::null(),
        Datum::null(),
        category,
        false,
        false,
    )?;
    let key = &mut work.keys[key_idx];
    key.nentries += 1;
    key.scanEntry.push(id);
    key.entryRes.push(GIN_FALSE);
    Ok(())
}

/// ginFillScanKey.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fill_scan_key<'scan>(
    state: &GinState,
    work: &mut GinScanState<'scan>,
    attnum: OffsetNumber,
    strategy: StrategyNumber,
    search_mode: i32,
    query: Datum,
    query_values: &[Datum],
    btree_orig: Datum,
    query_categories: &[GinNullCategory],
    partial_match: &[bool],
    jsp_ops: PgVec<'scan, JspGinOp>,
    map_item_operand: PgVec<'scan, i32>,
    trgm_graph: Option<TrgmPackedGraph>,
) -> PgResult<()> {
    let kcx = work.kcx();
    let n = query_values.len();
    // Which entries C's extractQuery hands a non-NULL extra_data[i]:
    // gin_extract_tsquery every operand (tsginidx.c:160), btree_gin its one
    // entry (btree_gin.c:79), gin_extract_query_trgm every trigram of a
    // regex with a graph (trgm_gin.c:128), jsonb path ops only [0] of a
    // palloc0 array (jsonb_gin.c:788-789); the rest leave it NULL.
    let extract_query = state.col(attnum).extract_query;
    let trgm_extra = trgm_graph.is_some();
    let has_extra_data = |i: usize| match extract_query {
        GinExtractQueryFn::Tsquery | GinExtractQueryFn::Btree(_) => true,
        GinExtractQueryFn::Trgm => trgm_extra,
        GinExtractQueryFn::JsonbPath => i == 0,
        _ => false,
    };

    let mut key = GinScanKeyData {
        nentries: n as u32,
        nuserentries: n as u32,
        scanEntry: mcx::vec_with_capacity_in(kcx, n + 1)?,
        requiredEntries: mcx::vec_new_in(kcx),
        additionalEntries: mcx::vec_new_in(kcx),
        entryRes: mcx::vec_with_capacity_in(kcx, n + 1)?,
        query,
        queryValues: {
            let mut v = mcx::vec_with_capacity_in(kcx, n)?;
            vec_append(&mut v, query_values)?;
            v
        },
        queryCategories: {
            let mut v = mcx::vec_with_capacity_in(kcx, n)?;
            vec_append(&mut v, query_categories)?;
            v
        },
        jspOps: jsp_ops,
        mapItemOperand: map_item_operand,
        trgmGraph: trgm_graph,
        strategy,
        searchMode: search_mode,
        attnum,
        excludeOnly: search_mode == GIN_SEARCH_MODE_ALL,
        curItem: {
            let mut ip = ::types_tuple::itemptr::ItemPointerData::invalid();
            item_pointer_set_min(&mut ip);
            ip
        },
        curItemMatches: false,
        recheckCurItem: false,
        isFinished: false,
    };
    for _ in 0..n {
        key.entryRes.push(GIN_FALSE);
    }

    let key_idx = work.keys.len();
    work.keys.push(key);

    let can_partial_match = state.col(attnum).can_partial_match;
    for i in 0..n {
        let id = fill_scan_entry(
            state,
            work,
            attnum,
            strategy,
            search_mode,
            query_values[i],
            btree_orig,
            query_categories[i],
            can_partial_match && partial_match.get(i).copied().unwrap_or(false),
            has_extra_data(i),
        )?;
        work.keys[key_idx].scanEntry.push(id);
    }

    if search_mode == GIN_SEARCH_MODE_INCLUDE_EMPTY {
        add_hidden_entry(state, work, key_idx, GIN_CAT_EMPTY_ITEM)?;
    } else if search_mode == GIN_SEARCH_MODE_EVERYTHING {
        add_hidden_entry(state, work, key_idx, GIN_CAT_EMPTY_QUERY)?;
    }
    Ok(())
}

/// ginFreeScanKeys.
pub(crate) fn ginFreeScanKeys(so: &mut GinScanOpaqueData) -> PgResult<()> {
    if let Some(mut work) = so.work.take() {
        work.with(|work| -> PgResult<()> {
    for entry in work.entries.iter() {
        if entry.buffer != InvalidBuffer {
            bm::release_buffer::call(entry.buffer)?;
        }
        // list / matchIterator / matchBitmap die with the work drop below.
    }
    Ok(())
    })?;
    }
    // ginscan.c:260 MemoryContextReset(so->keyCtx): both scan contexts
    // outlive the keys until ginendscan deletes them.
    so.work = Some(GinScanWork::new());
    Ok(())
}

/// ginNewScanKey.
pub(crate) fn ginNewScanKey(
    rel: &Relation<'_>,
    keys: &[ScanKeyData],
    so: &mut GinScanOpaqueData,
) -> PgResult<()> {
    let state = so.ginstate.expect("ginstate initialized at beginscan");
    let mut owner = GinScanWork::new();
    owner.with(|mut work| {
    so.isVoidRes = false;

    let mut has_null_query = false;
    let mut attr_has_normal_scan = [false; GIN_MAX_KEY_COLS];

    for skey in keys {
        if skey.sk_flags & SK_ISNULL != 0 {
            so.isVoidRes = true;
            break;
        }

        let kcx = work.kcx();
        let col = state.col(skey.sk_attno as OffsetNumber);
        let extracted = crate::secdef::call(col.secdef_proc(GIN_EXTRACTQUERY_PROC), || {
            crate::opclass::extract_query(
                kcx,
                col.extract_query,
                col.support_collation,
                skey.sk_argument,
                skey.sk_strategy,
            )
        })?;
        let crate::opclass::ExtractedQuery {
            entries: query_values,
            mut search_mode,
            jsp_ops,
            partial_match,
            map_item_operand,
            null_flags,
            trgm_graph,
            btree_orig,
        } = extracted;

        if !(GIN_SEARCH_MODE_DEFAULT..=GIN_SEARCH_MODE_ALL).contains(&search_mode) {
            search_mode = GIN_SEARCH_MODE_ALL;
        }
        if search_mode != GIN_SEARCH_MODE_DEFAULT {
            has_null_query = true;
        }
        if query_values.is_empty() && search_mode == GIN_SEARCH_MODE_DEFAULT {
            so.isVoidRes = true;
            break;
        }

        // C: nullFlags -> category codes; null keys need index placeholders.
        let categories: Vec<GinNullCategory> = (0..query_values.len())
            .map(|i| {
                if null_flags.get(i).copied().unwrap_or(false) {
                    has_null_query = true;
                    GIN_CAT_NULL_KEY
                } else {
                    GIN_CAT_NORM_KEY
                }
            })
            .collect();
        fill_scan_key(
            &state,
            &mut work,
            skey.sk_attno as OffsetNumber,
            skey.sk_strategy,
            search_mode,
            skey.sk_argument,
            query_values.as_slice(),
            btree_orig,
            &categories,
            partial_match.as_slice(),
            jsp_ops,
            map_item_operand,
            trgm_graph,
        )?;

        if search_mode != GIN_SEARCH_MODE_ALL {
            attr_has_normal_scan[skey.sk_attno as usize - 1] = true;
        }
    }

    // Second pass over GIN_SEARCH_MODE_ALL keys (excludeOnly resolution).
    let mut num_exclude_only = 0usize;
    for i in 0..work.keys.len() {
        if work.keys[i].searchMode != GIN_SEARCH_MODE_ALL {
            continue;
        }
        let a = work.keys[i].attnum as usize - 1;
        if !attr_has_normal_scan[a] {
            work.keys[i].excludeOnly = false;
            add_hidden_entry(&state, &mut work, i, GIN_CAT_EMPTY_QUERY)?;
            attr_has_normal_scan[a] = true;
        } else {
            num_exclude_only += 1;
        }
    }

    // excludeOnly keys must follow normal keys.
    if num_exclude_only > 0 {
        debug_assert!(num_exclude_only < work.keys.len());
        work.keys.sort_by_key(|k| k.excludeOnly);
    }

    if work.keys.is_empty() && !so.isVoidRes {
        has_null_query = true;
        let kcx2 = work.kcx();
        let empty_ops = mcx::vec_new_in(kcx2);
        let empty_map = mcx::vec_new_in(kcx2);
        fill_scan_key(
            &state,
            &mut work,
            FirstOffsetNumber,
            InvalidStrategy,
            GIN_SEARCH_MODE_EVERYTHING,
            Datum::null(),
            &[],
            Datum::null(),
            &[],
            &[],
            empty_ops,
            empty_map,
            None,
        )?;
    }

    // If the index is version 0, it may be missing null and placeholder
    // entries, which would render searches for nulls and full-index scans
    // unreliable.  Throw an error if so.
    if has_null_query && !so.isVoidRes {
        let stats = ginGetStats(rel)?;
        if stats.ginVersion < 1 {
            return Err(Box::new(
                ::types_error::PgError::error(
                    "old GIN indexes do not support whole-index scans nor searches for nulls",
                )
                .with_sqlstate(::types_error::ERRCODE_FEATURE_NOT_SUPPORTED)
                .with_hint(format!("To fix this, do REINDEX INDEX \"{}\".", rel.name())),
            ));
        }
    }

    Ok(())
    })?;
    so.work = Some(owner);
    // pgstat_count_index_scan is bumped by the caller (gingetbitmap) through
    // the scan's xs_pgstat link, as the other AMs do.
    Ok(())
}

/// ginrescan.
pub fn ginrescan(
    scan: &mut IndexScanDescData<'_>,
    scankey: Option<&[ScanKeyData]>,
) -> PgResult<()> {
    let IndexScanOpaque::Gin(so) = &mut scan.opaque else {
        non_gin_opaque()
    };
    ginFreeScanKeys(so)?;
    if let Some(keys) = scankey {
        if scan.numberOfKeys > 0 {
            debug_assert!(keys.len() == scan.numberOfKeys as usize);
            scan.keyData.clear();
            scan.keyData.extend(keys.iter().cloned());
        }
    }
    Ok(())
}

/// ginendscan.
pub fn ginendscan(scan: &mut IndexScanDescData<'_>) -> PgResult<()> {
    let IndexScanOpaque::Gin(so) = &mut scan.opaque else {
        non_gin_opaque()
    };
    ginFreeScanKeys(so)?;
    so.work = None;
    Ok(())
}

#[cold]
#[inline(never)]
pub(crate) fn non_gin_opaque() -> ! {
    panic!("gin entry point reached with a non-gin scan opaque")
}

const _: () = {
    let _: Option<PgBox<'static, GinScanOpaqueData>> = None;
};

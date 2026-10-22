//! The index-only row of an `IndexSource` (rung0-rows.md need 15, ruled: never fetch-always) = PostgreSQL's `IndexOnlyNext`: the
//! next TID from the AM, the heap fetched only where the visibility map does not vouch for the page (each fetch counted: EXPLAIN
//! ANALYZE's Heap Fetches, D574), the cells from the index tuple.
use gravity_abi::Datum as Word;
use mcx::Mcx;
use types_error::PgResult;
use types_rel::Relation;
use types_relscan::IndexScanDescData;
use types_scan::sdir::ScanDirection;
use types_slot::SlotData;
use types_snapshot::SnapshotData;
use types_tuple::ItemPointerGetBlockNumber;
use visibilitymap::VmBuffer;

const NAMEOID: types_core::Oid = 19;
pub const NAMEDATALEN: usize = 64;

/// The recipe's out attnos as index attribute numbers (1-based in the index's descriptor): `indkey` position + 1, and which
/// of them are `name` columns (btree stores a cstring: `next` decodes it into a `name` image, PostgreSQL's `StoreIndexTuple`).
/// D3453: a table column reads the first index column on it the AM can return (`returnable`, index position: PostgreSQL's
/// canreturn): createplan.c build_index_tlist marks a column the AM cannot return resjunk and setrefs.c
/// set_indexonlyscan_references strips it, so the scan's output reads a later column on the same table column (btree_gist's
/// gist_inet_ops beside inet_ops: GiST's image holds NULL for a column without a fetch function, gistFetchTuple). An empty
/// `returnable` is every column (a btree walk: nbtree returns them all).
pub fn index_attnums(index: &Relation<'_>, heap: &Relation<'_>, attnos: &[i16], returnable: &[bool]) -> PgResult<(Vec<i16>, Vec<bool>)> {
    let indkey = &index.rd_index.as_ref().ok_or_else(|| crate::pg_err("gravity: index without a form"))?.indkey;
    let mut v = Vec::with_capacity(attnos.len()); // alloc: Program
    let mut names = Vec::with_capacity(attnos.len()); // alloc: Program
    // bounded("small-fixed: one index-only column found among at most INDEX_MAX_KEYS index keys")
    for &a in attnos {
        let on = |j: usize| indkey[j] == a;
        let pos = (0..indkey.len())
            .find(|&j| on(j) && returnable.get(j).copied().unwrap_or(true))
            .ok_or_else(|| crate::pg_err("gravity: an index-only column outside the index's returnable key columns"))?;
        v.push(pos as i16 + 1);
        names.push(heap.rd_att.attrs.get(a as usize - 1).is_some_and(|f| f.atttypid == NAMEOID));
    }
    Ok((v, names))
}

/// D3453: which index columns (by position) the AM can return in an index-only scan's image (PostgreSQL's amcanreturn,
/// plancat.c's `canreturn`): every btree column; another AM's per column (GiST: a fetch function or no compress function;
/// SP-GiST: its config's canReturnData), asked once per bind.
pub fn returnable(mcx: Mcx<'_>, index: &Relation<'_>, btree: bool) -> PgResult<Vec<bool>> {
    let n = index.rd_index.as_ref().map_or(0, |f| f.indkey.len());
    if btree {
        return Ok(vec![true; n]); // alloc: Program
    }
    (0..n).map(|j| indexam_seams::index_can_return::call(mcx, index.rd_id, j as i32 + 1)).collect() // alloc: Program
}

/// D3453: per index column (by position), the column of the AM's index-only image that holds its value: itself where the AM
/// returns it, else the first returnable column on the same table column (the column PostgreSQL's recheckqual reads), else
/// itself. A lossy key on a column the AM cannot return is rechecked on that column's value (index_window.rs `only_passes`).
pub fn image_cols(index: &Relation<'_>, returnable: &[bool]) -> Vec<i16> {
    let indkey: &[i16] = index.rd_index.as_ref().map_or(&[][..], |f| &f.indkey[..]);
    let ok = |j: usize| returnable.get(j).copied().unwrap_or(true);
    (0..indkey.len())
        .map(|j| {
            let alt = (!ok(j) && indkey[j] > 0).then(|| (0..indkey.len()).find(|&i| indkey[i] == indkey[j] && ok(i))).flatten();
            alt.unwrap_or(j) as i16 + 1
        })
        .collect() // alloc: Program
}

/// `step` is the caller's row walk's: an entry whose heap tuple the snapshot rejects ticks it (a run of them is as long
/// as the index; IndexOnlyNext checks for interrupts per entry).
#[allow(clippy::too_many_arguments)]
pub fn next<'mcx>(step: &mut cancelpoll::Step, mcx: Mcx<'mcx>, scan: &mut IndexScanDescData<'mcx>, heap: &Relation<'mcx>, slot: &mut SlotData<'mcx>, snapshot: &SnapshotData<'mcx>, vm: &mut VmBuffer, dir: ScanDirection, index_attnums: &[i16], names: &[bool], name_bufs: &mut [u8], values: &mut [Word], isnull: &mut [u8], heap_fetches: &mut u64) -> PgResult<bool> {
    loop {
        let Some(tid) = indexam::index_getnext_tid(scan, dir)? else { return Ok(false) };
        let block = ItemPointerGetBlockNumber(&tid);
        let mut from_heap = false;
        if !tableam::table_index_fetch_all_visible(heap, &tid, |b| visibilitymap::vm_all_visible(heap, b, vm))? {
            // PostgreSQL's `InstrCountTuples2`: every heap visit counts, a tuple the snapshot then rejects too (D574). A plain
            // counter as PG's (D2900: no atomic per row); the source adds it to the recipe's shared count once a window
            *heap_fetches += 1;
            if !indexam::index_fetch_heap(mcx, scan, slot)? {
                step.tick().map_err(|_| crate::pg_err("gravity: an index-only walk stopped for its statement's cancel"))?;
                continue;
            }
            exectuples::exec_clear_tuple(slot, mcx);
            if scan.xs_heap_continue {
                return Err(crate::pg_err("gravity: index-only scan under a non-MVCC snapshot"));
            }
            from_heap = true;
        }
        let Some(row) = crate::ops::scan::raw::OnlyRow::of(scan) else { return Err(crate::pg_err("gravity: the index returned no tuple image")) };
        // bounded("key-columns: one cell per output column read from the index tuple, at most INDEX_MAX_KEYS (32) columns")
        for (k, &attnum) in index_attnums.iter().enumerate() {
            let mut null = false;
            // the AM's image (`xs_hitup` / `xs_itup`) lives until the next `amgettuple` on this descriptor; the window that
            // reads these words is consumed before the next call (a 1-row window per row)
            let d = row.attr(attnum as types_core::AttrNumber, &mut null);
            values[k] = d.as_u64();
            isnull[k] = null as u8;
            if names[k] && !null && row.is_index() {
                // the cstring into a NAMEDATALEN image (NUL-padded), live until this row's window is consumed
                let buf = &mut name_bufs[k * NAMEDATALEN..(k + 1) * NAMEDATALEN];
                buf.fill(0);
                // a btree name column's index datum is a NUL-terminated cstring inside `xs_itup`
                let src = crate::ops::scan::raw::index_cstr(d).to_bytes();
                let n = src.len().min(NAMEDATALEN - 1);
                buf[..n].copy_from_slice(&src[..n]);
                values[k] = buf.as_ptr() as u64;
            }
        }
        if !from_heap {
            predicate_seams::predicate_lock_page::call(heap, block, snapshot)?;
        }
        return Ok(true);
    }
}

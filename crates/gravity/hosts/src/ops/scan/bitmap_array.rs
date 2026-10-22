//! A bitmap index scan's `= ANY` keys on an access method without native array keys (D433; C `ExecIndexEvalArrayKeys` /
//! `ExecIndexAdvanceArrayKeys` under `MultiExecBitmapIndexScan`): only btree searches an SK_SEARCHARRAY key itself
//! (`amsearcharray`); for every other AM PostgreSQL scans the index once per combination of the arrays' elements and ORs
//! the scans into one TIDBitmap. A NULL array, an empty array, or an array of NULL elements only matches nothing (the
//! AM's operators are strict: a NULL element's scan adds no tid). Used by `BitmapSource` (one probe) and `Tree` (each
//! leaf). Created by M2-HEAP (sub-file of hosts v4/m2.rs).
use mcx::Mcx;
use types_error::PgResult;
use types_relscan::IndexScanDescData;
use types_scan::scankey::{ScanKeyData, SK_ISNULL, SK_SEARCHARRAY};

mod raw;
use raw::array_image;

/// One probe's bitmap into `tbm`: `keys` as given when `native` (btree) or no key is a search array, else one scan per
/// element combination with each array key's argument that element. Returns the tuples the index reported, summed over
/// the scans (`MultiExecBitmapIndexScan`'s nTuples, the Bitmap Index Scan's rows: M12 Row B).
pub fn getbitmap(mcx: Mcx<'_>, scan: &mut IndexScanDescData<'static>, keys: &[ScanKeyData], native: bool, tbm: &mut tidbitmap::TIDBitmap<'_>) -> PgResult<u64> {
    let arrays: Vec<usize> = if native { Vec::new() } else { (0..keys.len()).filter(|&i| keys[i].sk_flags & SK_SEARCHARRAY != 0).collect() }; // alloc: Exec
    if arrays.is_empty() {
        indexam::index_rescan(scan, if keys.is_empty() { None } else { Some(keys) }, None)?;
        return Ok(indexam::index_getbitmap(scan, tbm)? as u64);
    }
    if arrays.iter().any(|&i| keys[i].sk_flags & SK_ISNULL != 0) {
        return Ok(0);
    }
    // each array's image and its non-NULL elements (a by-reference element points into its image, kept to the end)
    let mut imgs = Vec::with_capacity(arrays.len()); // alloc: Exec
    // bounded("other: one image per search-array key of the probe, which the statement's index quals bound")
    for &i in &arrays {
        let img = array_image(keys[i].sk_argument.as_u64() as *const u8).ok_or_else(|| crate::pg_err("gravity: a bitmap scan's search array is still TOASTed (D4528)"))?;
        imgs.push(img); // alloc: Exec
    }
    let mut elems: Vec<Vec<datum::Datum>> = Vec::with_capacity(arrays.len()); // alloc: Exec
    // bounded("other: one array deconstructed per search-array key of the probe (the statement's index quals bound the keys); its elements are read once, as PG's ExecIndexEvalArrayKeys reads them with no check inside")
    for img in &imgs {
        let (len, byval, align) = gravity_fn::typesem::storage::len_byval_align(arrayfuncs::arr_elemtype(img))?;
        let (vals, nulls) = arrayfuncs::deconstruct_array(mcx, img, len as i32, byval, align as u8, true)?;
        let live: Vec<datum::Datum> = vals.iter().zip(nulls.iter()).filter(|(_, n)| !**n).map(|(v, _)| *v).collect(); // alloc: Exec
        if live.is_empty() {
            return Ok(0);
        }
        elems.push(live); // alloc: Exec
    }
    let mut k = keys.to_vec(); // alloc: Exec
    // bounded("other: one flag cleared per search-array key of the probe, which the statement's index quals bound")
    for &i in &arrays {
        k[i].sk_flags &= !SK_SEARCHARRAY;
    }
    // the odometer over the arrays' elements (ExecIndexAdvanceArrayKeys: the last array varies fastest)
    let (mut at, mut ntids) = (vec![0usize; arrays.len()], 0u64); // alloc: Exec
    let mut step = cancelpoll::step("bitmap getbitmap: one index scan per array element combination");
    loop {
        step.tick().map_err(|_| crate::pg_err("gravity: a bitmap array scan stopped for its statement's cancel"))?;
        // bounded("other: one argument per search-array key of the probe, which the statement's index quals bound")
        for (a, &i) in arrays.iter().enumerate() {
            k[i].sk_argument = elems[a][at[a]];
        }
        indexam::index_rescan(scan, Some(&k), None)?;
        ntids += indexam::index_getbitmap(scan, tbm)? as u64;
        let mut a = arrays.len();
        // bounded("other: the odometer's carry, at most one pass per search-array key of the probe")
        loop {
            if a == 0 {
                return Ok(ntids);
            }
            a -= 1;
            at[a] += 1;
            if at[a] < elems[a].len() {
                break;
            }
            at[a] = 0;
        }
    }
}

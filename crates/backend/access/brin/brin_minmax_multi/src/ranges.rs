use ::datum::Datum;
use ::fmgr::{function_call2_coll_in, FmgrInfo};
use ::mcx::{vec_with_capacity_in, Mcx, PgVec};
use ::types_brin::{BrinDesc, MinMaxMultiRanges};
use ::types_core::Oid;
use ::types_error::{PgError, PgResult, ERRCODE_DATA_CORRUPTED};
use ::types_tuple::varatt::{
    varatt_is_1b, varatt_is_1b_e, varsize_1b, varsize_4b, varsize_any, varsize_external, VARHDRSZ,
    VARHDRSZ_EXTERNAL,
};

use ::pg_qsort::pg_qsort_arg;
use crate::{
    minmax_multi_get_procinfo, minmax_multi_get_strategy_procinfo, BTEqualStrategyNumber,
    BTGreaterStrategyNumber, BTLessStrategyNumber, MINMAX_BUFFER_LOAD_FACTOR, PROCNUM_DISTANCE,
};

#[derive(Clone, Copy)]
pub struct ExpandedRange {
    pub minval: Datum,
    pub maxval: Datum,
    pub collapsed: bool,
}

#[derive(Clone, Copy)]
pub struct DistanceValue {
    pub index: i32,
    pub value: f64,
}

#[inline]
pub fn call_bool(
    mcx: Mcx<'_>,
    finfo: &mut FmgrInfo,
    colloid: Oid,
    a: Datum,
    b: Datum,
) -> PgResult<bool> {
    Ok(function_call2_coll_in(finfo, colloid, mcx, a, b)?.as_bool())
}

pub fn compare_values(
    mcx: Mcx<'_>,
    cmp: &mut FmgrInfo,
    colloid: Oid,
    a: Datum,
    b: Datum,
) -> PgResult<i32> {
    if call_bool(mcx, cmp, colloid, a, b)? {
        return Ok(-1);
    }
    if call_bool(mcx, cmp, colloid, b, a)? {
        return Ok(1);
    }
    Ok(0)
}

pub fn compare_expanded_ranges(
    mcx: Mcx<'_>,
    cmp: &mut FmgrInfo,
    colloid: Oid,
    ra: &ExpandedRange,
    rb: &ExpandedRange,
) -> PgResult<i32> {
    if call_bool(mcx, cmp, colloid, ra.minval, rb.minval)? {
        return Ok(-1);
    }
    if call_bool(mcx, cmp, colloid, rb.minval, ra.minval)? {
        return Ok(1);
    }
    if call_bool(mcx, cmp, colloid, ra.maxval, rb.maxval)? {
        return Ok(-1);
    }
    if call_bool(mcx, cmp, colloid, rb.maxval, ra.maxval)? {
        return Ok(1);
    }
    Ok(0)
}

pub fn minmax_multi_init(maxvalues: i32) -> PgResult<MinMaxMultiRanges> {
    debug_assert!(maxvalues > 0);
    Ok(MinMaxMultiRanges {
        typid: 0,
        colloid: 0,
        attno: 0,
        cmp: FmgrInfo::unresolved(),
        nranges: 0,
        nsorted: 0,
        nvalues: 0,
        maxvalues,
        target_maxvalues: 0,
        values: ::mcx::std_vec_from_elem("brin", Datum::null(), maxvalues as usize)?,
    })
}

pub fn range_deduplicate_values(mcx: Mcx<'_>, range: &mut MinMaxMultiRanges) -> PgResult<()> {
    if range.nsorted == range.nvalues {
        return Ok(());
    }

    let colloid = range.colloid;
    let mut cmp = range.cmp.clone();
    let start = (2 * range.nranges) as usize;

    pg_qsort_arg(
        &mut range.values[start..start + range.nvalues as usize],
        |a, b| compare_values(mcx, &mut cmp, colloid, *a, *b),
    )?;

    let mut n = 1usize;
    for i in 1..range.nvalues as usize {
        if compare_values(
            mcx,
            &mut cmp,
            colloid,
            range.values[start + i - 1],
            range.values[start + i],
        )? == 0
        {
            continue;
        }
        range.values[start + n] = range.values[start + i];
        n += 1;
    }

    range.nvalues = n as i32;
    range.nsorted = n as i32;

    assert_check_ranges(mcx, &mut cmp, colloid, range)?;
    Ok(())
}

const SERIALIZED_HEADER: usize = 20;

// SerializedRanges header layout: vl_len_ 0..4, typid 4..8, nranges 8..12,
// nvalues 12..16, maxvalues 16..20, data 20.. — the on-disk bytes must be
// byte-identical to C.
pub struct SerializedHeader {
    pub typid: Oid,
    pub nranges: i32,
    pub nvalues: i32,
    pub maxvalues: i32,
}

pub fn read_serialized_header(image: &[u8]) -> PgResult<SerializedHeader> {
    // A truncated on-disk summary is data corruption, not an out-of-bounds
    // read (the same guard brin_range_deserialize applies before trusting
    // the header's counts).
    if image.len() < SERIALIZED_HEADER {
        return Err(corrupt_range());
    }
    Ok(SerializedHeader {
        typid: u32::from_ne_bytes(image[4..8].try_into().unwrap()),
        nranges: i32::from_ne_bytes(image[8..12].try_into().unwrap()),
        nvalues: i32::from_ne_bytes(image[12..16].try_into().unwrap()),
        maxvalues: i32::from_ne_bytes(image[16..20].try_into().unwrap()),
    })
}

// SAFETY: p is a live NUL-terminated cstring.
unsafe fn cstring_len(p: *const u8) -> usize {
    let mut n = 0usize;
    while p.add(n).read() != 0 {
        n += 1;
    }
    n + 1
}

#[cold]
#[inline(never)]
fn corrupt_range() -> Box<PgError> {
    Box::new(
        PgError::error(
            "corrupted BRIN minmax-multi summary: value extends past range image".to_string(),
        )
        .with_sqlstate(ERRCODE_DATA_CORRUPTED),
    )
}

// Validated on-disk byte length of the serialized value at `data[p..]`, bounding
// every header/length read (and cstring strlen) by `data`. The C deserializer
// trusts these lengths (Assert-only); on attacker-craftable images we must
// reject an overrun with ERRCODE_DATA_CORRUPTED rather than read/copy OOB.
fn checked_value_len(data: &[u8], p: usize, typlen: i16) -> PgResult<usize> {
    let len = if typlen > 0 {
        typlen as usize
    } else if typlen == -1 {
        if p >= data.len() {
            return Err(corrupt_range());
        }
        // SAFETY: p < data.len(); reads past the first header byte are
        // bounds-checked against data before dereferencing.
        unsafe {
            let ptr = data.as_ptr().add(p);
            if varatt_is_1b_e(ptr) {
                if p + VARHDRSZ_EXTERNAL > data.len() {
                    return Err(corrupt_range());
                }
                varsize_external(ptr)
            } else if varatt_is_1b(ptr) {
                varsize_1b(ptr)
            } else {
                if p + VARHDRSZ > data.len() {
                    return Err(corrupt_range());
                }
                varsize_4b(ptr)
            }
        }
    } else {
        debug_assert!(typlen == -2);
        match data.get(p..).and_then(|s| s.iter().position(|&b| b == 0)) {
            Some(idx) => idx + 1,
            None => return Err(corrupt_range()),
        }
    };
    if len == 0 || p + len > data.len() {
        return Err(corrupt_range());
    }
    Ok(len)
}

/// brin_range_serialize: build the on-disk varlena image; the returned Datum
/// points at an `mcx` allocation.
pub fn brin_range_serialize<'mcx>(
    mcx: Mcx<'mcx>,
    range: &mut MinMaxMultiRanges,
) -> PgResult<Datum> {
    debug_assert!(range.nranges >= 0 && range.nsorted >= 0 && range.nvalues >= 0);
    debug_assert!(range.maxvalues > 0 && range.target_maxvalues > 0);
    debug_assert!(2 * range.nranges + range.nvalues <= range.target_maxvalues);
    debug_assert!(range.target_maxvalues <= range.maxvalues);
    debug_assert!(range.nvalues >= range.nsorted);

    range_deduplicate_values(mcx, range)?;

    let nvalues = (2 * range.nranges + range.nvalues) as usize;
    let typid = range.typid;
    let typbyval = lsyscache::get_typbyval(typid)?;
    let typlen = lsyscache::get_typlen(typid)?;

    let mut len = SERIALIZED_HEADER;
    for i in 0..nvalues {
        // SAFETY: by-ref values are live copies owned by the memtuple context.
        unsafe {
            if typlen == -1 {
                len += varsize_any(range.values[i].as_usize() as *const u8);
            } else if typlen == -2 {
                len += cstring_len(range.values[i].as_usize() as *const u8);
            } else {
                debug_assert!(typlen > 0);
                len += typlen as usize;
            }
        }
    }

    let mut out: PgVec<'mcx, u8> = vec_with_capacity_in(mcx, len)?;
    out.resize(len, 0);
    out[0..4].copy_from_slice(&(((len as u32) << 2).to_ne_bytes()));
    out[4..8].copy_from_slice(&typid.to_ne_bytes());
    out[8..12].copy_from_slice(&range.nranges.to_ne_bytes());
    out[12..16].copy_from_slice(&range.nvalues.to_ne_bytes());
    out[16..20].copy_from_slice(&range.target_maxvalues.to_ne_bytes());

    let mut off = SERIALIZED_HEADER;
    for i in 0..nvalues {
        let v = range.values[i];
        if typbyval {
            let word = v.as_u64().to_ne_bytes();
            out[off..off + typlen as usize].copy_from_slice(&word[..typlen as usize]);
            off += typlen as usize;
        } else {
            // SAFETY: live by-ref value, sized by its header / fixed typlen.
            unsafe {
                let p = v.as_usize() as *const u8;
                let sz = if typlen == -1 {
                    varsize_any(p)
                } else if typlen == -2 {
                    cstring_len(p)
                } else {
                    typlen as usize
                };
                out[off..off + sz].copy_from_slice(core::slice::from_raw_parts(p, sz));
                off += sz;
            }
        }
        debug_assert!(off <= len);
    }
    debug_assert!(off == len);

    Ok(Datum::from_usize(out.leak().as_ptr() as usize))
}

const fn maxalign(x: usize) -> usize {
    (x + 7) & !7
}

// tupmacs.h fetch_att: elog(ERROR, "unsupported byval length: %d") for a
// by-value type whose typlen is not 1/2/4/8 (XX000, catchable).
fn fetch_byval(bytes: &[u8], typlen: i16) -> PgResult<Datum> {
    Ok(match typlen {
        1 => Datum::from_char(bytes[0] as i8),
        2 => Datum::from_i16(i16::from_ne_bytes(bytes[0..2].try_into().unwrap())),
        4 => Datum::from_i32(i32::from_ne_bytes(bytes[0..4].try_into().unwrap())),
        8 => Datum::from_i64(i64::from_ne_bytes(bytes[0..8].try_into().unwrap())),
        other => {
            return Err(Box::new(PgError::error(format!("unsupported byval length: {other}"))))
        }
    })
}

/// brin_range_deserialize; by-ref values are copied into one `mcx` chunk with
/// MAXALIGN spacing (C's single-palloc layout).
pub fn brin_range_deserialize<'mcx>(
    mcx: Mcx<'mcx>,
    maxvalues: i32,
    image: &[u8],
) -> PgResult<MinMaxMultiRanges> {
    // The header itself must be present before we trust any of its counts.
    if image.len() < SERIALIZED_HEADER {
        return Err(corrupt_range());
    }
    let hdr = read_serialized_header(image)?;

    // nranges/nvalues/maxvalues come from the (attacker-craftable) image. Reject
    // negatives, count the total in i64 so 2*nranges+nvalues cannot wrap i32, and
    // require it to fit the allocated values array (target_maxvalues <= maxvalues).
    if hdr.nranges < 0 || hdr.nvalues < 0 || hdr.maxvalues <= 0 {
        return Err(corrupt_range());
    }
    let total = 2i64 * hdr.nranges as i64 + hdr.nvalues as i64;
    if total > hdr.maxvalues as i64 || hdr.maxvalues > maxvalues {
        return Err(corrupt_range());
    }
    let nvalues = total as usize;

    let mut range = minmax_multi_init(maxvalues)?;
    range.nranges = hdr.nranges;
    range.nvalues = hdr.nvalues;
    range.nsorted = hdr.nvalues;
    range.target_maxvalues = hdr.maxvalues;
    range.typid = hdr.typid;

    let typbyval = lsyscache::get_typbyval(hdr.typid)?;
    let typlen = lsyscache::get_typlen(hdr.typid)?;

    let data = &image[SERIALIZED_HEADER..];

    let mut datalen = 0usize;
    if !typbyval {
        let mut p = 0usize;
        for _ in 0..nvalues {
            let sz = checked_value_len(data, p, typlen)?;
            datalen += maxalign(sz);
            p += sz;
        }
    }

    let mut chunk: *mut u8 = core::ptr::null_mut();
    if datalen > 0 {
        let mut buf: PgVec<'mcx, u64> = vec_with_capacity_in(mcx, datalen / 8)?;
        buf.resize(datalen / 8, 0);
        chunk = buf.leak().as_mut_ptr().cast::<u8>();
    }

    let mut p = 0usize;
    let mut dataoff = 0usize;
    for i in 0..nvalues {
        if typbyval {
            // Bound the fixed-width by-value read against the image.
            if typlen <= 0 || p + typlen as usize > data.len() {
                return Err(corrupt_range());
            }
            range.values[i] = fetch_byval(&data[p..], typlen)?;
            p += typlen as usize;
        } else {
            let sz = checked_value_len(data, p, typlen)?;
            // SAFETY: checked_value_len guarantees p + sz <= data.len(); the
            // pre-scan computed datalen from the same sequence of sizes, so
            // dataoff + sz <= datalen bytes of chunk.
            unsafe {
                let dst = chunk.add(dataoff);
                core::ptr::copy_nonoverlapping(data.as_ptr().add(p), dst, sz);
                range.values[i] = Datum::from_usize(dst as usize);
            }
            dataoff += maxalign(sz);
            p += sz;
        }
    }

    Ok(range)
}

pub fn has_matching_range(
    mcx: Mcx<'_>,
    bdesc: &BrinDesc<'_>,
    colloid: Oid,
    ranges: &MinMaxMultiRanges,
    newval: Datum,
    attno: u16,
    typid: Oid,
) -> PgResult<bool> {
    if ranges.nranges == 0 {
        return Ok(false);
    }

    let minvalue = ranges.values[0];
    let maxvalue = ranges.values[(2 * ranges.nranges - 1) as usize];

    let mut lt = minmax_multi_get_strategy_procinfo(bdesc, attno, typid, BTLessStrategyNumber)?;
    if call_bool(mcx, &mut lt, colloid, newval, minvalue)? {
        return Ok(false);
    }

    let mut gt = minmax_multi_get_strategy_procinfo(bdesc, attno, typid, BTGreaterStrategyNumber)?;
    if call_bool(mcx, &mut gt, colloid, newval, maxvalue)? {
        return Ok(false);
    }

    let mut start: i32 = 0;
    let mut end: i32 = ranges.nranges - 1;
    loop {
        if start > end {
            return Ok(false);
        }
        let midpoint = (start + end) / 2;

        let minvalue = ranges.values[(2 * midpoint) as usize];
        let maxvalue = ranges.values[(2 * midpoint + 1) as usize];

        if call_bool(mcx, &mut lt, colloid, newval, minvalue)? {
            end = midpoint - 1;
            continue;
        }
        if call_bool(mcx, &mut gt, colloid, newval, maxvalue)? {
            start = midpoint + 1;
            continue;
        }
        return Ok(true);
    }
}

pub fn range_contains_value(
    mcx: Mcx<'_>,
    bdesc: &BrinDesc<'_>,
    colloid: Oid,
    attno: u16,
    typid: Oid,
    ranges: &mut MinMaxMultiRanges,
    newval: Datum,
    full: bool,
) -> PgResult<bool> {
    if has_matching_range(mcx, bdesc, colloid, ranges, newval, attno, typid)? {
        return Ok(true);
    }

    let mut eq = minmax_multi_get_strategy_procinfo(bdesc, attno, typid, BTEqualStrategyNumber)?;

    if ranges.nsorted >= 16 {
        let base = (2 * ranges.nranges) as usize;
        let sorted = &ranges.values[base..base + ranges.nsorted as usize];
        if bsearch_values(mcx, &mut ranges.cmp, ranges.colloid, sorted, newval)? {
            return Ok(true);
        }
    } else {
        for i in
            (2 * ranges.nranges) as usize..(2 * ranges.nranges + ranges.nsorted) as usize
        {
            if call_bool(mcx, &mut eq, colloid, newval, ranges.values[i])? {
                return Ok(true);
            }
        }
    }

    if !full {
        return Ok(false);
    }

    for i in (2 * ranges.nranges + ranges.nsorted) as usize
        ..(2 * ranges.nranges + ranges.nvalues) as usize
    {
        if call_bool(mcx, &mut eq, colloid, newval, ranges.values[i])? {
            return Ok(true);
        }
    }

    Ok(false)
}

fn bsearch_values(
    mcx: Mcx<'_>,
    cmp: &mut FmgrInfo,
    colloid: Oid,
    sorted: &[Datum],
    key: Datum,
) -> PgResult<bool> {
    Ok(bsearch_arg(sorted.len(), |i| compare_values(mcx, cmp, colloid, key, sorted[i]))?.is_some())
}

/// src/port/bsearch_arg.c: the probe order (hence the comparator's call
/// sequence, observable through a side-effecting operator) is C's, not a
/// lo/hi midpoint search.
pub(crate) fn bsearch_arg(
    nmemb: usize,
    mut compar: impl FnMut(usize) -> PgResult<i32>,
) -> PgResult<Option<usize>> {
    let mut base = 0usize;
    let mut lim = nmemb;
    while lim != 0 {
        let p = base + (lim >> 1);
        let c = compar(p)?;
        if c == 0 {
            return Ok(Some(p));
        }
        if c > 0 {
            base = p + 1;
            lim -= 1;
        }
        lim >>= 1;
    }
    Ok(None)
}

pub fn fill_expanded_ranges(
    eranges: &mut [ExpandedRange],
    neranges: usize,
    ranges: &MinMaxMultiRanges,
) {
    debug_assert!(neranges == (ranges.nranges + ranges.nvalues) as usize);

    let mut idx = 0usize;
    for i in 0..ranges.nranges as usize {
        eranges[idx] = ExpandedRange {
            minval: ranges.values[2 * i],
            maxval: ranges.values[2 * i + 1],
            collapsed: false,
        };
        idx += 1;
    }
    for i in 0..ranges.nvalues as usize {
        let v = ranges.values[(2 * ranges.nranges) as usize + i];
        eranges[idx] = ExpandedRange { minval: v, maxval: v, collapsed: true };
        idx += 1;
    }
    debug_assert!(idx == neranges);
}

pub fn sort_expanded_ranges(
    mcx: Mcx<'_>,
    cmp: &mut FmgrInfo,
    colloid: Oid,
    eranges: &mut [ExpandedRange],
) -> PgResult<usize> {
    let neranges = eranges.len();
    debug_assert!(neranges > 0);

    pg_qsort_arg(eranges, |a, b| compare_expanded_ranges(mcx, cmp, colloid, a, b))?;

    let mut n = 1usize;
    for i in 1..neranges {
        let a = eranges[i - 1];
        let b = eranges[i];
        if compare_expanded_ranges(mcx, cmp, colloid, &a, &b)? == 0 {
            continue;
        }
        if i != n {
            eranges[n] = eranges[i];
        }
        n += 1;
    }
    debug_assert!(n > 0 && n <= neranges);
    Ok(n)
}

pub fn merge_overlapping_ranges(
    mcx: Mcx<'_>,
    cmp: &mut FmgrInfo,
    colloid: Oid,
    eranges: &mut [ExpandedRange],
    mut neranges: usize,
) -> PgResult<usize> {
    let mut idx = 0usize;
    while idx + 1 < neranges {
        if call_bool(mcx, cmp, colloid, eranges[idx].maxval, eranges[idx + 1].minval)? {
            idx += 1;
            continue;
        }

        if call_bool(mcx, cmp, colloid, eranges[idx].maxval, eranges[idx + 1].maxval)? {
            eranges[idx].maxval = eranges[idx + 1].maxval;
        }
        eranges[idx].collapsed = false;

        eranges.copy_within(idx + 2..neranges, idx + 1);
        neranges -= 1;
    }
    Ok(neranges)
}

pub fn build_distances<'mcx>(
    mcx: Mcx<'mcx>,
    distance_fn: &mut FmgrInfo,
    colloid: Oid,
    eranges: &[ExpandedRange],
) -> PgResult<Option<PgVec<'mcx, DistanceValue>>> {
    let neranges = eranges.len();
    debug_assert!(neranges > 0);
    if neranges == 1 {
        return Ok(None);
    }

    let ndistances = neranges - 1;
    let mut distances: PgVec<'mcx, DistanceValue> = vec_with_capacity_in(mcx, ndistances)?;
    for i in 0..ndistances {
        let r = function_call2_coll_in(
            distance_fn,
            colloid,
            mcx,
            eranges[i].maxval,
            eranges[i + 1].minval,
        )?;
        distances.push(DistanceValue { index: i as i32, value: r.as_f64() });
    }

    // compare_distances: descending by value. C's qsort call here is pg_qsort
    // (port.h:478 remaps qsort to pg_qsort), so pg_qsort_arg tie order is the
    // parity requirement — which equal-distance gaps survive reduction.
    pg_qsort_arg(&mut distances, |da, db| -> PgResult<i32> {
        Ok(if da.value < db.value {
            1
        } else if da.value > db.value {
            -1
        } else {
            0
        })
    })?;

    Ok(Some(distances))
}

pub fn build_expanded_ranges<'mcx>(
    mcx: Mcx<'mcx>,
    cmp: &mut FmgrInfo,
    colloid: Oid,
    ranges: &MinMaxMultiRanges,
) -> PgResult<(PgVec<'mcx, ExpandedRange>, usize)> {
    let neranges = (ranges.nranges + ranges.nvalues) as usize;
    let mut eranges: PgVec<'mcx, ExpandedRange> = vec_with_capacity_in(mcx, neranges)?;
    ::mcx::pgvec_resize(
        &mut eranges,
        neranges,
        ExpandedRange { minval: Datum::null(), maxval: Datum::null(), collapsed: false },
    )?;
    fill_expanded_ranges(&mut eranges, neranges, ranges);
    let n = sort_expanded_ranges(mcx, cmp, colloid, &mut eranges)?;
    Ok((eranges, n))
}

pub fn count_values(cranges: &[ExpandedRange]) -> i32 {
    let mut count = 0;
    for r in cranges {
        count += if r.collapsed { 1 } else { 2 };
    }
    count
}

pub fn reduce_expanded_ranges<'mcx>(
    mcx: Mcx<'mcx>,
    eranges: &mut [ExpandedRange],
    neranges: usize,
    distances: Option<&[DistanceValue]>,
    max_values: i32,
    cmp: &mut FmgrInfo,
    colloid: Oid,
) -> PgResult<usize> {
    let ndistances = neranges as i32 - 1;
    let keep = max_values / 2 - 1;

    if keep >= ndistances {
        return Ok(neranges);
    }

    let mut values: PgVec<'mcx, Datum> = vec_with_capacity_in(mcx, max_values as usize)?;
    values.push(eranges[0].minval);
    values.push(eranges[neranges - 1].maxval);

    let distances = distances.expect("ndistances > 0 implies distances");
    for d in distances.iter().take(keep as usize) {
        let index = d.index as usize;
        debug_assert!(index + 1 < neranges);
        values.push(eranges[index].maxval);
        values.push(eranges[index + 1].minval);
        debug_assert!(values.len() <= max_values as usize);
    }
    debug_assert!(values.len() % 2 == 0);

    pg_qsort_arg(&mut values, |a, b| compare_values(mcx, cmp, colloid, *a, *b))?;

    let nvalues = values.len();
    for i in 0..nvalues / 2 {
        let collapsed =
            compare_values(mcx, cmp, colloid, values[2 * i], values[2 * i + 1])? == 0;
        eranges[i] = ExpandedRange {
            minval: values[2 * i],
            maxval: values[2 * i + 1],
            collapsed,
        };
    }

    Ok(nvalues / 2)
}

pub fn store_expanded_ranges(
    ranges: &mut MinMaxMultiRanges,
    eranges: &[ExpandedRange],
) {
    let mut idx = 0usize;

    ranges.nranges = 0;
    for r in eranges {
        if !r.collapsed {
            ranges.values[idx] = r.minval;
            ranges.values[idx + 1] = r.maxval;
            idx += 2;
            ranges.nranges += 1;
        }
    }

    ranges.nvalues = 0;
    for r in eranges {
        if r.collapsed {
            ranges.values[idx] = r.minval;
            idx += 1;
            ranges.nvalues += 1;
        }
    }

    ranges.nsorted = ranges.nvalues;

    debug_assert!(count_values(eranges) == 2 * ranges.nranges + ranges.nvalues);
    debug_assert!(2 * ranges.nranges + ranges.nvalues <= ranges.maxvalues);
}

pub fn ensure_free_space_in_buffer(
    mcx: Mcx<'_>,
    bdesc: &BrinDesc<'_>,
    colloid: Oid,
    attno: u16,
    typid: Oid,
    range: &mut MinMaxMultiRanges,
) -> PgResult<bool> {
    if 2 * range.nranges + range.nvalues < range.maxvalues {
        return Ok(false);
    }

    let mut cmp = minmax_multi_get_strategy_procinfo(bdesc, attno, typid, BTLessStrategyNumber)?;

    range_deduplicate_values(mcx, range)?;

    if (2 * range.nranges + range.nvalues) as f64
        <= range.maxvalues as f64 * MINMAX_BUFFER_LOAD_FACTOR
    {
        return Ok(true);
    }

    // Scratch context per compaction (C's local AllocSet): distance/comparator
    // calls may allocate heavily and this can run many times per page range.
    {
        let ctx = ::mcx::MemoryContext::new_wholesale("minmax-multi context");
        let cmcx = ctx.mcx();

        let (mut eranges, mut neranges) = build_expanded_ranges(cmcx, &mut cmp, colloid, range)?;
        assert_check_expanded_ranges(cmcx, bdesc, colloid, attno, typid, &eranges[..neranges])?;

        let mut distance_fn = minmax_multi_get_procinfo(bdesc, attno, PROCNUM_DISTANCE)?;
        let distances = build_distances(cmcx, &mut distance_fn, colloid, &eranges[..neranges])?;

        neranges = reduce_expanded_ranges(
            cmcx,
            &mut eranges,
            neranges,
            distances.as_deref(),
            (range.maxvalues as f64 * MINMAX_BUFFER_LOAD_FACTOR) as i32,
            &mut cmp,
            colloid,
        )?;
        assert_check_expanded_ranges(cmcx, bdesc, colloid, attno, typid, &eranges[..neranges])?;

        debug_assert!(
            count_values(&eranges[..neranges]) as f64
                <= range.maxvalues as f64 * MINMAX_BUFFER_LOAD_FACTOR
        );

        store_expanded_ranges(range, &eranges[..neranges]);
    }
    assert_check_ranges(mcx, &mut cmp, colloid, range)?;

    Ok(true)
}

pub fn range_add_value(
    mcx: Mcx<'_>,
    bdesc: &BrinDesc<'_>,
    colloid: Oid,
    attno: u16,
    typid: Oid,
    attbyval: bool,
    attlen: i16,
    ranges: &mut MinMaxMultiRanges,
    newval: Datum,
) -> PgResult<bool> {
    let mut cmp = minmax_multi_get_strategy_procinfo(bdesc, attno, typid, BTLessStrategyNumber)?;

    assert_check_ranges(mcx, &mut cmp, colloid, ranges)?;

    let modified = ensure_free_space_in_buffer(mcx, bdesc, colloid, attno, typid, ranges)?;

    if range_contains_value(mcx, bdesc, colloid, attno, typid, ranges, newval, false)? {
        return Ok(modified);
    }

    let newval = ::adt_scalar::datum_ops::datum_copy(mcx, newval, attbyval, attlen)?;

    ranges.values[(2 * ranges.nranges + ranges.nvalues) as usize] = newval;
    ranges.nvalues += 1;

    if ranges.nvalues == 1 {
        ranges.nsorted = 1;
    }

    assert_check_ranges(mcx, &mut cmp, colloid, ranges)?;
    debug_assert!(range_contains_value(
        mcx, bdesc, colloid, attno, typid, ranges, newval, true
    )?);

    Ok(true)
}

pub fn compactify_ranges(
    mcx: Mcx<'_>,
    bdesc: &BrinDesc<'_>,
    ranges: &mut MinMaxMultiRanges,
    max_values: i32,
) -> PgResult<()> {
    if ranges.nranges * 2 + ranges.nvalues <= max_values && ranges.nsorted == ranges.nvalues {
        return Ok(());
    }

    let colloid = ranges.colloid;
    let mut cmp = minmax_multi_get_strategy_procinfo(
        bdesc,
        ranges.attno,
        ranges.typid,
        BTLessStrategyNumber,
    )?;
    let mut distance_fn = minmax_multi_get_procinfo(bdesc, ranges.attno, PROCNUM_DISTANCE)?;

    {
        let ctx = ::mcx::MemoryContext::new_wholesale("minmax-multi context");
        let cmcx = ctx.mcx();

        let (mut eranges, mut neranges) = build_expanded_ranges(cmcx, &mut cmp, colloid, ranges)?;
        let distances = build_distances(cmcx, &mut distance_fn, colloid, &eranges[..neranges])?;

        neranges = reduce_expanded_ranges(
            cmcx,
            &mut eranges,
            neranges,
            distances.as_deref(),
            max_values,
            &mut cmp,
            colloid,
        )?;

        debug_assert!(count_values(&eranges[..neranges]) <= max_values);

        store_expanded_ranges(ranges, &eranges[..neranges]);
    }
    assert_check_ranges(mcx, &mut cmp, colloid, ranges)?;
    Ok(())
}

fn assert_array_order(
    mcx: Mcx<'_>,
    cmp: &mut FmgrInfo,
    colloid: Oid,
    values: &[Datum],
) -> PgResult<()> {
    for i in 1..values.len() {
        debug_assert!(call_bool(mcx, cmp, colloid, values[i - 1], values[i])?);
    }
    Ok(())
}

pub fn assert_check_ranges(
    mcx: Mcx<'_>,
    cmp: &mut FmgrInfo,
    colloid: Oid,
    ranges: &MinMaxMultiRanges,
) -> PgResult<()> {
    if !cfg!(debug_assertions) {
        return Ok(());
    }
    debug_assert!(ranges.nranges >= 0 && ranges.nsorted >= 0);
    debug_assert!(ranges.nvalues >= ranges.nsorted);
    debug_assert!(ranges.maxvalues >= 2 * ranges.nranges + ranges.nvalues);
    debug_assert!(ranges.typid != 0);

    let base = (2 * ranges.nranges) as usize;
    assert_array_order(mcx, cmp, colloid, &ranges.values[..base])?;
    assert_array_order(mcx, cmp, colloid, &ranges.values[base..base + ranges.nsorted as usize])?;

    if ranges.nranges > 0 {
        for i in 0..ranges.nvalues as usize {
            let value = ranges.values[base + i];
            if call_bool(mcx, cmp, colloid, value, ranges.values[0])? {
                continue;
            }
            if call_bool(mcx, cmp, colloid, ranges.values[base - 1], value)? {
                continue;
            }
            let mut start: i32 = 0;
            let mut end: i32 = ranges.nranges - 1;
            while start <= end {
                let midpoint = (start + end) / 2;
                if call_bool(mcx, cmp, colloid, value, ranges.values[(2 * midpoint) as usize])? {
                    end = midpoint - 1;
                    continue;
                }
                if call_bool(
                    mcx,
                    cmp,
                    colloid,
                    ranges.values[(2 * midpoint + 1) as usize],
                    value,
                )? {
                    start = midpoint + 1;
                    continue;
                }
                debug_assert!(false, "point value covered by a range");
                break;
            }
        }
    }

    if ranges.nsorted > 0 {
        let sorted = &ranges.values[base..base + ranges.nsorted as usize];
        for i in ranges.nsorted as usize..ranges.nvalues as usize {
            debug_assert!(!bsearch_values(
                mcx,
                &mut ranges.cmp.clone(),
                ranges.colloid,
                sorted,
                ranges.values[base + i]
            )?);
        }
    }
    Ok(())
}

pub fn assert_check_expanded_ranges(
    mcx: Mcx<'_>,
    bdesc: &BrinDesc<'_>,
    colloid: Oid,
    attno: u16,
    typid: Oid,
    eranges: &[ExpandedRange],
) -> PgResult<()> {
    if !cfg!(debug_assertions) {
        return Ok(());
    }
    let mut eq = minmax_multi_get_strategy_procinfo(bdesc, attno, typid, BTEqualStrategyNumber)?;
    let mut lt = minmax_multi_get_strategy_procinfo(bdesc, attno, typid, BTLessStrategyNumber)?;

    for r in eranges {
        let ok = if r.collapsed {
            call_bool(mcx, &mut eq, colloid, r.minval, r.maxval)?
        } else {
            call_bool(mcx, &mut lt, colloid, r.minval, r.maxval)?
        };
        debug_assert!(ok);
    }
    for i in 1..eranges.len() {
        debug_assert!(call_bool(mcx, &mut lt, colloid, eranges[i - 1].maxval, eranges[i].minval)?);
    }
    Ok(())
}

#[cfg(test)]
mod deserialize_tests {
    use super::checked_value_len;

    // A crafted 4B varlena declaring ~256 MB inside a tiny image must be
    // rejected rather than driving an OOB read/copy (idx-50).
    #[test]
    fn oversized_varlena_is_rejected() {
        let word: u32 = 0x1000_0000u32 << 2; // declared VARSIZE = 256 MB
        let mut data = word.to_ne_bytes().to_vec();
        data.extend_from_slice(&[0u8; 4]);
        assert!(checked_value_len(&data, 0, -1).is_err());
    }

    #[test]
    fn fixed_length_overrun_is_rejected() {
        let data = [0u8; 4];
        assert!(checked_value_len(&data, 0, 8).is_err());
        assert_eq!(checked_value_len(&data, 0, 4).unwrap(), 4);
    }

    #[test]
    fn unterminated_cstring_is_rejected() {
        let data = [b'x', b'y'];
        assert!(checked_value_len(&data, 0, -2).is_err());
        let ok = [b'x', 0u8];
        assert_eq!(checked_value_len(&ok, 0, -2).unwrap(), 2);
    }

    #[test]
    fn well_formed_varlena_is_accepted() {
        let word: u32 = 8u32 << 2; // VARSIZE = 8
        let mut data = word.to_ne_bytes().to_vec();
        data.extend_from_slice(&[1u8; 4]);
        assert_eq!(checked_value_len(&data, 0, -1).unwrap(), 8);
    }

    // A summary image shorter than SerializedRanges' fixed header (a truncated
    // on-disk tuple) must be reported as corruption, never indexed past its
    // end (audit row brin_minmax_multi-69f5e9d7).
    #[test]
    fn truncated_header_image_does_not_panic() {
        let image = [0u8; 10];
        let outcome = std::panic::catch_unwind(|| {
            let _ = super::read_serialized_header(&image);
        });
        assert!(outcome.is_ok(), "read_serialized_header must not panic on a short image");
    }

    // tupmacs.h fetch_att: elog(ERROR, "unsupported byval length: %d") for a
    // by-value typlen outside {1,2,4,8} (audit row brin_minmax_multi-82042a2c).
    #[test]
    fn unsupported_byval_length_does_not_panic() {
        let bytes = [0u8; 8];
        let outcome = std::panic::catch_unwind(|| {
            let _ = super::fetch_byval(&bytes, 3);
        });
        assert!(outcome.is_ok(), "fetch_byval must not panic on an unsupported typlen");
    }

    #[test]
    fn truncated_header_image_is_data_corruption() {
        let image = [0u8; 10];
        let err = super::read_serialized_header(&image).err().expect("short image");
        assert_eq!(err.sqlstate(), ::types_error::ERRCODE_DATA_CORRUPTED);
        let ok = [0u8; 20];
        assert_eq!(super::read_serialized_header(&ok).unwrap().maxvalues, 0);
    }

    #[test]
    fn unsupported_byval_length_is_c_elog_error() {
        let bytes = [0u8; 8];
        let err = super::fetch_byval(&bytes, 3).expect_err("typlen 3");
        assert_eq!(err.message, "unsupported byval length: 3");
        assert_eq!(err.sqlstate(), ::types_error::ERRCODE_INTERNAL_ERROR);
        assert_eq!(super::fetch_byval(&bytes, 4).unwrap().as_i32(), 0);
    }
}

#[cfg(test)]
mod bsearch_tests {
    use super::bsearch_arg;

    // sorted[i] == i + 1; returns (hit, probe sequence).
    fn probes(n: usize, key: i32) -> (Option<usize>, Vec<usize>) {
        let mut seen = Vec::new();
        let hit = bsearch_arg(n, |i| {
            seen.push(i);
            Ok(key.cmp(&(i as i32 + 1)) as i32)
        })
        .unwrap();
        (hit, seen)
    }

    // bsearch_arg.c probes index lim>>1 from the current base: with 16 sorted
    // points and key 9 C compares once at index 8 (the audit's midpoint search
    // probed 7, 11, 9, 8).
    #[test]
    fn probe_order_matches_c_bsearch_arg() {
        assert_eq!(probes(16, 9), (Some(8), vec![8]));
        assert_eq!(probes(16, 1), (Some(0), vec![8, 4, 2, 1, 0]));
        assert_eq!(probes(16, 16), (Some(15), vec![8, 12, 14, 15]));
        assert_eq!(probes(16, 17), (None, vec![8, 12, 14, 15]));
        assert_eq!(probes(16, 0), (None, vec![8, 4, 2, 1, 0]));
        assert_eq!(probes(0, 1), (None, vec![]));
    }
}

//! pgrust-only (PostgreSQL 18's jsonb has no typanalyze): under `pgrust.jsonb_path_stats` a jsonb column runs C's
//! std compute, then gains one path slot over the same sample rows (heap and pgrc alike). Pass 1 finds the
//! top 10 x target object-key paths by Lossy Counting (keys are unique, so a path occurs once per document);
//! pass 2 counts their value kinds, `->>` texts (numeric 1 and string "1" merge, as `->>` merges them), C's
//! Duj1 ndistinct, MCVs and a histogram.

use datum::Datum;
use mcx::{Mcx, MemoryContext, PgFxHashMap, PgVec};
use types_core::{InvalidOid, Oid};
use types_error::PgResult;

use adt_jsonb::container::{
    container_is_object, container_is_scalar, container_size, fill_item, get_jsonb_length,
    get_jsonb_offset, JsonbItem,
};
use adt_jsonb_pathstats as fmt;

use crate::{ComputeStats, FetchSource, StdCompute, VacAttrStats, STATISTIC_NUM_SLOTS};

pub(crate) const JSONBOID: Oid = 3802;
const BYTEAOID: Oid = 17;

/// A deeper object counts as one `object` value.
const MAX_DEPTH: usize = 8;
const DOC_BYTES_CAP: usize = 100 * 1024;
/// Visits per pass per unit of target; doc_cap = this over the walked documents, clamped.
const VISITS_PER_TARGET: u64 = 20_000;
const DOC_CAP_MIN: u32 = 128;
const DOC_CAP_MAX: u32 = 1024;
/// Detoasting dominates on large documents: past this many bytes per pass per unit of target, every
/// stride-th document is walked (the same ones in both passes).
const WALK_BYTES_PER_TARGET: u64 = 640 * 1024;

fn walk_stride(doc_bytes: u64, target: i32) -> u32 {
    let budget = WALK_BYTES_PER_TARGET * target.max(1) as u64;
    doc_bytes.div_ceil(budget).clamp(1, u32::MAX as u64) as u32
}
const BLOB_CAP: usize = 256 * 1024;
/// C's WIDTH_THRESHOLD: longer keys and `->>` texts are not stored.
const WIDTH_THRESHOLD: usize = 1024;

pub(crate) fn enabled() -> bool {
    guc_tables::backing::pgrust_jsonb_path_stats()
}

pub(crate) fn setup(stats: &mut VacAttrStats<'_>) -> PgResult<bool> {
    if !crate::std_typanalyze(stats)? {
        return Ok(false);
    }
    let std = match stats.compute {
        ComputeStats::Scalar => StdCompute::Scalar,
        ComputeStats::Distinct => StdCompute::Distinct,
        _ => StdCompute::Trivial,
    };
    stats.compute = ComputeStats::Jsonb { std };
    Ok(true)
}

pub(crate) fn std_compute_of(c: &ComputeStats) -> Option<ComputeStats> {
    match c {
        ComputeStats::Jsonb { std: StdCompute::Scalar } => Some(ComputeStats::Scalar),
        ComputeStats::Jsonb { std: StdCompute::Distinct } => Some(ComputeStats::Distinct),
        ComputeStats::Jsonb { std: StdCompute::Trivial } => Some(ComputeStats::Trivial),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_jsonb_stats<'mcx>(
    anl_mcx: Mcx<'mcx>,
    col_mcx: Mcx<'_>,
    stats: &mut VacAttrStats<'mcx>,
    std: StdCompute,
    src: &FetchSource<'_, '_>,
    samplerows: i32,
    totalrows: f64,
) -> PgResult<()> {
    match std {
        StdCompute::Scalar => {
            crate::compute_scalar_stats(anl_mcx, col_mcx, stats, src, samplerows, totalrows)?
        }
        StdCompute::Distinct => {
            crate::compute_distinct_stats(anl_mcx, col_mcx, stats, src, samplerows, totalrows)?
        }
        StdCompute::Trivial => crate::compute_trivial_stats(stats, src, samplerows)?,
    }
    compute_jsonb_path_slot(anl_mcx, col_mcx, stats, src, samplerows, totalrows)
}

#[derive(Clone, Copy)]
struct PathTrack {
    frequency: i32,
    delta: i32,
}

struct PathAcc<'col> {
    counts: fmt::Counts,
    values: PgFxHashMap<'col, &'col [u8], u32>,
    /// C: wider than WIDTH_THRESHOLD, assumed distinct
    toowide: u32,
    scalars: u32,
    width_sum: u64,
}

enum Member<'d> {
    Null,
    Bool(bool),
    Number(&'d [u8]),
    String(&'d [u8]),
    Array,
    Object,
}

/// False when `*left` ran out before the document's end.
fn walk_object<'d>(
    c: &'d [u8],
    depth: usize,
    left: &mut u32,
    keybuf: &mut std::vec::Vec<u8>,
    visit: &mut dyn FnMut(&[u8], Member<'d>),
) -> bool {
    let n = container_size(c);
    let base_off = 4 + 8 * n;
    for i in 0..n {
        if *left == 0 {
            return false;
        }
        *left -= 1;
        let start = (base_off + get_jsonb_offset(c, i)) as usize;
        let key = &c[start..start + get_jsonb_length(c, i) as usize];
        let vi = i + n;
        let item = fill_item(c, vi, base_off, get_jsonb_offset(c, vi));
        let mark = keybuf.len();
        fmt::push_segment(keybuf, key);
        if keybuf.len() > WIDTH_THRESHOLD {
            keybuf.truncate(mark);
            continue; // an over-long key: neither tracked nor descended
        }
        let mut child: Option<&'d [u8]> = None;
        let m = match item {
            JsonbItem::Null => Member::Null,
            JsonbItem::Bool(b) => Member::Bool(b),
            JsonbItem::Numeric(img) => Member::Number(img),
            JsonbItem::String(s) => Member::String(s),
            JsonbItem::Binary(ch) => {
                if container_is_object(ch) {
                    child = Some(ch);
                    Member::Object
                } else {
                    Member::Array
                }
            }
            JsonbItem::Array { .. } | JsonbItem::Object { .. } => {
                unreachable!("fill_item never yields begin tokens")
            }
        };
        visit(keybuf, m);
        if let Some(ch) = child {
            if depth + 1 < MAX_DEPTH && !walk_object(ch, depth + 1, left, keybuf, visit) {
                keybuf.truncate(mark);
                return false;
            }
        }
        keybuf.truncate(mark);
    }
    true
}

fn scalar_text<'s>(m: &'s Member<'s>, out: &'s mut std::vec::Vec<u8>) -> Option<&'s [u8]> {
    match m {
        Member::Bool(true) => Some(b"true"),
        Member::Bool(false) => Some(b"false"),
        Member::String(s) => Some(s),
        Member::Number(img) => {
            out.clear();
            adt_numeric::numeric_out_into(adt_numeric::Num::from_payload(&img[4..]), out);
            Some(out.as_slice())
        }
        Member::Null | Member::Array | Member::Object => None,
    }
}

fn doc_payload<'r>(row_mcx: Mcx<'r>, value: Datum) -> PgResult<Option<&'r [u8]>> {
    let raw: &'r [u8] = crate::varlena_image(value);
    if detoast::toast_raw_datum_size(raw) > DOC_BYTES_CAP {
        return Ok(None);
    }
    let img: &[u8] = if raw[0] & 0x03 == 0 {
        raw
    } else {
        let v = detoast_seams::detoast_attr::call(row_mcx, raw)?;
        adt_multirangetypes::leak_image(v)
    };
    Ok(Some(&img[4..]))
}

/// C's Duj1 in the stadistinct convention; `frac` = table-row fraction holding a scalar at the path.
fn path_ndistinct(acc: &PathAcc<'_>, frac: f64, totalrows: f64) -> f64 {
    let n = acc.scalars as f64;
    if n <= 0.0 {
        return 0.0;
    }
    let nmultiple = acc.values.values().filter(|&&c| c > 1).count() as f64;
    let ndistinct = acc.values.len() as f64;
    let f1 = ndistinct - nmultiple + acc.toowide as f64;
    let d = f1 + nmultiple;
    let big_n = (totalrows * frac).max(n);
    let mut st = if nmultiple == 0.0 {
        -frac // every value distinct: scales with the rows
    } else if acc.toowide == 0 && nmultiple == ndistinct {
        ndistinct // every value repeated: the sample saw them all
    } else {
        let denom = (n - f1) + f1 * n / big_n;
        ((n * d / denom).clamp(d, big_n) + 0.5).floor()
    };
    if st > 0.1 * totalrows {
        st = -(st / totalrows);
    }
    st
}

struct Draft<'a> {
    key: &'a [u8],
    freq: i32,
    counts: fmt::Counts,
    ndistinct: f32,
    avgwidth: i32,
    mcvs: std::vec::Vec<(u32, &'a [u8])>,
    hist_kind: u8,
    bounds: std::vec::Vec<&'a [u8]>,
}

/// C's compute_scalar_stats MCV choice.
fn choose_mcvs<'a>(
    acc: &PathAcc<'a>,
    num_mcv: usize,
    ndistinct: f64,
    analyzed: i32,
    population: f64,
) -> std::vec::Vec<(u32, &'a [u8])> {
    let mut track: std::vec::Vec<(u32, &'a [u8])> =
        acc.values.iter().filter(|(_, &c)| c > 1).map(|(&v, &c)| (c, v)).collect();
    track.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    let all = track.len() == acc.values.len()
        && acc.toowide == 0
        && ndistinct > 0.0
        && track.len() <= num_mcv;
    if !all {
        let keep = num_mcv.min(track.len());
        if keep > 0 {
            let counts: std::vec::Vec<i32> = track[..keep].iter().map(|&(c, _)| c as i32).collect();
            let nullfrac = 1.0 - acc.scalars as f64 / analyzed.max(1) as f64;
            let nd = if ndistinct < 0.0 { -ndistinct * population } else { ndistinct };
            let k = crate::analyze_mcv_list(&counts, keep as i32, nd, nullfrac, analyzed, population);
            track.truncate(k.max(0) as usize);
        } else {
            track.clear();
        }
    }
    track
}

/// Number-only paths (numeric order) or digit-led string-only paths (byte order) only.
fn choose_hist<'a>(
    acc: &PathAcc<'a>,
    mcvs: &[(u32, &'a [u8])],
    num_bins: usize,
) -> (u8, std::vec::Vec<&'a [u8]>) {
    let c = &acc.counts;
    let kind = if c.number > 0 && c.string == 0 && c.boolean == 0 {
        fmt::HIST_NUMERIC
    } else if c.string > 0
        && c.number == 0
        && c.boolean == 0
        && acc.values.keys().all(|v| v.first().is_some_and(u8::is_ascii_digit))
    {
        fmt::HIST_BYTES
    } else {
        return (fmt::HIST_NONE, std::vec::Vec::new());
    };
    let mut rest: std::vec::Vec<(f64, &'a [u8], u32)> = acc
        .values
        .iter()
        .filter(|(v, _)| !mcvs.iter().any(|(_, m)| m == *v))
        .map(|(&v, &c)| {
            let x = if kind == fmt::HIST_NUMERIC {
                std::str::from_utf8(v).ok().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0)
            } else {
                0.0
            };
            (x, v, c)
        })
        .collect();
    let num_hist = rest.len().min(num_bins + 1);
    if num_hist < 2 {
        return (fmt::HIST_NONE, std::vec::Vec::new());
    }
    if kind == fmt::HIST_NUMERIC {
        rest.sort_unstable_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    } else {
        rest.sort_unstable_by(|a, b| a.1.cmp(b.1));
    }
    let nvals: u64 = rest.iter().map(|r| r.2 as u64).sum();
    let mut bounds = std::vec::Vec::new();
    let mut j = 0usize;
    let mut cum: u64 = rest[0].2 as u64; // values at positions < cum belong to rest[..=j]
    for i in 0..num_hist {
        let pos = i as u64 * (nvals - 1) / (num_hist as u64 - 1);
        while pos >= cum && j + 1 < rest.len() {
            j += 1;
            cum += rest[j].2 as u64;
        }
        bounds.push(rest[j].1);
    }
    (kind, bounds)
}

pub(crate) fn compute_jsonb_path_slot<'mcx>(
    anl_mcx: Mcx<'mcx>,
    col_mcx: Mcx<'_>,
    stats: &mut VacAttrStats<'mcx>,
    src: &FetchSource<'_, '_>,
    samplerows: i32,
    totalrows: f64,
) -> PgResult<()> {
    if !enabled() || samplerows <= 0 {
        return Ok(());
    }
    let Some(slot) = (0..STATISTIC_NUM_SLOTS).find(|&i| stats.stakind[i] == 0) else {
        return Ok(()); // no free slot: the std stats stand alone
    };
    let target = stats.attstattarget().max(1);
    let num_paths_target = (target as i64 * 10).max(1);
    let bucket_width = (num_paths_target * 1000 / 7).max(1);

    let mut nonnull_rows: u32 = 0;
    let mut doc_bytes: u64 = 0;
    for row_no in 0..samplerows {
        let (value, isnull) = src.fetch(row_no as usize, stats.tupattnum);
        if !isnull {
            nonnull_rows += 1;
            // normalized to a 4-byte header, so heap and pgrc copies agree
            let size = detoast::toast_raw_datum_size(crate::varlena_image(value));
            if size <= DOC_BYTES_CAP {
                doc_bytes += size as u64;
            }
        }
    }
    if nonnull_rows == 0 {
        return Ok(());
    }
    let stride = walk_stride(doc_bytes, target);
    let walked = nonnull_rows.div_ceil(stride);
    let doc_cap = (VISITS_PER_TARGET * target as u64 / walked as u64)
        .clamp(DOC_CAP_MIN as u64, DOC_CAP_MAX as u64) as u32;

    let mut paths_tab: PgFxHashMap<'_, &[u8], PathTrack> =
        PgFxHashMap::with_hasher_in(Default::default(), col_mcx);
    let mut b_current = 1i32;
    let mut path_no = 0i64;
    let mut pruned_any = false;
    let mut analyzed_rows = 0u32;
    let mut skipped_docs = 0u32;
    let mut truncated_docs = 0u32;
    let mut row_scratch = MemoryContext::new_bump("jsonb path stats row");
    let mut keybuf: std::vec::Vec<u8> = std::vec::Vec::with_capacity(128);
    let mut root = fmt::Counts::default();
    let mut doc_no = 0u32;
    for row_no in 0..samplerows {
        commands_vacuum::vacuum_delay_point(true)?;
        row_scratch.reset();
        let (value, isnull) = src.fetch(row_no as usize, stats.tupattnum);
        if isnull {
            continue;
        }
        doc_no += 1;
        if (doc_no - 1) % stride != 0 {
            continue;
        }
        {
            let Some(payload) = doc_payload(row_scratch.mcx(), value)? else {
                skipped_docs += 1;
                continue;
            };
            analyzed_rows += 1;
            // a root scalar counts as a string: the planner reads the root record only to decline
            if container_is_scalar(payload) {
                root.string += 1;
                continue;
            }
            if !container_is_object(payload) {
                root.array += 1;
                continue;
            }
            root.object += 1;
            let mut left = doc_cap;
            keybuf.clear();
            let whole = walk_object(payload, 0, &mut left, &mut keybuf, &mut |key, _| {
                match paths_tab.get_mut(key) {
                    Some(t) => t.frequency += 1,
                    None => {
                        let k: &[u8] =
                            mcx::slice_borrow_in(col_mcx, key).expect("path key alloc");
                        paths_tab.insert(k, PathTrack { frequency: 1, delta: b_current - 1 });
                    }
                }
                path_no += 1;
                if path_no % bucket_width == 0 {
                    let before = paths_tab.len();
                    paths_tab.retain(|_, t| t.frequency + t.delta > b_current);
                    pruned_any |= paths_tab.len() != before;
                    b_current += 1;
                }
            });
            if !whole {
                truncated_docs += 1;
            }
        }
    }
    if analyzed_rows == 0 {
        return Ok(());
    }

    let cutoff_freq = 9 * path_no / bucket_width;
    let mut survivors: std::vec::Vec<(&[u8], i32)> = paths_tab
        .iter()
        .filter(|(_, t)| t.frequency as i64 > cutoff_freq)
        .map(|(&p, t)| (p, t.frequency))
        .collect();
    let mut dropped = paths_tab.len() - survivors.len();
    survivors.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let mut untracked_freq = cutoff_freq as f64;
    if survivors.len() > num_paths_target as usize {
        untracked_freq = untracked_freq.max(survivors[num_paths_target as usize].1 as f64);
        dropped += survivors.len() - num_paths_target as usize;
        survivors.truncate(num_paths_target as usize);
    }

    let mut index: PgFxHashMap<'_, &[u8], usize> =
        PgFxHashMap::with_hasher_in(Default::default(), col_mcx);
    let mut accs: std::vec::Vec<PathAcc<'_>> = ::mcx::stdvec::try_vec(survivors.len(), "jsonb_typanalyze")?;
    for (i, &(p, _)) in survivors.iter().enumerate() {
        index.insert(p, i);
        accs.push(PathAcc {
            counts: fmt::Counts::default(),
            values: PgFxHashMap::with_hasher_in(Default::default(), col_mcx),
            toowide: 0,
            scalars: 0,
            width_sum: 0,
        });
    }
    let mut textbuf: std::vec::Vec<u8> = std::vec::Vec::with_capacity(64);
    let mut doc_no = 0u32;
    for row_no in 0..samplerows {
        commands_vacuum::vacuum_delay_point(true)?;
        row_scratch.reset();
        let (value, isnull) = src.fetch(row_no as usize, stats.tupattnum);
        if isnull {
            continue;
        }
        doc_no += 1;
        if (doc_no - 1) % stride != 0 {
            continue;
        }
        {
            let Some(payload) = doc_payload(row_scratch.mcx(), value)? else { continue };
            if !container_is_object(payload) || container_is_scalar(payload) {
                continue;
            }
            let mut left = doc_cap;
            keybuf.clear();
            walk_object(payload, 0, &mut left, &mut keybuf, &mut |key, m| {
                let Some(&i) = index.get(key) else { return };
                let acc = &mut accs[i];
                let c = &mut acc.counts;
                c.present += 1;
                match m {
                    Member::Null => c.json_null += 1,
                    Member::Bool(_) => c.boolean += 1,
                    Member::Number(_) => c.number += 1,
                    Member::String(_) => c.string += 1,
                    Member::Array => c.array += 1,
                    Member::Object => c.object += 1,
                }
                let Some(text) = scalar_text(&m, &mut textbuf) else { return };
                acc.scalars += 1;
                acc.width_sum += text.len() as u64;
                if text.len() > WIDTH_THRESHOLD {
                    acc.toowide += 1;
                    return;
                }
                match acc.values.get_mut(text) {
                    Some(n) => *n += 1,
                    None => {
                        let k: &[u8] =
                            mcx::slice_borrow_in(col_mcx, text).expect("path value alloc");
                        acc.values.insert(k, 1);
                    }
                }
            });
        }
    }

    let docs_frac = nonnull_rows as f64 / samplerows as f64;
    let per_doc = docs_frac / analyzed_rows as f64; // table-row fraction of one document
    let population = totalrows * docs_frac; // table rows holding a document
    let num_mcv = target as usize;
    let mut drafts: std::vec::Vec<Draft<'_>> = ::mcx::stdvec::try_vec(survivors.len(), "jsonb_typanalyze")?;
    for (i, &(key, freq)) in survivors.iter().enumerate() {
        let acc = &accs[i];
        if acc.counts.present == 0 {
            continue;
        }
        let nd = path_ndistinct(acc, acc.scalars as f64 * per_doc, totalrows);
        let mcvs = choose_mcvs(acc, num_mcv, nd, analyzed_rows as i32, population);
        let (hist_kind, bounds) = choose_hist(acc, &mcvs, num_mcv);
        let avgwidth = if acc.scalars > 0 { (acc.width_sum / acc.scalars as u64) as i32 + 4 } else { 0 };
        drafts.push(Draft {
            key,
            freq,
            counts: acc.counts,
            ndistinct: nd as f32,
            avgwidth,
            mcvs,
            hist_kind,
            bounds,
        });
    }

    // byte budget: every record first, then MCV / histogram detail by path frequency
    let mut base = fmt::HEADER_LEN
        + fmt::record_len(b"", core::iter::empty(), core::iter::empty())
        + drafts
            .iter()
            .map(|d| fmt::record_len(d.key, core::iter::empty(), core::iter::empty()))
            .sum::<usize>();
    while base > BLOB_CAP {
        let Some(d) = drafts.pop() else { break }; // drafts are in frequency order
        base -= fmt::record_len(d.key, core::iter::empty(), core::iter::empty());
        untracked_freq = untracked_freq.max(d.freq as f64);
        dropped += 1;
    }
    let mut room = BLOB_CAP - base;
    for d in drafts.iter_mut() {
        let mut keep = 0usize;
        for &(_, v) in &d.mcvs {
            if 6 + v.len() > room {
                break;
            }
            room -= 6 + v.len();
            keep += 1;
        }
        d.mcvs.truncate(keep);
        let hist: usize = d.bounds.iter().map(|b| 2 + b.len()).sum();
        if hist <= room {
            room -= hist;
        } else {
            d.bounds.clear();
        }
    }
    drafts.sort_unstable_by(|a, b| a.key.cmp(b.key));

    // A document walked only up to doc_cap may hold any path past the cap.
    untracked_freq = untracked_freq.max(truncated_docs as f64);
    let complete =
        !pruned_any && dropped == 0 && truncated_docs == 0 && skipped_docs == 0 && stride == 1;
    let mut flags = 0u32;
    if complete {
        flags |= fmt::FLAG_COMPLETE;
    }
    if truncated_docs > 0 || skipped_docs > 0 {
        flags |= fmt::FLAG_PARTIAL_DOCS;
    }
    let untracked_frac = if complete {
        0.0
    } else {
        (untracked_freq.max(1.0) / analyzed_rows as f64).min(1.0)
    };
    let mut w = fmt::Writer::new(
        flags,
        samplerows as u32,
        nonnull_rows,
        analyzed_rows,
        totalrows,
        untracked_frac,
        doc_cap,
        truncated_docs,
    );
    root.present = analyzed_rows;
    w.path(b"", &root, 0.0, 0, &[], fmt::HIST_NONE, &[]);
    for d in &drafts {
        w.path(d.key, &d.counts, d.ndistinct, d.avgwidth, &d.mcvs, d.hist_kind, &d.bounds);
    }
    let blob = w.finish();

    stats.stats_valid = true;
    stats.stakind[slot] = fmt::STATISTIC_KIND_JSONB_PATHS;
    stats.staop[slot] = InvalidOid;
    stats.stacoll[slot] = InvalidOid;
    let mut vals: PgVec<'mcx, Datum> = mcx::vec_with_capacity_in(anl_mcx, 1)?;
    vals.push(bytea_datum_in(anl_mcx, &blob)?);
    stats.stavalues[slot] = vals;
    stats.stavalues_set[slot] = true;
    stats.stanumbers[slot] = PgVec::new_in(anl_mcx);
    stats.statypid[slot] = BYTEAOID;
    stats.statyplen[slot] = -1;
    stats.statypbyval[slot] = false;
    stats.statypalign[slot] = b'i';
    Ok(())
}

fn bytea_datum_in<'m>(mcx: Mcx<'m>, s: &[u8]) -> PgResult<Datum> {
    let mut img: PgVec<'m, u8> = mcx::vec_with_capacity_in(mcx, s.len() + datum::VARHDRSZ)?;
    mcx::vec_append_bytes(&mut img, &datum::set_varsize_4b(s.len() + datum::VARHDRSZ))?;
    mcx::vec_append_bytes(&mut img, s)?;
    Ok(Datum::from_usize(adt_multirangetypes::leak_image(img).as_ptr() as usize))
}

#[cfg(test)]
mod jsonb_path_stats_tests {
    use super::*;

    fn acc_of<'c>(mcx: Mcx<'c>, vals: &[(&'c [u8], u32)], counts: fmt::Counts) -> PathAcc<'c> {
        let mut values = PgFxHashMap::with_hasher_in(Default::default(), mcx);
        let mut scalars = 0;
        for &(v, c) in vals {
            values.insert(v, c);
            scalars += c;
        }
        PathAcc { counts, values, toowide: 0, scalars, width_sum: 0 }
    }

    #[test]
    fn walk_stride_bounds_bytes_per_pass() {
        let budget = WALK_BYTES_PER_TARGET * 100;
        assert_eq!(walk_stride(0, 100), 1);
        assert_eq!(walk_stride(30_000 * 1_000, 100), 1); // 1 KB documents: all walked
        assert_eq!(walk_stride(budget, 100), 1);
        assert_eq!(walk_stride(budget + 1, 100), 2);
        assert_eq!(walk_stride(20_000 * 50_000, 100), 16);
        assert_eq!(walk_stride(20_000 * 50_000, 1000), 2); // a higher target walks more
        assert_eq!(walk_stride(u64::MAX, 0), u32::MAX);
    }

    #[test]
    fn ndistinct_follows_c_conventions() {
        let cx = MemoryContext::new_bump("t");
        let c = fmt::Counts::default();
        // every value distinct: -frac
        let a = acc_of(cx.mcx(), &[(b"a", 1), (b"b", 1)], c);
        assert_eq!(path_ndistinct(&a, 0.5, 1000.0), -0.5);
        // every value repeated: the count itself
        let a = acc_of(cx.mcx(), &[(b"a", 5), (b"b", 3)], c);
        assert_eq!(path_ndistinct(&a, 0.8, 1000.0), 2.0);
        // over 10% of the table: a negative fraction of totalrows
        let vals: std::vec::Vec<std::vec::Vec<u8>> = (0..300).map(|i| format!("v{i}").into_bytes()).collect();
        let mut pairs: std::vec::Vec<(&[u8], u32)> = vals.iter().map(|v| (v.as_slice(), 2)).collect();
        pairs.push((b"x", 1));
        let a = acc_of(cx.mcx(), &pairs, c);
        let nd = path_ndistinct(&a, 1.0, 1000.0);
        assert!(nd < 0.0 && nd > -1.0, "{nd}");
    }

    #[test]
    fn histogram_bounds_are_equi_height_and_skip_mcvs() {
        let cx = MemoryContext::new_bump("t");
        let c = fmt::Counts { present: 10, number: 10, ..Default::default() };
        let a = acc_of(
            cx.mcx(),
            &[(b"1", 1), (b"2", 1), (b"10", 1), (b"3", 1), (b"7", 5), (b"4", 1)],
            c,
        );
        let mcvs = [(5u32, &b"7"[..])];
        let (k, b) = choose_hist(&a, &mcvs, 100);
        assert_eq!(k, fmt::HIST_NUMERIC);
        assert_eq!(b, vec![&b"1"[..], b"2", b"3", b"4", b"10"]);
        let (_, b) = choose_hist(&a, &mcvs, 1);
        assert_eq!(b, vec![&b"1"[..], b"10"]);
        // strings: only digit-led (date-like) values get a byte-order histogram
        let s = fmt::Counts { present: 3, string: 3, ..Default::default() };
        let d = acc_of(cx.mcx(), &[(b"2024-01-02", 1), (b"2023-05-06", 1), (b"2025-01-01", 1)], s);
        let (k, b) = choose_hist(&d, &[], 100);
        assert_eq!(k, fmt::HIST_BYTES);
        assert_eq!(b, vec![&b"2023-05-06"[..], b"2024-01-02", b"2025-01-01"]);
        let w = acc_of(cx.mcx(), &[(b"2024-01-02", 1), (b"abc", 1), (b"2025", 1)], s);
        assert_eq!(choose_hist(&w, &[], 100).0, fmt::HIST_NONE);
    }
}

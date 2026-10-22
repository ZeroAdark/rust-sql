//! Part rows compare by their deletion masks (both paths read the parts the same way), segment rows by `(ctid, row digest)`.

use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use datum::Datum;
use mcx::{Mcx, PgVec};
use pgrc_am::scan::DeletionFilter;
use pgrc_format::manifest::{SegState, SegmentRecord};
use pgrc_format::rowid::seg_local;
use pgrc_scan::DV_GRANULE_WORDS;
use tableam_vocab::{SO_ALLOW_PAGEMODE, SO_TYPE_SEQSCAN};
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_fmgr::{FmgrBuiltin, FmgrInfo, FunctionCallInfoBaseData as Fcinfo};
use types_rel::{AccessShareLock, NoLock, Relation};
use types_scan::sdir::ScanDirection;

use crate::image::images;
use crate::view::SegSource;

pub const PGRC_COLCACHE_DIFF_FOID: Oid = 9033;
pub const PGRC_COLCACHE_STATS_FOID: Oid = 9034;

pub const COLCACHE_BUILTINS: &[FmgrBuiltin] = &[
    FmgrBuiltin { foid: PGRC_COLCACHE_DIFF_FOID, name: "pgrc_colcache_diff", nargs: 1, strict: true, retset: false, func: fc_pgrc_colcache_diff },
    FmgrBuiltin { foid: PGRC_COLCACHE_STATS_FOID, name: "pgrc_colcache_stats", nargs: 1, strict: true, retset: false, func: fc_pgrc_colcache_stats },
];

fn open<'mcx>(mcx: Mcx<'mcx>, relid: Oid, name: &str) -> PgResult<Relation<'mcx>> {
    let rel = table::table_open(mcx, relid, AccessShareLock)?;
    if !pgrc_front::is_pgrc_table(&rel) {
        let e = PgError::error(format!("{name}: \"{}\" is not a pgrcolumnar table", rel.name()));
        rel.close(NoLock)?;
        return Err(Box::new(e));
    }
    Ok(rel)
}

pub fn fc_pgrc_colcache_diff(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let mcx = fcinfo.result_mcx();
    let rel = open(mcx, fcinfo.arg_oid(0), "pgrc_colcache_diff")?;
    let out = diff(mcx, &rel);
    rel.close(NoLock)?;
    match out? {
        Some(n) => Ok(Datum::from_i64(n)),
        None => {
            fcinfo.isnull = true;
            Ok(Datum::null())
        }
    }
}

type Row = (u32, u16, u64);

/// The row digest: the attribute count and everything past the fixed header (null bitmap and data), FNV-1a.
fn digest(t: &[u8]) -> u64 {
    let natts = u16::from_ne_bytes([t[18], t[19]]) & 0x07ff;
    natts.to_ne_bytes().iter().chain(&t[23..]).fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

fn skip(rows: &[Row], deleted: &[u32]) -> Vec<Row> {
    rows.iter().filter(|r| !seg_local(r.0, r.1).is_some_and(|p| deleted.binary_search(&p).is_ok())).copied().collect()
}

/// Rows in exactly one of the two lists (a ctid is unique in each).
fn sym_diff(a: Vec<Row>, b: Vec<Row>) -> i64 {
    let (a, b): (HashSet<Row>, HashSet<Row>) = (a.into_iter().collect(), b.into_iter().collect());
    a.symmetric_difference(&b).count() as i64
}

fn diff<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<Option<i64>> {
    let snap = snapmgr::GetActiveSnapshot();
    let gen = pgrc_am::scan::resolve_scan_parts(rel, Some(&*snap))?.gen;
    let newer = pgrc_front::ring::newer_segments(rel, gen.as_deref())?;
    let segs: Vec<SegmentRecord> = gen.iter().flat_map(|g| g.segments.iter()).filter(|r| r.state != SegState::Sealed).chain(&newer).copied().collect();
    let Some(v) = crate::view::view(mcx, rel, gen.as_ref(), &segs, &snap)? else { return Ok(None) };
    let f2 = pgrc_front::filter::build(mcx, rel, gen.as_ref(), &newer, &Some(Rc::clone(&snap)))?.unwrap_or_default();
    let mut n = 0i64;
    for (i, rec) in gen.iter().flat_map(|g| g.manifest.parts.iter()).enumerate() {
        for g in 0..rec.granule_count {
            let (mut a, mut b) = (None, None);
            v.filter.granule_into(i, g, &mut a);
            f2.granule_into(i, g, &mut b);
            let z = [0u64; DV_GRANULE_WORDS];
            let (a, b) = (a.as_deref().unwrap_or(&z), b.as_deref().unwrap_or(&z));
            n += a.iter().zip(b).map(|(x, y)| (x ^ y).count_ones() as i64).sum::<i64>();
        }
    }
    for sv in &v.segs {
        let heap = seg_rows(mcx, sv.relid, &snap)?;
        let got = match &sv.src {
            SegSource::Image { img, .. } => {
                let row = |(b, i): (u32, u16)| img.chunk(b).map(|c| (b, c.hdr[i as usize].off, digest(c.tuple_bytes(i as usize))));
                sv.rows().unwrap_or_default().into_iter().filter_map(row).collect()
            }
            SegSource::Heap => skip(&heap, v.filter.segment_positions(sv.slot)),
        };
        n += sym_diff(got, skip(&heap, f2.segment_positions(sv.slot)));
    }
    Ok(Some(n))
}

fn seg_rows<'mcx>(mcx: Mcx<'mcx>, relid: Oid, snap: &snapmgr::Snapshot) -> PgResult<Vec<Row>> {
    let seg = table::table_open(mcx, relid, AccessShareLock)?;
    let mut hs = heapam::heap_beginscan(mcx, &seg, Some(Rc::clone(snap)), 0, PgVec::new_in(mcx), None, SO_TYPE_SEQSCAN | SO_ALLOW_PAGEMODE)?;
    let mut rows = Vec::new();
    let out = (|| {
        while let Some(tup) = heapam::heap_getnext(&mut hs, ScanDirection::ForwardScanDirection)? {
            // SAFETY: a heap tuple image readable for `t_len` bytes while the scan is on its page.
            let t = unsafe { std::slice::from_raw_parts(tup.header_ptr(), tup.t_len as usize) };
            rows.push((types_tuple::ItemPointerGetBlockNumber(&tup.t_self), types_tuple::ItemPointerGetOffsetNumberNoCheck(&tup.t_self), digest(t)));
        }
        Ok(())
    })();
    heapam::heap_endscan(hs)?;
    seg.close(NoLock)?;
    out.map(|()| rows)
}

pub fn fc_pgrc_colcache_stats(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let mcx = fcinfo.result_mcx();
    let rel = open(mcx, fcinfo.arg_oid(0), "pgrc_colcache_stats")?;
    let out = stats(mcx, &rel);
    rel.close(NoLock)?;
    let mut img: PgVec<'_, u8> = PgVec::new_in(mcx);
    img.extend_from_slice(&[0u8; datum::VARHDRSZ]);
    img.extend_from_slice(out?.as_bytes());
    Ok(types_fmgr::varlena_result(datum::Varlena::from_image(img)))
}

fn stats<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<String> {
    let heaps = pgrc_front::tombs::open_existing(mcx, rel.rd_id)?;
    let mut relids: Vec<Oid> = heaps.iter().map(|h| h.rel.rd_id).collect();
    pgrc_front::tombs::close_all(heaps)?;
    relids.extend(pgrc_front::ring::newest(rel)?.iter().flat_map(|g| g.segments.iter().map(|r| r.relid)));
    let mut out = String::new();
    for i in images().list().iter().filter(|i| relids.contains(&i.relid)).map(Arc::as_ref) {
        let rows: usize = i.chunks.iter().flatten().map(|c| c.rows()).sum();
        let kind = if i.kind == crate::Kind::Tomb { "tomb" } else { "segment" };
        let (relid, blocks, recent, bytes, seq, builds, redecoded) = (i.relid, i.nblocks, i.recent.len(), i.bytes, i.seq, i.builds, i.redecoded);
        out += &format!("{kind} relid={relid} blocks={blocks} rows={rows} recent={recent} bytes={bytes} seq={seq} builds={builds} redecoded={redecoded}\n");
    }
    Ok(out)
}

//! A btree bitmap probe through a frozen index image (nbtree `IndexImage` / `ImageRel`): the morsel leader's bitmap
//! build reads the index by storage locator with catalog answers from the image's tables, the read path a pool thread
//! runs, instead of through the relcache entry. Taken only when the image holds (a btree, not backend-local, every
//! function it can call pool-hosted) and the transaction is not SERIALIZABLE (the image takes no predicate locks); else
//! the caller keeps the relation path. The image is captured per call and never kept: a REINDEX moves the locator.
//! The pgstat counts are the relation path's: idx_scan (bt_first per primitive scan), idx_tup_read (the TIDs reported,
//! index_getbitmap's) and the index's blocks fetched / hit, landed on the index's entry here.
//! Fire record (PGRUST_GRAVITY_REPORT): `index.image` when the build went through the image, `index.image.refused` when a
//! btree build kept the relation path, `index.leaves.wide` when pool threads walked its leaves (`getbitmap_wide`).
//! A2 entry 2 (`getbitmap_wide`): pool threads walk the leaves, each through its own `ImageRel` and scan over one shared
//! PostgreSQL parallel btree scan state (nbtree parallel.rs seize / release / done: one leaf page at a time), into its own
//! bitmap; the leader unions them. Taken only uninstrumented (EXPLAIN ANALYZE keeps the leader walk, so its Heap Blocks
//! exact / lossy counts are PostgreSQL's), for forward scans whose keys rebuild on any thread (no row comparison).
use std::rc::Rc;
use std::sync::Arc;

use pgsync::{Mutex, OnceLock};

use crate::ops::scan::bitmap_leaves::SharedScanKey;

use mcx::Mcx;
use types_error::PgResult;
use types_rel::Relation;
use types_scan::scankey::ScanKeyData;
use types_snapshot::SnapshotData;

use crate::ops::scan::index_source::BTREE_AM_OID;

/// `keys`' bitmap over `index` into `tbm` through its image, the TIDs reported; `None` when the image path does not
/// apply (nothing read, `tbm` untouched). `in_recovery` is the relation scan's `xactStartedInRecovery`.
pub fn getbitmap(mcx: Mcx<'_>, index: &Relation<'_>, snapshot: &Rc<SnapshotData<'static>>, keys: &[ScanKeyData], in_recovery: bool, tbm: &mut tidbitmap::TIDBitmap<'_>) -> PgResult<Option<u64>> {
    if index.rd_rel.relam != BTREE_AM_OID {
        return Ok(None);
    }
    if xact_seams::isolation_is_serializable::is_installed() && xact_seams::isolation_is_serializable::call() {
        fires::fire("index.image.refused");
        return Ok(None);
    }
    let Some(img) = nbtree::IndexImage::capture(index)? else {
        fires::fire("index.image.refused");
        return Ok(None);
    };
    if !img.proc_oids().all(gravity_fn::session::pool_hosted) {
        fires::fire("index.image.refused");
        return Ok(None);
    }
    fires::fire("index.image");
    let r = nbtree::ImageRel::new(&img, mcx)?;
    let mut scan = r.begin_scan(mcx, keys.len() as i32, in_recovery)?;
    scan.xs_snapshot = Some(snapshot.clone());
    let walked = (|| -> PgResult<i64> {
        nbtree::btrescan(&mut scan, if keys.is_empty() { None } else { Some(keys) })?;
        nbtree::btgetbitmap_over(&mut scan, &r, tbm)
    })();
    let ended = nbtree::btendscan(&mut scan);
    // the counts land whether or not the walk failed, as the relation path's batched counters drain at its scan's drop
    let shared = index.rd_rel.relisshared;
    let tuples = match (&walked, index.pgstat_enabled.get()) {
        (Ok(n), true) => *n as u64,
        _ => 0,
    };
    pgstat::relation::pgstat_count_index_scan_batched(index.rd_id, shared, scan.xs_pgstat.take_detached_scans(), tuples, 0);
    if index.pgstat_enabled.get() {
        pgstat::relation::pgstat_count_buffer_reads(index.rd_id, shared, r.blocks_fetched(), r.blocks_hit());
    }
    let ntids = walked?;
    ended?;
    Ok(Some(ntids as u64))
}

/// A fan-out of `job(ordinal)` over `width` pool threads, the caller parked until every one returned (exec installs
/// gravity_parallel's pool run: hosts has no pool dependency). `Ok(false)`: no pool, nothing ran (the leader walks).
pub type LeafJob<'a> = dyn Fn(u16) -> PgResult<()> + Sync + 'a;
pub type FanOut = fn(u16, &LeafJob<'_>) -> PgResult<bool>;

static FAN_OUT: OnceLock<FanOut> = OnceLock::new();

/// Installs the pool fan-out (exec's `init_seams`, once).
pub fn install_fan_out(f: FanOut) {
    let _ = FAN_OUT.set(f);
}

/// `getbitmap` with the leaf walk on `width` pool threads (A2 entry 2): each thread seizes the next leaf page of one shared
/// parallel btree scan through its own image view and scan (`maxbytes / width` of bitmap each), the leader unions the
/// frozen bitmaps into `tbm`. `None` when the image path or the fan-out does not apply (`tbm` untouched: the caller
/// walks); backward scans are not asked for here (the morsel form's walk is forward).
#[allow(clippy::too_many_arguments)]
pub fn getbitmap_wide(index: &Relation<'_>, snapshot: &Rc<SnapshotData<'static>>, keys: &[ScanKeyData], in_recovery: bool, width: u16, maxbytes: usize, tbm: &mut tidbitmap::TIDBitmap<'_>) -> PgResult<Option<u64>> {
    let Some(fan) = FAN_OUT.get().copied() else { return Ok(None) };
    if width < 2 || index.rd_rel.relam != BTREE_AM_OID {
        return Ok(None);
    }
    if xact_seams::isolation_is_serializable::is_installed() && xact_seams::isolation_is_serializable::call() {
        return Ok(None);
    }
    let Some(images) = keys.iter().map(SharedScanKey::of).collect::<Option<Vec<SharedScanKey>>>() else { return Ok(None) }; // alloc: Cold
    if !images.iter().all(|k| k.func == 0 || gravity_fn::session::pool_hosted(k.func)) {
        return Ok(None);
    }
    let Some(img) = nbtree::IndexImage::capture(index)? else { return Ok(None) };
    if !img.proc_oids().all(gravity_fn::session::pool_hosted) {
        return Ok(None);
    }
    let shared = Arc::new(types_relscan::ParallelIndexScanDescShared {
        ps_locator: img.locator(),
        ps_indexlocator: img.locator(),
        snapshot: snapmgr::SerializeSnapshot(snapshot),
        am: types_relscan::ParallelIndexAmShared::Btree(types_relscan::BTParallelScanShared::new()),
    });
    let types_relscan::ParallelIndexAmShared::Btree(bt) = &shared.am;
    // every claim's walk, whatever its ordinal: two claims can run on one pool thread (the later finds the scan done and
    // brings an empty bitmap), so a walk is kept by arrival, never filed by ordinal (one slot per ordinal lost the first)
    let walks: Mutex<Vec<crate::ops::scan::bitmap_leaves::Walked>> = Mutex::new(Vec::with_capacity(width as usize)); // alloc: Cold
    let failed: Mutex<Option<types_error::PgError>> = Mutex::new(None);
    let per = (maxbytes / width as usize).max(64 * 1024);
    // testing: PGRUST_GRAVITY_LEAF_FAULT=n raises in the n-th walk to arrive (the forced-error witness: no hang, the
    // statement's error, the others' pins released); read once per process
    static FAULT: OnceLock<Option<u64>> = OnceLock::new();
    let fault = *FAULT.get_or_init(|| gravity_driver::env::num::<u64>("PGRUST_GRAVITY_LEAF_FAULT"));
    let arrivals = core::sync::atomic::AtomicU64::new(0);
    let job = |_ord: u16| -> PgResult<()> {
        let n = arrivals.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
        match crate::ops::scan::bitmap_leaves::walk(&img, &shared, bt, &images, in_recovery, per, fault == Some(n)) {
            Ok(w) => {
                walks.lock().unwrap_or_else(|p| p.into_inner()).push(w); // alloc: Cold
                Ok(())
            }
            Err(e) => {
                let mut f = failed.lock().unwrap_or_else(|p| p.into_inner());
                if f.is_none() {
                    *f = Some(*e);
                }
                Err(Box::new(types_error::PgError::error("gravity: a parallel index leaf walk failed"))) // alloc: Cold
            }
        }
    };
    let ran = fan(width, &job);
    if let Some(e) = failed.into_inner().unwrap_or_else(|p| p.into_inner()) {
        return Err(Box::<types_error::PgError>::new(e)); // alloc: Cold
    }
    if !ran? {
        return Ok(None);
    }
    fires::fire("index.image");
    fires::fire("index.leaves.wide");
    let (mut tids, mut fetched, mut hit, mut scans) = (0i64, 0u64, 0u64, 0u64);
    // one walk's bitmap unioned is the unit (a pass over its pages; the walks are at most the fan's width)
    let mut step = cancelpoll::step("bitmap_image getbitmap_wide: one walk's bitmap unioned");
    for w in walks.into_inner().unwrap_or_else(|p| p.into_inner()) {
        step.tick().map_err(|_| crate::pg_err("gravity: a bitmap union stopped for its statement's cancel"))?;
        tbm.union_frozen(&w.frozen)?;
        (tids, fetched, hit, scans) = (tids + w.tids, fetched + w.fetched, hit + w.hit, scans + w.scans);
    }
    // the relation path's counts on the index's entry (one idx_scan per primitive scan any thread started)
    let shared_rel = index.rd_rel.relisshared;
    if index.pgstat_enabled.get() {
        pgstat::relation::pgstat_count_index_scan_batched(index.rd_id, shared_rel, scans, tids.max(0) as u64, 0);
        pgstat::relation::pgstat_count_buffer_reads(index.rd_id, shared_rel, fetched, hit);
    }
    Ok(Some(tids.max(0) as u64))
}

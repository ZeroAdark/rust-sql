//! A2 entry 2, the pool side of a btree's parallel leaf walk (bitmap_image.rs `getbitmap_wide` is the leader's): one pool
//! thread's scan over the shared PostgreSQL parallel btree scan state (nbtree parallel.rs seize / release / done), through
//! its own view of the leader's frozen index image (`ImageRel`: buffers by locator, catalog answers from the image's
//! tables, functions from the built-in table), into its own bitmap. Lint R14 lists this file: it names no relcache or
//! catalog API.
use std::sync::Arc;

use types_error::PgResult;
use types_scan::scankey::ScanKeyData;

/// A scan key as any thread rebuilds it (`ScanKeyData` holds an `FmgrInfo`, which is not `Sync`): its function by OID,
/// resolved from the built-in table (the leader took the walk only when every key function is pool-hosted), its
/// argument the leader's word (a by-reference argument points at the leader's
/// image, read only while the leader waits for the walk).
#[derive(Clone, Copy)]
pub(crate) struct SharedScanKey {
    flags: i32,
    attno: types_core::AttrNumber,
    strategy: types_scan::scankey::StrategyNumber,
    subtype: types_core::Oid,
    collation: types_core::Oid,
    pub(crate) func: types_core::Oid,
    arg: datum::Datum,
}

impl SharedScanKey {
    /// `k` as an image; `None` for a row comparison (its members hang off the argument and preprocessing writes them).
    pub(crate) fn of(k: &ScanKeyData) -> Option<SharedScanKey> {
        use types_scan::scankey::{SK_ROW_HEADER, SK_ROW_MEMBER};
        if k.sk_flags & (SK_ROW_HEADER | SK_ROW_MEMBER) != 0 {
            return None;
        }
        Some(SharedScanKey { flags: k.sk_flags, attno: k.sk_attno, strategy: k.sk_strategy, subtype: k.sk_subtype, collation: k.sk_collation, func: k.sk_func.fn_oid, arg: k.sk_argument })
    }

    /// A key the walk adds itself (index_ranges.rs: a range's bounds, its NULL test): `flags` on column `attno`, strategy
    /// `strategy` of function `func` (0: none) against `arg`, `subtype` its right-hand type, under `collation`.
    pub(crate) const fn bound(flags: i32, attno: types_core::AttrNumber, strategy: types_scan::scankey::StrategyNumber, subtype: types_core::Oid, collation: types_core::Oid, func: types_core::Oid, arg: datum::Datum) -> SharedScanKey {
        SharedScanKey { flags, attno, strategy, subtype, collation, func, arg }
    }

    pub(crate) fn key(&self) -> PgResult<ScanKeyData> {
        let mut k = ScanKeyData::empty();
        (k.sk_flags, k.sk_attno, k.sk_strategy, k.sk_subtype, k.sk_collation, k.sk_argument) = (self.flags, self.attno, self.strategy, self.subtype, self.collation, self.arg);
        if self.func != 0 {
            let Some(b) = fmgr_core::fmgr_isbuiltin(self.func) else {
                return Err(Box::new(types_error::PgError::error("gravity: index leaf walk: a key function is not built in"))); // alloc: Cold
            };
            fmgr_core::fmgr_info_from_builtin_into(b, self.func, &mut k.sk_func);
        }
        Ok(k)
    }
}

/// One thread's walk: its bitmap frozen for the leader, the TIDs it reported, its index buffer reads and its primitive
/// scans (the pgstat counts the leader lands).
pub(crate) struct Walked {
    pub(crate) frozen: Arc<tidbitmap::TbmSharedIterState>,
    pub(crate) tids: i64,
    pub(crate) fetched: u64,
    pub(crate) hit: u64,
    pub(crate) scans: u64,
}

/// Ends the shared scan for every participant when a walk leaves early (an error, or an unwind the pool catches): the
/// others parked on the next leaf wake and see it done.
struct AbortOnExit<'a>(&'a types_relscan::BTParallelScanShared, bool);

impl Drop for AbortOnExit<'_> {
    fn drop(&mut self) {
        if !self.1 {
            nbtree::bt_parallel_abort(self.0);
        }
    }
}

/// This thread's share of the walk: leaves seized from `shared` (whose btree state is `bt`) until the scan is done, keys
/// `images` rebuilt here, a bitmap of `maxbytes`. No snapshot on the scan: it reads index pages only, takes no predicate
/// lock (SERIALIZABLE keeps the leader walk) and drops no pin early (a bitmap scan never does); the heap pass judges
/// visibility. An error or an unwind ends the shared scan for all (`AbortOnExit`). `fault` (testing,
/// PGRUST_GRAVITY_LEAF_FAULT): raise once the scan is started, as a failure mid-walk would.
#[allow(clippy::too_many_arguments)]
pub(crate) fn walk(img: &nbtree::IndexImage, shared: &Arc<types_relscan::ParallelIndexScanDescShared>, bt: &types_relscan::BTParallelScanShared, images: &[SharedScanKey], in_recovery: bool, maxbytes: usize, fault: bool) -> PgResult<Walked> {
    let mut guard = AbortOnExit(bt, false);
    let cx = gravity_driver::mem::root::Root::new("index leaves");
    let wm = cx.mcx();
    let r = nbtree::ImageRel::new(img, wm)?;
    let mut scan = r.begin_scan(wm, images.len() as i32, in_recovery)?;
    scan.parallel_scan = Some(shared.clone());
    let ks = images.iter().map(SharedScanKey::key).collect::<PgResult<Vec<ScanKeyData>>>()?; // alloc: Cold
    let mut mine = tidbitmap::TIDBitmap::new(wm, maxbytes);
    let walked = (|| -> PgResult<i64> {
        nbtree::btrescan(&mut scan, if ks.is_empty() { None } else { Some(&ks) })?;
        if fault {
            return Err(Box::new(types_error::PgError::error("gravity: injected parallel leaf walk fault (PGRUST_GRAVITY_LEAF_FAULT)"))); // alloc: Cold
        }
        nbtree::btgetbitmap_over(&mut scan, &r, &mut mine)
    })();
    let ended = nbtree::btendscan(&mut scan);
    let tids = walked?;
    ended?;
    let w = Walked { frozen: mine.prepare_shared_iterate()?, tids, fetched: r.blocks_fetched(), hit: r.blocks_hit(), scans: scan.xs_pgstat.take_detached_scans() };
    guard.1 = true;
    Ok(w)
}

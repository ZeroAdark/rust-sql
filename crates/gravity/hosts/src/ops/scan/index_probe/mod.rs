//! D3014O nest.parallel: a worker's probe of an embedded nest inner's btree site (lowerv4 ops/nestloop/widen.rs marks the
//! site `SiteKind::IndexSource { worker }`). The leader serves the site with its own `IndexSource`; a worker gets this
//! source instead (driver `Derive::derive_worker_source`), the heap range source with a probe filter:
//! - per outer row (the seek), the site's key words probe the index through this execution's frozen image (idxmorsel A2's
//!   `nbtree::IndexImage` / `ImageRel::probe`: relcache-free buffer reads, catalog answers from the image), each worker
//!   through its own `ImageRel` and scan descriptor, built on its thread;
//! - the probed heap TIDs, in block order, are read as the heap range source reads a bitmap's exact pages
//!   (`bitmap_morsel.rs`): relcache-free page reads under the leader's snapshot, each visible tuple kept whose HOT chain
//!   root is a probed TID (PostgreSQL's heap_hot_search_buffer from the index's TID). A step marked wide has an order-free
//!   end (widen.rs), so the block order is never seen.
//! The image is captured once per execution on the leader (exec runner plan.rs, `capture`), with bitmap_image.rs's gates
//! (a btree, not backend-local, every function it or the keys call pool-hosted, not SERIALIZABLE); when it does not hold,
//! the execution runs the pipeline on the leader alone (D3014O P3), so a worker's probe never runs without one.
//! pgstat: the index's scans (one per probe, `idx_scan`), the TIDs it returned (`idx_tup_read`) and its block reads land at
//! the unbind (the leader's thread), as the heap's fetched tuples do (`SampleFilter::BITMAP`).
use std::sync::Arc;

use gravity_abi::Datum as Word;
use types_core::Oid;
use types_error::PgResult;
use types_rel::Relation;
use types_scan::scankey::ScanKeyData;
use types_storage::bufpage::PageRef;
use types_core::BlockNumber;

use crate::ops::scan::heap_source::{RangeRecipe, SampleFilter};
use crate::scratch::NodeScratch;

mod raw;

/// One execution's frozen image of the site's index, filed by the leader (`capture`) and read by every worker's probe.
pub struct Filed {
    img: Arc<nbtree::IndexImage>,
    /// The leader's `TransactionStartedDuringRecovery` (`begin_scan`'s).
    in_recovery: bool,
    /// The index's `relisshared` when its statistics count (`pgstat_enabled`).
    stats: Option<bool>,
}

/// The worker form of an index recipe (`SiteRecipe::worker`): the heap read's deform and the probe's keys. The image is
/// not the recipe's: a cached program's recipe serves executions of other statements and sessions (a parked Node's
/// worker sources were derived from the registry that built it), so each execution files its own image in its
/// `Resolved` by registry entry (`Resolved::file_probe`) and each worker's filter binds it from there (D3117, as D300's
/// verdict cells).
#[derive(Clone)]
pub struct WorkerProbe {
    pub range: RangeRecipe,
    pub index: Oid,
    pub keys: Vec<ScanKeyData>,
}

impl WorkerProbe {
    /// `None` when the recipe's walk is not a plain point probe a worker serves: every key a bound `column op value` key
    /// word in key order (no search array, row comparison, null test or ORDER BY), a forward heap walk (not index-only),
    /// table columns only, over a relation the heap range source reads by locator (not temporary).
    pub fn of(r: &crate::ops::scan::index_source::SiteRecipe, heap: &Relation<'_>) -> Option<WorkerProbe> {
        use crate::ops::scan::index_source::KeyWord;
        let plain = r.nkeys > 0 && r.keys.len() == r.nkeys && r.words.len() == r.nkeys && r.words.iter().enumerate().all(|(i, w)| matches!(w, KeyWord::Key(k) if *k as usize == i));
        let keys = plain && r.keys.iter().all(|k| k.sk_flags == 0) && r.host_keys.iter().all(|h| !h) && r.rows.is_empty() && r.am.is_none();
        if !keys || r.backward || r.attnos.iter().any(|&a| a < 1) || heap.uses_local_buffers() {
            return None;
        }
        let range = RangeRecipe::from_plan(heap, &r.attnos).ok()?;
        Some(WorkerProbe { range, index: r.index, keys: r.keys.clone() })
    }

    /// The execution's image of `index` (the site's), for the workers (exec runner plan.rs files it in the execution's
    /// `Resolved`): `None` when it does not hold: not imageable, a function it or a key calls not pool-hosted, or a
    /// SERIALIZABLE transaction (the image takes no predicate lock). The leader's thread, once per execution; it goes with
    /// the execution's `Resolved` (a REINDEX moves the locator; the next execution captures its own).
    /// `in_recovery` is the leader's `TransactionStartedDuringRecovery`.
    pub fn capture(&self, index: &Relation<'_>, in_recovery: bool) -> PgResult<Option<Arc<Filed>>> {
        if xact_seams::isolation_is_serializable::is_installed() && xact_seams::isolation_is_serializable::call() {
            return Ok(None);
        }
        let Some(img) = nbtree::IndexImage::capture(index)? else { return Ok(None) };
        if !img.proc_oids().all(gravity_fn::session::pool_hosted) || !self.keys.iter().all(|k| gravity_fn::session::pool_hosted(k.sk_func.fn_oid)) {
            return Ok(None);
        }
        let stats = index.pgstat_enabled.get().then_some(index.rd_rel.relisshared);
        Ok(Some(Arc::new(Filed { img: Arc::new(img), in_recovery, stats }))) // alloc: Exec
    }
}

/// A worker's view of the filed image on the thread that built it: its `ImageRel` and scan descriptor, in its own context.
/// Fields drop in order: the scan and the view before the image they borrow, all before the context they live in.
struct View {
    scan: types_relscan::IndexScanDescData<'static>,
    rel: nbtree::ImageRel<'static, 'static>,
    filed: Arc<Filed>,
    thread: std::thread::ThreadId,
    scratch: NodeScratch,
}

/// The heap walk's filter over one probe's TIDs (`HeapRangeSource<ProbeFilter>`).
pub struct ProbeFilter {
    /// The site's registry entry: the execution's image is `Resolved::probe_image(site)` (`bind`).
    site: usize,
    /// The bound execution's image (`bind`; `None` unbound).
    filed: Option<Arc<Filed>>,
    keys: Vec<ScanKeyData>,
    /// `keys.len()` (the scan descriptor's key count; `keys` is out of its field across a probe).
    nkeys: usize,
    view: Option<View>,
    /// The probe's TIDs, read in block order (tid_pages.rs).
    pages: crate::ops::scan::tid_pages::TidPages,
    /// `(probes, TIDs, blocks fetched, blocks hit)` of the index since the bind, and its `relisshared` when they count.
    counts: (u64, u64, u64, u64),
    stats: Option<(Oid, bool)>,
}

impl ProbeFilter {
    pub fn new(p: &WorkerProbe, site: usize) -> ProbeFilter {
        ProbeFilter { site, filed: None, keys: p.keys.clone(), nkeys: p.keys.len(), view: None, pages: crate::ops::scan::tid_pages::TidPages::new(), counts: (0, 0, 0, 0), stats: None }
    }

    /// This thread's view of the bound execution's image, built at its first probe on the thread (a worker's claims may
    /// move between pool threads) or of a new execution's image.
    fn view(&mut self) -> PgResult<&mut View> {
        let filed = self.filed.as_ref().ok_or_else(|| crate::pg_err("gravity: a worker index probe without its execution's image"))?;
        let me = std::thread::current().id();
        // no reference count of the shared image is touched per probe (every participant's would contend on it)
        if self.view.as_ref().is_none_or(|v| v.thread != me || !Arc::ptr_eq(&v.filed, filed)) {
            let filed = filed.clone();
            self.drop_view();
            let scratch = NodeScratch::new("GravityIndexProbe");
            let mcx = scratch.mcx();
            // the view's fields hold `filed` (the image) and `scratch` (the context) past `rel` and `scan` (`View`)
            let img = raw::image(&filed);
            let rel = nbtree::ImageRel::new(img, mcx)?;
            let scan = rel.begin_scan(mcx, self.nkeys as i32, filed.in_recovery)?;
            self.stats = filed.stats.map(|shared| (filed.img.oid(), shared));
            self.view = Some(View { scan, rel, filed, thread: me, scratch });
        }
        Ok(self.view.as_mut().expect("built"))
    }

    /// The view ended: its scan closed on the thread that built it (a pin is that thread's), its block counts taken.
    fn drop_view(&mut self) {
        let Some(mut v) = self.view.take() else { return };
        self.counts.2 += v.rel.blocks_fetched();
        self.counts.3 += v.rel.blocks_hit();
        if v.thread == std::thread::current().id() {
            let _ = nbtree::btendscan(&mut v.scan);
        }
    }
}

impl SampleFilter for ProbeFilter {
    const SPARSE: bool = true;
    const PAGE: bool = true;
    const BITMAP: bool = true;
    const RING: bool = false;

    fn home(&mut self, owner: crate::ops::scan::heap_source::HeldOwner) {
        self.pages.set_owner(owner);
    }

    /// One outer row's probe: the site's key words into the keys, the index's TIDs for them in block order. A NULL key
    /// argument matches nothing (as `IndexSource::seek`).
    fn start(&mut self, keys: &[Word], keys_null: &[u8]) -> PgResult<()> {
        self.pages.clear();
        let n = self.keys.len();
        if keys.len() < n || keys_null[..n].iter().any(|&x| x != 0) {
            return Ok(());
        }
        // bounded("small-fixed: one argument per scan key, at most INDEX_MAX_KEYS")
        for (k, &w) in self.keys.iter_mut().zip(keys) {
            k.sk_argument = datum::Datum::from_u64(w);
        }
        let mut tids = self.pages.take_tids();
        let keys = core::mem::take(&mut self.keys);
        let probed = self.view().and_then(|v| tids.with_vec(|t| v.rel.probe(&mut v.scan, &keys, t))); // alloc: Node charged owner
        self.keys = keys;
        self.pages.put_tids(tids);
        let got = probed?;
        self.counts.0 += 1;
        self.counts.1 += got as u64;
        // a probe's TIDs are as many as its matches: the sort polls
        self.pages.order(&mut cancelpoll::step("index_probe start: one probe's TIDs sorted")).map_err(|_| crate::pg_err("gravity: an index probe's sort stopped for its statement's cancel"))?;
        Ok(())
    }

    fn next_block(&mut self, nblocks: BlockNumber) -> PgResult<Option<BlockNumber>> {
        self.pages.next_block(nblocks)
    }

    fn keep(&mut self, _blk: BlockNumber, _maxoff: u16, vis: &mut [u16]) -> usize {
        vis.len()
    }

    /// The visible tuples whose HOT chain root is a probed offset (tid_pages.rs).
    fn keep_page(&mut self, _blk: BlockNumber, page: PageRef<'_>, vis: &mut [u16]) -> PgResult<usize> {
        self.pages.keep_page(page, vis)
    }

    /// The execution's image from its `Resolved` (the binding's): the one its leader captured for this registry entry,
    /// never a recipe's (another statement's or session's, D3117).
    fn bind(&mut self, b: &gravity_driver::BindingRef<'_>) -> PgResult<()> {
        let resolved = b.resolved.and_then(|r| r.downcast_ref::<crate::ops::scan::pgrc::granule_source::Resolved>());
        self.filed = resolved.and_then(|r| r.probe_image(self.site)).cloned();
        Ok(())
    }

    /// The index's counts since the bind, landed on the leader's thread at the unbind (bitmap_image.rs's).
    fn unbind(&mut self) {
        self.drop_view();
        self.filed = None;
        let (scans, tuples, fetched, hit) = core::mem::take(&mut self.counts);
        if let Some((index, shared)) = self.stats {
            pgstat::relation::pgstat_count_index_scan_batched(index, shared, scans, tuples, 0);
            pgstat::relation::pgstat_count_buffer_reads(index, shared, fetched, hit);
        }
    }
}

impl Drop for ProbeFilter {
    fn drop(&mut self) {
        self.drop_view();
    }
}

/// The site's lanes are ones the worker's heap read publishes: attribute lanes, then at most a last RowId lane.
pub fn lanes_served(desc: &gravity_program::SiteDesc) -> bool {
    use gravity_program::LaneId;
    let l = desc.lanes();
    let n = l.len() - l.last().is_some_and(|x| x.id == LaneId::RowId) as usize;
    l[..n].iter().all(|x| matches!(x.id, LaneId::Attr(a) if a > 0))
}

/// A worker's source for recipe `p` at registry entry `site` (`HostRegistry::derive_worker_source`).
pub fn derive(p: &WorkerProbe, site: usize, a: gravity_driver::ArenaRef) -> gravity_driver::SourceBox {
    gravity_driver::box_source(crate::ops::scan::heap_source::HeapRangeSource::with_sample(p.range.clone(), ProbeFilter::new(p, site)).serving_worker(), a)
}

pub const SIZE: usize = core::mem::size_of::<crate::ops::scan::heap_source::HeapRangeSource<ProbeFilter>>();

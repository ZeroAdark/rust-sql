
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use ::mcx::Mcx;
use ::types_core::{BlockNumber, Buffer, ForkNumber, InvalidBuffer, Oid};
use ::types_error::{PgError, PgResult};
use ::types_fmgr::FmgrInfo;
use ::types_nbtree::{BTMetaPageData, BTScanOpaqueData, SkipSupportData, BTORDER_PROC};
use ::types_relscan::{IndexScanDescData, IndexScanOpaque};
use ::types_scan::scankey::ScanKeyData;
use ::types_scan::sdir::ForwardScanDirection;
use ::types_snapshot::SnapshotData;
use ::types_storage::ReadBufferMode;
use ::types_tuple::itemptr::ItemPointerData;
use ::types_tuple::TupleDescData;

use crate::image::IndexImage;
use crate::readrel::BtReadRel;

/// A thread's view of an `IndexImage`. Not `Send`: build one per thread.
///
/// Serializable transactions are out of scope: the predicate-lock methods
/// fail when the calling thread's isolation level is serializable and are
/// no-ops otherwise, so a caller must not run an image scan under
/// SERIALIZABLE.
pub struct ImageRel<'img, 'mcx> {
    img: &'img IndexImage,
    tupdesc: Rc<TupleDescData<'mcx>>,
    procs: RefCell<Vec<(Oid, FmgrInfo)>>,
    meta: Cell<Option<BTMetaPageData>>,
    fetched: Cell<u64>,
    hit: Cell<u64>,
}

impl<'img, 'mcx> ImageRel<'img, 'mcx> {
    pub fn new(img: &'img IndexImage, mcx: Mcx<'mcx>) -> PgResult<Self> {
        if img.local_buffers {
            return Err(image_error(img, "index image: backend-local buffers"));
        }
        let mut compact = ::mcx::vec_with_capacity_in(mcx, img.compact.len())?;
        for a in &img.compact {
            compact.push(a.thaw());
        }
        let mut attrs = ::mcx::vec_with_capacity_in(mcx, img.attrs.len())?;
        for a in &img.attrs {
            attrs.push(*a);
        }
        let tupdesc = TupleDescData {
            natts: img.td_natts,
            tdtypeid: img.td_typeid,
            tdtypmod: img.td_typmod,
            tdrefcount: 1,
            constr: None,
            compact_attrs: compact,
            attrs,
        };
        Ok(ImageRel {
            img,
            tupdesc: Rc::new(tupdesc),
            procs: RefCell::new(Vec::new()),
            meta: Cell::new(img.metapage),
            fetched: Cell::new(0),
            hit: Cell::new(0),
        })
    }

    pub fn image(&self) -> &'img IndexImage {
        self.img
    }

    pub fn blocks_fetched(&self) -> u64 {
        self.fetched.get()
    }

    pub fn blocks_hit(&self) -> u64 {
        self.hit.get()
    }

    pub fn reset_counters(&self) {
        self.fetched.set(0);
        self.hit.set(0);
    }

    /// btbeginscan for this view: a scan descriptor with no index relation
    /// attached, driven by `btrescan`, `btgettuple_over` / `btgetbitmap_over`
    /// with this view, and `btendscan`. `xact_started_in_recovery` is the
    /// owning backend's `TransactionStartedDuringRecovery`. The caller must
    /// never set `kill_prior_tuple` on it.
    pub fn begin_scan(
        &self,
        mcx: Mcx<'mcx>,
        nkeys: i32,
        xact_started_in_recovery: bool,
    ) -> PgResult<IndexScanDescData<'mcx>> {
        let so = BTScanOpaqueData::alloc_in(mcx)?;
        let mut scan = ::types_relscan::index_scan_without_relation(
            mcx,
            nkeys,
            0,
            IndexScanOpaque::Btree(so),
            xact_started_in_recovery,
        )?;
        scan.xs_itupdesc = Some(self.tupdesc.clone());
        Ok(scan)
    }

    /// One point probe per outer row: restarts `scan` (from `begin_scan` on
    /// this view) with `keys` and appends every matching index TID to `out`,
    /// returning how many were appended. Index TIDs only: visibility and the
    /// heap fetch are the caller's. `keys` (and any array they point at) live
    /// in the calling thread's context; `kill_prior_tuple` is never set.
    pub fn probe(
        &self,
        scan: &mut IndexScanDescData<'_>,
        keys: &[ScanKeyData],
        out: &mut Vec<ItemPointerData>,
    ) -> PgResult<usize> {
        debug_assert!(!scan.kill_prior_tuple);
        crate::btrescan(scan, Some(keys))?;
        let start = out.len();
        while crate::btgettuple_over(scan, self, ForwardScanDirection)? {
            out.push(scan.xs_heaptid);
        }
        Ok(out.len() - start)
    }

    fn family_known(&self, opfamily: Oid) -> bool {
        self.img.families.contains(&opfamily)
    }

    fn resolve(&self, proc: Oid) -> PgResult<FmgrInfo> {
        if let Some((_, fi)) = self.procs.borrow().iter().find(|(p, _)| *p == proc) {
            return Ok(fi.clone());
        }
        if !self.img.invocable(proc) {
            return Err(image_error(
                self.img,
                &format!("index image: function {proc} is not in the image"),
            ));
        }
        let Some(builtin) = ::fmgr_core::fmgr_isbuiltin(proc) else {
            return Err(image_error(
                self.img,
                &format!("index image: function not built in ({proc})"),
            ));
        };
        let mut fi = FmgrInfo::unresolved();
        ::fmgr_core::fmgr_info_from_builtin_into(builtin, proc, &mut fi);
        self.procs.borrow_mut().push((proc, fi.clone()));
        Ok(fi)
    }


}

#[cold]
#[inline(never)]
fn image_error(img: &IndexImage, what: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("{what} (index \"{}\")", img.name)))
}

impl BtReadRel for ImageRel<'_, '_> {
    #[inline]
    fn name(&self) -> &str {
        &self.img.name
    }

    #[inline]
    fn indnatts(&self) -> i32 {
        self.img.indnatts
    }

    #[inline]
    fn indnkeyatts(&self) -> i32 {
        self.img.indnkeyatts
    }

    #[inline]
    fn tupdesc(&self) -> &TupleDescData<'_> {
        &self.tupdesc
    }

    #[inline]
    fn opcintype(&self, i: usize) -> Oid {
        self.img.opcintype[i]
    }

    #[inline]
    fn opfamily(&self, i: usize) -> Oid {
        self.img.opfamily[i]
    }

    #[inline]
    fn indoption(&self) -> &[i16] {
        &self.img.indoption
    }

    #[inline]
    fn indcollation(&self, i: usize) -> Oid {
        self.img.indcollation[i]
    }

    #[inline]
    fn amcache_get(&self) -> Option<BTMetaPageData> {
        self.meta.get()
    }

    #[inline]
    fn amcache_set(&self, metad: Option<BTMetaPageData>) {
        self.meta.set(metad)
    }

    fn order_procinfo(&self, attno: usize) -> PgResult<FmgrInfo> {
        let opcintype = self.img.opcintype[attno - 1];
        let proc = self.opfamily_proc(
            self.img.opfamily[attno - 1],
            opcintype,
            opcintype,
            BTORDER_PROC as i16,
        )?;
        if proc == 0 {
            return Err(crate::search::missing_support_function(self, attno));
        }
        self.resolve(proc)
    }

    #[inline]
    fn pgstat_enabled(&self) -> bool {
        self.img.pgstat_enabled
    }

    fn read_buffer(&self, blkno: BlockNumber) -> PgResult<Buffer> {
        let hit0 = ::bufmgr::counters::shared_blks_hit();
        let buf = ::bufmgr_seams::read_buffer_without_relcache::call(
            self.img.locator,
            ForkNumber::MAIN_FORKNUM,
            blkno,
            ReadBufferMode::Normal,
            None,
            self.img.permanent,
        )?;
        self.fetched.set(self.fetched.get() + 1);
        self.hit
            .set(self.hit.get() + (::bufmgr::counters::shared_blks_hit() - hit0));
        Ok(buf)
    }

    fn release_and_read_buffer(&self, old: Buffer, blkno: BlockNumber) -> PgResult<Buffer> {
        if old != InvalidBuffer {
            ::bufmgr_seams::release_buffer::call(old)?;
        }
        self.read_buffer(blkno)
    }

    fn predicate_lock_page(&self, _blkno: BlockNumber, _snapshot: &SnapshotData<'_>) -> PgResult<()> {
        serializable_refusal(self.img)
    }

    fn predicate_lock_relation(&self, _snapshot: &SnapshotData<'_>) -> PgResult<()> {
        serializable_refusal(self.img)
    }

    fn opfamily_proc(
        &self,
        opfamily: Oid,
        lefttype: Oid,
        righttype: Oid,
        procnum: i16,
    ) -> PgResult<Oid> {
        if !self.family_known(opfamily) {
            return Err(image_error(
                self.img,
                &format!("index image: no support procedures for operator family {opfamily}"),
            ));
        }
        Ok(self
            .img
            .amproc
            .get(&(opfamily, lefttype, righttype, procnum))
            .copied()
            .unwrap_or(0))
    }

    fn opfamily_member(
        &self,
        opfamily: Oid,
        lefttype: Oid,
        righttype: Oid,
        strategy: i16,
    ) -> PgResult<Oid> {
        if !self.family_known(opfamily) {
            return Err(image_error(
                self.img,
                &format!("index image: no operators for operator family {opfamily}"),
            ));
        }
        Ok(self
            .img
            .amop
            .get(&(opfamily, lefttype, righttype, strategy))
            .copied()
            .unwrap_or(0))
    }

    fn opcode(&self, opno: Oid) -> PgResult<Oid> {
        match self.img.opcode.get(&opno) {
            Some(p) => Ok(*p),
            None => Err(image_error(
                self.img,
                &format!("index image: no function for operator {opno}"),
            )),
        }
    }

    #[inline]
    fn fmgr_info(&self, proc: Oid) -> PgResult<FmgrInfo> {
        self.resolve(proc)
    }

    fn typlenbyvalalign(&self, typid: Oid) -> PgResult<(i16, bool, i8)> {
        match self.img.types.get(&typid) {
            Some(t) => Ok(*t),
            None => Err(image_error(
                self.img,
                &format!("index image: no type shape for type {typid}"),
            )),
        }
    }

    fn skip_support(
        &self,
        opfamily: Oid,
        opcintype: Oid,
        reverse: bool,
    ) -> PgResult<Option<SkipSupportData>> {
        let Some((_, sk)) = self.img.skip.iter().find(|(k, _)| *k == (opfamily, opcintype)) else {
            return Err(image_error(
                self.img,
                &format!("index image: no skip support entry for ({opfamily},{opcintype})"),
            ));
        };
        Ok(sk.map(|mut s| {
            if reverse {
                core::mem::swap(&mut s.low_elem, &mut s.high_elem);
                core::mem::swap(&mut s.decrement, &mut s.increment);
            }
            s
        }))
    }
}

fn serializable_refusal(img: &IndexImage) -> PgResult<()> {
    if ::xact::IsolationIsSerializable() {
        return Err(image_error(img, "index image: scan under SERIALIZABLE isolation"));
    }
    Ok(())
}

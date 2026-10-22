use core::mem::MaybeUninit;

use ::datum::Datum;
use ::mcx::{Allocator, Mcx, PgBox, PgVec};
use ::types_core::{
    BlockNumber, Buffer, BufferIsValid, InvalidBlockNumber, InvalidBuffer, OffsetNumber, Size,
    XLogRecPtr,
};
use ::types_error::PgResult;
use ::types_fmgr::FmgrInfo;
use ::types_scan::scankey::ScanKeyData;
use ::types_scan::sdir::{NoMovementScanDirection, ScanDirection};
use ::types_storage::storage::LocationIndex;
use ::types_tuple::itemptr::ItemPointerData;

use crate::page::MaxTIDsPerBTreePage;

pub struct BTStackData<'mcx> {
    pub bts_blkno: BlockNumber,
    pub bts_offset: OffsetNumber,
    pub bts_parent: Option<&'mcx mut BTStackData<'mcx>>,
}

pub type BTStack<'mcx> = Option<&'mcx mut BTStackData<'mcx>>;

const _: () = assert!(!core::mem::needs_drop::<BTStackData>());

// C sizes scankeys[INDEX_MAX_KEYS] as a keysz-sized flexible array member; keysz = scankeys.len().
pub struct BTScanInsertData<'mcx> {
    pub heapkeyspace: bool,
    pub allequalimage: bool,
    pub anynullkeys: bool,
    pub nextkey: bool,
    pub backward: bool,
    pub scantid: Option<ItemPointerData>,
    pub scankeys: PgVec<'mcx, ScanKeyData>,
}

impl BTScanInsertData<'_> {
    #[inline]
    pub fn keysz(&self) -> i32 {
        self.scankeys.len() as i32
    }
}

pub struct BTInsertStateData<'mcx> {
    // C's IndexTuple pointer: the caller-built on-image index tuple bytes.
    pub itup: &'mcx [u8],
    pub itemsz: Size,
    pub itup_key: BTScanInsertData<'mcx>,
    pub buf: Buffer,
    pub bounds_valid: bool,
    pub low: OffsetNumber,
    pub stricthigh: OffsetNumber,
    pub postingoff: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BTScanPosItem {
    pub heapTid: ItemPointerData,
    pub indexOffset: OffsetNumber,
    pub tupleOffset: LocationIndex,
}

const _: () = assert!(core::mem::size_of::<BTScanPosItem>() == 10);

// items[] is MaybeUninit for C palloc parity: btbeginscan never zero-fills the
// item arrays; only [firstItem, lastItem] is ever written then read.
pub struct BTScanPosData<'mcx> {
    pub buf: Buffer,
    pub currPage: BlockNumber,
    pub prevPage: BlockNumber,
    pub nextPage: BlockNumber,
    pub lsn: XLogRecPtr,
    pub dir: ScanDirection,
    pub nextTupleOffset: i32,
    pub moreLeft: bool,
    pub moreRight: bool,
    pub firstItem: i32,
    pub lastItem: i32,
    pub itemIndex: i32,
    /// C's items[MaxTIDsPerBTreePage] (~13.6 KB), made as the page reads need it (`set_item`): BT_POS_FEW_ITEMS at the
    /// first item saved below that index, all MaxTIDsPerBTreePage at the first one past it. A point probe's page reads
    /// fit the few, so a scan kept between statements (a parked probe) holds ~160 bytes of items, not ~13.6 KB.
    items: PgVec<'mcx, MaybeUninit<BTScanPosItem>>,
}

/// The items a position makes at its first item saved below this index (`BTScanPosData::set_item`).
pub const BT_POS_FEW_ITEMS: usize = 16;

impl<'mcx> BTScanPosData<'mcx> {
    /// A position in its own allocation (the mark's, `BTScanOpaqueData::markPos`): its header as `init_header` leaves
    /// it, no items made.
    pub fn alloc_in(mcx: Mcx<'mcx>) -> PgResult<PgBox<'mcx, Self>> {
        let layout = core::alloc::Layout::new::<Self>();
        let ptr = Allocator::allocate(&mcx, layout).map_err(|_| mcx.oom_boxed(layout.size()))?;
        let p = ptr.as_ptr() as *mut Self;
        // SAFETY: a fresh allocation of `layout`; init_header writes every field.
        unsafe {
            Self::init_header(p, mcx);
            Ok(PgBox::from_raw_in(p, mcx))
        }
    }

    /// # Safety
    /// `p` must be valid for writes of `Self`; every field is written, the items empty (none made).
    pub unsafe fn init_header(p: *mut Self, mcx: Mcx<'mcx>) {
        (&raw mut (*p).buf).write(InvalidBuffer);
        (&raw mut (*p).currPage).write(InvalidBlockNumber);
        (&raw mut (*p).prevPage).write(InvalidBlockNumber);
        (&raw mut (*p).nextPage).write(InvalidBlockNumber);
        (&raw mut (*p).lsn).write(0);
        (&raw mut (*p).dir).write(NoMovementScanDirection);
        (&raw mut (*p).nextTupleOffset).write(0);
        (&raw mut (*p).moreLeft).write(false);
        (&raw mut (*p).moreRight).write(false);
        (&raw mut (*p).firstItem).write(0);
        (&raw mut (*p).lastItem).write(0);
        (&raw mut (*p).itemIndex).write(0);
        (&raw mut (*p).items).write(PgVec::new_in(mcx));
    }

    /// # Safety
    /// `items[i]` must have been written since the position was loaded
    /// (readers stay within [firstItem, lastItem]).
    #[inline]
    pub unsafe fn item(&self, i: usize) -> BTScanPosItem {
        debug_assert!(i >= self.firstItem as usize && i <= self.lastItem as usize);
        self.items[i].assume_init()
    }

    #[inline]
    pub fn set_item(&mut self, i: usize, item: BTScanPosItem) -> PgResult<()> {
        if i >= self.items.len() {
            self.make_items(i + 1)?;
        }
        self.items[i].write(item);
        Ok(())
    }

    /// Items `[0, n)` made (`n` <= MaxTIDsPerBTreePage): BT_POS_FEW_ITEMS when they hold `n`, else all of them, what
    /// was written before kept.
    #[cold]
    #[inline(never)]
    pub fn make_items(&mut self, n: usize) -> PgResult<()> {
        debug_assert!(n <= MaxTIDsPerBTreePage);
        if n <= self.items.len() {
            return Ok(());
        }
        let to = if n <= BT_POS_FEW_ITEMS { BT_POS_FEW_ITEMS } else { MaxTIDsPerBTreePage };
        let more = to - self.items.len();
        if self.items.try_reserve_exact(more).is_err() {
            return Err(self.items.allocator().oom_boxed(more * core::mem::size_of::<BTScanPosItem>()));
        }
        // SAFETY: capacity >= `to` (reserved above); MaybeUninit items need no initialization.
        unsafe { self.items.set_len(to) };
        Ok(())
    }

    /// btmarkpos / btrestrpos's offsetof-bounded copy: the live items prefix `[0, n)` of `from` into this position's,
    /// made here as needed.
    pub fn copy_items(&mut self, from: &BTScanPosData<'_>, n: usize) -> PgResult<()> {
        if n > 0 {
            self.make_items(n)?;
            self.items[..n].copy_from_slice(&from.items[..n]);
        }
        Ok(())
    }

    /// The items made (their bytes are `items_made() * 10`).
    pub fn items_made(&self) -> usize {
        self.items.len()
    }
}

#[inline]
pub fn BTScanPosIsPinned(scanpos: &BTScanPosData) -> bool {
    debug_assert!(scanpos.currPage != InvalidBlockNumber || !BufferIsValid(scanpos.buf));
    BufferIsValid(scanpos.buf)
}

#[inline]
pub fn BTScanPosIsValid(scanpos: &BTScanPosData) -> bool {
    debug_assert!(scanpos.currPage != InvalidBlockNumber || !BufferIsValid(scanpos.buf));
    scanpos.currPage != InvalidBlockNumber
}

#[inline]
pub fn BTScanPosInvalidate(scanpos: &mut BTScanPosData) {
    scanpos.buf = InvalidBuffer;
    scanpos.currPage = InvalidBlockNumber;
}

// C divergence: callbacks drop C's unused Relation arg; by-ref skip support
// (uuid) is unimplemented, so no allocator is threaded either.
pub type SkipSupportIncDec = fn(Datum, &mut bool) -> Datum;

#[derive(Clone, Copy)]
pub struct SkipSupportData {
    pub low_elem: Datum,
    pub high_elem: Datum,
    pub decrement: SkipSupportIncDec,
    pub increment: SkipSupportIncDec,
}

// C divergence: low_compare/high_compare are owned copies of the arrayKeyData keys, not
// pointers into it (same mutability/lifetime). num_elems == -1 = skip array (elem_values empty).
pub struct BTArrayKeyInfo<'mcx> {
    pub scan_key: i32,
    pub num_elems: i32,
    pub elem_values: PgVec<'mcx, Datum>,
    pub cur_elem: i32,
    pub attlen: i16,
    pub attbyval: bool,
    pub null_elem: bool,
    pub sksup: Option<SkipSupportData>,
    pub low_compare: Option<ScanKeyData>,
    pub high_compare: Option<ScanKeyData>,
    // The by-ref skip array's current sk_argument storage: C datumCopy/pfree
    // pairs on every element change; the arena cannot free, so one buffer is
    // reused instead (capacity-bounded, not scan-visited-keys-bounded).
    pub skip_arg: PgVec<'mcx, u8>,
}


pub struct BTArrayScratch<'mcx> {
    /// _bt_preprocess_array_keys' modified copy of the input keys (C's arrayKeyData).
    pub keys: PgVec<'mcx, ScanKeyData>,
    pub key_map: PgVec<'mcx, i32>,
    pub nulls: PgVec<'mcx, bool>,
    pub spare_elems: PgVec<'mcx, PgVec<'mcx, Datum>>,
    pub images: PgVec<'mcx, PgVec<'mcx, u8>>,
    pub images_used: usize,
}

impl<'mcx> BTArrayScratch<'mcx> {
    pub fn new_in(mcx: Mcx<'mcx>) -> Self {
        BTArrayScratch {
            keys: PgVec::new_in(mcx),
            key_map: PgVec::new_in(mcx),
            nulls: PgVec::new_in(mcx),
            spare_elems: PgVec::new_in(mcx),
            images: PgVec::new_in(mcx),
            images_used: 0,
        }
    }

    pub fn take_elems(spare: &mut PgVec<'mcx, PgVec<'mcx, Datum>>, mcx: Mcx<'mcx>) -> PgVec<'mcx, Datum> {
        match spare.pop() {
            Some(mut v) => {
                v.clear();
                v
            }
            None => PgVec::new_in(mcx),
        }
    }

    /// Keeps an element buffer in `spare` (`spare_elems`) for a later array (one that never allocated is dropped).
    pub fn retire_elems(spare: &mut PgVec<'mcx, PgVec<'mcx, Datum>>, v: PgVec<'mcx, Datum>) {
        if v.capacity() > 0 && spare.try_reserve(1).is_ok() {
            spare.push(v);
        }
    }
}

pub struct BTScanOpaqueData<'mcx> {
    pub qual_ok: bool,
    pub numberOfKeys: i32,
    pub keyData: PgVec<'mcx, ScanKeyData>,

    pub numArrayKeys: i32,
    pub skipScan: bool,
    pub needPrimScan: bool,
    pub scanBehind: bool,
    pub oppositeDirCheck: bool,
    pub arrayKeys: PgVec<'mcx, BTArrayKeyInfo<'mcx>>,
    pub orderProcs: PgVec<'mcx, FmgrInfo>,
    pub arrayScratch: BTArrayScratch<'mcx>,

    pub killedItems: PgVec<'mcx, i32>,
    pub numKilled: i32,
    pub dropPin: bool,
    pub lsnWithPin: bool,

    pub currTuples: Option<PgVec<'mcx, u8>>,
    pub markTuples: Option<PgVec<'mcx, u8>>,

    pub markItemIndex: i32,

    pub currPos: BTScanPosData<'mcx>,
    /// The mark (btmarkpos / btrestrpos: a merge join's inner scan), made at the first mark the scan saves off its page
    /// (`copy_scanpos`): a scan that never marks, every point probe among them, holds no second ~13.6 KB position.
    pub markPos: Option<PgBox<'mcx, BTScanPosData<'mcx>>>,
}

impl<'mcx> BTScanOpaqueData<'mcx> {
    // btbeginscan's palloc + header assignment, in place.
    pub fn alloc_in(mcx: Mcx<'mcx>) -> PgResult<PgBox<'mcx, Self>> {
        let layout = core::alloc::Layout::new::<Self>();
        let ptr = Allocator::allocate(&mcx, layout).map_err(|_| mcx.oom_boxed(layout.size()))?;
        let p = ptr.as_ptr() as *mut Self;
        // SAFETY: fresh allocation of `layout`; every non-MaybeUninit field is
        // written exactly once before the box is formed.
        unsafe {
            (&raw mut (*p).qual_ok).write(false);
            (&raw mut (*p).numberOfKeys).write(0);
            (&raw mut (*p).keyData).write(PgVec::new_in(mcx));
            (&raw mut (*p).numArrayKeys).write(0);
            (&raw mut (*p).skipScan).write(false);
            (&raw mut (*p).needPrimScan).write(false);
            (&raw mut (*p).scanBehind).write(false);
            (&raw mut (*p).oppositeDirCheck).write(false);
            (&raw mut (*p).arrayKeys).write(PgVec::new_in(mcx));
            (&raw mut (*p).orderProcs).write(PgVec::new_in(mcx));
            (&raw mut (*p).arrayScratch).write(BTArrayScratch::new_in(mcx));
            (&raw mut (*p).killedItems).write(PgVec::new_in(mcx));
            (&raw mut (*p).numKilled).write(0);
            (&raw mut (*p).dropPin).write(false);
            (&raw mut (*p).lsnWithPin).write(false);
            (&raw mut (*p).currTuples).write(None);
            (&raw mut (*p).markTuples).write(None);
            (&raw mut (*p).markItemIndex).write(-1);
            BTScanPosData::init_header(&raw mut (*p).currPos, mcx);
            (&raw mut (*p).markPos).write(None);
            Ok(PgBox::from_raw_in(p, mcx))
        }
    }
}

pub struct BTReadPageState<'p> {
    pub minoff: OffsetNumber,
    pub maxoff: OffsetNumber,
    // High key (forward) / first non-pivot tuple (backward) bytes, borrowed from the
    // pinned leaf page like C's IndexTuple pointer; None on the rightmost/leftmost page.
    pub finaltup: Option<&'p [u8]>,
    pub page: &'p [u8],
    pub firstpage: bool,
    pub forcenonrequired: bool,
    pub startikey: i32,

    pub offnum: OffsetNumber,

    pub skip: OffsetNumber,
    pub continuescan: bool,

    pub rechecks: i16,
    pub targetdistance: i16,
    pub nskipadvances: i16,
}

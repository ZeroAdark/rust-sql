//! The scan family's raw memory (T-unsafe): an `unsafe` the family's host files need, behind a safe wrapper with its
//! safety contract (the agg family's raw.rs pattern).
#![allow(unsafe_code)]

use datum::Datum;
use gravity_driver::mem::root::Root;
use mcx::Mcx;
use nbtree::itup::index_getattr;
use types_core::AttrNumber;
use types_fmgr::LocalFcinfo;
use types_relscan::IndexScanDescData;
use types_tuple::{heap_getattr, HeapTupleData, TupleDescData};

/// D2181: where an index seek expands a bound search array whose image is compressed or external (nbtree borrows a
/// plain or short image only). Made at the first seek that expands one and reset at each later one, so a nest loop's
/// inner keeps one rescan's images, not one per rescan. The owner declares it before the keys' scan state is dropped
/// with it (a field of the same host).
#[derive(Default)]
pub(crate) struct SeekImages(Option<Root>);

impl SeekImages {
    /// The region for this seek's images; `reset` empties it first (the first expansion of a seek).
    pub(crate) fn region(&mut self, reset: bool) -> Mcx<'static> {
        let root = self.0.get_or_insert_with(|| Root::bump("GravityIndexArrays"));
        if reset {
            root.reset();
        }
        // SAFETY: the images are plain bytes read through the seek's keys only in the preprocessing of the rescan that
        // follows the seek (a bitmap source's: the build that follows it, D4528); the next reset is the next seek's,
        // which rebinds those keys first, and the root drops with its host, after the scan that read them.
        unsafe { root.held() }
    }
}

/// Arms key compare frame `fc`'s result context (`set_result_mcx`) with the source's exec context `mcx` before a key
/// operator's call (lint R6; construct bundle scan-05: the one call the scan hosts' key rechecks made under `unsafe`, moved
/// as is). The caller re-arms before every call through `fc`, and `mcx` (the exec context) outlives the call: it resets at
/// unbind.
pub(crate) fn arm_result_mcx(fc: &mut LocalFcinfo<2>, mcx: Mcx<'_>) {
    // SAFETY: the caller's contract above: the armed context stays live and un-reset across the call that follows.
    unsafe { fc.set_result_mcx(mcx) };
}

/// nbtree's `index_getattr` of attribute `attnum` of the AM's tuple image `itup` described by `desc` (lint R6; construct
/// bundle scan-05, moved as is from index_window.rs and indexonly.rs). The caller passes the scan's `xs_itup` (the AM's
/// image of the current row, live until the next `amgettuple` on its descriptor) with its `xs_itupdesc`, and an `attnum`
/// of that descriptor.
pub(crate) fn index_attr(itup: core::ptr::NonNull<u8>, attnum: AttrNumber, desc: &TupleDescData<'_>, isnull: &mut bool) -> Datum {
    // SAFETY: the caller's contract above.
    unsafe { index_getattr(itup.as_ptr(), attnum, desc, isnull) }
}

/// D2831: the AM's image of an index-only row, as PostgreSQL's IndexOnlyNext reads it: a heap tuple the AM reconstructed
/// (`xs_hitup` with `xs_hitupdesc`: SP-GiST's storeGettuple, stored by ExecForceStoreHeapTuple) when there is one, else
/// the index tuple (`xs_itup` with `xs_itupdesc`: StoreIndexTuple). Either lives until the next `amgettuple` on the scan's
/// descriptor, and its descriptor's attribute `i` is the index's column `i`.
#[derive(Clone, Copy)]
pub(crate) enum OnlyRow<'s, 'mcx> {
    Heap(&'s HeapTupleData<'mcx>, &'s TupleDescData<'mcx>),
    Index(core::ptr::NonNull<u8>, &'s TupleDescData<'mcx>),
}

impl<'s, 'mcx> OnlyRow<'s, 'mcx> {
    /// The current row's image of `scan`; `None` where the AM published neither with its descriptor.
    pub(crate) fn of(scan: &'s IndexScanDescData<'mcx>) -> Option<Self> {
        if let Some(t) = scan.xs_hitup.as_ref() {
            return scan.xs_hitupdesc.as_deref().map(|d| Self::Heap(t, d));
        }
        Some(Self::Index(scan.xs_itup?, scan.xs_itupdesc.as_deref()?))
    }

    /// Whether the image is an index tuple: a btree `name` column there is a cstring (StoreIndexTuple's step), where a
    /// reconstructed heap tuple holds the column's own datum.
    pub(crate) fn is_index(self) -> bool {
        matches!(self, Self::Index(..))
    }

    /// Attribute `attnum` (1-based in the image's descriptor), read before the next `amgettuple` on the scan.
    pub(crate) fn attr(self, attnum: AttrNumber, isnull: &mut bool) -> Datum {
        match self {
            Self::Index(itup, desc) => index_attr(itup, attnum, desc, isnull),
            // SAFETY: the AM's live reconstructed tuple formed by heap_form_tuple over `desc` (the type's contract above).
            Self::Heap(tup, desc) => unsafe { heap_getattr(tup, attnum as i32, desc, isnull) },
        }
    }
}

/// The cstring a btree `name` column's index datum `d` addresses (lint R6; construct bundle scan-05, moved as is from
/// indexonly.rs). The caller passes a non-null datum of such a column from `index_attr`: a NUL-terminated cstring inside
/// `xs_itup`, read before the next `amgettuple`.
pub(crate) fn index_cstr<'a>(d: Datum) -> &'a core::ffi::CStr {
    // SAFETY: the caller's contract above.
    unsafe { core::ffi::CStr::from_ptr(d.as_u64() as *const core::ffi::c_char) }
}

/// The heap tuple header at normal line pointer `lp` of heap page `page` (tid_pages.rs's chain walk); `None` for an item
/// shorter than a header. `item_raw` bounds the item to the page image; the caller holds the page's pin and share lock.
pub(crate) fn heap_header<'a>(page: types_storage::bufpage::PageRef<'a>, lp: types_storage::bufpage::ItemIdData) -> Option<&'a types_tuple::HeapTupleHeaderData> {
    let (ptr, len) = page.item_raw(lp);
    if (len as usize) < core::mem::size_of::<types_tuple::HeapTupleHeaderData>() {
        return None;
    }
    // SAFETY: `len` bytes at `ptr` lie in the page image, which the caller's pin keeps for 'a; a heap item is MAXALIGNed
    // and starts with its full header.
    Some(unsafe { &*ptr.cast::<types_tuple::HeapTupleHeaderData>() })
}

/// A bitmap site's recheck qual for one execution (`Resolved::recheck`): an `Rc` over the qual compiled in that execution's
/// context, carried in the statement's `Resolved`, which the pool's threads read as `dyn Any + Send + Sync`. Every bind
/// takes it on the leader's thread: the Node's own hosts at `Node::apply`, a worker slot's in the section's open (driver
/// section.rs `bind_hosts`, the leader's loop before the claims). A pool thread only calls the qual through its bound
/// source, and the `Resolved` drops on the leader.
pub(crate) struct LeaderFilter(crate::FilterFn);

impl LeaderFilter {
    pub(crate) fn new(f: crate::FilterFn) -> LeaderFilter {
        LeaderFilter(f)
    }

    /// The qual, for a bind on the leader's thread (the caller's contract above).
    pub(crate) fn get(&self) -> crate::FilterFn {
        // a count bump on the `Rc` (alloclint REFCOUNT-CLONE: the method form, which clippy's clone_on_ref_ptr resolves)
        self.0.clone()
    }
}

// SAFETY: a pool thread never clones or drops the `Rc`: `Resolved::recheck` is called by `BitmapSource::bind` alone, and
// every bind runs on the leader's thread (a worker slot's too: driver section.rs `bind_hosts` in the section's open, before
// the claims), as does every unbind (the section's close, `cool`, the park) and the `Resolved`'s drop, so the count is
// touched on the leader's thread alone. A worker's claims call the qual through the source's borrow, no count change.
unsafe impl Send for LeaderFilter {}
// SAFETY: as `Send` above: a shared `&Resolved` on a pool thread never reaches the `Rc`.
unsafe impl Sync for LeaderFilter {}

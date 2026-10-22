#![no_std]

extern crate alloc;

#[cfg(any(feature = "std", test))]
extern crate std;

use core::alloc::Layout;
use core::cell::{Cell, RefCell};
use core::fmt;
use core::ptr::NonNull;

pub use allocator_api2::alloc::Allocator;

use allocator_api2::alloc::AllocError;
use ::types_error::{PgError, PgResult, ERRCODE_OUT_OF_MEMORY, ERRCODE_PROGRAM_LIMIT_EXCEEDED};

mod arena_safe;
mod aset;
mod bump;
mod external_allocation_guard;
#[cfg(debug_assertions)]
mod freed;
mod generation;
mod memo_map;
pub mod governor;
pub mod heap;
pub mod ladder;
pub mod ladder_stats;
pub mod reconcile;
pub mod ledger2;
pub mod stdvec;
pub mod revoke;
mod owned;
mod owner;
mod send_root;
mod slab;
mod string;
mod collect;
mod rc;
pub use arena_safe::{ArenaForget, ArenaSafe, ForgetSafe};
pub use aset::{alloc_stats, parked_keeper_bytes};
pub use external_allocation_guard::ExternalAllocationGuard;
pub use memo_map::{PgMemoMap, MEMO_INLINE};
pub use owned::{Bind, McxOwned, PinnedContext};
pub use send_root::{RootA, SendRoot};
pub use string::PgString;
pub use collect::{box_in, concat_in, format_in, raise, reserved_in, to_vec_in, vec_array_in, vec_elem_in, CollectIn, GrowIn, PartitionIn, TryCollectOptionIn, TryCollectResultIn};
pub use rc::PgRc;

/// # Safety: caller asserts the full [`ArenaSafe`] contract; the field list is only a guard.
#[macro_export]
macro_rules! assert_arena_safe {
    ($($ty:ty { $($field:ident : $fty:ty),* $(,)? }),+ $(,)?) => {
        $(
            // SAFETY: asserted by the caller per the ArenaSafe contract.
            unsafe impl $crate::ArenaSafe for $ty {}
            const _: fn() = || {
                fn _assert_field_arena_safe<T: $crate::ArenaSafe>() {}
                $( let _ = _assert_field_arena_safe::<$fty>; )*
            };
        )+
    };
    ($($ty:ty),+ $(,)?) => {
        $(
            // SAFETY: asserted by the caller per the ArenaSafe contract.
            unsafe impl $crate::ArenaSafe for $ty {}
        )+
    };
}

pub type PgVec<'mcx, T> = allocator_api2::vec::Vec<T, Mcx<'mcx>>;
pub type PgBox<'mcx, T> = allocator_api2::boxed::Box<T, Mcx<'mcx>>;
pub type PgHashMap<'mcx, K, V> =
    hashbrown::HashMap<K, V, hashbrown::hash_map::DefaultHashBuilder, Mcx<'mcx>>;
pub type PgFxHashMap<'mcx, K, V> = hashbrown::HashMap<K, V, rustc_hash::FxBuildHasher, Mcx<'mcx>>;

enum Backend {
    // UnsafeCell, not RefCell: palloc's hot path pays no borrow flag (see aset_mut).
    Aset(core::cell::UnsafeCell<aset::AllocSet>),
    #[allow(dead_code)]
    Malloc,
    Bump(core::cell::UnsafeCell<bump::BumpArena>),
    // Leaked owned values run their destructor once at reset.
    BumpDrop(core::cell::UnsafeCell<bump::BumpArena>, RefCell<DropList>),
    BumpForget(core::cell::UnsafeCell<bump::BumpArena>),
    Generation(core::cell::UnsafeCell<generation::GenArena>),
    Slab(core::cell::UnsafeCell<slab::SlabArena>),
}

// Exposed-provenance address (not a borrow-stack sibling of the returned &mut).
struct DropEntry {
    addr: *mut u8,
    glue: unsafe fn(*mut u8),
}

struct DropList {
    entries: alloc::vec::Vec<DropEntry>,
}

impl DropList {
    fn new() -> Self {
        DropList { entries: alloc::vec::Vec::new() }
    }

    // Entries stay in the list so a panicking destructor leaves the remainder for the next reset/drop.
    fn pop(&mut self) -> Option<DropEntry> {
        self.entries.pop()
    }
}

// Borrow released before the entry runs so re-entrancy is safe.
fn run_drop_glue(mut next: impl FnMut() -> Option<DropEntry>) {
    while let Some(entry) = next() {
        // SAFETY: live leaked value of glue's type, Drop suppressed; sole drop, before arena reset.
        unsafe { (entry.glue)(entry.addr) };
    }
}

/// # Safety: `addr` must be an exposed, live, aligned `T` never otherwise dropped.
unsafe fn drop_glue<T>(addr: *mut u8) {
    let p = core::ptr::with_exposed_provenance_mut::<T>(addr as usize);
    core::ptr::drop_in_place(p);
}

unsafe fn drop_glue_noop(_addr: *mut u8) {}

// Never Vec::drop (deallocate through the leaked Mcx is the SB trap).
/// # Safety: `addr` — exposed address of a live arena `PgVec` header, element drops unrun.
unsafe fn drop_glue_vec_elems<T>(addr: *mut u8) {
    let header = core::ptr::with_exposed_provenance_mut::<PgVec<'static, T>>(addr as usize);
    let v: &mut PgVec<'static, T> = &mut *header;
    let len = v.len();
    let data: *mut T = v.as_mut_ptr();
    // len = 0 first guards a re-entrant double-drop.
    v.set_len(0);
    core::ptr::drop_in_place(core::ptr::slice_from_raw_parts_mut(data, len));
}

// Subtree totals summed on demand (C's recursive MemoryContextMemAllocated); charge never walks ancestors.
pub(crate) struct Acct {
    name: Cell<&'static str>,
    ident: RefCell<Option<alloc::string::String>>,
    pub(crate) self_used: Cell<usize>,
    pub(crate) self_peak: Cell<usize>,
    pub(crate) limit: Cell<usize>,
    pub(crate) limited_path: Cell<bool>,
    pub(crate) arena_footprint: Cell<usize>,
    pub(crate) arena_nblocks: Cell<usize>,
    // Per-alloc bumps never touch Acct, so this reads high vs C's live freeptr.
    pub(crate) window_tail: Cell<usize>,
    pub(crate) live_chunk_bytes: Cell<usize>,
    pub(crate) free_chunks: Cell<usize>,
    // slab.c's emptyblocks count; arena_nblocks excludes these for a Slab.
    pub(crate) empty_blocks: Cell<usize>,
    pub(crate) is_bump: bool,
    // An AllocSet whose reset discards live chunks by design, exempt from the leak check exact-accounting contexts get.
    pub(crate) wholesale_reset: Cell<bool>,
    // A `SendRoot`'s context: it refuses children and reset callbacks (both would tie it to their thread).
    pub(crate) send_root: Cell<bool>,
    // An unpooled root (`SendRoot::new_owned_unpooled`): its account node, its child list and its blocks were allocated
    // fresh, not taken from the creating thread's pools, and its drop frees them, parking nothing in the dropping thread's.
    pub(crate) unpooled: bool,
    pub(crate) kind: &'static str,
    pub(crate) parent: Option<AcctRc>,
    pub(crate) children: RefCell<alloc::vec::Vec<AcctWeak>>,
    // LEDGER2 step 2: the context's owner in the memory ledger, its block bytes posted to it as the footprint moves.
    pub(crate) led: crate::owner::Led,
}

impl Acct {
    pub(crate) fn set_blocks(&self, footprint: usize, nblocks: usize) {
        self.set_footprint(footprint);
        self.arena_nblocks.set(nblocks);
    }

    #[inline]
    pub(crate) fn set_footprint(&self, footprint: usize) {
        self.arena_footprint.set(footprint);
        self.led.sync(footprint);
    }

    fn free_bytes(&self) -> usize {
        if self.is_bump {
            self.window_tail.get()
        } else {
            self.arena_footprint.get().saturating_sub(self.live_chunk_bytes.get())
        }
    }

    fn ancestors(&self) -> impl Iterator<Item = &Acct> {
        let mut cur: Option<&Acct> = Some(self);
        core::iter::from_fn(move || {
            let node = cur?;
            cur = node.parent.as_deref();
            Some(node)
        })
    }

    #[cold]
    #[inline(never)]
    fn check_limit_slow(&self, n: usize) -> Result<(), AllocError> {
        for node in self.ancestors() {
            let new = node.subtree_sum().checked_add(n).ok_or(AllocError)?;
            if new > node.limit.get() {
                return Err(AllocError);
            }
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn admit_block(&self, n: usize) {
        crate::governor::admit(n, self.name.get(), self.arena_footprint.get().max(self.self_used.get()));
    }

    #[inline]
    pub(crate) fn check_limit(&self, n: usize) -> Result<(), AllocError> {
        if self.limited_path.get() {
            return self.check_limit_slow(n);
        }
        Ok(())
    }

    pub(crate) fn commit_block(&self, n: usize, footprint: usize, nblocks: usize) {
        let self_new = self.self_used.get() + n;
        self.self_used.set(self_new);
        if self_new > self.self_peak.get() {
            self.self_peak.set(self_new);
        }
        self.set_footprint(footprint);
        self.arena_nblocks.set(nblocks);
    }

    pub(crate) fn uncommit_block(&self, n: usize, footprint: usize, nblocks: usize) {
        self.self_used.set(self.self_used.get().saturating_sub(n));
        self.set_footprint(footprint);
        self.arena_nblocks.set(nblocks);
    }
    fn subtree_sum(&self) -> usize {
        let mut total = self.self_used.get();
        self.children.borrow_mut().retain(|w| match w.upgrade() {
            Some(c) => {
                total = total.saturating_add(c.subtree_sum());
                true
            }
            None => false,
        });
        total
    }

    fn subtree_allocated_sum(&self) -> usize {
        let mut total = if self.kind == "Malloc" {
            self.self_used.get()
        } else {
            self.arena_footprint.get()
        };
        self.children.borrow_mut().retain(|w| match w.upgrade() {
            Some(c) => {
                total = total.saturating_add(c.subtree_allocated_sum());
                true
            }
            None => false,
        });
        total
    }

    fn subtree_peak_sum(&self) -> usize {
        let mut total = self.self_peak.get();
        self.children.borrow_mut().retain(|w| match w.upgrade() {
            Some(c) => {
                total = total.saturating_add(c.subtree_peak_sum());
                true
            }
            None => false,
        });
        total
    }
}

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;

// Parked only when both counts are 0, so reuse cannot alias.
struct AcctInner {
    strong: Cell<usize>,
    weak: Cell<usize>,
    val: MaybeUninit<Acct>,
    // Debug builds: the address the context last handed out a handle at (`MemoryContext::mcx`). It lives on the heap,
    // so a context's moved copy and the stale bytes at its old address see the same value (`MemoryContext::check_home`).
    #[cfg(debug_assertions)]
    home: Cell<usize>,
}

const ACCT_POOL_MAX: usize = 256;

pub(crate) struct PoolMutex<T> {
    locked: core::sync::atomic::AtomicBool,
    val: UnsafeCell<T>,
}

// SAFETY: `with` holds the flag for the whole access; T's values are plain
// process-owned free-list nodes, safe to hand across threads under the lock.
unsafe impl<T> Sync for PoolMutex<T> {}

impl<T> PoolMutex<T> {
    pub(crate) const fn new(val: T) -> Self {
        PoolMutex {
            locked: core::sync::atomic::AtomicBool::new(false),
            val: UnsafeCell::new(val),
        }
    }

    // Single CAS attempt, no spin: a contended pool is bypassed (caller falls through to Global/mimalloc).
    #[inline]
    pub(crate) fn try_with<R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        use core::sync::atomic::Ordering;
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return None;
        }
        // SAFETY: the flag serializes access; released below.
        let r = f(unsafe { &mut *self.val.get() });
        self.locked.store(false, Ordering::Release);
        Some(r)
    }
}

// PGRUST_MCX_BUMP_FAST: 0 = not read yet, 1 = on, 2 = off.
static BUMP_FAST: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// `PGRUST_MCX_BUMP_FAST` (default on; `0` = off): a Bump / BumpDrop / BumpForget context's allocation that fits its
/// open window is served inline at the call site (Mcx::alloc_fast) instead of by the out-of-line allocate_slow. Read
/// once per process (std and test builds; other no_std builds: on); each bump arena copies it at its creation, so the
/// switch costs the inline path no instruction. Off = the old route.
#[inline]
pub(crate) fn bump_fast_on() -> bool {
    use core::sync::atomic::Ordering::Relaxed;
    match BUMP_FAST.load(Relaxed) {
        1 => true,
        2 => false,
        _ => {
            #[cfg(any(feature = "std", test))]
            let on = !std::env::var("PGRUST_MCX_BUMP_FAST").is_ok_and(|v| v.trim() == "0");
            #[cfg(not(any(feature = "std", test)))]
            let on = true;
            BUMP_FAST.store(if on { 1 } else { 2 }, Relaxed);
            on
        }
    }
}


#[cfg(all(feature = "std", not(test)))]
#[inline]
pub fn local_pool_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !std::env::var("PGRUST_MCX_POOL_STRIPE").is_ok_and(|v| v.trim() == "0")
    })
}

/// `Cell<Vec>` take/put keeps every access panic-free: a re-entrant access sees an empty list and falls through to `Global`, never a borrow error.
#[cfg(feature = "std")]
pub(crate) struct LocalStack<T> {
    items: Cell<alloc::vec::Vec<T>>,
    cap: usize,
    dispose: fn(T),
}

#[cfg(feature = "std")]
impl<T> LocalStack<T> {
    pub(crate) const fn new(cap: usize, dispose: fn(T)) -> Self {
        LocalStack { items: Cell::new(alloc::vec::Vec::new()), cap, dispose }
    }

    pub(crate) fn take(&self) -> Option<T> {
        let mut v = self.items.take();
        let r = v.pop();
        self.items.set(v);
        r
    }

    pub(crate) fn give(&self, item: T) {
        let mut v = self.items.take();
        if v.len() >= self.cap {
            self.items.set(v);
            (self.dispose)(item);
            return;
        }
        v.push(item);
        self.items.set(v);
    }

    pub(crate) fn give_wholesale(&self, item: T) {
        let mut v = self.items.take();
        if v.len() >= self.cap {
            for it in v.drain(..) {
                (self.dispose)(it);
            }
        }
        v.push(item);
        self.items.set(v);
    }

    pub(crate) fn drain_all(&self) {
        for it in self.items.take() {
            (self.dispose)(it);
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        let v = self.items.take();
        let n = v.len();
        self.items.set(v);
        n
    }
}

#[cfg(feature = "std")]
impl<T> Drop for LocalStack<T> {
    fn drop(&mut self) {
        for it in self.items.take() {
            (self.dispose)(it);
        }
    }
}

#[cfg(all(feature = "std", not(test)))]
mod tls_pools {
    use super::*;

    fn dispose_acct(p: NonNull<AcctInner>) {
        // SAFETY: pool entries come from acct_alloc_global with `val` already
        // dropped; the list is the sole owner.
        unsafe { crate::heap::Heap.deallocate(p.cast(), Layout::new::<AcctInner>()) };
    }

    // Uncapped: `give` must never free early.
    fn free_retired_root(addr: usize) {
        // SAFETY: `addr` is a `Box::into_raw` shell from `session_root_mut`,
        // retired (arena released, poisoned) and buried exactly once by the
        // Roots-phase cleanup; the burying thread is exiting and no live
        // reference remains (every TLS holder is ManuallyDrop).
        unsafe { drop(alloc::boxed::Box::from_raw(addr as *mut MemoryContext)) };
    }

    std::thread_local! {
        pub(crate) static ACCT: LocalStack<NonNull<AcctInner>> =
            const { LocalStack::new(ACCT_POOL_MAX, dispose_acct) };
        pub(crate) static CHILD_VECS: LocalStack<alloc::vec::Vec<AcctWeak>> =
            const { LocalStack::new(CHILD_VEC_POOL_MAX, core::mem::drop) };
        pub(crate) static RETIRED_ROOTS: LocalStack<usize> =
            const { LocalStack::new(usize::MAX, free_retired_root) };
    }
}

type AcctPool = PoolMutex<alloc::vec::Vec<NonNull<AcctInner>>>;
static ACCT_POOL: AcctPool = AcctPool::new(alloc::vec::Vec::new());

// Per-query context churn must not pay a malloc/free for a 1-2 entry Vec.
type ChildVecPool = PoolMutex<alloc::vec::Vec<alloc::vec::Vec<AcctWeak>>>;
static CHILD_VEC_POOL: ChildVecPool = ChildVecPool::new(alloc::vec::Vec::new());
const CHILD_VEC_POOL_MAX: usize = 64;

#[inline]
fn child_vec_take() -> alloc::vec::Vec<AcctWeak> {
    #[cfg(all(feature = "std", not(test)))]
    if local_pool_on() {
        return tls_pools::CHILD_VECS
            .try_with(|s| s.take())
            .ok()
            .flatten()
            .unwrap_or_default();
    }
    CHILD_VEC_POOL.try_with(|s| s.pop()).flatten().unwrap_or_default()
}

#[inline]
fn child_vec_give(v: alloc::vec::Vec<AcctWeak>) {
    debug_assert!(v.is_empty());
    if v.capacity() == 0 {
        return;
    }
    #[cfg(all(feature = "std", not(test)))]
    if local_pool_on() {
        let _ = tls_pools::CHILD_VECS.try_with(move |s| s.give(v));
        return;
    }
    let _ = CHILD_VEC_POOL.try_with(move |s| {
        if s.len() < CHILD_VEC_POOL_MAX {
            s.push(v);
        }
    });
}

#[inline]
fn acct_take() -> NonNull<AcctInner> {
    #[cfg(not(test))]
    {
        #[cfg(feature = "std")]
        if local_pool_on() {
            if let Ok(Some(p)) = tls_pools::ACCT.try_with(|s| s.take()) {
                return p;
            }
            return acct_alloc_global();
        }
        return acct_take_from(&ACCT_POOL);
    }
    #[cfg(test)]
    acct_alloc_global()
}

#[inline]
fn acct_give(p: NonNull<AcctInner>) {
    #[cfg(not(test))]
    {
        #[cfg(feature = "std")]
        if local_pool_on() {
            if tls_pools::ACCT.try_with(|s| s.give(p)).is_err() {
                // SAFETY: from acct_alloc_global; `val` already dropped,
                // nothing else owns it.
                unsafe { crate::heap::Heap.deallocate(p.cast(), Layout::new::<AcctInner>()) };
            }
            return;
        }
        acct_give_to(&ACCT_POOL, p);
    }
    #[cfg(test)]
    // SAFETY: from acct_alloc_global; `val` already dropped, nothing else owns it.
    unsafe {
        crate::heap::Heap.deallocate(p.cast(), Layout::new::<AcctInner>())
    };
}

#[inline]
fn acct_alloc_global() -> NonNull<AcctInner> {
    match crate::heap::Heap.allocate(Layout::new::<AcctInner>()) {
        Ok(p) => p.cast(),
        Err(_) => alloc::alloc::handle_alloc_error(Layout::new::<AcctInner>()),
    }
}

#[inline]
fn acct_take_from(pool: &AcctPool) -> NonNull<AcctInner> {
    if let Some(Some(p)) = pool.try_with(|s| s.pop()) {
        return p;
    }
    acct_alloc_global()
}

#[inline]
fn acct_give_to(pool: &AcctPool, p: NonNull<AcctInner>) {
    let pooled = pool.try_with(|s| {
        if s.len() >= ACCT_POOL_MAX {
            false
        } else {
            s.push(p);
            true
        }
    });
    if pooled != Some(true) {
        // SAFETY: from acct_alloc_global; `val` already dropped, nothing else owns it.
        unsafe { crate::heap::Heap.deallocate(p.cast(), Layout::new::<AcctInner>()) };
    }
}

struct AcctRc {
    ptr: NonNull<AcctInner>,
}

struct AcctWeak {
    ptr: NonNull<AcctInner>,
}

impl AcctRc {
    fn new(val: Acct) -> AcctRc {
        Self::init(acct_take(), val)
    }

    /// `new` in a fresh block, not one from the thread's pool (an unpooled root's: its drop frees it).
    fn new_fresh(val: Acct) -> AcctRc {
        Self::init(acct_alloc_global(), val)
    }

    fn init(ptr: NonNull<AcctInner>, val: Acct) -> AcctRc {
        // SAFETY: fresh or recycled-with-val-dropped; raw-write all three fields.
        unsafe {
            let inner = ptr.as_ptr();
            core::ptr::addr_of_mut!((*inner).strong).write(Cell::new(1));
            core::ptr::addr_of_mut!((*inner).weak).write(Cell::new(1));
            core::ptr::addr_of_mut!((*inner).val).write(MaybeUninit::new(val));
            #[cfg(debug_assertions)]
            core::ptr::addr_of_mut!((*inner).home).write(Cell::new(0));
        }
        AcctRc { ptr }
    }

    /// Debug builds: the context's last handle address (`AcctInner::home`).
    #[cfg(debug_assertions)]
    #[inline(always)]
    fn home(&self) -> &Cell<usize> {
        // SAFETY: the inner block is live while any strong (self) is.
        unsafe { &(*self.ptr.as_ptr()).home }
    }

    #[inline]
    fn downgrade(&self) -> AcctWeak {
        let weak = unsafe { &(*self.ptr.as_ptr()).weak };
        weak.set(weak.get() + 1);
        AcctWeak { ptr: self.ptr }
    }
}

impl Clone for AcctRc {
    #[inline]
    fn clone(&self) -> AcctRc {
        let strong = unsafe { &(*self.ptr.as_ptr()).strong };
        strong.set(strong.get() + 1);
        AcctRc { ptr: self.ptr }
    }
}

impl core::ops::Deref for AcctRc {
    type Target = Acct;
    #[inline]
    fn deref(&self) -> &Acct {
        // SAFETY: `val` is initialized while any strong (self) is live.
        unsafe { (*self.ptr.as_ptr()).val.assume_init_ref() }
    }
}

impl Drop for AcctRc {
    fn drop(&mut self) {
        let inner = self.ptr.as_ptr();
        // SAFETY: Rc discipline — drop val at last strong, reclaim at weak == 0.
        unsafe {
            let strong = &(*inner).strong;
            let s = strong.get() - 1;
            strong.set(s);
            if s != 0 {
                return;
            }
            let unpooled = {
                let val = (*inner).val.assume_init_ref();
                if debug_census::on() {
                    debug_census::dropped(val.name.get());
                }
                let mut children = val.children.borrow_mut();
                children.clear();
                if !val.unpooled {
                    child_vec_give(core::mem::take(&mut *children));
                }
                val.unpooled
            };
            core::ptr::drop_in_place(core::ptr::addr_of_mut!((*inner).val).cast::<Acct>());
            let weak = &(*inner).weak;
            let w = weak.get() - 1;
            weak.set(w);
            if w == 0 && unpooled {
                // an unpooled root's node leaves on this thread, not into its pool (a weak still held frees it to the
                // pool later: an unobserved root with no children has none)
                crate::heap::Heap.deallocate(self.ptr.cast(), Layout::new::<AcctInner>());
            } else if w == 0 {
                acct_give(self.ptr);
            }
        }
    }
}

impl AcctWeak {
    #[inline]
    fn strong_count(&self) -> usize {
        // SAFETY: a weak keeps the allocation (not the value) alive.
        unsafe { (*self.ptr.as_ptr()).strong.get() }
    }

    fn upgrade(&self) -> Option<AcctRc> {
        // SAFETY: allocation alive via the weak; strong == 0 must not resurrect.
        let strong = unsafe { &(*self.ptr.as_ptr()).strong };
        let s = strong.get();
        if s == 0 {
            None
        } else {
            strong.set(s + 1);
            Some(AcctRc { ptr: self.ptr })
        }
    }
}

impl Clone for AcctWeak {
    #[inline]
    fn clone(&self) -> AcctWeak {
        let weak = unsafe { &(*self.ptr.as_ptr()).weak };
        weak.set(weak.get() + 1);
        AcctWeak { ptr: self.ptr }
    }
}

impl Drop for AcctWeak {
    fn drop(&mut self) {
        let inner = self.ptr.as_ptr();
        // SAFETY: strong is 0 whenever weak reaches 0; val dropped by last AcctRc.
        unsafe {
            let weak = &(*inner).weak;
            let w = weak.get() - 1;
            weak.set(w);
            if w == 0 {
                debug_assert_eq!((*inner).strong.get(), 0);
                acct_give(self.ptr);
            }
        }
    }
}

/// Not Send: the counters are Cells; a handle must stay on its creating thread.
pub struct RootWeak(AcctWeak);

impl RootWeak {
    pub fn is_live(&self) -> bool {
        self.0.strong_count() > 0
    }

    pub fn tree_stats(&self) -> Option<TreeStats> {
        self.0.upgrade().map(|rc| tree_stats_node(&rc))
    }
}

// mcxt.c's TopMemoryContext linkage has no ambient equivalent here.
static ROOT_OBSERVER: core::sync::atomic::AtomicPtr<()> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_root_observer(f: fn(RootWeak)) {
    ROOT_OBSERVER.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

#[inline]
fn notify_root_observer(acct: &AcctRc) {
    let p = ROOT_OBSERVER.load(core::sync::atomic::Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: only set_root_observer stores here, always from fn(RootWeak).
        let f: fn(RootWeak) = unsafe { core::mem::transmute(p) };
        f(RootWeak(acct.downgrade()));
    }
}

pub mod global_footprint {
    use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    static BYTES: AtomicUsize = AtomicUsize::new(0);

    // (current, peak, base); a block freed on another thread credits that one.
    #[cfg(any(feature = "std", test))]
    std::thread_local! {
        static THREAD: core::cell::Cell<(isize, isize, isize)> =
            const { core::cell::Cell::new((0, 0, 0)) };
    }

    #[inline]
    fn thread_charge(delta: isize) {
        thread_note(delta);
        crate::governor::ledger_charge(delta);
    }

    #[inline]
    fn thread_note(delta: isize) {
        #[cfg(any(feature = "std", test))]
        let _ = THREAD.try_with(|t| {
            let (cur, peak, base) = t.get();
            let cur = cur.wrapping_add(delta);
            t.set((cur, peak.max(cur), base));
        });
        #[cfg(not(any(feature = "std", test)))]
        let _ = delta;
    }

    #[inline]
    pub(crate) fn add(n: usize) {
        BYTES.fetch_add(n, Relaxed);
        thread_charge(n as isize);
    }

    /// `add` with the statement's part left out: the caller charges it (a part-cache share, `governor::charge_cache`).
    #[cfg(any(feature = "std", test))]
    #[inline]
    pub(crate) fn add_unledgered(n: usize) {
        BYTES.fetch_add(n, Relaxed);
        thread_note(n as isize);
    }

    #[inline]
    pub(crate) fn sub(n: usize) {
        let prev = BYTES.fetch_sub(n, Relaxed);
        debug_assert!(prev >= n, "global footprint underflow: {prev} - {n}");
        thread_charge((n as isize).wrapping_neg());
    }

    #[inline]
    pub(crate) fn sub_process(n: usize) {
        let prev = BYTES.fetch_sub(n, Relaxed);
        debug_assert!(prev >= n, "global footprint underflow: {prev} - {n}");
    }

    pub fn thread_current() -> isize {
        #[cfg(any(feature = "std", test))]
        {
            THREAD.try_with(|t| t.get().0).unwrap_or(0)
        }
        #[cfg(not(any(feature = "std", test)))]
        0
    }

    /// Context bytes this thread freed after its peak since the last call.
    pub fn thread_freed_since_peak_rebase() -> usize {
        #[cfg(any(feature = "std", test))]
        {
            THREAD
                .try_with(|t| {
                    let (cur, peak, _base) = t.get();
                    t.set((cur, cur, cur));
                    peak.saturating_sub(cur).max(0) as usize
                })
                .unwrap_or(0)
        }
        #[cfg(not(any(feature = "std", test)))]
        0
    }

    pub fn bytes() -> usize {
        BYTES.load(Relaxed)
    }

    /// Charge/uncharge at block grain only, never per row; an uncharge must never exceed its charge.
    /// Raw (non-context) memory a backend keeps at block grain, so a
    /// statement that gives it back counts toward the release threshold.
    pub fn charge_raw(n: usize) {
        add(n);
    }

    pub fn uncharge_raw(n: usize) {
        sub(n);
    }

    pub fn charge_engine_estate(n: usize) {
        add(n);
    }

    pub fn uncharge_engine_estate(n: usize) {
        sub(n);
    }
}

pub mod debug_census {
    use core::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicUsize, Ordering::Relaxed};

    static ENABLED: AtomicBool = AtomicBool::new(false);
    const SLOTS: usize = 1024;
    // Open-addressed by name POINTER (each &'static str literal site is one
    // identity); snapshot merges equal strings from different sites.
    static NAME_PTR: [AtomicPtr<u8>; SLOTS] = [const { AtomicPtr::new(core::ptr::null_mut()) }; SLOTS];
    static NAME_LEN: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
    static LIVE: [AtomicI64; SLOTS] = [const { AtomicI64::new(0) }; SLOTS];

    pub fn enable() {
        ENABLED.store(true, Relaxed);
    }

    #[inline]
    pub fn on() -> bool {
        ENABLED.load(Relaxed)
    }

    fn slot(name: &'static str) -> Option<usize> {
        let p = name.as_ptr() as *mut u8;
        let mut i = (p as usize >> 3) % SLOTS;
        for _ in 0..SLOTS {
            let cur = NAME_PTR[i].load(Relaxed);
            if cur == p {
                return Some(i);
            }
            if cur.is_null() {
                match NAME_PTR[i].compare_exchange(core::ptr::null_mut(), p, Relaxed, Relaxed) {
                    Ok(_) => {
                        NAME_LEN[i].store(name.len(), Relaxed);
                        return Some(i);
                    }
                    Err(existing) if existing == p => return Some(i),
                    Err(_) => {}
                }
            }
            i = (i + 1) % SLOTS;
        }
        None // table full: drop the sample (debug instrument, never fail)
    }

    #[cold]
    pub(crate) fn created(name: &'static str) {
        if let Some(i) = slot(name) {
            LIVE[i].fetch_add(1, Relaxed);
        }
    }

    #[cold]
    pub(crate) fn dropped(name: &'static str) {
        if let Some(i) = slot(name) {
            LIVE[i].fetch_sub(1, Relaxed);
        }
    }

    pub fn snapshot() -> alloc::vec::Vec<(&'static str, i64)> {
        let mut rows: alloc::vec::Vec<(&'static str, i64)> = alloc::vec::Vec::new();
        for i in 0..SLOTS {
            let p = NAME_PTR[i].load(Relaxed);
            if p.is_null() {
                continue;
            }
            let n = LIVE[i].load(Relaxed);
            if n == 0 {
                continue;
            }
            // SAFETY: p/len came from one &'static str literal.
            let name: &'static str = unsafe {
                core::str::from_utf8_unchecked(core::slice::from_raw_parts(
                    p,
                    NAME_LEN[i].load(Relaxed),
                ))
            };
            match rows.iter_mut().find(|(rn, _)| *rn == name) {
                Some(r) => r.1 += n,
                None => rows.push((name, n)),
            }
        }
        rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        rows
    }
}


type SessionCleanup = alloc::boxed::Box<dyn FnOnce()>;

/// Drain order: Portals, then State, then Roots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionCleanupPhase {
    Portals,
    State,
    Roots,
}

static SESSION_CLEANUP_SINK: core::sync::atomic::AtomicPtr<()> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_session_cleanup_sink(f: fn(SessionCleanupPhase, SessionCleanup)) {
    SESSION_CLEANUP_SINK.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

/// D4812: mcx's own server-log lines (ledger2's shadow line, revoke's settled-ask line) go to the server's stderr writer once
/// it installs one (memruntime's start: elog's, which hands a memory safety thread's write off the thread, so the
/// watchdog's victim pick never waits on the log); raw stderr before that (a crate test).
static LINE_SINK: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_line_sink(f: fn(&str)) {
    LINE_SINK.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

/// D4812 (2): the block gate's scoped log defer (memruntime's: from `begin` to `end` this thread's server-log writes go
/// through the memlog ring, the one drainer, as a canceled statement's error recovery does), so a line a rung writes
/// inside an allocation never waits on the log's sink. None installed (a crate test): the lines go as
/// `server_log_line` writes them.
static GATE_LOG_BEGIN: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
static GATE_LOG_END: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_gate_log_defer(begin: fn(), end: fn()) {
    GATE_LOG_END.store(end as *mut (), core::sync::atomic::Ordering::Release);
    GATE_LOG_BEGIN.store(begin as *mut (), core::sync::atomic::Ordering::Release);
}

/// `f` with this thread's server-log writes deferred (`set_gate_log_defer`), the writer it had put back after. No
/// `Drop` (the no-drop rule): `f` never unwinds (the block gate's inline rungs neither allocate nor raise).
pub(crate) fn with_gate_log_defer<T>(f: impl FnOnce() -> T) -> T {
    let (b, e) = (GATE_LOG_BEGIN.load(core::sync::atomic::Ordering::Acquire), GATE_LOG_END.load(core::sync::atomic::Ordering::Acquire));
    if b.is_null() || e.is_null() {
        return f();
    }
    // SAFETY: only set_gate_log_defer stores here, always from fn().
    let (begin, end): (fn(), fn()) = unsafe { (core::mem::transmute::<*mut (), fn()>(b), core::mem::transmute::<*mut (), fn()>(e)) };
    begin();
    let out = f();
    end();
    out
}

/// One mcx line, its newline included, to the server log (`set_line_sink`).
#[cfg(any(feature = "std", test))]
pub(crate) fn server_log_line(b: &[u8]) {
    let p = LINE_SINK.load(core::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        let _ = std::io::Write::write_all(&mut std::io::stderr(), b);
        return;
    }
    // SAFETY: `LINE_SINK` only ever holds a `fn(&str)` stored by `set_line_sink`.
    let sink: fn(&str) = unsafe { core::mem::transmute::<*mut (), fn(&str)>(p) };
    match core::str::from_utf8(b) {
        Ok(s) => sink(s),
        Err(_) => sink(&alloc::string::String::from_utf8_lossy(b)), // alloc: Cold
    }
}

// D4682: per-thread latches a session teardown leaves set by design (a "torn down, refuse" mark) that a reused client
// backend thread must clear before its next session. A module registers its hook at first use (deduplicated); the
// launcher runs them all between two sessions on the thread, after the teardown.
const THREAD_REUSE_HOOKS_MAX: usize = 16;
static THREAD_REUSE_HOOKS: [core::sync::atomic::AtomicPtr<()>; THREAD_REUSE_HOOKS_MAX] =
    [const { core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()) }; THREAD_REUSE_HOOKS_MAX];

/// Registers `f` to run between two sessions on a reused client backend thread. Idempotent; panics past the fixed
/// capacity (a new hook needs the constant raised).
pub fn register_thread_reuse_hook(f: fn()) {
    use core::sync::atomic::Ordering::{AcqRel, Acquire};
    let p = f as *mut ();
    for slot in &THREAD_REUSE_HOOKS {
        let cur = slot.load(Acquire);
        if cur == p {
            return;
        }
        if cur.is_null() {
            match slot.compare_exchange(core::ptr::null_mut(), p, AcqRel, Acquire) {
                Ok(_) => return,
                Err(now) if now == p => return,
                Err(_) => continue,
            }
        }
    }
    panic!("mcx: more than {THREAD_REUSE_HOOKS_MAX} thread-reuse hooks");
}

/// Runs every registered thread-reuse hook on this thread (launch_backend cpool, after the session teardown); first empties
/// the thread's growth shelf (D4679: nothing the ended session parked reaches the next one).
pub fn run_thread_reuse_hooks() {
    aset::empty_growth_shelf();
    for slot in &THREAD_REUSE_HOOKS {
        let p = slot.load(core::sync::atomic::Ordering::Acquire);
        if !p.is_null() {
            // SAFETY: only register_thread_reuse_hook stores here, always from fn().
            let f: fn() = unsafe { core::mem::transmute(p) };
            f();
        }
    }
}

pub fn register_session_cleanup(f: SessionCleanup) {
    register_session_cleanup_phase(SessionCleanupPhase::State, f);
}

pub fn register_session_cleanup_phase(phase: SessionCleanupPhase, f: SessionCleanup) {
    let p = SESSION_CLEANUP_SINK.load(core::sync::atomic::Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: only set_session_cleanup_sink stores here, always from
        // fn(SessionCleanupPhase, SessionCleanup).
        let sink: fn(SessionCleanupPhase, SessionCleanup) = unsafe { core::mem::transmute(p) };
        sink(phase, f);
    }
}

// Set while a `session_root*` context is being retired at teardown, so
// `reset_noncore` skips its mid-life exact-accounting leak-check for the
// final wholesale arena release.
#[cfg(any(feature = "std", test))]
std::thread_local! {
    static SESSION_ROOT_RETIRING: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}
#[cfg(not(any(feature = "std", test)))]
static SESSION_ROOT_RETIRING: RetiringFlag = RetiringFlag(core::sync::atomic::AtomicBool::new(false));
#[cfg(not(any(feature = "std", test)))]
struct RetiringFlag(core::sync::atomic::AtomicBool);
#[cfg(not(any(feature = "std", test)))]
impl RetiringFlag {
    fn with<R>(&self, f: impl FnOnce(&core::cell::Cell<bool>) -> R) -> R {
        use core::sync::atomic::Ordering::Relaxed;
        let cell = core::cell::Cell::new(self.0.load(Relaxed));
        let r = f(&cell);
        self.0.store(cell.get(), Relaxed);
        r
    }
}

/// The shell struct itself is deliberately never freed: at teardown it is
/// reset and poisoned, but left allocated, so the handed-out `&'static` can never become dangling.
pub fn session_root(name: &'static str) -> &'static MemoryContext {
    session_root_from(MemoryContext::new(name))
}

pub fn session_root_from(ctx: MemoryContext) -> &'static MemoryContext {
    session_root_mut(ctx)
}

/// The cleanup closure holds only the address, never a reference, so the exclusive borrow stays unique until teardown.
pub fn session_root_mut(ctx: MemoryContext) -> &'static mut MemoryContext {
    ctx.set_owner_session();
    let raw: *mut MemoryContext = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(ctx));
    let addr = raw as usize;
    register_session_cleanup_phase(
        SessionCleanupPhase::Roots,
        alloc::boxed::Box::new(move || {
            // SAFETY: `addr` is the leaked shell from `Box::into_raw` above.
            // The closure runs at most once, on the owning thread, at
            // teardown. We do NOT reconstruct+drop the Box: freeing the shell
            // would leave the handed-out `&'static` dangling. Instead we
            // retire it in place — release the arena bulk and poison the shell
            // — so the reference stays valid (poisoned) forever and any later
            // access fails closed via `check_live`.
            let shell = unsafe { &mut *(addr as *mut MemoryContext) };
            shell.retire_session_root();
            bury_retired_root(addr);
        }),
    );
    // SAFETY: heap allocation; retired-but-never-freed by the closure above,
    // so the reference is valid for the process lifetime.
    unsafe { &mut *raw }
}

/// Without a per-thread pool the shell stays leaked-and-poisoned as before — never freed early.
fn bury_retired_root(addr: usize) {
    #[cfg(all(feature = "std", not(test)))]
    if local_pool_on() {
        let _ = tls_pools::RETIRED_ROOTS.try_with(|s| s.give(addr));
        return;
    }
    let _ = addr;
}

static IDLE_RELEASE: core::sync::atomic::AtomicPtr<()> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_idle_release(f: fn()) {
    IDLE_RELEASE.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

#[cold]
#[inline(never)]
pub fn run_idle_release() -> bool {
    let p = IDLE_RELEASE.load(core::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        return false;
    }
    // SAFETY: only set_idle_release stores here, always from fn().
    let f: fn() = unsafe { core::mem::transmute(p) };
    f();
    true
}

// ALLOC-LADDER (b), the ladder's first rung: every thread that owns allocator pages collects its own at its next idle
// point (mimalloc v3 frees a page only on its owning thread, unless the page is abandoned), and one purge on the
// ladder's thread returns what they handed back. The ladder never waits for the answers: it reads the rung's yield at
// its next pass. Today the answering threads are the runtime pool's workers (at their idle park); a session collects
// its own heap at its idle release already.

/// The rung's ask: a generation each answering thread compares with the last it answered.
static COLLECT_ASK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Answers so far (information).
static COLLECT_ANSWERS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The wake that reaches parked answering threads (the runtime pool's park lot), installed by the pool.
static COLLECT_WAKE: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
/// The thread's own collect without the arena purge (mimalloc `mi_collect(false)`), installed by the binary.
static OWN_COLLECT: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_collect_wake(f: fn()) {
    COLLECT_WAKE.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

pub fn set_own_collect(f: fn()) {
    OWN_COLLECT.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

/// Ask every answering thread to collect its own allocator pages at its next idle point, and wake the parked ones.
/// Never waits. The new generation.
pub fn ask_collect() -> u64 {
    let g = COLLECT_ASK.fetch_add(1, core::sync::atomic::Ordering::AcqRel) + 1;
    let p = COLLECT_WAKE.load(core::sync::atomic::Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: only set_collect_wake stores here, always from fn().
        let f: fn() = unsafe { core::mem::transmute(p) };
        f();
    }
    g
}

/// An answering thread at its idle point: when an ask came since `seen` (the generation it last answered), collect its
/// own pages and return the generation now answered; else `seen`. One relaxed load when nothing was asked.
#[inline]
pub fn answer_collect(seen: u64) -> u64 {
    let g = COLLECT_ASK.load(core::sync::atomic::Ordering::Acquire);
    if g == seen {
        return seen;
    }
    answer_collect_slow();
    g
}

#[cold]
#[inline(never)]
fn answer_collect_slow() {
    let p = OWN_COLLECT.load(core::sync::atomic::Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: only set_own_collect stores here, always from fn().
        let f: fn() = unsafe { core::mem::transmute(p) };
        f();
    }
    COLLECT_ANSWERS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// The cache give-back's allocator step on the calling thread (`allocarena::give_back`), installed by the binary.
static CACHE_GIVE_BACK: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
/// A give-back's purge after the owners answered: 1 = asked, 2 = one watchdog sample since (`purge_after_ask`).
static PURGE_DUE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

pub fn set_cache_give_back(f: fn()) {
    CACHE_GIVE_BACK.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

/// REUSE (PGRUST_REUSE_BEFORE_RELEASE=1): a reclaim's freed images kept for the next allocation instead of returned: this
/// thread's cache-heap pages back to the cache arena with no purge (installed by the binary under the switch:
/// `allocarena::collect_cache_local`) and every owner asked to collect at its idle park. False (nothing done) when the
/// switch is off: the caller gives back as before.
static REUSE_KEEP: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_reuse_keep(f: fn()) {
    REUSE_KEEP.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

pub fn keep_for_reuse() -> bool {
    let p = REUSE_KEEP.load(core::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        return false;
    }
    // SAFETY: only set_reuse_keep stores here, always from fn().
    let f: fn() = unsafe { core::mem::transmute(p) };
    f();
    let _ = ask_collect();
    true
}

/// LADDER-DESIGN 6a: a cache gave memory back (an idle release, a reclaim). Its freed images' pages return: the own runs
/// and this thread's cache-heap pages now; every pool worker's cache-heap and heap pages at its next idle park (the
/// ladder's collect ask); the arenas purged now and again one watchdog sample later, after the answers. Never waits.
pub fn give_back_caches() {
    let p = CACHE_GIVE_BACK.load(core::sync::atomic::Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: only set_cache_give_back stores here, always from fn().
        let f: fn() = unsafe { core::mem::transmute(p) };
        f();
    }
    let _ = ask_collect();
    release_retained();
    PURGE_DUE.store(1, core::sync::atomic::Ordering::Relaxed);
    // C3 / C4: the purger owns the cache arena's (and under query_purge_bg the general arenas') purge, which the release
    // above skips: wake it now (the ladder's R1 reaches here), it purges their expired ranges and the cache arena at once
    wake_purger();
}

/// The memory watchdog's sample: a give-back's second purge, one sample after its ask (the workers answered at their
/// idle park by then). Whether it purged.
pub fn purge_after_ask() -> bool {
    use core::sync::atomic::Ordering::Relaxed;
    match PURGE_DUE.load(Relaxed) {
        0 => false,
        1 => {
            let _ = PURGE_DUE.compare_exchange(1, 2, Relaxed, Relaxed);
            false
        }
        _ => {
            PURGE_DUE.store(0, Relaxed);
            release_retained()
        }
    }
}

/// (asks, answers) so far.
pub fn collect_counts() -> (u64, u64) {
    (COLLECT_ASK.load(core::sync::atomic::Ordering::Relaxed), COLLECT_ANSWERS.load(core::sync::atomic::Ordering::Relaxed))
}

/// C3 (`cache_purge_now`): what a cut of the process caches did: the bytes its trims freed, and the bytes past the caps
/// they could not free (held: in use, pinned, or shared), so the gap shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheCut {
    pub freed: u64,
    pub held: u64,
}

/// C3: the process caches' cut, applied off every query thread (installed by Gravity's hosts: each cache's cap re-read and
/// its trim run when it is past it); the cache arena's purge (installed by the binary: `allocarena::purge_cache_now`); and
/// the purger's wake (installed by memruntime's purger when it starts).
static CACHE_CUT: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
static CACHE_PURGE: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
static PURGE_WAKE: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_cache_cut(f: fn() -> CacheCut) {
    CACHE_CUT.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

pub fn set_cache_purge(f: fn(bool) -> bool) {
    CACHE_PURGE.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

pub fn set_purge_wake(f: fn()) {
    PURGE_WAKE.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

/// C3 (purge owned): the cache arena's purge made the purger's alone, and the dead-man fallback's purge count
/// (both installed by the binary: `allocarena::set_cache_purge_owned` / `purge_owned_fallbacks`).
static CACHE_PURGE_OWNER: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
static CACHE_PURGE_FALLBACKS: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_cache_purge_owner(f: fn(bool)) {
    CACHE_PURGE_OWNER.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

/// REUSE BEFORE RELEASE: the allocator's reuse counters `(live bytes, claims, bytes claimed, misses)` (installed by the
/// binary: `allocarena::reuse_stats`).
static REUSE_STATS: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_reuse_stats(f: fn() -> (u64, u64, u64, u64)) {
    REUSE_STATS.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

/// REUSE BEFORE RELEASE: `(live bytes, claims, bytes claimed, misses)`, zeros when nothing is installed.
pub fn reuse_stats() -> (u64, u64, u64, u64) {
    let p = REUSE_STATS.load(core::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        return (0, 0, 0, 0);
    }
    // SAFETY: only set_reuse_stats stores here, always from fn() -> (u64, u64, u64, u64).
    let f: fn() -> (u64, u64, u64, u64) = unsafe { core::mem::transmute(p) };
    f()
}

/// REUSE (the C4 gate): where new allocations' pages came from (installed by the binary: `allocarena::alloc_sources`).
static ALLOC_SOURCES: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_alloc_sources(f: fn() -> (u64, u64, u64, u64, u64)) {
    ALLOC_SOURCES.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

/// `(own committed-free, cache arena committed-free for general heaps, fresh general, fresh cache, cache arena
/// committed-free for the cache heap)` bytes, zeros when nothing is installed.
pub fn alloc_sources() -> (u64, u64, u64, u64, u64) {
    let p = ALLOC_SOURCES.load(core::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        return (0, 0, 0, 0, 0);
    }
    // SAFETY: only set_alloc_sources stores here, always from fn() -> (u64, u64, u64, u64, u64).
    let f: fn() -> (u64, u64, u64, u64, u64) = unsafe { core::mem::transmute(p) };
    f()
}

/// REUSE BEFORE RELEASE sizing: the cache arena's free-run histogram `(committed, any)` in bytes per log2 bucket of slices
/// (installed by the binary under `PGRUST_REUSE_RUNS=1`: `allocarena::cache_free_runs`).
static CACHE_RUNS: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub type FreeRuns = ([u64; 12], [u64; 12]);

pub fn set_cache_runs(f: fn() -> FreeRuns) {
    CACHE_RUNS.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

/// The histogram, or `None` when nothing is installed (the diagnostic is off).
pub fn cache_runs() -> Option<FreeRuns> {
    let p = CACHE_RUNS.load(core::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        return None;
    }
    // SAFETY: only set_cache_runs stores here, always from fn() -> FreeRuns.
    let f: fn() -> FreeRuns = unsafe { core::mem::transmute(p) };
    Some(f())
}

pub fn set_cache_purge_fallbacks(f: fn() -> u64) {
    CACHE_PURGE_FALLBACKS.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

/// C3: only the purger purges the cache arena from now (`on`; memruntime's purger once it runs), or everyone again.
pub fn cache_purge_owner(on: bool) {
    let p = CACHE_PURGE_OWNER.load(core::sync::atomic::Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: only set_cache_purge_owner stores here, always from fn(bool).
        let f: fn(bool) = unsafe { core::mem::transmute(p) };
        f(on);
    }
}

/// C4 (query_purge_bg): the general arenas' purge made the purger's alone, and their expired ranges purged now (both
/// installed by the binary: `allocarena::set_general_purge_owned` / `purge_general_expired`).
static GENERAL_PURGE_OWNER: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
static GENERAL_PURGE: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_general_purge_owner(f: fn(bool)) {
    GENERAL_PURGE_OWNER.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

pub fn set_general_purge(f: fn() -> u32) {
    GENERAL_PURGE.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

/// C4: only the purger purges the general arenas from now (`on`), or everyone again.
pub fn general_purge_owner(on: bool) {
    let p = GENERAL_PURGE_OWNER.load(core::sync::atomic::Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: only set_general_purge_owner stores here, always from fn(bool).
        let f: fn(bool) = unsafe { core::mem::transmute(p) };
        f(on);
    }
}

/// C4: the general arenas' expired ranges purged now; how many arenas purged something (0 when nothing is installed).
pub fn general_purge() -> u32 {
    let p = GENERAL_PURGE.load(core::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: only set_general_purge stores here, always from fn() -> u32.
    let f: fn() -> u32 = unsafe { core::mem::transmute(p) };
    f()
}

/// Purge after idle (the purger's hold) and purged bytes by thread (both installed by the binary:
/// `allocarena::purge_after_idle` / `purged_bytes`).
static IDLE_PURGE: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
static PURGED_BYTES: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_idle_purge(f: fn(bool, bool, u64) -> u32) {
    IDLE_PURGE.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

pub fn set_purged_bytes(f: fn() -> (u64, u64)) {
    PURGED_BYTES.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

/// The owned arenas' expired ranges purged only where no allocation took a slice for `idle_ms` (`general`: the general
/// arenas, `cache`: the cache arena); marks their owner alive. Arenas that purged; 0 when nothing is installed.
pub fn idle_purge(general: bool, cache: bool, idle_ms: u64) -> u32 {
    let p = IDLE_PURGE.load(core::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: only set_idle_purge stores here, always from fn(bool, bool, u64) -> u32.
    let f: fn(bool, bool, u64) -> u32 = unsafe { core::mem::transmute(p) };
    f(general, cache, idle_ms)
}

/// The purger's hold counters (installed by memruntime's purger when it starts: `purge::hold_counts`): `(ticks under
/// ladder pressure, bytes purged on them, idle purges, bytes they purged)`.
static PURGE_POLICY: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_purge_policy(f: fn() -> (u64, u64, u64, u64)) {
    PURGE_POLICY.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

/// The purger's hold counters; zeros when the purger does not run.
pub fn purge_policy() -> (u64, u64, u64, u64) {
    let p = PURGE_POLICY.load(core::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        return (0, 0, 0, 0);
    }
    // SAFETY: only set_purge_policy stores here, always from fn() -> (u64, u64, u64, u64).
    let f: fn() -> (u64, u64, u64, u64) = unsafe { core::mem::transmute(p) };
    f()
}

/// Bytes purged so far `(by the purger, by every other thread)`; zeros when nothing is installed.
pub fn purged_bytes() -> (u64, u64) {
    let p = PURGED_BYTES.load(core::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        return (0, 0);
    }
    // SAFETY: only set_purged_bytes stores here, always from fn() -> (u64, u64).
    let f: fn() -> (u64, u64) = unsafe { core::mem::transmute(p) };
    f()
}

/// C3: the purges the dead-man fallback ran on the cache arena (expected 0); 0 when nothing is installed.
pub fn cache_purge_fallbacks() -> u64 {
    let p = CACHE_PURGE_FALLBACKS.load(core::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: only set_cache_purge_fallbacks stores here, always from fn() -> u64.
    let f: fn() -> u64 = unsafe { core::mem::transmute(p) };
    f()
}

/// C3: the caches' cut now (the purger's tick and wake). Zero when nothing is installed.
pub fn cache_cut() -> CacheCut {
    let p = CACHE_CUT.load(core::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        return CacheCut::default();
    }
    // SAFETY: only set_cache_cut stores here, always from fn() -> CacheCut.
    let f: fn() -> CacheCut = unsafe { core::mem::transmute(p) };
    f()
}

/// C3: the cache arena's freed ranges purged now (`force`: also those inside the purge delay). Whether any was; false when
/// nothing is installed or another thread was purging.
pub fn cache_purge(force: bool) -> bool {
    let p = CACHE_PURGE.load(core::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        return false;
    }
    // SAFETY: only set_cache_purge stores here, always from fn(bool) -> bool.
    let f: fn(bool) -> bool = unsafe { core::mem::transmute(p) };
    f(force)
}

/// REACH (D4550): the process's one "memory short" epoch. A pressure source moves it (`memory_short_now`: the
/// reclaimer's tick below its low mark; the ladder's background trigger below LOW adds its own); each backend's
/// per-session caches read it with one relaxed load at their own check (a statement's start) and shed what they keep
/// when it moved since they last looked. It moves no bytes itself.
static MEMORY_SHORT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// REACH: the memory-short epoch now (one relaxed load).
#[inline]
pub fn memory_short_epoch() -> u64 {
    MEMORY_SHORT.load(core::sync::atomic::Ordering::Relaxed)
}

/// REACH: a pressure source found memory short: every backend's per-session caches shed at their next check.
#[inline]
pub fn memory_short_now() {
    MEMORY_SHORT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// C3: wake the purger (a cache's cap or the broker's target fell, a give-back): it applies the cut and purges at once.
/// Never waits; a no-op until the purger runs.
pub fn wake_purger() {
    let p = PURGE_WAKE.load(core::sync::atomic::Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: only set_purge_wake stores here, always from fn().
        let f: fn() = unsafe { core::mem::transmute(p) };
        f();
    }
}

#[cfg(test)]
mod collect_ask_tests {
    use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

    static COLLECTED: AtomicU64 = AtomicU64::new(0);
    static WOKEN: AtomicU64 = AtomicU64::new(0);
    /// The ask generation is process-wide: these tests take turns.
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// ALLOC-LADDER (b): an ask wakes the parked threads at once and never waits; each thread collects once per ask, at
    /// its own idle point, and an idle point with no new ask costs nothing.
    #[test]
    fn each_thread_answers_each_ask_once_at_its_own_idle_point() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        super::set_own_collect(|| {
            COLLECTED.fetch_add(1, Relaxed);
        });
        super::set_collect_wake(|| {
            WOKEN.fetch_add(1, Relaxed);
        });
        let (g0, _) = super::collect_counts();
        let (mut a, mut b) = (g0, g0);
        a = super::answer_collect(a);
        assert_eq!(COLLECTED.load(Relaxed), 0, "nothing asked: no collect");
        let g1 = super::ask_collect();
        assert_eq!((g1, WOKEN.load(Relaxed)), (g0 + 1, 1), "the ask wakes the parked threads");
        a = super::answer_collect(a);
        a = super::answer_collect(a);
        assert_eq!((a, COLLECTED.load(Relaxed)), (g1, 1), "thread a answers once");
        let _ = super::ask_collect();
        let g3 = super::ask_collect();
        b = super::answer_collect(b);
        assert_eq!((b, COLLECTED.load(Relaxed)), (g3, 2), "thread b, two asks behind, collects once");
        assert_eq!(super::collect_counts().1, 2);
    }

    /// LADDER-DESIGN 6a: a give-back runs its allocator step and asks the owners at once, purges now, and purges again
    /// one watchdog sample later (after the answers); a later sample does nothing more.
    #[test]
    fn a_give_back_purges_now_and_once_more_a_sample_after_its_ask() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        static STEP: AtomicU64 = AtomicU64::new(0);
        static PURGES: AtomicU64 = AtomicU64::new(0);
        super::set_cache_give_back(|| {
            STEP.fetch_add(1, Relaxed);
        });
        super::set_allocator_release(|| {
            PURGES.fetch_add(1, Relaxed);
        });
        let (g0, _) = super::collect_counts();
        super::give_back_caches();
        assert_eq!((STEP.load(Relaxed), PURGES.load(Relaxed), super::collect_counts().0), (1, 1, g0 + 1));
        assert!(!super::purge_after_ask(), "the sample of the ask: the owners have not answered yet");
        assert!(super::purge_after_ask(), "one sample later: the second purge");
        assert_eq!(PURGES.load(Relaxed), 2);
        assert!(!super::purge_after_ask(), "nothing more");
    }

    /// pagepool C.9: the session-end purge runs the allocator release by default; `PGRUST_EXIT_PURGE=0` skips it, and
    /// every other purge (the give-back's) still runs.
    #[test]
    fn the_exit_purge_switch_skips_only_the_session_end_purge() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        static PURGES: AtomicU64 = AtomicU64::new(0);
        super::set_allocator_release(|| {
            PURGES.fetch_add(1, Relaxed);
        });
        assert!(super::release_retained_at_exit(), "default: today's purge");
        assert_eq!(PURGES.load(Relaxed), 1);
        super::set_exit_purge(false);
        assert!(!super::release_retained_at_exit(), "off: no purge at session end");
        assert_eq!(PURGES.load(Relaxed), 1);
        assert!(super::release_retained(), "off: the other purges still run");
        assert_eq!(PURGES.load(Relaxed), 2);
        super::set_exit_purge(true);
        assert!(super::release_retained_at_exit());
        assert_eq!(PURGES.load(Relaxed), 3);
    }
}

static ALLOC_RELEASE: core::sync::atomic::AtomicPtr<()> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_allocator_release(f: fn()) {
    ALLOC_RELEASE.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

#[cold]
#[inline(never)]
pub fn release_retained() -> bool {
    let p = ALLOC_RELEASE.load(core::sync::atomic::Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: only set_allocator_release stores here, always from fn().
        let f: fn() = unsafe { core::mem::transmute(p) };
        f();
        // PRESSURE ONLY: the release's own purge does nothing (mimalloc's purge delay -1); under ladder pressure the
        // freed pages go back here, on this thread
        if PRESSURE_ONLY.load(core::sync::atomic::Ordering::Relaxed) && ladder::pressed_now() {
            // PGRUST_PURGE_PRESSURE_BG: the purger purges (and trims the parked regions) and this thread goes on; inline
            // only past the watchdog's cancel line
            if purge_bg() && !ladder::past_cancel_line() {
                wake_purger();
                BG_COUNTS[0].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            } else {
                if purge_bg() {
                    BG_COUNTS[1].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                }
                let _ = pressure_purge(false);
            }
        }
        true
    } else {
        false
    }
}

/// pagepool P0 (C.9): set once at boot from `PGRUST_EXIT_PURGE` (0 = off); default on = today's purge.
static EXIT_PURGE_OFF: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn set_exit_purge(on: bool) {
    EXIT_PURGE_OFF.store(!on, core::sync::atomic::Ordering::Relaxed);
}

/// The session-end purge (a backend's exit, park or retire, and the init-file load): `release_retained` unless
/// `PGRUST_EXIT_PURGE=0` turned it off. Whether it purged.
pub fn release_retained_at_exit() -> bool {
    if EXIT_PURGE_OFF.load(core::sync::atomic::Ordering::Relaxed) {
        false
    } else {
        release_retained()
    }
}

/// D4682 (Broker flip condition 4): the calling thread's retained heap: bytes its allocator keeps committed in the pages
/// this thread owns beyond their live blocks. Set once at boot by the server binary (mimalloc's own-theap area visit).
static THREAD_RETAINED: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_thread_retained(f: fn() -> usize) {
    THREAD_RETAINED.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

/// The calling thread's retained heap in bytes; None without a hook (wasm, unit rigs). Walks the thread's pages: call
/// at a session's end, never per statement.
#[cold]
#[inline(never)]
pub fn thread_retained() -> Option<usize> {
    let p = THREAD_RETAINED.load(core::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        return None;
    }
    // SAFETY: only set_thread_retained stores here, always from fn() -> usize.
    let f: fn() -> usize = unsafe { core::mem::transmute(p) };
    Some(f())
}

/// PRESSURE ONLY (`PGRUST_PURGE_PRESSURE_ONLY=1`, set by the binary with mimalloc's purge delay -1): no timer and no
/// collect purges; every allocator release (`release_retained`: a statement's end past the release threshold, the idle
/// release, a cache give-back from the reclaimer or the ladder) checks ladder pressure and purges then.
static PRESSURE_ONLY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn set_pressure_only(on: bool) {
    PRESSURE_ONLY.store(on, core::sync::atomic::Ordering::Relaxed);
}

/// Whether PRESSURE ONLY is on (the memory settings view).
pub fn pressure_only() -> bool {
    PRESSURE_ONLY.load(core::sync::atomic::Ordering::Relaxed)
}

/// Ladder pressure's purge (installed by the binary: `allocarena::purge_unused`): every free and committed run of
/// every arena decommitted now. Its calls and bytes, `(calls, bytes)` (the accounts rows ladder_pressure_*).
static PRESSURE_PURGE: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
static PRESSURE_CALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static PRESSURE_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn set_pressure_purge(f: fn(bool) -> u64) {
    PRESSURE_PURGE.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

/// Ladder pressure's purge now on this thread (`by_purger`: the purger thread's call). The bytes; 0 with nothing installed.
pub fn pressure_purge(by_purger: bool) -> u64 {
    let p = PRESSURE_PURGE.load(core::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: only set_pressure_purge stores here, always from fn(bool) -> u64.
    let f: fn(bool) -> u64 = unsafe { core::mem::transmute(p) };
    let n = f(by_purger);
    PRESSURE_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    PRESSURE_BYTES.fetch_add(n, core::sync::atomic::Ordering::Relaxed);
    n
}

/// `(ladder pressure purges, bytes they decommitted)` since the process started.
pub fn pressure_counts() -> (u64, u64) {
    (PRESSURE_CALLS.load(core::sync::atomic::Ordering::Relaxed), PRESSURE_BYTES.load(core::sync::atomic::Ordering::Relaxed))
}

/// IDLE TRIM UNDER PRESSURE ONLY (`PGRUST_IDLE_TRIM_PRESSURE_ONLY=1`, set by the binary; default off): the idle release
/// keeps the parked regions and slot sets for the next statement (a large region dropped there is unmapped, and the next
/// statement maps and faults it in again) unless ladder pressure holds (`ladder::pressed_now`: the whole reading over the
/// ladder's line). Its answers counted, `(trims run, trims held)`.
static IDLE_TRIM_PRESSURE_ONLY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static IDLE_TRIMS: [core::sync::atomic::AtomicU64; 2] = [const { core::sync::atomic::AtomicU64::new(0) }; 2];

pub fn set_idle_trim_pressure_only(on: bool) {
    IDLE_TRIM_PRESSURE_ONLY.store(on, core::sync::atomic::Ordering::Relaxed);
}

/// Whether IDLE TRIM UNDER PRESSURE ONLY is on (the memory settings view).
pub fn idle_trim_pressure_only() -> bool {
    IDLE_TRIM_PRESSURE_ONLY.load(core::sync::atomic::Ordering::Relaxed)
}

/// Whether the idle release trims the parked regions and slot sets now: always with the switch off, only under ladder
/// pressure with it on; never under PGRUST_PURGE_PRESSURE_BG (the purger trims them when pressure arrives) nor under the
/// parked_trim_rung knob (P3: the ladder's R2p trims them under pressure, `ladder::trim_parked`). Counted.
pub fn idle_trim_now() -> bool {
    let trim = !purge_bg()
        && !ladder::parked_rung_on()
        && (!IDLE_TRIM_PRESSURE_ONLY.load(core::sync::atomic::Ordering::Relaxed) || ladder::pressed_now());
    IDLE_TRIMS[usize::from(!trim)].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    trim
}

/// `(idle trims run, idle trims held for want of ladder pressure)` since the process started.
pub fn idle_trim_counts() -> (u64, u64) {
    (IDLE_TRIMS[0].load(core::sync::atomic::Ordering::Relaxed), IDLE_TRIMS[1].load(core::sync::atomic::Ordering::Relaxed))
}

/// THE PURGER PURGES UNDER PRESSURE (`PGRUST_PURGE_PRESSURE_BG=1`, set by the binary; default off; meant with pressure
/// only and C3's purger): an allocator release that meets ladder pressure wakes the purger and
/// goes on (inline only past the watchdog's cancel line, `ladder::past_cancel_line`); the purger, on each pressed tick,
/// trims the parked regions and slot sets (`pressure_trim`) and then purges; the idle release never trims. Counted:
/// `(wakes, inline past the cancel line, the purger's trims)`.
static PURGE_BG: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static BG_COUNTS: [core::sync::atomic::AtomicU64; 3] = [const { core::sync::atomic::AtomicU64::new(0) }; 3];
static PRESSURE_TRIM: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_purge_bg(on: bool) {
    PURGE_BG.store(on, core::sync::atomic::Ordering::Relaxed);
}

#[inline]
pub fn purge_bg() -> bool {
    PURGE_BG.load(core::sync::atomic::Ordering::Relaxed)
}

/// The trim of the parked regions and slot sets the purger runs under pressure (installed by the layer that owns them).
pub fn set_pressure_trim(f: fn()) {
    PRESSURE_TRIM.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

/// The purger's trim of the parked regions and slot sets under pressure; a no-op with nothing installed. Counted.
pub fn pressure_trim() {
    let p = PRESSURE_TRIM.load(core::sync::atomic::Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: only set_pressure_trim stores here, always from fn().
        let f: fn() = unsafe { core::mem::transmute(p) };
        f();
        BG_COUNTS[2].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
}

/// `(purger wakes from an allocator release under pressure, inline purges past the cancel line, the purger's trims)`.
pub fn purge_bg_counts() -> (u64, u64, u64) {
    let c = |i: usize| BG_COUNTS[i].load(core::sync::atomic::Ordering::Relaxed);
    (c(0), c(1), c(2))
}

/// Purge timing (installed by the binary: `allocarena::set_purge_phase` / `allocarena::purge_times`): the session loop
/// marks its thread inside a statement (1) or after its ReadyForQuery was sent (2), so each purge event's time lands on
/// the client's latency or off it; an executor worker marks itself (3).
static PURGE_PHASE: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
static PURGE_TIMES: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_purge_timing(phase: fn(u8), times: fn() -> [[u64; 4]; 5]) {
    PURGE_PHASE.store(phase as *mut (), core::sync::atomic::Ordering::Release);
    PURGE_TIMES.store(times as *mut (), core::sync::atomic::Ordering::Release);
}

/// This thread's phase for the purge timing (1 = inside a statement, 2 = after its ReadyForQuery, 3 = an executor
/// worker). A no-op with nothing installed.
#[inline]
pub fn purge_phase(phase: u8) {
    let p = PURGE_PHASE.load(core::sync::atomic::Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: only set_purge_timing stores here, always from fn(u8).
        let f: fn(u8) = unsafe { core::mem::transmute(p) };
        f(phase);
    }
}

/// The purge events by thread `[the purger, a session inside a statement, a session after its ReadyForQuery, any other
/// thread, an executor worker]`, each `[events, summed ns, longest ns, bytes]`; zeros with nothing installed.
pub fn purge_times() -> [[u64; 4]; 5] {
    let p = PURGE_TIMES.load(core::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        return [[0; 4]; 5];
    }
    // SAFETY: only set_purge_timing stores here, always from fn() -> [[u64; 4]; 5].
    let f: fn() -> [[u64; 4]; 5] = unsafe { core::mem::transmute(p) };
    f()
}

/// Cold by contract: call only from an idle backend.
#[cold]
#[inline(never)]
pub fn passivate_trim() -> bool {
    aset::trim_recycled_blocks();
    #[cfg(all(feature = "std", not(test)))]
    if local_pool_on() {
        let _ = tls_pools::ACCT.try_with(|s| s.drain_all());
        let _ = tls_pools::CHILD_VECS.try_with(|s| s.drain_all());
    }
    release_retained()
}

pub const WINDOW_KEEPER_MIN: usize = 8 * 1024;
pub const WINDOW_KEEPER_MAX: usize = 1024 * 1024;

pub struct MemoryContext {
    acct: AcctRc,
    backend: Backend,
    reset_cbs: RefCell<alloc::vec::Vec<alloc::boxed::Box<dyn FnOnce()>>>,
    // reset() early-exits on is_reset; per-row ResetExprContext is 2 loads, not the arena walk.
    is_reset: Cell<bool>,
    // Turns any post-teardown access into a deterministic panic instead of reading a retired context.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    poisoned: Cell<bool>,
    // Debug builds: the address this context handed out its last handle at (`mcx`), copied with the context when it
    // moves, so an Mcx left at the old address is named at its next allocation (`check_home`), not read as a context.
    #[cfg(debug_assertions)]
    home: Cell<usize>,
    // Test builds: this context's allocate_slow calls (the bump_fast tests); zero-size and a no-op in every other build.
    slow_calls: SlowCalls,
}

// MemoryContext::slow_calls: a counter only in test builds, so the release allocate_slow pays nothing.
#[cfg(test)]
#[derive(Default)]
struct SlowCalls(Cell<u64>);
#[cfg(not(test))]
#[derive(Default)]
struct SlowCalls;

impl SlowCalls {
    #[inline(always)]
    fn hit(&self) {
        #[cfg(test)]
        self.0.set(self.0.get() + 1);
    }
}

#[cfg(test)]
impl SlowCalls {
    // allocate_slow calls since the last take.
    fn take(&self) -> u64 {
        self.0.replace(0)
    }
}

impl MemoryContext {
    pub fn new(name: &'static str) -> Self {
        Self::with_backend(name, Backend::Aset(core::cell::UnsafeCell::new(aset::AllocSet::new())), None)
    }

    pub fn new_start_small(name: &'static str) -> Self {
        Self::with_backend(
            name,
            Backend::Aset(core::cell::UnsafeCell::new(aset::AllocSet::new_small(aset::MAX_BLOCK_SIZE))),
            None,
        )
    }

    /// A root context whose first block holds `init` bytes: a write-once
    /// copy of known size fits one block.
    pub fn new_sized(name: &'static str, init: usize) -> Self {
        let init = init.max(ALLOCSET_SMALL_INITSIZE);
        Self::with_backend(
            name,
            Backend::Aset(core::cell::UnsafeCell::new(aset::AllocSet::new_sized(
                init,
                init.max(aset::MAX_BLOCK_SIZE),
            ))),
            None,
        )
    }

    pub fn new_bump(name: &'static str) -> Self {
        Self::with_backend(name, Backend::Bump(new_arena()), None)
    }

    pub fn new_unobserved(name: &'static str) -> Self {
        Self::with_backend_as(name, Backend::Aset(core::cell::UnsafeCell::new(aset::AllocSet::new())), None, false)
    }

    /// `new_unobserved` that takes nothing from the creating thread's pools (no recycled keeper: its first block is
    /// allocated by its first alloc; a fresh account node) and gives nothing to the dropping thread's (its drop frees its
    /// blocks and its node): a root built in one account's home and dropped in it on another thread (`SendRoot`'s
    /// `new_owned_unpooled`), whose pooled pieces would otherwise park as that account's in a thread that then uses them as
    /// its own.
    pub fn new_unobserved_unpooled(name: &'static str) -> Self {
        let a = aset::AllocSet::new_sized(aset::INIT_BLOCK_SIZE, aset::MAX_BLOCK_SIZE);
        Self::with_backend_full(name, Backend::Aset(core::cell::UnsafeCell::new(a)), None, false, true)
    }

    /// `new_unobserved` over `new_start_small`'s 1 KiB keeper: a root that holds a few small lists.
    pub fn new_unobserved_small(name: &'static str) -> Self {
        Self::with_backend_as(name, Backend::Aset(core::cell::UnsafeCell::new(aset::AllocSet::new_small(aset::MAX_BLOCK_SIZE))), None, false)
    }

    /// `new` whose reset is wholesale like C's AllocSetReset (live chunks dropped, not reported as leaks): a per-row or
    /// per-group context PostgreSQL resets with its values still in it (ReScanExprContext), observed like `new`.
    pub fn new_wholesale(name: &'static str) -> Self {
        let ctx = Self::new(name);
        ctx.acct.wholesale_reset.set(true);
        ctx
    }

    pub fn new_wholesale_sized(name: &'static str, init: usize) -> Self {
        let ctx = Self::new_sized(name, init);
        ctx.acct.wholesale_reset.set(true);
        ctx
    }

    /// `new_unobserved` whose reset is wholesale like C's AllocSetReset (live chunks dropped, not reported as leaks): a
    /// per-call context whose last result stays live until its next reset (Gravity's parallel hosted aggregate calls).
    pub fn new_unobserved_wholesale(name: &'static str) -> Self {
        let ctx = Self::new_unobserved(name);
        ctx.acct.wholesale_reset.set(true);
        ctx
    }

    /// LEDGER2 step 2: this context's owner in the memory ledger is `o` from now (a root named by construction: a session
    /// root's session, a portal's PortalContext): the bytes it holds move with it in one step, and the contexts made under
    /// it from now take it. Contexts made under it before keep theirs.
    #[cfg(feature = "std")]
    pub fn set_owner(&self, o: memledger::Owner) {
        self.acct.led.retag(memledger::charge::CtxOwner::named(o));
    }

    /// `set_owner` over the whole live subtree: this context and every live descendant are `o`'s from now, each one's door-1
    /// bytes moving with it in one step, and the contexts made under any of them from now take `o`. A holder ADOPTS a
    /// product this way when it files it (the program cache taking a program's hosted owner and the contexts made under
    /// it), where `set_owner` would leave the children on the building statement. A mapped block (door 2) stays on the
    /// owner it mapped for, as with `set_owner`. Children dropped before the call are skipped (their weak links pruned).
    ///
    /// Debug builds check that each descendant was this context's owner's (as it was before the call; one that followed an
    /// ended owner's redirect while the other did not is the same owner's, `CtxOwner::followed`), unnamed, or already `o`'s:
    /// a context another owner named, hung under a product by mistake, must not move silently.
    #[cfg(feature = "std")]
    pub fn set_owner_tree(&self, o: memledger::Owner) {
        let to = memledger::charge::CtxOwner::named(o);
        let from = self.acct.led.owner();
        self.acct.led.retag(to);
        let mut work = alloc::vec::Vec::new();
        push_live_children(&self.acct, 0, &mut work);
        while let Some((_, c)) = work.pop() {
            let was = c.led.owner();
            debug_assert!(was == from || was == to || !was.is_named() || was.followed() == from.followed(), "mcx: set_owner_tree over a descendant another owner named ({was:?}, the product's {from:?})");
            c.led.retag(to);
            push_live_children(&c, 0, &mut work);
        }
    }

    /// LEDGER2 step 2: `set_owner` with the calling thread's session (a session-lifetime root: TopTransactionContext,
    /// MessageContext, CacheMemoryContext, a `session_root`); nothing off a backend thread.
    pub fn set_owner_session(&self) {
        #[cfg(feature = "std")]
        {
            let a = memledger::current();
            if a.is_backend() {
                self.set_owner(memledger::Owner::Session(a));
            }
        }
    }

    /// LEDGER2 step 2: `set_owner` with the calling backend's portal `token` (its PortalStart, `memledger::charge::
    /// portal_begin`): the PortalContext's bytes are the portal's; nothing off a backend thread.
    pub fn set_owner_portal(&self, token: u64) {
        #[cfg(feature = "std")]
        {
            let a = memledger::current();
            if a.is_backend() {
                self.set_owner(memledger::Owner::Portal { acct: a, portal: token });
            }
        }
        #[cfg(not(feature = "std"))]
        let _ = token;
    }

    pub fn is_child_of(&self, parent: &MemoryContext) -> bool {
        self.acct.parent.as_ref().is_some_and(|p| p.ptr == parent.acct.ptr)
    }

    pub fn new_child(&self, name: &'static str) -> MemoryContext {
        Self::with_backend(
            name,
            Backend::Aset(core::cell::UnsafeCell::new(aset::AllocSet::new())),
            Some(self.acct.clone()),
        )
    }

    /// C's ALLOCSET_SMALL_SIZES child: 1 kB first block, 8 kB block cap. The
    /// first block is a parked small keeper when there is one (aset.c's
    /// context_freelists[1]), else allocated at the first alloc.
    pub fn new_child_small(&self, name: &'static str) -> MemoryContext {
        Self::with_backend(
            name,
            Backend::Aset(core::cell::UnsafeCell::new(aset::AllocSet::new_small(ALLOCSET_SMALL_MAXSIZE))),
            Some(self.acct.clone()),
        )
    }

    /// Default-size child whose first block waits for the first alloc
    /// instead of taking a recycled keeper.
    pub fn new_child_lazy(&self, name: &'static str) -> MemoryContext {
        Self::with_backend(
            name,
            Backend::Aset(core::cell::UnsafeCell::new(aset::AllocSet::new_sized(
                aset::INIT_BLOCK_SIZE,
                aset::MAX_BLOCK_SIZE,
            ))),
            Some(self.acct.clone()),
        )
    }

    /// Live chunks are dropped at reset, not reported as leaks.
    pub fn new_child_wholesale(&self, name: &'static str) -> MemoryContext {
        let ctx = self.new_child(name);
        ctx.acct.wholesale_reset.set(true);
        ctx
    }

    /// A per-window bump context (gravity fmgr sites, `bind_under_window`): its keeper sized by the caller (clamped to
    /// 8 KiB..1 MiB, a power of two) so a normal window never leaves it, and up to `retain_cap` bytes of extra blocks kept
    /// across reset and reused in order, so a steady-state window allocates nothing. Every other bump context keeps
    /// bump.c's reset (extra blocks freed).
    pub fn new_child_bump_window(&self, name: &'static str, keeper_bytes: usize, retain_cap: usize) -> MemoryContext {
        let keeper = keeper_bytes.clamp(WINDOW_KEEPER_MIN, WINDOW_KEEPER_MAX).next_power_of_two();
        let cap = retain_cap.min(u32::MAX as usize) as u32;
        Self::with_backend(
            name,
            Backend::Bump(core::cell::UnsafeCell::new(bump::BumpArena::window(keeper as u32, cap))),
            Some(self.acct.clone()),
        )
    }

    /// Root counterpart of [`new_child_small`](Self::new_child_small) whose
    /// reset drops live chunks.
    pub fn new_small_wholesale(name: &'static str) -> Self {
        let ctx = Self::with_backend(
            name,
            Backend::Aset(core::cell::UnsafeCell::new(aset::AllocSet::new_small(ALLOCSET_SMALL_MAXSIZE))),
            None,
        );
        ctx.acct.wholesale_reset.set(true);
        ctx
    }

    pub fn new_child_bump(&self, name: &'static str) -> MemoryContext {
        Self::with_backend(name, Backend::Bump(new_arena()), Some(self.acct.clone()))
    }

    pub fn new_child_bump_with_max_block_size(
        &self,
        name: &'static str,
        max_block_size: usize,
    ) -> MemoryContext {
        Self::with_backend(
            name,
            Backend::Bump(core::cell::UnsafeCell::new(bump::BumpArena::with_max_block_size(max_block_size))),
            Some(self.acct.clone()),
        )
    }

    pub fn new_bumpdrop(name: &'static str) -> Self {
        Self::with_backend(
            name,
            Backend::BumpDrop(new_arena(), RefCell::new(DropList::new())),
            None,
        )
    }

    pub fn new_child_bumpdrop(&self, name: &'static str) -> MemoryContext {
        Self::with_backend(
            name,
            Backend::BumpDrop(new_arena(), RefCell::new(DropList::new())),
            Some(self.acct.clone()),
        )
    }

    pub fn new_bumpforget(name: &'static str) -> Self {
        Self::with_backend(name, Backend::BumpForget(new_arena()), None)
    }

    pub fn new_child_bumpforget(&self, name: &'static str) -> MemoryContext {
        Self::with_backend(name, Backend::BumpForget(new_arena()), Some(self.acct.clone()))
    }

    pub fn new_generation(name: &'static str) -> Self {
        Self::with_backend(
            name,
            Backend::Generation(core::cell::UnsafeCell::new(generation::GenArena::new())),
            None,
        )
    }

    pub fn new_child_generation(&self, name: &'static str) -> MemoryContext {
        Self::with_backend(
            name,
            Backend::Generation(core::cell::UnsafeCell::new(generation::GenArena::new())),
            Some(self.acct.clone()),
        )
    }

    /// `block_size` must be a power of two >= 1024; every alloc must be exactly `chunk_size`.
    pub fn new_slab(name: &'static str, block_size: usize, chunk_size: usize) -> Self {
        Self::with_backend(
            name,
            Backend::Slab(core::cell::UnsafeCell::new(slab::SlabArena::new(
                block_size, chunk_size,
            ))),
            None,
        )
    }

    pub fn new_child_slab(
        &self,
        name: &'static str,
        block_size: usize,
        chunk_size: usize,
    ) -> MemoryContext {
        Self::with_backend(
            name,
            Backend::Slab(core::cell::UnsafeCell::new(slab::SlabArena::new(
                block_size, chunk_size,
            ))),
            Some(self.acct.clone()),
        )
    }

    fn with_backend(name: &'static str, backend: Backend, parent: Option<AcctRc>) -> Self {
        Self::with_backend_as(name, backend, parent, true)
    }

    fn with_backend_as(name: &'static str, backend: Backend, parent: Option<AcctRc>, observed: bool) -> Self {
        Self::with_backend_full(name, backend, parent, observed, false)
    }

    fn with_backend_full(name: &'static str, backend: Backend, parent: Option<AcctRc>, observed: bool, unpooled: bool) -> Self {
        let (is_bump, kind, init_footprint, init_nblocks) = match &backend {
            Backend::Aset(a) => {
                // SAFETY: exclusive during construction.
                let a = unsafe { &*a.get() };
                (false, "AllocSet", a.footprint(), a.nblocks())
            }
            Backend::Malloc => (false, "Malloc", 0usize, 0usize),
            Backend::Bump(a) | Backend::BumpDrop(a, _) | Backend::BumpForget(a) => {
                // SAFETY: exclusive during construction.
                let a = unsafe { &*a.get() };
                (true, "Bump", a.footprint(), a.nblocks())
            }
            Backend::Generation(a) => {
                // SAFETY: exclusive during construction.
                let a = unsafe { &*a.get() };
                (true, "Generation", a.footprint(), a.nblocks())
            }
            Backend::Slab(a) => {
                // SAFETY: exclusive during construction.
                let a = unsafe { &*a.get() };
                (true, "Slab", a.footprint(), a.nblocks())
            }
        };
        if let Some(p) = &parent {
            if p.send_root.get() {
                send_root_refuses("a child", p.name.get());
            }
        } else {
            reserve_oom_errors();
        }
        let limited_path = parent.as_ref().is_some_and(|p| {
            p.limited_path.get() || p.limit.get() != usize::MAX
        });
        let val = Acct {
            name: Cell::new(name),
            ident: RefCell::new(None),
            self_used: Cell::new(0),
            wholesale_reset: Cell::new(false),
            send_root: Cell::new(false),
            unpooled,
            self_peak: Cell::new(0),
            limit: Cell::new(usize::MAX),
            limited_path: Cell::new(limited_path),
            arena_footprint: Cell::new(init_footprint),
            arena_nblocks: Cell::new(init_nblocks),
            window_tail: Cell::new(0),
            live_chunk_bytes: Cell::new(0),
            free_chunks: Cell::new(0),
            empty_blocks: Cell::new(0),
            is_bump,
            kind,
            parent,
            children: RefCell::new(if unpooled { alloc::vec::Vec::new() } else { child_vec_take() }),
            led: crate::owner::Led::NEW,
        };
        let acct = if unpooled { AcctRc::new_fresh(val) } else { AcctRc::new(val) };
        // LEDGER2 step 2: a child takes its parent's owner; an unnamed root the creating
        // thread's owner, once, here; a named root is re-owned by its constructor (`set_owner`)
        #[cfg(feature = "std")]
        acct.led.attach(acct.parent.as_ref().map_or_else(|| memledger::charge::CtxOwner::inferred(name), |p| p.led.owner()), init_footprint);
        if let Some(p) = &acct.parent {
            let mut children = p.children.borrow_mut();
            if children.len() == children.capacity() {
                children.retain(|w| w.strong_count() > 0);
            }
            children.push(acct.downgrade());
        } else if observed {
            notify_root_observer(&acct);
        }
        if debug_census::on() {
            debug_census::created(name);
        }
        MemoryContext {
            acct,
            backend,
            reset_cbs: RefCell::new(alloc::vec::Vec::new()),
            is_reset: Cell::new(true),
            poisoned: Cell::new(false),
            #[cfg(debug_assertions)]
            home: Cell::new(0),
            slow_calls: SlowCalls::default(),
        }
    }

    fn retire_session_root(&mut self) {
        // Clear the flag on every exit path, including a panic unwinding out of reset(), via a Drop guard.
        struct RetireFlagGuard;
        impl Drop for RetireFlagGuard {
            fn drop(&mut self) {
                SESSION_ROOT_RETIRING.with(|r| r.set(false));
            }
        }
        SESSION_ROOT_RETIRING.with(|r| r.set(true));
        let _guard = RetireFlagGuard;
        self.reset();
        self.release_arena();
        self.poisoned.set(true);
    }

    // A blockless arena stays behind the retired shell.
    fn release_arena(&mut self) {
        match &mut self.backend {
            Backend::Aset(a) => a.get_mut().release_keeper(),
            Backend::Bump(a) | Backend::BumpDrop(a, _) | Backend::BumpForget(a) => {
                a.get_mut().release_keeper()
            }
            Backend::Generation(a) => *a.get_mut() = generation::GenArena::new(),
            Backend::Malloc | Backend::Slab(_) => {}
        }
        let acct = &*self.acct;
        acct.self_used.set(0);
        acct.self_peak.set(0);
        acct.led.reset_mapped(0);
        acct.set_footprint(0);
        acct.arena_nblocks.set(0);
        acct.window_tail.set(0);
        acct.live_chunk_bytes.set(0);
        acct.free_chunks.set(0);
    }

    /// For a retired session root it converts a use-after-teardown into a deterministic panic in debug builds.
    #[inline(always)]
    fn check_live(&self) {
        #[cfg(debug_assertions)]
        if self.poisoned.get() {
            poisoned_access(self.acct.name.get());
        }
    }

    #[cfg(debug_assertions)]
    #[inline(always)]
    fn check_not_freed(&self, ptr: NonNull<u8>, layout: Layout, realloc: bool) {
        if layout.size() == 0 {
            return;
        }
        // SAFETY: shared read of arena bookkeeping; no &mut borrow is live (the
        // arenas are only ever borrowed for a single statement).
        let freed = unsafe {
            match &self.backend {
                Backend::Aset(set) => (*set.get()).is_freed(ptr, layout),
                Backend::Generation(a) => (*a.get()).is_freed(ptr),
                Backend::Slab(a) => !realloc && (*a.get()).is_freed(ptr),
                Backend::Malloc | Backend::Bump(_) | Backend::BumpDrop(..) | Backend::BumpForget(_) => {
                    false
                }
            }
        };
        if freed {
            freed::report(realloc, self.acct.name.get(), ptr);
        }
    }

    /// Contract: set the limit before creating children (limited_path cache).
    pub fn with_limit(self, limit: usize) -> Self {
        debug_assert!(
            self.acct.children.borrow().iter().all(|w| w.strong_count() == 0),
            "with_limit must be set before creating children (limited_path cache would go stale)",
        );
        if limit != usize::MAX {
            self.acct.limited_path.set(true);
        }
        self.acct.limit.set(limit);
        self
    }

    pub fn mcx(&self) -> Mcx<'_> {
        #[cfg(debug_assertions)]
        self.mark_home();
        Mcx(self)
    }

    /// Debug builds: the context hands out a handle at this address now (`check_home`).
    #[cfg(debug_assertions)]
    #[inline(always)]
    fn mark_home(&self) {
        let at = self as *const MemoryContext as usize;
        self.home.set(at);
        self.acct.home().set(at);
    }

    /// Debug builds: the context an Mcx points at still lives at that address. Every handle comes from `mcx`, which
    /// records the address, and the borrow checker keeps a context from moving while a handle borrows it, so a
    /// mismatch means unsafe code lifetime-extended a handle (a transmute to `Mcx<'static>`) over a context it held by
    /// value and then moved it: the handle now reads either reused bytes (its own record no longer names this address)
    /// or the context's stale copy (the shared record names the address the moved context handed a handle out at
    /// since). Checked before anything else is read through the handle.
    #[cfg(debug_assertions)]
    #[inline(always)]
    fn check_home(&self) {
        let at = self as *const MemoryContext as usize;
        if self.home.get() != at {
            moved_context(None);
        }
        if self.acct.home().get() != at {
            moved_context(Some(self.acct.name.get()));
        }
    }

    pub fn name(&self) -> &'static str {
        self.acct.name.get()
    }

    pub fn set_name(&self, name: &'static str) {
        self.acct.name.set(name);
    }

    pub fn set_ident(&self, id: Option<&str>) {
        *self.acct.ident.borrow_mut() = id.map(alloc::string::String::from);
    }

    pub fn detach_from_parent(&self) {
        if let Some(p) = &self.acct.parent {
            p.children.borrow_mut().retain(|w| w.ptr != self.acct.ptr);
        }
    }

    pub fn reattach_to_parent(&self) {
        if let Some(p) = &self.acct.parent {
            p.children.borrow_mut().push(self.acct.downgrade());
        }
    }

    pub fn ident(&self) -> Option<alloc::string::String> {
        self.acct.ident.borrow().clone()
    }

    pub fn used(&self) -> usize {
        self.acct.self_used.get()
    }

    pub fn subtree_used(&self) -> usize {
        self.acct.subtree_sum()
    }

    /// C `MemoryContextIsEmpty` (mcxt.c:749): a child or a reset callback (mcxt.c:583) makes it non-empty.
    pub fn is_empty_like_c(&self) -> bool {
        self.reset_cbs.borrow().is_empty()
            && !self.acct.children.borrow().iter().any(|w| w.strong_count() > 0)
            && self.subtree_used() == 0
    }

    pub fn subtree_allocated(&self) -> usize {
        self.acct.subtree_allocated_sum()
    }

    pub fn peak(&self) -> usize {
        self.acct.self_peak.get()
    }

    pub fn subtree_peak(&self) -> usize {
        self.acct.subtree_peak_sum()
    }

    pub fn limit(&self) -> usize {
        self.acct.limit.get()
    }

    pub fn register_reset_callback(&self, cb: impl FnOnce() + 'static) {
        if self.acct.send_root.get() {
            send_root_refuses("a reset callback", self.acct.name.get());
        }
        self.is_reset.set(false);
        self.reset_cbs.borrow_mut().push(alloc::boxed::Box::new(cb));
    }

    #[inline]
    pub fn reset(&mut self) {
        self.check_live();
        if *self.is_reset.get_mut() {
            return;
        }
        if let Backend::Bump(a) | Backend::BumpForget(a) = &mut self.backend {
            if self.reset_cbs.get_mut().is_empty() && a.get_mut().rewind_keeper() {
                *self.is_reset.get_mut() = true;
                return;
            }
        }
        self.reset_dirty();
    }

    // is_reset flips only after the work: a panicking destructor unwinds with the flag still false, so the retried reset re-drains.
    #[inline(never)]
    fn reset_dirty(&mut self) {
        if !self.reset_cbs.get_mut().is_empty() {
            self.fire_reset_callbacks();
        }
        if let Backend::Bump(a) | Backend::BumpForget(a) = &mut self.backend {
            let a = a.get_mut();
            if a.rewind_keeper() {
                self.is_reset.set(true);
                return;
            }
            a.reset();
            let footprint = a.footprint();
            let acct = &*self.acct;
            acct.self_used.set(footprint);
            acct.self_peak.set(footprint);
            acct.led.reset_mapped(footprint);
            acct.set_footprint(footprint);
            acct.arena_nblocks.set(a.nblocks());
            acct.window_tail.set(a.window_tail());
            self.is_reset.set(true);
            return;
        }
        self.reset_noncore();
        self.is_reset.set(true);
    }

    #[cold]
    #[inline(never)]
    fn reset_noncore(&mut self) {
        // Leak check only for exact-accounting backends, and never during a session-root retirement.
        if !self.acct.is_bump
            && !self.acct.wholesale_reset.get()
            && !SESSION_ROOT_RETIRING.with(core::cell::Cell::get)
        {
            debug_assert_eq!(
                self.acct.self_used.get(),
                0,
                "context {:?} reset with {} bytes still charged (leaked allocation?)",
                self.acct.name.get(),
                self.acct.self_used.get(),
            );
        }
        let acct = &*self.acct;
        match &mut self.backend {
            Backend::Aset(set) => {
                set.get_mut().reset(acct);
                acct.self_used.set(0);
                acct.self_peak.set(0);
            }
            Backend::Malloc => {
                acct.self_used.set(0);
                acct.led.reset_mapped(0);
                acct.set_footprint(0);
                acct.arena_nblocks.set(0);
                acct.self_peak.set(0);
            }
            Backend::Bump(_) | Backend::BumpForget(_) => unreachable!("handled in reset()"),
            Backend::BumpDrop(a, droplist) => {
                // Run destructors BEFORE the bytes are reclaimed (order load-bearing).
                run_drop_glue(|| droplist.get_mut().pop());
                let a = a.get_mut();
                a.reset();
                let footprint = a.footprint();
                acct.self_used.set(footprint);
                acct.self_peak.set(footprint);
                acct.led.reset_mapped(footprint);
                acct.set_footprint(footprint);
                acct.arena_nblocks.set(a.nblocks());
                acct.window_tail.set(a.window_tail());
            }
            Backend::Generation(a) => {
                let a = a.get_mut();
                a.reset();
                acct.self_used.set(0);
                acct.self_peak.set(0);
                acct.led.reset_mapped(a.footprint());
                acct.set_footprint(a.footprint());
                acct.arena_nblocks.set(a.nblocks());
                acct.window_tail.set(a.idle_keeper_tail());
                acct.free_chunks.set(0);
            }
            Backend::Slab(a) => {
                let a = a.get_mut();
                a.reset();
                acct.self_used.set(0);
                acct.self_peak.set(0);
                acct.led.reset_mapped(a.footprint());
                acct.set_footprint(a.footprint());
                acct.arena_nblocks.set(a.nblocks());
                acct.window_tail.set(0);
                acct.free_chunks.set(0);
                acct.empty_blocks.set(0);
            }
        }
    }

    /// Tests: the allocator that served dedicated chunk `p` of this context as the chunk recorded it (`heap::Served`; debug
    /// builds check it against the block source's lookup): Some(true) the block source; None when not recorded, the bit is
    /// off or `p` is no dedicated chunk here.
    #[doc(hidden)]
    pub fn dedicated_served(&self, p: NonNull<u8>) -> Option<bool> {
        match &self.backend {
            // SAFETY: single-statement borrow, never re-entered (as stats).
            Backend::Aset(a) => unsafe { &*a.get() }.dedicated_served(p),
            // SAFETY: as above.
            Backend::Bump(a) | Backend::BumpDrop(a, _) | Backend::BumpForget(a) => unsafe { &*a.get() }.oversize_served(p),
            _ => None,
        }
    }

    pub fn mem_allocated(&self) -> usize {
        match &self.backend {
            Backend::Bump(a) | Backend::BumpDrop(a, _) | Backend::BumpForget(a) => {
                // SAFETY: single-statement borrow, never re-entered (as stats).
                unsafe { &*a.get() }.mem_allocated()
            }
            _ => self.stats().arena_footprint,
        }
    }

    pub fn stats(&self) -> ContextStats {
        ContextStats {
            name: self.acct.name.get(),
            ident: self.ident(),
            used: self.acct.self_used.get(),
            peak: self.acct.self_peak.get(),
            subtree_used: self.acct.subtree_sum(),
            subtree_peak: self.acct.subtree_peak_sum(),
            limit: self.acct.limit.get(),
            arena_footprint: match &self.backend {
                Backend::Aset(_) => self.acct.arena_footprint.get(),
                Backend::Malloc => self.acct.self_used.get(),
                Backend::Bump(a) | Backend::BumpDrop(a, _) | Backend::BumpForget(a) => {
                    // SAFETY: single-statement borrow, never re-entered (as aset_mut).
                    unsafe { &*a.get() }.footprint()
                }
                // SAFETY: as above.
                Backend::Generation(a) => unsafe { &*a.get() }.footprint(),
                // SAFETY: as above.
                Backend::Slab(a) => unsafe { &*a.get() }.footprint(),
            },
            nblocks: self.acct.arena_nblocks.get(),
            free_bytes: self.acct.free_bytes(),
            free_chunks: self.acct.free_chunks.get(),
        }
    }

    pub fn stats_tree(&self) -> TreeStats {
        tree_stats_node(&self.acct)
    }

    #[cold]
    pub fn oom(&self, request: usize) -> PgError {
        crate::oom_named(self.acct.name.get(), request)
    }

    pub fn is_bumpforget(&self) -> bool {
        matches!(self.backend, Backend::BumpForget(..))
    }

    /// # Safety: `addr` — live `T` (glue's type) in this arena, Drop suppressed, valid until reset.
    unsafe fn register_drop(&self, addr: *mut u8, glue: unsafe fn(*mut u8)) -> bool {
        if let Backend::BumpDrop(_, droplist) = &self.backend {
            self.is_reset.set(false);
            droplist.borrow_mut().entries.push(DropEntry { addr, glue });
            true
        } else {
            false
        }
    }

    fn fire_reset_callbacks(&self) {
        loop {
            let cb = self.reset_cbs.borrow_mut().pop();
            match cb {
                Some(cb) => cb(),
                None => break,
            }
        }
    }

    // always-inline: on the AllocSet lanes an outlined charge is a call per palloc.
    #[inline(always)]
    fn charge(&self, n: usize) -> Result<(), AllocError> {
        self.acct.check_limit(n)?;
        self.charge_unlimited(n);
        Ok(())
    }

    // charge()'s writes past check_limit, which has no effect when limited_path is false: Mcx::alloc_fast's pool hit
    // (D4483) charges with this, so a hit moves self_used / self_peak exactly as charge() does.
    #[inline(always)]
    fn charge_unlimited(&self, n: usize) {
        let acct = &*self.acct;
        let self_new = acct.self_used.get() + n;
        acct.self_used.set(self_new);
        if self_new > acct.self_peak.get() {
            acct.self_peak.set(self_new);
        }
    }

    fn uncharge(&self, n: usize) {
        let acct = &*self.acct;
        debug_assert!(
            acct.self_used.get() >= n,
            "context {:?} uncharging {} with only {} charged",
            acct.name.get(),
            n,
            acct.self_used.get(),
        );
        acct.self_used.set(acct.self_used.get().saturating_sub(n));
    }
}

static OOM_OBSERVER: core::sync::atomic::AtomicPtr<()> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_oom_observer(f: fn(context_name: &str, request: usize)) {
    OOM_OBSERVER.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

fn notify_oom_observer(context_name: &str, request: usize) {
    let p = OOM_OBSERVER.load(core::sync::atomic::Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: only set_oom_observer stores here, always from the fn type.
        let f: fn(&str, usize) = unsafe { core::mem::transmute(p) };
        f(context_name, request);
    }
}

#[cold]
#[cold]
#[inline(never)]
pub fn raise_oom_named(context_name: &str, request: usize) -> ! {
    #[cfg(any(feature = "std", test))]
    std::panic::panic_any(alloc::boxed::Box::new(oom_named(context_name, request)));
    #[cfg(not(any(feature = "std", test)))]
    panic!("out of memory in {context_name}: {request} bytes")
}

pub fn oom_named(context_name: &str, request: usize) -> PgError {
    notify_oom_observer(context_name, request);
    PgError::error("out of memory")
        .with_sqlstate(ERRCODE_OUT_OF_MEMORY)
        .with_detail(alloc::format!(
            "Failed on request of size {request} in memory context \"{context_name}\"."
        ))
}

// C's ErrorContext reserve: pre-built OOM errors for when even the error's
// Box cannot be allocated.
const OOM_RESERVE_DETAIL_CAP: usize = 256;
static OOM_RESERVE: [core::sync::atomic::AtomicPtr<PgError>; 4] =
    [const { core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()) }; 4];

pub fn reserve_oom_errors() {
    use core::sync::atomic::Ordering;
    for slot in &OOM_RESERVE {
        if !slot.load(Ordering::Relaxed).is_null() {
            continue;
        }
        let e = PgError::error("out of memory")
            .with_sqlstate(ERRCODE_OUT_OF_MEMORY)
            .with_detail(alloc::string::String::with_capacity(OOM_RESERVE_DETAIL_CAP));
        let p = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(e));
        if slot
            .compare_exchange(core::ptr::null_mut(), p, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            // SAFETY: `p` came from Box::into_raw above and was never published.
            drop(unsafe { alloc::boxed::Box::from_raw(p) });
        }
    }
}

fn take_reserved_oom_error() -> Option<alloc::boxed::Box<PgError>> {
    OOM_RESERVE.iter().find_map(|slot| {
        let p = slot.swap(core::ptr::null_mut(), core::sync::atomic::Ordering::AcqRel);
        // SAFETY: slots only ever hold pointers from Box::into_raw in
        // reserve_oom_errors; the swap hands this one to us alone.
        (!p.is_null()).then(|| unsafe { alloc::boxed::Box::from_raw(p) })
    })
}

struct NoGrow<'a>(&'a mut alloc::string::String);

impl fmt::Write for NoGrow<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.0.capacity() - self.0.len() < s.len() {
            return Err(fmt::Error);
        }
        self.0.push_str(s);
        Ok(())
    }
}

#[cold]
#[inline(never)]
pub fn oom_boxed_named(context_name: &str, request: usize) -> alloc::boxed::Box<PgError> {
    let layout = Layout::new::<PgError>();
    // SAFETY: PgError is not zero-sized.
    let raw = unsafe { alloc::alloc::alloc(layout) }.cast::<PgError>();
    if raw.is_null() {
        notify_oom_observer(context_name, request);
        let Some(mut e) = take_reserved_oom_error() else {
            alloc::alloc::handle_alloc_error(layout)
        };
        let detail = e.detail.get_or_insert_with(alloc::string::String::new);
        detail.clear();
        let wrote = fmt::write(
            &mut NoGrow(detail),
            format_args!("Failed on request of size {request} in memory context \"{context_name}\"."),
        );
        if wrote.is_err() {
            e.detail = None;
        }
        return e;
    }
    let e = oom_named(context_name, request);
    // SAFETY: `raw` is a fresh allocation with PgError's layout; writing
    // initializes it and Box::from_raw takes ownership with the same layout.
    let b = unsafe {
        raw.write(e);
        alloc::boxed::Box::from_raw(raw)
    };
    reserve_oom_errors();
    b
}

#[cold]
pub fn oom_malloc() -> PgError {
    PgError::error("out of memory").with_sqlstate(ERRCODE_OUT_OF_MEMORY)
}

#[inline]
pub fn std_vec_with_capacity<T>(context_name: &str, cap: usize) -> PgResult<alloc::vec::Vec<T>> {
    let mut v = alloc::vec::Vec::new();
    std_vec_reserve(context_name, &mut v, cap)?;
    Ok(v)
}

#[inline]
pub fn std_vec_reserve<T>(
    context_name: &str,
    v: &mut alloc::vec::Vec<T>,
    additional: usize,
) -> PgResult<()> {
    v.try_reserve_exact(additional).map_err(|_| {
        oom_named(context_name, additional.saturating_mul(core::mem::size_of::<T>()))
    })?;
    Ok(())
}

/// Reserves the growth first, so the resize itself cannot allocate.
#[inline]
pub fn std_vec_resize<T: Clone>(
    context_name: &str,
    v: &mut alloc::vec::Vec<T>,
    new_len: usize,
    value: T,
) -> PgResult<()> {
    if new_len > v.len() {
        std_vec_reserve(context_name, v, new_len - v.len())?;
    }
    v.resize(new_len, value);
    Ok(())
}

#[inline]
pub fn std_vec_from_elem<T: Clone>(context_name: &str, value: T, n: usize) -> PgResult<alloc::vec::Vec<T>> {
    let mut v = std_vec_with_capacity(context_name, n)?;
    v.resize(n, value);
    Ok(v)
}

#[inline]
pub fn pgvec_reserve<'mcx, T>(v: &mut PgVec<'mcx, T>, additional: usize) -> PgResult<()> {
    let mcx = *v.allocator();
    v.try_reserve(additional)
        .map_err(|_| mcx.oom_boxed(additional.saturating_mul(core::mem::size_of::<T>())))?;
    Ok(())
}

#[inline]
pub fn pgvec_resize<'mcx, T: Clone>(v: &mut PgVec<'mcx, T>, new_len: usize, value: T) -> PgResult<()> {
    if new_len > v.len() {
        pgvec_reserve(v, new_len - v.len())?;
    }
    v.resize(new_len, value);
    Ok(())
}

#[inline]
pub fn std_vec_from_slice<T: Clone>(context_name: &str, src: &[T]) -> PgResult<alloc::vec::Vec<T>> {
    let mut v = std_vec_with_capacity(context_name, src.len())?;
    v.extend_from_slice(src);
    Ok(v)
}

#[cold]
#[inline(never)]
fn send_root_refuses(what: &str, name: &str) -> ! {
    panic!("mcx: SendRoot {name:?} takes no {what} (it would tie the root to the thread that made it)");
}

/// Debug builds: an Mcx used after its context moved (`MemoryContext::check_home`); `name` when the stale bytes still
/// name the context.
#[cfg(debug_assertions)]
#[cold]
#[inline(never)]
fn moved_context(name: Option<&str>) -> ! {
    let ctx = name.map_or_else(|| alloc::string::String::from("(its old address now holds other bytes)"), |n| alloc::format!("{n:?}"));
    panic!(
        "mcx: a memory context moved after a handle to it was taken: an Mcx still points at its old address (context \
         {ctx}); a lifetime-extended handle must come from an address-stable owner (PinnedContext, McxOwned, SendRoot)"
    );
}

#[cfg(debug_assertions)]
#[cold]
#[inline(never)]
fn poisoned_access(name: &str) -> ! {
    panic!(
        "mcx: access to session-root context {name:?} after session teardown \
         (use-after-teardown)"
    );
}

impl Drop for MemoryContext {
    fn drop(&mut self) {
        self.fire_reset_callbacks();
        if let Backend::BumpDrop(_, droplist) = &self.backend {
            // Pop each entry under a short scoped borrow, dropping the guard before running user drop glue.
            run_drop_glue(|| droplist.borrow_mut().pop());
        }
        self.acct.ident.borrow_mut().take();
        self.acct.self_used.set(0);
        self.acct.window_tail.set(0);
        // A node kept alive by its children's parent links reports no blocks from here on (its owner's bytes leave with them).
        self.acct.led.reset_mapped(0);
        self.acct.set_blocks(0, 0);
        self.acct.live_chunk_bytes.set(0);
        self.acct.free_chunks.set(0);
        if self.acct.unpooled {
            if let Backend::Aset(a) = &self.backend {
                // SAFETY: the drop's &mut: no chunk in use, nothing else reaches the arena.
                unsafe { (*a.get()).release_unpooled() };
            }
        }
    }
}

impl fmt::Debug for MemoryContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryContext")
            .field("name", &self.acct.name.get())
            .field("used", &self.acct.self_used.get())
            .field("subtree_used", &self.acct.subtree_sum())
            .field("peak", &self.acct.self_peak.get())
            .field("limit", &self.acct.limit.get())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextStats {
    pub name: &'static str,
    pub ident: Option<alloc::string::String>,
    pub used: usize,
    pub peak: usize,
    pub subtree_used: usize,
    pub subtree_peak: usize,
    pub limit: usize,
    pub arena_footprint: usize,
    pub nblocks: usize,
    pub free_bytes: usize,
    pub free_chunks: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeStats {
    pub name: &'static str,
    pub ident: Option<alloc::string::String>,
    pub kind: &'static str,
    pub used: usize,
    pub peak: usize,
    pub subtree_used: usize,
    pub subtree_peak: usize,
    pub limit: usize,
    pub is_bump: bool,
    pub arena_footprint: usize,
    pub nblocks: usize,
    pub free_bytes: usize,
    pub free_chunks: usize,
    pub empty_blocks: usize,
    pub children: alloc::vec::Vec<TreeStats>,
}

// mcxt.c:908 bounds its recursion by stack_is_too_deep and tallies the rest
// iteratively; the tree is one level per nested subxact, so never recurse here.
impl Drop for TreeStats {
    fn drop(&mut self) {
        let mut pending = core::mem::take(&mut self.children);
        while let Some(mut t) = pending.pop() {
            pending.append(&mut t.children);
        }
    }
}

fn tree_stats_leaf(acct: &Acct) -> TreeStats {
    TreeStats {
        name: acct.name.get(),
        ident: acct.ident.borrow().clone(),
        kind: acct.kind,
        used: acct.self_used.get(),
        peak: acct.self_peak.get(),
        subtree_used: acct.self_used.get(),
        subtree_peak: acct.self_peak.get(),
        limit: acct.limit.get(),
        is_bump: acct.is_bump,
        arena_footprint: acct.arena_footprint.get(),
        nblocks: acct.arena_nblocks.get(),
        free_bytes: acct.free_bytes(),
        free_chunks: acct.free_chunks.get(),
        empty_blocks: acct.empty_blocks.get(),
        children: alloc::vec::Vec::new(),
    }
}

// mcxt.c links a new child at the head of firstchild: siblings walk newest-first.
fn push_live_children(acct: &Acct, parent: usize, work: &mut alloc::vec::Vec<(usize, AcctRc)>) {
    let start = work.len();
    acct.children.borrow_mut().retain(|w| match w.upgrade() {
        Some(c) => {
            work.push((parent, c));
            true
        }
        None => false,
    });
    work[start..].reverse();
}

fn tree_stats_node(acct: &Acct) -> TreeStats {
    let mut nodes: alloc::vec::Vec<(usize, TreeStats)> = alloc::vec![(usize::MAX, tree_stats_leaf(acct))];
    let mut work = alloc::vec::Vec::new();
    push_live_children(acct, 0, &mut work);
    while let Some((parent, c)) = work.pop() {
        let idx = nodes.len();
        nodes.push((parent, tree_stats_leaf(&c)));
        push_live_children(&c, idx, &mut work);
    }
    while nodes.len() > 1 {
        let (parent, child) = nodes.pop().unwrap();
        let p = &mut nodes[parent].1;
        p.subtree_used = p.subtree_used.saturating_add(child.subtree_used);
        p.subtree_peak = p.subtree_peak.saturating_add(child.subtree_peak);
        p.children.push(child);
    }
    nodes.pop().unwrap().1
}

#[doc = "An allocation cannot outlive its context:"]
#[doc = "```compile_fail,E0597"]
#[doc = "let v;"]
#[doc = "{"]
#[doc = "    let ctx = mcx::MemoryContext::new(\"short-lived\");"]
#[doc = "    v = mcx::PgVec::<u8>::new_in(ctx.mcx());"]
#[doc = "} // `ctx` dropped here while `v` still borrows it"]
#[doc = "assert_eq!(v.len(), 0);"]
#[doc = "```"]
#[doc = "A reset is statically impossible while allocations are live:"]
#[doc = "```compile_fail,E0502"]
#[doc = "let mut ctx = mcx::MemoryContext::new_bump(\"per-tuple\");"]
#[doc = "let v = mcx::PgVec::<u8>::new_in(ctx.mcx());"]
#[doc = "ctx.reset(); // ERROR: `v` still borrows `ctx`"]
#[doc = "assert_eq!(v.len(), 0);"]
#[doc = "```"]
#[derive(Clone, Copy)]
pub struct Mcx<'mcx>(&'mcx MemoryContext);

impl<'mcx> Mcx<'mcx> {
    pub fn context(self) -> &'mcx MemoryContext {
        self.0
    }

    #[cold]
    pub fn oom(self, request: usize) -> PgError {
        self.0.oom(request)
    }

    /// C `palloc(n)` for the per-row lanes: the bump window's inline fast path (one compare, one store), every other case
    /// (a new block, an oversize chunk, a non-bump backend, `n` past MaxAllocSize) out of line. 8-byte aligned.
    #[inline(always)]
    pub fn alloc_bytes(self, n: usize) -> Result<NonNull<u8>, PgError> {
        #[cfg(debug_assertions)]
        self.0.check_home();
        if let Backend::Bump(a) | Backend::BumpForget(a) = &self.0.backend {
            // SAFETY: single-statement borrow, never re-entered (bump_mut).
            if let Some(p) = unsafe { bump_mut(a) }.try_bump(n) {
                self.0.is_reset.set(false);
                return Ok(p);
            }
        }
        self.alloc_bytes_slow(n)
    }

    #[cold]
    #[inline(never)]
    fn alloc_bytes_slow(self, n: usize) -> Result<NonNull<u8>, PgError> {
        let layout = Layout::from_size_align(n.max(1), 8).map_err(|_| self.oom(n))?;
        Allocator::allocate(&self, layout).map(|p| p.cast::<u8>()).map_err(|_| self.oom(n))
    }

    #[cold]
    #[inline(never)]
    pub fn oom_boxed(self, request: usize) -> alloc::boxed::Box<PgError> {
        oom_boxed_named(self.0.acct.name.get(), request)
    }

    // Backend dispatch emitted once out of line, not inlined per generic T.
    #[inline(never)]
    pub fn alloc_uninit_bytes(self, layout: Layout) -> Result<NonNull<u8>, AllocError> {
        Allocator::allocate(&self, layout).map(|p| p.cast::<u8>())
    }
}

impl fmt::Debug for Mcx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Mcx({:?})", self.0.acct.name.get())
    }
}

/// C mcxt.c: `palloc` rejects any request above `MaxAllocSize` before the
/// context sees it ("invalid memory alloc request size"). Out-of-line so the
/// allocate/grow fast paths carry only a compare + never-taken branch.
// mcxt.c MemoryContextSizeFailure: elog(ERROR), recovered at the statement
// boundary. Returning AllocError here would leave every infallible lane
// (PgVec::push, extend, resize) to handle_alloc_error — an abort of the whole
// server — where C raises a catchable error.
// the unwind carries the ERROR itself (a boxed PgError, XX000, the
// governor's form), never a plain message: the statement boundary recovers
// only a PgError payload and takes a plain panic as C's abort(), so a
// plain message restarted the server (a hosted DISTINCT run past 1 GiB,
// parked by its host's landing pad and resumed after the program returned).
#[cold]
#[inline(never)]
fn alloc_ceiling_exceeded(size: usize) -> ! {
    #[cfg(any(feature = "std", test))]
    std::panic::panic_any(invalid_alloc_size(size));
    #[cfg(not(any(feature = "std", test)))]
    panic!("invalid memory alloc request size {size}")
}

pub fn ceiling_error(size: usize) -> PgError {
    PgError::error(alloc::format!("invalid memory alloc request size {size}"))
}

// SAFETY contract for callers: one-statement &mut, never re-entered; one context, one thread.
#[inline(always)]
unsafe fn aset_mut(set: &core::cell::UnsafeCell<aset::AllocSet>) -> &mut aset::AllocSet {
    &mut *set.get()
}

// SAFETY contract: as aset_mut.
#[inline(always)]
unsafe fn bump_mut(a: &core::cell::UnsafeCell<bump::BumpArena>) -> &mut bump::BumpArena {
    &mut *a.get()
}

#[inline]
fn new_arena() -> core::cell::UnsafeCell<bump::BumpArena> {
    core::cell::UnsafeCell::new(bump::BumpArena::new())
}

// SAFETY (trait contract): delegates to aset/Global/bump; accounting is undone on failure.
unsafe impl Allocator for Mcx<'_> {
    // Only the hits inline at the call site (alloc_fast: an AllocSet pool hit or a Bump window hit); the ceiling check
    // and everything else is ONE out-of-line call (D4483: inlining the whole path put 5,869 copies, 2.8 MB, in .text). A
    // hit serves only non-dedicated sizes (<= the AllocSet / Bump chunk limit), far below MAX_ALLOC_SIZE, so the ceiling
    // check can wait for the slow path.
    #[inline(always)]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if let Some(hit) = self.alloc_fast(layout) {
            return Ok(hit);
        }
        self.allocate_slow(layout, true)
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        #[cfg(debug_assertions)]
        self.0.check_home();
        self.0.check_live();
        #[cfg(debug_assertions)]
        self.0.check_not_freed(ptr, layout, false);
        match &self.0.backend {
            Backend::Aset(set) => {
                self.0.uncharge(layout.size());
                #[cfg(test)]
                crate::churn_probe::bump();
                // SAFETY: single-statement borrow, never re-entered (aset_mut).
                unsafe { aset_mut(set) }.dealloc(ptr, layout, &self.0.acct)
            }
            Backend::Malloc => {
                self.0.uncharge(layout.size());
                #[cfg(test)]
                crate::churn_probe::bump();
                crate::heap::Heap.deallocate(ptr, layout)
            }
            // No BumpFree: bytes and charge release wholesale at reset.
            Backend::Bump(_) | Backend::BumpDrop(..) | Backend::BumpForget(_) => {}
            // SAFETY: single-statement borrow (as bump_mut); ptr/layout per trait contract.
            Backend::Generation(a) => unsafe {
                (*a.get()).dealloc(ptr, layout, &self.0.acct)
            },
            // SAFETY: as above.
            Backend::Slab(a) => unsafe { (*a.get()).dealloc(ptr, layout, &self.0.acct) },
        }
    }

    unsafe fn grow(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        if new_layout.size() > MAX_ALLOC_SIZE {
            alloc_ceiling_exceeded(new_layout.size());
        }
        self.grow_unchecked(ptr, old_layout, new_layout)
    }

    unsafe fn shrink(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        #[cfg(debug_assertions)]
        self.0.check_home();
        self.0.check_live();
        #[cfg(debug_assertions)]
        self.0.check_not_freed(ptr, old_layout, true);
        match &self.0.backend {
            Backend::Aset(set) => {
                // SAFETY: single-statement borrow, never re-entered (aset_mut).
                let result =
                    unsafe { aset_mut(set) }.realloc(ptr, old_layout, new_layout, &self.0.acct);
                if result.is_ok() {
                    self.0.uncharge(old_layout.size() - new_layout.size());
                }
                result
            }
            Backend::Malloc => {
                let result = crate::heap::Heap.shrink(ptr, old_layout, new_layout);
                if result.is_ok() {
                    self.0.uncharge(old_layout.size() - new_layout.size());
                }
                result
            }
            // A zero-size target releases the chunk now: a ZST deallocate is a no-op by contract, so a retained chunk would pin its block forever.
            Backend::Generation(a) => {
                if new_layout.size() == 0 {
                    // SAFETY: single-statement borrow (as bump_mut); ptr/old_layout per trait contract.
                    unsafe { (*a.get()).dealloc(ptr, old_layout, &self.0.acct) };
                    let dangling = new_layout.align() as *mut u8;
                    // SAFETY: align is a non-zero power of two, hence non-null.
                    let nn = unsafe { NonNull::new_unchecked(dangling) };
                    return Ok(NonNull::slice_from_raw_parts(nn, 0));
                }
                if ptr.as_ptr() as usize % new_layout.align() == 0 {
                    return Ok(NonNull::slice_from_raw_parts(ptr, new_layout.size()));
                }
                // SAFETY: single-statement borrow (as bump_mut); copy fits new_layout.
                unsafe {
                    let arena = &mut *a.get();
                    let new = arena.alloc(new_layout, &self.0.acct)?;
                    core::ptr::copy_nonoverlapping(
                        ptr.as_ptr(),
                        new.cast::<u8>().as_ptr(),
                        new_layout.size(),
                    );
                    arena.dealloc(ptr, old_layout, &self.0.acct);
                    Ok(new)
                }
            }
            Backend::Slab(_) => {
                if old_layout.size() == new_layout.size()
                    && ptr.as_ptr() as usize % new_layout.align() == 0
                {
                    Ok(NonNull::slice_from_raw_parts(ptr, new_layout.size()))
                } else {
                    Err(AllocError)
                }
            }
            // Bump: narrow in place, nothing to uncharge.
            Backend::Bump(a) | Backend::BumpDrop(a, _) | Backend::BumpForget(a) => {
                if ptr.as_ptr() as usize % new_layout.align() == 0 {
                    return Ok(NonNull::slice_from_raw_parts(ptr, new_layout.size()));
                }
                // SAFETY: single-statement borrow (bump_mut); copy fits new_layout.
                unsafe {
                    let new = bump_mut(a).alloc(new_layout, &self.0.acct)?;
                    core::ptr::copy_nonoverlapping(
                        ptr.as_ptr(),
                        new.cast::<u8>().as_ptr(),
                        new_layout.size(),
                    );
                    Ok(new)
                }
            }
        }
    }
    // shrink needs no ceiling: a legitimately-huge allocation may shrink to a size that is still above MaxAllocSize.
}

impl Mcx<'_> {
    /// # Safety
    /// As `Allocator::grow`; the caller has applied its own size ceiling.
    unsafe fn grow_unchecked(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        #[cfg(debug_assertions)]
        self.0.check_home();
        self.0.check_live();
        #[cfg(debug_assertions)]
        self.0.check_not_freed(ptr, old_layout, true);
        self.0.is_reset.set(false);
        match &self.0.backend {
            Backend::Aset(set) => {
                let delta = new_layout.size() - old_layout.size();
                self.0.charge(delta)?;
                // SAFETY: single-statement borrow, never re-entered (aset_mut).
                let result =
                    unsafe { aset_mut(set) }.realloc(ptr, old_layout, new_layout, &self.0.acct);
                if result.is_err() {
                    self.0.uncharge(delta);
                }
                result
            }
            Backend::Malloc => {
                let delta = new_layout.size() - old_layout.size();
                self.0.charge(delta)?;
                let result = crate::heap::Heap.grow(ptr, old_layout, new_layout);
                if result.is_err() {
                    self.0.uncharge(delta);
                }
                result
            }
            Backend::Bump(a) | Backend::BumpDrop(a, _) | Backend::BumpForget(a) => {
                // SAFETY: single-statement borrow (bump_mut); ptr/layouts per trait contract.
                unsafe { bump_mut(a).grow(ptr, old_layout, new_layout, &self.0.acct) }
            }
            // SAFETY: single-statement borrow (as bump_mut); ptr/layouts per trait contract.
            Backend::Generation(a) => unsafe {
                (*a.get()).grow(ptr, old_layout, new_layout, &self.0.acct)
            },
            // Slab realloc only tolerates the identical chunk size.
            Backend::Slab(_) => {
                if old_layout.size() == new_layout.size()
                    && ptr.as_ptr() as usize % new_layout.align() == 0
                {
                    Ok(NonNull::slice_from_raw_parts(ptr, new_layout.size()))
                } else {
                    Err(AllocError)
                }
            }
        }
    }
}

impl Mcx<'_> {
    // The allocation every hot lane executes, inlined at each site (D4483): an AllocSet context with no limit on its
    // path, served by a pool hit (AllocSet::alloc_hit). The hit makes exactly charge()'s writes (charge_unlimited:
    // check_limit has no effect when limited_path is false), after the hit instead of before it: nothing reads
    // self_used in between (admit_block reads it on the refill path only, which stays in the slow path) and a hit cannot
    // fail. A context with a limit on its path always takes the slow path. None = no state touched.
    // A Bump / BumpDrop / BumpForget context's window hit (PGRUST_MCX_BUMP_FAST, default on) is BumpArena::alloc's plain
    // bump, which charges nothing at any limit (bump accounting runs at block transitions, in the slow path), so the
    // inline hit makes exactly allocate_slow's writes: is_reset (reset() early-exits on it) and the window. Over-aligned,
    // ZST, dedicated sizes and a full window take the slow path. Before it every bump allocation through the Allocator
    // trait paid the out-of-line call, the ceiling check and the 5-way match, more than an AllocSet freelist hit.
    #[inline(always)]
    fn alloc_fast(&self, layout: Layout) -> Option<NonNull<[u8]>> {
        #[cfg(debug_assertions)]
        self.0.check_home();
        self.0.check_live();
        if let Backend::Aset(set) = &self.0.backend {
            let acct = &*self.0.acct;
            if !acct.limited_path.get() {
                // SAFETY: single-statement borrow, never re-entered (aset_mut).
                if let Some(hit) = unsafe { aset_mut(set) }.alloc_hit(layout, acct) {
                    self.0.is_reset.set(false);
                    self.0.charge_unlimited(layout.size());
                    return Some(hit);
                }
            }
            return None;
        }
        if let Backend::Bump(a) | Backend::BumpDrop(a, _) | Backend::BumpForget(a) = &self.0.backend {
            // Keeps the AllocSet hit's path as it was (one discriminant test, no taken branch): without the hint LLVM
            // tests the bump range first and every AllocSet hit pays 3 more instructions and a taken branch.
            core::hint::cold_path();
            if layout.align() <= aset::POOL_MAX_ALIGN {
                // SAFETY: single-statement borrow, never re-entered (bump_mut).
                if let Some(hit) = unsafe { bump_mut(a) }.try_bump_fast(layout.size()) {
                    self.0.is_reset.set(false);
                    return Some(hit);
                }
            }
        }
        None
    }

    #[inline(always)]
    fn allocate_unchecked(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if let Some(hit) = self.alloc_fast(layout) {
            return Ok(hit);
        }
        self.allocate_slow(layout, false)
    }

    // Everything but the pool hit, out of line once (D4483): the ceiling check of Allocator::allocate (`ceiling`), then
    // the full path: the 5-backend match, charge / check_limit, AllocSet refills and dedicated chunks, the failure
    // uncharge.
    #[inline(never)]
    fn allocate_slow(&self, layout: Layout, ceiling: bool) -> Result<NonNull<[u8]>, AllocError> {
        self.0.slow_calls.hit();
        if ceiling && layout.size() > MAX_ALLOC_SIZE {
            alloc_ceiling_exceeded(layout.size());
        }
        #[cfg(debug_assertions)]
        self.0.check_home();
        self.0.check_live();
        self.0.is_reset.set(false);
        match &self.0.backend {
            Backend::Aset(set) => {
                self.0.charge(layout.size())?;
                // SAFETY: single-statement borrow, never re-entered (aset_mut).
                let result = unsafe { aset_mut(set) }.alloc(layout, &self.0.acct);
                if result.is_err() {
                    self.0.uncharge(layout.size());
                }
                result
            }
            Backend::Malloc => {
                self.0.charge(layout.size())?;
                let result = crate::heap::Heap.allocate(layout);
                if result.is_err() {
                    self.0.uncharge(layout.size());
                }
                result
            }
            Backend::Bump(a) | Backend::BumpDrop(a, _) | Backend::BumpForget(a) => {
                // SAFETY: single-statement borrow, never re-entered (bump_mut).
                unsafe { bump_mut(a) }.alloc(layout, &self.0.acct)
            }
            Backend::Generation(a) => {
                // SAFETY: single-statement borrow, never re-entered (as bump_mut).
                unsafe { &mut *a.get() }.alloc(layout, &self.0.acct)
            }
            Backend::Slab(a) => {
                // SAFETY: single-statement borrow, never re-entered (as bump_mut).
                unsafe { &mut *a.get() }.alloc(layout, &self.0.acct)
            }
        }
    }

    #[inline(never)]
    pub fn alloc_uninit_bytes_huge(self, layout: Layout) -> Result<NonNull<u8>, AllocError> {
        if layout.size() > MAX_ALLOC_HUGE_SIZE {
            alloc_ceiling_exceeded(layout.size());
        }
        self.allocate_unchecked(layout).map(|p| p.cast::<u8>())
    }
}

pub const ALLOCSET_SMALL_INITSIZE: usize = 1024;
pub const ALLOCSET_SMALL_MAXSIZE: usize = 8 * 1024;
pub const MAX_ALLOC_SIZE: usize = 0x3FFF_FFFF;

pub const MAX_ALLOC_HUGE_SIZE: usize = usize::MAX / 2;

#[cold]
fn invalid_alloc_size(request: usize) -> alloc::boxed::Box<PgError> {
    PgError::error(alloc::format!("invalid memory alloc request size {request}")).into()
}

#[inline]
pub fn check_alloc_size(request: usize) -> PgResult<()> {
    if request > MAX_ALLOC_SIZE {
        return Err(invalid_alloc_size(request));
    }
    Ok(())
}

#[inline]
pub fn add_size(s1: usize, s2: usize) -> PgResult<usize> {
    match s1.checked_add(s2) {
        Some(result) => Ok(result),
        None => Err(add_size_error(s1, s2)),
    }
}

#[cold]
fn add_size_error(s1: usize, s2: usize) -> alloc::boxed::Box<PgError> {
    PgError::error(alloc::format!("invalid memory allocation request size {s1} + {s2}"))
        .with_sqlstate(ERRCODE_PROGRAM_LIMIT_EXCEEDED)
        .into()
}

#[inline]
pub fn mul_size(s1: usize, s2: usize) -> PgResult<usize> {
    match s1.checked_mul(s2) {
        Some(result) => Ok(result),
        None => Err(mul_size_error(s1, s2)),
    }
}

#[cold]
fn mul_size_error(s1: usize, s2: usize) -> alloc::boxed::Box<PgError> {
    PgError::error(alloc::format!("invalid memory allocation request size {s1} * {s2}"))
        .with_sqlstate(ERRCODE_PROGRAM_LIMIT_EXCEEDED)
        .into()
}

#[inline]
pub fn alloc_in<'mcx, T>(mcx: Mcx<'mcx>, value: T) -> PgResult<PgBox<'mcx, T>> {
    check_alloc_size(core::mem::size_of::<T>())?;
    PgBox::try_new_in(value, mcx)
        .map_err(|_| mcx.oom_boxed(core::mem::size_of::<T>()))
}

pub fn alloc_leak_in<'mcx, T>(mcx: Mcx<'mcx>, value: T) -> PgResult<&'mcx T> {
    const { assert!(!core::mem::needs_drop::<T>()) };
    Ok(&*leak_in(alloc_in(mcx, value)?))
}

/// Drop never runs.
pub fn leak_in<'mcx, T>(b: PgBox<'mcx, T>) -> &'mcx mut T {
    const { assert!(!core::mem::needs_drop::<T>()) };
    PgBox::leak(b)
}

pub fn arena_leak<'mcx, T>(b: PgBox<'mcx, T>) -> &'mcx mut T {
    // Return a child retag (a sibling would be invalidated under stacked borrows).
    let (raw, alloc): (*mut T, Mcx<'mcx>) =
        allocator_api2::boxed::Box::into_raw_with_allocator(b);
    let addr = core::ptr::with_exposed_provenance_mut::<u8>(
        (raw as *mut u8).expose_provenance(),
    );
    // SAFETY: live T unboxed into `alloc`'s arena, Drop suppressed; glue is the unique destructor.
    let registered = unsafe { alloc.context().register_drop(addr, drop_glue::<T>) };
    debug_assert!(
        registered || !core::mem::needs_drop::<T>(),
        "arena_leak: value of a Drop type leaked into a non-BumpDrop context \
         (its destructor will never run); use a BumpDrop context",
    );
    // SAFETY: sole &'mcx mut (box consumed); child retag keeps the stored copy valid.
    unsafe { &mut *raw }
}

pub fn arena_box_in<'mcx, T>(mcx: Mcx<'mcx>, value: T) -> PgResult<&'mcx mut T> {
    check_alloc_size(core::mem::size_of::<T>())?;
    let b = PgBox::try_new_in(value, mcx)
        .map_err(|_| mcx.oom_boxed(core::mem::size_of::<T>()))?;
    Ok(arena_leak(b))
}

pub fn arena_vec_in<'mcx, T>(
    mcx: Mcx<'mcx>,
    vec: PgVec<'mcx, T>,
) -> PgResult<&'mcx mut PgVec<'mcx, T>> {
    check_alloc_size(core::mem::size_of::<PgVec<'mcx, T>>())?;
    let b = PgBox::try_new_in(vec, mcx)
        .map_err(|_| mcx.oom_boxed(core::mem::size_of::<PgVec<'mcx, T>>()))?;
    let (raw, alloc): (*mut PgVec<'mcx, T>, Mcx<'mcx>) =
        allocator_api2::boxed::Box::into_raw_with_allocator(b);
    let addr =
        core::ptr::with_exposed_provenance_mut::<u8>((raw as *mut u8).expose_provenance());
    // SAFETY: live header leaked into `alloc`'s arena; element-only glue is the unique destructor.
    let registered = unsafe { alloc.context().register_drop(addr, drop_glue_vec_elems::<T>) };
    debug_assert!(
        registered || !core::mem::needs_drop::<T>(),
        "arena_vec_in: Vec of a Drop element type leaked into a non-BumpDrop \
         context (element destructors will never run); use a BumpDrop context",
    );
    // SAFETY: sole &'mcx mut to the live header; child retag of `raw`.
    Ok(unsafe { &mut *raw })
}

pub fn arena_string_in<'mcx>(
    mcx: Mcx<'mcx>,
    s: PgString<'mcx>,
) -> PgResult<&'mcx mut PgString<'mcx>> {
    let b = alloc_in(mcx, s)?;
    let raw: *mut PgString<'mcx> =
        allocator_api2::boxed::Box::into_raw_with_allocator(b).0;
    let addr =
        core::ptr::with_exposed_provenance_mut::<u8>((raw as *mut u8).expose_provenance());
    // SAFETY: PgString's only Drop is its POD-element Vec<u8>'s.
    let registered = unsafe { mcx.context().register_drop(addr, drop_glue_noop) };
    debug_assert!(registered, "arena_string_in: use a BumpDrop context");
    // SAFETY: sole &'mcx mut to the live header; child retag of `raw`.
    Ok(unsafe { &mut *raw })
}

/// C `pfree(node)` of a node whose pointees stay in their context: the box's own chunk goes back to its context, and its
/// drop glue never runs (`ForgetSafe`: the skipped drops would give back only arena bytes the context reclaims at its
/// reset).
pub fn free_shell<'mcx, T: ForgetSafe>(b: PgBox<'mcx, T>) {
    let (raw, alloc) = allocator_api2::boxed::Box::into_raw_with_allocator(b);
    let layout = Layout::new::<T>();
    if layout.size() == 0 {
        return; // a zero-sized box holds no allocation
    }
    // SAFETY: `raw` is the box's live allocation, made by `alloc` with T's layout and given back once, here; leaving its
    // contents undropped is sound for a ForgetSafe T (their glue frees arena bytes only).
    unsafe { alloc.deallocate(NonNull::new_unchecked(raw.cast::<u8>()), layout) };
}

/// Objects die with the context without per-object drop.
pub fn forget_box_in<'mcx, T: ForgetSafe>(mcx: Mcx<'mcx>, value: T) -> PgResult<&'mcx mut T> {
    Ok(PgBox::leak(alloc_in(mcx, value)?))
}

pub fn arena_box_in_forget<'mcx, T: ArenaSafe>(
    mcx: Mcx<'mcx>,
    value: T,
) -> PgResult<&'mcx mut T> {
    const { assert!(!core::mem::needs_drop::<T>()) };
    debug_assert!(
        mcx.context().is_bumpforget(),
        "arena_box_in_forget: use a BumpForget context (forget-on-reset)",
    );
    let b = alloc_in(mcx, value)?;
    Ok(allocator_api2::boxed::Box::leak(b))
}

pub fn arena_vec_in_forget<'mcx, T: ArenaSafe>(
    mcx: Mcx<'mcx>,
    vec: PgVec<'mcx, T>,
) -> PgResult<&'mcx mut PgVec<'mcx, T>> {
    const { assert!(!core::mem::needs_drop::<T>()) };
    debug_assert!(
        mcx.context().is_bumpforget(),
        "arena_vec_in_forget: use a BumpForget context (forget-on-reset)",
    );
    let b = alloc_in(mcx, vec)?;
    Ok(allocator_api2::boxed::Box::leak(b))
}

/// Move out without deallocate: the captured context may already be reset.
pub fn box_into_inner_leak<'mcx, T>(b: PgBox<'mcx, T>) -> T {
    let (raw, _alloc) = allocator_api2::boxed::Box::into_raw_with_allocator(b);
    // SAFETY: `raw` read exactly once; `_alloc` is a Copy handle, never dereferenced.
    unsafe { core::ptr::read(raw) }
}

/// Sized -> unsized `PgBox` coercion; caller supplies the thin->fat cast.
///
/// # Safety
///
/// `coerce` MUST return the genuine unsizing coercion of the pointer it is
/// handed and nothing else — i.e. the same allocation, same address, only a
/// pointer-metadata (vtable / slice-length) attachment, exactly what
/// `|p| p as *mut dyn Trait` or `|p| p as *mut [T]` produce. The returned
/// pointer is fed to `Box::from_raw_in` with the allocator just decomposed, so
/// any other pointer (null, dangling, an interior offset, a foreign or
/// integer-cast address, or forged slice metadata) makes the reconstructed
/// `PgBox` own memory it does not back — a later use or drop then dereferences
/// and frees that bogus address through the context allocator (heap
/// corruption). This is why the function is `unsafe`: the signature alone
/// (`FnOnce(*mut P) -> *mut U`, with `U: ?Sized` admitting even `Sized` `U`)
/// cannot express or check the derived-pointer precondition, so the caller
/// must uphold it.
pub unsafe fn box_unsize_dyn<'mcx, P, U>(
    sized: PgBox<'mcx, P>,
    coerce: impl FnOnce(*mut P) -> *mut U,
) -> PgBox<'mcx, U>
where
    P: 'mcx,
    U: ?Sized + 'mcx,
{
    let (raw, alloc) = allocator_api2::boxed::Box::into_raw_with_allocator(sized);
    let fat: *mut U = coerce(raw);
    // SAFETY: by this fn's contract `coerce` returns the unsizing coercion of
    // `raw` (same allocation+address, metadata only), and `alloc` is the exact
    // allocator just decomposed from `sized`.
    unsafe { allocator_api2::boxed::Box::from_raw_in(fat, alloc) }
}

/// Move payload `P` out of an unsized `PgBox` without dropping `P`.
/// # Safety: `data` — the payload's data pointer inside `sized`; runtime type `P` (tag-checked).
pub unsafe fn box_read_payload<'mcx, P, U>(sized: PgBox<'mcx, U>, data: *const P) -> P
where
    P: 'mcx,
    U: ?Sized + 'mcx,
{
    let (raw, alloc) = allocator_api2::boxed::Box::into_raw_with_allocator(sized);
    let layout = core::alloc::Layout::for_value(unsafe { &*raw });
    let value = unsafe { core::ptr::read(data) };
    if layout.size() != 0 {
        let nn = core::ptr::NonNull::new(raw as *mut u8)
            .expect("box_read_payload: box raw pointer was null");
        unsafe { allocator_api2::alloc::Allocator::deallocate(&alloc, nn, layout) };
    }
    value
}

/// stable extend_from_slice is a per-element loop, ~10x a bare memcpy.
#[inline]
pub fn vec_append_bytes(v: &mut PgVec<'_, u8>, bytes: &[u8]) -> PgResult<()> {
    let n = bytes.len();
    if n == 0 {
        return Ok(());
    }
    let mcx = *v.allocator();
    let old = v.len();
    if v.capacity() - old < n {
        let target = grow_target(v.capacity(), old.saturating_add(n));
        check_alloc_size(target)?;
        v.try_reserve_exact(target - old).map_err(|_| mcx.oom_boxed(n))?;
    }
    // SAFETY: capacity >= old + n after the reserve; dst disjoint; set_len covers the n bytes.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), v.as_mut_ptr().add(old), n);
        v.set_len(old + n);
    }
    Ok(())
}

#[inline]
pub fn grow_target(capacity: usize, needed: usize) -> usize {
    let mut newlen = capacity.max(1).saturating_mul(2);
    while needed > newlen {
        newlen = newlen.saturating_mul(2);
    }
    if newlen > MAX_ALLOC_SIZE {
        newlen = needed.max(MAX_ALLOC_SIZE);
    }
    newlen
}

#[inline]
pub fn vec_new_in<'mcx, T>(mcx: Mcx<'mcx>) -> PgVec<'mcx, T> {
    const { assert!(!core::mem::needs_drop::<T>()) };
    PgVec::new_in(mcx)
}

#[inline]
pub fn vec_with_capacity_in<'mcx, T>(mcx: Mcx<'mcx>, cap: usize) -> PgResult<PgVec<'mcx, T>> {
    const { assert!(!core::mem::needs_drop::<T>()) };
    let request = cap.saturating_mul(core::mem::size_of::<T>());
    check_alloc_size(request)?;
    if cap == 0 || core::mem::size_of::<T>() == 0 {
        return Ok(PgVec::new_in(mcx));
    }
    // Alloc via non-generic alloc_uninit_bytes (no per-T grow machinery).
    let layout = core::alloc::Layout::array::<T>(cap).map_err(|_| mcx.oom_boxed(request))?;
    let p = mcx.alloc_uninit_bytes(layout).map_err(|_| mcx.oom_boxed(request))?;
    // SAFETY: `p` is a fresh arena allocation of exactly `Layout::array::<T>(cap)`
    // bytes from `mcx`; len 0 with capacity `cap` (the same layout Vec recomputes
    // on grow/drop), matching this allocator.
    Ok(unsafe { PgVec::from_raw_parts_in(p.cast::<T>().as_ptr(), 0, cap, mcx) })
}

#[inline]
pub fn vec_with_capacity_huge_in<'mcx, T>(
    mcx: Mcx<'mcx>,
    cap: usize,
) -> PgResult<PgVec<'mcx, T>> {
    const { assert!(!core::mem::needs_drop::<T>()) };
    let request = cap.saturating_mul(core::mem::size_of::<T>());
    if request > MAX_ALLOC_HUGE_SIZE {
        return Err(invalid_alloc_size(request));
    }
    if cap == 0 || core::mem::size_of::<T>() == 0 {
        return Ok(PgVec::new_in(mcx));
    }
    // Never funnels through the ceiling-checked Allocator::allocate.
    let layout = core::alloc::Layout::array::<T>(cap).map_err(|_| mcx.oom_boxed(request))?;
    let p = mcx.alloc_uninit_bytes_huge(layout).map_err(|_| mcx.oom_boxed(request))?;
    // SAFETY: `p` is a fresh allocation of exactly `Layout::array::<T>(cap)`
    // bytes from `mcx`; len 0 with capacity `cap` (the same layout Vec
    // recomputes on grow/drop), matching this allocator.
    Ok(unsafe { PgVec::from_raw_parts_in(p.cast::<T>().as_ptr(), 0, cap, mcx) })
}

pub fn vec_reserve_huge<'mcx, T>(v: &mut PgVec<'mcx, T>, additional: usize) -> PgResult<()> {
    const { assert!(!core::mem::needs_drop::<T>()) };
    use allocator_api2::alloc::Allocator;
    let len = v.len();
    let mcx = *v.allocator();
    let need = len
        .checked_add(additional)
        .ok_or_else(|| invalid_alloc_size(usize::MAX))?;
    if need <= v.capacity() {
        return Ok(());
    }
    let request = need.saturating_mul(core::mem::size_of::<T>());
    if request > MAX_ALLOC_HUGE_SIZE {
        return Err(invalid_alloc_size(request));
    }
    debug_assert!(core::mem::size_of::<T>() != 0, "huge reserve of a ZST cannot fail");
    let new_layout = core::alloc::Layout::array::<T>(need).map_err(|_| mcx.oom_boxed(request))?;
    let old_cap = v.capacity();
    if old_cap != 0 {
        // repalloc_huge reallocs (mremap when mapped): never both arrays resident.
        let old_layout =
            core::alloc::Layout::array::<T>(old_cap).expect("existing capacity has a valid layout");
        let old_ptr = v.as_mut_ptr() as *mut u8;
        // SAFETY: the buffer is live in `mcx` with `old_layout`; on success it moved to `p`.
        if let Ok(p) = unsafe {
            mcx.grow_unchecked(NonNull::new_unchecked(old_ptr), old_layout, new_layout)
        } {
            let old_vec = core::mem::replace(v, PgVec::new_in(mcx));
            let _ = split_vec_raw_parts(old_vec);
            // SAFETY: `p` holds `len` initialized elements and room for `need`.
            *v = unsafe { PgVec::from_raw_parts_in(p.cast::<T>().as_ptr(), len, need, mcx) };
            return Ok(());
        }
    }
    let p = mcx.alloc_uninit_bytes_huge(new_layout).map_err(|_| mcx.oom_boxed(request))?;
    let old_vec = core::mem::replace(v, PgVec::new_in(mcx));
    let (old_ptr, old_len, _old_cap, _mcx) = split_vec_raw_parts(old_vec);
    debug_assert_eq!(old_len, len);
    // SAFETY: `p` is a fresh `need`-element allocation (need > old_cap >= len),
    // disjoint from the old buffer; `len` initialized elements are copied and
    // the old buffer is returned to the same allocator with its true layout.
    unsafe {
        core::ptr::copy_nonoverlapping(old_ptr, p.cast::<T>().as_ptr(), len);
        if old_cap != 0 {
            let old_layout = core::alloc::Layout::array::<T>(old_cap)
                .expect("existing capacity has a valid layout");
            Allocator::deallocate(&mcx, NonNull::new_unchecked(old_ptr as *mut u8), old_layout);
        }
        *v = PgVec::from_raw_parts_in(p.cast::<T>().as_ptr(), len, need, mcx);
    }
    Ok(())
}

fn split_vec_raw_parts<'mcx, T>(v: PgVec<'mcx, T>) -> (*mut T, usize, usize, Mcx<'mcx>) {
    let mut v = core::mem::ManuallyDrop::new(v);
    (v.as_mut_ptr(), v.len(), v.capacity(), *v.allocator())
}

#[inline]
pub fn box_new_in<'mcx, T>(mcx: Mcx<'mcx>, value: T) -> PgBox<'mcx, T> {
    PgBox::new_in(value, mcx)
}

#[inline]
pub fn vec_with_capacity_in_infallible<'mcx, T>(mcx: Mcx<'mcx>, cap: usize) -> PgVec<'mcx, T> {
    const { assert!(!core::mem::needs_drop::<T>()) };
    PgVec::with_capacity_in(cap, mcx)
}

#[inline]
pub fn vec_from_elem_in<'mcx, T: Clone>(mcx: Mcx<'mcx>, value: T, n: usize) -> PgVec<'mcx, T> {
    const { assert!(!core::mem::needs_drop::<T>()) };
    let mut v = PgVec::with_capacity_in(n, mcx);
    v.resize(n, value);
    v
}

/// Context must be live (else box_into_inner_leak).
#[inline]
pub fn box_into_inner<'mcx, T>(b: PgBox<'mcx, T>) -> T {
    allocator_api2::boxed::Box::into_inner(b)
}

/// Copy elements lower to one memcpy.
#[inline]
pub fn slice_in<'mcx, T: Clone>(mcx: Mcx<'mcx>, src: &[T]) -> PgResult<PgVec<'mcx, T>> {
    const { assert!(!core::mem::needs_drop::<T>()) };
    use allocator_api2::alloc::Allocator;
    let len = src.len();
    let request = len.saturating_mul(core::mem::size_of::<T>());
    check_alloc_size(request)?;
    if len == 0 {
        return Ok(PgVec::new_in(mcx));
    }
    let layout = core::alloc::Layout::array::<T>(len).map_err(|_| mcx.oom_boxed(request))?;
    let ptr = Allocator::allocate(&mcx, layout).map_err(|_| mcx.oom_boxed(request))?;
    let dst = ptr.as_ptr() as *mut T;

    // Frees the buffer if a clone() panics; the prefix needs no drops.
    struct FillGuard<'m, T> {
        dst: *mut T,
        layout: core::alloc::Layout,
        mcx: Mcx<'m>,
    }
    impl<T> Drop for FillGuard<'_, T> {
        fn drop(&mut self) {
            use allocator_api2::alloc::Allocator;
            // SAFETY: `dst` is the live allocation from `mcx`, solely guard-owned.
            unsafe {
                Allocator::deallocate(
                    &self.mcx,
                    core::ptr::NonNull::new_unchecked(self.dst as *mut u8),
                    self.layout,
                );
            }
        }
    }

    let guard = FillGuard { dst, layout, mcx };
    // SAFETY: fresh allocation for `len` Ts; each slot written once; src distinct.
    for (i, elem) in src.iter().enumerate() {
        unsafe { guard.dst.add(i).write(elem.clone()) };
    }
    let dst = guard.dst;
    core::mem::forget(guard);
    // SAFETY: `len` initialized Ts, capacity exactly `len`.
    Ok(unsafe { PgVec::from_raw_parts_in(dst, len, len, mcx) })
}

pub fn slice_borrow_in<'mcx, T: Clone>(mcx: Mcx<'mcx>, src: &[T]) -> PgResult<&'mcx [T]> {
    const { assert!(!core::mem::needs_drop::<T>()) };
    let v: PgVec<'mcx, T> = slice_in(mcx, src)?;
    vec_borrow_in(mcx, v)
}

pub fn vec_borrow_in<'mcx, T>(_mcx: Mcx<'mcx>, v: PgVec<'mcx, T>) -> PgResult<&'mcx [T]> {
    const { assert!(!core::mem::needs_drop::<T>()) };
    let boxed: allocator_api2::boxed::Box<[T], Mcx<'mcx>> = v.into_boxed_slice();
    Ok(allocator_api2::boxed::Box::leak(boxed))
}

/// D4561 step 2: a PgVec of exactly `n` slots, for an array `freeze` turns into a context slice. The slots past its `len`
/// stay allocated for the context's life, so the length is reserved exactly where it is known (memlead 2026-10-15).
#[inline]
pub fn pgvec_exact_in<'mcx, T>(mcx: Mcx<'mcx>, n: usize) -> PgResult<PgVec<'mcx, T>> {
    let mut v = PgVec::new_in(mcx);
    v.try_reserve_exact(n).map_err(|_| mcx.oom_boxed(n.saturating_mul(core::mem::size_of::<T>())))?;
    Ok(v)
}

/// D4561 step 2: a query-lifetime array frozen into its context, as C's palloc'd array: no Drop and no free, the buffer
/// goes with the context's reset or delete (the planner's contexts are wholesale or deleted; memlead 2026-10-15 Q1: no
/// ledger change). Unlike `vec_borrow_in` it does not shrink-realloc, so build it with `pgvec_exact_in`. `T: Copy` has
/// no Drop, so only bytes are left behind.
#[inline]
pub fn freeze<'mcx, T: Copy>(v: PgVec<'mcx, T>) -> &'mcx [T] {
    debug_assert_eq!(v.len(), v.capacity(), "mcx::freeze: build the array with pgvec_exact_in at its final length");
    v.leak()
}

/// D4561 step 2: `freeze` for an array whose length is known only once it is built (pushed under a filter). Its spare
/// capacity goes back to the context first (a shrink in place where the chunk allows it, else a move), so no cap - len
/// slack stays for the context's life. A failed shrink keeps the slack rather than failing: the array is complete.
#[inline]
pub fn freeze_fit<'mcx, T: Copy>(v: PgVec<'mcx, T>) -> &'mcx [T] {
    if v.len() == v.capacity() || core::mem::size_of::<T>() == 0 {
        return v.leak();
    }
    freeze_fit_shrink(v)
}

#[cold]
#[inline(never)]
fn freeze_fit_shrink<'mcx, T: Copy>(v: PgVec<'mcx, T>) -> &'mcx [T] {
    let (ptr, len, cap, mcx) = v.into_raw_parts_with_alloc();
    let (size, align) = (core::mem::size_of::<T>(), core::mem::align_of::<T>());
    // SAFETY: (ptr, cap) is the PgVec's live allocation from `mcx` with T's array layout, and len < cap; T: Copy needs no
    // drop. The slice borrows the context for 'mcx, which outlives it (no free: the context's reset or delete takes it).
    unsafe {
        let base = NonNull::new_unchecked(ptr.cast::<u8>());
        let old = Layout::from_size_align_unchecked(cap * size, align);
        if len == 0 {
            mcx.deallocate(base, old);
            return &[];
        }
        let new = Layout::from_size_align_unchecked(len * size, align);
        let p = match mcx.shrink(base, old, new) {
            Ok(p) => p.cast::<T>().as_ptr(),
            Err(_) => ptr,
        };
        core::slice::from_raw_parts(p, len)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) mod churn_probe {
    use core::sync::atomic::{AtomicU64, Ordering};
    pub(crate) static REAL_FREES: AtomicU64 = AtomicU64::new(0);
    pub(crate) fn bump() {
        REAL_FREES.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn take() -> u64 {
        REAL_FREES.swap(0, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod bump_policy_wrapper_tests {
    use super::*;

    #[test]
    fn capped_wrapper_grow_reset_and_shrink() {
        let parent = MemoryContext::new_bump("parent");
        let mut child = parent.new_child_bump_with_max_block_size("capped", 8192);
        for _ in 0..2 {
            {
                let mcx = child.mcx();
                let small = Layout::from_size_align(32, 8).unwrap();
                let large = Layout::from_size_align(1024, 8).unwrap();
                let aligned = Layout::from_size_align(16, 256).unwrap();
                let p = Allocator::allocate(&mcx, small).unwrap().cast::<u8>();
                // SAFETY: p owns 32 writable bytes; grow/shrink receive the live allocation's layout.
                unsafe {
                    p.as_ptr().write_bytes(0xa7, 32);
                    let q = Allocator::grow(&mcx, p, small, large).unwrap().cast::<u8>();
                    assert_eq!(core::slice::from_raw_parts(q.as_ptr(), 32), &[0xa7; 32]);
                    let r = Allocator::shrink(&mcx, q, large, aligned).unwrap().cast::<u8>();
                    assert_eq!(r.as_ptr() as usize % 256, 0);
                    assert_eq!(core::slice::from_raw_parts(r.as_ptr(), 16), &[0xa7; 16]);
                }
            }
            child.reset();
        }
    }
}

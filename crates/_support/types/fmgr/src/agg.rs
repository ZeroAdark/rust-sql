use core::cell::{Cell, UnsafeCell};
use core::ptr::NonNull;

use ::mcx::{Mcx, MemoryContext};

use crate::fcinfo::{FmNode, FmNodePtr, FunctionCallInfoBaseData};

// nodetags.h value, parity-asserted in fmgr_core tests.
pub const T_AGG_STATE: u32 = 429;

/// The trans/final-fn-visible slice of C's `AggState`: rides `fcinfo->context`
/// (C `AggCheckCallContext`) and owns the aggcontext arena. Wholesale reset:
/// allocating transfns assert `!needs_drop` for their state (docs/no-drop.md).
/// `cur_agg` is C's curperagg/curpertrans pair erased to (Aggref ptr,
/// aggshared); typed access lives in nodeagg (AggGetAggref/AggStateIsShared).
/// `callbacks` is C's curaggcontext ExprContext_CB chain (AggRegisterCallback),
/// fired LIFO at group reset and at node teardown. `sets` are C's
/// aggcontexts[] of sorted grouping sets; `cur_set` is select_current_set's
/// choice (NO_SET = the base context: hashed sets and single-set nodes).
#[repr(C)]
pub struct AggStateNode {
    node: FmNode,
    aggcontext: MemoryContext,
    cur_agg: Cell<Option<(NonNull<()>, bool)>>,
    // The owner keeps a transition function's read-write expanded result made in the aggcontext as the new
    // transition value (C ExecAggCopyTransValue's "do nothing" arm), so array_append may grow the state in place.
    // Set by nodeagg; other owners (WindowAgg, Gravity hosts) copy every new by-ref value and leave it false.
    expanded_trans: Cell<bool>,
    callbacks: UnsafeCell<Callbacks>,
    sets: UnsafeCell<::alloc::vec::Vec<SetContext>>,
    cur_set: Cell<u32>,
}

type Callbacks = ::alloc::vec::Vec<(unsafe fn(*mut ()), *mut ())>;

struct SetContext {
    ctx: MemoryContext,
    callbacks: Callbacks,
}

const NO_SET: u32 = u32::MAX;

fn run_callbacks(cbs: &mut Callbacks) {
    let cbs = core::mem::take(cbs);
    for (func, arg) in cbs.into_iter().rev() {
        // SAFETY: registration contract (register_shutdown_callback).
        unsafe { func(arg) };
    }
}

impl AggStateNode {
    pub fn new(aggcontext: MemoryContext) -> Self {
        Self {
            node: FmNode { tag: T_AGG_STATE },
            aggcontext,
            cur_agg: Cell::new(None),
            expanded_trans: Cell::new(false),
            callbacks: UnsafeCell::new(::alloc::vec::Vec::new()),
            sets: UnsafeCell::new(::alloc::vec::Vec::new()),
            cur_set: Cell::new(NO_SET),
        }
    }

    /// One aggcontext per sorted grouping set (ExecInitAgg's aggcontexts[]), made once at init. The contexts sit inline
    /// in the vector, so a later growth would move every one of them out from under the handles a transition function
    /// keeps (`agg_context`, `aggcontext_ctx`): only an empty node, before any set is selected, takes them, and the
    /// exact reservation keeps this one fill from moving the ones it already pushed.
    pub fn add_set_contexts(&mut self, n: usize, mut make: impl FnMut() -> MemoryContext) {
        let sets = self.sets.get_mut();
        debug_assert!(sets.is_empty(), "add_set_contexts runs once, at init: a second fill would move the set contexts");
        debug_assert_eq!(self.cur_set.get(), NO_SET, "add_set_contexts runs before any set is selected");
        sets.reserve_exact(n);
        for _ in 0..n {
            sets.push(SetContext { ctx: make(), callbacks: ::alloc::vec::Vec::new() });
        }
    }

    pub fn num_set_contexts(&self) -> usize {
        // SAFETY: single-threaded backend; the vector is only resized by
        // add_set_contexts, which holds &mut self.
        unsafe { (&*self.sets.get()).len() }
    }

    /// C select_current_set for a sorted set; `None` selects the base context.
    pub fn select_set(&self, setno: Option<usize>) {
        let v = match setno {
            Some(n) if n < self.num_set_contexts() => n as u32,
            _ => NO_SET,
        };
        self.cur_set.set(v);
    }

    /// C ReScanExprContext(aggcontexts[setno]): shutdown callbacks, then reset.
    pub fn reset_set(&self, setno: usize) {
        // SAFETY: single-threaded backend; callbacks may re-enter
        // register_shutdown_callback, which pushes onto a fresh vector.
        let sets = unsafe { &mut *self.sets.get() };
        if setno >= sets.len() {
            return;
        }
        let mut cbs = core::mem::take(&mut sets[setno].callbacks);
        run_callbacks(&mut cbs);
        // SAFETY: as above; no Mcx borrow of this context outlives a group.
        let sets = unsafe { &mut *self.sets.get() };
        sets[setno].ctx.reset();
    }

    // Shared provenance survives writes through the node's interior-mutable slots.
    pub fn fm_node_ptr(&self) -> FmNodePtr {
        Some(NonNull::from(self).cast::<FmNode>())
    }

    pub fn aggcontext(&self) -> Mcx<'_> {
        let cur = self.cur_set.get();
        if cur != NO_SET {
            // SAFETY: single-threaded backend; cur_set < len (select_set).
            return unsafe { (&*self.sets.get())[cur as usize].ctx.mcx() };
        }
        self.aggcontext.mcx()
    }

    /// The aggcontext itself (C `aggcontext` as a MemoryContext: a parent for expanded objects).
    pub fn aggcontext_ctx(&self) -> &MemoryContext {
        let cur = self.cur_set.get();
        if cur != NO_SET {
            // SAFETY: single-threaded backend; cur_set < len (select_set).
            return unsafe { &(&*self.sets.get())[cur as usize].ctx };
        }
        &self.aggcontext
    }

    pub fn set_expanded_trans(&self, on: bool) {
        self.expanded_trans.set(on);
    }

    pub fn expanded_trans(&self) -> bool {
        self.expanded_trans.get()
    }

    pub fn set_current_agg(&self, aggref: NonNull<()>, shared: bool) {
        self.cur_agg.set(Some((aggref, shared)));
    }

    pub fn clear_current_agg(&self) {
        self.cur_agg.set(None);
    }

    pub fn current_agg(&self) -> Option<(NonNull<()>, bool)> {
        self.cur_agg.get()
    }

    /// C `RegisterExprContextCallback` on the curaggcontext (nodeAgg.c
    /// AggRegisterCallback): fired LIFO before the group memory goes away.
    ///
    /// # Safety
    /// `arg` must stay valid until the callback fires (group reset or node
    /// teardown), and `func(arg)` must be safe to call exactly once.
    pub unsafe fn register_shutdown_callback(&self, func: unsafe fn(*mut ()), arg: *mut ()) {
        let cur = self.cur_set.get();
        // SAFETY: single-threaded backend; no other borrow escapes this module.
        unsafe {
            if cur != NO_SET {
                (&mut *self.sets.get())[cur as usize].callbacks.push((func, arg));
            } else {
                (*self.callbacks.get()).push((func, arg));
            }
        }
    }

    fn run_shutdown_callbacks(&self) {
        for setno in 0..self.num_set_contexts() {
            // SAFETY: single-threaded backend; callbacks may re-enter register
            // only after the take.
            let mut cbs = core::mem::take(unsafe { &mut (&mut *self.sets.get())[setno].callbacks });
            run_callbacks(&mut cbs);
        }
        // SAFETY: as above.
        let mut cbs = core::mem::take(unsafe { &mut *self.callbacks.get() });
        run_callbacks(&mut cbs);
    }

    pub fn reset(&mut self) {
        self.run_shutdown_callbacks();
        self.cur_set.set(NO_SET);
        for set in self.sets.get_mut().iter_mut() {
            set.ctx.reset();
        }
        self.aggcontext.reset();
    }
}

// Guard-owner Drop (docs/no-drop.md): nodeagg's make_agg_state_node registers
// the drop_in_place on the query context, mirroring C's ExecutorEnd shutdown
// of the aggcontext ExprContexts.
impl Drop for AggStateNode {
    fn drop(&mut self) {
        self.run_shutdown_callbacks();
    }
}

impl FunctionCallInfoBaseData {
    /// C `AggCheckCallContext`: `Some(aggcontext)` iff called as an aggregate
    /// trans/final fn (the WindowAgg arm is unarmed — loud at the caller).
    ///
    /// # Safety
    /// `context`, if set, points at a live FmNode-led node outliving `'a`,
    /// with no `&mut` formed to it during the call.
    #[inline]
    pub unsafe fn agg_context<'a>(&self) -> Option<Mcx<'a>> {
        // SAFETY: caller contract; the tag check proves the concrete type.
        unsafe { self.agg_state_node().map(|n| n.aggcontext()) }
    }

    /// The `AggStateNode` behind `fcinfo->context`, if any (C
    /// `IsA(fcinfo->context, AggState)` demux).
    ///
    /// # Safety
    /// As [`Self::agg_context`].
    #[inline]
    pub unsafe fn agg_state_node<'a>(&self) -> Option<&'a AggStateNode> {
        let p = self.context?;
        // SAFETY: caller contract; the tag check proves the concrete type.
        unsafe {
            if p.as_ref().tag != T_AGG_STATE {
                return None;
            }
            Some(p.cast::<AggStateNode>().as_ref())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carrier_survives_current_aggregate_changes() {
        #[allow(unused_mut)]
        let mut node = AggStateNode::new(MemoryContext::new_bump("carrier-test"));
        let carrier = node.fm_node_ptr().unwrap().cast::<AggStateNode>();
        let aggref = NonNull::dangling();
        node.set_current_agg(aggref, true);
        // SAFETY: node stays at its address and only shared accesses follow pointer creation.
        let observed = unsafe { carrier.as_ref() };
        assert_eq!(observed.current_agg(), Some((aggref, true)));
        node.clear_current_agg();
        assert_eq!(observed.current_agg(), None);
    }

    #[test]
    fn allocation_carrier_survives_partition_reset() {
        let raw = ::alloc::boxed::Box::into_raw(::alloc::boxed::Box::new(
            AggStateNode::new(MemoryContext::new_bump("reset-carrier-test")),
        ));
        let mut owner = NonNull::new(raw).unwrap();
        let carrier = owner.cast::<FmNode>();
        // SAFETY: owner is the live allocation pointer; reset's exclusive borrow ends before reads.
        unsafe {
            owner.as_ref().set_current_agg(NonNull::dangling(), true);
            owner.as_mut().reset();
            owner.as_ref().clear_current_agg();
            assert_eq!(carrier.cast::<AggStateNode>().as_ref().current_agg(), None);
            drop(::alloc::boxed::Box::from_raw(raw));
        }
    }

    static FIRED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

    unsafe fn bump(arg: *mut ()) {
        FIRED.fetch_add(arg as usize, core::sync::atomic::Ordering::Relaxed);
    }

    /// The set contexts sit inline in their vector: a second fill would move them under live handles, so debug builds
    /// refuse it.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "add_set_contexts runs once, at init")]
    fn a_second_set_context_fill_is_refused() {
        let parent = MemoryContext::new("query");
        let mut node = AggStateNode::new(MemoryContext::new_bump("base"));
        node.add_set_contexts(1, || parent.new_child_wholesale("set"));
        node.add_set_contexts(1, || parent.new_child_wholesale("set"));
    }

    #[test]
    fn grouping_set_contexts_reset_independently() {
        use core::sync::atomic::Ordering::Relaxed;
        let parent = MemoryContext::new("query");
        let mut node = AggStateNode::new(MemoryContext::new_bump("base"));
        node.add_set_contexts(2, || parent.new_child_wholesale("set"));
        let fc_base = node.aggcontext().context() as *const MemoryContext;
        node.select_set(Some(1));
        let fc_set1 = node.aggcontext().context() as *const MemoryContext;
        assert_ne!(fc_base, fc_set1, "selected set must own the aggcontext");
        let layout = core::alloc::Layout::from_size_align(4096, 8).unwrap();
        let _ = ::mcx::Allocator::allocate(&node.aggcontext(), layout).unwrap();
        assert!(node.aggcontext().context().used() > 0);
        // SAFETY: bump ignores its argument beyond the counter.
        unsafe { node.register_shutdown_callback(bump, 100 as *mut ()) };
        node.select_set(Some(0));
        // SAFETY: as above.
        unsafe { node.register_shutdown_callback(bump, 10 as *mut ()) };
        node.select_set(None);
        // SAFETY: as above.
        unsafe { node.register_shutdown_callback(bump, 1 as *mut ()) };
        assert_eq!(node.aggcontext().context() as *const MemoryContext, fc_base);

        FIRED.store(0, Relaxed);
        node.reset_set(1);
        assert_eq!(FIRED.load(Relaxed), 100, "only set 1's callback fires");
        node.select_set(Some(1));
        assert_eq!(node.aggcontext().context().used(), 0);
        node.select_set(None);
        node.reset();
        assert_eq!(FIRED.load(Relaxed), 111);
    }
}

//! The admission module's unsafe steps (rule R6, the module's raw home): the program tier's `Send + Sync` claim for a
//! shareable template, the regen check's scratch context, and a thread-local's value by reference past its `with`
//! (reentry's levels). Each keeps its contract here.
#![allow(unsafe_code)]

use gravity_driver::mem::root::Root;
use mcx::Mcx;

use super::SharedTpl;

// SAFETY: `shareable` files only templates with no InitPlan and no arm program, so no `RecipeTpl` is reachable from it;
// every other field is `Send + Sync` (`_TPL_PARTS`) and never mutated after `install` built it. planprod P6-b: the lists
// (`words`, `funcs`, `inits`, `arms`, `merge`, `checks`, `guards`: `TplVec`, whose `RootA` is neither) are blocks of the
// template's own root (`GraphTpl::root`), read in place and never grown after the build (`tpl_list`); the root's owner
// moves only under its lock (`GraphTpl::adopt`, `publish`); the lists free at the template's drop, which the last `Arc`
// runs on one thread, before the root (field order).
unsafe impl Send for SharedTpl {}
unsafe impl Sync for SharedTpl {}

/// planprod P6-b: a new list of a template (`GraphTpl`; P6-c1: a `RecipeSet`) in its root `root`, which `install` (or
/// `registry`) builds the template beside.
pub(super) fn tpl_list<T>(root: &mut mcx::SendRoot) -> super::TplVec<T> {
    // SAFETY: `SendRoot::handle`'s contract: the list grows only on the building thread, before the template is shared; it
    // is then held by the template beside the root (`GraphTpl::root`, `RecipeSet::root`: the last field, so the lists drop
    // first) or, on an early return, dropped before the root (`install` and `registry` declare the root before every list);
    // the template moves and is shared only whole (`Arc<GraphTpl>`, `SharedTpl`; `Arc<RecipeSet>`, never off its backend)
    allocator_api2::vec::Vec::new_in(unsafe { root.handle() })
}

/// memsig E-prog (C): the handle a recipe set's rungs build their kept lists with (`AdmitCx::lists`), on the set's root.
pub(super) fn tpl_lists(root: &mut mcx::SendRoot) -> super::TplLists {
    // SAFETY: `SendRoot::handle`'s contract, as `tpl_list`'s: `registry` declares the root before the handle and every
    // list made with it, so an early return drops them first; a list made with it is a field of a rung's template, held
    // only by the set's list beside the root (`TplLists`: never cloned, the set moves and is shared only whole) and grown
    // only at its admission, on the building thread, before the set is shared
    super::TplLists(unsafe { root.handle() })
}

impl super::TplLists {
    /// A new list in the set's root, for a rung's template (`AdmitCx::lists`).
    pub(crate) fn list<T>(&self) -> super::TplVec<T> {
        // SAFETY: `RootA::again`'s contract: the new handle is held beside the same root (a list of a template its set
        // holds), by the same holder (`tpl_lists`)
        allocator_api2::vec::Vec::new_in(unsafe { self.0.again() })
    }
}

/// D3620 (reentry `enter`): a thread-local's value by reference past its `with`, so the caller's run is not inside `with`'s
/// closure (whose frame would sit on every nested statement's stack path). Only for a value with no drop glue: such a value
/// is never destroyed while its thread lives (its storage is the thread's own). The reference is `!Send` (`ThreadRef`), so
/// it never leaves the thread.
pub(crate) fn thread_ref<T: 'static>(key: &'static std::thread::LocalKey<T>) -> ThreadRef<T> {
    const { assert!(!core::mem::needs_drop::<T>(), "thread_ref: a value with drop glue") };
    // SAFETY: as above: no destructor ever runs on the value, its storage lives as long as this thread, and the reference
    // stays on it
    ThreadRef(key.with(|v| unsafe { &*(v as *const T) }), core::marker::PhantomData)
}

/// `thread_ref`'s reference, pinned to its thread (`!Send`, `!Sync`).
pub(crate) struct ThreadRef<T: 'static>(&'static T, core::marker::PhantomData<*const ()>);

impl<T: 'static> core::ops::Deref for ThreadRef<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.0
    }
}

/// `regen::stale`'s scratch context, `ctx` the root it made for the one check.
pub(super) fn regen_mcx(ctx: &Root) -> Mcx<'static> {
    // SAFETY: `ctx` outlives the check and nothing opened in it escapes (the relation is closed, the part list is an `Arc`).
    unsafe { ctx.held() }
}

/// varatt.h VARTAG_ONDISK: an external datum's tag for a pointer into a toast relation.
const VARTAG_ONDISK: u8 = 18;

/// D3576 (`extern_word`): the image of the varlena at `ptr` when it is an on-disk toast pointer (`VARATT_IS_EXTERNAL_ONDISK`),
/// else none. `ptr` is a non-NULL by-reference varlena datum (the caller read its type's typlen -1), alive for the binding.
pub(super) fn ondisk_image(ptr: usize) -> Option<&'static [u8]> {
    let p = ptr as *const u8;
    // SAFETY: a live varlena datum: its header byte, then (a 1-byte external header) its tag byte
    let ondisk = unsafe { types_tuple::varatt::varatt_is_1b_e(p) && types_tuple::varatt::vartag_external(p) == VARTAG_ONDISK };
    // SAFETY: an external datum's whole image is `varsize_any` bytes, alive while the parameter list is
    ondisk.then(|| unsafe { core::slice::from_raw_parts(p, types_tuple::varatt::varsize_any(p)) })
}

/// D4786 (`list_salted`): the bytes of the class-keyed constant list at `image` (gravity_lower `class_keyed_lists`: a
/// non-null by-reference plan Const's in-line array image, its count read through the same header by `list_class`),
/// alive with the plan the admission reads.
pub(super) fn list_image(image: usize) -> &'static [u8] {
    let p = image as *const u8;
    // SAFETY: a live in-line varlena (an external one is never class-keyed: `konst::list_len` refuses it): `varsize_any`
    // bytes from its header
    unsafe { core::slice::from_raw_parts(p, types_tuple::varatt::varsize_any(p)) }
}

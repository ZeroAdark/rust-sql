//! The scan family's raw half in exec (lint R6; construct bundle scan-05): the `unsafe impl`s its admit templates need,
//! each with its safety contract, moved as is from ops/scan/param_sample.rs, ops/scan/sites_heap.rs and ops/scan/param_key.rs (and ops/scan/param_tid.rs's, memsig).
#![allow(unsafe_code)]
use super::param_sample::Tpl;
use super::sites_heap::Local;

// SAFETY: as sites_heap.rs `Local` (below): a sample template lives in its session thread's backend cache and is bound on
// that thread; its parameter list is a block of its recipe set's root (memsig E-prog (C), as `param_key::Tpl`'s).
unsafe impl Send for Tpl {}
unsafe impl Sync for Tpl {}

// SAFETY: a recipe template lives in its session thread's backend cache (`Backend::graph_registry`, as the `Index` /
// `Range` templates beside it, which are not `Send` either) and is bound on that thread; it never crosses threads.
unsafe impl<T> Send for Local<T> {}
// SAFETY: as `Send` above: only the owning session thread reads it.
unsafe impl<T> Sync for Local<T> {}

// SAFETY: as ops/scan/sites_heap.rs `Local`: a template lives in its session thread's backend cache and is bound on that thread.
// memsig E-prog (C): its lists (`TplVec`, whose `RootA` is neither) are blocks of its recipe set's root (admit `TplLists`),
// grown only at its admission on that thread and read in place after; they drop with the set's list, before the root.
unsafe impl Send for super::param_key::Tpl {}
unsafe impl Sync for super::param_key::Tpl {}

// SAFETY: as `param_key::Tpl` above (memsig E-prog (C): its lists in its recipe set's root).
unsafe impl Send for super::param_tid::Tpl {}
unsafe impl Sync for super::param_tid::Tpl {}

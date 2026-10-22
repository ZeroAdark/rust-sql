//! The served entry's unsafe steps (rule R6, the module's raw home): the desc table's one-at-a-time borrow, the plan,
//! text and snapshot the seams' retention contract keeps alive, the session's leaked descs root, a Serve's run-long
//! borrows of itself and of what it owns, and the reads of a node's context block. Each keeps its contract here.
#![allow(unsafe_code)]

use std::rc::Rc;

use gravity_driver::mem::root::Root;
use mcx::Mcx;
use types_nodes::list::NodeList;
use types_nodes::Node;
use types_portal::QueryDescHandle;
use types_snapshot::SnapshotData;
use types_tuple::ItemPointerData;

use super::run::Serve;
use super::{decode, PlannedStmt, QueryDesc, DESCS};

/// The desc behind a handle of ours; the table borrow ends before `f` runs (a receiver or a key
/// expression may re-enter the seams for another statement), the in-use flag refuses re-entry on this one. D3620: inlined
/// over an out-of-line lookup (`in_use`), so a served statement's stack path (every level of a nested one) holds no frame
/// of its own for it.
#[inline(always)]
pub(super) fn with_desc<R>(h: QueryDescHandle, f: impl FnOnce(&mut QueryDesc) -> R) -> R {
    struct Unuse(*mut QueryDesc);
    impl Drop for Unuse {
        fn drop(&mut self) {
            // SAFETY: the box stays in the table (in_use) until this guard drops.
            unsafe { (*self.0).in_use.set(false) }
        }
    }
    let guard = Unuse(in_use(h));
    // SAFETY: the boxed desc is address-stable and in_use excludes a second `&mut`.
    f(unsafe { &mut *guard.0 })
}

/// `with_desc`'s lookup: the boxed desc behind `h`, marked in use (a stale handle or a re-entry panics).
#[inline(never)]
fn in_use(h: QueryDescHandle) -> *mut QueryDesc {
    let (idx, generation) = decode(h);
    DESCS.with(|d| {
        let mut d = d.borrow_mut();
        match d.slots.get_mut(idx as usize) {
            Some(Some(qd)) if qd.generation == generation => {
                assert!(!qd.in_use.replace(true), "gravity_entry: re-entered QueryDescHandle {h:?}");
                &mut **qd as *mut QueryDesc
            }
            _ => panic!("gravity_entry: stale QueryDescHandle {h:?}"),
        }
    })
}

/// The desc's plan (`QueryDesc::plannedstmt`).
pub(super) fn plan(p: *const PlannedStmt<'static>) -> &'static PlannedStmt<'static> {
    // SAFETY: create_query_desc's retention contract (the caller keeps the plan until free).
    unsafe { &*p }
}

/// D3198: the desc's source text (`QueryDesc::source_text`: the executor hooks' queryDesc->sourceText).
pub(super) fn text(src: (*const u8, usize)) -> &'static str {
    // SAFETY: the same retention contract; the bytes came from a `&str`.
    unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(src.0, src.1)) }
}

/// The seam's plan, kept by the desc (`create_query_desc`).
pub(super) fn retained(plannedstmt: &PlannedStmt<'_>) -> &'static PlannedStmt<'static> {
    // SAFETY: the retention contract of the seam (the caller keeps the plan and the text past free).
    unsafe { core::mem::transmute::<&PlannedStmt<'_>, &'static PlannedStmt<'static>>(plannedstmt) }
}

/// D2710: the context of the identity copy's root, which the desc keeps (`QueryDesc::ident`).
pub(super) fn identity_mcx(ctx: &Root) -> Mcx<'static> {
    // SAFETY: the root lives in the desc until free, past every use of the copy.
    unsafe { ctx.held() }
}

/// D1506: the session's descs context (`run::desc_mcx`), `m` the leaked root DESC_CTX hands out.
pub(super) fn descs_mcx(m: *mut Root) -> Mcx<'static> {
    // SAFETY: a leaked Box, freed only by `retire_descs` (and only when nothing is left in it), which first makes every
    // later call an error
    unsafe { (*m).held() }
}

/// D1506 (`run::retire_descs`): the leaked descs root `m` freed, when nothing is left in it.
pub(super) fn free_descs(m: *mut Root) {
    // SAFETY: `m` came from Box::into_raw in `desc_mcx`, and DESC_CTX no longer hands it out
    if !m.is_null() && unsafe { (*m).used() } == 0 {
        drop(unsafe { Box::from_raw(m) });
    }
}

/// A Serve's own context (`Serve::mcx`).
pub(super) fn serve_mcx(ctx: &Root) -> Mcx<'static> {
    // SAFETY: as in `Serve::new`: the context lives in the boxed Serve, past every use.
    unsafe { ctx.held() }
}

/// The Serve borrowed for its adapter's run (`Serve::open`, `open_next_arm`).
pub(super) fn serve_for_run(s: &mut Serve) -> &'static mut Serve {
    // SAFETY: the Serve is boxed in the QueryDesc and outlives the adapter's run; the arrays and the snapshot do not move
    // while it is open (`close` ends every borrow).
    unsafe { &mut *(s as *mut Serve) }
}

/// The desc's snapshot for the run (`executor_run`'s `snap_ptr`, `Serve::snap`, `Serve::open`'s argument).
pub(super) fn snapshot(p: *const Rc<SnapshotData<'static>>) -> &'static Rc<SnapshotData<'static>> {
    // SAFETY: the desc is boxed and keeps its snapshot until free; the binding borrows it only while the adapter is open.
    unsafe { &*p }
}

/// The producers' resolved set for the binding (`Serve::open`).
pub(super) fn resolved(r: &(dyn core::any::Any + Send + Sync)) -> &'static (dyn core::any::Any + Send + Sync) {
    // SAFETY: the producers live in the boxed Serve until `close_run`, past every borrow of the binding (as the snapshot).
    unsafe { &*(r as *const (dyn core::any::Any + Send + Sync)) }
}

/// D274 / D280: the words the producers' plan or the serial protocols bound (`Serve::open`).
pub(super) fn bound_words(w: &[u64], n: &[u8]) -> (&'static [u64], &'static [u8]) {
    // SAFETY: as `resolved` (the serial words live in the boxed Serve until `close_run` too).
    unsafe { (&*(w as *const [u64]), &*(n as *const [u8])) }
}

/// Site `i`'s block of a node's context (`Serve::snapshot_sites`).
pub(super) fn site<'a>(ctx: *mut gravity_abi::Ctx, i: usize) -> &'a gravity_abi::Site {
    // SAFETY: the node's ctx has `sites.len()` site blocks.
    unsafe { &*gravity_abi::ctx::site_ptr(ctx, i) }
}

/// D682: a node's resume word (`run::resumes`).
pub(super) fn resume(ctx: *const gravity_abi::Ctx) -> i64 {
    // SAFETY: the node's context block lives as long as the node.
    unsafe { (*ctx).resume }
}

/// D2710: `n`'s target list replaced (`current_of::identity_plan`).
pub(super) fn set_targetlist<'p>(n: Node<'p>, tl: NodeList<'p>) {
    // SAFETY: `n` is a node of this statement's private copy, referenced by nothing else while it is rewritten.
    unsafe { n.with_plan_mut(|p| p.targetlist = tl) };
}

/// A served tid column's datum read as its ItemPointerData (`current_of::pack`).
pub(super) fn tid<'a>(ctid: u64) -> &'a ItemPointerData {
    // SAFETY: a served tid column's datum points at its 6-byte ItemPointerData for the row's push.
    unsafe { &*(ctid as usize as *const ItemPointerData) }
}

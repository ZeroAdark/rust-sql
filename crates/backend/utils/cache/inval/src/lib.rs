#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]

#[cfg(feature = "bench-internals")]
pub mod bench_internals;
pub mod eoxact;
pub mod invalidate;
pub mod local;
mod msgs;
mod registration;
#[cfg(test)]
mod tests;

use std::cell::{Cell, RefCell};
use std::mem::ManuallyDrop;

use datum::Datum;
use mcx::{bind, Mcx, McxOwned, MemoryContext, PgVec};
use types_core::Oid;
use types_storage::SharedInvalidationMessage;

pub use msgs::InvalidationMsgsGroup;

pub type SyscacheCallbackFunction = fn(arg: Datum, cacheid: i32, hashvalue: u32);
pub type RelcacheCallbackFunction = fn(arg: Datum, relid: Oid);
pub type RelSyncCallbackFunction = fn(arg: Datum, relid: Oid);

pub(crate) const CAT_CACHE_MSGS: usize = 0;
pub(crate) const REL_CACHE_MSGS: usize = 1;

pub(crate) const MAX_SYSCACHE_CALLBACKS: usize = 64;
pub(crate) const MAX_RELCACHE_CALLBACKS: usize = 10;
pub(crate) const MAX_RELSYNC_CALLBACKS: usize = 10;
// syscache_ids.h: SysCacheSize == USERMAPPINGUSERSERVER + 1.
pub(crate) const SYS_CACHE_SIZE: usize = 85;
// procnumber.h; CacheInvalidateSmgr packs the ProcNumber into three bytes.
pub(crate) const MAX_BACKENDS_BITS: i32 = 18;

#[derive(Clone, Copy)]
pub(crate) struct SyscacheCallbackItem {
    pub(crate) id: i16,
    pub(crate) link: i16,
    pub(crate) function: SyscacheCallbackFunction,
    pub(crate) arg: Datum,
}

#[derive(Clone, Copy)]
pub(crate) struct RelcacheCallbackItem {
    pub(crate) function: RelcacheCallbackFunction,
    pub(crate) arg: Datum,
}

#[derive(Clone, Copy)]
pub(crate) struct RelsyncCallbackItem {
    pub(crate) function: RelSyncCallbackFunction,
    pub(crate) arg: Datum,
}

// C's static callback arrays verbatim: fixed capacity, Copy items, one
// indirect call per dispatch (non-per-row). The registrant set is closed in C
// but lives outside this crate, so rule-4 enum dispatch cannot enumerate it.
pub(crate) struct CallbackTables {
    pub(crate) syscache_list: [Option<SyscacheCallbackItem>; MAX_SYSCACHE_CALLBACKS],
    pub(crate) syscache_count: usize,
    pub(crate) relcache_list: [Option<RelcacheCallbackItem>; MAX_RELCACHE_CALLBACKS],
    pub(crate) relcache_count: usize,
    pub(crate) relsync_list: [Option<RelsyncCallbackItem>; MAX_RELSYNC_CALLBACKS],
    pub(crate) relsync_count: usize,
}

thread_local! {
    // Apart from the message state: dispatch copies one Copy item per step
    // under a short borrow (re-reading count/link like C) and invokes with NO
    // borrow held — callbacks re-enter inval (reference's ResetPlanCache).
    pub(crate) static CALLBACKS: RefCell<CallbackTables> = const {
        RefCell::new(CallbackTables {
            syscache_list: [None; MAX_SYSCACHE_CALLBACKS],
            syscache_count: 0,
            relcache_list: [None; MAX_RELCACHE_CALLBACKS],
            relcache_count: 0,
            relsync_list: [None; MAX_RELSYNC_CALLBACKS],
            relsync_count: 0,
        })
    };
    // Cells, not part of the RefCell table: the per-message head-link probe
    // in CallSyscacheCallbacks must cost one load, like C's static array.
    pub(crate) static SYSCACHE_LINKS: [Cell<i16>; SYS_CACHE_SIZE] =
        const { [const { Cell::new(0) }; SYS_CACHE_SIZE] };
}

pub(crate) struct InvalState<'mcx> {
    pub(crate) mcx: Mcx<'mcx>,
    // Cursor-written: an aborted subxact can leave dead slots past the live
    // group's `nextmsg`; later adds overwrite them in place.
    pub(crate) msg_arrays: [PgVec<'mcx, SharedInvalidationMessage>; 2],
    pub(crate) trans_stack: PgVec<'mcx, registration::TransInvalidationInfo>,
    pub(crate) inplace_info: Option<registration::InvalidationInfo>,
    // D3866: the row-stamped catalog caches (l2cache ROW_STAMPED_CAT_IDS) a row of which, committed before this
    // transaction, it changed (invalidate.rs `changes_committed_row`), a bit per cache id: the commit's send moves their
    // program stamps (eoxact `l2_bump_for_messages`). Cleared at transaction end.
    pub(crate) stamp_touched: u128,
    // D3866 (step 2): what the messages this transaction registered name (`PendingInvalidationsTouch`). Cleared with
    // `stamp_touched`.
    pub(crate) pending: PendingSummary,
    // C3 S5 (D3890): the relations whose relcache messages a sender scoped (`with_relcache_scope`), each with the scopes
    // registered for it (`REL_SCOPE_ALL` too when it was also registered whole); a relation not here is whole. Cleared
    // with `stamp_touched`.
    pub(crate) rel_scopes: PgVec<'mcx, (Oid, u8)>,
    // C3 S5 (D3889): the catalog rows (cache id, hash value) whose catcache messages a sender scoped, as `rel_scopes`;
    // `cat_unscoped`, the caches with a row registered unscoped (a later scoped row of one of them is whole too).
    pub(crate) cat_scopes: PgVec<'mcx, (i32, u32, u8)>,
    pub(crate) cat_unscoped: u128,
    // D3888: the operator families this transaction's support catalog rows named (invalidate.rs `support_family`), each
    // once: `PreCommit_Inval` registers relcache messages of the relations built from them. Cleared with `stamp_touched`.
    pub(crate) support_families: PgVec<'mcx, Oid>,
    // LogLogicalInvalidations wire-image buffers, capacity retained.
    pub(crate) wal_scratch: [PgVec<'mcx, u8>; 2],
}

bind!(pub(crate) InvalStateTy => InvalState<'mcx>);

thread_local! {
    // ManuallyDrop keeps the TLS payload !needs_drop (lessons §8); the
    // C statics live in TopMemoryContext for the backend's whole life anyway.
    pub(crate) static STATE: RefCell<Option<ManuallyDrop<McxOwned<InvalStateTy>>>> =
        const { RefCell::new(None) };
    pub(crate) static DEBUG_DISCARD_CACHES: Cell<i32> = const { Cell::new(0) };
    pub(crate) static ACCEPT_RECURSION_DEPTH: Cell<i32> = const { Cell::new(0) };
}

// Single borrow per entry. Seams invoked under it must be ones whose C bodies
// never re-enter inval (sinval send, xlog insert, xact/relcache/catalog
// reads); anything that can re-enter (user callbacks, catcache prepare,
// LocalExecuteInvalidationMessage) runs with the borrow released.
pub(crate) fn with_state<R>(f: impl for<'mcx> FnOnce(&mut InvalState<'mcx>) -> R) -> R {
    STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            init_state(&mut slot);
        }
        slot.as_mut().unwrap().with_mut(f)
    })
}

#[cold]
#[inline(never)]
fn init_state(slot: &mut Option<ManuallyDrop<McxOwned<InvalStateTy>>>) {
    let owned =
        McxOwned::<InvalStateTy>::try_new(MemoryContext::new("CacheInvalidation"), |mcx| {
            Ok(InvalState {
                mcx,
                msg_arrays: [PgVec::new_in(mcx), PgVec::new_in(mcx)],
                trans_stack: PgVec::new_in(mcx),
                inplace_info: None,
                stamp_touched: 0,
                pending: PendingSummary::default(),
                rel_scopes: PgVec::new_in(mcx),
                cat_scopes: PgVec::new_in(mcx),
                cat_unscoped: 0,
                support_families: PgVec::new_in(mcx),
                wal_scratch: [PgVec::new_in(mcx), PgVec::new_in(mcx)],
            })
        })
        .expect("CacheInvalidation context allocation");
    *slot = Some(ManuallyDrop::new(owned));
    // Session-memory teardown (FPBUDGET-1): freed at clean task end.
    ::mcx::register_session_cleanup(Box::new(|| {
        STATE.with(|cell| {
            if let Some(owned) = cell.borrow_mut().take() {
                drop(ManuallyDrop::into_inner(owned));
            }
        });
    }));
}

/// True when the current transaction holds registered invalidation messages
/// not yet broadcast to the shared queue (uncommitted DDL). Retention
/// (wretain): a pooled parallel worker's sinval drain cannot see these, so a
/// leader with pending messages flags the launch and workers fall back to
/// C's fresh-process InvalidateSystemCaches.
pub fn TransactionHasPendingInvalidationMessages() -> bool {
    with_state(|state| {
        state.trans_stack.iter().any(|t| {
            t.ii.current_cmd_invalid_msgs.num_in_group() > 0
                || t.prior_cmd_invalid_msgs.num_in_group() > 0
        }) || state
            .inplace_info
            .as_ref()
            .is_some_and(|i| i.current_cmd_invalid_msgs.num_in_group() > 0)
    })
}

/// D3866 (step 2): whether this transaction's unbroadcast invalidation messages touch what a program over relations
/// `relids` and catalog caches `cat_ids` read (`PendingSummary`, noted at registration: no message list is read): a
/// relcache message of one of `relids` (or of every relation), a catalog-wide message (relmap messages are sent at once,
/// never pending), or a catcache message of one of `cat_ids`; of a row-stamped cache (l2cache
/// `ROW_STAMPED_CAT_IDS`) only when the transaction changed a row of it committed before it (`stamp_touched`): its own
/// created rows name objects no committed program names, its dropped ones objects its statements can no longer name.
/// `false`: a program filed under the committed catalog state reads nothing this transaction changed.
pub fn PendingInvalidationsTouch(relids: &[Oid], cat_ids: &[i32]) -> bool {
    PendingInvalidationsTouchParts(relids, &[], cat_ids, false)
}

/// C3 S5d: `PendingInvalidationsTouch` by what was read of each relation, under the senders' scopes (`rel_scopes`,
/// `cat_unscoped`): a relcache entry (`relids`) is touched by a whole entry's message, not by one scoped to a part no
/// program holds (the triggers' enabled states) or to the storage; the storage or size statistics (`storage`) by a
/// whole entry's message or a storage-scoped one; a catalog cache by a row registered unscoped (a pg_class row whose
/// storage alone changed moves only the storage readers). `under`: the reads were made under the DDL, so a row-stamped
/// cache's row the transaction created touches them too (a read before the DDL cannot have found that row, D3866; a
/// read under it can, and must not outlive it).
pub fn PendingInvalidationsTouchParts(relids: &[Oid], storage: &[Oid], cat_ids: &[i32], under: bool) -> bool {
    with_state(|state| {
        let (p, touched) = (&state.pending, state.stamp_touched);
        let scope = |r: Oid| if p.has_rel(r) { rel_scope(&state.rel_scopes, r) } else { 0 };
        p.all
            || cat_ids.iter().any(|&id| {
                let bit = eoxact::stamp_bit(id);
                state.cat_unscoped & bit != 0 && (under || !l2cache::row_stamped(id) || touched & bit != 0)
            })
            || relids.iter().any(|&r| scope(r) & REL_SCOPE_ALL != 0)
            || storage.iter().any(|&r| scope(r) & (REL_SCOPE_ALL | REL_SCOPE_STORAGE) != 0)
    })
}

/// D3866 (step 2): what the invalidation messages this transaction registered name, noted as each is registered
/// (registration.rs), so `PendingInvalidationsTouch` reads no message list: one transaction's runs to tens of thousands
/// (Django's test-database migrations), and a scan of it per admission cost up to 1 ms. A superset of the pending
/// messages: those an aborted subtransaction or a sent in-place update dropped stay named until the transaction ends.
#[derive(Clone, Copy)]
pub(crate) struct PendingSummary {
    /// catcache ids with a message (`eoxact::stamp_bit`)
    pub(crate) cats: u128,
    /// a catalog-wide message, or a relcache message of every relation
    pub(crate) all: bool,
    /// the relations of the relcache messages: a 4096-bit filter, two bits a relation (a false hit only touches)
    pub(crate) rels: [u64; 64],
}

impl Default for PendingSummary {
    fn default() -> Self {
        PendingSummary { cats: 0, all: false, rels: [0; 64] }
    }
}

impl PendingSummary {
    fn rel_bits(rel: Oid) -> [usize; 2] {
        let h = u64::from(rel).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        [(h >> 52) as usize, ((h >> 40) & 0xFFF) as usize]
    }

    pub(crate) fn note_rel(&mut self, rel: Oid) {
        if rel == types_core::InvalidOid {
            self.all = true;
            return;
        }
        for b in Self::rel_bits(rel) {
            self.rels[b / 64] |= 1u64 << (b % 64);
        }
    }

    pub(crate) fn has_rel(&self, rel: Oid) -> bool {
        Self::rel_bits(rel).iter().all(|&b| self.rels[b / 64] & (1u64 << (b % 64)) != 0)
    }
}

/// C3 S5 (D3890): what a relcache invalidation of a relation changed, as its sender knows it: the whole entry (the
/// default), only its triggers' enabled states (`REL_SCOPE_TRIGGERS`: EnableDisableTrigger; pg_trigger has no
/// syscache, so that message is the only one), or (D3889) only its storage and its size statistics
/// (`REL_SCOPE_STORAGE`: a pg_class row update changing no other column, `pg_class_storage_only`: TRUNCATE's new
/// relfilenode, a REINDEX; an index rebuild's statistics, catalog_index `index_update_stats`). At commit every scope
/// moves the L2 relcache generation (the entry is rebuilt); the program generation of what a lowering read of the entry
/// moves only for the whole entry (a trigger's enabled state is read where it fires), the storage generation for it
/// and the storage scope (eoxact `l2_bump_for_messages`, l2cache `invalidate_rel_part`). A scoped catcache message (the
/// pg_class row's) leaves its row's program generation (l2cache `invalidate_cat_storage`).
pub const REL_SCOPE_ALL: u8 = 1;
pub const REL_SCOPE_TRIGGERS: u8 = 2;
pub const REL_SCOPE_STORAGE: u8 = 4;

/// The scope `with_relcache_scope` gives this backend's registrations now (0: none, the whole object).
pub fn relcache_scope() -> u8 {
    REL_SCOPE.with(Cell::get)
}

thread_local! {
    /// The scope `with_relcache_scope` gives this backend's relcache registrations (0: none, the whole entry).
    static REL_SCOPE: Cell<u8> = const { Cell::new(0) };
}

/// `f` with this backend's relcache invalidation registrations scoped `scope` (`REL_SCOPE_TRIGGERS`): the sender's
/// knowledge of what it changed.
pub fn with_relcache_scope<T>(scope: u8, f: impl FnOnce() -> T) -> T {
    let _back = RelScopeBack(REL_SCOPE.with(|s| s.replace(scope)));
    f()
}

/// The scope `with_relcache_scope` displaced, put back on any exit (an unwind too).
struct RelScopeBack(u8);

impl Drop for RelScopeBack {
    fn drop(&mut self) {
        REL_SCOPE.with(|s| s.set(self.0));
    }
}

/// C3 S5: a relcache registration of `rel_id` under this backend's scope (`with_relcache_scope`), noted before
/// `PendingSummary::note_rel`: a scoped one is kept (`REL_SCOPE_ALL` with it when the relation was registered before,
/// unscoped: the summary is a superset, so a false hit only widens it), a whole one widens a kept relation.
pub(crate) fn note_rel_scope(state: &mut InvalState<'_>, rel_id: Oid) {
    let scope = match REL_SCOPE.with(Cell::get) {
        0 => REL_SCOPE_ALL,
        s => s,
    };
    if let Some(e) = state.rel_scopes.iter_mut().find(|e| e.0 == rel_id) {
        e.1 |= scope;
    } else if scope != REL_SCOPE_ALL && rel_id != types_core::InvalidOid {
        let before = if state.pending.has_rel(rel_id) { REL_SCOPE_ALL } else { 0 };
        state.rel_scopes.push((rel_id, scope | before));
    }
}

/// C3 S5: the scopes registered for `rel_id` this transaction (`REL_SCOPE_ALL` when not kept).
pub(crate) fn rel_scope(scopes: &[(Oid, u8)], rel_id: Oid) -> u8 {
    scopes.iter().find(|e| e.0 == rel_id).map_or(REL_SCOPE_ALL, |e| e.1)
}

/// C3 S5 (D3889): a catcache registration of row `hash` of cache `cache_id` under this backend's scope, as
/// `note_rel_scope`: a scoped one is kept (whole too when a row of the cache was registered unscoped before), a whole
/// one widens a kept row and marks its cache.
pub(crate) fn note_cat_scope(state: &mut InvalState<'_>, cache_id: i32, hash: u32) {
    let scope = match REL_SCOPE.with(Cell::get) {
        0 => REL_SCOPE_ALL,
        s => s,
    };
    let bit = crate::eoxact::stamp_bit(cache_id);
    if scope == REL_SCOPE_ALL {
        state.cat_unscoped |= bit;
    }
    if let Some(e) = state.cat_scopes.iter_mut().find(|e| e.0 == cache_id && e.1 == hash) {
        e.2 |= scope;
    } else if scope != REL_SCOPE_ALL {
        let before = if state.cat_unscoped & bit != 0 { REL_SCOPE_ALL } else { 0 };
        state.cat_scopes.push((cache_id, hash, scope | before));
    }
}

/// C3 S5: the scopes registered for row `hash` of cache `cache_id` this transaction (`REL_SCOPE_ALL` when not kept).
pub(crate) fn cat_scope(scopes: &[(i32, u32, u8)], cache_id: i32, hash: u32) -> u8 {
    scopes.iter().find(|e| e.0 == cache_id && e.1 == hash).map_or(REL_SCOPE_ALL, |e| e.2)
}

pub fn set_debug_discard_caches(value: i32) {
    DEBUG_DISCARD_CACHES.set(value);
}

pub fn debug_discard_caches() -> i32 {
    DEBUG_DISCARD_CACHES.get()
}

pub fn init_seams() {
    guc_tables::vars::debug_discard_caches.install(guc_tables::GucVarAccessors {
        get: debug_discard_caches,
        set: set_debug_discard_caches,
    });
    inval_seams::accept_invalidation_messages::set(local::AcceptInvalidationMessages);
    // D3.2 overlay rule (coarse form): a session holding unbroadcast
    // invalidation messages modified a catalog this transaction; it must
    // build cache entries privately (no L2 read or publish) until commit.
    l2cache::set_pending_invals_probe(|| TransactionHasPendingInvalidationMessages());
    // C3 S4b: what a program read is checked against this transaction's own messages (gravity_deps `Deps::current`)
    l2cache::set_pending_touch_probe(PendingInvalidationsTouchParts);
}

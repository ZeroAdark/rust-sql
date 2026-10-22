//! hir's raw half (lint R6): the `Send + Sync` claim of a hosted owner's template (D3438), shared read-only across
//! backends by the process-wide program tier, with its contract; the per-expression window reset (D3583); and an owner's
//! lanes past 0, published once by a compare-exchange (D3620).
#![allow(unsafe_code)]

use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use gravity_abi::PtrWord;
use types_core::Oid;

use super::{Lane, LANES};

/// D3438: what every backend that runs a program filed without its owner (`Program::hosted`) rebuilds the owner from
/// (`rebuild`): the hosted expressions' programs, read and never written once compiled, each call's binding and what its
/// carrier says, the user functions' row stamps the lowering read, the catalog its pool sites reach and the IN-list
/// descriptors. Nothing of a backend is in it: no `FmgrCall`, memory context, island, cell or plan address.
/// planprod P6: the programs' lists live in the template's own root (`root`, 'GravityHostedTpl', its last field, so they
/// drop before it; memsig E-prog: compacted there from the lowering's lists root, where they were built in place): owned
/// at birth by the lowering's statement, by the session when the building backend's cache files
/// the program (`reown`, from `adopt`), and by the program caches once the process-wide tier holds it (`publish`); never
/// moved after that. Each call's binding (`calls`: a call's facts hold its argument types) stays on the heap.
pub(super) struct HostTpl {
    /// D4580 (`own_bytes`, from the publish): first, so it drops before the frees of what it counts.
    own: pgsync::Mutex<memledger::charge::Charge>,
    pub(super) progs: mcx::PgVec<'static, gravity_expr::nodes::HostedProgram>,
    pub(super) calls: Vec<Vec<super::TplCall>>,
    pub(super) stamps: Vec<(Oid, [u32; 3])>,
    pub(super) any: bool,
    /// D3620: no site writes into the result region (`byval_only`): a rebuilt owner gets no base (`owner_of`'s `lazy`).
    pub(super) lazy: bool,
    pub(super) catalog: gravity_fn::pool_catalog::Reach<'static>,
    pub(super) descs: Vec<std::sync::Arc<crate::step::raw::saop_host::Desc>>,
    pub(super) bytes: usize,
    /// The program caches took the root's bytes (`publish`): no owner moves them after.
    published: AtomicBool,
    root: pgsync::Mutex<mcx::SendRoot>,
}

// SAFETY: a template's programs are immutable after the lowering (the backends' cells read their sites, argument forms and
// names; nodes and owned copies are never written), and every other field is plain data or a `Send + Sync` descriptor.
// `progs`' lists are blocks of `root` and hold its context's handle: nothing allocates, grows or frees through them while
// the template is shared (they are read in place), they free only at the template's drop, which the last `Arc` runs on one
// thread before the root drops (field order), and the root's owner moves only under its lock (`reown`, `publish`).
unsafe impl Send for HostTpl {}
unsafe impl Sync for HostTpl {}

impl HostTpl {
    /// The template over `programs` (`make_template`): their lists compacted into a new root owned by the running
    /// statement (memsig E-prog: from the lowering's lists root, which drops after, `hir::finish_keeping`).
    pub(super) fn new(programs: Vec<gravity_expr::Program<'static>>, calls: Vec<Vec<super::TplCall>>, stamps: Vec<(Oid, [u32; 3])>, any: bool, lazy: bool, catalog: &gravity_fn::pool_catalog::Reach<'_>, descs: Vec<std::sync::Arc<crate::step::raw::saop_host::Desc>>, bytes: usize) -> HostTpl {
        let (root, m) = owned_root("GravityHostedTpl", memledger::charge::statement_owner());
        let mut progs = mcx::reserved_in(programs.len(), m);
        // SAFETY: the plan arena outlives the programs (`nodes::func::set_fn_expr`'s contract), and their lists are held in
        // `progs`, which drops before `root` (field order)
        mcx::GrowIn::extend_in(&mut progs, programs.into_iter().map(|p| unsafe { p.into_hosted(m) }));
        HostTpl { own: pgsync::Mutex::new(memledger::charge::Charge::NONE), progs, calls, stamps, any, lazy, catalog: catalog.clone_in(m), descs, bytes, published: AtomicBool::new(false), root: pgsync::Mutex::new(root) }
    }

    /// The building backend's cache filed the program (`adopt`): the root is `o`'s, unless the program caches hold it.
    pub(super) fn reown(&self, o: memledger::Owner) {
        if !self.published.load(Ordering::Acquire) {
            pgsync::lock(&self.root).set_owner(o);
        }
    }

    /// The process-wide tier holds the program (`hir::publish`): the root is the program caches' from now, once.
    pub(super) fn publish(&self, caches: memledger::Owner) {
        use memledger::charge::{release_of, Charge, Door};
        if !self.published.swap(true, Ordering::AcqRel) {
            pgsync::lock(&self.root).set_owner(caches);
            *pgsync::lock(&self.own) = Charge::open(caches, release_of(caches), memledger::Kind::Region, Door::Heap, self.own_bytes());
        }
    }

    pub(super) fn own_bytes(&self) -> u64 {
        use core::mem::size_of;
        let calls: usize = self.calls.iter().map(|c| c.capacity() * size_of::<super::TplCall>() + c.iter().map(|t| t.facts.argtypes.capacity() * size_of::<Oid>()).sum::<usize>()).sum();
        let lists = self.calls.capacity() * size_of::<Vec<super::TplCall>>() + calls + self.stamps.capacity() * size_of::<(Oid, [u32; 3])>() + self.descs.capacity() * size_of::<std::sync::Arc<crate::step::raw::saop_host::Desc>>();
        memledger::arc_bytes::<HostTpl>() + mcx::SendRoot::HEADER_BYTES + lists as u64
    }

    /// The bytes the root holds now (the witnesses').
    #[cfg(test)]
    pub(super) fn root_bytes(&self) -> usize {
        pgsync::lock(&self.root).allocated()
    }
}

gravity_abi::host! {
    /// D3583: expression `i`'s function-manager windows reset (`Ir::reset_seen`).
    pub(super) unsafe fn h_reset_ex(lane: u64, i: u64) -> u64 {
        // SAFETY: `lane` is the running function's lane (`Ir::enter`'s), whose `states` hold its `n` expressions' cells;
        // a non-null `fmgr` is a call that lane owns, live for the program's life
        let l = &*(lane as *const super::Lane);
        if i < l.n {
            for c in (*l.states.ptr().add(i as usize)).heads_mut().filter(|c| !c.fmgr.ptr().is_null()) {
                (*c.fmgr.ptr()).reset_window();
            }
        }
        0
    }
}

/// planprod P4: the allocator of an owner's product root (`owner_of`, the owner's last field `root`). `'static` as the
/// owner's lists are (`Hold`, `Lanes::first`): every allocation `owner_of` makes through it is held by the owner's `hold`
/// or `lanes`, which drop before `root`; `owner_of` alone calls this, and keeps the handle no longer than its own body.
pub(super) fn product_mcx(root: &mcx::PinnedContext) -> mcx::Mcx<'static> {
    // SAFETY: the contract above: what is allocated through the handle drops before `root`, which outlives the call
    unsafe { root.handle() }
}

/// A hosted expression's program moved into its owner's product root (`m`, `product_mcx`'s): its lists
/// there, for the owner's life (`Progs::Own`, in the owner's `hold`, which drops before the root).
pub(super) fn hosted_program(c: gravity_expr::Compiled<'_>, m: mcx::Mcx<'static>) -> gravity_expr::HostedCompiled {
    // SAFETY: the plan arena outlives the program (`nodes::func::set_fn_expr`'s contract), and the result is held in the
    // owner's `hold`, which drops before the root `m` allocates from (`Owner::root`, the owner's last field)
    unsafe { c.into_in(m) }
}

/// D3438: the programs a template keeps (`make_template`), at the template's lifetime.
pub(super) fn for_template(c: Vec<gravity_expr::Compiled<'_>>) -> Vec<gravity_expr::Compiled<'static>> {
    // SAFETY: the plan arena outlives the programs (`nodes::func::set_fn_expr`'s contract); a lifetime-only change of the
    // list, so the heap buffers the cells point into (`prog.sites`, `prog.convs`, `state.calls`) do not move.
    unsafe { core::mem::transmute::<Vec<gravity_expr::Compiled<'_>>, Vec<gravity_expr::Compiled<'static>>>(c) }
}

/// planprod row 4 / P6: a lane past 0's root ('GravityHostedLane') or a template's ('GravityHostedTpl'), owned at birth
/// by `holder` (a 1 KiB keeper: a few lists), and its allocator at the root's lifetime. `'static` as the holder's lists are (`LaneHold`, `HostTpl`):
/// everything made through the handle is held beside the root, the holder's last field, which drops after them; a SendRoot
/// holds its context box raw, so the address holds when the root moves into the holder; the lists are built on one thread
/// before the holder is published, and never grow after.
pub(super) fn owned_root(name: &'static str, holder: memledger::Owner) -> (mcx::SendRoot, mcx::Mcx<'static>) {
    let mut root = mcx::SendRoot::new_owned_small(name, holder);
    let m = root_mcx(&mut root);
    (root, m)
}

/// `root`'s allocator at the root's holder's lifetime (`owned_root`'s), under `owned_root`'s contract: the root moves into
/// its holder (its last field) and outlives every block made through the handle.
pub(super) fn root_mcx(root: &mut mcx::SendRoot) -> mcx::Mcx<'static> {
    let ctx = root.mcx().context();
    // SAFETY: the contract above; a SendRoot holds its context box raw, so the address holds when the root moves
    let ctx: &'static mcx::MemoryContext = unsafe { &*(ctx as *const mcx::MemoryContext) };
    ctx.mcx()
}

/// Lane 0's result region (`owner_of`): `bytes` uninitialized bytes in `m` (D848: never zeroed).
pub(super) fn region_in(bytes: usize, m: mcx::Mcx<'static>) -> mcx::PgVec<'static, core::mem::MaybeUninit<u8>> {
    let mut region = mcx::reserved_in(bytes, m);
    // SAFETY: `reserved_in` reserved `bytes`, and `MaybeUninit` bytes need no initialization
    unsafe { region.set_len(bytes) };
    region
}

/// D3620: an owner's lanes. Lane 0 (the leader's) is built with the owner; the other `LANES - 1` (the pool workers' and the
/// nested lanes) are one table allocated at the first entry of an ordinal past 0 (`h_enter`), so a program only the leader
/// runs (every deep statement level of a SQL-function recursion among them) holds one lane, not `LANES` (~30 KB a level).
/// A lane never moves once built: its address is its spill chunks' key (`spillable`), so lane 0 is boxed apart from the
/// owner (which moves into its `Rc`) and the rest's table is published once.
pub(super) struct Lanes {
    /// planprod P4: in the owner's product root (`Owner::root`, which drops after the lanes). The rest's table stays on the
    /// heap: a pool worker builds it, and a context is one thread's.
    pub(super) first: mcx::PgBox<'static, Lane>,
    pub(super) rest: AtomicPtr<Lane>,
    /// The tables `rest_lane` built and the ones a race's losers freed (the race witness's count).
    #[cfg(test)]
    pub(super) tables: (core::sync::atomic::AtomicUsize, core::sync::atomic::AtomicUsize),
}

impl Lanes {
    pub(super) fn new(first: Lane, m: mcx::Mcx<'static>) -> Lanes {
        Lanes {
            first: mcx::box_in(first, m),
            rest: AtomicPtr::new(core::ptr::null_mut()),
            #[cfg(test)]
            tables: Default::default(),
        }
    }

    /// Lane `ordinal` (< `LANES`, the driver's ordinal space), the rest's table allocated at the first entry past lane 0.
    pub(super) fn lane(&mut self, ordinal: usize) -> *mut Lane {
        if ordinal == 0 {
            return &mut *self.first;
        }
        self.rest_lane(ordinal)
    }

    /// Lane `ordinal` past 0 (1..`LANES`). Workers entering at once race on the table's one compare-exchange (only
    /// atomics, so `&self`); a loser frees the copy it built and takes the winner's.
    pub(super) fn rest_lane(&self, ordinal: usize) -> *mut Lane {
        let mut rest = self.rest.load(Ordering::Acquire);
        if rest.is_null() {
            let table: Box<[Lane]> = (1..LANES).map(|_| Lane::idle()).collect(); // alloc: Program
            let built = Box::into_raw(table).cast::<Lane>();
            #[cfg(test)]
            self.tables.0.fetch_add(1, Ordering::Relaxed);
            rest = match self.rest.compare_exchange(core::ptr::null_mut(), built, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => built,
                Err(won) => {
                    // SAFETY: `built` is the table this call leaked above and never published.
                    drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(built, LANES - 1)) });
                    #[cfg(test)]
                    self.tables.1.fetch_add(1, Ordering::Relaxed);
                    won
                }
            };
        }
        debug_assert!(ordinal < LANES);
        // SAFETY: the rest's table holds `LANES - 1` lanes and `ordinal` is in 1..LANES.
        unsafe { rest.add(ordinal - 1) }
    }

    /// planprod row 4 (`adopt`): every armed lane past 0 re-owns its root to `o` (lane 0's lists are the owner's root's).
    /// `&self`, as `adopt` holds the owner shared: it runs on the leader when the backend's cache files the program, with no
    /// worker inside it (no entry of this program runs), as `rest_lane`'s callers' entries are the only other writers.
    pub(super) fn reown(&self, o: memledger::Owner) {
        let rest = self.rest.load(Ordering::Acquire);
        if rest.is_null() {
            return;
        }
        for i in 0..LANES - 1 {
            // SAFETY: a published table holds `LANES - 1` lanes for the owner's life; no worker is inside (above)
            let l = unsafe { &mut *rest.add(i) };
            if let Some(h) = l.hold.as_mut() {
                h.root.set_owner(o);
            }
        }
    }

    /// Every lane built so far: lane 0, then the rest's table when an entry past lane 0 allocated it.
    pub(super) fn each(&mut self) -> impl Iterator<Item = &mut Lane> {
        let rest = *self.rest.get_mut();
        let rest: &mut [Lane] = if rest.is_null() {
            &mut []
        } else {
            // SAFETY: a published table holds `LANES - 1` lanes and lives until `Lanes` drops; `&mut self` excludes any entry.
            unsafe { core::slice::from_raw_parts_mut(rest, LANES - 1) }
        };
        core::iter::once(&mut *self.first).chain(rest.iter_mut())
    }
}

impl Drop for Lanes {
    fn drop(&mut self) {
        let rest = *self.rest.get_mut();
        if !rest.is_null() {
            // SAFETY: the table `lane` published (`LANES - 1` lanes), freed only here.
            drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(rest, LANES - 1)) });
        }
    }
}

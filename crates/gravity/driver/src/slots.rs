use gravity_abi::PtrWord;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use pgsync::Mutex;

use gravity_program::cost::runtime::{RELEASED_CAP, RELEASED_LOG_CAP as LOG_CAP};
use gravity_program::{ContentKey, Program};

use crate::mem::arena::{Arena, ArenaBox, ArenaVec, Mark};
use crate::binding::WorkerBinding;
use crate::node::{build_image, image_bytes, Derive, Image, Inner, Node, Role};

mod raw;
pub(crate) use raw::{grow_partial, grow_partial_in_place, grow_partial_to, share_buf_of};

/// D4631: a heap list kept across a park (emptied at the statement's end, kept at its capacity) joins `list_bytes`, so a
/// parked set's depot charge (`Parked::held`) counts it.
pub struct WorkerSlot {
    pub(crate) inner: ArenaBox<Inner>,
    pub(crate) regions: ArenaVec<(usize, usize)>,
    pub(crate) claim_mark: Mark,
    pub(crate) binding: Option<WorkerBinding<'static>>,
    pub(crate) busy: AtomicBool,
    pub(crate) arena: Arena,
    /// The breaker state still holds the previous section's bytes: the
    /// worker zeroes it at its first claim (W × 32 MB memsets off the
    /// leader), and a slot that never claimed has no partial to merge.
    pub(crate) state_stale: bool,
    /// This section's partial region: the arena region for a program-constant
    /// breaker, `share_buf` for a share-budgeted one; `state_arm` the share to
    /// arm it with at the first claim (0 = program-constant).
    pub(crate) state_ptr: *mut u8,
    pub(crate) state_bytes: u32,
    pub(crate) state_arm: u32,
    /// The most this worker's partial holds before its breaker spills (`grant::ceiling_of`: each participant its own
    /// work_mem budget), set at the section's bind; 0 = none (its grows keep the W-then-G roads).
    pub(crate) share_ceil: u64,
    /// The forced-spill test fault on its partial's room asks (`grow::Fault`, the grant's `spill_every`), set beside
    /// `share_ceil`.
    pub(crate) share_fault: crate::mem::grow::Fault,
    /// C10: its pipeline's breaker spills past its grant (`Pipeline::holds`), set beside `share_ceil`.
    pub(crate) share_spills: bool,
    /// NR-1: the SPILL-DEBT row its pipeline's breaker names past its grant (`Holds::debt`), set beside `share_spills`.
    pub(crate) share_debt: Option<gravity_program::spill::NoSpill>,
    /// D4033: the section's share when this worker's share opened smaller at the pool's room (D3226 (c1): `state_arm`
    /// below it), else 0, set at the section's bind. Its region's `SPILL_REGION_COMMIT` need 0 answer carries it
    /// (`grow::region_commit`), so its phase-1 table's partitions are cut as every other worker's of the section are
    /// (kernels `fold_bits`): the owner phase reads every partial at one partition count.
    pub(crate) share_cut: u32,
    share_buf: crate::node::RegionBuf,
    /// The largest share this ordinal armed at, the `grown_bytes` census (D222: a depot re-take is not growth).
    share_high: u32,
    /// The program-constant partial region (`Budget::Fixed`) on lazy zero pages beside the arena, kept at the largest need.
    fixed_buf: crate::node::RegionBuf,
    /// The limits the two buffers count (`Counters::limit`): the uncut share, the fixed bytes.
    share_counted: u64,
    fixed_counted: u64,
    /// D254: the buffers a grown partial replaced on the worker's thread, settled by the leader at the section's close (a charged
    /// buffer's `Rc`s are the leader thread's to touch).
    retired: Vec<crate::node::RegionBuf>,
    /// D3196 (rearm E1): the slot's hosts are still bound past the section that bound them, a nested run's (a rescan's member,
    /// which the next rescan opens again): what they were bound for. `None`: no host is bound outside a section.
    pub(crate) warm: Option<Warm>,
    /// D4630 (b): the section's open left the slot's hosts unbound under `bind_at_claim`: its first claim binds them on
    /// the claiming thread (`Open::claim_on`); a slot no claim reached is let go unbound (`section::raw::detach`).
    pub(crate) bind_pending: bool,
    /// D4725 (`serial_skip`): the sites below 64 the slot never binds, set at each open (`serial_sites`).
    pub(crate) skip: u64,
}

/// D3196 (rearm E1): what a warm slot's hosts were bound for: the binding's statement words (the snapshot, command, abort
/// byte, identity, per-tuple context, pins and resolved relations). A slot binds every source of the program, whatever
/// pipeline opens it, from no parameter image (`section::bind_hosts`) and no width (no binding carries one): an open of any
/// pipeline at any width under the same words keeps the bound hosts; any other open unbinds them first, as do a failed
/// section and the statement's end. `of` names every word of the binding: a new one is a build error there until the key
/// weighs it.
#[derive(Clone, Copy)]
pub(crate) struct Warm {
    snapshot: crate::binding::Snapshot,
    command_id: crate::binding::CommandId,
    abort_flag: usize,
    identity: crate::binding::Identity,
    per_tuple: crate::binding::PerTuple,
    pins: crate::binding::Pins,
    resolved: usize,
}

impl PartialEq for Warm {
    fn eq(&self, o: &Warm) -> bool {
        (self.snapshot, self.command_id, self.abort_flag, self.identity, self.per_tuple, self.pins, self.resolved) == (o.snapshot, o.command_id, o.abort_flag, o.identity, o.per_tuple, o.pins, o.resolved)
    }
}

impl Warm {
    pub(crate) fn of(parts: &crate::binding::BindingParts<'_>) -> Warm {
        let crate::binding::BindingParts { snapshot, command_id, params: _, param_nulls: _, abort_flag, pins, per_tuple, identity, resolved } = *parts;
        let abort_flag = abort_flag as *const core::sync::atomic::AtomicU8 as usize;
        let resolved = resolved.map_or(0, |r| r as *const (dyn core::any::Any + Send + Sync) as *const () as usize);
        Warm { snapshot, command_id, abort_flag, identity, per_tuple, pins, resolved }
    }
}

/// D3196 (rearm E1): `PGRUST_GRAVITY_WARM_SECTIONS=0` unbinds every section's slots at its close, a nested run's too (the
/// measurement's lever; read once).
pub(crate) fn warm_on() -> bool {
    static ON: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *ON.get_or_init(|| crate::env::num::<u32>("PGRUST_GRAVITY_WARM_SECTIONS") != Some(0))
}

/// D4630 (b): a worker slot's bind moves from the leader's section open to the slot's first claim, on the claiming thread,
/// for a slot whose every host allows it (`Source::binds_at_claim`): the leader's open then binds none of the width's
/// slots, one after another. Default on; `PGRUST_GRAVITY_SLOT_BIND_AT_CLAIM=0` binds at the open again (read once, or set
/// by `set_bind_at_claim`).
pub(crate) fn bind_at_claim() -> bool {
    match BIND_AT_CLAIM.load(core::sync::atomic::Ordering::Relaxed) {
        0 => {
            let on = crate::env::num::<u32>("PGRUST_GRAVITY_SLOT_BIND_AT_CLAIM") != Some(0);
            BIND_AT_CLAIM.store(1 + on as u8, core::sync::atomic::Ordering::Relaxed);
            on
        }
        v => v == 2,
    }
}

static BIND_AT_CLAIM: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// D4725: a worker slot never binds the source of a site only leader pipelines drive (`serial_sites`: a pgrc merged
/// sorted site's, whose bind is not at-claim), at the open or at its claim: no worker runs a pipeline that reads it, so
/// one such site no longer sends every slot of the width back to the leader's serial bind. Default off;
/// `PGRUST_GRAVITY_SLOT_SERIAL_SKIP=1` on (read once, or set by `set_serial_skip`).
pub(crate) fn serial_skip() -> bool {
    match SERIAL_SKIP.load(core::sync::atomic::Ordering::Relaxed) {
        0 => {
            let on = crate::env::num::<u32>("PGRUST_GRAVITY_SLOT_SERIAL_SKIP").is_some_and(|v| v != 0);
            SERIAL_SKIP.store(1 + on as u8, core::sync::atomic::Ordering::Relaxed);
            on
        }
        v => v == 2,
    }
}

static SERIAL_SKIP: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// D4725: `serial_skip` set for the process (tests, a measurement), over the environment's.
pub fn set_serial_skip(on: bool) {
    SERIAL_SKIP.store(1 + on as u8, core::sync::atomic::Ordering::Relaxed);
}

/// D4725: the sites below 64 that pipelines drive, none of them parallel-eligible (exec admission's `AdmitCx::leader`):
/// a worker slot never reads them. A site no pipeline drives (a nest's or a SubPlan's inner, probed from any pipeline) is
/// never one.
pub(crate) fn serial_sites(pipelines: &[gravity_program::Pipeline]) -> u64 {
    let (mut driven, mut parallel) = (0u64, 0u64);
    for p in pipelines {
        if let Ok(s) = u32::try_from(p.driving_site) {
            if s < 64 {
                driven |= 1 << s;
                if p.is_eligible() {
                    parallel |= 1 << s;
                }
            }
        }
    }
    driven & !parallel
}

/// D4630 (b): `bind_at_claim` set for the process (tests, a measurement), over the environment's.
pub fn set_bind_at_claim(on: bool) {
    BIND_AT_CLAIM.store(1 + on as u8, core::sync::atomic::Ordering::Relaxed);
}

/// D4709: a statement whose sources are granule sources and by-rowid fetch sites parks its worker slots in the
/// depot as an all-granule statement does, its fetch recipes in the depot salt (exec `recipe_salt`), so its next run
/// takes the set instead of building every slot and binding its buffers again on the leader. Default on;
/// `PGRUST_GRAVITY_DEPOT_FETCH=0` turns it off, the off lever (read once, or set by `set_depot_fetch`).
pub fn depot_fetch() -> bool {
    match DEPOT_FETCH.load(core::sync::atomic::Ordering::Relaxed) {
        0 => {
            let on = crate::env::num::<u32>("PGRUST_GRAVITY_DEPOT_FETCH") != Some(0);
            DEPOT_FETCH.store(1 + on as u8, core::sync::atomic::Ordering::Relaxed);
            on
        }
        v => v == 2,
    }
}

static DEPOT_FETCH: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// D4709: `depot_fetch` set for the process (tests, a measurement), over the environment's.
pub fn set_depot_fetch(on: bool) {
    DEPOT_FETCH.store(1 + on as u8, core::sync::atomic::Ordering::Relaxed);
    DEPOT_FETCH_SET.store(true, core::sync::atomic::Ordering::Relaxed);
}

static DEPOT_FETCH_SET: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Whether `set_depot_fetch` set the switch (the memory settings view's source: `set` over the environment's).
pub fn depot_fetch_was_set() -> bool {
    DEPOT_FETCH_SET.load(core::sync::atomic::Ordering::Relaxed)
}

/// D3258: a slot dropped with W still carved (its Node dropped without `Entry::end`: the statement ledger's 53200 is a panic
/// unwinding from the block that passed the limit, so a refusal right after a section's bind dropped the Exec in
/// `run_parallel` with no `Node::reset`, the only road W took back) gives W back here. 3 MiB x 16 workers stayed held by no
/// statement at each such 53200 (census 8: the cancel at 90 % landed in cbgen_C014's section open), so every later grant the
/// governor refused waited for bytes that never came back (`held_by_statements` > 0: halved at the 30 s queue stall).
/// `reset` takes W first, so a slot it saw gives back nothing here.
impl Drop for WorkerSlot {
    fn drop(&mut self) {
        let left = core::mem::take(&mut self.inner.w);
        if left != 0 {
            crate::mem::grant::POOL.release(left as usize);
        }
    }
}

impl WorkerSlot {
    /// D3957: a refused image arena is the statement's `OutOfMemory`; the open that provisions the slot fails its exec.
    fn new(program: &Program, derive: &mut dyn Derive, owner: *mut u8) -> crate::Result<WorkerSlot> {
        let bytes = image_bytes(program, derive, Role::Worker);
        let arena = Arena::try_new(bytes).ok_or_else(|| crate::node::refused_arena(bytes))?;
        let a = arena.handle();
        let Image { inner, regions, .. } = build_image(a, program, derive, Role::Worker, owner);
        let claim_mark = a.mark();
        Ok(WorkerSlot { inner, regions, claim_mark, binding: None, busy: AtomicBool::new(false), state_stale: false, state_ptr: core::ptr::null_mut(), state_bytes: 0, state_arm: 0, share_ceil: 0, share_fault: crate::mem::grow::Fault::OFF, share_spills: false, share_debt: None, share_cut: 0, share_buf: crate::node::RegionBuf::empty(), share_high: 0, fixed_buf: crate::node::RegionBuf::empty(), share_counted: 0, fixed_counted: 0, retired: Vec::new(), warm: None, bind_pending: false, skip: 0, arena }) // alloc: Node
    }

    pub(crate) fn fixed_region(&mut self, bytes: u32, stmt: Option<&crate::mem::grant::Reservation>, limits: &crate::mem::grant::Counters) -> crate::Result<*mut u8> {
        if self.fixed_buf.len() < bytes as usize {
            self.fixed_buf = crate::node::RegionBuf::empty();
            self.fixed_buf = self.bind_region(bytes as usize, stmt)?;
            limits.limit(bytes as u64, core::mem::replace(&mut self.fixed_counted, bytes as u64));
        } else if !self.fixed_buf.recharge(stmt, None) {
            self.fixed_buf = crate::node::RegionBuf::empty();
            self.fixed_buf = self.bind_region(bytes as usize, stmt)?;
        }
        Ok(self.fixed_buf.as_mut_ptr())
    }

    /// A worker's region at its section's bind (leader thread), from the depot against G. D275: when G's room is gone (an
    /// earlier section's bind carved it as the workers' W: a W8 build's, then the probe's program-constant regions found
    /// none) an admitted statement's region comes from this worker's W, else is reserved past G and counted as grown
    /// (`chunk_region`, the arena's and the grows' road: D254, no runtime past-share error before M7's spill.breakers).
    fn bind_region(&mut self, bytes: usize, stmt: Option<&crate::mem::grant::Reservation>) -> crate::Result<crate::node::RegionBuf> {
        match crate::park::REGIONS.take(bytes, stmt) {
            Err(e @ crate::Error::OutOfMemory { .. }) if stmt.is_some_and(crate::mem::grant::Reservation::admitted) => crate::node::region_past(crate::park::region_bytes(bytes), Some(&mut self.inner.w), stmt).ok_or(e),
            r => r,
        }
    }

    /// D3226 (c1): `bind_region` for the worker's share: past its refusal, a region at the pool's room now
    /// (`share_at_room`), with the share bytes it holds, which the bind arms (`Section::bind`).
    fn bind_share(&mut self, bytes: u32, stmt: Option<&crate::mem::grant::Reservation>) -> crate::Result<(crate::node::RegionBuf, u32)> {
        match self.bind_region(bytes as usize, stmt) {
            Ok(buf) => Ok((buf, bytes)),
            Err(e) => crate::mem::region::share_at_room(bytes, stmt, e),
        }
    }

    /// The share-budgeted partial region of `bytes` (alloc: Node); the second value is the share past this ordinal's
    /// largest so far, 0 at the same or a smaller share even when the depot had to map it again; the third the share the
    /// region holds: `bytes`, or less when the pool's room opened it (D3226 (c1), `bind_share`).
    /// Under an admitted reservation a region more than an eighth over the share (the depot's rule for a charged take, D225)
    /// goes back to the depot for one of this share's size, as one under it does: kept, an earlier narrower section's larger
    /// shares drew past the reservation, which counts each worker at its section's share (`footprint`: the breaker cut
    /// `width` ways). An uncharged taker keeps it (the depot hands it up to 4x its ask, D223). Option c': `reserve` > 0 (a
    /// `Budget::Pool` side, `grow::pool_reserve`) binds a reservation of that many bytes charged at the share
    /// (`reserved_region`), whose pool the fold commits in place; between binds it parks in the depot uncommitted.
    /// `laid`: the region started small (`grant::starts_small`): its reservation is marked so (`RegionBuf::set_laid`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn share_region(&mut self, bytes: u32, stmt: Option<&crate::mem::grant::Reservation>, limits: &crate::mem::grant::Counters, uncut: u64, reserve: u64, floor: bool, laid: u32) -> crate::Result<(*mut u8, u32, u32)> {
        let want = crate::park::region_bytes(bytes as usize);
        let mut held = bytes;
        let grows = self.share_buf.len() < bytes as usize;
        let over = stmt.is_some_and(crate::mem::grant::Reservation::admitted) && self.share_buf.len().saturating_sub(want) > want / 8;
        if reserve > bytes as u64 || self.share_buf.is_reserved() {
            crate::park::REGIONS.put_from_life(core::mem::take(&mut self.share_buf), stmt);
            self.share_buf = if reserve > bytes as u64 {
                if laid != 0 {
                    // A parked laid region's resident length is the start, past the share when it grew past it (D3794): the
                    // table's limit is the region's writable end, so its commits in place go on from there
                    let buf = crate::node::laid_region(want, reserve as usize, laid as usize, Some(&mut self.inner.w), stmt)?;
                    held = held.max(buf.len().min(buf.lay_reserve()) as u32 & !7);
                    buf
                } else {
                    let (buf, h) = crate::mem::region::reserved_share(bytes, reserve as usize, Some(&mut self.inner.w), stmt, floor)?;
                    held = h;
                    buf
                }
            } else {
                let (buf, h) = self.bind_share(bytes, stmt)?;
                held = h;
                buf
            };
            let counted = uncut.max(self.share_counted);
            limits.limit(counted, core::mem::replace(&mut self.share_counted, counted));
        } else if grows || over {
            // mid-statement: the charge goes to the pool so G's room comes back (`put_from_life`; `put` settles, kept in G)
            crate::park::REGIONS.put_from_life(core::mem::take(&mut self.share_buf), stmt);
            let (buf, h) = self.bind_share(bytes, stmt)?;
            (self.share_buf, held) = (buf, h);
            // the counted limit follows the largest share this ordinal took, as it did when a region only ever grew
            // (footprint's per-worker max): a smaller region only draws less of it
            let counted = uncut.max(self.share_counted);
            limits.limit(counted, core::mem::replace(&mut self.share_counted, counted));
        } else if !self.share_buf.recharge(stmt, None) {
            self.share_buf = crate::node::RegionBuf::empty();
            let (buf, h) = self.bind_share(bytes, stmt)?;
            (self.share_buf, held) = (buf, h);
        }
        let grown = if bytes > self.share_high { bytes } else { 0 };
        self.share_high = self.share_high.max(bytes);
        Ok((self.share_buf.as_mut_ptr(), grown, held))
    }

    /// The share region back to the depot, G's room with it (leader thread, between sections: a section's partials merge or spill at its close,
    /// so an idle ordinal's region holds nothing a later pipeline reads). Its counted limit stays (`share_region`'s rule) until
    /// the ordinal's next bind re-counts it or the statement's end clears it.
    pub(crate) fn release_share(&mut self) {
        if self.share_buf.is_empty() {
            return;
        }
        if self.state_ptr == self.share_buf.as_mut_ptr() {
            (self.state_ptr, self.state_bytes, self.state_arm, self.state_stale) = (core::ptr::null_mut(), 0, 0, false);
        }
        crate::park::REGIONS.put_from_life(core::mem::take(&mut self.share_buf), None);
    }

    /// LEDGER2 step 5 (leader thread, at the bind): the share or fixed buffer this bind laid the worker's partial in (the one
    /// holding `state_ptr`, as `partial_charged` reads it) charged to its slot's owner `o`; the other buffer, kept from an
    /// earlier section for another slot, stays with whom it was charged to.
    pub(crate) fn own_partial(&mut self, o: memledger::Owner) {
        if self.state_ptr.is_null() {
            return;
        }
        let p = self.state_ptr as usize;
        let holds = |b: &crate::node::RegionBuf| !b.is_empty() && p >= b.as_ptr() as usize && p < b.as_ptr() as usize + b.len();
        if holds(&self.share_buf) {
            self.share_buf.own_by(o);
        } else if holds(&self.fixed_buf) {
            self.fixed_buf.own_by(o);
        }
    }

    /// Ruling 5's resident bytes (leader thread): the share and fixed buffers and the arena chunks this worker holds.
    pub(crate) fn resident(&self) -> u64 {
        (self.share_buf.len() + self.fixed_buf.len()) as u64 + self.inner.chunk_bytes()
    }

    /// The bytes the pool charges for the share or fixed buffer this worker's last bind laid its partial in, when that
    /// partial is breaker slot `slot`'s (leader thread, between sections); 0 otherwise (an arena partial included).
    pub(crate) fn partial_charged(&self, slot: u8) -> u64 {
        if self.state_ptr.is_null() || self.inner.own_slot != slot {
            return 0;
        }
        let p = self.state_ptr as usize;
        let holds = |b: &crate::node::RegionBuf| !b.is_empty() && p >= b.as_ptr() as usize && p < b.as_ptr() as usize + b.len();
        if holds(&self.share_buf) {
            self.share_buf.charged_bytes()
        } else if holds(&self.fixed_buf) {
            self.fixed_buf.charged_bytes()
        } else {
            0
        }
    }

    /// D254 (leader thread, the section closed): the buffers grown partials replaced back to the depot.
    pub(crate) fn settle_retired(&mut self) {
        self.retired.drain(..).for_each(|b| crate::park::REGIONS.put(b));
    }

    /// D4631: the heap blocks of the slot's per-statement lists at their capacity (each emptied at the statement's end,
    /// `settle_regions` and `Node::reset_timed`, and kept for the next statement's pushes): a parked set's depot charge
    /// counts them (`Parked::held`).
    fn list_bytes(&self) -> usize {
        fn b<T>(v: &Vec<T>) -> usize {
            v.capacity() * core::mem::size_of::<T>()
        }
        let n = &*self.inner;
        b(&self.retired) + b(&n.chunks) + b(&n.spills) + b(&n.owner_maps) + b(&n.bigs) + b(&n.work_maps) + b(&n.stmt_pins) + b(&n.slot_pins) + b(&n.readers)
    }

    /// The statement's end (leader thread): the buffers and chunks to the free list.
    pub(crate) fn settle_regions(&mut self) {
        self.settle_retired();
        let regions = &crate::park::REGIONS;
        regions.put(core::mem::take(&mut self.share_buf));
        regions.put(core::mem::take(&mut self.fixed_buf));
        (self.share_counted, self.fixed_counted) = (0, 0);
        crate::hostimpl::arena::settle_grow_chunks(&mut self.inner);
        crate::hostimpl::arena::drop_result_spills(&mut self.inner);
        self.inner.rehome_out();
        self.inner.chunk_next = 0;
        self.inner.pinned = 0;
        self.inner.stmt_pins.clear();
        self.inner.slot_pins.clear();
        self.state_ptr = core::ptr::null_mut();
        self.state_bytes = 0;
        self.state_arm = 0;
    }

    /// The share and fixed buffers dropped (a parked slot under pool pressure); the pointers into
    /// them are re-taken at the next bind.
    pub(crate) fn shed_regions(&mut self) -> usize {
        let freed = self.share_buf.len() + self.fixed_buf.len() + self.inner.chunks.iter().map(|c| c.len()).sum::<usize>();
        self.retired.clear();
        self.share_buf = crate::node::RegionBuf::empty();
        self.fixed_buf = crate::node::RegionBuf::empty();
        (self.share_counted, self.fixed_counted) = (0, 0);
        self.inner.chunks.clear();
        self.inner.chunk_next = 0;
        self.inner.pinned = 0;
        self.state_ptr = core::ptr::null_mut();
        self.state_bytes = 0;
        self.state_arm = 0;
        freed
    }

    /// The arena's capacity in bytes plus the region buffers and the sources' kept buffers (D3213): what the depot's cap
    /// and the idle trims count.
    pub(crate) fn bytes(&self) -> usize {
        self.arena.handle().capacity() + self.share_buf.len() + self.fixed_buf.len() + self.inner.chunks.iter().map(|c| c.len()).sum::<usize>() + self.inner.kept_bytes()
    }

    pub(crate) fn zero_regions(&mut self) {
        let a = self.arena.handle();
        for &(off, n) in self.regions.iter() {
            a.zero(off, n);
        }
    }

    pub fn ctx(&self) -> *mut gravity_abi::Ctx {
        self.inner.ctx
    }

    pub fn pins(&self) -> u64 {
        self.inner.pins()
    }

    pub fn owner(&self) -> *mut u8 {
        self.inner.block.owner.ptr()
    }

    pub fn rows_processed(&self) -> u64 {
        self.binding.as_ref().map_or(0, |b| b.rows_processed)
    }

    /// D3196 (rearm E1): a warm slot's hosts unbound (leader thread): the statement's end (`Node::cool_workers`).
    pub(crate) fn cool(&mut self) {
        if self.warm.take().is_some() {
            self.inner.for_each_host(&mut |h| h.unbind());
        }
    }
}

/// D1507: relations whose parked sources must give back what they keep across statements (`Source::release_rel`: the
/// kept decode cursors hold their part files open). Recorded by `note_released` from the relcache invalidation callback;
/// applied at statement boundaries only (`drain_released`, `SlotDepot::take` / `put`, the entry's park at acquire and
/// release). SAFETY / ordering: invalidation processing runs inside any catalog access, possibly while this backend holds
/// the depot lock (`put`'s byte-cap eviction drops slots under it) or a borrow of its park, so the recording side takes
/// no lock, touches no park, allocates nothing and cannot fail: a CAS into a fixed set, else the `ALL` flag. A drain
/// swaps `ANY` off, then `ALL`, then each slot to 0 BEFORE it walks: a relid recorded during the walk sets `ANY` again
/// after that swap and is applied by the next drain, never lost. What a walk moves out is dropped after the depot lock.
static RELEASED: [AtomicU32; RELEASED_CAP] = [const { AtomicU32::new(0) }; RELEASED_CAP];
static RELEASED_ALL: AtomicBool = AtomicBool::new(false);
static RELEASED_ANY: AtomicBool = AtomicBool::new(false);

/// The parks' share of the record (each backend's `Entry` park, never reached from the callback): a process-wide log that
/// every Entry reads from its own `released_seen` generation, so a session that moved threads still sees it (D1507).
/// `note` bumps `gen` (fetch_add, AcqRel) and THEN stores `(gen << 32) | rel` into `log[gen % LOG_CAP]` (Release); `since`
/// loads `gen` (Acquire) and each word (Acquire, pairing with that store). A word whose tag is not its generation (not
/// stored yet, or overwritten by a writer LOG_CAP ahead) and a gap over LOG_CAP both release every relation: conservative,
/// a released cursor reopens lazily; a pending store is covered by that release-all, never skipped as stale.
pub(crate) struct ReleaseLog {
    gen: AtomicU64,
    log: [AtomicU64; LOG_CAP],
}

impl ReleaseLog {
    pub(crate) const fn new() -> ReleaseLog {
        ReleaseLog { gen: AtomicU64::new(0), log: [const { AtomicU64::new(0) }; LOG_CAP] }
    }

    /// The generation a new reader starts from (it has nothing parked yet).
    pub(crate) fn current(&self) -> u64 {
        self.gen.load(Ordering::Acquire)
    }

    /// Record `rel` (0: every relation). No lock, no allocation, cannot fail.
    pub(crate) fn note(&self, rel: u32) {
        let g = self.gen.fetch_add(1, Ordering::AcqRel) + 1;
        self.log[(g % LOG_CAP as u64) as usize].store(((g as u32 as u64) << 32) | rel as u64, Ordering::Release);
    }

    /// What was recorded after `*seen`, and `*seen` moved to the current generation; `None` when nothing was.
    pub(crate) fn since(&self, seen: &mut u64) -> Option<Released> {
        let cur = self.gen.load(Ordering::Acquire);
        let from = *seen;
        if cur == from {
            return None;
        }
        *seen = cur;
        let mut out = Released { all: cur.wrapping_sub(from) > LOG_CAP as u64, rels: [0; RELEASED_CAP] };
        let mut n = 0;
        let mut g = from;
        while !out.all && g != cur {
            g = g.wrapping_add(1);
            let w = self.log[(g % LOG_CAP as u64) as usize].load(Ordering::Acquire);
            let rel = w as u32;
            if (w >> 32) as u32 != g as u32 || rel == 0 {
                out.all = true;
            } else if !out.rels[..n].contains(&rel) {
                if n == RELEASED_CAP {
                    out.all = true;
                } else {
                    out.rels[n] = rel;
                    n += 1;
                }
            }
        }
        Some(out)
    }
}

pub(crate) static RELEASE_LOG: ReleaseLog = ReleaseLog::new();

/// D4768: the relations whose storage went (`note_removed`, from the columnar catalog's removed hook: DROP
/// TABLE / DATABASE, TRUNCATE, a rewrite's old storage), for a set that came to its Node before the removal and is put
/// after it (`Node::depot_seen`).
pub(crate) static REMOVED_LOG: ReleaseLog = ReleaseLog::new();

/// D4768: relation `rel`'s storage went (0: none): a set that came to its Node before this and is put after it drops at
/// the put (`Node::depot_seen`). The removal notes each relation, then evicts the parked sets (`SlotDepot::evict_reading`).
/// No lock, no allocation.
pub fn note_removed(rel: u32) {
    if rel != 0 {
        REMOVED_LOG.note(rel);
    }
}

/// A drained record: every relation (`all`), else the listed relids (0: an empty entry).
pub struct Released {
    all: bool,
    rels: [u32; RELEASED_CAP],
}

impl Released {
    /// Every relation: a release of every kept cursor and pin, whatever the relcache recorded.
    pub(crate) const ALL: Released = Released { all: true, rels: [0; RELEASED_CAP] };

    /// D4768: the listed relids (none listed under `all`).
    fn listed(&self) -> &[u32] {
        let n = self.rels.iter().position(|&r| r == 0).unwrap_or(RELEASED_CAP);
        &self.rels[..n]
    }

    /// P2 W1: whether the record may reach a source that reads one of `rels` (`Park::release_recorded` passes a parked
    /// Node it cannot: `Source::release_rel` releases nothing from a source of another relation).
    pub(crate) fn touches(&self, rels: &[u32]) -> bool {
        self.all || rels.iter().any(|r| self.listed().contains(r))
    }

    /// Each `release_rel` argument the record asks for (`0` once for all).
    fn each(&self, mut f: impl FnMut(u32)) {
        if self.all {
            f(0);
        } else {
            self.rels.iter().filter(|&&r| r != 0).for_each(|&r| f(r));
        }
    }
}

/// D1507: relation `rel`'s cached state is stale (`0`: every relation's), from the relcache invalidation callback. No lock,
/// no allocation, no error (see `RELEASED`); it only records, for the depot and for this thread's park.
pub fn note_released(rel: u32) {
    let mut placed = rel == 0;
    if !placed {
        for s in &RELEASED {
            match s.compare_exchange(0, rel, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => placed = true,
                Err(v) => placed = v == rel,
            }
            if placed {
                break;
            }
        }
    }
    if rel == 0 || !placed {
        RELEASED_ALL.store(true, Ordering::Release);
    }
    RELEASED_ANY.store(true, Ordering::Release);
    RELEASE_LOG.note(rel);
}

/// Whether `rel` (0: any relation) is recorded and not yet drained: a read, no swap (tests of the callback's reach).
pub fn released_pending(rel: u32) -> bool {
    RELEASED_ANY.load(Ordering::Acquire) && (rel == 0 || RELEASED_ALL.load(Ordering::Acquire) || RELEASED.iter().any(|s| s.load(Ordering::Acquire) == rel))
}

/// The depot's record swapped out (see `RELEASED` for the order); `None` when nothing was recorded.
fn take_released() -> Option<Released> {
    if !RELEASED_ANY.swap(false, Ordering::AcqRel) {
        return None;
    }
    let all = RELEASED_ALL.swap(false, Ordering::AcqRel);
    let mut rels = [0u32; RELEASED_CAP];
    for (r, s) in rels.iter_mut().zip(&RELEASED) {
        *r = s.swap(0, Ordering::AcqRel);
    }
    Some(Released { all, rels })
}

/// A park's record after its reader's `*seen` generation (the entry, outside any park borrow; see `RELEASE_LOG`).
pub(crate) fn park_released_since(seen: &mut u64) -> Option<Released> {
    RELEASE_LOG.since(seen)
}

/// The generation a new Entry starts from.
pub(crate) fn park_released_current() -> u64 {
    RELEASE_LOG.current()
}

/// `record` applied to one slot's sources: what they kept goes to `doomed`, which the caller drops after its lock.
fn release_slot(inner: &mut Inner, record: &Released, doomed: &mut Vec<Box<dyn core::any::Any>>) {
    inner.for_each_host(&mut |h| {
        if let crate::node::HostRef::Source(s) = h {
            record.each(|rel| doomed.extend(s.release_rel(rel)));
        }
    });
}

/// D1507: the recorded relations released from every parked slot set now (the entry seam's statement top; one relaxed
/// load when nothing was recorded). The released cursors drop after the depot lock.
pub fn drain_released() {
    if !RELEASED_ANY.load(Ordering::Relaxed) {
        return;
    }
    let mut doomed: Vec<Box<dyn core::any::Any>> = Vec::new(); // alloc: Cold
    {
        // D4398: in the ladder thread's pass, left for the next drain while another thread holds the depot
        let Some(mut e) = memledger::ladder_pass::lock_or_skip(&DEPOT.entries) else { return };
        DEPOT.release_locked(&mut e, &mut doomed);
    }
    drop(doomed);
}

/// The process-wide slot depot (partitioned-record.md §4.5): worker slots parked between statements
/// under (program content, recipe salt) — capacity only (arena, region and source buffers; no binding,
/// partial or `Resolved`), a fresh backend finds them; `DEPOT_SETS` / `DEPOT_SETS_PER_KEY` cap them by count (the least
/// recently put drop first), `DEPOT_BYTES` by bytes (the largest drop first).
pub struct SlotDepot {
    entries: Mutex<Vec<Parked>>,
    clock: AtomicU64,
    takes: AtomicU64,
    puts: AtomicU64,
    evictions: AtomicU64,
}

struct Parked {
    /// D4631: the set's own heap no other charge holds, its slot list's block and each slot's per-statement lists
    /// (`WorkerSlot::list_bytes`), on the depot's holder from the put; declared FIRST, so a trim or an eviction gives the
    /// dropping thread those bytes before they free, and a take drops it on the taking thread, whose backend then holds
    /// the set as the one that made it did. Made on the parking backend's threads and kept with the set, they were charged
    /// nowhere, and another backend's idle release freed them below its account's zero.
    held: memledger::Charge,
    key: (ContentKey, u64),
    slots: Vec<WorkerSlot>,
    bytes: usize,
    /// D4554: of `bytes`, what its sources' parks give back (`Source::park_bytes`): the buffers a warm set keeps under the
    /// keep-warm switch (`Node::release_workers`), 0 once parked (`shed`).
    warm: usize,
    stamp: u64,
}

impl Parked {
    /// D4768: whether a source of the set reads a relation `removed` names.
    fn reads_any(&self, removed: &dyn Fn(u32) -> bool) -> bool {
        self.slots.iter().any(|s| s.inner.reads_any(removed))
    }

    /// The set at `i` out of the depot's list into `out` (dropped by the caller after the lock).
    fn evict_at(e: &mut Vec<Parked>, i: usize, out: &mut Vec<Parked>) {
        out.push(e.swap_remove(i)); // alloc: Cold
    }

    /// D4554: the set's sources parked here, as the keep-warm switch's off arm parks them at the release: their warm
    /// buffers given back, the set kept. The bytes freed.
    fn shed(&mut self) -> usize {
        if self.warm == 0 {
            return 0;
        }
        let warm = |slots: &[WorkerSlot]| slots.iter().map(|s| s.inner.park_bytes()).sum::<usize>();
        let before = warm(&self.slots);
        // bounded("other: one parked set's worker slots, at most its width")
        for s in self.slots.iter_mut() {
            s.inner.for_each_host(&mut |h| {
                if let crate::node::HostRef::Source(src) = h {
                    src.park();
                }
            });
        }
        let freed = before.saturating_sub(warm(&self.slots));
        (self.bytes, self.warm) = (self.slots.iter().map(WorkerSlot::bytes).sum(), 0);
        freed
    }
}

pub use mempolicy::keep::DEPOT_BYTES;

/// D3213: the parked sets the depot keeps, and the sets one key keeps: the memory policy's (`mempolicy::keep`).
pub use mempolicy::keep::{DEPOT_SETS, DEPOT_SETS_PER_KEY};

/// DR-18: the slot depot's cap: 1/3 of the grant pool (`DEPOT_BYTES` at the measured pool; `mempolicy::keep`).
pub fn depot_bytes() -> usize {
    mempolicy::keep::slot_depot_bytes(crate::mem::grant::POOL.capacity())
}

pub static DEPOT: SlotDepot = SlotDepot { entries: Mutex::new(Vec::new()), clock: AtomicU64::new(0), takes: AtomicU64::new(0), puts: AtomicU64::new(0), evictions: AtomicU64::new(0) }; // alloc: Cold

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DepotStats {
    pub entries: usize,
    pub bytes: usize,
    /// D4631: of the depot holder's account, the parked sets' own heap (`Parked::held`: each set's slot list and lists).
    pub held: u64,
    pub takes: u64,
    pub puts: u64,
    pub evictions: u64,
}

impl SlotDepot {
    fn take(&self, key: (ContentKey, u64)) -> Option<Vec<WorkerSlot>> {
        let mut doomed: Vec<Box<dyn core::any::Any>> = Vec::new(); // alloc: Cold
        let taken = {
            let mut e = self.entries.lock().unwrap_or_else(|p| p.into_inner());
            // D1507: the recorded relations released before a set is handed out (no statement runs on a dropped relation's part)
            self.release_locked(&mut e, &mut doomed);
            (0..e.len()).filter(|&i| e[i].key == key).max_by_key(|&i| e[i].stamp).map(|i| {
                self.takes.fetch_add(1, Ordering::Relaxed);
                e.swap_remove(i)
            })
        };
        drop(doomed);
        // D4631: the set's depot charge back on the taking thread first: its backend holds the set's lists from now on
        taken.map(|Parked { held, slots, .. }| {
            drop(held);
            slots
        })
    }

    /// `seen`: the removals' generation when the set came to its Node (`Node::depot_seen`).
    fn put(&self, key: (ContentKey, u64), slots: Vec<WorkerSlot>, mut seen: u64) {
        let bytes = slots.iter().map(WorkerSlot::bytes).sum();
        // D4631: on the parking backend's thread, where its statement's threads grew them
        let loose = slots.capacity() * core::mem::size_of::<WorkerSlot>() + slots.iter().map(WorkerSlot::list_bytes).sum::<usize>();
        let held = memledger::Charge::open(memledger::Owner::Cache(memledger::Holder::SlotDepot), memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, loose as u64);
        let warm = if mempolicy::keep::keep_warm() { slots.iter().map(|s| s.inner.park_bytes()).sum() } else { 0 };
        let stamp = self.clock.fetch_add(1, Ordering::Relaxed) + 1;
        let mut doomed: Vec<Box<dyn core::any::Any>> = Vec::new(); // alloc: Cold
        let mut evicted: Vec<Parked> = Vec::new(); // alloc: Cold
        let set = Parked { held, key, slots, bytes, warm, stamp };
        let mut removed = None;
        'park: {
            let mut e = self.entries.lock().unwrap_or_else(|p| p.into_inner());
            // D4768: a relation removed since the set came to its Node, one it reads: no take could find it, so it drops
            // here instead of waiting in the depot (checked under the lock `evict_reading` evicts under)
            if REMOVED_LOG.since(&mut seen).is_some_and(|r| r.all || set.reads_any(&|rel| r.listed().contains(&rel))) {
                removed = Some(set);
                self.evictions.fetch_add(1, Ordering::Relaxed);
                break 'park;
            }
            e.push(set); // alloc: Cold
            self.puts.fetch_add(1, Ordering::Relaxed);
            self.release_locked(&mut e, &mut doomed);
            while e.iter().filter(|p| p.key == key).count() > DEPOT_SETS_PER_KEY {
                let victim = (0..e.len()).filter(|&i| e[i].key == key && e[i].stamp != stamp).min_by_key(|&i| e[i].stamp).expect("an older set of the key");
                Parked::evict_at(&mut e, victim, &mut evicted);
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
            while e.len() > DEPOT_SETS {
                let victim = (0..e.len()).filter(|&i| e[i].stamp != stamp).min_by_key(|&i| e[i].stamp).expect("an older set");
                Parked::evict_at(&mut e, victim, &mut evicted);
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
            let cap = depot_bytes();
            while e.len() > 1 && e.iter().map(|p| p.bytes).sum::<usize>() > cap {
                let victim = (0..e.len()).filter(|&i| e[i].stamp != stamp).max_by_key(|&i| e[i].bytes).expect("an older entry");
                Parked::evict_at(&mut e, victim, &mut evicted);
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
        // D1699: the evicted sets drop after the lock, as `trim_for`'s: a slot's drop unmaps its arena, frees its chunks and
        // drops its sources (a kept cursor closes its part file); under the lock it held every backend's take / put / trim.
        drop(removed);
        drop(evicted);
        drop(doomed);
    }

    /// D1507: the recorded relations (`take_released`) released from every parked set; capacity stays (a relcache
    /// invalidation also comes for ANALYZE, ALTER, statistics: the cursors reopen lazily). The caller holds the lock and
    /// drops `doomed` after it: a released cursor's drop closes its file.
    fn release_locked(&self, e: &mut [Parked], doomed: &mut Vec<Box<dyn core::any::Any>>) {
        let Some(record) = take_released() else { return };
        for p in e.iter_mut() {
            for s in p.slots.iter_mut() {
                release_slot(&mut s.inner, &record, doomed);
            }
        }
    }

    /// D4768: the parked sets that read a relation `removed` names (relids whose storage went: the columnar catalog's
    /// removed hook, which notes each relation first, `note_removed`, for a set put later) evicted now, on the calling
    /// thread. No program that read a removed relation runs again, so no take finds those sets: they waited for a put's
    /// eviction or a trim, up to `DEPOT_SETS` of them. A set a statement holds is not here. Each set's depot charge drops
    /// first (`Parked::held`), so its bytes come off the depot's holder and free on this thread. The sets evicted.
    pub fn evict_reading(&self, removed: &dyn Fn(u32) -> bool) -> usize {
        let mut doomed: Vec<Box<dyn core::any::Any>> = Vec::new(); // alloc: Cold
        let mut evicted: Vec<Parked> = Vec::new(); // alloc: Cold
        {
            let mut e = self.entries.lock().unwrap_or_else(|p| p.into_inner());
            self.release_locked(&mut e, &mut doomed);
            let mut i = 0;
            while i < e.len() {
                if e[i].reads_any(removed) {
                    Parked::evict_at(&mut e, i, &mut evicted);
                } else {
                    i += 1;
                }
            }
            self.evictions.fetch_add(evicted.len() as u64, Ordering::Relaxed);
        }
        let n = evicted.len();
        drop(evicted);
        drop(doomed);
        n
    }

    /// Parked sets dropped oldest first until `bytes` are freed (the grant pool under pressure: parked
    /// slots hold their regions and chunks in the pool, and a statement past its reservation could not
    /// reach them, so a wide grouping refused mid-statement); `usize::MAX` drops every set.
    /// The bytes freed; the slots drop outside the lock.
    pub fn trim_for(&self, bytes: usize) -> usize {
        let mut victims: Vec<Parked> = Vec::new(); // alloc: Cold
        let mut freed = 0usize;
        {
            let mut e = self.entries.lock().unwrap_or_else(|p| p.into_inner());
            while freed < bytes && !e.is_empty() {
                let oldest = (0..e.len()).min_by_key(|&i| e[i].stamp).expect("non-empty");
                let p = e.swap_remove(oldest);
                freed += p.bytes;
                victims.push(p); // alloc: Cold
            }
        }
        self.evictions.fetch_add(victims.len() as u64, Ordering::Relaxed);
        drop(victims);
        freed
    }

    /// Memory step 5b's reclaim (`mem::reclaim`): parked sets dropped oldest first until `bytes` are freed, the newest set
    /// always kept (a hot repeat retakes its own parked set; dropping it re-streamed the part cache's dictionaries every
    /// run, D3169). D4554: then the kept sets' warm buffers (`Parked::shed`), the sets kept. The bytes freed; the slots drop
    /// outside the lock (a shed only frees its sources' buffers, under it).
    pub fn trim_but_newest(&self, bytes: usize) -> usize {
        let mut victims: Vec<Parked> = Vec::new(); // alloc: Cold
        let mut freed = 0usize;
        {
            // D4398: in the ladder thread's pass, nothing while another thread holds the depot
            let Some(mut e) = memledger::ladder_pass::lock_or_skip(&self.entries) else { return 0 };
            while freed < bytes && e.len() > mempolicy::keep::NEWEST_KEPT {
                let oldest = (0..e.len()).min_by_key(|&i| e[i].stamp).expect("non-empty");
                let p = e.swap_remove(oldest);
                freed += p.bytes;
                victims.push(p); // alloc: Cold
            }
            // bounded("other: the sets the depot keeps past the trim, at most NEWEST_KEPT")
            for p in e.iter_mut() {
                if freed >= bytes {
                    break;
                }
                freed += p.shed();
            }
        }
        self.evictions.fetch_add(victims.len() as u64, Ordering::Relaxed);
        drop(victims);
        freed
    }

    /// C3: the bytes `trim_but_newest` could free now (every parked set but the `NEWEST_KEPT` newest, and their warm buffers).
    pub fn trimmable_bytes(&self) -> usize {
        // D4398: in the ladder thread's pass, none while another thread holds the depot
        let Some(e) = memledger::ladder_pass::lock_or_skip(&self.entries) else { return 0 };
        let total: usize = e.iter().map(|p| p.bytes).sum();
        let (mut kept, mut below) = (0usize, u64::MAX);
        for _ in 0..mempolicy::keep::NEWEST_KEPT {
            if let Some(p) = e.iter().filter(|p| p.stamp < below).max_by_key(|p| p.stamp) {
                // D4554: a kept set's warm buffers are the trim's too (`Parked::shed`)
                kept += p.bytes.saturating_sub(p.warm);
                below = p.stamp;
            }
        }
        total.saturating_sub(kept)
    }

    /// Every parked set dropped, their regions' pool bytes back when it returns (a large mapped region went to the reaper,
    /// `mem::reap`, which it waits for).
    pub fn trim(&self) -> usize {
        let freed = self.trim_for(usize::MAX);
        crate::mem::reap::drain();
        freed
    }

    pub fn stats(&self) -> DepotStats {
        // D4398: on the memory ladder's pass (the part cache's short hook), an empty count while another thread holds it
        let Some(e) = memledger::ladder_pass::lock_or_skip(&self.entries) else {
            return DepotStats { entries: 0, bytes: 0, held: 0, takes: self.takes.load(Ordering::Relaxed), puts: self.puts.load(Ordering::Relaxed), evictions: self.evictions.load(Ordering::Relaxed) };
        };
        DepotStats { entries: e.len(), bytes: e.iter().map(|p| p.bytes).sum(), held: e.iter().map(|p| p.held.committed()).sum(), takes: self.takes.load(Ordering::Relaxed), puts: self.puts.load(Ordering::Relaxed), evictions: self.evictions.load(Ordering::Relaxed) }
    }

    /// Drop every parked slot (the tests; a process never needs it).
    pub fn clear(&self) {
        self.entries.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }
}

impl Node {
    /// D1507: `record` applied to a parked Node's sources and the worker slots it keeps (`Park::release_rel`).
    pub(crate) fn release_recorded(&mut self, record: &Released, doomed: &mut Vec<Box<dyn core::any::Any>>) {
        self.for_each_host(|h| {
            if let crate::node::HostRef::Source(s) = h {
                record.each(|rel| doomed.extend(s.release_rel(rel)));
            }
        });
        for w in self.workers.iter_mut() {
            release_slot(&mut w.inner, record, doomed);
        }
    }

    /// `width` worker slots: the depot's for `salt` when it has them, the rest built (alloc: Node); `None` keeps them with the Node.
    pub fn provision(&mut self, width: u16, derive: &mut dyn Derive, salt: Option<u64>) -> crate::Result<()> {
        if self.workers.is_empty() && width > 0 {
            if let Some(salt) = salt {
                let key = (self.program().key, salt);
                // D4768: before the take: a removal after this reads as since at the put
                self.depot_seen = REMOVED_LOG.current();
                if let Some(slots) = DEPOT.take(key) {
                    self.workers = slots;
                    // C8: the set's arenas leave the depot for this backend's session (the release parked them)
                    #[cfg(not(target_family = "wasm"))]
                    for w in self.workers.iter() {
                        w.arena.charge_here();
                    }
                    // RESIDUE: and its sources' kept heap leaves the depot's holder for the taking statement
                    let (to, release) = memledger::charge::context();
                    for w in self.workers.iter_mut() {
                        w.inner.for_each_host(&mut |h| {
                            if let crate::node::HostRef::Source(s) = h {
                                s.unpark();
                                s.transfer_held(to, release);
                            }
                        });
                    }
                }
                self.depot_key = Some(key);
            }
        }
        let spill = &mut *self.spill as *mut crate::mem::spill::SpillTable;
        let owner = self.owner_ptr();
        for (i, slot) in self.workers.iter_mut().enumerate() {
            slot.inner.spill_index = i + 1;
            slot.inner.spill = spill;
            slot.inner.block.owner = gravity_abi::word(owner);
        }
        self.spill.provision(self.workers.len() + 1);
        while self.workers.len() < width as usize {
            let mut slot = WorkerSlot::new(&self.program().clone(), derive, owner)?;
            slot.inner.spill_index = self.workers.len() + 1;
            slot.inner.spill = spill;
            self.spill.provision(self.workers.len() + 2);
            self.workers.push(slot); // alloc: Node
        }
        Ok(())
    }

    /// At the statement's end the slots go back to the depot detached (their readers dropped at `reset`); the Node keeps none.
    pub(crate) fn release_workers(&mut self) {
        let Some(key) = self.depot_key.take() else { return };
        if self.workers.is_empty() {
            return;
        }
        for w in self.workers.iter_mut() {
            debug_assert!(w.binding.is_none() && w.warm.is_none(), "a slot released while attached or warm");
            w.inner.spill = core::ptr::null_mut();
            w.state_ptr = core::ptr::null_mut();
            w.state_bytes = 0;
            w.state_arm = 0;
            w.state_stale = false;
            w.inner.pinned = 0;
            w.inner.chunk_next = 0;
            w.inner.block.owner = gravity_abi::word(core::ptr::null_mut());
            // C8 (D4087): the set's kept cursors' part uses leave the backend's session for the depot's; RESIDUE: and its
            // sources' kept heap leaves the backend's account for the slot depot's holder (its own account, SLOT_DEPOT)
            w.inner.for_each_host(&mut |h| {
                if let crate::node::HostRef::Source(s) = h {
                    // D4464: parked as a parked Node's sources are (`Node::park`): its bind-sized buffers given back first.
                    // D4554: under the keep-warm switch they stay with the set for its statement's next run, on the depot's
                    // holder below, given back with the set (the depot's caps and trims, the reclaimer, the idle release)
                    if !mempolicy::keep::keep_warm() {
                        s.park();
                    }
                    s.kept_uses_to(memledger::Owner::Cache(memledger::Holder::Parked), memledger::Release::Evict);
                    s.transfer_held(memledger::Owner::Cache(memledger::Holder::SlotDepot), memledger::Release::Evict);
                }
            });
            // C8: and its arena's pages, so a set evicted or taken by another backend leaves no session holding them
            w.arena.transfer_charge(memledger::Owner::Cache(memledger::Holder::Parked), memledger::Release::Evict);
        }
        DEPOT.put(key, core::mem::take(&mut self.workers), self.depot_seen);
    }

    /// D4554 (`kept_parked`): after `release_workers`, the Node's sources and those of the worker slots no depot park took
    /// move what their unbinds left pending to the session (`Source::settle_kept`): no statement or portal ends holding a
    /// kept cursor's use.
    pub(crate) fn settle_kept(&mut self) {
        self.for_each_host(|h| {
            if let crate::node::HostRef::Source(s) = h {
                s.settle_kept();
            }
        });
        for w in self.workers.iter_mut() {
            w.inner.for_each_host(&mut |h| {
                if let crate::node::HostRef::Source(s) = h {
                    s.settle_kept();
                }
            });
        }
    }

    pub fn worker(&self, ordinal: u16) -> &WorkerSlot {
        &self.workers[ordinal as usize]
    }
}


#[cfg(test)]
mod release_log_tests {
    //! D1507: the parks' release log (`ReleaseLog`): what a reader applies between its `seen` and `gen`.
    use super::*;

    fn rels(r: &Released) -> Vec<u32> {
        r.rels.iter().copied().filter(|&x| x != 0).collect()
    }

    #[test]
    fn a_reader_gets_the_relations_since_its_generation_once() {
        let log = ReleaseLog::new();
        let mut seen = log.current();
        assert!(log.since(&mut seen).is_none(), "nothing recorded");
        log.note(7);
        log.note(9);
        log.note(7);
        let r = log.since(&mut seen).unwrap();
        assert!(!r.all);
        assert_eq!(rels(&r), vec![7, 9], "each relation once"); // alloc: Cold
        assert!(log.since(&mut seen).is_none(), "applied once");
        log.note(0);
        assert!(log.since(&mut seen).unwrap().all, "relid 0 (a reset) releases every relation");
    }

    #[test]
    fn an_overrun_past_the_ring_releases_every_relation() {
        let log = ReleaseLog::new();
        let mut seen = log.current();
        for i in 0..=LOG_CAP as u32 {
            log.note(100 + i);
        }
        assert!(log.since(&mut seen).unwrap().all, "LOG_CAP + 1 records: the oldest was overwritten");
        let mut seen = log.current();
        for i in 0..(RELEASED_CAP as u32 + 1) {
            log.note(500 + i);
        }
        assert!(log.since(&mut seen).unwrap().all, "more distinct relations than a record holds");
    }

    #[test]
    fn a_generation_not_stored_yet_releases_every_relation_never_skips() {
        let log = ReleaseLog::new();
        let mut seen = log.current();
        log.note(7);
        // a writer between its fetch_add and its store: gen moved, the word still carries an older tag
        log.gen.fetch_add(1, Ordering::AcqRel);
        let r = log.since(&mut seen).unwrap();
        assert!(r.all, "the pending generation is covered by a release-all, not skipped as stale");
        assert!(log.since(&mut seen).is_none());
    }

    #[test]
    fn a_record_written_on_another_thread_reaches_this_reader() {
        static LOG: ReleaseLog = ReleaseLog::new();
        let mut seen = LOG.current();
        std::thread::spawn(|| LOG.note(41507)).join().unwrap();
        let r = LOG.since(&mut seen).unwrap();
        assert_eq!((r.all, rels(&r)), (false, vec![41507])); // alloc: Cold
    }

    #[test]
    fn concurrent_writers_are_each_applied_or_covered_by_a_release_all() {
        static LOG: ReleaseLog = ReleaseLog::new();
        let mut seen = LOG.current();
        let writers: Vec<_> = (1..=4u32).map(|t| std::thread::spawn(move || (0..8u32).for_each(|i| LOG.note(t * 100 + i)))).collect();
        let mut got = std::collections::HashSet::new();
        let mut all = false;
        let mut sweep = |seen: &mut u64| {
            if let Some(r) = LOG.since(seen) {
                all |= r.all;
                got.extend(rels(&r));
            }
        };
        while writers.iter().any(|w| !w.is_finished()) {
            sweep(&mut seen);
        }
        writers.into_iter().for_each(|w| w.join().unwrap());
        sweep(&mut seen);
        let want: std::collections::HashSet<u32> = (1..=4u32).flat_map(|t| (0..8u32).map(move |i| t * 100 + i)).collect();
        assert!(all || got == want, "every record applied, or a release-all covered the rest");
        assert_eq!(seen, LOG.current());
    }
}

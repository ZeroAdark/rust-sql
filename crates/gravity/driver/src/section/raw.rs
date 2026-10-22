//! `section`'s unsafe steps (rule R6, the module's raw home): the section's slots and program behind raw pointers, a
//! claim's call into the program, owner regions cut from spare or pinned bytes, the bound worker ctx and binding, and
//! the one-owner header.
#![allow(unsafe_code)]

use gravity_abi::PtrWord;
use gravity_abi::breaker::{HashAgg, OwnerTop};
use gravity_abi::ctx::{FLAG_RESUME, RESUME_NONE};

use crate::node::Ret;

use super::*;

unsafe impl Sync for Open<'_> {}
unsafe impl Send for Open<'_> {}

impl Drop for BusyMark {
    fn drop(&mut self) {
        // SAFETY: the slot outlives the claim (the Node's, detached only by `close_section`).
        unsafe { (*self.0).store(false, Ordering::Release) };
    }
}

/// skewsplit: a claim's cut leaves its `Inner` on every exit of the claim (`claim_on`), an unwind's included.
struct CutOff(*mut crate::node::Inner);

impl Drop for CutOff {
    fn drop(&mut self) {
        // SAFETY: the claim's `Inner`, this thread's until the claim's `BusyMark` drops (after this)
        unsafe { (*self.0).claim_cut = None };
    }
}

impl ReleaseOnUnwind {
    pub(super) fn inner(&mut self) -> &mut crate::node::Inner {
        // SAFETY: the claim's slot, held by this thread until the claim's `BusyMark` drops (after this).
        unsafe { &mut *self.0 }
    }
}

impl Open<'_> {
    pub(super) fn program(&self) -> &Program {
        unsafe { &*self.program }
    }

    pub(super) fn slot(&self, ordinal: u16) -> &mut WorkerSlot {
        assert!(ordinal < self.width, "ordinal {ordinal} of width {}", self.width);
        unsafe { &mut *self.slots.add(ordinal as usize) }
    }

    pub fn run_claim(&self, ordinal: u16, claim: MorselRange) -> Result<Claimed> {
        let slot: *mut WorkerSlot = self.slot(ordinal);
        // SAFETY: the section's slot `ordinal`, this thread's until `_busy` drops
        let busy = unsafe { core::ptr::addr_of!((*slot).busy) };
        assert!(!unsafe { &*busy }.swap(true, Ordering::AcqRel), "ordinal {ordinal} claimed from two threads");
        // cleared on every exit, a contained panic's unwind included: the pool catches the
        // panic and fails the statement, and the Node (its slots) serves the next execution
        let _busy = BusyMark(busy);
        self.claim_on(slot, claim)
    }

    /// D1744: `slot` stays raw across the program's call, whose hosts write the slot's partial (`Home::Worker`, a region
    /// grow) and its `Inner` (`env`): no `&mut` of either is live across it. D1707 / D1712: the call settles through its own
    /// pointer to the slot's binding, and grows through its own pointer to the slot (`attach_own`).
    fn claim_on(&self, raw: *mut WorkerSlot, claim: MorselRange) -> Result<Claimed> {
        let program = self.program();
        // SAFETY: the claim's slot, this thread's (`run_claim`); this borrow ends before the call
        let slot = unsafe { &mut *raw };
        if slot.state_stale {
            // SAFETY: the slot's own partial region, `state_bytes` long; this ordinal runs on one
            // thread at a time. The header prefix is zeroed (`ZERO_PREFIX`): every table zeroes
            // the extent it lays out (pass 3 item 2: a program-constant region too).
            unsafe {
                core::ptr::write_bytes(slot.state_ptr, 0, crate::node::ZERO_PREFIX.min(slot.state_bytes as usize));
                if slot.state_arm != 0 {
                    HashAgg::arm(slot.state_ptr, slot.state_arm);
                }
            }
            slot.state_stale = false;
        }
        slot.set_claim(claim);
        let leg = slot.binding.as_ref().and_then(|b| b.leg);
        if let Some(l) = &leg {
            l.claim_begin(claim);
        }
        let ctx = slot.inner.ctx;
        let owner = slot.owner();
        // SAFETY: the claim's slot, this thread's (`run_claim`); `slot` is not used again before the call
        unsafe { attach_own(raw) };
        // SAFETY: `ctx` is the slot's bound context
        let env = unsafe {
            (*ctx).flags &= !FLAG_RESUME;
            (*ctx).resume = RESUME_NONE;
            crate::hostimpl::env_of(ctx)
        };
        // C5: a revoke asked since this participant's last service cuts its share's region before the claim fills it
        // SAFETY: the claim's own `Inner` (`env`), this thread's; no borrow of it is live
        unsafe {
            crate::mem::grow::revoke_home(&mut *env);
            crate::mem::grow::note_spillable(&mut *env);
        }
        let _release = ReleaseOnUnwind(env);
        // skewsplit (D3320): the pool claim's cut, entered for the claim's length, for its pipeline's driving range site
        // alone (`claimcut`); off at every exit (`_cut` drops before `entered`)
        let drive = program.pipelines[self.pipeline].driving_site;
        let entered = crate::claimcut::enter(drive >= 0);
        // SAFETY: the claim's own `Inner` (`env`), this thread's; no borrow of it is live
        let _cut = unsafe {
            (*env).claim_cut = (entered.on() && drive >= 0).then_some(drive as u32);
            CutOff(env)
        };
        let _run = crate::node::OwnerRun::enter(owner); // D2355
        // G1-B: the regions the claim draws count under its pipeline's breaker kind in the ledger's per-kind split
        let _kind = memledger::with_kind(crate::mem::region::pipeline_kind(program, self.pipeline));
        // D4043: the claim's draws past its W come out of its statement's purse before the pool's capacity
        let _purse = crate::mem::grant::claim_purse(self.purse.as_ref());
        // D4630 (b): the slot's first claim binds its hosts (the open left them for it, `bind_at_claim`), here, on the
        // claiming thread, under the claim's owner, kind and purse; a failed bind fails the claim, as it failed the open
        // SAFETY: the claim's slot, this thread's (`run_claim`); this borrow ends before the call
        if unsafe { (*raw).bind_pending } {
            // SAFETY: as above; no other borrow of the slot is live, and this one ends before the call
            bind_pending(unsafe { &mut *raw })?;
        }
        // D4030: the claim's host code gets the statement's abort byte, as a Node's call does (node/raw.rs `lent`, D3887)
        // and an owner phase (`lend_cancel`, D3902). Without it every long host loop's poll (`cancelpoll::canceled`) on a
        // pool worker read no byte and the poll-gap probe never saw a worker, so a W >= 2 statement's cancel waited out its
        // workers' host calls (a cold part's text dictionary builds: seconds; a memory-watchdog cancel that never
        // answered before the kernel's kill).
        let (v, stopped) = lend_cancel_seen(ctx, || unsafe { program.code_of(self.pipeline).call(ctx) });
        // SAFETY: as above; no borrow of the slot or its `Inner` is live
        let ret = unsafe { crate::node::Inner::settle_call(env, v, stopped) };
        let slot = unsafe { &mut *raw };
        // R3 on every exit: a claim drops its pins here, on the thread that took them (the ordinal's next claim may run on
        // another runtime thread, and the leader detaches the slot); a DONE one too, ended early by a LIMIT (D227)
        slot.inner.for_each_host(&mut |h| h.release());
        let pins = slot.pins();
        if let Some(l) = &leg {
            l.claim_end(ret.is_ok());
            // R1D: what the claim leaves behind poisoned (debug / SCRIBBLE), so a leg that kept a pointer into it reads garbage
            if cfg!(debug_assertions) || crate::sink::scribble() {
                slot.inner.poison_out(program.layout.out_rows);
            }
        }
        slot.rewind(program.layout.out_rows);
        let claimed = match ret? {
            Ret::Done => Claimed::Done,
            Ret::Suspended { .. } => Claimed::Stopped,
            Ret::Recheck { site } => return Err(Error::Recheck(site)),
            // D824: a lazy build's probe runs leader-only (lowerv4 refuses a parallel one by name)
            Ret::BuildWait { consumed } => return Err(Error::BadReturn(gravity_abi::status::build_wait(consumed))),
        };
        if pins != 0 {
            return Err(Error::host("pins held at claim settle"));
        }
        Ok(claimed)
    }

    /// The produced slot's leader region.
    pub fn leader_region(&self) -> &mut [u8] {
        let (ptr, len) = self.leader.get();
        unsafe { core::slice::from_raw_parts_mut(ptr, len) }
    }

    /// Owner `ordinal`'s region: the leader region behind its 64-byte header cut into `width` equal parts, or the chunk
    /// `own_past_grant` pinned for the owners (KN-02 (B)) cut the same way.
    pub fn owner_region(&self, ordinal: u16) -> crate::sink::Region {
        let bytes = self.owner_bytes() as usize;
        let base = match self.owners_base.get() {
            b if b.is_null() => {
                // D3322: a cut of the leader region ends inside it (`owner_within`)
                debug_assert!(gravity_abi::breaker::OWNED_HDR + (ordinal as usize + 1) * bytes <= self.leader.get().1, "owner {ordinal}'s cut of {bytes} B passes the {} B leader region", self.leader.get().1);
                unsafe { self.leader.get().0.add(gravity_abi::breaker::OWNED_HDR) }
            }
            b => b,
        };
        crate::sink::Region { ptr: unsafe { base.add(ordinal as usize * bytes) }, len: bytes }
    }

    /// The chunk `own_past_grant` pinned for the owners (`width` x `owner_bytes`), when it did.
    pub(super) fn owners_chunk(&self) -> Option<&mut [u8]> {
        let base = self.owners_base.get();
        // SAFETY: the chunk own_past_grant pinned, `width` x `owner_bytes` long and pinned to the statement's end; its owner
        // phase has ended (the callers run on the leader after it), so it is this thread's alone
        (!base.is_null()).then(|| unsafe { core::slice::from_raw_parts_mut(base, self.width as usize * self.owner_bytes() as usize) })
    }

    /// The site an owner renders through: the pipeline's driving site on its own worker ctx (the
    /// two-pass form's second pass runs over site 1; its lanes are not site 0's).
    pub(super) fn owner_site(&self, ordinal: u16) -> *mut gravity_abi::Site {
        let site = self.program().pipelines[self.pipeline].driving_site.max(0) as usize;
        unsafe { gravity_abi::ctx::site_ptr(self.slot(ordinal).ctx(), site) }
    }

    /// Owner `ordinal`'s round-2 flush: its round-2 buffers to its round-2 file of the produced slot.
    pub(super) fn round2_flush<'a>(&'a self, merge: &'a dyn Merge, ordinal: u16) -> impl FnMut() -> Result<()> + 'a {
        let region = self.owner_region(ordinal);
        let slot = self.produced_slot();
        let spill: *const crate::mem::spill::SpillTable = &*self.exec.node().spill;
        move || {
            let (Some(slot), Some(r2)) = (slot, merge.round2_partial(region)) else { return Err(Error::Unsupported(UNSUPPORTED_SCRATCH)) };
            // SAFETY: the node's spill table outlives the section; the owner's round-2 buffers are its own.
            unsafe { (*spill).flush_round2(slot, ordinal as usize, r2.ptr) }.map(|_| ())
        }
    }

    /// Slot `o`'s source at `site`, read once the section's claims are over.
    pub(super) fn slot_source(&self, o: u16, site: usize) -> Option<&(dyn crate::source::Source + 'static)> {
        // SAFETY: each slot's ctx is bound for the section; no claim runs, so nothing writes its sources.
        unsafe { crate::source::site_source(self.slot(o).ctx(), site) }
    }
}

/// The one-owner header over `leader`, pointing at the arena region (D485: at the k owners' chunk).
pub(crate) fn owned_header(leader: &mut [u8], region: Owners) -> Result<()> {
    use gravity_abi::breaker::{Owned, OWNED_HDR, TABLE_OWNED};
    if leader.len() < OWNED_HDR {
        return Err(Error::Unsupported(UNSUPPORTED_SCRATCH));
    }
    assert!(region.bytes <= OWNER_WALL && leader.len() <= u32::MAX as usize, "the one-owner header's u32 words");
    // SAFETY: the leader region's first 64 bytes.
    unsafe { *(leader.as_mut_ptr() as *mut Owned) = Owned { nowners: region.k, owner_bytes: region.bytes as u32, limit: leader.len() as u32, pad0: [0; 3], kind: TABLE_OWNED, base: region.base as u64, pad1: [0; 5], cell: core::cell::UnsafeCell::new([]) } };
    Ok(())
}

/// D4630 (b): the hosts of a slot the open left unbound (`bind_pending`), bound on the calling thread under its owner: at
/// the slot's first claim, or (D4695) before an owner renders through the site of a slot no claim reached. The mark clears
/// first: a failed bind fails its claim or owner phase, as it failed the open, and is not bound again.
pub(super) fn bind_pending(slot: &mut WorkerSlot) -> Result<()> {
    if !core::mem::take(&mut slot.bind_pending) {
        return Ok(());
    }
    let b = slot.binding.as_ref().expect("a slot bound at its claim has its binding").as_ref();
    let mut err = None;
    slot.inner.for_each_host_but(slot.skip, &mut |h| {
        if err.is_none() {
            err = h.bind(&b).err();
        }
    });
    err.map_or(Ok(()), Err)
}

pub(super) fn detach(slot: &mut WorkerSlot) {
    if slot.binding.is_none() {
        return;
    }
    // D4630 (b): a slot no claim reached under `bind_at_claim` never bound its hosts: nothing to unbind
    if !core::mem::take(&mut slot.bind_pending) {
        slot.inner.for_each_host(&mut |h| h.unbind());
    }
    let_go(slot);
}

/// D3196 (rearm E1): `detach` with the slot's hosts left bound, warm for `warm` (the next open that matches it binds none).
pub(super) fn detach_warm(slot: &mut WorkerSlot, warm: crate::slots::Warm) {
    if slot.binding.is_none() {
        return;
    }
    let_go(slot);
    // D4630 (b): a slot no claim reached never bound: it stays cold (the next open binds it)
    if !core::mem::take(&mut slot.bind_pending) {
        slot.warm = Some(warm);
    }
}

/// The slot's ctx, partials, grow home, push target and binding cleared (its hosts as they are).
fn let_go(slot: &mut WorkerSlot) {
    // SAFETY: the slot's own identity cell and ctx, the leader thread, no claim running (the section's close or failed open)
    unsafe {
        *slot.inner.identity = gravity_abi::Identity::default();
        let h = &mut *slot.inner.ctx;
        h.params = gravity_abi::word(core::ptr::null());
        h.params_null = gravity_abi::word(core::ptr::null());
        h.cancel = gravity_abi::word(core::ptr::null());
        h.identity = gravity_abi::word(core::ptr::null());
        h.worker_ordinal = 0;
        h.host_lane0 = 0;
        h.width = 1;
    }
    unsafe { *slot.inner.partials = Partials::NONE };
    slot.inner.home = crate::mem::grow::Home::None;
    // C5: its share no longer counts as its statement's spillable bytes
    // SAFETY: the slot's own `Inner`, its home just cleared
    unsafe { crate::mem::grow::note_spillable(&mut slot.inner) };
    slot.inner.detach();
    slot.binding = None;
}

/// D1707 (D1706 (b) on the workers): the slot's binding attached for one claim through a pointer taken from the claim's own
/// `raw`, the one the call's settle (`add_rows`) writes through. The pointer `open_section` attached came from its bind-time
/// borrow of the slot: every later reborrow of the slot (`run_claim`'s, a claim's, `partial`'s) reads the binding under it,
/// so once a claim's settle wrote through it the ordinal's next claim wrote through a frozen pointer. The push target stays
/// the one `open_section` bound (`Inner::attach`); a worker's call runs no nested call, so nothing is put back.
/// D1712: the grow home too (`Home::Worker`, which `open_section` took from the same borrow): a claim's region grow
/// (`slots::grow_partial`) writes the slot's region fields through it.
/// # Safety
/// `raw` is a slot `open_section` bound, held by this thread (`run_claim`), and no borrow of it is used again before the
/// call it attaches for.
unsafe fn attach_own(raw: *mut WorkerSlot) {
    // the slot's `inner` field alone (its `Inner` lives in the slot's arena), disjoint from the binding borrowed below
    let inner = &mut (*raw).inner;
    if let Some(b) = (*raw).binding.as_mut() {
        let b: *mut WorkerBinding<'static> = b;
        inner.attach_raw(b as *mut (dyn crate::binding::Attached + 'static));
    }
    if matches!(inner.home, crate::mem::grow::Home::Worker(_)) {
        inner.home = crate::mem::grow::Home::Worker(raw);
    }
}

/// A table header's first four u32 words (cap, len, third word, byte 12), for the phase-2 failure counters.
pub(super) fn head_words<'a>(leader: crate::sink::Region) -> &'a [u32] {
    // SAFETY: the destination's header is a table header whatever its kind (cap, len, third word, byte 12).
    unsafe { core::slice::from_raw_parts(leader.ptr as *const u32, 4) }
}

/// A round-2 partial's bytes (`Merge::round2_partial`: the owner's own buffers, idle while they are read).
pub(super) fn bytes_of<'a>(r: crate::sink::Region) -> &'a [u8] {
    // SAFETY: the region a merge names as its round-2 partial, `r.len` bytes from `r.ptr`.
    unsafe { core::slice::from_raw_parts(r.ptr, r.len) }
}

/// D3902: `f` (an owner phase, between kernel calls) with the bound ctx's abort byte lent to the host code it runs, as
/// node/raw.rs `lent` lends it to a kernel call's (D3887): the pool's wait for a draw past the grant (`take_wait`) and the
/// other long host loops stop on the statement's cancel (`cancelpoll::canceled`). C9: a raise or a host's error after such
/// a stop is the statement's cancel when the stop is still the statement's to raise (`crate::stopped_error`): an owner
/// fold's hosted combine (merge.rs `FOLD_HOST`), an owner reduce's raise, the leader's fold of a partial.
pub(super) fn lend_cancel<T>(ctx: *mut gravity_abi::Ctx, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let (r, stopped) = lend_cancel_seen(ctx, f);
    match r {
        Err(e) if stopped => Err(crate::stopped_error(cancel_of(ctx), e)),
        r => r,
    }
}

/// `lend_cancel`'s lending, with whether a host loop of `f` stopped for the abort bit (`cancelpoll::scoped_seen`): a claim's
/// kernel call settles with it (node/raw.rs `settle_call`).
pub(super) fn lend_cancel_seen<R>(ctx: *mut gravity_abi::Ctx, f: impl FnOnce() -> R) -> (R, bool) {
    let cancel = cancel_of(ctx) as *const core::sync::atomic::AtomicU8;
    if cancel.is_null() {
        return (f(), false);
    }
    // SAFETY: the callers pass a ctx bound for the section or the statement (`cancel_of`): its abort byte outlives the call
    cancelpoll::scoped_seen(unsafe { &*cancel }, f)
}

/// A bound ctx's abort byte (`Ctx::cancel`).
pub(super) fn cancel_of(ctx: *mut gravity_abi::Ctx) -> *const u8 {
    // SAFETY: the callers pass a ctx bound for the section or the statement.
    unsafe { (*ctx).cancel.ptr() }
}

/// Round-2 file `ordinal` of `slot` dropped. Callers hold the stream on their thread alone (`SpillTable::reset_round2`).
pub(super) fn reset_round2(spill: &crate::mem::spill::SpillTable, slot: u8, ordinal: usize) {
    // SAFETY: as the callers say at each call: no other thread reads or writes the stream.
    unsafe { spill.reset_round2(slot, ordinal) }
}

/// An owner's round-2 buffers flushed into round-2 file `ordinal` of `slot`. Callers own both (`SpillTable::flush_round2`).
pub(super) fn flush_round2_at(spill: &crate::mem::spill::SpillTable, slot: u8, ordinal: usize, region: *mut u8) -> Result<u64> {
    // SAFETY: as the callers say at each call: the buffers and the file are this thread's alone.
    unsafe { spill.flush_round2(slot, ordinal, region) }
}

/// A failed phase 2's destination: its header prefix zeroed and, for a share region, armed with its own length.
pub(super) fn rearm(r: &mut [u8], shared: bool) {
    // SAFETY: the leader region, `r.len()` bytes, the leader thread's alone between phase 2 and the serial fold.
    unsafe {
        core::ptr::write_bytes(r.as_mut_ptr(), 0, crate::node::ZERO_PREFIX.min(r.len()));
        if shared {
            HashAgg::arm(r.as_mut_ptr(), r.len() as u32);
        }
    }
}

/// `own_all`'s cut of the spare region: `zeroed` bytes zeroed, the rest of `len` poisoned in debug and scribble runs.
pub(super) fn lay_cut(spare_ptr: *mut u8, len: usize, zeroed: usize) {
    // SAFETY: `spare` is `spare_len` bytes or more, idle for the fold, this thread's alone.
    unsafe {
        if zeroed < len && (cfg!(debug_assertions) || crate::sink::scribble()) {
            core::ptr::write_bytes(spare_ptr.add(zeroed), crate::sink::POISON, len - zeroed);
        }
        core::ptr::write_bytes(spare_ptr, 0, zeroed);
    }
}

/// A pinned pool chunk of `bytes`, its first `zeroed` bytes zeroed (`arena::pin_owner`; null when the pool refuses). Every
/// pin here is an owner region or the owners' chunk: C10 counts one past W and G as SPILL-DEBT's `AggOwnerAll` road
/// (`arena::pin_owner`, D3582).
pub(super) fn pin_zeroed(ctx: *mut gravity_abi::Ctx, bytes: usize, zeroed: usize) -> *mut u8 {
    // SAFETY: the callers pass the node's ctx, bound for the statement.
    unsafe { crate::hostimpl::arena::pin_owner(ctx, bytes, zeroed, false) }
}

/// D3236 (B): the pin at `p` filed as `slot`'s one-owner region (`arena::file_slot_pin`): it goes back when the slot frees.
pub(super) fn file_slot_pin(ctx: *mut gravity_abi::Ctx, slot: Option<u8>, p: *mut u8) {
    // SAFETY: as `pin_zeroed`.
    unsafe { crate::hostimpl::arena::file_slot_pin(ctx, slot, p) }
}

/// D4078 (ADMITFP (A)): the owner region at `p` this owner phase pinned, given back before the split pins its chunk
/// (`arena::give_owner_pin`, at or over `floor`); the bytes given back (0: kept).
pub(super) fn give_owner_pin(ctx: *mut gravity_abi::Ctx, slot: Option<u8>, p: *mut u8, floor: usize) -> u64 {
    // SAFETY: as `pin_zeroed`; the caller reads the region no more (own_all_lent returns the split's owners).
    unsafe { crate::hostimpl::arena::give_owner_pin(ctx, slot, p, floor) }
}

/// D491: a temp file mapped shared for the serial owners past the pool (`arena::map_owners`; null: no file road).
pub(super) fn map_owners(ctx: *mut gravity_abi::Ctx, bytes: usize) -> crate::Result<*mut u8> {
    // SAFETY: as `pin_zeroed`.
    unsafe { crate::hostimpl::arena::map_owners(ctx, bytes) }
}

/// D491: a mapping `map_owners` made, given back (`arena::unmap_owners`).
pub(super) fn unmap_owners(ctx: *mut gravity_abi::Ctx, p: *mut u8) {
    // SAFETY: as `pin_zeroed`; the split that mapped it reruns and reads it no more.
    unsafe { crate::hostimpl::arena::unmap_owners(ctx, p) }
}

/// `pin_zeroed` taking only the pool's room now past W and G (`arena::pin_owner`'s `now`: no wait, no refusal kept), the
/// owners' chunk (`AggOwnerAll` past W and G, as `pin_zeroed`).
pub(super) fn pin_zeroed_now(ctx: *mut gravity_abi::Ctx, bytes: usize, zeroed: usize) -> *mut u8 {
    // SAFETY: as `pin_zeroed`.
    unsafe { crate::hostimpl::arena::pin_owner(ctx, bytes, zeroed, true) }
}

/// Owners 1.. of a chunk of `width` owner regions of `bytes` (`Open::own_past_grant`'s pin, owner 0's prefix zeroed by it):
/// each one's first `zeroed` bytes zeroed, as a one-round cut's.
pub(super) fn zero_owners(base: *mut u8, width: usize, bytes: usize, zeroed: usize) {
    // bounded("small-fixed: one owner's header prefix (at most ZERO_PREFIX bytes) per owner, at most SLOTS (256) owners")
    for o in 1..width {
        // SAFETY: owner `o`'s region inside the chunk just pinned (`width` x `bytes`), this thread's alone until the owner phase runs.
        unsafe { core::ptr::write_bytes(base.add(o * bytes), 0, zeroed) };
    }
}

/// The room a pin may draw (`arena::pin_room`).
pub(super) fn pin_room(ctx: *mut gravity_abi::Ctx) -> Option<u64> {
    // SAFETY: as `pin_zeroed`.
    unsafe { crate::hostimpl::arena::pin_room(ctx) }
}

/// D4078: the statement's one-owner wall, at most `format` (`arena::owner_wall_of`).
pub(super) fn owner_wall_of(ctx: *mut gravity_abi::Ctx, format: usize) -> usize {
    // SAFETY: as `pin_zeroed`.
    unsafe { crate::hostimpl::arena::owner_wall_of(ctx, format) }
}

/// The room one pin may draw (`arena::pin_room_one`).
pub(super) fn pin_room_one(ctx: *mut gravity_abi::Ctx) -> Option<u64> {
    // SAFETY: as `pin_zeroed`.
    unsafe { crate::hostimpl::arena::pin_room_one(ctx) }
}

/// The owner header `open` wrote at a probe region's start.
pub(super) fn owner_top<'a>(r: crate::sink::Region) -> &'a OwnerTop {
    // SAFETY: the probe's first bytes, the header `open` wrote (a merge with no owner form leaves it zero: n = 0).
    unsafe { &*(r.ptr as *const gravity_abi::breaker::OwnerTop) }
}

/// D3111: an opened one-round owner's state before a partition's fold, its header and kept bytes `[0, end)` (`end` a
/// multiple of 8 at most its table offset), into `snap`.
pub(super) fn owner_save(r: crate::sink::Region, end: usize, snap: &mut Vec<u64>) {
    debug_assert!(end % 8 == 0 && end <= owner_top(r).table_off as usize && end <= r.len);
    snap.clear();
    // SAFETY: the owner's first `end` bytes (its header and kept state, below its table space), this thread's alone and
    // 8-aligned as every owner region (`OwnerTop::view`).
    snap.extend_from_slice(unsafe { core::slice::from_raw_parts(r.ptr as *const u64, end / 8) }); // alloc: Cold
}

/// D3111: `snap` (`owner_save`'s) laid over the start of `r`, an owner the same form just opened in a grown region: its
/// layout words (`table_off`, `round2_off`, `resume`, `n`) stay the open's. False, and `r` untouched, when the open's
/// `n` differs from the saved one or its table space starts below the saved bytes' end.
pub(super) fn owner_restore(r: crate::sink::Region, snap: &[u64]) -> bool {
    let t = owner_top(r);
    let (table_off, round2_off, resume, n) = (t.table_off, t.round2_off, t.resume, t.n);
    if snap.len() * 8 < gravity_abi::breaker::OWNERTOP_HDR || snap.len() * 8 > table_off as usize {
        return false;
    }
    // SAFETY: the saved header's words (an `OwnerTop`, 8-aligned in the words).
    if unsafe { &*(snap.as_ptr() as *const OwnerTop) }.n != n {
        return false;
    }
    // SAFETY: `r`'s first `8 * snap.len()` bytes, below its table space (checked), this thread's alone; then its header.
    unsafe {
        core::ptr::copy_nonoverlapping(snap.as_ptr(), r.ptr as *mut u64, snap.len());
        let h = &mut *(r.ptr as *mut OwnerTop);
        (h.table_off, h.round2_off, h.resume, h.n) = (table_off, round2_off, resume, n);
    }
    true
}

/// Owner `o`'s region of a split chunk, `bytes` long, its header prefix zeroed as a one-round cut's.
pub(super) fn split_owner(base: *mut u8, o: u32, bytes: usize) -> crate::sink::Region {
    // SAFETY: owner `o`'s bytes of the chunk, this thread's alone; its header prefix zeroed as a one-round cut's.
    let region = crate::sink::Region { ptr: unsafe { base.add(o as usize * bytes) }, len: bytes };
    unsafe { core::ptr::write_bytes(region.ptr, 0, crate::node::ZERO_PREFIX.min(bytes)) };
    region
}

/// The statement's reservation the Exec holds while its section binds (`Inner::stmt`).
pub(super) fn stmt_of<'a>(node: &crate::node::Node) -> Option<&'a crate::mem::grant::Reservation> {
    // SAFETY: as `Inner::stmt`: the Exec holds the reservation while its section binds (leader thread).
    unsafe { node.stmt_ptr().as_ref() }
}

/// A forked worker binding kept in its slot for the section: the slot outlives it (`detach` drops it at the close).
pub(super) fn erase(wb: WorkerBinding<'_>) -> WorkerBinding<'static> {
    // SAFETY: the binding is dropped by `detach` before the leader's binding it borrows from ends.
    unsafe { core::mem::transmute(wb) }
}

/// KEYED-22: worker ctx `from`'s per-site rows (`rows_in`, `rows_out`) added to the node ctx `to`'s and zeroed in `from`.
pub(super) fn fold_site_rows(from: *mut gravity_abi::Ctx, to: *mut gravity_abi::Ctx, nsites: usize) {
    if from.is_null() || from == to {
        return;
    }
    // bounded("other: two counters per site of the program, a site per plan node it lowered (the plan's own size)")
    for i in 0..nsites {
        // SAFETY: two ctxs `build_image` laid for one program (`nsites` sites each), a slot's and the node's; the leader
        // thread after the section's claims, no worker running.
        unsafe {
            let (f, t) = (&mut *gravity_abi::ctx::site_ptr(from, i), &mut *gravity_abi::ctx::site_ptr(to, i));
            (t.rows_in, t.rows_out) = (t.rows_in.wrapping_add(f.rows_in), t.rows_out.wrapping_add(f.rows_out));
            (f.rows_in, f.rows_out) = (0, 0);
        }
    }
}

/// EXPLAIN ANALYZE's counter cells (an instrumented program's `cells` of `Program.instr`) of a worker slot summed into the
/// node's and zeroed in the slot, as `fold_site_rows` sums the site rows: each participant counts into its own image's
/// region, so the node holds the execution's counts at any width.
pub(super) fn fold_instr(from: *mut u64, to: *mut u64, cells: usize) {
    if from.is_null() || to.is_null() || from == to {
        return;
    }
    // bounded("other: one counter cell per EXPLAIN ANALYZE counter of the program, a few per plan node (the plan's own size)")
    for i in 0..cells {
        // SAFETY: two regions `build_image` laid for one program (`cells` cells each), a slot's and the node's; the leader
        // thread after the section's claims, no worker running.
        unsafe {
            *to.add(i) = (*to.add(i)).wrapping_add(*from.add(i));
            *from.add(i) = 0;
        }
    }
}

/// A section is one start of the nodes its pipeline runs, however many claims opened it (each claim's open counts a loop,
/// lowerv4 `instr::count_open`): every loops cell (`Program.instr` counter 1) the section moved is set to its value at the
/// section's open plus one.
pub(super) fn settle_loops(to: *mut u64, cells: &[(i32, u8)], at_open: &[u64]) {
    if to.is_null() || at_open.len() != cells.len() {
        return;
    }
    // bounded("other: one counter cell per EXPLAIN ANALYZE counter of the program, a few per plan node (the plan's own size)")
    for (i, (&(_, ctr), &v0)) in cells.iter().zip(at_open).enumerate() {
        // SAFETY: the node's region of `cells.len()` cells (`build_image`); the leader thread after the section's claims.
        unsafe {
            if ctr == 1 && *to.add(i) != v0 {
                *to.add(i) = v0 + 1;
            }
        }
    }
}

/// D3014 S2': a section a rerun runs (a nested run's member) reads the executor parameters its owner wrote for this rescan
/// in the program's frame words (the words site's key cells, `Px::frame_word`) on the leader's ctx: each worker's ctx gets
/// a copy of them at the section's open, before any claim (a worker kept the words of the statement's start: a LATERAL
/// bound read as 0 there).
pub(super) fn copy_frame_words(program: &gravity_program::Program, from: *mut gravity_abi::Ctx, to: *mut gravity_abi::Ctx) {
    let Some((ws, n)) = program.sites.iter().enumerate().find_map(|(x, s)| match s.kind {
        gravity_program::SiteKind::Words { n } => Some((x, n as usize)),
        _ => None,
    }) else {
        return;
    };
    if from.is_null() || to.is_null() {
        return;
    }
    // SAFETY: both bound ctxs lay site `ws` with `n` key cells (node/image.rs); the leader thread, no worker running yet
    unsafe {
        let (f, t) = ((*gravity_abi::ctx::site_ptr(from, ws)).keys.ptr() as *const u64, (*gravity_abi::ctx::site_ptr(to, ws)).keys.ptr());
        if !f.is_null() && !t.is_null() && f != t as *const u64 {
            core::ptr::copy_nonoverlapping(f, t, n);
        }
    }
}

/// Worker `i`'s ctx bound: the binding's params, abort byte and identity, its ordinal, the width, the arm flag, its budget,
/// and (D3862) the hosted lane worker 0 takes (`host_lane0`). At the top level that is lane 0, which the leader and worker 0
/// share, as before. Inside a nested run (D1036: the leader's ordinal, `leader` the Node's ctx, is a nested lane) lane 0
/// holds the outer row's pending hosted results (a ltrim's text it emits after the run), so worker 0 takes lane `width`
/// instead: a pool lane no worker of this section takes (they are 0..width), and no suspended row holds one (a nested run
/// starts only from a row on the leader, and a section runs whole before its caller goes on). A pool lane, not the run's
/// nested lane: worker 0 runs on a pool thread, where only a pool lane's twins may be armed (D2983).
pub(super) fn bind_ctx(inner: &crate::node::Inner, b: &WorkerBinding<'static>, i: u16, width: u16, arm: u32, budget: u64, leader: *mut gravity_abi::Ctx) {
    // SAFETY: the slot's own identity cell and bound ctx, written on the leader thread before any claim; the Node's own
    // ctx (`leader`), alive while the node is, read on that thread.
    unsafe {
        *inner.identity = b.parts.abi_identity();
        let nested = !leader.is_null() && (*leader).worker_ordinal != 0;
        let h = &mut *inner.ctx;
        h.host_lane0 = if nested { width } else { 0 };
        h.params = gravity_abi::word(b.params.as_ptr());
        h.params_null = gravity_abi::word(b.param_nulls.as_ptr());
        h.cancel = gravity_abi::word(b.abort_flag.as_ptr());
        h.identity = gravity_abi::word(inner.identity);
        h.worker_ordinal = i;
        h.width = width;
        h.flags = (h.flags & !gravity_abi::ctx::FLAG_ARM_RUN) | arm;
        h.budget_bytes = budget;
    }
}

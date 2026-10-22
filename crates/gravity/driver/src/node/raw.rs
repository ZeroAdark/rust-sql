//! `node`'s unsafe steps (rule R6, the module's raw home): the Node's calls over a raw receiver (D1702 / D1744) and
//! their settle through `env`, the attached binding's erased lifetime (D1706), the image's lay and bind, the bound ctx's
//! words, sites and partials, and the slot regions' views, lays and scatters.
#![allow(unsafe_code)]

use gravity_abi::PtrWord;
use gravity_abi::breaker::HashAgg;
use gravity_abi::ctx::{FLAG_LATER_CLAIM, FLAG_RESUME, RESUME_NONE};
use gravity_abi::status::Status;

use crate::mem::spill::region_live;

use super::*;

impl Drop for AttachBack {
    fn drop(&mut self) {
        // SAFETY: the node `Node::attach_raw` was given, alive while this guard lives (its contract)
        unsafe { (&mut *self.0).inner.attached = self.1 }
    }
}

impl Node {
    /// The leader region of slot `slot`, started small (`RegionBuf::is_laid`), committed in place up to `want`
    /// bytes (at most its reservation) before an owner phase cuts its owners from it, in one exact draw (W, G, else the
    /// pool directly, waiting for its room: `grow::commit_exact`). Its (base, bytes) after: as before when it is no laid
    /// reservation, already holds `want`, or the pool stayed short (the owner phase then takes its road as before).
    pub(crate) fn grow_leader_in_place(&mut self, slot: u8, want: u64) -> (*mut u8, usize) {
        let k = slot as usize;
        let (ptr, len, laid, top) = {
            let r = &mut self.slots[k];
            // a slot's length is a u32: the reservation's last page (MAX_REGION is 4 GiB) stays out
            (r.ptr, r.bytes as u64, r.buf.is_laid() && r.ptr == r.buf.as_mut_ptr(), (r.buf.reserve() as u64).min(u32::MAX as u64 & !0xffff))
        };
        if !laid || want <= len {
            return (ptr, len as usize);
        }
        let new = (crate::park::region_bytes(want as usize) as u64).min(top);
        // the owners' commit refused (`owner_refuses`): the owner phase takes its road as when the pool is short
        if new > len && self.owner_refuses(Some(slot)) {
            return (ptr, len as usize);
        }
        let inner = &mut *self.inner;
        let r = &mut self.slots[k];
        // SAFETY: the statement's reservation the Exec keeps alive while it holds this Node (`Inner::stmt`)
        let stmt = unsafe { inner.stmt.as_ref() };
        // SAFETY: the leader's own slot region, on the leader's thread, no producer writing it (the section's jobs ended)
        // one exact draw of the owners' counted need (`grow::commit_exact`), never the past-G grow road
        if new > len && unsafe { crate::mem::grow::commit_exact(&mut r.buf, r.ptr, new, Some(&mut inner.w), stmt) } {
            r.bytes = new as u32;
        }
        (r.ptr, r.bytes as usize)
    }

    /// The bytes of the temp files this execution's owners mapped in place of pool chunks (`arena::map_owners`,
    /// `pin_mapped`) and still hold: its owner phases' spill, counted beside its spill files' bytes in the statement's
    /// report.
    pub fn owner_mapped_bytes(&self) -> u64 {
        self.inner.owner_maps.iter().map(|m| m.len() as u64).sum()
    }

    /// The owner phase's gate, as a hash aggregate's T1 floor commit has one: whether an owner draw for produced slot
    /// `slot` (a one-owner pin, the pooled owners' chunk, the leader's commit in place: any region not cut from the idle
    /// leader region) is refused by the slot's `Fault` (the forced-spill test fault, a revoke's cut). A refused draw takes
    /// the owners' temp-file road (`arena::map_owners`), as one the pool refuses does: the owner phase's spill exit. A
    /// load and a branch while no cause is on.
    pub(crate) fn owner_refuses(&self, slot: Option<u8>) -> bool {
        let Some(r) = slot.and_then(|s| self.slots.get(s as usize)) else { return false };
        if !r.fault.get().armed() {
            return false;
        }
        let refused = crate::mem::grow::with_fault(&r.fault, |f| f.refuse());
        crate::section::OWN_GATED.fetch_add(refused as u64, core::sync::atomic::Ordering::Relaxed);
        refused
    }

    /// V4R-121 (exec v4 MergeAppend's held arm runs): the sort run at `run` put to `slot`'s stream as the leader's run
    /// (owner 0), then cleared, as a kernel's `SPILL_RUN_PUT`; the run's index.
    /// # Safety
    /// `run` is a live sort run in its put form (dense rows, var records compacted), not touched by any thread meanwhile.
    pub unsafe fn put_sort_run(&mut self, slot: u8, run: *mut u8) -> Result<u32> {
        self.spill.run_put(slot, 0, run)
    }

    /// D4291: slot `slot`'s sealed fixed-width sort run (`sealed_fixed_run`, checked by the caller, `Node::own_spillable`)
    /// put to its stream as the leader's run (owner 0, the kernel's own `SPILL_RUN_PUT` road: the stream's temp file). The
    /// caller lays its region again and gives back its bytes past its opened length (`Node::put_sealed_sorts`). The run's
    /// index.
    pub(crate) fn put_own_run(&mut self, slot: u8) -> Result<u32> {
        let ptr = self.slots[slot as usize].ptr;
        // SAFETY: `sealed_fixed_run` held at the caller: the run's rows are its put form (dense, no var copies), and no
        // reader has begun (each consumer of the slot is unopened); the leader thread between calls
        unsafe { self.spill.run_put(slot, 0, ptr) }
    }

    /// The node's counter region (`ProgramBlock.instr`), null for a plain program: a section's close sums each worker
    /// slot's cells into it (`section/raw.rs` `fold_instr`).
    pub(crate) fn instr_ptr(&self) -> *mut u64 {
        self.inner.block.instr.ptr()
    }

    /// EXPLAIN ANALYZE's counter cells of the execution so far (`Program.instr` order); empty for a plain program.
    pub fn instr_cells(&self) -> &[u64] {
        let p = self.inner.block.instr.ptr();
        if p.is_null() {
            return &[];
        }
        // SAFETY: an exec region of `program.instr.len()` cells in the node's arena (`build_image`).
        unsafe { core::slice::from_raw_parts(p, self.program.instr.len()) }
    }

    /// D3500: this recheck node's counts the statement keeps, added into the stopped node's counter region `to` (its
    /// `instr_ptr`: a node of the same program, so the same `Program.instr` cells). PostgreSQL's EvalPlanQual runs the plan
    /// again in its own tree, whose counters EXPLAIN never prints: so the recheck's scans, joins and builds count nothing
    /// here. But the MERGE actions the recheck leads to run on the statement's ModifyTable (ExecMergeMatched /
    /// ExecMergeNotMatched: mt_merge_inserted, _updated, _deleted, lowerv4 `Ctr::MergeInserted` .. `MergeDeleted`, 15 to
    /// 17), and a MATCHED row that no longer joins goes on as two rows (its target row's BY SOURCE action, its source row's
    /// NOT MATCHED one), which ExecMergeMatched counts on the outer plan: the rows the recheck handed the actions
    /// (`Ctr::MergeTaken`, 29) beyond its one row go to the node's `Ctr::MergeSplit` cell (30), exec's to print.
    pub(crate) fn fold_recheck(&self, to: *mut u64) {
        let (cells, from) = (&self.program.instr, self.instr_cells());
        if to.is_null() || from.len() != cells.len() {
            return;
        }
        let add = |i: usize, v: u64| {
            // SAFETY: the stopped node's region of `cells.len()` cells (`build_image`, the same program), on the leader's
            // thread: its run is stopped at the recheck, no section of it running.
            unsafe { *to.add(i) = (*to.add(i)).wrapping_add(v) }
        };
        for (i, (&(node, ctr), &v)) in cells.iter().zip(from).enumerate() {
            match ctr {
                15..=17 => add(i, v),
                29 if v > 1 => {
                    if let Some(j) = cells.iter().position(|&c| c == (node, 30)) {
                        add(j, v - 1);
                    }
                }
                _ => {}
            }
        }
    }

    pub fn site_pos(&self, i: usize) -> ScanPos {
        unsafe { (*site_ptr(self.inner.ctx, i)).pos }
    }

    /// D3580: the row the statement's emit pipeline last took from the source site it drives, as the last call left it:
    /// C execCurrentOf's reading of the scan node's current tuple, for a WHERE CURRENT OF over a served portal with no
    /// lock. A stop writes back the site's cursor after its last row (lowerv4 frame/stop.rs `write_back`), so the row is
    /// the window's at the iteration index before it, through the window's selection, at its offset lane, else in order
    /// from the base (`source::Cursor::pos_of`'s rule); a probe window's base is the row. `NONE` when no emit pipeline
    /// drives a source, its site has taken no row of its window or is at its end, or two emits hold different rows.
    pub fn taken(&self) -> ScanPos {
        let program = &self.program;
        let mut found = ScanPos::NONE;
        for k in 0..program.pipelines.len() {
            let s = program.pipelines[k].driving_site;
            if s < 0 || !crate::pull::is_emit(program, k) || !program.sites.get(s as usize).is_some_and(SiteDesc::is_source) {
                continue;
            }
            let row = taken_row(site_ref(self.inner.ctx, s as usize));
            if !row.is_valid() {
                continue;
            }
            if found.is_valid() && found != row {
                return ScanPos::NONE;
            }
            found = row;
        }
        found
    }

    /// KEYED-22: site `i`'s rows out this execution, every worker's included (`close_section` folds a section's slots in).
    pub fn site_rows_out(&self, i: usize) -> u64 {
        assert!(i < self.program.sites.len(), "site {i} of {}", self.program.sites.len());
        // SAFETY: one of the program's sites, laid out by `build` in the node's ctx; read on the leader thread.
        unsafe { (*site_ptr(self.inner.ctx, i)).rows_out }
    }

    /// Breaker slot `slot`'s region: the leader's table (the merge target, what a
    /// consumer reads, what a serial producer fills). Empty until a share-budgeted slot opens.
    pub fn slot_region(&self, slot: u8) -> &mut [u8] {
        let r = &self.slots[slot as usize];
        if r.ptr.is_null() {
            return &mut [];
        }
        unsafe { core::slice::from_raw_parts_mut(r.ptr, r.bytes as usize) }
    }

    /// After producer `p` ran: the seal (`seal_spill`), or for an arm that continues (D393b) its leader's records scattered
    /// into the slot's files (a no-op for an end that does not spill: a fold's record stays for the next arm).
    /// D393b: `slot`'s leader region, when no claim laid a table in it (a section's arm), laid as an empty phase-1 table of
    /// the workers' shape: the run's last arm then owns the files' records from a live table even with no rows of its own.
    pub(crate) fn lay_leader_t1(&mut self, slot: u8, layout: &gravity_abi::breaker::RecordLayout, pbits: u8, bytes: u32) {
        let region = self.slot_region(slot);
        let (ptr, len) = (region.as_mut_ptr(), region.len());
        // SAFETY: the slot's own region, opened (its header prefix zeroed), `len` bytes; the leader thread between claims.
        if len < gravity_abi::breaker::HASHAGG_HDR || unsafe { crate::mem::spill::region_live(ptr, len) } {
            return;
        }
        let limit = len.min(u32::MAX as usize) as u32;
        unsafe { gravity_abi::breaker::T1::at(ptr, layout, pbits, bytes, limit) };
    }

    pub fn identity(&self) -> Identity {
        unsafe { *self.inner.identity }
    }

    /// D1706: `b`, a call's own pointer to the bound binding, attached for the call (`Inner::attach_raw`); the one before
    /// back when the guard drops, every exit: a nested run's call (M8, through `Entry`'s current execution) attaches its
    /// own over the outer call's, so the outer call's settle writes through its own pointer again, never an ended call's.
    /// # Safety
    /// `this` is a valid node and `b` the binding it is bound to, both alive while the guard lives, and no borrow of the
    /// node is live across the guard but through `this`.
    pub(crate) unsafe fn attach_raw(this: *mut Node, b: *mut Binding<'_>) -> AttachBack {
        let n = &mut *this;
        let before = n.inner.attached;
        n.inner.attach_raw(b as *mut (dyn Attached + '_));
        AttachBack(this, before)
    }

    /// D899: the bound context's parameter words read `params` / `nulls` from now on: a pipeline's own image (its late key
    /// words bound, the parallel runner's `run_with_late`) for its run, then the binding's own again. The caller keeps
    /// both images alive while the context points at them; a section copies the binding's pointers at its launch.
    pub fn repoint_params(&mut self, params: &[u64], nulls: &[u8]) {
        assert_eq!(self.state, NodeState::Bound, "repoint_params on a {:?} node", self.state);
        // SAFETY: the node is bound, so `ctx` is its live context (as `apply` wrote it); only the pointers change
        unsafe {
            let h = &mut *self.inner.ctx;
            h.params = gravity_abi::word(params.as_ptr());
            h.params_null = gravity_abi::word(nulls.as_ptr());
        }
    }

    /// `call_claim`, or (D824, `resume`) the same claim re-entered where its call stopped with `BuildWait`: the resume
    /// word, the arena and the claim the stop left, the sources' pins kept (`Entry::build_wait`).
    pub fn call_claim_at(&mut self, pipeline: usize, claim: MorselRange, resume: bool) -> Result<Ret> {
        // SAFETY: this call's own node
        unsafe { Self::call_claim_at_raw(self, pipeline, claim, resume) }
    }

    /// `call_claim_at` over a raw receiver (D1702, D1744's form): the kernel's hosts may re-enter the runner (an M8 nested
    /// run, through `Entry`'s current execution) and write this node (its nest count, lanes, holds, slots): no `&mut Node`
    /// is live across the call.
    /// # Safety
    /// `this` is a valid node, and no borrow of it is live across the call but through `this`.
    pub(crate) unsafe fn call_claim_at_raw(this: *mut Node, pipeline: usize, claim: MorselRange, resume: bool) -> Result<Ret> {
        let (ctx, program, owner) = {
            let n = &mut *this;
            assert_eq!(n.state, NodeState::Bound, "call on a {:?} node", n.state);
            let ctx = n.inner.ctx;
            let arm = n.arm_flag(pipeline) | n.pending_flags(pipeline);
            let later = if core::mem::take(&mut n.later_claim) { FLAG_LATER_CLAIM } else { 0 };
            (*ctx).flags = arm | later | if resume { FLAG_RESUME } else { 0 };
            (*ctx).claim = claim;
            if !resume {
                (*ctx).resume = RESUME_NONE;
                (*ctx).arena_cur = gravity_abi::word(n.inner.scratch);
                (*ctx).arena_end = gravity_abi::word(n.inner.scratch_end);
                // a fresh Call rewinds the chunk pool with the window (a kernel that grows the arena per
                // claim would otherwise take a chunk per claim on the serial arm: tier 3's gather columns)
                n.inner.chunk_next = n.inner.pinned;
            }
            (ctx, Arc::as_ptr(&n.program), n.inner.block.owner.ptr())
        };
        let _run = OwnerRun::enter(owner); // D2355
        // G1-B: the regions the call draws count under its pipeline's breaker kind in the ledger's per-kind split
        let _kind = memledger::with_kind(crate::mem::region::pipeline_kind(&*program, pipeline));
        (*this).swept.set(false);
        let (v, stopped) = lent(ctx, || (*program).code_of(pipeline).call(ctx));
        // D3586: the call's (and a nested run's inside it) site words may be live again
        (*this).swept.set(false);
        // `ctx` is bound (the call above ran on it)
        let r = Inner::settle_call(crate::hostimpl::env_of(ctx), v, stopped);
        let n = &mut *this;
        n.fold_grows(pipeline);
        // D824: a BuildWait keeps the claim and the sources' pins: the same claim resumes after the build
        if matches!(r, Ok(Ret::BuildWait { .. })) {
            return r;
        }
        (*ctx).claim = MorselRange::ALL;
        // M8 (RT-1): a nested member's stop keeps every pin: the outer call's row still reads its own (`Node::nest`)
        if !matches!(r, Ok(Ret::Done)) && n.nested == 0 {
            n.inner.for_each_host(&mut |h| h.release());
        }
        r
    }

    /// D824: every host's hold released (a `BuildWait`'s kept pins, when the build it waited for failed or was cancelled).
    pub(crate) fn release_hosts(&mut self) {
        self.inner.for_each_host(&mut |h| h.release());
        unsafe { (*self.inner.ctx).claim = MorselRange::ALL };
    }

    pub fn call_pipeline(&mut self, pipeline: usize, call: Call) -> Result<Ret> {
        // SAFETY: this call's own node
        unsafe { Self::call_pipeline_raw(self, pipeline, call) }
    }

    /// `call_pipeline` over a raw receiver, as `call_claim_at_raw` (D1702).
    /// # Safety
    /// `this` is a valid node, and no borrow of it is live across the call but through `this`.
    pub(crate) unsafe fn call_pipeline_raw(this: *mut Node, pipeline: usize, call: Call) -> Result<Ret> {
        let (ctx, program, owner) = {
            let n = &mut *this;
            assert_eq!(n.state, NodeState::Bound, "call on a {:?} node", n.state);
            let ctx = n.inner.ctx;
            n.inner.out.cap = call.out_cap as i64;
            let arm = n.arm_flag(pipeline);
            (*ctx).flags = arm | n.pending_flags(pipeline) | if call.resume { FLAG_RESUME } else { 0 };
            if !call.resume {
                (*ctx).resume = RESUME_NONE;
                (*ctx).arena_cur = gravity_abi::word(n.inner.scratch);
                (*ctx).arena_end = gravity_abi::word(n.inner.scratch_end);
                n.inner.chunk_next = n.inner.pinned;
                // D2378: a pipeline call is no claim: its sources run whole. Run nested inside a claim's call (an eager
                // Init's reader runs by claims, D1035 step 2; its members run at its first read), a member's range source
                // read that claim at its open and scanned only the reader's blocks; `hold` keeps the claim, `reattach` puts it back
                (*ctx).claim = MorselRange::ALL;
            }
            (ctx, Arc::as_ptr(&n.program), n.inner.block.owner.ptr())
        };
        let _run = OwnerRun::enter(owner); // D2355
        // G1-B: the regions the call draws count under its pipeline's breaker kind in the ledger's per-kind split
        let _kind = memledger::with_kind(crate::mem::region::pipeline_kind(&*program, pipeline));
        (*this).swept.set(false);
        let (v, stopped) = lent(ctx, || (*program).code_of(pipeline).call(ctx));
        // D3586: the call's (and a nested run's inside it) site words may be live again
        (*this).swept.set(false);
        // `ctx` is bound (the call above ran on it)
        let r = Inner::settle_call(crate::hostimpl::env_of(ctx), v, stopped);
        (*this).fold_grows(pipeline);
        r
    }
}

/// D3887: a kernel call with the statement's abort byte (the ctx's `cancel`) lent to the host code it runs: a long host
/// loop (a dictionary build, the part cache's trim) polls it per frame / chunk / part (`cancelpoll::canceled`), as the
/// kernel's back-edges poll it between host calls. C9: with whether such a loop stopped for the abort bit (`settle_call`).
/// # Safety
/// `ctx` is bound: its `cancel` is null or a live abort byte.
#[inline(always)]
unsafe fn lent<R>(ctx: *mut gravity_abi::Ctx, call: impl FnOnce() -> R) -> (R, bool) {
    let cancel = (*ctx).cancel.ptr() as *const core::sync::atomic::AtomicU8;
    if cancel.is_null() {
        return (call(), false);
    }
    cancelpoll::scoped_seen(&*cancel, call)
}

impl Inner {
    /// The statement this `Inner` draws for (the leader's while an `Exec` holds it).
    pub(crate) fn stmt(&self) -> Option<&crate::mem::grant::Reservation> {
        // SAFETY: set from the `Exec`'s `Rc<Reservation>` at `Entry::start` and cleared before the Exec gives the Node up
        // (`Entry::release`); the Exec keeps the reservation alive meanwhile.
        unsafe { self.stmt.as_ref() }
    }

    /// The binding's push target into the program block (abi-v2 §2.3), the binding kept for settle.
    pub(crate) fn attach(&mut self, b: &mut (dyn Attached + '_)) {
        let (push, sink) = b.target();
        (self.block.push, self.block.sink) = (gravity_abi::word(push), gravity_abi::word(sink));
        self.pushed = 0;
        let p = b as *mut (dyn Attached + '_);
        let p: *mut (dyn Attached + 'static) = unsafe { core::mem::transmute(p) };
        self.attached = NonNull::new(p);
    }

    /// D1706: the binding kept for settle re-pointed at `b`, a call's own pointer to the binding `attach` bound (the same
    /// push target): the pointer from `Node::apply`'s borrow lost its access to every later `&mut` borrow of the binding.
    /// # Safety
    /// `b` is the bound binding, alive while attached.
    pub(crate) unsafe fn attach_raw(&mut self, b: *mut (dyn Attached + '_)) {
        let p: *mut (dyn Attached + 'static) = core::mem::transmute(b);
        self.attached = NonNull::new(p);
    }

    #[cold]
    fn bare_error(&self) -> String {
        let c = unsafe { &*self.ctx };
        let pos: String = (0..self.descs.len()).map(|i| format!("{}{}:{:?}", if i > 0 { " " } else { "" }, i, unsafe { (*site_ptr(self.ctx, i)).pos })).collect(); // alloc: Cold
        format!("worker {}/{}, claim {}..{}, resume {:#x}, budget {} B, sites [{}]", c.worker_ordinal, c.width, c.claim.lo, c.claim.hi, c.resume, c.budget_bytes, pos) // alloc: Cold
    }

    /// After a kernel returned: the status decoded, the sites' positions recorded on the binding,
    /// an error taken from the host's slot (a host stores what it failed with; a kernel that
    /// reports `ERROR` on its own has none).
    /// The out block (values, nulls, region) and the claim's scratch overwritten with `POISON` at a claim's end (R1D).
    pub(crate) fn poison_out(&mut self, out_rows: u32) {
        let o = &self.out;
        let cells = out_rows as usize * self.out_natts;
        let spans = [(o.values.ptr() as *mut u8, cells * 8), (o.isnull.ptr(), cells), (o.region.ptr(), o.region_cap as usize), (self.scratch, self.scratch_end as usize - self.scratch as usize)];
        for (p, n) in spans {
            if !p.is_null() && n > 0 {
                unsafe { core::ptr::write_bytes(p, crate::sink::POISON, n) };
            }
        }
    }

    /// A call settled through `this`, the node's `env` (`hostimpl::env_of`: the pointer every host writes it through). D1744
    /// (D1358b's raw-receiver form): the send below runs the sink's host, which writes `err` and `pushed` through `env`, so no
    /// `&mut Inner` is live across it (with a `&mut self` receiver release kept the `err` read before the send: a failed
    /// send at a recheck was dropped and the recheck returned).
    /// C9: `stopped`, a host loop of the call stopped for the statement's abort bit (`lent`): its error or raise is the
    /// statement's cancel when the stop is still the statement's to raise (`crate::stopped_error`); D4306: so is its scratch
    /// refusal (kind 2).
    /// # Safety
    /// `this` is a bound `Inner`'s `env`, no borrow of it live.
    pub(crate) unsafe fn settle_call(this: *mut Inner, v: i64, stopped: bool) -> Result<Ret> {
        let mut stored = (*this).err.take();
        // NR-3: a kernel pin refused on a debt home in this call (`hostimpl::arena` pin_unserved)
        let pin_debt = (*this).pin_debt.take();
        let mut status = Status::decode(v);
        // PostgreSQL projects and sends row by row: the rows the program completed before the one that failed reach the
        // receiver first, and an error of theirs (an output function's, the client encoding's) is the statement's (ENTRY E3).
        // A recheck's rows before the rechecked one reach the sink before the row the recheck pushes (scan order)
        if matches!(status, Status::Error | Status::Raised { .. } | Status::Recheck { .. }) && (*this).out.count > 0 && (*this).attached.is_some() {
            if let (Some(push), false) = ((*this).block.push.ptr(), (*this).ctx.is_null()) {
                if push((*this).ctx) < 0 {
                    let failed = (*this).err.take();
                    if failed.is_some() && matches!(status, Status::Recheck { .. }) {
                        status = Status::Error;
                    }
                    stored = failed.or(stored);
                }
            }
        }
        let n = &mut *this;
        let r = match status {
            Status::Ok(0) => Ok(Ret::Done),
            Status::Suspend { out_full } => Ok(Ret::Suspended { out_full }),
            Status::Recheck { site } => Ok(Ret::Recheck { site }),
            Status::BuildWait { consumed } => Ok(Ret::BuildWait { consumed }),
            Status::Error | Status::Raised { .. } if stopped => {
                let e = match status {
                    Status::Raised { mask } => stored.unwrap_or(Error::Raised(mask)),
                    _ => stored.unwrap_or_else(|| Error::host("program reported an error after its host code stopped for the statement's cancel")),
                };
                Err(crate::stopped_error(if n.ctx.is_null() { core::ptr::null() } else { (*n.ctx).cancel.ptr() }, e))
            }
            Status::Error => Err(stored.unwrap_or_else(|| Error::host(format!("program reported an error without a message ({})", n.bare_error())))), // alloc: Cold
            // NR-3: the kernel's own out of memory for a pin the pool and the temp-file road refused (kernels `pin_refused`),
            // raised: exec names the debt home's row beside it (`take_pin_debt`)
            Status::Raised { mask } if stored.is_none() && pin_debt.is_some() => {
                crate::note_pin_debt(pin_debt);
                Err(Error::Raised(mask))
            }
            Status::Raised { mask } => Err(stored.unwrap_or(Error::Raised(mask))),
            Status::Cancelled => Err(stored.unwrap_or(Error::Cancelled)),
            // D4306: a scratch refusal (kind 2) of a call stopped for the abort bit: the kernel read the canceled draw's null
            // (a pin's pool wait or stepped zeroing the cancel ended, `cancelpoll::seen`) as its scratch running out
            Status::Unsupported { kind } if stopped && kind == gravity_abi::status::UNSUPPORTED_SCRATCH => {
                let e = crate::stopped_error(if n.ctx.is_null() { core::ptr::null() } else { (*n.ctx).cancel.ptr() }, stored.unwrap_or(Error::Unsupported(kind)));
                Err(e).inspect_err(|e| crate::note_unsupported(None, e, true))
            }
            // NR-3: a scratch refusal after such a pin: the out of memory `Entry`'s pool_refused names (D2845, its kept
            // refusal's bytes), naming the debt home's row
            Status::Unsupported { kind } if kind == gravity_abi::status::UNSUPPORTED_SCRATCH && stored.is_none() && pin_debt.is_some() && n.stmt.as_ref().is_some_and(|s| s.refused() != 0) => {
                let (s, pool) = (&*n.stmt, &crate::mem::grant::POOL);
                Err(Error::OutOfMemory { bytes: s.refused(), held: pool.held(), capacity: pool.capacity(), why: pin_debt })
            }
            Status::Unsupported { kind } => Err(stored.unwrap_or(Error::Unsupported(kind))).inspect_err(|e| crate::note_unsupported(None, e, true)),
            Status::Fault { site, lane, code, part } => Err(Error::Fault { site, lane, code, part }),
            Status::Stop | Status::Ok(_) => Err(Error::BadReturn(v)),
            Status::Bad(x) => Err(Error::host(format!("bad kernel return {x:#x}: {}", n.bare_error()))), // alloc: Cold
        };
        if let Some(mut p) = n.attached {
            let b = p.as_mut();
            if n.counts_pushed {
                b.add_rows(n.pushed);
            }
            n.pushed = 0;
        }
        // D776: the row a mark names in source site `of`'s own scan (stored before the host call, taken once; a late site,
        // so after `of`'s own) is its position, not the window's base. The positioned sites' own positions are laid at the
        // next read (`positions`), not here: a call that walked every site cost a many-pipeline statement a pass over
        // all its sites per pipeline call
        for m in n.row_marks.iter_mut() {
            let s = &mut *site_ptr(n.ctx, m.site as usize);
            m.taken = if s.pos.is_valid() { core::mem::replace(&mut s.pos, ScanPos::NONE) } else { ScanPos::NONE };
            if !m.of_positioned && m.taken.is_valid() {
                n.positions.get_mut()[m.of as usize] = m.taken;
            }
        }
        n.positions_stale.set(true);
        #[cfg(debug_assertions)]
        n.settle_eager();
        if r.is_err() {
            n.out.count = 0;
            n.out.region_len = 0;
            (*n.ctx).resume = RESUME_NONE;
        }
        r
    }

    /// The sites' positions as the last call settled them (`lay_positions`).
    pub(crate) fn positions(&self) -> &[ScanPos] {
        self.lay_positions();
        // SAFETY: the cell's vector, written only by `lay_positions`, `clear_positions` (`&mut self`) and a settle's
        // non-positioned mark (a call, which no reader holds a slice across: each copies it at the stop it handles)
        unsafe { &*self.positions.get() }
    }

    /// When a call settled since the last lay: each positioned site's own `pos` from the ctx, and over a positioned `of`
    /// the row its mark site's last call took, in site order (the walk every settle made before reads became lazy). A
    /// stop's reader reads them before any other call, and a COPY site's ctx word is changed only after a lay
    /// (`Node::set_copies`), so the lay finds the words the settle left.
    pub(crate) fn lay_positions(&self) {
        if self.positions_stale.replace(false) {
            // SAFETY: as `positions`; no slice of the vector is live (the last one was copied before this settle's call)
            let p = unsafe { &mut *self.positions.get() };
            let mut marks = self.row_marks.iter();
            for i in 0..self.descs.len() {
                if self.descs[i].is_positioned() {
                    // SAFETY: the node's own ctx, a site slot per descriptor
                    p[i] = unsafe { (*site_ptr(self.ctx, i)).pos };
                } else if matches!(self.descs[i].kind, gravity_program::SiteKind::RowOf { .. }) {
                    let m = marks.next().expect("a RowMark per RowOf site");
                    if m.of_positioned && m.taken.is_valid() {
                        p[m.of as usize] = m.taken;
                    }
                }
            }
        }
        #[cfg(debug_assertions)]
        {
            // SAFETY: as `positions`
            let p = unsafe { &*self.positions.get() };
            assert_eq!(p, &self.positions_eager[..], "the positions laid at a read differ from the walk at the call's settle");
        }
    }

    /// The walk every settle made before reads became lazy, into `positions_eager` (debug builds: `lay_positions` checks
    /// each read against it).
    #[cfg(debug_assertions)]
    fn settle_eager(&mut self) {
        let mut marks = self.row_marks.iter();
        for i in 0..self.descs.len() {
            if self.descs[i].is_positioned() {
                // SAFETY: the node's own ctx, a site slot per descriptor
                self.positions_eager[i] = unsafe { (*site_ptr(self.ctx, i)).pos };
            } else if let gravity_program::SiteKind::RowOf { of } = self.descs[i].kind {
                let m = marks.next().expect("a RowMark per RowOf site");
                if m.taken.is_valid() {
                    self.positions_eager[of as usize] = m.taken;
                }
            }
        }
    }

    pub(crate) fn binding(&mut self) -> &mut (dyn Attached + 'static) {
        match self.attached {
            Some(mut p) => unsafe { p.as_mut() },
            None => panic!("host call on an unbound node"),
        }
    }

    /// The node's partials block written (its own and the consumed slots' regions).
    pub(crate) fn set_partials(&self, p: Partials) {
        // SAFETY: the node's own partials block, laid by `build`.
        unsafe { *self.partials = p };
    }

    /// Consumed slot `k`'s region in the partials block's overflow array.
    pub(super) fn set_consumed_more(&self, k: usize, ptr: *mut u8) {
        // SAFETY: `consumed_more` holds `max_consumes` entries, `k` below it.
        unsafe { *self.consumed_more.add(k) = gravity_abi::word(ptr) };
    }

    /// The bound ctx's words from `b`: the identity cell, the params, their nulls and the abort byte (`Node::apply`).
    pub(super) fn bind_words(&self, b: &Binding<'_>) {
        // SAFETY: the node's own identity cell and context, laid by `build`; the leader thread, before any call.
        unsafe {
            *self.identity = b.parts.abi_identity();
            let h = &mut *self.ctx;
            h.params = gravity_abi::word(b.params.as_ptr());
            h.params_null = gravity_abi::word(b.param_nulls.as_ptr());
            h.cancel = gravity_abi::word(b.abort_flag.as_ptr());
            h.identity = gravity_abi::word(self.identity);
        }
    }

    /// The bound ctx's words cleared (`Node::remove`).
    pub(super) fn unbind_words(&self) {
        // SAFETY: as `bind_words`.
        unsafe {
            *self.identity = Identity::default();
            let h = &mut *self.ctx;
            h.params = gravity_abi::word(core::ptr::null());
            h.params_null = gravity_abi::word(core::ptr::null());
            h.cancel = gravity_abi::word(core::ptr::null());
            h.identity = gravity_abi::word(core::ptr::null());
        }
    }

    /// The leader's ordinal back in the ctx (an unwound nested run left its lane there, D1036).
    pub(super) fn lead_ordinal(&self) {
        // SAFETY: the node's own context, alive while the node is.
        unsafe { (*self.ctx).worker_ordinal = 0 };
    }

    /// The ctx's arena window, width and claim back to a fresh statement's (`Node::reset`).
    pub(super) fn rewind_ctx(&self) {
        // SAFETY: the node's own context, alive while the node is; the leader thread, between statements.
        unsafe {
            let h = &mut *self.ctx;
            h.arena_cur = gravity_abi::word(self.scratch);
            h.arena_end = gravity_abi::word(self.scratch_end);
            h.width = 1;
            h.claim = MorselRange::ALL;
        }
    }
}

/// A leader region the driver does not scatter: not live, empty, an owner header or a v4 build (`Node::spill_leader`).
pub(super) fn leader_unscattered(region: &[u8]) -> bool {
    // SAFETY: a `Partitioned` breaker's region is a record or build table (`breakers.md`), `region.len()` bytes.
    unsafe { !region_live(region.as_ptr(), region.len()) || region_len(region.as_ptr()) == 0 || matches!(gravity_abi::breaker::table_kind(region.as_ptr()), gravity_abi::breaker::TABLE_OWNED | gravity_abi::breaker::TABLE_V4BUILD) }
}

/// A worker partial the driver does not scatter: not live, empty or a v4 build (`Node::spill_partial`).
pub(super) fn partial_unscattered(ptr: *mut u8) -> bool {
    // SAFETY: a worker's live partial region (its header names its kind).
    unsafe { !region_live(ptr, usize::MAX) || region_len(ptr) == 0 || gravity_abi::breaker::table_kind(ptr) == gravity_abi::breaker::TABLE_V4BUILD }
}

/// NEW-53: the record bytes of the phase-1 table at `ptr` when it is a live one holding records (`Node::hand_off`), else 0.
pub(super) fn t1_record_bytes(ptr: *mut u8) -> usize {
    if partial_unscattered(ptr) {
        return 0;
    }
    // SAFETY: a live table region (`partial_unscattered`); its header names its kind
    unsafe {
        match gravity_abi::breaker::table_kind(ptr) {
            gravity_abi::breaker::TABLE_T1 => {
                let t = &*(ptr as *const gravity_abi::breaker::T1);
                (t.held() as usize).saturating_mul(t.layout.size() as usize)
            }
            _ => 0,
        }
    }
}

/// NEW-53: the live phase-1 table at `ptr` (`t1_record_bytes` said `bytes`) handed into `slot`'s stream (`SpillTable::hand_own`).
pub(super) fn hand_own(spill: &mut SpillTable, slot: u8, ptr: *mut u8, bytes: usize, memory: bool) -> Result<u64> {
    // SAFETY: the caller checked a live phase-1 table at `ptr` of `bytes` record bytes (`t1_record_bytes`); the leader thread.
    unsafe { spill.hand_own(slot, ptr, bytes, memory) }
}

/// A live table at `ptr` scattered into file `index` of `slot`'s stream at `bits` (`SpillTable::scatter_own`).
pub(super) fn scatter_own(spill: &mut SpillTable, slot: u8, index: usize, ptr: *mut u8, bits: u8) -> Result<u64> {
    // SAFETY: the callers checked a live table at `ptr` (`leader_unscattered` / `partial_unscattered`); the leader thread.
    unsafe { spill.scatter_own(slot, index, ptr, bits) }
}

/// Site `i` of a bound ctx.
pub(super) fn site_at<'a>(ctx: *mut Ctx, i: usize) -> &'a mut gravity_abi::Site {
    // SAFETY: `i` is one of the program's sites, laid out by `build` in the node's ctx; the leader thread.
    unsafe { &mut *site_ptr(ctx, i) }
}

/// D4291: the sort run at the base of a slot region (`ptr`, `extent` bytes readable: its length and what it committed in
/// place): sealed by its sort (its rows sorted in place, `SORT_RUN_SEALED`), not empty, fixed-width (no var copies, so its
/// rows as they lie are its put form; a var run's compaction needs its kernel's var table) and inside the region.
/// No var copies is the var top still at the rows' end (`SortRun::reserve_var` lays each copy below it, past the rows; the
/// kernel's put compacts them, `var_bytes`, which stays 0 until then): a run holding copies put as it lies would leave
/// them behind in the pages the give-back returns, and its reader's by-reference cells would read zeros there (D4291's
/// W8 crash: a zero-length varlena copied as `max(text)`'s first value, mcx's dangling 0x8).
pub(super) fn sealed_fixed_run(ptr: *const u8, extent: usize) -> bool {
    use gravity_abi::breaker::{SortRun, SORTRUN_HDR, SORTRUN_SIZED, SORTRUN_SIZED_WORDS, SORTRUN_VAR_WORDS, TABLE_SORT};
    if ptr.is_null() || extent < SORTRUN_HDR {
        return false;
    }
    // SAFETY: a live slot region of at least a run header, 8-aligned
    let r = unsafe { &*(ptr as *const SortRun) };
    if r.kind != TABLE_SORT || r.pad[0] != super::ownspill::SORT_RUN_SEALED || r.cap == 0 || r.len == 0 || r.len > r.cap || r.row_bytes == 0 {
        return false;
    }
    // its var words past `cap` rows inside the region before any is read: `var_top` and `var_bytes` (a share-wide run's
    // `SORTRUN_VAR_WORDS`), and `region` too in an `at_rows` run's trailer (`SORTRUN_SIZED_WORDS`)
    let words = if r.pad[1] == SORTRUN_SIZED { SORTRUN_SIZED_WORDS } else { SORTRUN_VAR_WORDS };
    if SORTRUN_HDR + r.cap as usize * r.row_bytes as usize + words as usize > extent {
        return false;
    }
    // SAFETY: as above, its var words inside the region; a sealed run's header names the region it was laid in
    let laid = unsafe { SortRun::region_bytes(ptr, r.row_bytes) };
    laid <= extent && r.var_bytes() == 0 && r.var_top() as usize == SORTRUN_HDR + r.cap as usize * r.row_bytes as usize
}

/// A slot region laid for a producer: its header prefix zeroed and, for a share region, armed (`arm`: the share's bytes).
pub(super) fn lay_region(ptr: *mut u8, bytes: u32, arm: Option<u32>) {
    // SAFETY: the slot's own region of `bytes` (arena or buffer), 8-aligned.
    unsafe {
        core::ptr::write_bytes(ptr, 0, ZERO_PREFIX.min(bytes as usize));
        if let Some(a) = arm {
            HashAgg::arm(ptr, a);
        }
    }
}

/// `Node::taken`'s row of one source site: the window row before its cursor, `NONE` before a row or past the window. A
/// window that names each row's tid (a RowId lane: an index walk's batch, a spanning bitmap window) gives it; a page
/// window its offset lane's, else the base's offset plus the row's index; a one-row window its base.
fn taken_row(st: &gravity_abi::Site) -> ScanPos {
    use gravity_abi::ctx::lane;
    let base = st.pos;
    if !base.is_valid() || st.state == site::STATE_EOF || st.cursor == 0 || st.cursor > st.end() {
        return ScanPos::NONE;
    }
    // SAFETY: `cursor - 1 < end()`: the window's selection and lanes, the source's until its next window, which a stopped
    // call resumes inside
    unsafe {
        let r = st.index(st.cursor - 1);
        let mut off = None;
        for k in 0..st.nlanes as usize {
            let l = &*st.lanes.ptr().add(k);
            if l.ptr.ptr().is_null() {
                continue;
            }
            match lane::id_kind(l.id) {
                lane::ID_ROWID => {
                    let w = *(l.ptr.ptr() as *const u64).add(r);
                    return ScanPos::row(base.rel, (w >> 16) as u32, w as u16);
                }
                lane::ID_OFFSET => off = Some(*(l.ptr.ptr() as *const u16).add(r)),
                _ => {}
            }
        }
        match off {
            Some(o) => ScanPos::row(base.rel, base.block, o),
            None if base.is_row() => base,
            None => ScanPos::row(base.rel, base.block, base.off + r as u16 + 1),
        }
    }
}

/// Site `i` of a bound ctx, read only.
pub(super) fn site_ref<'a>(ctx: *mut Ctx, i: usize) -> &'a gravity_abi::Site {
    // SAFETY: `i` is one of the program's sites, laid out by `build` in the Node's ctx.
    unsafe { &*site_ptr(ctx, i) }
}

/// The image's site array (`build_image`): the ctx's `sites` word points right past its header.
pub(super) fn point_sites(ctx: *mut Ctx) {
    // SAFETY: a fresh ctx of the layout's `ctx_size()` bytes in the image's arena, its sites right after the header.
    unsafe { (*ctx).sites = gravity_abi::word((ctx as *mut u8).add(gravity_abi::HEADER_SIZE) as *mut gravity_abi::Site) };
}

/// Site `i`'s windows laid in the image (`build_image`): its values, nulls, keys, key nulls and lanes (`w`, in that
/// order) and its lanes' bound; the site's pointer.
pub(super) fn lay_site(ctx: *mut Ctx, i: usize, w: (*mut Datum, *mut u8, *mut Datum, *mut u8, *mut Lane), lanes_cap: u16) -> *mut gravity_abi::Site {
    // SAFETY: `i` is one of the program's sites, in the ctx `point_sites` laid; the image is the building thread's own.
    unsafe {
        let s = site_ptr(ctx, i);
        (*s).values = gravity_abi::word(w.0);
        (*s).isnull = gravity_abi::word(w.1);
        (*s).keys = gravity_abi::word(w.2);
        (*s).keys_null = gravity_abi::word(w.3);
        (*s).lanes = gravity_abi::word(w.4);
        (*s).lanes_cap = lanes_cap;
        s
    }
}

/// The end of the image region of `bytes` bytes at `p` (`build_image`'s scratch).
pub(super) fn region_end(p: *mut u8, bytes: usize) -> *mut u8 {
    // SAFETY: `p` starts an arena region of `bytes` bytes, so its end is in the same allocation, or one past it.
    unsafe { p.add(bytes) }
}

/// The image's Inner Box taken back from its own pointer (`build_image`, D1706).
pub(super) fn inner_back(env: *mut Inner, al: ArenaRef) -> ArenaBox<Inner> {
    // SAFETY: the Box just let go of, taken back with its allocator
    unsafe { ArenaBox::from_raw_in(env, al) }
}

/// The built image bound (`build_image`): each site's targets (`bind::sites`), then the ctx header's program and out
/// blocks, its arena window (`arena`: cursor, end), the whole claim, width 1 and the site count.
pub(super) fn bind_image(env: *mut Inner, ctx: *mut Ctx, role: Role, arena: (*mut u8, *mut u8), nsites: u16) {
    // SAFETY: `env` is the image's own Inner, its ctx's sites laid and its sources filled (`bind::sites`), and `ctx` its
    // fresh ctx; the image is the building thread's own.
    unsafe {
        crate::bind::sites(env, role);
        let h = &mut *ctx;
        h.program = gravity_abi::word(core::ptr::addr_of_mut!((*env).block));
        h.out = gravity_abi::word(core::ptr::addr_of_mut!((*env).out));
        h.arena_cur = gravity_abi::word(arena.0);
        h.arena_end = gravity_abi::word(arena.1);
        h.claim = MorselRange::ALL;
        h.width = 1;
        h.nsites = nsites;
    }
}

/// The bound ctx's resume word and claim (a parked or held call's).
pub(super) fn resume_claim(ctx: *mut Ctx) -> (i64, MorselRange) {
    // SAFETY: the node's own context, bound, on the leader's thread.
    unsafe { ((*ctx).resume, (*ctx).claim) }
}

/// The bound ctx's resume word and claim set back.
pub(super) fn set_resume_claim(ctx: *mut Ctx, resume: i64, claim: MorselRange) {
    // SAFETY: as `resume_claim`.
    unsafe { ((*ctx).resume, (*ctx).claim) = (resume, claim) };
}

/// The bound ctx's arena window: its cursor and end.
pub(super) fn arena_window(ctx: *mut Ctx) -> (*mut u8, *mut u8) {
    // SAFETY: as `resume_claim`.
    unsafe { ((*ctx).arena_cur.ptr(), (*ctx).arena_end.ptr()) }
}

/// The bound ctx's arena window set: its cursor and end.
pub(super) fn set_arena_window(ctx: *mut Ctx, cur: *mut u8, end: *mut u8) {
    // SAFETY: as `resume_claim`.
    unsafe { ((*ctx).arena_cur, (*ctx).arena_end) = (gravity_abi::word(cur), gravity_abi::word(end)) };
}

/// The bound ctx's call flags.
pub(super) fn ctx_flags(ctx: *mut Ctx) -> u32 {
    // SAFETY: the context is this node's, alive while it is bound; the leader's thread is the only one writing it here.
    unsafe { (*ctx).flags }
}

/// The bound ctx's call flags set.
pub(super) fn set_ctx_flags(ctx: *mut Ctx, flags: u32) {
    // SAFETY: as `ctx_flags`.
    unsafe { (*ctx).flags = flags };
}

/// The bound ctx's worker ordinal.
pub(super) fn worker_ordinal(ctx: *mut Ctx) -> u16 {
    // SAFETY: as `ctx_flags`.
    unsafe { (*ctx).worker_ordinal }
}

/// The bound ctx's worker ordinal set.
pub(super) fn set_worker_ordinal(ctx: *mut Ctx, ordinal: u16) {
    // SAFETY: as `ctx_flags`.
    unsafe { (*ctx).worker_ordinal = ordinal };
}

#[cfg(test)]
mod sealed_run_tests {
    use super::sealed_fixed_run;
    use gravity_abi::breaker::SortRun;

    /// A sealed share-wide run of two 16-byte rows in a 4 KiB region, with `copy` bytes of var copies reserved.
    fn run(buf: &mut [u64; 512], copy: u32) -> *mut u8 {
        let p = buf.as_mut_ptr() as *mut u8;
        // SAFETY: an armed region of 4096 bytes (word 2 = the share), 8-aligned
        unsafe {
            *(p.add(8) as *mut u32) = 4096;
            let r = SortRun::at_share(p, 16);
            r.len = 2;
            if copy != 0 {
                r.reserve_var(copy).expect("room for a copy");
            }
            r.pad[0] = super::super::ownspill::SORT_RUN_SEALED;
        }
        p
    }

    #[test]
    fn a_run_with_var_copies_is_not_its_put_form() {
        let mut buf = [0u64; 512];
        assert!(sealed_fixed_run(run(&mut buf, 0), 4096), "a sealed run with no copy puts as it lies");
        let mut buf = [0u64; 512];
        assert!(!sealed_fixed_run(run(&mut buf, 16), 4096), "a run holding var copies (var_bytes 0 until its kernel's put) is not");
    }
}

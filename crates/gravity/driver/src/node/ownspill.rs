//! D4291 (D4287's fix: a statement spills its finished sort operations before it waits): a pipeline open whose draw
//! past W and G the grant pool refuses puts the statement's own finished sorts to their temp files before it waits.
//!
//! D4287: under auto work_mem a Sort grows in place past its grant to the pool's edge; its run seals in memory (a serial
//! sort keeps its run when it never put one), and the next pipeline's 2 MiB start region, a draw with no spill road, is
//! refused: 53200 where PostgreSQL's sort had written its tapes. The finished run is the statement's own memory and its
//! reader has not begun, so the open is the place to put it: the sort's own put road (`SpillTable::run_put`, the
//! kernel's `SPILL_RUN_PUT`: the stream's temp file, `temp_file_limit`, pgsql_tmp naming, dropped with the stream at the
//! statement's end or error), the region laid again at its opened length and its growth given back (pages and holds).
//! The reader's `begin_in` then finds no sealed run and merges the stream, as after any put: one run, read in its order.
//!
//! The ladder: the depots (no I/O), then the statement's own finished sorts, then GOVWAIT's
//! revoke and wait. While a run qualifies (`own_spillable`) the open's draws past W and G take the depots alone and a
//! refusal ends the open (`Reservation::own_try`, region.rs `own_first`); the runs are put and the open is retried ONCE
//! through the whole ladder. A second refusal is today's plain 53200, and the server log names it.
//!
//! Phase 1 covers a run no reader has begun, in a leader slot, between calls: not a run a reader has begun (the mid-read
//! switch, phase 2), not a var-width run (its put form needs its kernel's var table), not a slot kept for a rerun or a
//! pulled set, and not a no-spill draw refused inside a kernel call (D4287 / D4291 residuals R-a, R-b).

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::{Node, Share};
use crate::{Error, Result};

/// The sealed mark a sort kernel leaves on a run whose rows lie sorted in its region (`SortRun::pad[0]`; kernels
/// ops/sort/rowsort.rs `SEALED`, exported as `gravity_kernels::ops::sort::entries::RUN_SEALED`: D4291's witness asserts
/// they agree).
#[doc(hidden)]
pub const SORT_RUN_SEALED: u8 = 0x5e;

/// D4291's census, process-wide: opens that put their statement's own sorts and retried, the runs put, the bytes given
/// back, and the retries refused again (each a 53200 the server log names).
pub static OWN_SPILL: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

impl Node {
    /// Whether slot `s` holds a run pipeline `p`'s open may put: a live share-budgeted leader slot (not kept for a rerun or a
    /// pulled set, not a lazy build's pending one) whose region is a reservation with bytes to give back past the length
    /// its open asked (`own_keep`), holding a sealed fixed-width sort run (`raw::sealed_fixed_run`) none of whose readers
    /// has begun: each pipeline consuming the slot is unopened this execution, or is `p` itself, and no cut was laid over
    /// it for a reader not opened yet (D4358: merge.cosplit's ranges over its resident rows, `Node::hold_as_laid`).
    fn own_spillable(&self, s: usize, p: usize) -> bool {
        let slot = s as u8;
        let r = &self.slots[s];
        if !r.shared || !self.slot_life.is_live(slot) || self.slot_life.is_kept(slot) || self.slot_life.is_pending(slot) || self.slot_life.is_held_as_laid(slot) || r.ptr.is_null() || r.ptr as *const u8 != r.buf.as_ptr() {
            return false;
        }
        if !r.buf.is_reserved() || r.buf.past_bytes(own_keep(r)) == 0 || !super::raw::sealed_fixed_run(r.ptr, r.buf.len() + r.buf.committed()) {
            return false;
        }
        self.program.pipelines.iter().enumerate().all(|(q, pipe)| q == p || !pipe.consumes.contains(&slot) || !self.is_opened(q))
    }

    /// D4291: pipeline `p`'s open, on the own-spill road when the statement holds a run `own_spillable` names: a refusal
    /// past the depots ends the first try, the runs are put (`put_own_sorts`) and the open runs once more through the
    /// whole ladder. Any other open is `open_pipeline_once` as before.
    pub(crate) fn open_pipeline(&mut self, p: usize, share: Share, images: u32) -> Result<()> {
        if self.inner.stmt().is_none() || !(0..self.slots.len()).any(|s| self.own_spillable(s, p)) {
            return self.open_pipeline_once(p, share, images);
        }
        self.set_own_try(true);
        let first = self.open_pipeline_once(p, share, images);
        let own = self.inner.stmt().is_some_and(crate::mem::grant::Reservation::own_refused);
        self.set_own_try(false);
        match first {
            Err(Error::OutOfMemory { .. }) if own => {}
            r => return r,
        }
        let given = self.put_own_sorts(p)?;
        OWN_SPILL[0].fetch_add(1, Relaxed);
        self.open_pipeline_once(p, share, images).inspect_err(|e| {
            if matches!(e, Error::OutOfMemory { .. }) {
                OWN_SPILL[3].fetch_add(1, Relaxed);
                // the client's error stays the plain 53200 of any refused open; the log names the road it took first
                eprintln!("gravity: own spill: pipeline {p}'s open was refused again after its statement's finished sorts went to their temp files ({given} B given back): {e:?}"); // alloc: Cold
            }
        })
    }

    /// The statement's own-spill road open (`on`) or closed for its draws (`Reservation::set_own_try`).
    fn set_own_try(&self, on: bool) {
        if let Some(s) = self.inner.stmt() {
            s.set_own_try(on);
        }
    }

    /// D4291: every run `own_spillable` names for pipeline `p`'s open put and given back (`put_sealed_sorts`). The bytes
    /// given back.
    fn put_own_sorts(&mut self, p: usize) -> Result<u64> {
        let (runs, given) = self.put_sealed_sorts(p)?;
        OWN_SPILL[1].fetch_add(runs, Relaxed);
        OWN_SPILL[2].fetch_add(given, Relaxed);
        Ok(given)
    }

    /// Sort give-back, the revoke's finished sorts (memlead D4314 (b)): once per revoke its statement was asked, between
    /// calls (`Entry::claim_at`, beside the home's cut), every sealed run no reader has begun (`own_spillable`, no
    /// pipeline excepted) put and given back as a refused open's are (`put_sealed_sorts`), whatever pipeline runs now and
    /// whether it has a home. Counted as the revoke's give-back (`REVOKE_GIVEN`, the statement's `revoke-sla:` line). The
    /// bytes given back.
    pub(crate) fn give_back_sorts(&mut self) -> Result<u64> {
        let (stmt, gen) = (mcx::revoke::statement(), mcx::revoke::generation());
        if gen == 0 || (stmt, gen) == self.sorts_revoked || self.inner.stmt().is_none() {
            return Ok(0);
        }
        self.sorts_revoked = (stmt, gen);
        let (_, given) = self.put_sealed_sorts(usize::MAX)?;
        if given != 0 {
            crate::mem::grow::REVOKE_GIVEN.fetch_add(given, Relaxed);
            mcx::revoke::sla_given(gen, given);
        }
        Ok(given)
    }

    /// The finished-sort half of the sort give-back (`mem::grow::give_back_sorted`): every run `own_spillable` names with
    /// pipeline `except`'s reads allowed (`usize::MAX`: none) put to its slot's stream (`put_own_run`: the sort's own put
    /// road), its region's bytes past its opened length given back (its committed growth, a warm take's length), the
    /// region laid again at that length (`lay_region`: the reader's `begin_in` finds no sealed run and merges the stream,
    /// as after the kernel's own put). A cancel during a put fails it (the write's own cancel step); the stream's files go
    /// with the statement. The runs put and the bytes given back.
    fn put_sealed_sorts(&mut self, except: usize) -> Result<(u64, u64)> {
        let (mut runs, mut given) = (0u64, 0u64);
        for s in 0..self.slots.len() {
            if !self.own_spillable(s, except) {
                continue;
            }
            let keep = own_keep(&self.slots[s]);
            self.put_own_run(s as u8)?;
            let r = &mut self.slots[s];
            given += crate::mem::grow::give_back_sorted(&mut r.buf, keep) as u64;
            super::raw::lay_region(r.ptr, keep as u32, Some(keep as u32));
            r.bytes = keep as u32;
            runs += 1;
        }
        Ok((runs, given))
    }
}

/// D4291: the length an own spill lays slot `r`'s region back to: the length its open asked (`SlotRegion::opened`), at
/// least a run header (the reader lays its merge's header there), at most the region's own length (its growth in place is
/// `committed` past it).
fn own_keep(r: &super::SlotRegion) -> usize {
    (r.opened as usize).max(gravity_abi::breaker::SORTRUN_HDR).min(r.buf.len()) & !7
}

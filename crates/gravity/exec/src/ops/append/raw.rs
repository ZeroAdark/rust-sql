//! append's raw half in exec (rule R6: the family's unsafe code lives here): D2013's nest.prune object. `lay` writes the
//! object (kernels ops/append/raw.rs `PruneHead`, then the address of the protocol's state) into its cell; `op` is its
//! runtime, called by `m8_prune` at a nest inner union's first read after a re-open: it reads the nest's parameters from
//! the caller's frame words (lowerv4 params.rs `word`'s pairs: value, then flags SET 1, NULL 2) and hands them to the
//! safe decision (prune.rs `nest_decide`). Also the plan and parameters prune.rs `over_state` binds into its throwaway
//! executor state, and the by-reference images spool.rs `ArmSpool` copies out of a pushed batch.
#![allow(unsafe_code)]
use gravity_abi::{Ctx, Datum};
use gravity_driver::{BorrowedBatch, Result};
use gravity_kernels::ops::append::raw::{PruneHead, PRUNE_MAGIC};
use gravity_lower::rerun::spool::fixed_words;
use types_error::PgResult;
use types_nodes::plannodes::PlannedStmt;
use types_portal::params::ParamExternData;
use types_portal::ParamListHandle;

use super::prune::{ExecWord, NestPruneRt, EXEC_WORDS};
use super::spool::{ArmSpool, RUN_BYTES};

/// prune.rs `NestPruneRt`'s kept pruning state, behind its mutex.
pub(crate) struct LeaderHold(pub(crate) super::prune::ExecHold);

// SAFETY: prune.rs `nest_decide` builds, uses and replaces the state only on the thread that prepared the execution (the
// rt's `leader`, checked before the lock), and the rt drops on that thread at the execution's end: the state never runs
// on another thread; the mutex only makes the shared rt's field Sync.
unsafe impl Send for LeaderHold {}

/// The object's words: the head, then the state's address.
pub(crate) const WORDS: usize = core::mem::size_of::<PruneHead>() / 8 + 1;

/// The object for `rt` laid into `cell` (WORDS words); its address, the handle word's value.
pub(crate) fn lay(cell: &mut [u64], rt: &NestPruneRt) -> u64 {
    assert!(cell.len() >= WORDS, "nest.prune's object cell");
    let c = cell.as_mut_ptr();
    // SAFETY: `cell` holds at least WORDS 8-aligned words: the head (two words), then the state's address.
    unsafe {
        c.cast::<PruneHead>().write(PruneHead { magic: PRUNE_MAGIC, op });
        c.add(WORDS - 1).write(rt as *const NestPruneRt as u64);
    }
    c as u64
}

/// The object's `PruneHead::op`: the pruned arms' mask, else ERROR with the error stashed.
///
/// # Safety
/// `h` is an object `lay` made (its last word the live `NestPruneRt`, both living until the execution's end), `ctx` the
/// caller's context, bound for this call.
unsafe fn op(h: *mut PruneHead, ctx: *mut Ctx) -> i64 {
    let rt = &*(*h.cast::<u64>().add(WORDS - 1) as *const NestPruneRt);
    let r = params(rt, ctx).and_then(|(words, n)| super::prune::nest_decide(rt, &words[..n]));
    match r {
        Ok(mask) => mask,
        Err(e) => {
            gravity_fn::stash_error(e);
            gravity_abi::status::ERROR
        }
    }
}

/// The nest's parameters `rt.ids` from `ctx`'s frame words (the words site's keys).
///
/// # Safety
/// `ctx` is null or the caller's context, bound for this call.
unsafe fn params(rt: &NestPruneRt, ctx: *mut Ctx) -> PgResult<([ExecWord; EXEC_WORDS], usize)> {
    let mut words = [ExecWord { param: -1, value: 0, null: 1 }; EXEC_WORDS];
    let ids = rt.ids();
    if ids.len() > EXEC_WORDS {
        return Err(crate::glue::internal("gravity: m8_prune over more params than prune::EXEC_WORDS"));
    }
    if ids.is_empty() {
        return Ok((words, 0));
    }
    let ws = rt.words_site();
    let mut keys: *const u64 = core::ptr::null();
    for (k, &id) in ids.iter().enumerate() {
        if id < 0 {
            return Err(crate::glue::internal("gravity: m8_prune over a negative param id (D2013)"));
        }
        // D3612: an InitPlan's param, read (its Init run) at this decision
        if let Some(w) = rt.init_word(id)? {
            words[k] = w;
            continue;
        }
        if keys.is_null() {
            if ctx.is_null() || ws == usize::MAX {
                return Err(crate::glue::internal("gravity: m8_prune with no frame words (D2013)"));
            }
            // SAFETY: the caller's bound context; site `ws` is the program's words site, whose keys array holds the frame
            // words, one pair per param id up to the largest a producer sets (lowerv4 params.rs `reserve`), as exec
            // rerun's `frame_param`.
            keys = *(ctx as *const u8).add(gravity_abi::ctx::site_off(ws) + gravity_abi::site::site::KEYS).cast::<*const u64>();
        }
        let (v, f) = (*keys.add(2 * id as usize), *keys.add(2 * id as usize + 1));
        if f & 1 == 0 {
            return Err(crate::glue::internal(&format!("gravity: m8_prune reads exec param {id} before the nest set it (D2013)"))); // alloc: Cold
        }
        words[k] = ExecWord { param: id, value: v, null: (f & 2 != 0) as u8 };
    }
    Ok((words, ids.len()))
}

/// `pstmt` at the lifetime `'e` of prune.rs `over_state`'s throwaway executor state, which only reads it.
pub(super) fn state_plan<'e>(pstmt: &'static PlannedStmt<'static>) -> &'e PlannedStmt<'e> {
    // SAFETY: the `'static` plan outlives this read-only throwaway state (`querydesc::shorten_pstmt`'s narrowing at ExecutorStart).
    unsafe { &*(pstmt as *const PlannedStmt<'static>).cast::<PlannedStmt<'e>>() }
}

/// The portal's extern parameters behind `params` (not null), for prune.rs `over_state`'s throwaway executor state.
pub(super) fn state_params<'e>(params: ParamListHandle) -> &'e [ParamExternData] {
    // SAFETY: the portal's parameter array outlives the admission that binds this execution (as `admit_in`'s words).
    unsafe { types_portal::params::resolve(params) }
}

/// `VARSIZE_ANY` of the image at `p` as `gravity_expr::byref::image_len` reads it, or the fixed `typlen`.
/// # Safety
/// `p` is a live by-reference datum of a type whose `typlen` is `typlen`.
unsafe fn image_len(p: *const u8, typlen: i16) -> usize {
    if typlen > 0 {
        return typlen as usize;
    }
    let b0 = *p;
    if b0 & 1 != 0 {
        return if b0 == 1 { 18 } else { (b0 >> 1) as usize };
    }
    ((core::ptr::read_unaligned(p as *const u32) >> 2) & 0x3fff_ffff) as usize
}

impl ArmSpool {
    /// Each row of `b` encoded onto the spool (spool.rs `Sink::push`, after its width check).
    pub(super) fn push_rows(&mut self, b: &BorrowedBatch<'_>) -> Result<()> {
        for (v, n) in b.iter() {
            // SAFETY: the batch's by-reference datums point into its region or a statement-stable image (`BorrowedBatch.region`).
            unsafe { self.row(v, n)? };
        }
        Ok(())
    }

    /// # Safety
    /// The by-reference datums of `v` that are not NULL are live images of their columns' types.
    unsafe fn row(&mut self, v: &[Datum], isnull: &[u8]) -> Result<()> {
        let natts = self.cols.len();
        let fixed = fixed_words(natts) * 8;
        let mut bytes = fixed;
        for k in 0..natts {
            if let (Some(l), 0) = (self.cols[k], isnull[k]) {
                bytes += (image_len(v[k] as usize as *const u8, l) + 7) & !7;
            }
        }
        if !self.run.is_empty() && self.run.len() + bytes > RUN_BYTES {
            self.put()?;
        }
        let at = self.run.len();
        self.run.resize(at + bytes, 0); // alloc: Exec
        let row = &mut self.run[at..];
        row[..8].copy_from_slice(&((bytes / 8) as u64).to_ne_bytes());
        let mut img = fixed;
        for k in 0..natts {
            if isnull[k] != 0 {
                row[(1 + natts) * 8 + k] = 1;
                continue;
            }
            let word = match self.cols[k] {
                None => v[k],
                Some(l) => {
                    let p = v[k] as usize as *const u8;
                    let len = image_len(p, l);
                    row[img..img + len].copy_from_slice(core::slice::from_raw_parts(p, len));
                    let d = img as u64;
                    img += (len + 7) & !7;
                    d
                }
            };
            row[(1 + k) * 8..(2 + k) * 8].copy_from_slice(&word.to_ne_bytes());
        }
        self.rows += 1;
        Ok(())
    }
}

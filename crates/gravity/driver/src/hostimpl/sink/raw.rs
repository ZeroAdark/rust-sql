//! `sink`'s unsafe half (rule R6, the module's raw home): its host entries and the raw-pointer steps they take,
//! moved verbatim from sink.rs (private ones pub(super)); it imports them back.
#![allow(unsafe_code)]

use gravity_abi::PtrWord;
use super::*;

gravity_abi::host! {
    /// `Program.push` over the concrete sink `S` behind the block's `sink` word (abi-v2 §2.3): the
    /// `Out` block as a batch, the rows counted on the owner (settled to the binding after the call),
    /// the block emptied; `OK` / `STOP`, or the sink's failure stored behind `env`.
    /// # Safety — `ctx` is bound with `sink` an `S`.
    pub unsafe fn push<S: Sink>(ctx: *mut Ctx) -> i64 {
        let p = &*(*ctx).program.ptr();
        let sink = &mut *(p.sink.ptr() as *mut S);
        guarded(p.env.ptr(), || settle(ctx, |b| sink.push(b)), |status| status)
    }
}

gravity_abi::host! {
    /// # Safety — `ctx` is bound with `sink` an `L`.
    pub unsafe fn push_leg<L: SinkLeg>(ctx: *mut Ctx) -> i64 {
        let p = &*(*ctx).program.ptr();
        let leg = &*(p.sink.ptr() as *const L);
        guarded(p.env.ptr(), || settle(ctx, |b| leg.push(b)), |status| status)
    }
}

gravity_abi::host! {
    /// # Safety — `ctx` is bound.
    pub unsafe fn unbound(ctx: *mut Ctx) -> i64 {
        fail(inner_of(ctx), Error::host("push without a bound sink"))
    }
}

#[inline(always)]
pub(super) unsafe fn settle(ctx: *mut Ctx, push: impl FnOnce(BorrowedBatch<'_>) -> Result<Flow>) -> i64 {
    let n = inner((*(*ctx).program.ptr()).env.ptr());
    let out = &mut *(*ctx).out.ptr();
    let rows = out.count as usize;
    let cells = rows * n.out_natts;
    let region = core::slice::from_raw_parts(out.region.ptr(), out.region_len as usize);
    let flow = push(BorrowedBatch { natts: n.out_natts, rows, values: core::slice::from_raw_parts(out.values.ptr(), cells), isnull: core::slice::from_raw_parts(out.isnull.ptr(), cells), region });
    n.pushed += rows as u64;
    if cfg!(debug_assertions) {
        core::ptr::write_bytes(out.values.ptr() as *mut u8, POISON, cells * 8);
        core::ptr::write_bytes(out.isnull.ptr(), POISON, cells);
    }
    out.count = 0;
    out.region_len = 0;
    match flow {
        Ok(Flow::Continue) => OK,
        Ok(Flow::Stop) => STOP,
        Err(e) => fail(n, e),
    }
}

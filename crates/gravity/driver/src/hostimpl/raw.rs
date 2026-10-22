//! `hostimpl`'s unsafe half (rule R6, the module's raw home): its host entries and the raw-pointer steps they take,
//! moved verbatim from mod.rs (private ones pub(super)); it imports them back.
#![allow(unsafe_code)]

use gravity_abi::PtrWord;
use super::*;

/// # Safety — `env` is the `Inner` the block was bound with.
pub(crate) unsafe fn inner<'a>(env: *mut u8) -> &'a mut Inner {
    &mut *(env as *mut Inner)
}

/// # Safety — `ctx` is bound.
pub(crate) unsafe fn inner_of<'a>(ctx: *mut Ctx) -> &'a mut Inner {
    &mut *env_of(ctx)
}

/// D1744: the `Inner` a bound `ctx`'s block was bound with, raw: the provenance the hosts write it through (the driver's
/// own reads across a program or host call take it so, `Inner::settle_call`).
/// # Safety — `ctx` is bound.
pub(crate) unsafe fn env_of(ctx: *mut Ctx) -> *mut Inner {
    (*(*ctx).program.ptr()).env.ptr() as *mut Inner
}

/// D314: a host call's body under the one landing pad (`gravity_abi::status::unwind::pad`): an unwind out of the host
/// (a PostgreSQL ERROR raised by unwinding, a thread exit, a bug's panic) is parked, the call fails with a host error
/// (`failed` gets its status), and `gravity_program::Code::call` resumes the unwind the moment the program returns.
/// # Safety — `env` is the `Inner` the block was bound with.
#[inline(always)]
pub(crate) unsafe fn guarded<T>(env: *mut u8, body: impl FnOnce() -> T, failed: impl FnOnce(i64) -> T) -> T {
    // C9: every host entry is a step (the probe's label: the host entry and its source or host type) polled at its entry
    // and its exit. A set abort bit is seen, never a skipped body: a release must run; the kernel stops at its next check
    let mut step = cancelpoll::step(core::any::type_name_of_val(&body));
    let _ = step.tick();
    let r = match gravity_abi::status::unwind::pad(body) {
        Some(v) => v,
        None => failed(unwound(env)),
    };
    let _ = step.tick();
    r
}

/// # Safety — As `guarded`.
#[cold]
#[inline(never)]
pub(super) unsafe fn unwound(env: *mut u8) -> i64 {
    fail(inner(env), Error::host("gravity: a host call unwound (resumed past the program)"))
}

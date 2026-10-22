//! The kernels' contract (contract-runtime landing 4, the raise seed): the one raise through which a kernel stashes
//! PostgreSQL's error for the IR to leave with, and PG's `MAKE_SQLSTATE`. m6's, m8's and m11's raises call it. And
//! (landing 12) the entry wrapper: a host the IR calls is an `impl Host`, its symbol `entryN::<H>`, the one landing pad.
use gravity_abi::status::ERROR;
use gravity_abi::Ctx;
use gravity_fn::PgError;

/// A host the lowered IR calls with the program's context first. `run` is its body; `fail` is what the entry answers
/// when the body panics, the value its IR already reads as a failure (`ERROR`, or a record entry's `sunk` buffer).
pub trait Host {
    type Args: Copy;
    /// # Safety
    /// `ctx` is the running program's context and `a` the words its IR passes, as the host's IR declares them.
    unsafe fn run(ctx: *mut Ctx, a: Self::Args) -> u64;
    fn fail(_a: Self::Args) -> u64 {
        ERROR as u64
    }
}

/// The one landing pad (`gravity_abi::status::unwind::pad`): the `entryN::<H>` wrappers here, the `host!` entries under
/// `#[guard]` and the entries still written as `extern "C" fn` under `guard!` (host_abi.rs) use it.
pub use gravity_abi::status::unwind::pad;

/// `contract::host` (constructs #11): an entry the IR calls by its own name, written as a plain function; the macro writes
/// the `extern "C"` function and, under `#[guard(FAIL)]`, the landing pad. Defined in abi (`gravity_abi::host!`) so the
/// driver and fn, which sit below kernels, write their entries the same way.
pub use gravity_abi::host;

unsafe fn guarded<H: Host>(ctx: u64, a: H::Args) -> u64 {
    // SAFETY: the entry's own arguments, as `Host::run` requires
    pad(|| unsafe { H::run(ctx as *mut Ctx, a) }).unwrap_or_else(|| H::fail(a))
}

/// `H` with no word past the context.
/// # Safety
/// As `Host::run`.
pub unsafe extern "C" fn entry0<H: Host<Args = ()>>(ctx: u64) -> u64 {
    guarded::<H>(ctx, ())
}
/// `H` with one word past the context.
/// # Safety
/// As `Host::run`.
pub unsafe extern "C" fn entry1<H: Host<Args = (u64,)>>(ctx: u64, a: u64) -> u64 {
    guarded::<H>(ctx, (a,))
}
/// `H` with two words past the context.
/// # Safety
/// As `Host::run`.
pub unsafe extern "C" fn entry2<H: Host<Args = (u64, u64)>>(ctx: u64, a: u64, b: u64) -> u64 {
    guarded::<H>(ctx, (a, b))
}
/// `H` with three words past the context.
/// # Safety
/// As `Host::run`.
pub unsafe extern "C" fn entry3<H: Host<Args = (u64, u64, u64)>>(ctx: u64, a: u64, b: u64, c: u64) -> u64 {
    guarded::<H>(ctx, (a, b, c))
}
/// `H` with four words past the context.
/// # Safety
/// As `Host::run`.
pub unsafe extern "C" fn entry4<H: Host<Args = (u64, u64, u64, u64)>>(ctx: u64, a: u64, b: u64, c: u64, d: u64) -> u64 {
    guarded::<H>(ctx, (a, b, c, d))
}

/// PG's `MAKE_SQLSTATE` (elog.h: `PGSIXBIT` of each character, 6 bits apiece, the first lowest). The kernel boundary
/// keeps types_error out of this crate, so the code rides `PgError`'s own type.
pub const fn sqlstate(c: [u8; 5]) -> i32 {
    let (mut v, mut i) = (0, 0);
    while i < 5 {
        v += ((c[i] as i32 - b'0' as i32) & 0x3f) << (6 * i);
        i += 1;
    }
    v
}

/// PostgreSQL's error `msg` under SQLSTATE `state`, with `hint` when there is one, stashed as the statement's error:
/// `ERROR` (below zero), for the IR to leave with. Cold path only.
#[cold]
#[inline(never)]
pub fn raise(msg: impl Into<String>, hint: Option<&str>, state: [u8; 5]) -> i64 {
    let e = PgError::error(msg);
    let mut s = e.sqlstate();
    s.0 = sqlstate(state);
    let e = e.with_sqlstate(s);
    let e = match hint {
        Some(h) => e.with_hint(h),
        None => e,
    };
    gravity_fn::stash_error(Box::new(e)); // alloc: Cold
    ERROR
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Boom;
    impl Host for Boom {
        type Args = (u64,);
        unsafe fn run(_ctx: *mut Ctx, a: (u64,)) -> u64 {
            if a.0 == 0 {
                panic!("host bug")
            }
            a.0
        }
        fn fail(a: (u64,)) -> u64 {
            a.0 + 100
        }
    }

    struct Plain;
    impl Host for Plain {
        type Args = ();
        unsafe fn run(_ctx: *mut Ctx, _a: ()) -> u64 {
            panic!("host bug")
        }
    }

    /// The parked payload's message, taken off the thread's slot (`unwind::resume` inside a catch).
    fn take_parked() -> Option<&'static str> {
        std::panic::catch_unwind(gravity_abi::status::unwind::resume).err().and_then(|p| p.downcast_ref::<&str>().copied())
    }

    /// Landing 12: a host's value goes through untouched; its panic parks and the entry answers the host's failure word.
    #[test]
    fn a_panicking_host_parks_and_answers_its_failure_word() {
        assert_eq!(unsafe { entry1::<Boom>(0, 7) }, 7);
        assert_eq!(take_parked(), None, "nothing parked on a return");
        assert_eq!(unsafe { entry1::<Boom>(0, 0) }, 100, "the host's own failure word");
        assert_eq!(take_parked(), Some("host bug"), "the panic is parked for the program's caller");
        assert_eq!(unsafe { entry0::<Plain>(0) }, ERROR as u64, "ERROR by default");
        assert_eq!(take_parked(), Some("host bug"));
    }
}

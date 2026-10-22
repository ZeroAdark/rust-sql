//! The row lock's host entry: the C-ABI function the lowered IR calls for LockRows (nodeLockRows.c ExecLockRows'
//! table_tuple_lock). `lock_row` reaches `ProgramBlock.lock` (`LockFn`), which the driver resolves to the bound `LockHost`
//! (driver `hostimpl/dml.rs` `lock`, hosts `lock.rs`). Its body runs under the landing pad (`contract::host!`'s `#[guard]`):
//! a bug's panic never crosses the `extern "C"` frame. The entry lives in raw.rs (rules R6 / R8).
pub use super::raw::lock_row;

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_abi::status::{recheck, ERROR, WRITE_DONE, WRITE_RECHECK, WRITE_SKIPPED};
    use gravity_abi::{Ctx, ProgramBlock};
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};

    // the one test that reads them sets them first (no other test calls `fake_lock`)
    static NEXT: AtomicI64 = AtomicI64::new(0);
    static SEEN: AtomicU64 = AtomicU64::new(u64::MAX);

    unsafe extern "C" fn fake_lock(_ctx: *mut Ctx, site: u64) -> i64 {
        SEEN.store(site, Relaxed);
        NEXT.load(Relaxed)
    }

    /// A host whose body panics: its pad parks the panic and it answers ERROR (hosts never unwind into a program).
    unsafe extern "C" fn panicking_lock(_ctx: *mut Ctx, _site: u64) -> i64 {
        gravity_abi::status::unwind::pad(|| -> i64 { panic!("lock host bug") }).unwrap_or(ERROR)
    }

    /// A zeroed ctx whose program block carries only `lock`: the one field `lock_row` reads.
    fn call(lock: unsafe extern "C" fn(*mut Ctx, u64) -> i64, site: u64) -> u64 {
        let mut prog: ProgramBlock = unsafe { core::mem::zeroed() };
        prog.lock = Some(lock);
        let mut ctx: Ctx = unsafe { core::mem::zeroed() };
        ctx.program = &mut prog;
        unsafe { lock_row(&mut ctx as *mut Ctx as u64, site) }
    }

    #[test]
    fn lock_row_hands_back_the_hosts_status_for_the_named_site() {
        for status in [WRITE_DONE, WRITE_SKIPPED, WRITE_RECHECK, ERROR, recheck(3)] {
            NEXT.store(status, Relaxed);
            assert_eq!(call(fake_lock, 7) as i64, status);
            assert_eq!(SEEN.load(Relaxed), 7);
        }
    }

    #[test]
    fn lock_row_parks_a_hosts_panic_and_returns_error() {
        let v = call(panicking_lock, 0);
        assert_eq!(v as i64, ERROR);
        let resumed = std::panic::catch_unwind(gravity_abi::status::unwind::resume);
        let payload = resumed.expect_err("the parked panic resumes at the caller");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"lock host bug"));
    }
}

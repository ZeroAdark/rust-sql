//! `host!` (contract::host, constructs #11): each form writes an entry with the ABI its callers take by address (the
//! typed pointers below fail to compile otherwise), keeps the name and the body, and `#[guard]` puts the body under the
//! one landing pad: a panic parks for the program's caller and the entry answers its failure value.
use gravity_abi::status::unwind::resume;
use gravity_abi::status::ERROR;

pub trait Scale {
    const BY: u64;
}
pub struct Ten;
impl Scale for Ten {
    const BY: u64 = 10;
}

gravity_abi::host! {
    /// A guarded unsafe entry (`lock_row`'s form): its answer, or ERROR after a panic.
    #[guard(ERROR as u64)]
    pub unsafe fn guarded_answer(ctx: u64, site: u64) -> u64 {
        assert!(site != 0, "host bug");
        *(ctx as *const u64) + site
    }

    /// A guarded safe entry whose failure value reads its own argument (a record entry's sunk buffer).
    #[guard(bytes + 100)]
    #[inline(never)]
    pub fn guarded_safe(bytes: u64) -> u64 {
        assert!(bytes != 0, "safe host bug");
        bytes * 2
    }

    /// A bare generic entry (the driver's `push<S: Sink>` form): the body as written.
    pub unsafe fn bare_scaled<S: Scale>(p: *const u64) -> i64 {
        (*p * S::BY) as i64
    }

    /// A bare entry with no answer.
    fn bare_store(slot: *mut u64, v: u64) {
        unsafe { *slot = v }
    }

    /// An unwinding entry (the fmgr shims' form): a panic goes on to the caller.
    #[unwind]
    pub unsafe fn unwinding(n: i64) -> i64 {
        assert!(n >= 0, "unwinds");
        n + 1
    }
}

/// The parked payload's message, taken off the thread's slot.
fn take_parked() -> Option<&'static str> {
    std::panic::catch_unwind(resume).err().and_then(|p| p.downcast_ref::<&str>().copied())
}

#[test]
fn a_guarded_entry_answers_its_body_and_parks_a_panic_as_its_failure_value() {
    let entry: unsafe extern "C" fn(u64, u64) -> u64 = guarded_answer;
    let base = 40u64;
    assert_eq!(unsafe { entry(&base as *const u64 as u64, 2) }, 42);
    assert_eq!(take_parked(), None, "nothing parked on a return");
    assert_eq!(unsafe { entry(0, 0) }, ERROR as u64);
    assert_eq!(take_parked(), Some("host bug"), "the panic waits for the program's caller");

    let safe: extern "C" fn(u64) -> u64 = guarded_safe;
    assert_eq!(safe(4), 8);
    assert_eq!(safe(0), 100, "the failure value may read the arguments");
    assert_eq!(take_parked(), Some("safe host bug"));
}

#[test]
fn bare_entries_keep_their_body_generics_and_c_abi() {
    let scaled: unsafe extern "C" fn(*const u64) -> i64 = bare_scaled::<Ten>;
    assert_eq!(unsafe { scaled(&7u64) }, 70);
    let store: extern "C" fn(*mut u64, u64) = bare_store;
    let mut slot = 0u64;
    store(&mut slot, 9);
    assert_eq!(slot, 9);
}

#[test]
fn an_unwind_entry_is_c_unwind_and_lets_the_panic_through() {
    let entry: unsafe extern "C-unwind" fn(i64) -> i64 = unwinding;
    assert_eq!(unsafe { entry(1) }, 2);
    let p = std::panic::catch_unwind(|| unsafe { entry(-1) }).expect_err("the panic reaches the caller");
    assert_eq!(p.downcast_ref::<&str>().copied(), Some("unwinds"));
    assert_eq!(take_parked(), None, "nothing parked: no pad");
}

//! D314: a host function's body that unwinds comes back as the call's error. A PostgreSQL ERROR raised by unwinding (boxed or
//! not) is the call's typed error with nothing parked; any other unwind (a FATAL, a bug's panic) fails the call and is parked
//! for the program's caller, which resumes it (`gravity_abi::status::unwind::resume`). Driven through `with_payload`, whose body
//! runs under the crate's guard.

use gravity_fn::{with_payload, PgError};

/// A 1-byte-header text image of `x`.
const X: [u8; 2] = [(2 << 1) | 1, b'x'];

fn raise(payload: Box<dyn core::any::Any + Send>) -> Result<u64, Box<PgError>> {
    // SAFETY: `X` is a valid short varlena image, live through the call.
    unsafe { with_payload(X.as_ptr(), |_| -> u64 { std::panic::resume_unwind(payload) }) }
}

#[test]
fn an_error_comes_back_typed_and_any_other_unwind_is_parked_for_the_program_caller() {
    // SAFETY: as in `raise`.
    assert_eq!(unsafe { with_payload(X.as_ptr(), |b| b.len() as u64 + 6) }.ok(), Some(7));
    // a PostgreSQL ERROR raised by unwinding, boxed or not: the call's error, nothing parked
    for payload in [Box::new(Box::new(PgError::error("division by zero"))) as Box<dyn core::any::Any + Send>, Box::new(PgError::error("division by zero"))] {
        assert_eq!(raise(payload).err().map(|e| e.message().to_string()), Some("division by zero".to_string()));
        gravity_abi::status::unwind::resume();
    }
    // a FATAL and a bug's panic: the call fails, and the unwind goes on at `resume` with its payload
    let fatal = Box::new(Box::new(PgError::new(types_error::FATAL, "terminating connection".to_string())));
    assert!(raise(fatal).is_err());
    let resumed = std::panic::catch_unwind(gravity_abi::status::unwind::resume).expect_err("the FATAL resumes");
    assert_eq!(resumed.downcast_ref::<Box<PgError>>().map(|e| e.level()), Some(types_error::FATAL));
    assert!(raise(Box::new("a bug")).is_err());
    let resumed = std::panic::catch_unwind(gravity_abi::status::unwind::resume).expect_err("the panic resumes");
    assert_eq!(resumed.downcast_ref::<&str>(), Some(&"a bug"));
}

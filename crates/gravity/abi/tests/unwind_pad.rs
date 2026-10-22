//! The one landing pad (`status::unwind::pad`): a body that returns is the answer; a body that unwinds answers `None`,
//! and its payload resumes at the program's caller (`resume`), the first payload when two were parked.
use gravity_abi::status::unwind::{pad, resume};

#[test]
fn a_returning_body_is_the_answer_and_parks_nothing() {
    assert_eq!(pad(|| 7u64), Some(7));
    resume(); // nothing parked: no unwind
}

#[test]
fn an_unwinding_body_answers_none_and_resumes_at_the_caller() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    assert_eq!(pad(|| -> u64 { panic!("first") }), None);
    assert_eq!(pad(|| -> u64 { panic!("second") }), None);
    std::panic::set_hook(prev);
    let p = std::panic::catch_unwind(resume).expect_err("the parked payload resumes");
    assert_eq!(p.downcast_ref::<&str>().copied(), Some("first"));
    resume(); // the slot is empty again
}

/// Landing 13: the host slots are `extern "C"`; a host whose body panics answers through its pad, so the program
/// (the caller of the slot) sees a status, and the payload resumes at the program's caller.
#[test]
fn a_c_host_slot_answers_its_pads_status_and_the_payload_resumes_after() {
    unsafe extern "C" fn host(_ctx: *mut gravity_abi::Ctx, site: u64) -> i64 {
        pad(|| -> i64 {
            if site == 0 {
                panic!("host bug");
            }
            site as i64
        })
        .unwrap_or(-1)
    }
    let slot: gravity_abi::program::LockFn = Some(host);
    let call = slot.expect("bound");
    assert_eq!(unsafe { call(core::ptr::null_mut(), 5) }, 5);
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    assert_eq!(unsafe { call(core::ptr::null_mut(), 0) }, -1);
    std::panic::set_hook(prev);
    let p = std::panic::catch_unwind(resume).expect_err("the parked payload resumes");
    assert_eq!(p.downcast_ref::<&str>().copied(), Some("host bug"));
}

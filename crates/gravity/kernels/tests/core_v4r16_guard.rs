//! V4R-16: a panic inside a kernels `host_abi` entry never crosses its `extern "C"` frame. The entry parks the unwind
//! (`gravity_abi::status::unwind`) and returns its failure word, and the program's caller (`gravity_program::Code::call`)
//! resumes it: a PostgreSQL ERROR keeps its SQLSTATE, and a bug's panic keeps its own payload. Before V4R-16 the unwind
//! met the `extern "C"` frame and Rust aborted the process, which is the whole server, because a backend is a thread.
//! D314 already guards the driver trampolines and the fn hosts; this covers the kernel's own code.
use std::panic::catch_unwind;

use gravity_abi::breaker::{RecordLayout, TopK, LAYOUT_WORDS, TOPK_SECOND_PAIR};
use gravity_abi::status::{unwind, ERROR};
use gravity_fn::PgError;
use gravity_kernels::dict::entries_tables::{entry_runs_offer, set_entry_runs_offer};
use gravity_kernels::host_abi::{topk_insert, topk_order};

/// The unwind the entry parked, resumed as the program's caller does.
fn resumed() -> Option<Box<dyn std::any::Any + Send>> {
    catch_unwind(unwind::resume).err()
}

/// A top-k run of two attributes (the key word, a second word) over `words`, holding the rows `(k, 10 k)`.
unsafe fn run_of(words: &mut [u64], keys: &[u64]) -> *mut u8 {
    let run = words.as_mut_ptr() as *mut u8;
    TopK::at_cap(run, keys.len() as u32, keys.len() as u32, 2, 0);
    for &k in keys {
        assert_eq!(topk_insert(run, [k, 10 * k].as_ptr(), [0u8, 0].as_ptr(), 2), 1);
    }
    run
}

/// Out-of-range input: a run that claims a second key pair (`TOPK_SECOND_PAIR`) but has two attributes, so
/// `topk_order`'s comparator reads word 2 of a two-word row. That is a bounds-check panic in the kernel's own code.
#[test]
fn a_bug_panic_in_an_entry_is_its_error_not_an_abort() {
    let mut words = vec![0u64; TopK::bytes_cap(2, 2) as usize / 8 + 1];
    unsafe {
        let run = run_of(&mut words, &[7, 3]);
        (*(run as *mut TopK)).flags |= TOPK_SECOND_PAIR;
        assert_eq!(topk_order(run, 2), ERROR, "the entry's failure word, the process alive");
    }
    let payload = resumed().expect("the panic parked for the program's caller");
    let msg = payload.downcast_ref::<String>().map(String::as_str).unwrap_or("");
    assert!(msg.contains("index out of bounds"), "the bug's own payload: {msg:?}");
    assert!(resumed().is_none(), "one park, one resume");
}

/// The guard leaves a well-formed call alone: the same answer, nothing parked.
#[test]
fn a_guarded_entry_that_does_not_panic_answers_as_before() {
    let mut words = vec![0u64; TopK::bytes_cap(3, 2) as usize / 8 + 1];
    unsafe {
        let run = run_of(&mut words, &[9, 2, 5]);
        assert_eq!(topk_order(run, 2), 2);
        let t = TopK::view(run);
        assert_eq!((t.key_of(0), t.key_of(1)), (2, 5));
    }
    assert!(resumed().is_none());
}

const SQLSTATE_22012: i32 = sqlstate(*b"22012");

const fn sqlstate(c: [u8; 5]) -> i32 {
    let (mut v, mut i) = (0, 0);
    while i < 5 {
        v += ((c[i] as i32 - b'0' as i32) & 0x3f) << (6 * i);
        i += 1;
    }
    v
}

/// A PostgreSQL ERROR raised by unwinding inside the offer `entry_runs_offer` calls (the source's side, installed by the
/// hosts): the entry still answers 0, and the resumed payload is the ERROR with its SQLSTATE intact.
#[test]
fn an_error_raised_inside_an_entry_keeps_its_sqlstate() {
    unsafe fn raising(_ctx: *mut gravity_abi::Ctx, _site: usize, _l: &RecordLayout, _bits: u8) {
        let e = PgError::error("division by zero");
        let mut s = e.sqlstate();
        s.0 = SQLSTATE_22012;
        std::panic::panic_any(Box::new(e.with_sqlstate(s)));
    }
    set_entry_runs_offer(raising);
    let layout = [0u64; LAYOUT_WORDS];
    let r = unsafe { entry_runs_offer(core::ptr::null_mut(), 0, layout.as_ptr() as *const RecordLayout, 4) };
    assert_eq!(r, 0, "an offer never fails the claim");
    let payload = resumed().expect("the ERROR parked for the program's caller");
    let pg = payload.downcast_ref::<Box<PgError>>().expect("the PgError payload, intact");
    assert_eq!((pg.message(), pg.sqlstate().0), ("division by zero", SQLSTATE_22012));
}

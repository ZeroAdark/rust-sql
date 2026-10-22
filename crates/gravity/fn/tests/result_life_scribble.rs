//! The SCRIBBLE rig (design §3): a kept `UntilNextCall` result reads tildes after the next call of its builtin; a retained copy
//! and every other builtin's results survive; a site's result context stays bounded under a per-row reset (D212's requirement).
mod common;

use std::ffi::CString;
use std::sync::Mutex;

use datum::Datum;
use gravity_fn::result_life::{copy_result, result_bytes, scribble};
use gravity_fn::{FmgrCall, Region, ResultLife, FMGR_SITE_ALLOWANCE};
use mcx::MemoryContext;

const TEXTIN: u32 = 46;
const NUMERIC_IN: u32 = 1701;
const TEXTOID: u32 = 25;
const NUMERICOID: u32 = 1700;

static SERIAL: Mutex<()> = Mutex::new(());

fn input(call: &mut FmgrCall, s: &CString, typoid: u32) -> Datum {
    let r = call.call_site(&[Datum::from_usize(s.as_ptr() as usize), Datum::from_u32(typoid), Datum::from_i32(-1)], &[false; 3]).unwrap();
    assert!(!r.isnull);
    r.value
}

fn text(v: Datum) -> Vec<u8> {
    // SAFETY: the caller's datum is a live varlena (or a scribbled copy of one, whose bytes are still readable).
    unsafe { result_bytes(v, -1) }.to_vec()
}

fn payload(v: &[u8]) -> &[u8] {
    &v[4..]
}

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    common::install_seams();
    scribble::force(true);
    g
}

#[test]
fn a_kept_outbuf_result_reads_tildes_after_the_next_call() {
    let _g = setup();
    let parent = MemoryContext::new("statement grant");
    let mut call = FmgrCall::bind_under(TEXTIN, 3, 0, &parent, -1).unwrap();
    assert_eq!(call.result_life(), ResultLife::UntilNextCall);
    let (a, b) = (CString::new("first value").unwrap(), CString::new("second").unwrap());
    let r1 = input(&mut call, &a, TEXTOID);
    let kept = text(r1);
    assert_eq!(payload(&kept), b"first value");
    let r2 = input(&mut call, &b, TEXTOID);
    assert_eq!(payload(&text(r2)), b"second");
    // SAFETY: the scribbled copy lives in the site's context until its reset.
    let stale = unsafe { core::slice::from_raw_parts(r1.as_usize() as *const u8, kept.len()) };
    assert!(stale.iter().all(|&c| c == scribble::BYTE), "the kept pointer must read tildes: {stale:?}");
}

#[test]
fn another_site_of_the_same_builtin_scribbles_too() {
    let _g = setup();
    let parent = MemoryContext::new("statement grant");
    let mut a = FmgrCall::bind_under(TEXTIN, 3, 0, &parent, -1).unwrap();
    let mut b = FmgrCall::bind_under(TEXTIN, 3, 0, &parent, -1).unwrap();
    let r1 = input(&mut a, &CString::new("site a").unwrap(), TEXTOID);
    let len = text(r1).len();
    input(&mut b, &CString::new("site b").unwrap(), TEXTOID);
    // SAFETY: as above.
    let stale = unsafe { core::slice::from_raw_parts(r1.as_usize() as *const u8, len) };
    assert!(stale.iter().all(|&c| c == scribble::BYTE), "a thread-shared scratch builtin: any site's call ends the result");
}

#[test]
fn a_retained_copy_and_other_builtins_survive() {
    let _g = setup();
    let parent = MemoryContext::new("statement grant");
    let mut t = FmgrCall::bind_under(TEXTIN, 3, 0, &parent, -1).unwrap();
    let mut n = FmgrCall::bind_under(NUMERIC_IN, 3, 0, &parent, -1).unwrap();
    let r1 = input(&mut t, &CString::new("kept by retain").unwrap(), TEXTOID);
    let mut buf = vec![0u8; 256];
    let mut region = Region::over(&mut buf);
    // SAFETY: r1 is live until textin's next call.
    let copy = unsafe { copy_result(r1, -1, &mut region) }.unwrap();
    input(&mut n, &CString::new("1.5").unwrap(), NUMERICOID);
    assert_eq!(payload(&text(r1)), b"kept by retain", "another builtin's call leaves the result alone");
    input(&mut t, &CString::new("next row").unwrap(), TEXTOID);
    assert_eq!(payload(&text(copy)), b"kept by retain", "the retained copy outlives the next call");
}

#[test]
fn a_reset_site_is_forgotten_and_released_bytes_scribble() {
    let _g = setup();
    let parent = MemoryContext::new("statement grant");
    let mut t = FmgrCall::bind_under(TEXTIN, 3, 0, &parent, -1).unwrap();
    input(&mut t, &CString::new("row one").unwrap(), TEXTOID);
    t.reset_window();
    let r = input(&mut t, &CString::new("row two").unwrap(), TEXTOID);
    assert_eq!(payload(&text(r)), b"row two");
    drop(t);
    let mut u = FmgrCall::bind_under(TEXTIN, 3, 0, &parent, -1).unwrap();
    input(&mut u, &CString::new("after the drop").unwrap(), TEXTOID);
    let mut bytes = [1u8; 8];
    // SAFETY: a local buffer.
    unsafe { scribble::released(bytes.as_mut_ptr(), bytes.len()) };
    assert_eq!(bytes, [scribble::BYTE; 8]);
}

#[test]
fn a_site_context_stays_bounded_under_a_per_row_reset() {
    let _g = setup();
    let parent = MemoryContext::new("statement grant");
    let mut n = FmgrCall::bind_under(NUMERIC_IN, 3, 0, &parent, -1).unwrap();
    let v = CString::new("123456789012345678901234567890.123456789").unwrap();
    for _ in 0..200_000 {
        n.reset_window();
        input(&mut n, &v, NUMERICOID);
    }
    assert!(n.result_bytes() <= FMGR_SITE_ALLOWANCE, "per-row reset: {} bytes", n.result_bytes());
    for _ in 0..20_000 {
        input(&mut n, &v, NUMERICOID);
    }
    assert!(n.result_bytes() > FMGR_SITE_ALLOWANCE, "no reset (D212's shape) grows with the rows");
}

/// D324: a site that a thread-local's destructor drops at thread exit (a cached program's `FmgrCall` when a backend ends)
/// can drop after the SCRIBBLE list's own destructor has run. Its forget must not panic there: a panic in a thread-local
/// destructor aborts the process. KEPT is touched first, so its destructor runs after the list's (last in, first out). The
/// scenario runs in a child process of this test binary so that an abort fails this test instead of the whole run.
#[test]
fn a_site_dropped_at_thread_exit_after_the_live_list_forgets_quietly() {
    const NAME: &str = "a_site_dropped_at_thread_exit_after_the_live_list_forgets_quietly";
    if std::env::var_os("GRAVITY_FN_D324_CHILD").is_some() {
        let _g = setup();
        std::thread::spawn(|| {
            thread_local! {
                static KEPT: std::cell::RefCell<Option<FmgrCall>> = const { std::cell::RefCell::new(None) };
            }
            KEPT.with(|_| {});
            let parent: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("statement grant")));
            let mut call = FmgrCall::bind_under(TEXTIN, 3, 0, parent, -1).unwrap();
            input(&mut call, &CString::new("kept past the thread").unwrap(), TEXTOID);
            KEPT.with(|k| *k.borrow_mut() = Some(call));
        })
        .join()
        .unwrap();
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new(exe).args([NAME, "--exact", "--nocapture", "--test-threads=1"]).env("GRAVITY_FN_D324_CHILD", "1").output().unwrap();
    let log = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success() && log.contains("1 passed"), "the child ended {:?}:\n{log}", out.status);
}

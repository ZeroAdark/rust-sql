//! D3811: a wait for another thread's compile of the same module (cache.rs `Flight::wait`) ends at the waiter's pending
//! interrupt within one slice, and leaves the flight to its compiler: that compile files its module, the waiter's next look
//! hits it, nothing stays in flight. A tier-0 promotion stopped in such a wait raises the host's interrupt error (a cancel's
//! 57014) as an ereport is, and its code is not failed: the next promotion takes the module (lib.rs `promoted`). Found by
//! the Hibernate sweep: a backend waiting on the compile pool's 840 s compile answered neither pg_cancel_backend,
//! pg_terminate_backend nor a fast shutdown. Its own test binary: `PGRUST_GRAVITY_TIER0` is read once per process.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use gravity_backend::{cache, mapped_cached_all, promote, Lookup};
use gravity_ir::text::parse_program;
use gravity_ir::Func;
use types_error::{PgError, PgResult};

static CACHE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn cache_lock() -> std::sync::MutexGuard<'static, ()> {
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The thread whose interrupt is pending (the seams are process-wide; other threads read none).
static PENDING: std::sync::Mutex<Option<std::thread::ThreadId>> = std::sync::Mutex::new(None);

fn pending() -> bool {
    *PENDING.lock().unwrap() == Some(std::thread::current().id())
}

/// The host's CHECK_FOR_INTERRUPTS: a pending interrupt is a cancel, taken (cleared) as ProcessInterrupts takes it.
fn check() -> PgResult<()> {
    let mut g = PENDING.lock().unwrap();
    if *g == Some(std::thread::current().id()) {
        *g = None;
        return Err(PgError::error("canceling statement due to user request").with_sqlstate(types_error::error::ERRCODE_QUERY_CANCELED).into());
    }
    Ok(())
}

/// C6 (d): the compile's stop points, its wait and `cache::take_interrupt` ask the interrupt facade
/// (`cancelpoll::interrupts`), which reads PostgreSQL's seams: the test installs them, once per process (a seam is set
/// once).
fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        postgres_seams::interrupts_pending::set(pending);
        postgres_seams::check_for_interrupts::set(check);
    });
}

fn konst(k: i64) -> (gravity_ir::Program, Func) {
    let src = format!("(program (fn e ((c ptr)) (i64) (block entry ((c ptr)) (k (const i64 {k})) (return k))))");
    let p = parse_program(&src, &|_| None).unwrap_or_else(|e| panic!("{e}"));
    let f = Func(p.funcs.iter().position(|f| f.name == "e").unwrap() as u32);
    (p, f)
}

fn call(code: &gravity_program::Code) -> i64 {
    // SAFETY: the program never reads its ctx.
    unsafe { code.call(core::ptr::null_mut()) }
}

/// A compile of `p` held open on another thread until `go` fires; returns once that thread holds the key's flight (its
/// compile closure runs only after its claim).
fn held_compile<'s>(s: &'s std::thread::Scope<'s, '_>, p: &'s gravity_ir::Program, f: Func, go: mpsc::Receiver<()>) -> std::thread::ScopedJoinHandle<'s, Lookup> {
    let (claimed, is_claimed) = mpsc::channel();
    let h = s.spawn(move || {
        let (codes, o) = cache::get_or_compile_all(p, &[f], |ctx| {
            claimed.send(()).expect("the test waits for the claim");
            go.recv().expect("released");
            gravity_backend::gravity_tpde::compile(ctx.mcx(), p)
        })
        .expect("the held compile files its module");
        assert!(call(&codes[0]) > 0);
        o.lookup
    });
    is_claimed.recv_timeout(Duration::from_secs(30)).expect("the holder claimed the key");
    h
}

#[test]
fn a_waiter_with_a_pending_interrupt_leaves_and_the_held_compile_files_its_module() {
    let _cache = cache_lock();
    install();
    // tier 0 is read once per process (the other test forces it): this one asks the cache directly, past tier 0
    std::env::set_var("PGRUST_GRAVITY_TIER0", "force");
    let (p, f) = konst(38111);
    let (go, rx) = mpsc::channel();
    std::thread::scope(|s| {
        let holder = held_compile(s, &p, f, rx);
        let waited = cache::stats().waited;
        *PENDING.lock().unwrap() = Some(std::thread::current().id());
        let t0 = Instant::now();
        let e = cache::get_or_compile_all(&p, &[f], |_| panic!("the waiter compiles nothing")).err().expect("the wait ends at the pending interrupt");
        let el = t0.elapsed();
        *PENDING.lock().unwrap() = None;
        assert_eq!(e.message(), cache::INTERRUPTED);
        assert_eq!(cache::stats().waited, waited + 1, "it waited on the holder's flight (it compiled nothing of its own)");
        assert!(el < Duration::from_secs(1), "the waiter left within a slice, not after the compile: {el:?}");
        assert!(cache::lookup_all(&p, &[f]).is_none(), "nothing filed yet: the holder still compiles");
        go.send(()).unwrap();
        assert_eq!(holder.join().unwrap(), Lookup::Miss, "the holder compiled and filed the module");
    });
    let (codes, o) = cache::lookup_all(&p, &[f]).expect("filed by the holder");
    assert_eq!((o.lookup, call(&codes[0])), (Lookup::Hit, 38111), "the waiter's next look hits the holder's module: no poisoned key");
}

#[test]
fn a_promotion_stopped_in_its_wait_raises_the_interrupt_and_stays_promotable() {
    let _cache = cache_lock();
    install();
    std::env::set_var("PGRUST_GRAVITY_TIER0", "force");
    let (p, f) = konst(38112);
    let (go, rx) = mpsc::channel();
    std::thread::scope(|s| {
        // the holder claims the key first: the pool's job for the tier-0 miss then waits on its flight too
        let holder = held_compile(s, &p, f, rx);
        let (codes, o) = mapped_cached_all(&p, &[f]).unwrap();
        assert_eq!((codes[0].kind(), o.lookup), ("tier0", Lookup::Miss), "a forced miss interprets");
        assert_eq!(call(&codes[0]), 38112);
        let t = codes[0].tier0().expect("tier-0 code");
        *PENDING.lock().unwrap() = Some(std::thread::current().id());
        let t0 = Instant::now();
        let raised = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| t.promote())).err().expect("the promotion raises the interrupt");
        let el = t0.elapsed();
        assert_eq!(*PENDING.lock().unwrap(), None, "the host's check took the interrupt");
        let e = raised.downcast_ref::<Box<PgError>>().expect("PostgreSQL's error, thrown as an ereport is");
        assert_eq!(e.sqlstate(), types_error::error::ERRCODE_QUERY_CANCELED);
        assert!(el < Duration::from_secs(1), "the promotion's wait ended within a slice: {el:?}");
        assert!(t.failure().is_none() && !t.promoted(), "a cancel fails nothing: {:?}", t.failure());
        go.send(()).unwrap();
        assert_eq!(holder.join().unwrap(), Lookup::Miss);
        promote::quiesce();
        assert!(t.promote(), "the next promotion takes the module: {:?}", t.failure());
        assert_eq!((codes[0].kind(), call(&codes[0])), ("mapped", 38112));
    });
}

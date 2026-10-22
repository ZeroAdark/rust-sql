//! Tier 0's asynchronous promotion (C03 / DR-21, backend promote.rs) and the code cache's in-flight marker (cache.rs
//! `claim`): under `PGRUST_GRAVITY_TIER0=force` a miss interprets and its module is filed by the compile pool beside the run;
//! one key compiles once however many callers miss it at once; a compile that fails or panics leaves the key unfiled.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use gravity_backend::{cache, mapped_cached_all, promote, Lookup};
use gravity_ir::text::parse_program;
use gravity_ir::Func;

static CACHE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn cache_lock() -> std::sync::MutexGuard<'static, ()> {
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
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

#[test]
fn a_tier_zero_miss_is_filed_by_the_pool_beside_its_interpreted_run() {
    let _cache = cache_lock();
    std::env::set_var("PGRUST_GRAVITY_TIER0", "force");
    let (p, f) = konst(5101);
    let (filed, queued) = (promote::FILED.load(Ordering::Relaxed), promote::QUEUED.load(Ordering::Relaxed));
    let (codes, o) = mapped_cached_all(&p, &[f]).unwrap();
    assert_eq!((codes[0].kind(), o.lookup), ("tier0", Lookup::Miss), "a forced miss: tier 0");
    assert_eq!(call(&codes[0]), 5101, "interpreted");
    assert_eq!(promote::QUEUED.load(Ordering::Relaxed), queued + 1, "the module's compile went to the pool");
    promote::quiesce();
    assert_eq!(promote::FILED.load(Ordering::Relaxed), filed + 1, "the pool filed it");
    let misses = cache::stats().misses;
    assert!(cache::lookup_all(&p, &[f]).is_some(), "filed with no promotion");
    let t = codes[0].tier0().expect("tier-0 code");
    assert!(t.promote(), "{:?}", t.failure());
    assert_eq!((codes[0].kind(), call(&codes[0])), ("mapped", 5101), "promoted from the pool's module");
    assert_eq!(cache::stats().misses, misses, "the promotion compiled nothing");
}

#[test]
fn callers_missing_one_key_at_once_compile_it_once() {
    let _cache = cache_lock();
    let (p, f) = konst(5201);
    static COMPILES: AtomicUsize = AtomicUsize::new(0);
    let waited = cache::stats().waited;
    let outs: Vec<Lookup> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..4)
            .map(|i| {
                let p = &p;
                s.spawn(move || {
                    std::thread::sleep(Duration::from_millis(5 * i));
                    let (codes, o) = cache::get_or_compile_all(p, &[f], |ctx| {
                        COMPILES.fetch_add(1, Ordering::Relaxed);
                        // D3392: the compile stays open until the other three callers are counted waiting (a waiter is counted
                        // before it blocks), not for a fixed time: a caller scheduled late under load hit the filed module
                        let t0 = std::time::Instant::now();
                        while cache::stats().waited < waited + 3 && t0.elapsed() < Duration::from_secs(30) {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        gravity_backend::gravity_tpde::compile(ctx.mcx(), p)
                    })
                    .unwrap();
                    assert_eq!(call(&codes[0]), 5201);
                    o.lookup
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(COMPILES.load(Ordering::Relaxed), 1, "one compile for four callers: {outs:?}");
    assert_eq!(outs.iter().filter(|l| **l == Lookup::Miss).count(), 1, "{outs:?}");
    assert!(cache::stats().waited >= waited + 3, "the other three waited for it");
}

#[test]
fn a_compile_that_fails_or_panics_leaves_its_key_unfiled_and_wakes_its_waiters() {
    let _cache = cache_lock();
    let (p, f) = konst(5301);
    static TRIES: AtomicUsize = AtomicUsize::new(0);
    static CLAIMED: AtomicBool = AtomicBool::new(false);
    // the first caller's compile fails after its waiter has queued behind it: the waiter then compiles the key itself. The
    // waiter starts once the first caller's compile has begun (it holds the key's flight), not after a fixed sleep: on a
    // loaded box the first thread can start late and the waiter would take the key first (a dev precheck at load 118).
    let (first, second) = std::thread::scope(|s| {
        let p = &p;
        let a = s.spawn(move || {
            cache::get_or_compile_all(p, &[f], |_| {
                TRIES.fetch_add(1, Ordering::Relaxed);
                CLAIMED.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(200));
                Err(types_error::PgError::error("refused here".to_string()).into())
            })
            .map(|(_, o)| o.lookup)
        });
        while !CLAIMED.load(Ordering::Acquire) && !a.is_finished() {
            std::thread::yield_now();
        }
        let b = s.spawn(move || {
            cache::get_or_compile_all(p, &[f], |ctx| {
                TRIES.fetch_add(1, Ordering::Relaxed);
                gravity_backend::gravity_tpde::compile(ctx.mcx(), p)
            })
            .map(|(_, o)| o.lookup)
        });
        (a.join().unwrap(), b.join().unwrap())
    });
    assert!(first.is_err(), "the failed compile is its caller's error");
    assert_eq!(second.unwrap(), Lookup::Miss, "its waiter compiled the key itself");
    assert_eq!(TRIES.load(Ordering::Relaxed), 2);

    let (q, g) = konst(5302);
    let r = cache::get_or_compile_all(&q, &[g], |_| panic!("the code generator's assert"));
    assert!(r.is_err(), "a code generator panic is the compile's error");
    assert!(cache::lookup_all(&q, &[g]).is_none(), "not filed");
    let (codes, o) = cache::get_or_compile_all(&q, &[g], |ctx| gravity_backend::gravity_tpde::compile(ctx.mcx(), &q)).unwrap();
    assert_eq!((o.lookup, call(&codes[0])), (Lookup::Miss, 5302), "the key was never poisoned: the next miss compiles it");
}

/// D4762: the module the pool filed for a tier-0 program that never promoted is named by its codes (`Code::module`; their
/// mapping is 0), so the program tier's doom reaches it by its key and the next `evict_doomed` takes it (nothing holds it).
/// A doomed module a live code still holds (a promoted tier-0 code runs from it) stays until that code lets go: the pass
/// takes only modules whose map entry is the last holder.
#[test]
fn a_never_promoted_tier_zero_module_is_doomed_by_its_key_and_a_held_one_waits() {
    let _cache = cache_lock();
    std::env::set_var("PGRUST_GRAVITY_TIER0", "force");
    let (p, f) = konst(5762);
    let (q, g) = konst(5763);
    let (codes, o) = mapped_cached_all(&p, &[f]).unwrap();
    let (held, oq) = mapped_cached_all(&q, &[g]).unwrap();
    assert_eq!((codes[0].kind(), o.lookup, held[0].kind(), oq.lookup), ("tier0", Lookup::Miss, "tier0", Lookup::Miss), "forced misses: tier 0");
    promote::quiesce();
    assert!(cache::lookup_all(&p, &[f]).is_some() && cache::lookup_all(&q, &[g]).is_some(), "the pool filed both");
    assert_eq!((codes[0].mapping(), codes[0].module()), (0, o.key.0), "never promoted: no mapping, its module's key");
    let t = held[0].tier0().expect("tier-0 code");
    assert!(t.promote(), "{:?}", t.failure());
    assert_eq!((held[0].kind(), held[0].module()), ("mapped", oq.key.0), "promoted from the pool's module, which it holds");
    let entries = cache::stats().entries;
    assert_eq!(cache::doom_modules([codes[0].module(), held[0].module()].into_iter()), 2, "both doomed by their keys");
    assert!(cache::evict_doomed() > 0, "the unheld one evicted");
    assert_eq!(cache::stats().entries, entries - 1, "the held one stays");
    assert_eq!(call(&held[0]), 5763, "and runs");
    drop(held);
    assert!(cache::evict_doomed() > 0, "let go: the next pass takes it");
    assert!(cache::lookup_all(&p, &[f]).is_none() && cache::lookup_all(&q, &[g]).is_none(), "both gone from the cache");
    assert_eq!(call(&codes[0]), 5762, "the unpromoted code still runs interpreted");
}

//! Tier 0's compile pool stopped (backend promote.rs `stop`, its own test binary: a stop holds for the process): queued
//! jobs are dropped, the threads end, and a later miss's promotion compiles inline as before the pool.

use std::sync::atomic::Ordering;

use gravity_backend::{cache, mapped_cached_all, promote, Lookup};
use gravity_ir::text::parse_program;
use gravity_ir::Func;

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
fn a_stopped_pool_drops_its_jobs_ends_its_threads_and_promotion_compiles_inline() {
    std::env::set_var("PGRUST_GRAVITY_TIER0", "force");
    let (p, f) = konst(5401);
    let (codes, _) = mapped_cached_all(&p, &[f]).unwrap();
    assert_eq!(codes[0].kind(), "tier0");
    promote::quiesce();
    assert_eq!(promote::threads(), 1, "the pool started one thread");
    promote::stop();
    assert_eq!(promote::threads(), 0, "stopped: no thread left");

    let dropped = promote::DROPPED.load(Ordering::Relaxed);
    let (q, g) = konst(5402);
    let (c, o) = mapped_cached_all(&q, &[g]).unwrap();
    assert_eq!((c[0].kind(), o.lookup, call(&c[0])), ("tier0", Lookup::Miss, 5402), "a miss still interprets");
    assert_eq!(promote::DROPPED.load(Ordering::Relaxed), dropped + 1, "its job was dropped");
    assert!(cache::lookup_all(&q, &[g]).is_none(), "nothing filed it");
    let t = c[0].tier0().expect("tier-0 code");
    assert!(t.promote(), "{:?}", t.failure());
    assert_eq!((c[0].kind(), call(&c[0])), ("mapped", 5402), "promoted inline");
}

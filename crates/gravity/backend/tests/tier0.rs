//! H16 (d) (main 09-24): under the tier-0 knob (`PGRUST_GRAVITY_TIER0=force` here, read once per process: this binary's
//! one test sets it first) a code-cache miss hands back interpreted tier-0 code; its promotion compiles through the process
//! cache, after which a miss of the same IR elsewhere is a hit; a program whose hosts the interpreter refuses compiles at once.

use std::sync::atomic::Ordering;

use gravity_backend::{mapped_cached_all, Lookup, TIER0_REFUSED};
use gravity_ir::text::parse_program;
use gravity_ir::Func;

// D2553: the code cache is process-wide and one test resets it (cache::reset) while another expects its promotion to be a
// later hit; the tests here run one at a time.
static CACHE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn cache_lock() -> std::sync::MutexGuard<'static, ()> {
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

/// D2935: these tests pin the INLINE promotion arm (a miss stays a miss until the test promotes it). C03 DR-21's pool
/// (promote.rs) files a tier-0 miss's module beside its interpreted run, so with the pool on, a later miss of the same IR
/// is a hit whenever the pool wins the race (cgstress 16:00Z: 'mapped' for the D303 test's second program(4201) on a
/// 4-cpu scope). The pool's own arm is tier0_async.rs / tier0_stop.rs. Call under `cache_lock`.
fn force_tier0_inline() {
    std::env::set_var("PGRUST_GRAVITY_TIER0", "force");
    std::env::set_var(gravity_backend::promote::ASYNC_ENV, "off");
}

extern "C" fn h_fid(x: f64) -> f64 {
    x
}

/// `(ptr) -> (i64)` returning `k`; `float_host` declares a host with a float argument (the interpreter's refusal).
fn konst(k: i64, float_host: bool) -> (gravity_ir::Program, Func) {
    let host = if float_host { "(host fid (f64) f64 pure)" } else { "" };
    let src = format!("(program {host} (fn e ((c ptr)) (i64) (block entry ((c ptr)) (k (const i64 {k})) (return k))))");
    let p = parse_program(&src, &|name| (name == "fid").then_some(h_fid as extern "C" fn(f64) -> f64 as usize)).unwrap_or_else(|e| panic!("{e}"));
    let f = Func(p.funcs.iter().position(|f| f.name == "e").unwrap() as u32);
    (p, f)
}

fn call(code: &gravity_program::Code) -> i64 {
    // SAFETY: the program never reads its ctx.
    unsafe { code.call(core::ptr::null_mut()) }
}

#[test]
fn a_forced_miss_is_tier_zero_its_promotion_compiles_through_the_cache_and_a_refused_host_compiles_at_once() {
    let _cache = cache_lock();
    force_tier0_inline();
    let (p, f) = konst(4101, false);
    let (codes, o) = mapped_cached_all(&p, &[f]).unwrap();
    assert_eq!((codes[0].kind(), o.lookup, o.bytes), ("tier0", Lookup::Miss, 0), "a miss under force: tier 0, nothing compiled");
    assert_eq!(call(&codes[0]), 4101, "interpreted");
    let t = codes[0].tier0().expect("tier-0 code");
    assert!(t.promote(), "{:?}", t.failure());
    assert!(t.promoted());
    assert_eq!((codes[0].kind(), call(&codes[0])), ("mapped", 4101), "promoted: the compiled code answers");

    let (again, o2) = mapped_cached_all(&p, &[f]).unwrap();
    assert_eq!((again[0].kind(), o2.lookup), ("mapped", Lookup::Hit), "the promotion filed the module: a later miss of this IR is a hit");
    assert_eq!(call(&again[0]), 4101);

    let refused = TIER0_REFUSED.load(Ordering::Relaxed);
    let (fp, ff) = konst(4102, true);
    let (c, o3) = mapped_cached_all(&fp, &[ff]).unwrap();
    assert_eq!((c[0].kind(), o3.lookup, call(&c[0])), ("mapped", Lookup::Miss, 4102), "a float host: compiled at once");
    assert_eq!(TIER0_REFUSED.load(Ordering::Relaxed), refused + 1, "counted");
}

/// D303: two tier-0 programs of one descriptor shape and different IR have different content keys (the park key, the
/// worker-slot depot and the observed map key on it); one IR twice is one key.
#[test]
fn tier_zero_programs_of_one_shape_and_different_ir_have_different_content_keys() {
    let _cache = cache_lock();
    force_tier0_inline();
    let program = |k: i64| {
        let (p, f) = konst(k, false);
        let (mut codes, _) = mapped_cached_all(&p, &[f]).unwrap();
        let code = codes.pop().unwrap();
        assert_eq!(code.kind(), "tier0");
        // the worst interleaving, every run: what a pool queued for this miss is filed before the next lookup
        gravity_backend::promote::quiesce();
        let layout = gravity_program::Layout { nsites: 0, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 };
        gravity_program::Program::new(layout, &[], &[], gravity_program::WritePlan::SELECT, code)
    };
    let (a, b, a2) = (program(4201), program(4202), program(4201));
    assert_ne!(a.key, b.key, "different IR, one descriptor shape: different programs");
    assert_eq!(a.key, a2.key, "one IR: one program");
}

/// The tier-0 row's T1 (progcache.cases): a program's content key is its lowering's, whatever its code's tier or mapping.
/// Tier-0 code, the same IR mapped from the cache after the promotion, and mapped again after an eviction and a fresh compile
/// key one program (code-cache codes carry the IR's content key and the entry, as tier-0 code does: D303).
#[test]
fn one_ir_keys_one_program_across_its_promotion_and_a_recompile() {
    let _cache = cache_lock();
    force_tier0_inline();
    let layout = gravity_program::Layout { nsites: 0, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 };
    let key = |code| gravity_program::Program::new(layout, &[], &[], gravity_program::WritePlan::SELECT, code).key;
    let (p, f) = konst(4301, false);
    let (mut c, _) = mapped_cached_all(&p, &[f]).unwrap();
    let tiered = c.pop().unwrap();
    assert_eq!(tiered.kind(), "tier0");
    assert!(tiered.tier0().expect("tier-0 code").promote());
    let k_tier = key(tiered);
    let (mut c, o) = mapped_cached_all(&p, &[f]).unwrap();
    let hit = c.pop().unwrap();
    assert_eq!((hit.kind(), o.lookup, call(&hit)), ("mapped", Lookup::Hit, 4301));
    assert_eq!(key(hit), k_tier, "the promoted IR's cache hit keys the tier-0 program's key");
    // every holder dropped: the reset evicts the module, the next promotion compiles it again (a new mapping)
    gravity_backend::cache::reset();
    let (mut c, o) = mapped_cached_all(&p, &[f]).unwrap();
    assert_eq!(o.lookup, Lookup::Miss, "evicted");
    let again = c.pop().unwrap();
    assert!(again.tier0().expect("tier-0 code").promote());
    drop(again);
    let (mut c, o) = mapped_cached_all(&p, &[f]).unwrap();
    let recompiled = c.pop().unwrap();
    assert_eq!((recompiled.kind(), o.lookup, call(&recompiled)), ("mapped", Lookup::Hit, 4301));
    assert_eq!(key(recompiled), k_tier, "recompiled after an eviction: the same program key");
}

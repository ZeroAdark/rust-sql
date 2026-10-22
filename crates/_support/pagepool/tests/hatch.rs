//! pphatch: `PGRUST_ALLOCATOR=mimalloc` in the environment at the first allocation sends every `PoolAlloc` call to
//! mimalloc for the process's life: each block is mimalloc's, the pool stays off (no reservation, no claim, its switches
//! read off) and the choice is read once, kept by a forked child, which frees its parent's blocks to the same allocator.
//! `PGRUST_ALLOCATOR=hearth` (or the old `pagepool`) chooses the pool. Unset (or naming no allocator), this binary takes the
//! server's per-architecture default (`by_arch_default!`, the user 2026-10-18; x86_64 since D4872): the pool on Linux
//! aarch64 and x86_64, mimalloc elsewhere; a binary without it (every other test) keeps the pool. Each case is a child of this binary started with its
//! environment, since the choice is made before `main`.
#![cfg(all(feature = "global", not(target_family = "wasm"), not(miri), not(loom)))]

use pagepool::PoolAlloc;

#[global_allocator]
static A: PoolAlloc = PoolAlloc;
pagepool::by_arch_default!();

const PROBE: &str = "PPHATCH_PROBE";
/// What serves with `PGRUST_ALLOCATOR` naming no allocator, on this architecture.
const DEFAULT: &str = if pagepool::HEARTH_BY_ARCH { "pagepool" } else { "mimalloc" };

fn run_probe(allocator: Option<&str>, want: &str) {
    let mut c = std::process::Command::new(std::env::current_exe().expect("this test binary"));
    c.args(["--exact", "probe", "--nocapture", "--test-threads=1"]).env(PROBE, want).env("PGRUST_POOL_MCX", "1").env("PGRUST_POOL_RESERVE", "1");
    match allocator {
        Some(v) => c.env("PGRUST_ALLOCATOR", v),
        None => c.env_remove("PGRUST_ALLOCATOR"),
    };
    let out = c.output().expect("the probe child");
    let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success() && text.contains("probe ok"), "PGRUST_ALLOCATOR={allocator:?}:\n{text}");
}

#[test]
fn the_hatch_sends_every_call_to_mimalloc() {
    run_probe(Some("mimalloc"), "mimalloc");
    run_probe(Some("MiMalloc"), "mimalloc");
}

/// Named, the pool on every architecture; unset or naming no allocator, the architecture's default (the pool on Linux
/// aarch64 and x86_64, as before the per-architecture default).
#[test]
fn without_the_hatch_the_pool_serves() {
    run_probe(None, DEFAULT);
    run_probe(Some("pagepool"), "pagepool");
    run_probe(Some("hearth"), "pagepool");
    run_probe(Some("HEARTH"), "pagepool");
    run_probe(Some("jemalloc"), DEFAULT);
}

/// The per-architecture default (the user 2026-10-18: "for now can we run mimalloc on x86 and hearth on graviton?", then
/// D4872: "Yes, after one x86 check"), both rules read here (`hearth_chosen` is pure: the forced-arch probe), and this
/// architecture's own.
#[test]
fn the_default_is_the_architectures() {
    for (hearth, arch) in [(true, "Linux aarch64, x86_64"), (false, "macOS")] {
        assert_eq!(pagepool::hearth_chosen(None, hearth), hearth, "{arch}: unset = the default");
        assert_eq!(pagepool::hearth_chosen(Some("jemalloc"), hearth), hearth, "{arch}: naming no allocator = the default");
        assert_eq!(pagepool::hearth_chosen(Some(""), hearth), hearth, "{arch}: empty = the default");
        for v in ["hearth", "Hearth", "pagepool"] {
            assert!(pagepool::hearth_chosen(Some(v), hearth), "{arch}: {v} = Hearth");
        }
        assert!(!pagepool::hearth_chosen(Some("MiMalloc"), hearth), "{arch}: mimalloc = mimalloc");
    }
    assert_eq!(pagepool::HEARTH_BY_ARCH, cfg!(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64"))));
    assert!(pagepool::by_arch() && !pagepool::by_arch_late(), "by_arch_default!'s constructor ran before the first allocation");
    assert_eq!(pagepool::hearth_default(), pagepool::HEARTH_BY_ARCH, "this binary takes the server's default");
}

/// D4872 (the user, "Yes, after one x86 check": x86_64 Linux with SPAN_FIT off bulk-loads ~5.5 % faster than
/// mimalloc, analytic query suites no worse): on x86_64 Linux the server's default is Hearth, as on Graviton, and the hatch still
/// wins. Born red: before it, this binary on x86_64 Linux was served by mimalloc with PGRUST_ALLOCATOR unset.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn x86_64_linux_defaults_to_hearth_and_the_hatch_still_wins() {
    assert!(pagepool::HEARTH_BY_ARCH && pagepool::hearth_default(), "x86_64 Linux: the server's default is Hearth");
    run_probe(None, "pagepool");
    run_probe(Some("mimalloc"), "mimalloc");
}

/// The blocks of every road (small tier, buckets, runs, a span), zeroed, grown across roads: whose they are.
fn blocks_are(hatch: bool) {
    let sized: Vec<Vec<u8>> = [8usize, 100, 1000, 4096, 70_000, 3 << 20].iter().map(|&n| vec![1u8; n]).collect();
    let zeroed = vec![0u64; 1 << 18];
    assert!(zeroed.iter().all(|&x| x == 0));
    let mut grown = Vec::<u8>::with_capacity(16);
    for i in 0..300_000u32 {
        grown.push(i as u8);
    }
    let big = [sized[4].as_ptr(), sized[5].as_ptr(), zeroed.as_ptr().cast(), grown.as_ptr()];
    if hatch {
        for p in sized.iter().map(|b| b.as_ptr()).chain(big) {
            assert!(hatch_block(p) && !pagepool::owns(p), "{p:?}: mimalloc's");
        }
    } else {
        // the small tier may lay a run in the system's memory while the pool starts: the big roads are the pool's
        assert!(big.iter().all(|&p| pagepool::owns(p)), "the pool's");
    }
}

/// A block of the hatch's allocator: mimalloc's.
#[cfg(not(pagepool_tsan))]
fn hatch_block(p: *const u8) -> bool {
    // SAFETY: a live block's address, only compared.
    unsafe { libmimalloc_sys::mi_is_in_heap_region(p.cast()) }
}

/// Under cfg(pagepool_tsan) the hatch forwards to System and no mimalloc is linked (its malloc override would take malloc
/// from TSan): any block not the pool's.
#[cfg(pagepool_tsan)]
fn hatch_block(_: *const u8) -> bool {
    true
}

#[test]
fn probe() {
    let Ok(want) = std::env::var(PROBE) else { return };
    let hatch = want == "mimalloc";
    assert_eq!(pagepool::hatch_on(), hatch);
    assert_eq!(pagepool::pool_allocates(), !hatch);
    assert_eq!(pagepool::allocator_decides(), 1, "read once");
    blocks_are(hatch);
    assert_eq!(pagepool::switches().mcx, !hatch, "PGRUST_POOL_MCX=1 inert under the hatch");
    assert_eq!(pagepool::reserve_on(), !hatch, "PGRUST_POOL_RESERVE=1 too");
    assert_eq!(pagepool::global_on(), !hatch);
    assert_eq!(pagepool::on(), !hatch);
    let c = pagepool::small_counts();
    assert_eq!(c.fresh == 0, hatch, "the small tier made runs only for the pool: {}", c.fresh);
    if hatch {
        assert_eq!((c.adopted, c.spare, c.retired, c.abandoned), (0, 0, 0, 0), "and no run went through its stacks");
        assert_eq!(pagepool::thread_retained(), 0);
        assert_eq!(pagepool::range(), (0, 0), "no reservation");
        assert!(!pagepool::init(&pagepool::Config::from_policy()), "init refuses");
        assert!(!pagepool::start(), "no pool thread");
        assert_eq!(pagepool::stats().claimed, 0);
    } else {
        assert!(pagepool::stats().claimed > 0);
    }
    let inherited = vec![7u8; 100_000];
    // SAFETY: the child runs on this thread alone and leaves by _exit.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let ok = std::panic::catch_unwind(move || {
            assert_eq!(pagepool::allocator_decides(), 1, "inherited, not read again");
            assert_eq!(pagepool::hatch_on(), hatch);
            assert!(inherited.iter().all(|&b| b == 7));
            drop(inherited);
            blocks_are(hatch);
            assert_eq!(pagepool::small_counts().fresh == 0, hatch);
        })
        .is_ok();
        // SAFETY: ends the forked child without running the parent's exit work.
        unsafe { libc::_exit(if ok { 0 } else { 1 }) };
    }
    assert!(pid > 0, "fork");
    let mut status = 0;
    // SAFETY: our own child.
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0, "the forked child: {status:#x}");
    assert_eq!(pagepool::allocator_decides(), 1);
    println!("probe ok");
}

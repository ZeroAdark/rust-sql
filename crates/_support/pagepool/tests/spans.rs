//! Spans: a pool span grows in place; a lazy span grows by mremap when its neighbour is taken; a warm lazy span is reused
//! with no fault; commit_to's accounting is exact; guard_past traps past the guard (a forked child).

mod common;
use common::*;
use pagepool::*;

fn h() -> Hint {
    Hint::new(Class::Task, Tag::PROGRAM)
}

#[test]
fn base_pages_needs_pool_alloc() {
    setup();
    assert!(base_pages(8 * MIB, || ()).is_none(), "PoolAlloc does not serve this binary: the caller's own road");
}

#[test]
#[cfg_attr(pagepool_tsan, ignore = "TSan: its SEGV handler exits 66, so guard_past_traps' child never dies by SIGSEGV")]
fn spans() {
    setup();
    pool_span_grows_in_place();
    lazy_span_grows_by_mremap();
    warm_lazy_span_reused_faults_zero();
    commit_to_is_exact();
    guard_past_traps();
    flush_shelf();
    balanced();
}

fn pool_span_grows_in_place() {
    let mut s = span(4 * MIB, h()).unwrap();
    assert!(!s.is_lazy() && s.len() == 4 * MIB);
    let base = s.as_ptr();
    touch(base as usize, s.len());
    // SAFETY: ours.
    assert!(unsafe { grow_span(&mut s, 7 * MIB) }, "the next superpages of a fresh chunk are free");
    assert_eq!((s.as_ptr(), s.len()), (base, 8 * MIB));
    let info = run_of((base as usize + 7 * MIB) as *const u8).unwrap();
    assert_eq!((info.kind, info.base), (Kind::Span { superpages: 4 }, base as usize));
    // SAFETY: ours.
    unsafe { shrink_span(&mut s, 3 * MIB) };
    assert_eq!(s.len(), 4 * MIB);
    // SAFETY: ours, not used again.
    unsafe { give_span(s) };
}

fn lazy_span_grows_by_mremap() {
    let mut a = span(64 * MIB, h()).unwrap();
    let b = span(64 * MIB, h()).unwrap();
    assert!(a.is_lazy() && b.is_lazy());
    // SAFETY: ours.
    unsafe {
        commit_to(&mut a, 64 * MIB);
        for k in 0..32 {
            a.as_ptr().add(k * SUPER).write(k as u8 + 1);
        }
    }
    let old = a.as_ptr();
    let c0 = stats().claimed;
    // SAFETY: ours.
    let grown = unsafe { grow_span(&mut a, 128 * MIB) };
    if !grown {
        eprintln!("SKIP lazy grow by mremap: refused (kernel without MREMAP_DONTUNMAP?)");
    } else {
        assert_ne!(a.as_ptr(), old, "moved: the next address space is the neighbour's");
        assert_eq!(a.len(), 128 * MIB);
        // SAFETY: ours, moved with its pages.
        for k in 0..32 {
            assert_eq!(unsafe { a.as_ptr().add(k * SUPER).read() }, k as u8 + 1);
        }
        assert_eq!(stats().claimed, c0, "a move claims nothing");
        assert_eq!(resident(old as usize, 64 * MIB), 0, "the old range holds no pages");
    }
    // SAFETY: ours, not used again.
    unsafe {
        give_span(a);
        give_span(b);
    }
}

fn warm_lazy_span_reused_faults_zero() {
    // card 2 (ppround): exact, or a loud SKIP if the host swapped since the first touch (a swapped page faults back in)
    let sw = swap_counts();
    let mut c = span(48 * MIB, h()).unwrap();
    // SAFETY: ours.
    unsafe { commit_to(&mut c, 48 * MIB) };
    touch(c.as_ptr() as usize, c.len());
    let base = c.as_ptr();
    // SAFETY: ours, not used again.
    unsafe { give_span(c) };
    let d = span(48 * MIB, h().expect_kib(48 << 10)).unwrap();
    assert_eq!(d.as_ptr(), base, "the warm span came back");
    assert_eq!(d.committed(), 48 * MIB);
    let f0 = minflt();
    touch(d.as_ptr() as usize, d.len());
    let faults = minflt() - f0;
    eprintln!("MEASURED warm lazy span reuse: {faults} faults over 48 MiB");
    swap_exact!(sw, faults <= HOST_SLACK, "{faults} faults on a warm span");
    // SAFETY: ours, not used again.
    unsafe { give_span(d) };
}

fn commit_to_is_exact() {
    let mut e = span(256 * MIB, h()).unwrap();
    let s0 = stats();
    // SAFETY: ours.
    unsafe { commit_to(&mut e, 10 * MIB) };
    let s1 = stats();
    assert_eq!(s1.claimed - s0.claimed, 10 * MIB);
    assert_eq!(s1.lazy_committed - s0.lazy_committed, 10 * MIB);
    assert_eq!(s1.in_use[Tag::PROGRAM.0 as usize] - s0.in_use[Tag::PROGRAM.0 as usize], 10 * MIB);
    // SAFETY: ours.
    unsafe { commit_to(&mut e, 5 * MIB) };
    assert_eq!(stats().claimed, s1.claimed, "commit_to never shrinks");
    // SAFETY: ours.
    unsafe { commit_to(&mut e, 10 * MIB + 1) };
    assert_eq!(stats().claimed - s1.claimed, hooks::page_size());
    // SAFETY: ours, not used again.
    unsafe { give_span(e) };
}

fn guard_past_traps() {
    let mut f = span(64 * MIB, h()).unwrap();
    // SAFETY: ours.
    assert!(unsafe { guard_past(&mut f, MIB) });
    let base = f.as_ptr() as usize;
    for (off, want_segv) in [(4096usize, false), (2 * MIB, true)] {
        // SAFETY: fork in a test: the child only stores one byte and exits.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // SAFETY: inside our span; past the guard it traps.
            unsafe {
                core::ptr::write_volatile((base + off) as *mut u8, 7);
                libc::_exit(0);
            }
        }
        let mut st = 0;
        // SAFETY: our own child.
        assert_eq!(unsafe { libc::waitpid(pid, &mut st, 0) }, pid);
        let segv = libc::WIFSIGNALED(st) && libc::WTERMSIG(st) == libc::SIGSEGV;
        assert_eq!(segv, want_segv, "store at +{off:#x}: status {st:#x}");
    }
    // SAFETY: ours, not used again.
    unsafe { give_span(f) };
}

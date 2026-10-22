// The arena's two block forms (arena.rs `Block`: boxed under a huge page, mapped from 2 MiB up) keep one
// contract: unused memory is zero, `zero` / `reset_to` re-zero, exhaustion is refused.

use gravity_program::Arena;

fn exercise(bytes: usize) {
    let arena = Arena::new(bytes);
    let a = arena.handle();
    assert_eq!(a.used(), 0);
    assert!(a.capacity() >= bytes);
    let m0 = a.mark();
    let p = a.alloc(24);
    assert_eq!(a.offset_of(p), 0);
    assert_eq!(a.used(), 24);
    let s = unsafe { core::slice::from_raw_parts_mut(p, 24) };
    assert!(s.iter().all(|&b| b == 0));
    s.fill(0xab);
    let tail = a.alloc(a.capacity() - a.used() - 8);
    let t = unsafe { core::slice::from_raw_parts(tail.add(a.capacity() - 64), 48) };
    assert!(t.iter().all(|&b| b == 0));
    assert!(a.try_alloc(16).is_none(), "exhausted past the capacity");
    a.zero(8, 8);
    assert_eq!(&arena.bytes(24)[..], &[0xab; 8].iter().chain([0u8; 8].iter()).chain([0xab; 8].iter()).copied().collect::<Vec<_>>()[..]);
    a.reset_to(m0);
    assert_eq!(a.used(), 0);
    let q = a.alloc(24);
    assert_eq!(q, p);
    assert!(unsafe { core::slice::from_raw_parts(q, 24) }.iter().all(|&b| b == 0), "reset_to re-zeroes");
}

#[test]
fn boxed_block_under_a_huge_page() {
    exercise(64 << 10);
}

#[test]
fn mapped_block_from_a_huge_page_up() {
    exercise(2 << 20);
    exercise(64 << 20);
}

#[test]
fn a_mapped_arena_is_built_without_touching_its_pages() {
    // A mapping, not 64 MiB of zero-fill (the calloc form was 2.3–3.4 ms per backend). D3317: counted in this thread's page
    // faults, not in wall time (a box under load took 349 ms for the 8 untouched mappings): building an arena touches its
    // header only, where a zero fill faults every page of it (8 x 64 MiB: 131072 4 KiB faults, or 256 of 2 MiB).
    let f0 = minor_faults();
    let arenas: Vec<Arena> = (0..8).map(|_| Arena::new(64 << 20)).collect();
    let faults = minor_faults() - f0;
    assert_eq!(arenas.iter().map(|a| a.handle().used()).sum::<usize>(), 0);
    assert!(faults < 8 * 8, "8 x 64 MiB arenas faulted {faults} pages in: they were touched, not mapped");
}

/// The calling thread's minor page faults so far (`getrusage(RUSAGE_THREAD)`; the whole process elsewhere).
fn minor_faults() -> i64 {
    #[cfg(target_os = "linux")]
    let who = libc::RUSAGE_THREAD;
    #[cfg(not(target_os = "linux"))]
    let who = libc::RUSAGE_SELF;
    // SAFETY: a zeroed rusage the call fills
    let mut r: libc::rusage = unsafe { core::mem::zeroed() };
    // SAFETY: `r` is a valid rusage for the call to write
    assert_eq!(unsafe { libc::getrusage(who, &mut r) }, 0);
    r.ru_minflt as i64
}

/// D1703: a boxed block's words are written through the box's own pointer (the header at `Arena::new`, the bump and
/// the bytes through the handle). The block's pointer was `as_ptr` of a shared borrow of the box, which Stacked / Tree
/// Borrows make read-only: every write through it was UB that plain `cargo test` never shows. The proof runs this under
/// `MIRIFLAGS=-Zmiri-tree-borrows cargo +nightly miri test -p gravity_program --test arena`.
#[test]
fn a_boxed_block_is_written_through_the_boxs_own_pointer() {
    let arena = Arena::new(256);
    let a = arena.handle();
    let m0 = a.mark();
    let p = a.alloc(24);
    unsafe { core::slice::from_raw_parts_mut(p, 24) }.fill(0xab);
    assert_eq!((a.used(), a.capacity()), (24, 256));
    assert_eq!(arena.bytes(24), &[0xab; 24]);
    a.zero(8, 8);
    assert_eq!(&arena.bytes(24)[8..16], &[0; 8]);
    a.reset_to(m0);
    assert!(unsafe { core::slice::from_raw_parts(a.alloc(24), 24) }.iter().all(|&b| b == 0), "reset_to re-zeroes");
}

/// D2487 / D3933: an arena collection growing past the capacity unwinds with a PgError (54000, the arena's message), so
/// the statement ends as an ERROR: not an `AllocError` that `handle_alloc_error` turns into a server abort, and not a
/// plain panic, which the statement boundary takes as a crash since D2933 (D3933: a 80-way join's Node arena).
#[test]
fn an_arena_vec_past_the_capacity_raises_an_error_not_aborts() {
    let arena = Arena::new(4 << 10);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut v = gravity_program::ArenaVec::<u64>::new_in(arena.handle());
        for i in 0..1024u64 {
            v.push(i);
        }
    }));
    let payload = r.expect_err("past the capacity");
    let e = payload.downcast_ref::<types_error::PgError>().expect("a PgError payload, not a plain panic");
    assert_eq!(e.sqlstate(), types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED);
    assert!(e.message().contains("arena is exhausted"), "{}", e.message());
    let h = arena.handle();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h.alloc(8 << 10)));
    assert!(r.expect_err("alloc past the capacity").downcast_ref::<types_error::PgError>().is_some(), "alloc too");
}

/// D2487: a scope resets its arena when dropped, on an unwind too, so the statement after one that ran the arena out
/// finds it empty again (the graph admission's arena lives as long as the backend).
#[test]
fn a_scope_resets_its_arena_on_return_and_on_unwind() {
    let arena = std::sync::Arc::new(Arena::new(4 << 10));
    {
        let a = arena.handle();
        let _s = a.scope();
        a.alloc(64);
        assert_eq!(a.used(), 64);
    }
    assert_eq!(arena.handle().used(), 0, "reset on return");
    let held = arena.clone();
    let r = std::thread::spawn(move || {
        let a = held.handle();
        let _s = a.scope();
        a.alloc(64);
        let mut v = gravity_program::ArenaVec::<u64>::new_in(a);
        v.extend(0..1024u64);
    })
    .join();
    assert!(r.is_err(), "the overflow panicked");
    assert_eq!(arena.handle().used(), 0, "reset on the unwind");
}

/// Landing 20: the arena's last vector grows in place (its doublings cost its final size, not twice it); a vector with a
/// block after it moves and keeps its elements.
#[test]
fn the_last_vector_grows_in_place_and_others_move() {
    let arena = Arena::new(1 << 20);
    let a = arena.handle();
    let mut v = gravity_program::ArenaVec::<u64>::new_in(a);
    v.push(0);
    let first = v.as_ptr();
    v.extend(1..4096u64);
    assert_eq!(v.as_ptr(), first, "the top block grew in place");
    assert_eq!(a.used(), v.capacity() * 8, "no buffer left behind");
    let mut w = gravity_program::ArenaVec::<u64>::new_in(a);
    w.push(7);
    v.extend(4096..8193u64);
    assert_ne!(v.as_ptr(), first, "a block below another one moves");
    assert!(v.iter().copied().eq(0..8193u64) && w[0] == 7);
}

#[test]
fn a_reset_past_the_kept_prefix_reads_zero_and_the_arena_refills() {
    // the graph arena is a reservation (landing 20): a reset hands the pages above KEEP_BYTES back and must still leave
    // the whole span zero, the unaligned tail included, and usable again
    let keep = gravity_program::arena::KEEP_BYTES;
    let arena = Arena::new(keep + (8 << 20) + 40);
    let a = arena.handle();
    let m0 = a.mark();
    for _ in 0..2 {
        let n = a.capacity() - 16;
        let p = a.alloc(n);
        unsafe { core::ptr::write_bytes(p, 0x5a, n) };
        a.reset_to(m0);
        assert_eq!(a.used(), 0);
        let q = a.alloc(n);
        let s = unsafe { core::slice::from_raw_parts(q, n) };
        for off in [0, keep - 8, keep, keep + 4096, n - 4097, n - 1] {
            assert_eq!(s[off], 0, "byte {off} survived the reset");
        }
        assert!(s.iter().all(|&b| b == 0));
        a.reset_to(m0);
    }
}

/// D3957: a refused mapping (a size past every address space: refused with no rail to set) is `new`'s ERROR 53200, as C's
/// palloc failure is, never a plain panic (the backend's abort since D2933); `try_new` is `None`, a size past `usize` too.
#[cfg(target_pointer_width = "64")]
#[test]
fn a_refused_mapping_raises_53200_never_a_plain_panic() {
    let payload = std::panic::catch_unwind(|| drop(Arena::new(1 << 60))).expect_err("a 1 EiB mapping is refused");
    let e = payload.downcast_ref::<types_error::PgError>().expect("a PgError payload (an ERROR), not a plain panic");
    assert_eq!(e.sqlstate(), types_error::ERRCODE_OUT_OF_MEMORY);
    assert_eq!(e.message(), "out of memory");
    assert!(e.detail().is_some_and(|d| d.starts_with("Failed on request of size ")), "{:?}", e.detail());
    assert!(Arena::try_new(1 << 60).is_none());
    assert!(Arena::try_new(usize::MAX).is_none(), "a size past usize is refused, not wrapped");
}

/// D2845: a mapping the kernel refuses (an address-space rail, `RLIMIT_AS`) is `try_new`'s `None`, never a panic; the
/// rail is set in a forked child (the test binary's other threads keep theirs), which reports by its exit code.
#[cfg(target_os = "linux")]
#[test]
fn a_refused_mapping_is_none_never_a_panic() {
    // SAFETY: the child calls only getrlimit / setrlimit / mmap (try_new's mapped form allocates nothing) and _exit.
    unsafe {
        let pid = libc::fork();
        assert!(pid >= 0, "fork");
        if pid == 0 {
            let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            let code = if libc::getrlimit(libc::RLIMIT_AS, &mut lim) != 0 {
                3
            } else if libc::setrlimit(libc::RLIMIT_AS, &libc::rlimit { rlim_cur: 64 << 20, rlim_max: lim.rlim_max }) != 0 {
                2
            } else {
                let refused = Arena::try_new(1 << 30);
                let code = if refused.is_none() { 0 } else { 1 };
                core::mem::forget(refused);
                code
            };
            libc::_exit(code);
        }
        let mut status = 0;
        assert_eq!(libc::waitpid(pid, &mut status, 0), pid);
        assert!(libc::WIFEXITED(status), "the child exits ({status:#x})");
        assert_eq!(libc::WEXITSTATUS(status), 0, "1: a 1 GiB mapping past a 64 MiB rail was not refused; 2/3: the rail was not set");
    }
    assert!(Arena::try_new(1 << 30).is_some(), "without the rail the mapping is made");
}

//! pagepool P3 (F1): `PGRUST_POOL_FENCE=1`: every lazy span keeps the slot after it PROT_NONE, so each span is a mapping
//! of its own (one VMA) through its open, growth in place, move, shrink and warm reuse; nothing else changes.

mod common;
use common::*;
use pagepool::*;

fn h() -> Hint {
    Hint::new(Class::Task, Tag::PROGRAM)
}

/// The mappings (`/proc/self/maps`): (start, end, perms).
#[cfg(target_os = "linux")]
fn maps() -> Vec<(usize, usize, String)> {
    let mut v = Vec::new();
    for l in std::fs::read_to_string("/proc/self/maps").unwrap().lines() {
        let mut f = l.split_whitespace();
        let (r, p) = (f.next().unwrap(), f.next().unwrap());
        let (lo, hi) = r.split_once('-').unwrap();
        v.push((usize::from_str_radix(lo, 16).unwrap(), usize::from_str_radix(hi, 16).unwrap(), p.to_string()));
    }
    v
}

/// `s` is read-write mappings of its own (none reaches past either end), and the 2 MiB past it is PROT_NONE.
#[cfg(target_os = "linux")]
fn alone(s: &Span, what: &str) {
    let (lo, hi) = (s.as_ptr() as usize, s.as_ptr() as usize + s.len());
    let m = maps();
    let mine: Vec<_> = m.iter().filter(|(a, b, _)| *a < hi && *b > lo).collect();
    assert!(!mine.is_empty(), "{what}: mapped");
    assert_eq!((mine[0].0, mine[mine.len() - 1].1), (lo, hi), "{what}: no mapping shared with a neighbour: {mine:x?}");
    assert!(mine.iter().all(|(_, _, p)| p == "rw-p"), "{what}: read-write: {mine:x?}");
    let fence = m.iter().find(|(a, b, _)| *a <= hi && hi < *b).expect("the fence is reserved");
    assert_eq!((fence.0, fence.2.as_str()), (hi, "---p"), "{what}: its fence is PROT_NONE");
}

#[cfg(not(target_os = "linux"))]
fn alone(_s: &Span, _what: &str) {}

#[test]
fn lazy_spans_are_fenced() {
    hooks::fence_switch(true);
    setup();
    // the first span, then its growth in place over its own fence (the fence moves to the new end)
    let mut a = span(64 * MIB, h()).unwrap();
    assert!(a.is_lazy());
    let a0 = a.as_ptr();
    alone(&a, "a, new");
    // SAFETY: ours.
    unsafe {
        commit_to(&mut a, 64 * MIB);
        touch(a0 as usize, 64 * MIB);
        assert!(grow_span(&mut a, 96 * MIB), "nothing past the last span's fence");
    }
    assert_eq!((a.as_ptr(), a.len()), (a0, 96 * MIB), "in place");
    alone(&a, "a, grown in place");

    // a neighbour starts one fence slot past it
    let b = span(64 * MIB, h()).unwrap();
    assert_eq!(b.as_ptr() as usize, a0 as usize + a.len() + SUPER, "one fence slot between neighbours");
    alone(&b, "b, new");

    // a cannot grow in place over b: it moves, and its old fence goes with its old range
    // SAFETY: ours.
    if unsafe { grow_span(&mut a, 128 * MIB) } {
        assert_ne!(a.as_ptr(), a0);
        alone(&a, "a, moved");
        alone(&b, "b, after a moved");
        // a new span past the bump is fenced too
        let c = span(64 * MIB, h()).unwrap();
        alone(&c, "c, new");
        // SAFETY: ours, not used again.
        unsafe { give_span(c) };
    } else {
        eprintln!("SKIP the move: refused (kernel without MREMAP_DONTUNMAP?)");
    }

    // a shrink moves the fence down to the new end
    let (a1, alen) = (a.as_ptr(), a.len());
    // SAFETY: ours; nothing past 32 MiB is used again.
    unsafe {
        commit_to(&mut a, alen);
        touch(a1 as usize, alen);
        shrink_span(&mut a, 32 * MIB);
    }
    assert_eq!(a.len(), 32 * MIB);
    alone(&a, "a, shrunk");

    // a warm span comes back with its fence
    // SAFETY: ours, not used again.
    unsafe { give_span(a) };
    let d = span(32 * MIB, h().expect_kib(32 << 10)).unwrap();
    assert_eq!(d.as_ptr(), a1, "the warm span is reused");
    alone(&d, "d, warm reuse");
    // SAFETY: ours, not used again.
    unsafe {
        give_span(b);
        give_span(d);
    }
    flush_shelf();
    balanced();
}

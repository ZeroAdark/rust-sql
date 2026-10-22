//! D4705: an ask too big for any road is refused, never served short. Sizes are compared before they narrow to u32
//! (2^60 bytes once read as 0 superpages and came back as a zero-length span).

mod common;
use common::*;
use pagepool::frames::{self, Mode};
use pagepool::*;

const HUGE: [usize; 4] = [1 << 60, (1 << 32) * SUPER, usize::MAX / 2, usize::MAX];

#[test]
fn huge_asks_are_refused() {
    setup();
    let h = Hint::new(Class::Task, Tag::OTHER);
    let before = stats();
    for &n in &HUGE {
        assert!(take(n, 16, h).is_none(), "take({n:#x})");
        assert!(take(n, SUPER, h).is_none(), "take({n:#x}, 2 MiB)");
        assert!(span(n, h).is_none(), "span({n:#x})");
        assert!(usable(n, 16) >= SUPER.min(n), "usable({n:#x})");
    }
    let mut pool = span(2 * SUPER, h).expect("a pool span");
    let mut lazy = span(64 * MIB, h).expect("a lazy span");
    let p = take(4 << 16, 1 << 16, h).expect("runs");
    for &n in &HUGE {
        // SAFETY: our own spans; a refused grow leaves them as they were.
        unsafe {
            assert!(!grow_span(&mut pool, n), "grow_span(pool, {n:#x})");
            assert!(!grow_span(&mut lazy, n), "grow_span(lazy, {n:#x})");
            assert!(resize(p.cast(), 4 << 16, n, 1 << 16).is_none(), "resize(runs, {n:#x})");
            assert!(guard_past(&mut lazy, n), "guard_past past the end is a no-op");
        }
        if let Some(z) = frames::zone() {
            // SAFETY: frames' own PROT_NONE range; a refused commit maps nothing.
            assert!(unsafe { frames::commit(z.lo(), n & !(hooks::page_size() - 1), true, Mode::Move, Tag::FRAME) }.is_none(), "commit({n:#x})");
        }
    }
    assert_eq!((pool.len(), lazy.len()), (2 * SUPER, 64 * MIB), "refused grows changed nothing");
    // SAFETY: ours, not used again.
    unsafe {
        give(p.cast(), 4 << 16);
        give_span(pool);
        give_span(lazy);
    }
    flush_shelf();
    assert_eq!(stats().in_use_total(), before.in_use_total());
    balanced();
}

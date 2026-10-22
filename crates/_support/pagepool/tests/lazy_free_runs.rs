//! pagepool P3 (F1, the third class; after ppsmall's idle_free_runs): with pool_lazy_decay_ms, the free runs of
//! superpages still in use go back at the second age mark that finds them free (a MADV_DONTNEED inside the superpage,
//! never a whole one), never a run taken between the marks; the runs kept in use keep their bytes.

mod common;
use common::*;
use pagepool::*;

#[test]
fn lazy_free_runs() {
    init_with(|c| c.lazy_decay_ms = 50);
    let h = Hint::new(Class::Task, Tag::OTHER);
    let run = 64 << 10;
    let k = 12;
    let blocks: Vec<_> = (0..16).map(|_| take(k * run, run, h).expect("a runs-road block").cast::<u8>()).collect();
    for b in &blocks {
        // SAFETY: ours.
        unsafe { core::ptr::write_bytes(b.as_ptr(), 0x5a, k * run) };
    }
    // one block kept per superpage, the rest given: free runs inside superpages still in use
    let (mut kept, mut given, mut supers) = (Vec::new(), Vec::new(), std::collections::HashSet::new());
    for b in blocks {
        if supers.insert(b.as_ptr() as usize >> 21) {
            kept.push(b);
        } else {
            given.push(b.as_ptr() as usize);
            // SAFETY: ours, not used again.
            unsafe { give(b, k * run) };
        }
    }
    flush_shelf();
    assert!(given.len() >= 2, "free runs beside a kept block: {} given", given.len());
    assert!(given.iter().all(|&a| resident(a, k * run) > 0));
    assert_eq!(lazy_pass_now(), 0, "the first mark only sees them");
    // one free stretch taken again between the marks
    let again = take(k * run, run, h).expect("a runs-road block").cast::<u8>();
    let aa = again.as_ptr() as usize;
    assert!(given.contains(&aa), "the free stretch of a superpage in use is taken first");
    // SAFETY: ours.
    unsafe { core::ptr::write_bytes(again.as_ptr(), 0x6b, k * run) };
    let r0 = stats().releases;
    // the given stretches still free, and each superpage's tail never handed out (32 - 2k runs; no page there: no remap)
    let tails = supers.len() * (32 - 2 * k) * run;
    assert_eq!(lazy_pass_now(), (given.len() - 1) * k * run + tails, "the second mark gives back the runs still free");
    assert!(stats().releases > r0);
    for &a in given.iter().filter(|&&a| a != aa) {
        assert_eq!(resident(a, k * run), 0);
    }
    assert!(resident(aa, k * run) >= k * run, "never a run taken between the marks");
    for b in &kept {
        assert!(resident(b.as_ptr() as usize, k * run) >= k * run, "the runs in use keep their bytes");
        // SAFETY: ours.
        assert!(unsafe { core::slice::from_raw_parts(b.as_ptr(), k * run) }.iter().all(|&x| x == 0x5a));
    }
    // SAFETY: ours, not used again.
    unsafe {
        give(again, k * run);
        for b in kept {
            give(b, k * run);
        }
    }
    flush_shelf();
    balanced();
}

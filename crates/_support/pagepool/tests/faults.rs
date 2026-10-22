//! Warm reuse removes faults: 1,000 take / give rounds of 8 KiB and 4 MiB after one warm pass fault 0; a WarmZero
//! `Dense` take does no fill and faults 0; a Cold superpage faults at most once per 2 MiB under THP.

mod common;
use common::*;
use pagepool::{give, give_span, hooks, prefill, span, take, Class, Hint, Tag, Zero};

#[test]
#[cfg_attr(pagepool_tsan, ignore = "TSan: its shadow memory faults too, so the minor-fault counts run high")]
fn faults() {
    setup();
    warm_takes_fault_zero();
    warm_zero_dense_skips_the_fill();
    cold_superpage_faults_once_per_2mib();
    balanced();
}

fn warm_takes_fault_zero() {
    let h = Hint::new(Class::Task, Tag::OTHER);
    let round = || {
        for size in [8 << 10, 4 * MIB] {
            let p = take(size, 16, h).unwrap();
            touch(p.cast::<u8>().as_ptr() as usize, size);
            // SAFETY: taken above, not used again.
            unsafe { give(p.cast(), size) };
        }
    };
    round();
    let f0 = minflt();
    for _ in 0..1000 {
        round();
    }
    let faults = minflt() - f0;
    eprintln!("MEASURED warm-take faults: {faults} over 1000 rounds of 8 KiB + 4 MiB");
    assert!(faults <= HOST_SLACK, "{faults} faults over 1000 warm rounds");
}

fn warm_zero_dense_skips_the_fill() {
    assert_eq!(prefill(4 * SUPER, 1), 4 * SUPER);
    let (z0, f0) = (hooks::zero_fills(), minflt());
    let s = span(SUPER, Hint::new(Class::Task, Tag::OTHER).zero(Zero::Dense)).unwrap();
    // SAFETY: the span we hold.
    let bytes = unsafe { core::slice::from_raw_parts(s.as_ptr(), s.len()) };
    assert!(bytes.iter().step_by(4096).all(|&b| b == 0));
    touch(s.as_ptr() as usize, s.len());
    let (fills, faults) = (hooks::zero_fills() - z0, minflt() - f0);
    eprintln!("MEASURED WarmZero Dense take: {fills} fills, {faults} faults");
    assert_eq!(fills, 0);
    assert!(faults <= HOST_SLACK, "{faults} faults on a WarmZero take");
    // SAFETY: ours, not used again.
    unsafe { give_span(s) };
}

fn cold_superpage_faults_once_per_2mib() {
    if hooks::thp_never() {
        eprintln!("SKIP cold superpage faults: THP is never on this host");
        return;
    }
    let s = span(4 * SUPER, Hint::new(Class::Task, Tag::OTHER).zero(Zero::Sparse)).unwrap();
    let fb0 = thp_fallbacks();
    let f0 = minflt();
    touch(s.as_ptr() as usize, s.len());
    let (faults, fallbacks) = (minflt() - f0, thp_fallbacks() - fb0);
    eprintln!("MEASURED cold faults per 2 MiB: {:.2} ({faults} over 4 superpages, host THP fallbacks {fallbacks})", faults as f64 / 4.0);
    if fallbacks > 0 {
        eprintln!("SKIP cold superpage faults: the kernel fell back to small pages (fragmented host memory), not judged");
    } else {
        assert!(faults <= 4, "{faults} faults over 4 cold superpages: THP did not back them");
    }
    // SAFETY: ours, not used again.
    unsafe { give_span(s) };
}

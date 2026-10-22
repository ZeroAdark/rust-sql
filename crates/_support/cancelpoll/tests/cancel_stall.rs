//! C9: the pollgap gate's injected stall (`set_stall_ms`) holds the first kernel call after it is set for that long with no
//! abort poll, once, so the probe logs exactly one gap; off, nothing. One test: the switches are process-wide. Named
//! cancel_* for the crate tests' serial tail (D4079): the gap is wall-clock time.
use core::sync::atomic::AtomicU8;

#[test]
fn the_injected_stall_is_one_gap_of_the_first_kernel_call() {
    let byte = AtomicU8::new(0);
    cancelpoll::set_probe_ms(20);
    cancelpoll::scoped(&byte, || {});
    assert_eq!(cancelpoll::probe_max().1, 0, "no stall set: no gap");
    cancelpoll::set_stall_ms(60);
    cancelpoll::scoped(&byte, || {});
    let (max, n) = cancelpoll::probe_max();
    assert_eq!(n, 1, "one gap: the stall's");
    assert!((60..1000).contains(&max), "{max} ms");
    cancelpoll::scoped(&byte, || {});
    assert_eq!(cancelpoll::probe_max().1, 1, "once per setting");
    cancelpoll::set_probe_ms(0);
}

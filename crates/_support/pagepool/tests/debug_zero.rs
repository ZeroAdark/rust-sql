//! D4868: under the debug arm (`PGRUST_POOL_DEBUG=1`) a freshly carved batch is poisoned, so a zeroed take of its first
//! block must zero it. `refill_block` once kept the carve's zero flag over the poison, and `fill` trusted it: the zeroed
//! take came back as 0xdb bytes (every server's first query crashed on a Gravity object built zeroed). Own binary: the pool
//! reads its switches once.

mod common;
use common::*;
use pagepool::*;

#[test]
fn a_zeroed_take_of_a_fresh_carve_reads_zero_under_debug() {
    // Edition-2021 std::env::set_var (safe fn), the binary's first act, before the pool reads its switches
    std::env::set_var("PGRUST_POOL_DEBUG", "1");
    assert!(switches().debug, "the debug arm is on");
    setup();
    // each size a bucket of its own in two classes: every take is the first block of a fresh carve
    for (i, bytes) in [16usize, 48, 64, 200, 256, 1000, 1024, 4096, 16 << 10, 48 << 10].into_iter().enumerate() {
        let class = if i % 2 == 0 { Class::Task } else { Class::Cache };
        let p = take(bytes, 16, Hint::new(class, Tag::OTHER).zero(Zero::Dense)).expect("a zeroed block");
        // SAFETY: the block just taken, its `len()` bytes ours.
        let b = unsafe { core::slice::from_raw_parts(p.cast::<u8>().as_ptr(), p.len()) };
        if let Some(off) = b.iter().position(|&x| x != 0) {
            panic!("a zeroed take of {bytes} B ({class:?}) reads {:#04x} at +{off}", b[off]);
        }
        // SAFETY: ours, not used again.
        unsafe { give(p.cast(), bytes) };
    }
}

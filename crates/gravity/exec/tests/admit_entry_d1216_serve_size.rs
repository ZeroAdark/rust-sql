//! D1216 (M6-PERF): a served statement's execution state (`run::Serve`, boxed in the QueryDesc at every ExecutorStart) carried
//! its key words inline as `[u64; 512]` + `[u8; 512]`, 6.3 KB that the entry copied twice per execution (Serve::new's return,
//! then the Box): ~0.34 us per nested SPI statement, the F3 cell's 1.13 x rowexec. The words are sized to the admission now,
//! so the state stays a small fraction of the two arrays alone (4.6 KB).
#[test]
fn d1216_serve_is_small() {
    let n = core::mem::size_of::<gravity_exec::admit::entry::run::Serve>();
    assert!(n <= 3072, "gravity_exec::admit::entry::run::Serve is {n} bytes: the key words belong on the heap, sized to the admission (D1216)");
}

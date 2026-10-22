//! D4760 (pppress, `PGRUST_POOL_FINE_CLASSES`): with the switch on, blocks of 1-128 KiB take the least of four buckets per
//! doubling that is a whole number of 512 B grains (1-2 KiB keeps 1024 / 1536 / 2048): what `take` hands out is what
//! `usable` says, each a distinct block aligned to its size's power of two, named by `run_of`, and freed back to a
//! balanced pool.
mod common;
use common::*;
use pagepool::hooks;
use pagepool::*;

#[test]
fn fine_classes() {
    hooks::fine_classes_switch(true);
    setup();
    assert!(fine_classes_on());
    let h = Hint::new(Class::Session, Tag::MCX);
    let cases = [(1025, 1536), (1280, 1536), (1537, 2048), (2049, 2560), (2561, 3072), (3073, 3584), (5000, 5120), (9000, 10240), (13000, 14336), (20000, 20480), (30000, 32768), (33000, 40960), (50000, 57344), (65537, 81920), (100000, 114688), (131072, 131072)];
    let mut all = Vec::new();
    for &(ask, want) in &cases {
        assert_eq!(usable(ask, 16), want, "usable({ask})");
        for _ in 0..4 {
            let p = take(ask, 16, h).expect("a block");
            assert_eq!(p.len(), want, "take({ask})");
            let a = p.cast::<u8>().as_ptr() as usize;
            let info = run_of(a as *const u8).unwrap();
            assert!(matches!(info.kind, Kind::Blocks { .. }) && info.base == a && info.bytes == want, "{info:?}");
            assert_eq!(a % (1 << want.trailing_zeros()).min(64 << 10), 0, "aligned to its size's power of two");
            touch(a, want);
            all.push((a, want));
        }
    }
    // an alignment past a fine bucket's own takes the next bucket that has it
    let p = take(2049, 1024, h).unwrap();
    assert_eq!(p.len(), 3072);
    all.push((p.cast::<u8>().as_ptr() as usize, 3072));
    all.sort();
    for w in all.windows(2) {
        assert!(w[0].0 + w[0].1 <= w[1].0, "blocks overlap: {w:?}");
    }
    for (a, b) in all {
        // SAFETY: taken above with this size, ours, freed once.
        unsafe { give(core::ptr::NonNull::new(a as *mut u8).unwrap(), b) };
    }
    flush_shelf();
    balanced();
    purge_now(usize::MAX, None);
    balanced();
}

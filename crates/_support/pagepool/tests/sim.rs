//! The simulation runs the product allocator and pgsync locks allocate: no pgsync anywhere in the crate's source.

#[test]
fn no_pgsync_in_src() {
    let dir = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR")).join("src");
    let mut n = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "rs") {
            let text = std::fs::read_to_string(&p).unwrap();
            assert!(!text.contains("pgsync::"), "{} names pgsync", p.display());
            assert!(!text.contains("std::sync::Mutex"), "{} uses std::sync::Mutex", p.display());
            n += 1;
        }
    }
    assert!(n >= 10, "read {n} source files");
}

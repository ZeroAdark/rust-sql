#[test]
fn lints() {
    let dir = manifest_dir();
    let sites = gravity_lint::alloc_sites(dir);
    assert!(sites.is_empty(), "untagged allocation sites: {:?}", sites.iter().map(|s| format!("{}:{} {}", s.path.display(), s.line_no, s.line)).collect::<Vec<_>>());
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}

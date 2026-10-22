//! D4507: `compile_with` stays outlined. Fat LTO + PGO inlined the whole TPDE compiler into the promotion pool thread's
//! entry (`__rust_begin_short_backtrace`, 1,703,680 B of .text in the dist + PGO binary) beside the outlined copy the
//! inline-compile path calls; `#[inline(never)]` on `compile_with` keeps one copy (dist + PGO: .text -1,608,192 B). No
//! test observes inlining, and a plain build never inlines it (the plain dist binary is byte-identical either way), so
//! this one reads the attribute off the source: the item's own attribute / doc block must carry it.

#[test]
fn compile_with_is_never_inlined() {
    let src = include_str!("../src/lib.rs");
    let at = src.find("\npub fn compile_with<").expect("gravity_tpde::compile_with not found in src/lib.rs");
    // the contiguous block of attribute and doc lines right above the item
    let block: Vec<&str> = src[..at]
        .lines()
        .rev()
        .map(str::trim_start)
        .take_while(|l| l.starts_with("#[") || l.starts_with("///"))
        .collect();
    assert!(
        block.iter().any(|l| l.starts_with("#[inline(never)]")),
        "compile_with lost #[inline(never)]: fat LTO + PGO inlines the whole compiler into the pool thread's entry again (D4507); its attributes: {block:?}"
    );
}

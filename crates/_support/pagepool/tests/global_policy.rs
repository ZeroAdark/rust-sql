//! `PGRUST_MEMPOLICY`'s pool knobs count though `PoolAlloc` starts the pool at the first block (a child per setting).
#![cfg(all(feature = "global", not(target_family = "wasm")))]

use pagepool::PoolAlloc;

#[global_allocator]
static A: PoolAlloc = PoolAlloc;

fn child_len(policy: Option<&str>) -> usize {
    let mut c = std::process::Command::new(std::env::current_exe().unwrap());
    // one test thread: libtest then writes `test <name> ... ` before the child's own line, on that line (the parse below
    // finds the field anywhere), the same on any box (a child left to pick its threads printed it apart, or not)
    c.args(["child_prints_the_pool_range", "--exact", "--include-ignored", "--nocapture", "--test-threads", "1"])
        .env("PAGEPOOL_POLICY_CHILD", "1");
    match policy {
        Some(p) => c.env("PGRUST_MEMPOLICY", p),
        None => c.env_remove("PGRUST_MEMPOLICY"),
    };
    let out = c.output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|l| {
            let v = l.split_once("POOL_LEN ")?.1;
            v[..v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len())].parse().ok()
        })
        .unwrap_or_else(|| panic!("child: {text}"))
}

#[test]
#[ignore = "the child of the_policys_pool_knobs_count"]
fn child_prints_the_pool_range() {
    if std::env::var_os("PAGEPOOL_POLICY_CHILD").is_some() {
        println!("POOL_LEN {}", pagepool::range().1);
    }
}

#[test]
fn the_policys_pool_knobs_count() {
    let (base, wide) = (child_len(None), child_len(Some("pool_va_mul=3")));
    assert!(base > 0, "the pool started at the first block");
    assert!(wide > base, "pool_va_mul=3 widened the pool zone: {base} -> {wide}");
}

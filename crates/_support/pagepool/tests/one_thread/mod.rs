//! D4855: the main of a test binary whose checks read a process-wide pool count (`harness = false` in Cargo.toml). Its
//! tests run one after another on the main thread, so no harness thread allocates inside a count window. Speaks libtest's
//! `--list` and result lines for the crate-test runners; a name argument filters (`--exact`: whole names), `--skip` drops.

pub fn main(tests: &[(&str, fn())]) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        for (name, _) in tests {
            println!("{name}: test");
        }
        return;
    }
    let exact = args.iter().any(|a| a == "--exact");
    let (mut names, mut skips) = (Vec::new(), Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--skip" => skips.extend(it.next()),
            "--test-threads" | "--format" | "--color" | "--logfile" | "--shuffle-seed" | "-Z" => {
                it.next();
            }
            s if s.starts_with('-') => {}
            _ => names.push(a),
        }
    }
    fn hit(exact: bool, n: &str, w: &str) -> bool {
        if exact { n == w } else { n.contains(w) }
    }
    let picked: Vec<&(&str, fn())> = tests
        .iter()
        .filter(|t| {
            (names.is_empty() || names.iter().any(|w| hit(exact, t.0, w))) && !skips.iter().any(|w| hit(exact, t.0, w))
        })
        .collect();
    println!("\nrunning {} tests", picked.len());
    let mut failed = Vec::new();
    for &&(name, f) in &picked {
        let ok = std::panic::catch_unwind(f).is_ok();
        println!("test {name} ... {}", if ok { "ok" } else { "FAILED" });
        if !ok {
            failed.push(name);
        }
    }
    if !failed.is_empty() {
        println!("\nfailures:");
        for name in &failed {
            println!("    {name}");
        }
    }
    let verdict = if failed.is_empty() { "ok" } else { "FAILED" };
    println!(
        "\ntest result: {verdict}. {} passed; {} failed; 0 ignored; 0 measured; {} filtered out\n",
        picked.len() - failed.len(),
        failed.len(),
        tests.len() - picked.len()
    );
    if !failed.is_empty() {
        std::process::exit(101);
    }
}

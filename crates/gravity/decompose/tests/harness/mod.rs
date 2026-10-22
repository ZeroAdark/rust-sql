// The shared golden harness for the decomposer's corpus tests (tests/decompose.rs, tests/regress_histogram.rs).
//
//   REGEN=1         rewrite every golden the run produces
//   REGEN=changed   rewrite only the goldens whose text differs, list them with a one-line diff summary each
//   FIXTURES=a,b*   run only the fixtures whose path under tests/plans (no extension) matches one of the
//                   comma-separated globs (`*`, `?`), e.g. FIXTURES='regress-full/alter_table-*,fieldobs-001'
//
// The corpus walk runs on every core (std::thread::scope); the result order is the sorted fixture order
// whatever the scheduling, so the goldens, the histogram and the failure lists are deterministic.
// Every test ends with a `--stats` line (`cargo test … -- --nocapture` shows it).
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Regen {
    Off,
    All,
    Changed,
}

pub fn regen() -> Regen {
    match std::env::var("REGEN") {
        Err(_) => Regen::Off,
        Ok(v) if v == "changed" => Regen::Changed,
        Ok(_) => Regen::All,
    }
}

pub fn plans_root() -> PathBuf {
    manifest_dir().join("tests").join("plans")
}

/// The fixture's name as FIXTURES matches it: its path under tests/plans without the extension.
pub fn fixture_name(path: &Path) -> String {
    let rel = path.strip_prefix(plans_root()).unwrap_or(path).with_extension("");
    rel.to_string_lossy().replace('\\', "/")
}

fn glob_match(pat: &[u8], s: &[u8]) -> bool {
    match (pat.first(), s.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(b'*'), _) => glob_match(&pat[1..], s) || (!s.is_empty() && glob_match(pat, &s[1..])),
        (Some(b'?'), Some(_)) => glob_match(&pat[1..], &s[1..]),
        (Some(p), Some(c)) => p == c && glob_match(&pat[1..], &s[1..]),
        (Some(_), None) => false,
    }
}

/// The FIXTURES filter, or None when unset.
pub fn filter() -> Option<Vec<String>> {
    let v = std::env::var("FIXTURES").ok()?;
    let pats: Vec<String> = v
        .split(',')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(|p| {
            let p = p.strip_prefix("tests/plans/").unwrap_or(p);
            p.strip_suffix(".plan").unwrap_or(p).to_string()
        })
        .collect();
    Some(pats)
}

pub fn selected(path: &Path) -> bool {
    match filter() {
        None => true,
        Some(pats) => {
            let name = fixture_name(path);
            pats.iter().any(|p| glob_match(p.as_bytes(), name.as_bytes()))
        }
    }
}

/// The `.plan` files directly under `dir`, sorted, narrowed by FIXTURES.
pub fn fixtures_in(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "plan") && selected(p))
        .collect();
    v.sort();
    v
}

/// `f` over every item on all cores; the output is in input order.
pub fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(items.len().max(1));
    if threads <= 1 {
        return items.iter().map(&f).collect();
    }
    let next = AtomicUsize::new(0);
    let mut parts: Vec<Vec<(usize, R)>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= items.len() {
                            break mine;
                        }
                        mine.push((i, f(&items[i])));
                    }
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("a fixture worker panicked")).collect()
    });
    let mut all: Vec<(usize, R)> = parts.drain(..).flatten().collect();
    all.sort_by_key(|(i, _)| *i);
    all.into_iter().map(|(_, r)| r).collect()
}

/// One line: where the texts first differ and by how many lines.
pub fn diff_summary(want: &str, got: &str) -> String {
    let (w, g): (Vec<&str>, Vec<&str>) = (want.lines().collect(), got.lines().collect());
    let common = w.iter().zip(&g).take_while(|(a, b)| a == b).count();
    let delta = g.len() as isize - w.len() as isize;
    match (w.get(common), g.get(common)) {
        (Some(a), Some(b)) => format!("line {}: -`{a}` +`{b}` ({delta:+} lines)", common + 1),
        (Some(a), None) => format!("line {}: -`{a}` ({delta:+} lines)", common + 1),
        (None, Some(b)) => format!("line {}: +`{b}` ({delta:+} lines)", common + 1),
        (None, None) => "trailing whitespace only".to_string(),
    }
}

/// The per-run tally every test prints as its `--stats` line.
pub struct Stats {
    pub label: &'static str,
    pub loaded: usize,
    pub compared: usize,
    pub rewritten: Vec<String>,
    pub failures: Vec<String>,
    start: Instant,
}

impl Stats {
    pub fn new(label: &'static str) -> Self {
        Stats { label, loaded: 0, compared: 0, rewritten: Vec::new(), failures: Vec::new(), start: Instant::now() }
    }

    /// Compare `got` with the golden at `golden` under the REGEN mode; `name` labels it in the report.
    pub fn check(&mut self, golden: &Path, name: &str, got: &str) {
        self.compared += 1;
        let stored = fs::read_to_string(golden).ok();
        match regen() {
            Regen::All => {
                fs::create_dir_all(golden.parent().unwrap()).unwrap();
                fs::write(golden, got).unwrap();
            }
            Regen::Changed => match stored {
                Some(want) if want == got => {}
                Some(want) => {
                    fs::write(golden, got).unwrap();
                    self.rewritten.push(format!("{name}: {}", diff_summary(&want, got)));
                }
                None => {
                    fs::create_dir_all(golden.parent().unwrap()).unwrap();
                    fs::write(golden, got).unwrap();
                    self.rewritten.push(format!("{name}: new ({} lines)", got.lines().count()));
                }
            },
            Regen::Off => match stored {
                Some(want) if want == got => {}
                Some(want) => self.failures.push(format!("{name}: differs from its golden ({})\n--- want\n{want}\n--- got\n{got}", diff_summary(&want, got))),
                None => self.failures.push(format!("{name}: no golden at {} (REGEN=1 writes it)", golden.display())),
            },
        }
    }

    /// Print the `--stats` line (and the rewritten list under REGEN=changed), then fail on any mismatch.
    pub fn finish(self) {
        for r in &self.rewritten {
            println!("regen: {r}");
        }
        let mode = match regen() {
            Regen::Off => "off",
            Regen::All => "all",
            Regen::Changed => "changed",
        };
        println!(
            "--stats {}: fixtures={} graphs={} regen={} rewritten={} filter={} time={:.2}s",
            self.label,
            self.loaded,
            self.compared,
            mode,
            self.rewritten.len(),
            filter().map(|p| p.join(",")).unwrap_or_else(|| "-".into()),
            self.start.elapsed().as_secs_f64()
        );
        assert!(self.failures.is_empty(), "{}", self.failures.join("\n"));
    }
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}

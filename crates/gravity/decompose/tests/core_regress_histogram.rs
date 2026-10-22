// The real pg_regress core suite (decompose.md §4.5): one fixture per plan shape under
// tests/plans/regress-full, a golden per fixture, and the histogram that is D5's todo list.
use std::collections::BTreeMap;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use gravity_decompose::{corpus, decompose_in, dump, ledger, Kind};
use gravity_program::Arena;
use mcx::MemoryContext;

mod harness;

thread_local!(static CAPTURING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });

const EXCLUDED_TAGS: [&str; 5] = ["GATHER", "GATHERMERGE", "FOREIGNSCAN", "CUSTOMSCAN", "PLANINVALITEM"];

fn dir(name: &str) -> PathBuf {
    manifest_dir().join("tests").join(name).join("regress-full")
}

fn fixtures() -> Vec<PathBuf> {
    harness::fixtures_in(&dir("plans"))
}

struct Outcome {
    text: String,
    test: String,
    served: Vec<Kind>,
    refused: Option<(Kind, &'static str, &'static str)>,
    panic: Option<String>,
    excluded_hits: Vec<&'static str>,
}

fn run(path: &Path) -> Result<Outcome, String> {
    let raw = fs::read_to_string(path).unwrap();
    let ctx = MemoryContext::new("fixture");
    // the plan reader asserts on a text it cannot read (an identity-key capture holds shapes the structure key never
    // kept): a panic there is a load failure named in the report, not the run's end
    CAPTURING.with(|c| c.set(true));
    let loaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| corpus::load(ctx.mcx(), &raw)));
    CAPTURING.with(|c| c.set(false));
    let fx = match loaded {
        Ok(r) => r.map_err(|e| format!("{}: {e}", path.display()))?,
        Err(p) => {
            let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
            return Err(format!("{}: the plan reader panicked: {msg}", path.display()));
        }
    };
    let arena = Arena::new(1 << 20);
    let mut text = format!("-- {}\n-- {}\n", fx.name, fx.sql);
    let test = raw.lines().find_map(|l| l.strip_prefix("-- test: ")).unwrap_or("").to_string();
    let excluded_hits = EXCLUDED_TAGS.iter().copied().filter(|t| raw.contains(&format!("{{{t} "))).collect();
    CAPTURING.with(|c| c.set(true));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut body = String::new();
        let r = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).map(|g| {
            dump::dump(&g, &mut body).unwrap();
            g.mapped_kinds().collect::<Vec<Kind>>()
        });
        (body, r)
    }));
    CAPTURING.with(|c| c.set(false));
    let outcome = match result {
        Err(p) => {
            let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
            writeln!(text, "panic: {msg}").unwrap();
            Outcome { text, test, served: Vec::new(), refused: None, panic: Some(msg), excluded_hits }
        }
        Ok((body, Ok(served))) => {
            text.push_str(&body);
            Outcome { text, test, served, refused: None, panic: None, excluded_hits }
        }
        Ok((_, Err(r))) => {
            dump::dump_refusal(&r, &mut text).unwrap();
            let state = match ledger::state_of(r.kind) {
                ledger::State::Mapped(_) => "shape",
                ledger::State::Refused(m, _) => m.name(),
                ledger::State::Excluded(..) => "excluded",
            };
            Outcome { text, test, served: Vec::new(), refused: Some((r.kind, state, r.reason)), panic: None, excluded_hits }
        }
    };
    Ok(outcome)
}

/// The corpus, decomposed once per process on all cores, in fixture order (the three corpus tests share it).
fn outcomes() -> &'static [(PathBuf, Outcome)] {
    static ALL: std::sync::OnceLock<Vec<(PathBuf, Outcome)>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        std::panic::set_hook(Box::new(|info| {
            if !CAPTURING.with(|c| c.get()) {
                eprintln!("{info}");
            }
        }));
        let fx = fixtures();
        let mut v: Vec<(PathBuf, Outcome)> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
        for r in harness::par_map(&fx, |f| run(f).map(|o| (f.clone(), o))) {
            match r {
                Ok(x) => v.push(x),
                Err(e) => failures.push(e),
            }
        }
        assert!(!v.is_empty() || harness::filter().is_some(), "no fixtures under tests/plans/regress-full (scripts/gravity-capture-plans.sh regress-full)");
        if !failures.is_empty() && !matches!(harness::regen(), harness::Regen::Off) {
            eprintln!("fixtures that do not load ({}), skipped under REGEN:\n{}", failures.len(), failures.join("\n"));
        } else {
            assert!(failures.is_empty(), "fixtures that do not load:\n{}", failures.join("\n"));
        }
        v
    })
}

#[test]
fn every_regress_fixture_reproduces_its_golden() {
    let mut stats = harness::Stats::new("regress-full");
    for (f, o) in outcomes() {
        stats.loaded += 1;
        stats.check(&dir("golden").join(f.with_extension("graph").file_name().unwrap()), &harness::fixture_name(f), &o.text);
    }
    stats.finish();
    if harness::filter().is_some() {
        return;
    }
    let mut orphans = Vec::new();
    for e in fs::read_dir(dir("golden")).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "graph") && !dir("plans").join(p.with_extension("plan").file_name().unwrap()).exists() {
            orphans.push(p);
        }
    }
    assert!(orphans.is_empty(), "goldens without a fixture: {orphans:?}");
}

fn count<K: Ord>(m: &mut BTreeMap<K, usize>, k: K) {
    *m.entry(k).or_default() += 1;
}

fn sorted_desc<K: Clone + Ord>(m: &BTreeMap<K, usize>) -> Vec<(K, usize)> {
    let mut v: Vec<(K, usize)> = m.iter().map(|(k, n)| (k.clone(), *n)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

// The histogram (decompose.md §5.1): the first refused kind per fixture by ledger state, the shape reasons
// inside mapped kinds, the excluded kinds' hits, the mapped kinds served; byte-compared to histogram.txt.
#[test]
fn regress_histogram() {
    let all = outcomes();
    let mut served = 0usize;
    let mut by_state: BTreeMap<&str, BTreeMap<&str, usize>> = BTreeMap::new();
    let mut shape: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    let mut mapped: BTreeMap<&str, usize> = BTreeMap::new();
    let mut hits: BTreeMap<&str, usize> = BTreeMap::new();
    let mut panics: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_test: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for (_, o) in all {
        for h in &o.excluded_hits {
            count(&mut hits, h);
        }
        let t = per_test.entry(o.test.clone()).or_default();
        if let Some(m) = &o.panic {
            t.1 += 1;
            count(&mut panics, m.clone());
            continue;
        }
        match o.refused {
            None => {
                served += 1;
                t.0 += 1;
                for k in &o.served {
                    count(&mut mapped, k.name());
                }
            }
            Some((k, state, reason)) => {
                t.1 += 1;
                if state == "shape" {
                    count(&mut shape, (k.name(), reason));
                } else {
                    count(by_state.entry(state).or_default(), k.name());
                }
            }
        }
    }
    let mut out = String::new();
    writeln!(out, "fixtures {} served {} refused {}", all.len(), served, all.len() - served).unwrap();
    for (state, kinds) in &by_state {
        writeln!(out, "refused at {state} ({}):", kinds.values().sum::<usize>()).unwrap();
        for (k, n) in sorted_desc(kinds) {
            writeln!(out, "  {k} {n}").unwrap();
        }
    }
    writeln!(out, "shape refusals inside mapped kinds ({}):", shape.values().sum::<usize>()).unwrap();
    for ((k, r), n) in sorted_desc(&shape) {
        writeln!(out, "  {k}: {r} {n}").unwrap();
    }
    writeln!(out, "decomposer panics ({}):", panics.values().sum::<usize>()).unwrap();
    for (m, n) in sorted_desc(&panics) {
        writeln!(out, "  {m} {n}").unwrap();
    }
    writeln!(out, "excluded kinds present in fixtures:").unwrap();
    for t in EXCLUDED_TAGS {
        writeln!(out, "  {t} {}", hits.get(t).copied().unwrap_or(0)).unwrap();
    }
    writeln!(out, "mapped kinds in served fixtures:").unwrap();
    for (k, n) in sorted_desc(&mapped) {
        writeln!(out, "  {k} {n}").unwrap();
    }
    writeln!(out, "per test (served/refused):").unwrap();
    for (t, (s, r)) in &per_test {
        writeln!(out, "  {t} {s}/{r}").unwrap();
    }
    println!("{out}");
    if harness::filter().is_some() {
        println!("regress_histogram: histogram.txt not compared (FIXTURES set; a whole-corpus property)");
        return;
    }
    let mut stats = harness::Stats::new("histogram");
    stats.loaded = all.len();
    stats.check(&dir("golden").join("histogram.txt"), "regress-full/histogram.txt", &out);
    stats.finish();
    for (state, _) in &by_state {
        assert!(*state == "excluded" || ledger::CURRENT.name() < *state, "a refusal at {state} is not past the current milestone");
    }
}

/// The D5 closure's order-provenance row (decompose.md §5.2, lane d5-order): every sorted group, merge join, MergeAppend
/// feed and window walk whose input order the graph now proves — a Sort's keys through a merge join's outer, a
/// MergeAppend's merge, a sorted group's emit, an incremental sort, a subquery scan, a filter's equality. (join-059,
/// the 25th of the row, is a `GROUP BY … ORDER BY thousand DESC` over a descending incremental sort: decision 15's
/// "not the ascending key order" refusal now that its order is known.)
#[test]
fn order_provenance_serves_the_closure_shapes() {
    const LIFTED: [&str; 24] = [
        "aggregates-035", "aggregates-036", "aggregates-102", "aggregates-103", "aggregates-104", "aggregates-106", "aggregates-110",
        "incremental_sort-015", "incremental_sort-016", "join-083", "join-084", "join-085",
        "partition_aggregate-009", "partition_join-019", "select_distinct-005", "select_distinct-017", "select_distinct_on-003",
        "select_distinct_on-004", "subselect-050", "union-015", "union-026", "union-029", "union-058", "union-059",
    ];
    let mut refused = Vec::new();
    for name in LIFTED {
        let o = run(&dir("plans").join(format!("{name}.plan"))).unwrap();
        if let Some((k, _, reason)) = o.refused {
            refused.push(format!("{name}: {k:?}: {reason}"));
        }
        assert!(o.panic.is_none(), "{name} panicked");
    }
    assert!(refused.is_empty(), "closure shapes refused: {refused:?}");
}

/// D4's closure: the join / subplan / append family refuses only by a ruling or a written need (decompose.md §5.2).
#[test]
fn d4_family_refuses_only_by_ruling() {
    const FAMILY: [Kind; 17] = [
        Kind::NestLoop, Kind::NestLoopParam, Kind::MergeJoin, Kind::HashJoin, Kind::Hash, Kind::Material, Kind::Memoize, Kind::SubPlan, Kind::AlternativeSubPlan,
        Kind::CteScan, Kind::SubqueryScan, Kind::Append, Kind::MergeAppend, Kind::PartitionPruneInfo, Kind::RecursiveUnion, Kind::WorkTableScan, Kind::WindowAgg,
    ];
    const BY_RULING: [&str; 8] = [
        "a MergeAppend child not in the merge keys' order",
        "a MergeAppend child with no known order",
        "a merge join's outer with no known order",
        "a merge join's inner with no known order",
        "a WindowAgg over an input with no known order",
        "a WindowAgg over an input not in its partition and order keys' order",
        "a hashed SubPlan on a pipeline already consuming two slots",
        // D1047, main 09:55Z (INTERIM until M8-SUB serves the placement gap by the M8 exit): a SubPlan read in a pipeline
        // with no op of it before the read
        "a SubPlan read in a pipeline with no op of it before the read",
    ];
    let mut open = Vec::new();
    for (path, o) in outcomes() {
        if let Some((k, _, reason)) = o.refused {
            if FAMILY.contains(&k) && !BY_RULING.contains(&reason) {
                open.push(format!("{}: {k:?}: {reason}", path.file_stem().unwrap().to_string_lossy()));
            }
        }
    }
    assert!(open.is_empty(), "the D4 family refuses outside its rulings: {open:?}");
}

/// A rerun set holds the pipelines that read the owner's parameters and nothing else: join-065's hash build
/// (uncorrelated) is built once, its probe (`$exec0` in the hash key) reruns; limit-010's top-k reads the parameter
/// through the Limit folded into it.
#[test]
fn rerun_sets_leave_uncorrelated_breakers_built_once() {
    for (name, line) in [("join-065", "rerun node=0 pipelines=p1 slots=0,1"), ("limit-010", "rerun node=0 subplan=1 pipelines=p0 slots=0")] {
        let o = run(&dir("plans").join(format!("{name}.plan"))).unwrap();
        assert!(o.refused.is_none() && o.panic.is_none(), "{name} refused");
        assert_eq!(o.text.lines().filter(|l| l.starts_with("rerun ")).collect::<Vec<_>>(), [line], "{name}");
    }
    let o = run(&dir("plans").join("join-091.plan")).unwrap();
    assert!(o.text.lines().all(|l| !l.starts_with("rerun ")), "join-091's spools are uncorrelated");
}

/// D3: the source family's rows of the regress-full histogram are the by-ruling / named-need set and nothing else.
#[test]
fn source_rows_are_by_ruling() {
    const KINDS: [&str; 17] = ["SeqScan", "IndexScan", "IndexOnlyScan", "BitmapHeapScan", "BitmapIndexScan", "BitmapAnd", "BitmapOr", "TidScan", "TidRangeScan", "SampleScan", "FunctionScan", "ValuesScan", "TableFuncScan", "CteScan", "NamedTuplestoreScan", "WorkTableScan", "ForeignScan"];
    const ALLOWED: [&str; 1] = ["SeqScan: table access method"];
    let text = std::fs::read_to_string(dir("golden").join("histogram.txt")).unwrap();
    let mut extra = Vec::new();
    for l in text.lines().filter(|l| l.starts_with("  ") && !l.starts_with("   ")) {
        let l = l.trim();
        let Some((kind, rest)) = l.split_once(": ") else { continue };
        if !KINDS.contains(&kind) || rest.starts_with("more than MAX_") {
            continue;
        }
        let reason = rest.rsplit_once(' ').map_or(rest, |(r, _)| r);
        if !ALLOWED.contains(&format!("{kind}: {reason}").as_str()) {
            extra.push(l.to_string());
        }
    }
    assert!(extra.is_empty(), "source rows outside the by-ruling set: {extra:?}");
}

/// The residue rows after D1–D5 (decisions 78–80) stay closed: no cap on the record, every key type an image, user-defined
/// aggregates served, SubPlans inside aggregate arguments and VALUES rows declared.
#[test]
fn residue_rows_are_closed() {
    const CLOSED: [&str; 7] = ["more than MAX_RECORD_KEYS", "more than MAX_RECORD_SLOTS", "a group key of a type without a record word", "a user-defined aggregate", "a SubPlan inside an aggregate", "a VALUES row with a SubPlan", "already consuming two slots"];
    let text = std::fs::read_to_string(dir("golden").join("histogram.txt")).unwrap();
    let open: Vec<&str> = text.lines().filter(|l| CLOSED.iter().any(|c| l.contains(c))).collect();
    assert!(open.is_empty(), "residue rows re-opened: {open:?}");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}

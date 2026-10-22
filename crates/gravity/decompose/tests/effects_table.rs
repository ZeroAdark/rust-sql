//! C1 (one effects answer): the census mirror's table. docs/gravity/lowerv4/tools/graphprops_facts.py derives F1 / F5
//! from each plan fixture on its own (facts_m8b `the_census_mirror_agrees_with_every_golden`) and needs the effects home's
//! builtin rules to do so. It reads them from docs/gravity/lowerv4/tools/effects_facts.tsv, which this test writes from the
//! Rust answers under REGEN=changed and otherwise compares: a change to one of the rules below that does not regenerate
//! the file fails here, not later in whichever golden first holds the oid (main 2026-10-11 19:57:20Z; the mirror had
//! drifted by 12 quiet::call oids and one privilege function before it read this table).
//!
//! Rows are `<rule>\t<key>\t<value>`, sorted, one fact each:
//! - by builtin pg_proc oid (1 .. FIRST_NORMAL_OID): `call` (types_core::quiet::call), `pattern` (effects::builtin_pattern:
//!   1 LIKE, 2 ILIKE, 3 `~`, 4 `~*`), `const_arg` (quiet::const_arg; key `<oid> <nargs>`, value the argument's place),
//!   `int_mod` / `int_div`, `privilege` (quiet::PRIVILEGES: value `<nargs> <role 0/1> <NAME,NAME..>`), `start_time`,
//!   `collation_free`, `agg` (effects::agg_oid_quiet: value `plain`, or `typmod` when only a bounded numeric argument
//!   makes it quiet), `text_extremum` (quiet::text_extremum_quiet; key `<aggoid> <collation>` over PROBE_COLLATIONS);
//! - by pg_type oid: `io_text_target`, `out_quiet`, `xml_value` (quiet::xml_value_quiet);
//! - by SQLValueFunctionOp: `svf_raise_free`, `epoch_clock`; the epoch-ns shape's oids and bound: `epoch_ns`;
//! - `probe.*`: the rules that read an argument, answered by Rust over the fixed inputs PROBE_* below (key: the inputs,
//!   value 0 / 1). graphprops_facts.py --check-table replays each row through the mirror's own evaluator, and facts_m8b
//!   fails on any row it answers otherwise. The probe inputs are fixed here; change them only with the mirror's check.
use std::fmt::Write as _;
use std::path::PathBuf;

use gravity_decompose::catalog::typefacts::{KeyColl, FIRST_NORMAL_OID};
use gravity_decompose::effects::{self, Pattern};
use types_core::quiet as q;

mod harness;

/// Divisors probed for every `int_mod` / `int_div` function (`None`: NULL or not a constant).
const PROBE_DIVISORS: [Option<i64>; 8] = [None, Some(i64::MIN), Some(-2), Some(-1), Some(0), Some(1), Some(7), Some(i64::MAX)];

/// Constants probed at every `const_arg` place (a substr length, a left / right n; `None`: NULL or not a constant).
const PROBE_WORDS: [Option<i64>; 9] =
    [None, Some(i32::MIN as i64), Some(i32::MIN as i64 + 1), Some(-1), Some(0), Some(1), Some(5), Some(i32::MAX as i64), Some(1 << 40)];

/// Privilege texts probed for every `PRIVILEGES` function: every privilege name acl.c knows, in case and with spaces, the
/// GRANT / ADMIN OPTION forms, lists, an empty chunk, unknown words.
const PROBE_PRIVILEGES: [&str; 33] = [
    "SELECT", "select", " Select\t", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER", "MAINTAIN", "USAGE",
    "MEMBER", "SET", "EXECUTE", "CREATE", "CONNECT", "TEMPORARY", "TEMP", "ALTER SYSTEM", "SELECT WITH GRANT OPTION",
    "select with grant option", "USAGE WITH ADMIN OPTION", "MEMBER WITH ADMIN OPTION", "SELECT WITH ADMIN OPTION",
    "EXECUTE WITH GRANT OPTION", "SELECT,INSERT", "select, update ", "SELECT,", ",", "", "FOO", "SELECT,FOO", "SELECTX",
];

/// LIKE / ILIKE / regex patterns probed: no trailing backslash, one, two, three.
const PROBE_PATTERNS: [&[u8]; 6] = [b"", b"abc", b"a%c_", b"abc\\", b"abc\\\\", b"\\\\\\"];

/// Collations probed by the pattern and text-extremum rules: none (0, two implicit collations in conflict), every
/// pg_collation.dat row of the reference (default, C, POSIX, ucs_basic, unicode, pg_c_utf8, pg_unicode_fast), a user one.
const PROBE_COLLATIONS: [u32; 9] = [0, 100, 950, 951, 962, 963, 811, 6411, FIRST_NORMAL_OID];

/// Numeric images (numeric.c's NumericShort / NumericLong in little-endian order) probed by `epoch_ns_k_quiet`.
const PROBE_NUMERICS: [(&str, &[u8]); 15] = [
    ("empty", &[]),
    ("0", &[0x00, 0x80]),
    ("1", &[0x00, 0x80, 0x01, 0x00]),
    ("0.5", &[0xFF, 0x80, 0x88, 0x13]),
    ("1e9", &[0x02, 0x80, 0x0A, 0x00]),
    ("-1e9", &[0x02, 0xA0, 0x0A, 0x00]),
    ("1e9+1", &[0x02, 0x80, 0x0A, 0x00, 0x00, 0x00, 0x01, 0x00]),
    ("1e9+0.5", &[0x82, 0x80, 0x0A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x88, 0x13]),
    ("123456789.5", &[0x82, 0x80, 0x01, 0x00, 0x29, 0x09, 0x85, 0x1A, 0x88, 0x13]),
    ("1e12 (weight 3)", &[0x03, 0x80, 0x01, 0x00]),
    ("1 (long form)", &[0x00, 0x00, 0x00, 0x00, 0x01, 0x00]),
    ("NaN", &[0x00, 0xC0]),
    ("+Infinity", &[0x00, 0xD0]),
    ("odd length", &[0x00, 0x80, 0x01]),
    ("digit 10000", &[0x00, 0x80, 0x10, 0x27]),
];

/// The SQLValueFunctionOp values probed (primnodes.h: 0 ..= 14 are PostgreSQL 18.6's; past them none).
const PROBE_SVF_OPS: std::ops::RangeInclusive<u32> = 0..=20;

/// pg_type oids probed by the type rules: every builtin oid.
const BUILTIN: std::ops::Range<u32> = 1..FIRST_NORMAL_OID;

fn hex(b: &[u8]) -> String {
    if b.is_empty() {
        return "-".into();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn word(w: Option<i64>) -> String {
    w.map_or("null".into(), |v| v.to_string())
}

/// The table, from the Rust answers alone.
fn table() -> String {
    let mut rows: Vec<(String, String, String)> = Vec::new();
    let mut row = |rule: &str, key: String, value: String| rows.push((rule.to_string(), key, value));
    let flag = |b: bool| if b { "1".to_string() } else { "0".to_string() };
    for oid in BUILTIN {
        if q::call(oid) {
            row("call", oid.to_string(), "1".into());
        }
        if let Some(p) = effects::builtin_pattern(oid) {
            let form = match p {
                Pattern::Like => 1,
                Pattern::ILike => 2,
                Pattern::Regex(false) => 3,
                Pattern::Regex(true) => 4,
            };
            row("pattern", oid.to_string(), form.to_string());
            for coll in PROBE_COLLATIONS {
                for pat in PROBE_PATTERNS {
                    let good = effects::pattern_good(p, pat, coll, KeyColl::of(coll).bytes_eq, || false);
                    row("probe.pattern", format!("{oid} {coll} {}", hex(pat)), flag(good));
                }
            }
        }
        for nargs in 0..=4 {
            if let Some(place) = q::const_arg(oid, nargs) {
                row("const_arg", format!("{oid} {nargs}"), place.to_string());
            }
        }
        if q::const_arg(oid, 2).is_some() || q::const_arg(oid, 3).is_some() {
            for w in PROBE_WORDS {
                row("probe.text_arg", format!("{oid} {}", word(w)), flag(q::text_arg_quiet(oid, w)));
            }
        }
        if q::int_mod(oid) || q::int_div(oid) {
            row(if q::int_mod(oid) { "int_mod" } else { "int_div" }, oid.to_string(), "1".into());
            for d in PROBE_DIVISORS {
                row("probe.divisor", format!("{oid} {}", word(d)), flag(q::divisor_quiet(oid, d) == Some(true)));
            }
        }
        if let Some(&(_, names, role, nargs)) = q::PRIVILEGES.iter().find(|p| p.0 == oid) {
            row("privilege", oid.to_string(), format!("{nargs} {} {}", u8::from(role), names.join(",")));
            for text in PROBE_PRIVILEGES {
                row("probe.privilege", format!("{oid} {}", hex(text.as_bytes())), flag(q::privilege_quiet(oid, text.as_bytes()) == Some(true)));
            }
        }
        if q::start_time(oid) {
            row("start_time", oid.to_string(), "1".into());
        }
        if q::collation_free(oid) {
            row("collation_free", oid.to_string(), "1".into());
        }
        if effects::agg_oid_quiet(oid, || false) {
            row("agg", oid.to_string(), "plain".into());
        } else if effects::agg_oid_quiet(oid, || true) {
            row("agg", oid.to_string(), "typmod".into());
        }
        for coll in PROBE_COLLATIONS {
            if q::text_extremum_quiet(oid, coll) {
                row("text_extremum", format!("{oid} {coll}"), "1".into());
            }
        }
    }
    for t in BUILTIN {
        if q::io_text_target(t) {
            row("io_text_target", t.to_string(), "1".into());
        }
        if q::out_quiet(t) {
            row("out_quiet", t.to_string(), "1".into());
        }
        if q::xml_value_quiet(t) {
            row("xml_value", t.to_string(), "1".into());
        }
    }
    for op in PROBE_SVF_OPS {
        row("svf_raise_free", op.to_string(), flag(q::svf_raise_free(op)));
        row("epoch_clock", op.to_string(), flag(q::epoch_clock_value(op)));
    }
    row("epoch_ns", "cast".into(), q::EPOCH_NS_CAST.to_string());
    row("epoch_ns", "mul".into(), q::EPOCH_NS_MUL.to_string());
    row("epoch_ns", "extract".into(), q::EPOCH_NS_EXTRACT.to_string());
    row("epoch_ns", "max_k".into(), q::EPOCH_NS_MAX_K.to_string());
    for (what, image) in PROBE_NUMERICS {
        row("probe.epoch_k", hex(image), format!("{} {what}", flag(q::epoch_ns_k_quiet(image))));
    }
    rows.sort();
    rows.dedup();
    let mut out = String::from("# rule\tkey\tvalue: written by crates/gravity/decompose/tests/effects_table.rs (REGEN=changed); do not edit\n");
    for (rule, key, value) in rows {
        writeln!(out, "{rule}\t{key}\t{value}").unwrap();
    }
    out
}

fn tsv() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("run under cargo test")).join("../../../crates/gravity/decompose/testdata/gravity/lowerv4/tools/effects_facts.tsv")
}

/// The committed table is the Rust answers' (REGEN=changed rewrites it).
#[test]
fn the_census_mirror_table_is_the_effects_homes() {
    let mut stats = harness::Stats::new("effects_table");
    stats.check(&tsv(), "effects_facts.tsv", &table());
    stats.finish();
}

/// The table holds what the mirror needs, so an empty or truncated one cannot pass for current: every rule is there and
/// the counts are the ones the mirror's lists had when it began reading the table (a rule that grows raises them).
#[test]
fn the_table_has_every_rule() {
    let text = table();
    let count = |rule: &str| text.lines().filter(|l| l.split('\t').next() == Some(rule)).count();
    for (rule, at_least) in [("call", 50), ("pattern", 28), ("const_arg", 4), ("int_mod", 3), ("int_div", 9), ("privilege", 6),
        ("start_time", 3), ("collation_free", 11), ("agg", 50), ("text_extremum", 6), ("io_text_target", 4), ("out_quiet", 16),
        ("xml_value", 26), ("svf_raise_free", 21), ("epoch_ns", 4), ("probe.pattern", 1000), ("probe.divisor", 96),
        ("probe.privilege", 198), ("probe.text_arg", 36), ("probe.epoch_k", 15)]
    {
        assert!(count(rule) >= at_least, "{rule}: {} rows, at least {at_least}", count(rule));
    }
}

/// The mirror's argument evaluators (divisor_quiet, text_arg_quiet, privilege_quiet, pattern_quiet, epoch_ns_k_quiet) answer
/// every probe row of the table as the effects home does: graphprops_facts.py --check-table.
#[test]
fn the_census_mirror_evaluators_answer_the_probes_as_rust() {
    let script = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("run under cargo test")).join("../../../crates/gravity/decompose/testdata/gravity/lowerv4/tools/graphprops_facts.py");
    let out = std::process::Command::new("python3").arg(script).arg("--check-table").output().expect("python3 runs graphprops_facts.py");
    println!("{}", String::from_utf8_lossy(&out.stdout));
    assert!(out.status.success(), "graphprops_facts.py --check-table:\n{}", String::from_utf8_lossy(&out.stderr).lines().take(40).collect::<Vec<_>>().join("\n"));
}

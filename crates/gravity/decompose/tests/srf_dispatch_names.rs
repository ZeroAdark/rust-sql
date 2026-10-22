// The builtin set-returning function dispatch (catalog/srf.rs `kind_of`) names each function it implements: every entry
// of `NAMES` is fmgroids.h's name of a pg_proc.dat row whose OID it holds, and each one dispatches to its own kind. So
// the literal-OID gate reading does not apply to that table: an OID there is the identity of the function it implements.
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use gravity_decompose::catalog::srf::{kind_of, lists_a_directory, LISTINGS, NAMES};
use gravity_decompose::ops::shared::srf::SrfKind;

fn manifest_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset: run under cargo test"))
}

/// pg_proc.dat's rows as (oid, proname, proargtypes).
fn procs() -> Vec<(u32, String, String)> {
    let path = manifest_dir().join("../../postgres-18.6-reference/src/include/catalog/pg_proc.dat");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let text: String = text.lines().filter(|l| !l.trim_start().starts_with('#')).collect::<Vec<_>>().join("\n");
    let mut out = Vec::new();
    for row in text.split('{').skip(1) {
        let row = row.split('}').next().unwrap_or("");
        let get = |key: &str| {
            let at = row.find(&format!("{key} => '"))? + key.len() + 5;
            row[at..].find('\'').map(|end| row[at..at + end].to_owned())
        };
        if let (Some(oid), Some(name)) = (get("oid"), get("proname")) {
            out.push((oid.parse().unwrap(), name, get("proargtypes").unwrap_or_default()));
        }
    }
    out
}

/// Gen_fmgrtab.pl's name for each row: F_<PRONAME>, and when the name is overloaded `_` plus the argument types joined by
/// `_` (a zero-argument overload ends in `_`: F_RANDOM_), every character outside [A-Za-z0-9_] as `_`. This rule gives all
/// 3,397 names of PostgreSQL 18.6's generated fmgroids.h.
fn fmgroids() -> BTreeMap<String, u32> {
    let rows = procs();
    let mut count = BTreeMap::new();
    for (_, name, _) in &rows {
        *count.entry(name.clone()).or_insert(0) += 1;
    }
    let ident = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c.to_ascii_uppercase() } else { '_' }).collect::<String>();
    rows.iter()
        .map(|(oid, name, args)| {
            let mut n = format!("F_{}", ident(name));
            if count[name] > 1 {
                n.push('_');
                n.push_str(&args.split_whitespace().map(ident).collect::<Vec<_>>().join("_"));
            }
            (n, *oid)
        })
        .collect()
}

#[test]
fn every_dispatched_oid_is_its_functions_own_name() {
    let names = fmgroids();
    let bad: Vec<String> = NAMES
        .iter()
        .filter(|(name, oid)| names.get(*name) != Some(oid))
        .map(|(name, oid)| format!("{name} = {oid}, pg_proc.dat: {:?}", names.get(*name)))
        .collect();
    assert!(bad.is_empty(), "names that are not their pg_proc.dat row:\n{}", bad.join("\n"));
}

#[test]
fn every_named_function_dispatches_to_its_own_kind() {
    let other: Vec<&str> = NAMES.iter().filter(|(_, oid)| matches!(kind_of(*oid, true), SrfKind::Other | SrfKind::Scalar)).map(|(n, _)| *n).collect();
    assert!(other.is_empty(), "named but not dispatched: {other:?}");
    let mut seen = std::collections::BTreeSet::new();
    assert!(NAMES.iter().all(|(_, oid)| seen.insert(*oid)), "an OID named twice");
}

#[test]
fn every_directory_listing_is_its_functions_own_name() {
    let names = fmgroids();
    let bad: Vec<String> = LISTINGS
        .iter()
        .filter(|(name, oid)| names.get(*name) != Some(oid))
        .map(|(name, oid)| format!("{name} = {oid}, pg_proc.dat: {:?}", names.get(*name)))
        .collect();
    assert!(bad.is_empty(), "names that are not their pg_proc.dat row:\n{}", bad.join("\n"));
    assert!(LISTINGS.iter().all(|(_, oid)| lists_a_directory(*oid)), "a listing lists_a_directory does not name");
    assert!(LISTINGS.iter().all(|(_, oid)| !NAMES.iter().any(|(_, n)| n == oid)), "a listing also dispatched by kind_of");
}

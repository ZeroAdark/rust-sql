// Every built-in aggregate is classified (decompose.md §2.6): the catalog's pg_aggregate.dat / pg_proc.dat re-read here,
// independently of scripts/gravity-agg-table.py, and checked row by row against `aggtab::AGG_TABLE`.
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use gravity_decompose::ops::shared::agg::fold_of;
use gravity_decompose::ops::shared::agg::{FinalForm, Register};
use gravity_decompose::ops::shared::agg_class::{Class, Split, Trans};
use gravity_decompose::ops::agg::trans::table as aggtab;
use gravity_decompose::ops::agg::trans::table::AGG_TABLE;

type Entry = BTreeMap<String, String>;

/// A `.dat` file's `{ key => 'value', … }` entries (a quoted value may hold braces and escaped quotes).
fn parse(file: &str) -> Vec<Entry> {
    let path = manifest_dir().join("../../postgres-18.6-reference/src/include/catalog").join(file);
    let text: String = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())).lines().filter(|l| !l.starts_with('#')).collect::<Vec<_>>().join("\n");
    let c: Vec<char> = text.chars().collect();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < c.len() {
        if c[i] != '{' {
            i += 1;
            continue;
        }
        let mut e = Entry::new();
        i += 1;
        while c[i] != '}' {
            if !(c[i].is_alphanumeric() || c[i] == '_') {
                i += 1;
                continue;
            }
            let k0 = i;
            while c[i].is_alphanumeric() || c[i] == '_' {
                i += 1;
            }
            let key: String = c[k0..i].iter().collect();
            while c[i] != '\'' {
                i += 1;
            }
            i += 1;
            let mut v = String::new();
            while c[i] != '\'' {
                if c[i] == '\\' {
                    i += 1;
                }
                v.push(c[i]);
                i += 1;
            }
            i += 1;
            e.insert(key, v);
        }
        out.push(e);
    }
    out
}

/// `name(type,type)` → (name, the pg_proc `proargtypes` spelling); a bare name has one aggregate of that name.
fn signature(s: &str) -> (&str, Option<String>) {
    match s.split_once('(') {
        Some((n, rest)) => (n, Some(rest.trim_end_matches(')').split(',').map(str::trim).filter(|a| !a.is_empty()).collect::<Vec<_>>().join(" "))),
        None => (s, None),
    }
}

fn catalog() -> Vec<(u32, Entry, Entry)> {
    let procs: Vec<Entry> = parse("pg_proc.dat").into_iter().filter(|p| p.get("prokind").is_some_and(|k| k == "a")).collect();
    let aggs = parse("pg_aggregate.dat");
    assert_eq!(procs.len(), aggs.len(), "every prokind 'a' pg_proc row has its pg_aggregate row");
    aggs.into_iter()
        .map(|a| {
            let (name, args) = signature(&a["aggfnoid"]);
            let hits: Vec<&Entry> = procs.iter().filter(|p| p["proname"] == name && args.as_ref().is_none_or(|x| p.get("proargtypes").map_or("", |s| s.as_str()) == x)).collect();
            assert_eq!(hits.len(), 1, "{} resolves to one pg_proc row", a["aggfnoid"]);
            (hits[0]["oid"].parse().unwrap(), a.clone(), hits[0].clone())
        })
        .collect()
}

#[test]
fn every_pg_aggregate_row_is_classified() {
    let cat = catalog();
    assert_eq!(cat.len(), AGG_TABLE.len(), "the table has exactly the catalog's rows");
    assert!(AGG_TABLE.windows(2).all(|w| w[0].fnoid < w[1].fnoid), "sorted by oid (row() is a binary search)");
    let field = |e: &Entry, k: &str| e.get(k).cloned().unwrap_or_default();
    for (oid, a, p) in &cat {
        let sig = &a["aggfnoid"];
        let row = aggtab::row(*oid).unwrap_or_else(|| panic!("UNCLASSIFIED built-in aggregate {sig} (oid {oid}): add its class to scripts/gravity-agg-table.py and regenerate"));
        assert_eq!(row.name, p["proname"], "{sig}");
        assert_eq!(row.kind as char, field(a, "aggkind").chars().next().unwrap_or('n'), "{sig} aggkind");
        assert_eq!(row.ndirect.to_string(), a.get("aggnumdirectargs").cloned().unwrap_or("0".into()), "{sig} aggnumdirectargs");
        assert_eq!(row.transfn, a["aggtransfn"], "{sig} aggtransfn");
        assert_eq!(row.finalfn, field(a, "aggfinalfn"), "{sig} aggfinalfn");
        assert_eq!(row.combinefn, field(a, "aggcombinefn"), "{sig} aggcombinefn");
        assert_eq!(row.initval.map(str::to_string), a.get("agginitval").cloned(), "{sig} agginitval");
        assert_eq!(row.args.len(), p.get("proargtypes").map_or(0, |s| s.split_whitespace().count()), "{sig} argument count");
        assert_eq!(row.sortop != 0, a.contains_key("aggsortop"), "{sig} aggsortop");
    }
}

#[test]
fn the_class_follows_the_catalog_columns() {
    let mut by_fns: BTreeMap<(&str, &str), Class> = BTreeMap::new();
    for r in AGG_TABLE.iter() {
        let seen = *by_fns.entry((r.transfn, r.finalfn)).or_insert(r.class);
        assert_eq!(seen, r.class, "{}: one (transfn, finalfn) pair, one class", r.name);
        assert_eq!(r.kind == b'o', matches!(r.class, Class::OrderedSet(_)), "{} aggkind 'o'", r.name);
        assert_eq!(r.kind == b'h', matches!(r.class, Class::Hypothetical(_)), "{} aggkind 'h'", r.name);
        assert_eq!(r.sortop != 0, matches!(r.class, Class::Min(_) | Class::Max(_) | Class::BoolAnd | Class::BoolOr), "{}: the extrema (bool_and / bool_or among them) are the rows with a sort operator", r.name);
        assert_eq!(r.ndirect > 0 || r.kind != b'n', matches!(r.class, Class::OrderedSet(_) | Class::Hypothetical(_)), "{} direct arguments", r.name);
        for (code, scaled) in [(false, false), (false, true), (true, false), (true, true)] {
            let (reg, trans, final_) = fold_of(r.class, r.fnoid, code, scaled);
            assert!(trans.bytes() >= 8 && trans.bytes() % 8 == 0, "{}: the slot is whole words", r.name);
            if trans == Trans::Var {
                assert!(!matches!(final_, FinalForm::Identity | FinalForm::Bool), "{}: a variable-length slot has a rendering final", r.name);
            }
            assert!(!matches!(reg, Register::DistinctCount | Register::Combine), "{}: the modifier registers come from Mods, never from the class", r.name);
        }
    }
}

/// D2348: max(oid) / min(oid) order unsigned (an oid past 2^31 is the larger), as pg_lsn's and xid8's do: the image
/// registers, not the signed word ones.
#[test]
fn the_oid_extrema_are_unsigned() {
    use gravity_decompose::ops::shared::agg_class::Ext;
    for (fnoid, class, reg) in [(2118, Class::Max(Ext::U64), Register::MaxImage), (2134, Class::Min(Ext::U64), Register::MinImage)] {
        let row = aggtab::row(fnoid).expect("a built-in row");
        assert_eq!(row.class, class, "{fnoid}");
        assert_eq!(fold_of(row.class, fnoid, false, true).0, reg, "{fnoid}");
    }
}

/// Every row's transfn_oid is its aggtransfn's pg_proc row, and the effects home's strictness of it is that row's
/// proisstrict (lowerv4 table/rec.rs pair_filter asks there).
#[test]
fn transfn_oid_is_the_catalog_transition_function() {
    let procs = parse("pg_proc.dat");
    for row in AGG_TABLE.iter() {
        let p = procs.iter().find(|p| p.get("oid").and_then(|o| o.parse::<u32>().ok()) == Some(row.transfn_oid));
        let p = p.unwrap_or_else(|| panic!("{}: transfn_oid {} is no pg_proc row", row.name, row.transfn_oid));
        assert_eq!(p["proname"], row.transfn, "{} ({}): transfn_oid names aggtransfn", row.name, row.fnoid);
        let strict = p.get("proisstrict").is_none_or(|s| s == "t");
        let props = gravity_decompose::effects::builtin_func(row.transfn_oid).map(|f| f.strict);
        assert_eq!(props, Some(strict), "{} ({}): {}'s strictness", row.name, row.fnoid, row.transfn);
    }
}

#[test]
fn a_user_defined_aggregate_has_no_row() {
    for oid in [0, 1, 16384, 99_999, u32::MAX] {
        assert!(aggtab::row(oid).is_none());
    }
}

/// `nodes.h`'s AggSplit values: SIMPLE 0, INITIAL_SERIAL = SKIPFINAL | SERIALIZE, FINAL_DESERIAL = COMBINE | DESERIALIZE.
#[test]
fn aggsplit_reads_the_operation_bits() {
    const COMBINE: u32 = 0x01;
    const SKIPFINAL: u32 = 0x02;
    const SERIALIZE: u32 = 0x04;
    const DESERIALIZE: u32 = 0x08;
    assert_eq!(Split::of(0), Split::Simple);
    assert_eq!(Split::of(SKIPFINAL | SERIALIZE), Split::Initial);
    assert_eq!(Split::of(SKIPFINAL), Split::Initial);
    assert_eq!(Split::of(COMBINE | DESERIALIZE), Split::Final);
    assert_eq!(Split::of(COMBINE | SKIPFINAL | SERIALIZE | DESERIALIZE), Split::Intermediate);
    assert!(Split::Final.combines() && !Split::Final.skips_final());
    assert!(Split::Initial.skips_final() && !Split::Initial.combines());
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}

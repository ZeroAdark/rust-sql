//! The v4 rig (plan §5.6, P9): lower a corpus and check v4's result against the PG oracle straight, under stop / resume,
//! under cancel and with SCRIBBLE, every result through the one comparator (`multiset`, ruling 1). The corpus is the
//! decompose plan fixtures plus each milestone's `tests/fixtures/m<k>/`; the oracle is PostgreSQL 18.6's result recorded
//! beside a plan fixture as `<name>.pg` (the `.case` format below). kernels/tests/common is not used by v4.
#![allow(dead_code)]

pub mod multiset;
mod drive;
pub use drive::{ABORT, BETWEEN, LEASE};
mod lowered;
pub mod on_demand;
mod rig_fmgr;
pub use lowered::{lower_fixture, lower_fixture_baked, lower_fixture_opts, pin_ir, wide_named, LoweredIn};

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use gravity_decompose::corpus;
use gravity_expr::catalog::{DomainFacts, FuncFacts, IoFacts, TextFacts, TypeFacts};
use gravity_expr::Catalog;
use gravity_fn::{Shim, Tier};
use gravity_lower::Lowered;
use gravity_program::Arena;
use mcx::MemoryContext;
use multiset::{Class, Outcome, Row, Verdict};
use types_core::Oid;
use types_error::{PgError, PgResult};

// ------------------------------------------------------------------------------------------------ the case / golden format

/// A `.case` file (the shared comparator fixtures, scripts/gravity-gate/fixtures/multiset/) or a `.pg` golden. Header lines
/// `sql:` (repeatable, joined by newlines), `cols:` (TAB-separated), `class:` (`exact | unordered | ordered 1,2 | limited
/// [1,2]`, 1-based keys), `verdict:` (`match | mismatch | mismatch-order`); then sections `[pg]`, `[pg_full]` (PG without
/// the top-level LIMIT / OFFSET / FETCH), `[got]` of COPY-text rows (TAB-separated, `\N` = NULL, `\\` `\t` `\n` `\r`
/// escapes), or `[pg_error]` / `[got_error]` holding one `SQLSTATE<TAB>message` line. `#` lines before the first section
/// and blank lines anywhere are skipped (so no case holds a one-column row whose value is ''). V4R-2's float cases add
/// `tol: 1:f8,2:f4` (the plan's float-tolerant columns, 1-based) or `types:` (TAB-separated declared result types).
/// V4R-270's key classes: `eqk: <col 1-based><TAB><text><TAB><its class's text>` lines (multiset::Eqk). `schema:` (D432,
/// multiset.py's CREATE TABLEs that type bare columns) is kept but not read: multiset.rs reads the SQL alone.
#[derive(Clone, Debug)]
pub struct Case {
    pub name: String,
    pub sql: String,
    pub cols: Option<Vec<String>>,
    pub class: Option<Class>,
    pub verdict: Option<Verdict>,
    pub tol: Option<multiset::Tol>,
    pub eqk: multiset::Eqk,
    pub types: Option<Vec<String>>,
    pub schema: Option<String>,
    pub pg: Outcome,
    pub pg_full: Option<Vec<Row>>,
    pub got: Option<Outcome>,
}

impl Case {
    /// The statement's class now; without `cols:`, the names come from the SELECT list and the width from the first row.
    pub fn class_now(&self) -> Class {
        let first = |o: Option<&Outcome>| match o {
            Some(Outcome::Rows(r)) => r.first().map(Vec::len),
            _ => None,
        };
        match &self.cols {
            Some(c) => multiset::classify(&self.sql, c),
            None => multiset::classify_with(&self.sql, None, first(Some(&self.pg)).or(first(self.got.as_ref()))),
        }
    }

    /// The float-tolerant columns: the case's `tol:` (the plan's), else multiset.rs's reading of the SQL.
    pub fn tol_now(&self) -> multiset::Tol {
        let n = self.cols.as_ref().map(Vec::len).or(match &self.pg { Outcome::Rows(r) => r.first().map(Vec::len), _ => None });
        self.tol.clone().unwrap_or_else(|| multiset::float_cols(&self.sql, n, self.types.as_deref()))
    }
}

/// Up to five rows only in `pg` and five only in `got` (multiset difference), for a failure message.
pub fn explain(pg: &[Row], got: &[Row]) -> String {
    let (p, g) = (multiset::bag(pg), multiset::bag(got));
    let five = |v: Vec<&Row>| v.into_iter().take(5).map(|r| format!("{r:?}")).collect::<Vec<_>>();
    format!("{} pg rows, {} got rows; only in pg: {:?}; only in got: {:?}", pg.len(), got.len(), five(multiset::beyond(&p, &g)), five(multiset::beyond(&g, &p)))
}

fn unescape(f: &str) -> Option<String> {
    if f == "\\N" {
        return None;
    }
    let (mut out, mut it) = (String::with_capacity(f.len()), f.chars());
    while let Some(c) = it.next() {
        out.push(match (c, c == '\\') {
            (_, true) => match it.next() {
                Some('t') => '\t',
                Some('n') => '\n',
                Some('r') => '\r',
                Some(x) => x,
                None => '\\',
            },
            (c, false) => c,
        });
    }
    Some(out)
}

pub fn parse_class(s: &str) -> Result<Class, String> {
    let (word, keys) = s.trim().split_once(' ').unwrap_or((s.trim(), ""));
    let keys: Vec<usize> = keys.split(',').filter(|k| !k.trim().is_empty()).map(|k| k.trim().parse::<usize>().map(|k| k - 1).map_err(|e| format!("class key {k:?}: {e}"))).collect::<Result<_, _>>()?;
    match word {
        "exact" => Ok(Class::Exact),
        "unordered" => Ok(Class::Unordered),
        "ordered" if !keys.is_empty() => Ok(Class::Ordered(keys)),
        "limited" => Ok(Class::Limited(keys)),
        _ => Err(format!("class {s:?}")),
    }
}

const SECTIONS: [&str; 5] = ["pg", "pg_full", "got", "pg_error", "got_error"];

pub fn parse_case(name: &str, text: &str) -> Result<Case, String> {
    let (mut sql, mut cols, mut class, mut verdict, mut tol, mut types, mut eqk) = (Vec::new(), None, None, None, None, None, multiset::Eqk::new());
    let mut schema = None;
    let mut sections: Vec<(String, Vec<&str>)> = Vec::new();
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim().is_empty() {
            continue;
        }
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')).filter(|s| SECTIONS.contains(s)) {
            sections.push((s.to_owned(), Vec::new()));
        } else if let Some((_, rows)) = sections.last_mut() {
            rows.push(line);
        } else if line.starts_with('#') {
        } else if let Some((k, v)) = line.split_once(':') {
            let v = v.strip_prefix(' ').unwrap_or(v);
            match k {
                "sql" => sql.push(v.to_owned()),
                "cols" => cols = Some(v.split('\t').map(str::to_owned).collect()),
                "types" => types = Some(v.split('\t').map(str::to_owned).collect()),
                "schema" => schema = Some(v.to_owned()),
                "tol" => tol = Some(v.split(',').map(|x| match x.trim().split_once(':') {
                    Some((c, f)) if c.parse::<usize>().is_ok_and(|c| c >= 1) && (f == "f4" || f == "f8") => Ok((c.parse::<usize>().unwrap() - 1, if f == "f4" { multiset::F::F4 } else { multiset::F::F8 })),
                    _ => Err(format!("{name}: tol {x:?}")),
                }).collect::<Result<_, _>>()?),
                "eqk" => match v.split('\t').collect::<Vec<_>>()[..] {
                    [c, t, k] if c.parse::<usize>().is_ok_and(|c| c >= 1) => { eqk.entry(c.parse::<usize>().unwrap() - 1).or_default().insert(unescape(t).unwrap_or_default(), unescape(k).unwrap_or_default()); }
                    _ => return Err(format!("{name}: eqk {v:?}")),
                },
                "class" => class = Some(parse_class(v)?),
                "verdict" => verdict = Some(match v.trim() {
                    "match" => Verdict::Match,
                    "mismatch" => Verdict::Mismatch,
                    "mismatch-order" => Verdict::MismatchOrder,
                    x => return Err(format!("{name}: verdict {x:?}")),
                }),
                _ => return Err(format!("{name}: header {k:?}")),
            }
        } else {
            return Err(format!("{name}: line {line:?}"));
        }
    }
    let rows = |s: &str| sections.iter().find(|x| x.0 == s).map(|x| x.1.iter().map(|l| l.split('\t').map(unescape).collect()).collect::<Vec<Row>>());
    let outcome = |s: &str| match (rows(s), sections.iter().find(|x| x.0 == format!("{s}_error"))) {
        (Some(r), None) => Ok(Some(Outcome::Rows(r))),
        (None, Some((_, l))) => {
            let (st, msg) = l.first().and_then(|l| l.split_once('\t')).ok_or(format!("{name}: [{s}_error] wants SQLSTATE<TAB>message"))?;
            Ok(Some(Outcome::Error { sqlstate: st.to_owned(), message: msg.to_owned() }))
        }
        (None, None) => Ok(None),
        _ => Err(format!("{name}: both [{s}] and [{s}_error]")),
    };
    let pg = outcome("pg")?.ok_or(format!("{name}: no [pg]"))?;
    Ok(Case { name: name.to_owned(), sql: sql.join("\n"), cols, class, verdict, tol, eqk, types, schema, pg, pg_full: rows("pg_full"), got: outcome("got")? })
}

/// `(stem, text)` of every `*.<ext>` file in `dir`, sorted by name; a missing directory is an empty list.
pub fn files(dir: &Path, ext: &str) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == ext))
        .map(|p| (p.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_owned(), std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))))
        .collect();
    v.sort();
    v
}

fn repo(rel: &str) -> PathBuf {
    manifest_dir().join("../../..").join(rel)
}

/// The comparator fixtures M0-GATE keeps beside scripts/gravity-gate/multiset.py.
pub fn comparator_fixtures() -> PathBuf {
    repo("scripts/gravity-gate/fixtures/multiset")
}

/// The comparator's selftest: every case's computed class and verdict are the ones its file states (the same check
/// multiset.py's selftest makes over the same files, V4R-2's `float/` cases included). Returns the number of cases.
pub fn selftest(dir: &Path) -> usize {
    selftest_cases(&[files(dir, "case"), files(&dir.join("float"), "case")].concat())
}

/// selftest over the given `(name, text)` cases: every computed class and verdict is the one the case states.
pub fn selftest_cases(cases: &[(String, String)]) -> usize {
    let mut bad = Vec::new();
    for (name, text) in cases {
        let c = parse_case(name, text).unwrap_or_else(|e| panic!("{e}"));
        let class = c.class_now();
        if c.class.as_ref() != Some(&class) {
            bad.push(format!("{name}: classified {class:?}, the case says {:?}", c.class));
        }
        let v = multiset::compare_typed(&class, &c.pg, c.pg_full.as_deref(), c.got.as_ref().unwrap_or_else(|| panic!("{name}: no [got]")), &c.tol_now(), &c.eqk, c.types.as_deref());
        if c.verdict != Some(v) {
            bad.push(format!("{name}: {v:?}, the case says {:?}", c.verdict));
        }
    }
    assert!(bad.is_empty(), "{} of {} comparator cases disagree:\n{}", bad.len(), cases.len(), bad.join("\n"));
    cases.len()
}

// ------------------------------------------------------------------------------------------------ the corpus and the oracle

/// The decompose plan fixtures (census shapes; no goldens beside them).
pub fn plans_dir() -> PathBuf {
    repo("crates/gravity/decompose/tests/plans")
}

/// Milestone `k`'s fixtures: `<name>.plan` + its PG golden `<name>.pg` (plan §4.1: one directory per milestone).
pub fn milestone_dir(k: u32) -> PathBuf {
    manifest_dir().join(format!("tests/fixtures/m{k}"))
}

/// One corpus entry: a plan fixture (decompose's corpus format) and, when one is recorded, its PG golden.
pub struct Entry {
    pub name: String,
    pub plan: String,
    pub golden: Option<Case>,
}

pub fn corpus(dir: &Path) -> Vec<Entry> {
    let goldens = files(dir, "pg");
    files(dir, "plan")
        .into_iter()
        .map(|(name, plan)| {
            let golden = goldens.iter().find(|g| g.0 == name).map(|g| parse_case(&name, &g.1).unwrap_or_else(|e| panic!("{e}")));
            Entry { name, plan, golden }
        })
        .collect()
}

/// How the rig runs a lowered statement (P9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Straight,
    /// Stop at every `n`-th window boundary and resume until done: the result is the straight result.
    Stop(u32),
    /// Raise the cancel byte after `n` windows: the statement ends with PG's cancel error, or, when it finished first, the
    /// straight result.
    Cancel(u32),
    /// PGRUST_GRAVITY_SCRIBBLE=1: every fmgr result is scribbled once its life ends; the result is unchanged.
    Scribble,
}

pub const MODES: [Mode; 4] = [Mode::Straight, Mode::Stop(1), Mode::Cancel(1), Mode::Scribble];

pub fn cancel_error() -> Outcome {
    Outcome::Error { sqlstate: "57014".into(), message: "canceling statement due to user request".into() }
}

/// v4's outcome for `e` under `mode` against PG's, by the statement's order class; `Err` is the report.
pub fn check(e: &Entry, mode: Mode, got: &Outcome) -> Result<(), String> {
    let g = e.golden.as_ref().ok_or_else(|| format!("{}: no PG golden ({}.pg)", e.name, e.name))?;
    let class = g.class_now();
    let want = if matches!(mode, Mode::Cancel(_)) && *got == cancel_error() { cancel_error() } else { g.pg.clone() };
    match multiset::compare_typed(&class, &want, g.pg_full.as_deref(), got, &g.tol_now(), &g.eqk, g.types.as_deref()) {
        Verdict::Match => Ok(()),
        v => Err(format!("{} {mode:?}: {v:?} under {class:?}: {}", e.name, match (&want, got) {
            (Outcome::Rows(p), Outcome::Rows(r)) => explain(p, r),
            _ => format!("pg {want:?}, got {got:?}"),
        })),
    }
}

// ------------------------------------------------------------------------------------------------ lowering

/// The rig's catalog: pg_type / pg_proc / pg_operator from the reference tree's `.dat` files (no server; gravity_lower's
/// shapes rig reads the same), tiers and shims from `gravity_fn`, text by the C collation (bytes decide). Opfamily, opclass,
/// cmp-proc, domain and io lookups are errors naming what was asked. `props.rs` reads it before admission: a lookup that
/// fails sets the conservative tokens, so an error-only catalog would skew every census comparison.
pub struct RigCatalog {
    types: HashMap<Oid, TypeFacts>,
    procs: HashMap<Oid, FuncFacts>,
    ops: HashMap<Oid, (Oid, Oid, Oid)>,
    subscript: HashMap<Oid, Oid>,                         // pg_type.typsubscript (genbki's arrays: array_subscript_handler)
    aggs: HashMap<Oid, syscache_seams::PgAggregateShape>, // pg_aggregate.dat by aggfnoid
    io: HashMap<Oid, IoFacts>,                            // pg_type.dat typinput / typoutput (getTypeInputInfo / getTypeOutputInfo)
    cmp: HashMap<(Oid, Oid, Oid), Oid>,                   // pg_amproc.dat btree support 1 by (opfamily, left, right): a RowCompareExpr's
    typrow: HashMap<Oid, (String, i8, Oid)>,              // typname, typstorage, typarray: the type-shape and typcache seams (D1191)
    amop: HashMap<Oid, Vec<syscache_seams::PgAmopMemberShape>>, // pg_amop.dat by operator, and by (family, left, right, strategy),
    amop_at: HashMap<(Oid, Oid, Oid, i16), Oid>,                // and pg_amproc.dat by (family, left, right, procnum): the
    amproc: HashMap<(Oid, Oid, Oid, i16), Oid>,                 // operator-family seams typesem's key reader asks (hash_key)
    opfamily: HashMap<Oid, (Oid, String)>,                      // pg_opfamily.dat: method and name, and pg_proc.dat's
    signature: HashMap<Oid, (Oid, Vec<Oid>)>,                   // result and argument types (KeySem's sort key reader)
}

/// One `.dat` file's entries as key -> value (a brace or `\'` inside a quoted value is no boundary; `#` lines skipped).
pub fn dat(file: &str) -> Vec<HashMap<String, String>> {
    let p = repo("crates/postgres-18.6-reference/src/include/catalog").join(file);
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let (mut out, mut cur, mut key, mut val) = (Vec::new(), None::<HashMap<String, String>>, String::new(), None::<String>);
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if let Some(v) = val.as_mut() {
            match c {
                '\\' => v.extend(chars.next()),
                '\'' => drop(cur.as_mut().map(|m| m.insert(std::mem::take(&mut key), val.take().unwrap_or_default()))),
                _ => v.push(c),
            }
        } else if cur.is_some() {
            match c {
                '\'' => val = Some(String::new()),
                '}' => out.extend(cur.take()),
                c if c.is_ascii_alphanumeric() || c == '_' => key.push(c),
                _ => {}
            }
        } else if c == '#' {
            chars.by_ref().find(|&d| d == '\n');
        } else if c == '{' {
            cur = Some(HashMap::new());
        }
    }
    out
}

impl RigCatalog {
    pub fn get() -> &'static RigCatalog {
        static CAT: std::sync::OnceLock<RigCatalog> = std::sync::OnceLock::new();
        CAT.get_or_init(RigCatalog::load)
    }

    fn load() -> RigCatalog {
        type E = HashMap<String, String>;
        let oid = |e: &E| e.get("oid").and_then(|o| o.parse::<Oid>().ok());
        let byte = |e: &E, k: &str, d: u8| e.get(k).map_or(d, |v| v.as_bytes()[0]);
        let pg_type = dat("pg_type.dat");
        let by_name: HashMap<&str, Oid> = pg_type.iter().filter_map(|e| Some((e.get("typname")?.as_str(), oid(e)?))).collect();
        let (mut types, mut procs, mut names, mut typrow) = (HashMap::new(), HashMap::new(), HashMap::new(), HashMap::new());
        let mut subscript_names: Vec<(Oid, String)> = Vec::new();
        let mut io_names: Vec<(Oid, String, String, Oid, bool)> = Vec::new();   // typid, typinput, typoutput, ioparam, varlena
        for e in &pg_type {
            let Some(o) = oid(e) else { continue };
            subscript_names.extend(e.get("typsubscript").map(|h| (o, h.clone())));
            let len = e.get("typlen").map_or(-1, |v| match v.as_str() { "NAMEDATALEN" => 64, "SIZEOF_POINTER" => 8, v => v.parse().expect("typlen") });
            let collation = match e.get("typcollation").map(String::as_str) { Some("default") => 100, Some("C") => 950, _ => 0 };
            let elem = e.get("typelem").and_then(|n| by_name.get(n.as_str())).copied().unwrap_or(0);
            let (align, typtype) = (byte(e, "typalign", b'i') as i8, byte(e, "typtype", b'b') as i8);
            types.insert(o, TypeFacts { len, byval: e.get("typbyval").is_some_and(|v| v != "f"), align, typtype, collation, elem });
            let array = e.get("array_type_oid").and_then(|a| a.parse::<Oid>().ok());
            typrow.insert(o, (e["typname"].clone(), byte(e, "typstorage", b'p') as i8, array.unwrap_or(0)));
            io_names.extend(e.get("typinput").zip(e.get("typoutput")).map(|(i, out)| (o, i.clone(), out.clone(), if elem != 0 { elem } else { o }, len == -1)));   // getTypeIOParam
            if let Some(a) = array {
                // genbki's array type: varlena, the element's alignment when double, else int
                let align = if align == b'd' as i8 { align } else { b'i' as i8 };
                types.insert(a, TypeFacts { len: -1, byval: false, align, typtype: b'b' as i8, collation, elem: o });
                subscript_names.push((a, "array_subscript_handler".to_owned()));
                io_names.push((a, "array_in".to_owned(), "array_out".to_owned(), o, true));
                typrow.insert(a, (format!("_{}", e["typname"]), b'x' as i8, 0));   // genbki's array: typstorage x
            }
        }
        const POLY: &str = " any anyarray anyelement anynonarray anyenum anyrange anymultirange anycompatible anycompatiblearray \
            anycompatiblenonarray anycompatiblerange anycompatiblemultirange internal record ";
        let mut sigs = HashMap::new();   // regprocedure text `name(type,type)` -> oid (pg_aggregate.dat's aggfnoid)
        let mut signature = HashMap::new();
        for e in dat("pg_proc.dat") {
            let (Some(o), Some(n)) = (oid(&e), e.get("proname")) else { continue };
            names.entry(n.clone()).or_insert(o);
            let args = e.get("proargtypes").map_or("", String::as_str);
            let ty = |n: &str| by_name.get(n).copied().unwrap_or(0);
            signature.insert(o, (e.get("prorettype").map_or(0, |r| ty(r)), args.split_whitespace().map(ty).collect::<Vec<_>>()));
            sigs.insert(format!("{n}({})", args.split_whitespace().collect::<Vec<_>>().join(",")), o);
            let node_arg = args.split_whitespace().any(|t| POLY.contains(&format!(" {t} "))) || e.get("provariadic").is_some_and(|v| v == "any");
            let (strict, retset) = (e.get("proisstrict").is_none_or(|v| v == "t"), e.get("proretset").is_some_and(|v| v == "t"));
            let (volatility, parallel) = (Some(byte(&e, "provolatile", b'i')), Some(byte(&e, "proparallel", b's')));
            // pg_language.dat's oids: prolang defaults to internal
            let prolang = Some(match e.get("prolang").map(String::as_str) { Some("c") => 13, Some("sql") => 14, _ => 12 });
            procs.insert(o, FuncFacts { strict, retset, nargs: args.split_whitespace().count() as i16, volatility, parallel, node_arg, leakproof: Some(e.get("proleakproof").is_some_and(|v| v == "t")), prolang, body: if prolang == Some(12) { o } else { 0 } });
        }
        let pg_operator = dat("pg_operator.dat");
        let ops = pg_operator.iter().filter_map(|e| {
            let t = |k: &str| e.get(k).and_then(|n| by_name.get(n.as_str())).copied().unwrap_or(0);
            Some((oid(e)?, (names.get(e.get("oprcode")?).copied().unwrap_or(0), t("oprleft"), t("oprright"))))
        }).collect();
        // regoperator text `op(left,right)` -> oid (aggsortop)
        let side = |e: &E, k: &str| e.get(k).map_or("0", String::as_str).to_owned();
        let opsigs: HashMap<String, Oid> = pg_operator.iter().filter_map(|e| Some((format!("{}({},{})", e.get("oprname")?, side(e, "oprleft"), side(e, "oprright")), oid(e)?))).collect();
        let subscript = subscript_names.into_iter().map(|(t, h)| (t, if h == "-" { 0 } else { names.get(&h).copied().unwrap_or_else(|| panic!("typsubscript {h}")) })).collect();
        // pg_aggregate.dat with pg_aggregate.h's BKI_DEFAULTs; regproc / regoperator / regtype by name (genbki's `_<type>`), `-` = none
        let arrays: HashMap<String, Oid> = pg_type.iter().filter_map(|e| Some((format!("_{}", e.get("typname")?), e.get("array_type_oid")?.parse().ok()?))).collect();
        let aggs = dat("pg_aggregate.dat").iter().map(|e| {
            let text = |k: &str, d: &str| e.get(k).map_or(d, String::as_str).to_owned();
            let proc = |k: &str| match text(k, "-").as_str() { "-" => 0, n => names.get(n).copied().unwrap_or_else(|| panic!("pg_aggregate {k} {n}")) };
            let typ = |k: &str| match text(k, "0").as_str() { "0" => 0, n => by_name.get(n).or_else(|| arrays.get(n)).copied().unwrap_or_else(|| panic!("pg_aggregate {k} {n}")) };
            let ch = |k: &str, d: &str| text(k, d).as_bytes()[0] as i8;
            let fnoid = text("aggfnoid", "");
            let fnoid = *sigs.get(&fnoid).or_else(|| names.get(&fnoid)).unwrap_or_else(|| panic!("pg_aggregate aggfnoid {fnoid}"));   // regprocedure, or a unique regproc name
            let sortop = match text("aggsortop", "0").as_str() { "0" => 0, o => *opsigs.get(o).unwrap_or_else(|| panic!("pg_aggregate aggsortop {o}")) };
            (fnoid, syscache_seams::PgAggregateShape {
                aggkind: ch("aggkind", "n"), aggnumdirectargs: text("aggnumdirectargs", "0").parse().expect("aggnumdirectargs"),
                aggtransfn: proc("aggtransfn"), aggfinalfn: proc("aggfinalfn"), aggcombinefn: proc("aggcombinefn"),
                aggserialfn: proc("aggserialfn"), aggdeserialfn: proc("aggdeserialfn"), aggmtransfn: proc("aggmtransfn"),
                aggminvtransfn: proc("aggminvtransfn"), aggmfinalfn: proc("aggmfinalfn"),
                aggfinalextra: text("aggfinalextra", "f") == "t", aggmfinalextra: text("aggmfinalextra", "f") == "t",
                aggfinalmodify: ch("aggfinalmodify", "r"), aggmfinalmodify: ch("aggmfinalmodify", "r"), aggsortop: sortop,
                aggtranstype: typ("aggtranstype"), aggtransspace: text("aggtransspace", "0").parse().expect("aggtransspace"),
                aggmtranstype: typ("aggmtranstype"),
            })
        }).collect();
        let proc_of = |n: &str| names.get(n).copied().unwrap_or_else(|| panic!("pg_type io {n}"));
        let io = io_names.into_iter().map(|(t, i, o, ioparam, varlena)| (t, IoFacts { input: proc_of(&i), ioparam, output: proc_of(&o), varlena })).collect();
        let fam: HashMap<String, Oid> = dat("pg_opfamily.dat").iter().filter_map(|e| Some((format!("{}/{}", e.get("opfmethod")?, e.get("opfname")?), oid(e)?))).collect();
        let t = |e: &E, k: &str| by_name.get(e.get(k)?.as_str()).copied();
        let cmp = dat("pg_amproc.dat").iter().filter(|e| e.get("amprocnum").is_some_and(|n| n == "1")).filter_map(|e| Some(((*fam.get(e.get("amprocfamily")?)?, t(e, "amproclefttype")?, t(e, "amprocrighttype")?), *names.get(e.get("amproc")?)?))).collect();
        let am: HashMap<String, Oid> = dat("pg_am.dat").iter().filter_map(|e| Some((e.get("amname")?.clone(), oid(e)?))).collect();
        let opfamily = dat("pg_opfamily.dat").iter().filter_map(|e| Some((oid(e)?, (*am.get(e.get("opfmethod")?)?, e.get("opfname")?.clone())))).collect();
        let (mut amop, mut amop_at) = (HashMap::<Oid, Vec<syscache_seams::PgAmopMemberShape>>::new(), HashMap::new());
        for e in dat("pg_amop.dat") {
            let Some(((((&f, l), r), &opno), &method)) = fam.get(e.get("amopfamily").map_or("", String::as_str)).zip(t(&e, "amoplefttype")).zip(t(&e, "amoprighttype")).zip(e.get("amopopr").and_then(|o| opsigs.get(o))).zip(e.get("amopmethod").and_then(|m| am.get(m))) else { continue };
            let strategy: i16 = e.get("amopstrategy").and_then(|v| v.parse().ok()).unwrap_or(0);
            amop.entry(opno).or_default().push(syscache_seams::PgAmopMemberShape { amopfamily: f, amoplefttype: l, amoprighttype: r, amopstrategy: strategy, amopmethod: method });
            amop_at.insert((f, l, r, strategy), opno);
        }
        let amproc = dat("pg_amproc.dat").iter().filter_map(|e| Some(((*fam.get(e.get("amprocfamily")?)?, t(e, "amproclefttype")?, t(e, "amprocrighttype")?, e.get("amprocnum")?.parse().ok()?), *names.get(e.get("amproc")?)?))).collect();
        RigCatalog { types, procs, ops, subscript, aggs, io, cmp, typrow, amop, amop_at, amproc, opfamily, signature }
    }

    /// The syscache seams lowering asks outside the Catalog trait (gravity_expr's aggregate and array paths), from the same
    /// `.dat` files; once per test binary, before the first fixture decomposes.
    pub fn install_seams() {
        static SEAMS: std::sync::Once = std::sync::Once::new();
        SEAMS.call_once(|| {   // seams take fn pointers: each closure reads the catalog itself
            syscache_seams::pg_type_element_shape::set(|t| Ok(RigCatalog::get().types.get(&t).map(|f| syscache_seams::PgTypeElementShape { typelem: f.elem, typsubscript: RigCatalog::get().subscript.get(&t).copied().unwrap_or(0) })));
            syscache_seams::lookup_pg_aggregate_shape::set(|fnoid| Ok(RigCatalog::get().aggs.get(&fnoid).copied()));
            syscache_seams::pg_type_typarray::set(|t| Ok(RigCatalog::get().typrow.get(&t).map(|p| p.2))); // an ARRAY sublink's type (V4R-447)
            // D1191: a type's pg_type row as tupdesc (lookup_pg_type_shape) and the typcache (lookup_pg_type_typcache_shape)
            // ask it (a record's / an array's / a whole row's facts; gravity_lower's rig installs both)
            syscache_seams::lookup_pg_type_shape::set(|t| Ok(RigCatalog::get().types.get(&t).zip(RigCatalog::get().typrow.get(&t)).map(|(f, p)| types_tuple::PgTypeShape { typlen: f.len, typbyval: f.byval, typalign: f.align, typstorage: p.1, typcollation: f.collation })));
            syscache_seams::lookup_pg_type_typcache_shape::set(|t| Ok(RigCatalog::get().types.get(&t).zip(RigCatalog::get().typrow.get(&t)).map(|(f, p)| {
                let mut typname = types_tuple::NameData::default();
                typname.namestrcpy(&p.0);
                let typsubscript = RigCatalog::get().subscript.get(&t).copied().unwrap_or(0);
                syscache_seams::PgTypeTypcacheShape { typname, typlen: f.len, typbyval: f.byval, typalign: f.align, typstorage: p.1, typtype: f.typtype, typisdefined: true, typrelid: 0, typsubscript, typelem: f.elem, typarray: p.2, typcollation: f.collation }
            })));
            // typesem's key reader (Catalog::hash_key, a hashed SubPlan's key): the operator, its families and their procs
            syscache_seams::lookup_pg_operator_shape::set(|o| Ok(RigCatalog::get().ops.get(&o).map(|&(oprcode, oprleft, oprright)| syscache_seams::PgOperatorShape { oprnamespace: 11, oprleft, oprright, oprresult: 16, oprcom: 0, oprnegate: 0, oprcode, oprrest: 0, oprjoin: 0, oprcanmerge: false, oprcanhash: false })));
            syscache_seams::lookup_pg_amop_members_by_operator::set(|m, o| {
                let mut v = mcx::PgVec::new_in(m);
                for &a in RigCatalog::get().amop.get(&o).into_iter().flatten() {
                    v.push(a);
                }
                Ok(v)
            });
            syscache_seams::lookup_pg_amop_by_strategy::set(|f, l, r, s| Ok(RigCatalog::get().amop_at.get(&(f, l, r, s)).copied().unwrap_or(0)));
            syscache_seams::lookup_pg_amproc::set(|f, l, r, n| Ok(RigCatalog::get().amproc.get(&(f, l, r, n)).copied().unwrap_or(0)));
            // KeySem's sort key reader (a streamed SetOp's keys): get_equality_op_for_ordering_op asks the family's method
            syscache_seams::lookup_pg_opfamily_shape::set(|f| {
                Ok(RigCatalog::get().opfamily.get(&f).map(|(method, name)| {
                    let mut opfname = types_tuple::NameData::default();
                    opfname.namestrcpy(name);
                    syscache_seams::PgOpfamilyShape { opfmethod: *method, opfname }
                }))
            });
            // and its comparator's argument type (`word_order`: get_func_signature)
            syscache_seams::lookup_pg_proc_signature::set(|m, f| {
                Ok(RigCatalog::get().signature.get(&f).map(|(ret, args)| {
                    let mut v = mcx::PgVec::new_in(m);
                    args.iter().for_each(|&a| v.push(a));
                    (*ret, v)
                }))
            });
            rig_fmgr::install(); // D537: fmgr_info's pg_proc leg (a LANGUAGE sql call)
        });
        // D3606: every thread lowers as a served statement runs (per thread, so outside the once): armed with its
        // database's encoding, UTF8 (pgrust serves no other: initdb and CREATE DATABASE refuse the rest), and with the rig
        // databases' default collation (initdb --no-locale: C). Unarmed, a test thread is SQL_ASCII with no default locale,
        // where every UTF8-gated text path (text.memmem claims, like_bound, lower_like, text_frame, RE2) is off.
        gravity_fn::typesem::collation::arm_database_encoding(6).expect("UTF8");
        if !pg_locale::default_locale_installed() {
            pg_locale::set_default_locale_c_for_tests();
        }
    }
}

fn unknown(what: &str, oid: Oid) -> Box<PgError> {
    Box::new(PgError::error(format!("v4 rig catalog: no {what} {oid}")))
}

impl<'mcx> Catalog<'mcx> for RigCatalog {
    fn func(&self, funcid: Oid) -> PgResult<FuncFacts> {
        self.procs.get(&funcid).copied().ok_or_else(|| unknown("function", funcid))
    }
    fn opcode(&self, opno: Oid) -> PgResult<Oid> {
        self.ops.get(&opno).map(|o| o.0).ok_or_else(|| unknown("operator", opno))
    }
    fn op_input_types(&self, opno: Oid) -> PgResult<(Oid, Oid)> {
        self.ops.get(&opno).map(|o| (o.1, o.2)).ok_or_else(|| unknown("operator", opno))
    }
    fn btree_cmp_proc(&self, opfamily: Oid, l: Oid, r: Oid) -> PgResult<Oid> {
        self.cmp.get(&(opfamily, l, r)).copied().ok_or_else(|| unknown("opfamily", opfamily))
    }
    fn opclass(&self, opclass: Oid) -> PgResult<(Oid, Oid)> {
        Err(unknown("opclass", opclass))
    }
    fn type_facts(&self, typid: Oid) -> PgResult<TypeFacts> {
        self.types.get(&typid).copied().ok_or_else(|| unknown("type", typid))
    }
    fn type_cmp_proc(&self, typid: Oid) -> PgResult<Oid> {
        Err(unknown("type", typid))
    }
    fn base_type(&self, typid: Oid, _typmod: &mut i32) -> PgResult<Oid> {
        Ok(typid)
    }
    fn domain(&self, typid: Oid) -> PgResult<DomainFacts<'mcx>> {
        Err(unknown("domain", typid))
    }
    fn io(&self, typid: Oid) -> PgResult<IoFacts> {
        self.io.get(&typid).copied().ok_or_else(|| unknown("io of", typid))
    }
    fn text(&self, _collation: Oid) -> PgResult<TextFacts> {
        Ok(TextFacts { deterministic: true, equality_is_bytes: true, order_is_bytes: true, ctype_is_c: true })
    }
    fn tier(&self, funcid: Oid) -> Tier {
        gravity_fn::tier_of(funcid)
    }
    fn shim(&self, funcid: Oid) -> Option<&'static Shim> {
        gravity_fn::shim_of(funcid)
    }
    fn hash_key(&self, op: Oid, coll: Oid) -> PgResult<Result<gravity_fn::typesem::KeySem, gravity_fn::typesem::TypeClass>> {
        let cx = MemoryContext::new("v4 rig hash_key");
        gravity_fn::typesem::resolve(cx.mcx(), &gravity_fn::typesem::KeyReq::hash(op, coll))
    }
    fn hash_key_typed(&self, op: Oid, coll: Oid, lt: Oid, rt: Oid) -> PgResult<Result<gravity_fn::typesem::KeySem, gravity_fn::typesem::TypeClass>> {
        let cx = MemoryContext::new("v4 rig hash_key_typed");
        gravity_fn::typesem::resolve(cx.mcx(), &gravity_fn::typesem::KeyReq::hash(op, coll).typed(lt, rt))
    }
}

/// The census's catalog for one plan fixture: the rig's (`RigCatalog`), except a function the fixture's `-- func` lines
/// describe, answered from the same line `Facts::func` reads, so a reader of either sees one row.
pub struct FixtureCatalog<'f> {
    pub funcs: &'f [corpus::FuncLine],
}

impl<'f> FixtureCatalog<'f> {
    pub fn of(facts: &'f corpus::FixtureFacts) -> Self {
        FixtureCatalog { funcs: &facts.funcs }
    }
}

/// A `-- func` line as the catalog's row (the line names no body: a function past the builtin table has none).
pub fn func_facts(f: &corpus::FuncLine) -> FuncFacts {
    use gravity_decompose::effects::Parallel;
    let parallel = match f.props.parallel {
        Parallel::Safe => b's',
        Parallel::Restricted => b'r',
        Parallel::Unsafe => b'u',
    };
    FuncFacts {
        strict: f.props.strict,
        retset: f.retset,
        nargs: f.nargs,
        volatility: Some(f.props.volatility.letter() as u8),
        parallel: Some(parallel),
        node_arg: f.node_arg,
        leakproof: Some(f.props.leakproof),
        prolang: Some(f.lang),
        body: 0,
    }
}

impl<'mcx> Catalog<'mcx> for FixtureCatalog<'_> {
    fn func(&self, funcid: Oid) -> PgResult<FuncFacts> {
        match self.funcs.iter().find(|f| f.oid == funcid) {
            Some(f) => Ok(func_facts(f)),
            None => <RigCatalog as Catalog<'mcx>>::func(RigCatalog::get(), funcid),
        }
    }
    fn opcode(&self, opno: Oid) -> PgResult<Oid> {
        <RigCatalog as Catalog<'mcx>>::opcode(RigCatalog::get(), opno)
    }
    fn op_input_types(&self, opno: Oid) -> PgResult<(Oid, Oid)> {
        <RigCatalog as Catalog<'mcx>>::op_input_types(RigCatalog::get(), opno)
    }
    fn btree_cmp_proc(&self, opfamily: Oid, l: Oid, r: Oid) -> PgResult<Oid> {
        <RigCatalog as Catalog<'mcx>>::btree_cmp_proc(RigCatalog::get(), opfamily, l, r)
    }
    fn opclass(&self, opclass: Oid) -> PgResult<(Oid, Oid)> {
        <RigCatalog as Catalog<'mcx>>::opclass(RigCatalog::get(), opclass)
    }
    fn type_facts(&self, typid: Oid) -> PgResult<TypeFacts> {
        <RigCatalog as Catalog<'mcx>>::type_facts(RigCatalog::get(), typid)
    }
    fn type_cmp_proc(&self, typid: Oid) -> PgResult<Oid> {
        <RigCatalog as Catalog<'mcx>>::type_cmp_proc(RigCatalog::get(), typid)
    }
    fn base_type(&self, typid: Oid, typmod: &mut i32) -> PgResult<Oid> {
        <RigCatalog as Catalog<'mcx>>::base_type(RigCatalog::get(), typid, typmod)
    }
    fn domain(&self, typid: Oid) -> PgResult<DomainFacts<'mcx>> {
        <RigCatalog as Catalog<'mcx>>::domain(RigCatalog::get(), typid)
    }
    fn io(&self, typid: Oid) -> PgResult<IoFacts> {
        <RigCatalog as Catalog<'mcx>>::io(RigCatalog::get(), typid)
    }
    fn text(&self, collation: Oid) -> PgResult<TextFacts> {
        <RigCatalog as Catalog<'mcx>>::text(RigCatalog::get(), collation)
    }
    fn tier(&self, funcid: Oid) -> Tier {
        <RigCatalog as Catalog<'mcx>>::tier(RigCatalog::get(), funcid)
    }
    fn shim(&self, funcid: Oid) -> Option<&'static Shim> {
        <RigCatalog as Catalog<'mcx>>::shim(RigCatalog::get(), funcid)
    }
    fn hash_key(&self, op: Oid, coll: Oid) -> PgResult<Result<gravity_fn::typesem::KeySem, gravity_fn::typesem::TypeClass>> {
        <RigCatalog as Catalog<'mcx>>::hash_key(RigCatalog::get(), op, coll)
    }
    fn hash_key_typed(&self, op: Oid, coll: Oid, lt: Oid, rt: Oid) -> PgResult<Result<gravity_fn::typesem::KeySem, gravity_fn::typesem::TypeClass>> {
        <RigCatalog as Catalog<'mcx>>::hash_key_typed(RigCatalog::get(), op, coll, lt, rt)
    }
}

thread_local! {
    /// D4620: the default-off roads the next fixtures on this thread read on (`FixtureFacts::roads_on`, as
    /// `SET pgrust.gravity_road_off = '+<road>'` on a server); empty, every such road reads off.
    pub static ROADS_ON: core::cell::RefCell<Vec<&'static str>> = const { core::cell::RefCell::new(Vec::new()) };
}

/// One plan fixture (decompose's corpus text) through decompose, then `f` over the graph and its facts: `None` when
/// decompose does not take the statement (not a graph statement).
fn fixture<R>(text: &str, f: impl FnOnce(&Arena, &gravity_decompose::PipelineGraph<'_>, &corpus::FixtureFacts) -> R) -> Option<R> {
    RigCatalog::install_seams();
    let ctx = MemoryContext::new("v4 rig fixture");
    let mut fx = corpus::load(ctx.mcx(), text).unwrap_or_else(|e| panic!("fixture: {e}"));
    fx.facts.roads_on = ROADS_ON.with(|r| r.borrow().clone());
    let arena = Arena::new(1 << 20);
    let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).ok()?;
    Some(f(&arena, &graph, &fx.facts))
}

/// lower_graph as the server's admission calls it: the compiler's EXECUTE gates deferred (exec admit.rs `install`: the
/// check is every execution's), so no ACL seam is asked.
fn lower<'p>(a: &Arena, g: &gravity_decompose::PipelineGraph<'p>, facts: &corpus::FixtureFacts, words: &mut gravity_lower::Sequential<'p>, opts: gravity_lower::LowerOpts) -> gravity_lower::Result<Lowered> {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    gravity_expr::nodes::func::acl_defer_begin();
    let l = gravity_lowerv4::lower_graph(a.handle(), rig.mcx(), g, facts, &FixtureCatalog::of(facts), words, opts);
    gravity_expr::nodes::func::acl_defer_end();
    l
}

/// D3886's lowering guard (tests/lower_insns_guard.rs): plan fixture `text` decomposed once and lowered `n` times, each
/// lowering alone between `start` and `stop` (its own arena and word plan; the `Lowered` drops after `stop`). `None` when
/// decompose does not take the statement; `Some(Err)`: the lowering's refusal.
pub fn lower_n(text: &str, n: usize, mut start: impl FnMut(), mut stop: impl FnMut()) -> Option<Result<(), String>> {
    fixture(text, |_, g, facts| {
        for _ in 0..n {
            let a = Arena::new(1 << 20);
            let mut words = gravity_lower::Sequential::new(a.handle());
            start();
            let l = lower(&a, g, facts, &mut words, gravity_lower::LowerOpts::default());
            stop();
            if let Err(r) = l {
                return Err(r.to_string());
            }
        }
        Ok(())
    })
}

/// D4620: the grouping-set cascades one plan fixture's graph declares (`PipelineGraph::cascades`).
pub fn cascades_of(text: &str) -> Option<usize> {
    fixture(text, |_, g, _| g.cascades.len())
}

/// The kinds and properties `props.rs` reads off one plan fixture's graph (what admission sees), by name.
pub fn props_fixture(text: &str) -> Option<(BTreeSet<String>, BTreeSet<String>)> {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    use gravity_lowerv4::{applies::Prop, graph::Gx, props::Props};
    fixture(text, |_, graph, facts| {
        let catalog = FixtureCatalog::of(facts);
        let p = Props::of(&Gx { graph, facts, catalog: &catalog, over: None, mcx: rig.mcx() });
        (p.kinds.iter().map(|k| k.name().to_owned()).collect(), Prop::ALL.iter().filter(|x| p.props.has(**x)).map(|x| x.name().to_owned()).collect())
    })
}

// ------------------------------------------------------------------------------------------------ running (gravity_driver)

/// The statement a plan fixture lowers to, run by the real driver in `mode` (main 09-24, decision 2): `Entry` / `Binding`,
/// the interpreter's code, the plan's literals as the bind words. Only a program without a source or write site runs here (a
/// Result, VALUES or function statement); the server runner judges the rest. `Err` is why no outcome: `refused: ..` (the lowering's), else a
/// rig or driver failure.
pub fn run_fixture(text: &str, mode: Mode) -> Option<Result<Outcome, String>> {
    fixture(text, |a, g, facts| {
        let mut words = gravity_lower::Sequential::new(a.handle());
        let l = lower(a, g, facts, &mut words, gravity_lower::LowerOpts::default()).map_err(|r| format!("refused: {r}"))?;
        // M5 L0-e: a source or write site needs the server's scan / writer (the driver derives only those, driver node.rs); a
        // frame-words site (an expanding step's offset word, frame/px.rs) and an Init proto (M8, on_demand.rs) run here
        let hosted = l.descs.sites.iter().filter(|s| s.is_source() || matches!(s.kind, gravity_program::SiteKind::Write { .. })).count();
        if hosted > 0 {
            return Err(format!("{hosted} host site(s) (a source / write): the server runner's statement, not the crate's"));
        }
        let codes = gravity_lower::compile(&l, gravity_lower::Backend::Interp).map_err(|e| format!("backend: {}", e.message()))?;
        let program = std::sync::Arc::new(gravity_lower::assemble(&l, codes));
        let n = words.words.len().max(1);
        let (mut params, mut nulls) = (vec![0u64; n], vec![0u8; n]);
        for (i, w) in words.words.iter().enumerate() {
            match *w {
                gravity_lower::WordSrc::Const { value, .. } | gravity_lower::WordSrc::Elem { value, .. } => (params[i], nulls[i]) = (value.value.as_usize() as u64, value.isnull as u8),
                gravity_lower::WordSrc::Kernel => {}
                other => return Err(format!("word {i} is {other:?}: the crate binds literals only")),
            }
        }
        let mut lives = on_demand::lay(&program, &mut params, &mut nulls)?;
        lives.extend(on_demand::lay_literals(&program, g, &mut params, &mut nulls)?);
        gravity_fn::result_life::scribble::force(mode == Mode::Scribble);
        let r = drive::drive(&program, &params, &nulls, mode, &lives);
        gravity_fn::result_life::scribble::force(false);
        r
    })
}

pub fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::Straight => "straight",
        Mode::Stop(_) => "stop",
        Mode::Cancel(_) => "cancel",
        Mode::Scribble => "scribble",
    }
}

/// The rows a statement sends, as COPY text; `Cancel(n)` raises the cancel byte at the n-th push.
struct Collect<'o> {
    out: &'o [gravity_program::OutCol],
    rows: Vec<Row>,
    err: Option<String>,
    pushes: u32,
    cancel: Option<(u32, &'o std::sync::atomic::AtomicU8)>,
}

impl gravity_driver::Sink for Collect<'_> {
    fn push(&mut self, b: gravity_driver::BorrowedBatch<'_>) -> gravity_driver::Result<gravity_driver::Flow> {
        for i in 0..b.rows {
            let (v, n) = b.row(i);
            match self.out.iter().enumerate().map(|(k, c)| cell(c, v[k], n[k] != 0)).collect() {
                Ok(r) => self.rows.push(r),
                Err(e) => drop(self.err.get_or_insert(e)),
            }
        }
        self.pushes += 1;
        match self.cancel {
            Some((n, f)) if self.pushes >= n => f.store(1, std::sync::atomic::Ordering::Release),
            _ => {}
        }
        Ok(gravity_driver::Flow::Continue)
    }
}

/// One out cell as COPY text (unescaped, as parse_case reads a golden): the types the crate's fixtures return.
fn cell(c: &gravity_program::OutCol, v: u64, null: bool) -> Result<Option<String>, String> {
    if null {
        return Ok(None);
    }
    Ok(Some(match c.typid {
        16 => (if v != 0 { "t" } else { "f" }).to_owned(),
        21 => (v as i16).to_string(),
        23 => (v as i32).to_string(),
        20 => (v as i64).to_string(),
        25 | 1042 | 1043 | 705 | 1700 => {
            let p = v as *const u8;
            // SAFETY: a by-reference out datum points at a varlena the push's region or the bound words hold for the push.
            let b0 = unsafe { *p };
            let (hdr, len) = match b0 & 3 {
                _ if b0 == 1 => return Err("an out-of-line (TOAST) datum".into()),
                1 | 3 => (1, (b0 >> 1) as usize),
                // SAFETY: as above; a 4-byte header.
                0 => (4, (unsafe { (p as *const u32).read_unaligned() } >> 2) as usize),
                _ => return Err("a compressed datum".into()),
            };
            // SAFETY: the varlena holds `len` bytes, its header included.
            let body = unsafe { std::slice::from_raw_parts(p.add(hdr), len.saturating_sub(hdr)) };
            let mut num = Vec::new(); // numeric (M8-RERUN B2c: m8x-032's sum over int8) renders through numeric_out
            let body = if c.typid == 1700 { adt_numeric::numeric_out_into(adt_numeric::Num::from_payload(body), &mut num); &num[..] } else { body };
            String::from_utf8_lossy(body).into_owned()
        }
        t => return Err(format!("out type {t}: the crate renders bool / int2 / int4 / int8 / text / numeric only")),
    }))
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
/// These tests run as package gravity_lowerv4_it (its dir is the sibling crates/gravity/lowerv4_it): the last component is
/// swapped back to lowerv4, whose fixtures and sources they read.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(|d| std::path::Path::new(&d).with_file_name("lowerv4")).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}

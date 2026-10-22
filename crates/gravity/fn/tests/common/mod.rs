//! Shared by the crate tests: the reference catalog (pg_proc / pg_type rows the tiers were
//! classified from), the test-time seam installation, and the Datum image builders.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Once;

use datum::Datum;

pub fn repo_root() -> PathBuf {
    manifest_dir().to_path_buf().join("../../..").canonicalize().unwrap()
}

pub fn parse_dat(path: &str) -> Vec<HashMap<String, String>> {
    let text = std::fs::read_to_string(repo_root().join(path)).unwrap();
    let text: String = text.lines().filter(|l| !l.trim_start().starts_with('#')).collect::<Vec<_>>().join("\n");
    let mut rows = Vec::new();
    let mut rest = text.as_str();
    while let Some(s) = rest.find('{') {
        // the matching close brace, braces inside quoted values ignored
        let bytes = rest.as_bytes();
        let (mut j, mut depth, mut quoted) = (s, 0usize, false);
        while j < bytes.len() {
            match bytes[j] {
                b'\\' if quoted => j += 1,
                b'\'' => quoted = !quoted,
                b'{' if !quoted => depth += 1,
                b'}' if !quoted => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        let e = j - s;
        let mut body = &rest[s + 1..s + e];
        let mut row = HashMap::new();
        while let Some(arrow) = body.find("=>") {
            let key = body[..arrow].trim().trim_start_matches(',').trim().to_owned();
            let after = &body[arrow + 2..];
            let Some(q) = after.find('\'') else { break };
            let vb = after[q + 1..].as_bytes();
            let mut k = 0;
            let mut val = String::new();
            while k < vb.len() && vb[k] != b'\'' {
                if vb[k] == b'\\' && k + 1 < vb.len() {
                    k += 1;
                }
                val.push(vb[k] as char);
                k += 1;
            }
            row.insert(key, val);
            body = after.get(q + 1 + k + 1..).unwrap_or("");
        }
        if !row.is_empty() {
            rows.push(row);
        }
        rest = &rest[s + e + 1..];
    }
    rows
}

pub struct Proc {
    pub oid: u32,
    pub name: String,
    pub prosrc: String,
    pub args: Vec<String>,
    pub ret: String,
    pub strict: bool,
    pub volatile: char,
    pub kind: char,
    pub retset: bool,
    pub variadic: bool,
}

pub fn procs() -> HashMap<u32, Proc> {
    parse_dat("crates/postgres-18.6-reference/src/include/catalog/pg_proc.dat")
        .into_iter()
        .filter(|r| r.contains_key("oid") && r.contains_key("proname"))
        .map(|r| {
            let p = Proc {
                oid: r["oid"].parse().unwrap(),
                name: r["proname"].clone(),
                prosrc: r.get("prosrc").cloned().unwrap_or_default(),
                args: r.get("proargtypes").map(|a| a.split_whitespace().map(str::to_owned).collect()).unwrap_or_default(),
                ret: r["prorettype"].clone(),
                strict: r.get("proisstrict").map_or(true, |s| s == "t"),
                volatile: r.get("provolatile").map_or('i', |s| s.chars().next().unwrap()),
                kind: r.get("prokind").map_or('f', |s| s.chars().next().unwrap()),
                retset: r.get("proretset").is_some_and(|s| s == "t"),
                variadic: r.contains_key("provariadic") || r.contains_key("proallargtypes"),
            };
            (p.oid, p)
        })
        .collect()
}

/// The index AM support procedures (`pg_amproc.dat`): the catalog ground for a `handler/internal`
/// row whose types alone do not say so.
pub fn amprocs() -> std::collections::HashSet<String> {
    parse_dat("crates/postgres-18.6-reference/src/include/catalog/pg_amproc.dat").into_iter().filter_map(|r| r.get("amproc").cloned()).collect()
}

pub struct Ty {
    pub oid: u32,
    pub byval: bool,
    pub len: i32,
    pub collation: bool,
    pub input: String,
    pub elem: String,
    pub align: u8,
    pub array_oid: u32,
}

pub fn types() -> HashMap<String, Ty> {
    parse_dat("crates/postgres-18.6-reference/src/include/catalog/pg_type.dat")
        .into_iter()
        .filter(|r| r.contains_key("typname"))
        .map(|r| {
            let t = Ty {
                oid: r.get("oid").and_then(|o| o.parse().ok()).unwrap_or(0),
                byval: matches!(r.get("typbyval").map(String::as_str), Some("t") | Some("FLOAT8PASSBYVAL")),
                len: match r.get("typlen").map(String::as_str) {
                    Some("NAMEDATALEN") => 64,
                    Some("SIZEOF_POINTER") => 8,
                    Some(l) => l.parse().unwrap_or(-1),
                    None => -1,
                },
                collation: r.get("typcollation").is_some_and(|c| c != "0"),
                input: r.get("typinput").cloned().unwrap_or_default(),
                elem: r.get("typelem").cloned().unwrap_or_default(),
                align: r.get("typalign").and_then(|a| a.bytes().next()).unwrap_or(b'i'),
                array_oid: r.get("array_type_oid").and_then(|o| o.parse().ok()).unwrap_or(0),
            };
            (r["typname"].clone(), t)
        })
        .collect()
}

/// The shim's argument class for a SQL type (the generator's `arg_class`): `V` by value, `P` a
/// payload view (the text class, numeric, interval), `I` an image view, `D` a polymorphic element.
pub fn arg_kind(t: &str, types: &HashMap<String, Ty>) -> gravity_fn::ArgKind {
    use gravity_fn::ArgKind::*;
    if BYVAL_TYPES.contains(&t) {
        return V;
    }
    if VIEW_TYPES.contains(&t) || BYREF_TYPES.contains(&t) || t == "unknown" {
        return P;
    }
    if POLY_ELEM.contains(&t) {
        return D;
    }
    if POLY_IMG.contains(&t) {
        return I;
    }
    match types.get(t) {
        Some(ty) if ty.byval => V,
        Some(ty) if ty.len == -2 => P,
        _ => I,
    }
}

pub const POLY_ELEM: &[&str] = &["anyelement", "anynonarray", "anyenum", "anycompatible", "anycompatiblenonarray", "any"];
pub const POLY_IMG: &[&str] = &["anyarray", "anycompatiblearray", "anyrange", "anycompatiblerange", "anymultirange", "anycompatiblemultirange", "record"];

/// The concrete type a polymorphic parameter takes in instantiation `k` of the differential.
pub fn instantiate(t: &str, k: usize) -> Option<&'static str> {
    Some(match t {
        "anyelement" | "anynonarray" | "anycompatible" | "anycompatiblenonarray" | "any" => ["int4", "text"][k],
        "anyarray" | "anycompatiblearray" => ["_int4", "_text"][k],
        "anyenum" => "oid",
        "anyrange" | "anycompatiblerange" => ["int4range", ""][k],
        "anymultirange" | "anycompatiblemultirange" => ["int4multirange", ""][k],
        "record" => return None,
        _ => return Some(""),
    })
    .filter(|s| !s.is_empty())
}

pub const BYVAL_TYPES: &[&str] = &[
    "bool", "char", "int2", "int4", "int8", "oid", "xid", "cid", "xid8", "float4", "float8", "money", "date", "time",
    "timestamp", "timestamptz", "pg_lsn", "regproc", "regprocedure", "regoper", "regoperator", "regclass",
    "regcollation", "regtype", "regrole", "regnamespace", "regconfig", "regdictionary",
];

/// Whether a result of SQL type `t` comes back by value from the fmgr call and the shim.
pub fn ret_byval(t: &str, types: &HashMap<String, Ty>) -> bool {
    BYVAL_TYPES.contains(&t) || types.get(t).is_some_and(|ty| ty.byval)
}
pub const VIEW_TYPES: &[&str] = &["text", "varchar", "bpchar", "bytea", "name", "cstring"];
pub const BYREF_TYPES: &[&str] = &["numeric", "interval"];
pub const HANDLER_TYPES: &[&str] = &["internal", "trigger", "event_trigger", "language_handler", "fdw_handler", "index_am_handler", "tsm_handler", "table_am_handler", "pg_ddl_command", "void"];
pub const POLY_TYPES: &[&str] = &["anyelement", "anyarray", "anynonarray", "anyenum", "anyrange", "anymultirange", "anycompatible", "anycompatiblearray", "anycompatiblenonarray", "anycompatiblerange", "anycompatiblemultirange", "any", "record", "unknown"];

pub fn plain(t: &str) -> bool {
    BYVAL_TYPES.contains(&t) || VIEW_TYPES.contains(&t) || BYREF_TYPES.contains(&t)
}

static SEAMS: Once = Once::new();

/// The seams the typed bodies read at test time: GUC backing stores, the encoding, the float
/// digits, the time zone facts, the default (C) locale — and the server's builtin tables (the
/// overlay and the late tables), so the fmgr side resolves every wrapper a shim was cut from.
pub fn install_seams() {
    SEAMS.call_once(|| {
        // SAFETY: single-threaded test init, before any getenv (adt_timestamp's tests' precedent).
        unsafe { std::env::set_var("PGRUST_TZDIR", "/usr/share/zoneinfo") };
        seams_init::register_builtin_tables();
        init_small::init_seams();
        mbutils::init_seams();
        adt_float::init_seams();
        pgtz::init_seams();
        adt_timestamp::init_seams();
        guc_tables::init_seams();
        elog::init_seams();
        fd::init_seams();
        xact_seams::get_current_sub_transaction_id::set(|| 1);
        pg_locale::init_seams();
        pg_locale::set_default_locale_c_for_tests();
        detoast::init_seams();
        regex_core::init_seams();
        adt_regexp::init_seams();
        postgres_seams::check_for_interrupts::set(|| Ok(()));
        guc_tables::vars::quote_all_identifiers.install(guc_tables::GucVarAccessors { get: || false, set: |_| {} });
        adt_datetime::tz::pg_timezone_initialize().unwrap();
    });
}

/// A 4-byte-header varlena image over a payload (the Datum the fmgr path sees).
pub fn image(payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(payload.len() + 4);
    v.extend_from_slice(&datum::set_varsize_4b(payload.len() + 4));
    v.extend_from_slice(payload);
    v
}

pub fn image_datum(img: &[u8]) -> Datum {
    Datum::from_usize(img.as_ptr() as usize)
}

/// The byte size of a by-reference result image at `p` for the SQL type `t`.
pub fn type_oid(t: &str, types: &HashMap<String, Ty>) -> u32 {
    match t.strip_prefix('_') {
        Some(elem) => types[elem].array_oid,
        None => types[t].oid,
    }
}

pub unsafe fn result_len(p: *const u8, t: &str, types: &HashMap<String, Ty>) -> usize {
    match t {
        "cstring" | "unknown" => core::ffi::CStr::from_ptr(p.cast()).to_bytes().len() + 1,
        t if t.starts_with('_') || POLY_IMG.contains(&t) => (p.cast::<u32>().read_unaligned() >> 2) as usize,
        _ => match types[t].len {
            -1 => {
                let b = *p;
                if b & 1 == 1 {
                    ((b >> 1) & 0x7f) as usize
                } else {
                    (p.cast::<u32>().read_unaligned() >> 2) as usize
                }
            }
            n => n as usize,
        },
    }
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}

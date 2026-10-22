//! The rig's result comparator (plan §5.6, ruling 1): classify a statement from its text, then diff PG's result and v4's by
//! the class. One algorithm with scripts/gravity-gate/multiset.py, held to it by the shared fixtures (`common::selftest`).
//! Values and keys compare as exact text (NULL = NULL, NULL != ''): nothing is normalised inside a value, so ordered aggs,
//! windows and string_agg ORDER BY stay exact under every class, and keys `1.0` / `1.00` are distinct runs (strict, never
//! lax). string_agg / array_agg / json_agg with no ORDER BY also compare exactly (main 09-24): a real element permutation
//! gets a named residual class checking it is a permutation of PG's elements, never a silent pass.
//! V4R-2 (Michael 09-24): a column whose value is an order-dependent float aggregate compares within 1e-12 relative (float8)
//! or 4 ulps (float4); NaN / Infinity / NULL exactly, -0 = +0. Which columns: the harness's plan (`Tol`) when it has one,
//! else `float_cols`' reading of the SQL, the same as multiset.py's float_cols(); everything else stays exact.
//! V4R-270 (D7): an equal-not-identical group / DISTINCT key outputs any member of its group: `Eqk` maps a key column's
//! texts to their class under PG's `=` for the column's type and collation (the rig asks PG), both sides before the compare.
//! D3927 (user ruling 2026-10-11, main 13:26:21Z; amends V4R-2 for this shape only): a select item round(<float
//! aggregate>::numeric [, n]) compares as decimals within one unit in its n-th decimal (`F::R(n)`), as multiset.py's
//! round_kind() / tol_eq() 'r<n>'; NULL and NaN exactly.
//! D4019 (main 2026-10-11T14:56:26Z, seed 17 rnd-4115): an ORDER BY key whose declared type is numeric groups its tie runs by
//! VALUE (`peer_keys`, `num_value`), so dscale-only peers (0.000 / 0.00000000000000000000) are one run and their order is free
//! (the 09-29 equal-member ruling); the keys only, every row stays exact text. No declared type: runs by text, as above.
//! D4194 (main 2026-10-12T16:02:05Z GO; wfprove's same-shape cases): the same for the two other key types whose `=` is coarser
//! than their text (`Peer`, `peer_value`): float4 / float8 (-0 = 0, float8_cmp) and interval (interval_cmp's span: '1 mon' =
//! '30 days', '24:00:00' = '1 day'). Not a judging equivalence: only the order among peers under the sort's own `=` is free;
//! jsonb numerics, arrays / records of these types and non-deterministic collations stay strict.
use std::cmp::Ordering;
use std::collections::BTreeMap;

pub type Row = Vec<Option<String>>;

/// Key columns are 0-based result positions (the fixtures write them 1-based).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Class { Exact, Unordered, Ordered(Vec<usize>), Limited(Vec<usize>) }

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome { Rows(Vec<Row>), Error { sqlstate: String, message: String } }

/// `MismatchOrder`: the rows are equal as multisets but the class compare rejects them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict { Match, Mismatch, MismatchOrder }

/// A float-tolerant column's type; `Tol` maps 0-based result columns to it. `R(n)` (D3927): round(<float
/// aggregate>::numeric, n), decimals within 10^-n.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum F { F4, F8, R(i32) }
pub type Tol = BTreeMap<usize, F>;
/// V4R-270: 0-based key column -> {text: its equality class's text}.
pub type Eqk = BTreeMap<usize, BTreeMap<String, String>>;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok { Word(String, bool), Lit(String), P(char) }

type T = (Tok, u32);
const OPS: &str = "+-*/<>=~!@#%^&|`?";

/// Tokens with their paren / bracket depth (a bracket carries its enclosing depth). Comments are skipped (block comments
/// nest); unquoted words are ASCII-lowercased; E'' B'' X'' N'' U&'' prefixes fold into the string; `$n`, `::` and
/// `$tag$..$tag$` are one token each; an operator run stops before `--` or `/*`.
fn lex(sql: &str) -> Vec<T> {
    let c: Vec<char> = sql.chars().collect();
    let at = |i: usize, s: &str| s.chars().enumerate().all(|(k, x)| c.get(i + k) == Some(&x));
    let run = |mut i: usize, f: &dyn Fn(usize) -> bool| -> usize { while i < c.len() && f(i) { i += 1 } i };
    let word = |i: usize| c[i].is_alphanumeric() || c[i] == '_' || c[i] == '$' || !c[i].is_ascii();
    let digit = |i: usize| c.get(i).is_some_and(|y| y.is_ascii_digit());
    let span = |s: usize, e: usize| c[s..e.min(c.len())].iter().collect::<String>();
    // the closing `$` of a dollar-quote tag opening at `i`: `$$` or `$tag$`, the tag not starting with a digit
    let tag = |i: usize| Some(i + 1 + c[i + 1..].iter().enumerate().take_while(|(k, y)| (y.is_alphanumeric() || **y == '_') && !(*k == 0 && y.is_ascii_digit())).count()).filter(|&j| c.get(j) == Some(&'$'));
    let (mut i, mut d, mut out) = (0usize, 0u32, Vec::new());
    while i < c.len() {
        let (x, s) = (c[i], i);
        if x.is_whitespace() || at(i, "--") {
            i = if x.is_whitespace() { i + 1 } else { run(i, &|j| c[j] != '\n') };
            continue;
        } else if at(i, "/*") {
            let mut n = 0;
            while i < c.len() && !(n == 1 && at(i, "*/")) { (n, i) = (n + at(i, "/*") as i32 - at(i, "*/") as i32, i + 1 + (at(i, "/*") || at(i, "*/")) as usize) }
            i += 2;
            continue;
        } else if x == '\'' || ("eEbBxXnN".contains(x) && at(i + 1, "'")) || at(i, "U&'") || at(i, "u&'") {
            let bs = "eE".contains(x);
            i += c[i..].iter().position(|&y| y == '\'').unwrap_or(0) + 1;
            while i < c.len() && !(c[i] == '\'' && !at(i + 1, "'")) { i += 1 + ((bs && c[i] == '\\') || c[i] == '\'') as usize }
            i += 1;
        } else if x == '"' {
            i += 1;
            while i < c.len() && !(c[i] == '"' && !at(i + 1, "\"")) { i += 1 + (c[i] == '"') as usize }
            i += 1;
            out.push((Tok::Word(span(s + 1, i - 1).replace("\"\"", "\""), true), d));
            continue;
        } else if x == '$' && digit(i + 1) {
            i = run(i + 1, &|j| digit(j));
        } else if let Some(e) = (x == '$').then(|| tag(i)).flatten() {
            let t = span(s, e + 1);
            i = run(e + 1, &|j| !at(j, &t)) + t.chars().count();
        } else if x.is_ascii_digit() || (x == '.' && digit(i + 1)) {
            i = run(i, &|j| digit(j) || c[j] == '.');
            if c.get(i).is_some_and(|y| "eE".contains(*y)) && (digit(i + 1) || (c.get(i + 1).is_some_and(|y| "+-".contains(*y)) && digit(i + 2))) {
                i = run(i + 2, &|j| digit(j));
            }
        } else if word(i) {
            i = run(i, &word);
            out.push((Tok::Word(span(s, i).to_ascii_lowercase(), false), d));
            continue;
        } else if at(i, "::") {
            i += 2;
        } else if OPS.contains(x) {
            i = run(i + 1, &|j| OPS.contains(c[j]) && !at(j, "--") && !at(j, "/*"));
        } else {
            i += 1;
            d -= (x == ')' || x == ']') as u32 * d.min(1);
            out.push((Tok::P(x), d));
            d += (x == '(' || x == '[') as u32;
            continue;
        }
        out.push((Tok::Lit(span(s, i)), d));
    }
    out
}

/// The canonical text of a token run: tokens joined by one space, quoted words re-quoted.
fn text(t: &[T]) -> String {
    let one = |t: &Tok| match t {
        Tok::Word(w, true) => format!("\"{}\"", w.replace('"', "\"\"")),
        Tok::Word(w, false) | Tok::Lit(w) => w.clone(),
        Tok::P(p) => p.to_string(),
    };
    t.iter().map(|(x, _)| one(x)).collect::<Vec<_>>().join(" ")
}

fn is(t: Option<&T>, w: &str, d: u32) -> bool { matches!(t, Some((Tok::Word(s, false), e)) if s == w && *e == d) }

/// `t` ends with the word run `w` (at the depth of its own tokens).
fn ends(t: &[T], w: &[&[&str]]) -> bool {
    t.len() >= w.len() && w.iter().enumerate().all(|(k, alts)| alts.iter().any(|a| matches!(&t[t.len() - w.len() + k].0, Tok::Word(s, false) if s == a)))
}

fn split(t: &[T]) -> Vec<&[T]> { t.split(|x| *x == (Tok::P(','), 0)).collect() }

/// The first depth-0 index at or after `from` holding one of `words` or `;`.
fn until(t: &[T], from: usize, words: &[&str]) -> usize {
    (from..t.len()).find(|&j| words.iter().any(|w| is(t.get(j), w, 0)) || t[j] == (Tok::P(';'), 0)).unwrap_or(t.len())
}

const VERBS: [&str; 7] = ["select", "values", "table", "insert", "update", "delete", "merge"];
const ITEM_END: [&str; 14] = ["from", "where", "group", "having", "window", "order", "limit", "offset", "fetch", "for", "union", "intersect", "except", "into"];

/// The statement's order class; `cols` = the result column names (RowDescription / the psql header).
pub fn classify(sql: &str, cols: &[String]) -> Class {
    classify_with(sql, Some(cols), Some(cols.len()))
}

/// `cols` absent (tuples-only output): rule (2) takes the names the first depth-0 SELECT's items give (`AS name`, a lone
/// word, `w . w`'s last word; none at all when an item is a star); `ncols` bounds an ordinal when known.
pub fn classify_with(sql: &str, cols: Option<&[String]>, ncols: Option<usize>) -> Class {
    let mut t = lex(sql);
    while t.last() == Some(&(Tok::P(';'), 0)) { t.pop(); }
    while t.len() > 1 && t[0] == (Tok::P('('), 0) && t.iter().skip(1).position(|x| *x == (Tok::P(')'), 0)) == Some(t.len() - 2) {
        t = t[1..t.len() - 1].iter().map(|(x, d)| (x.clone(), d - 1)).collect();
    }
    let verb = match t.first() {
        _ if sql.trim_start().starts_with('\\') => None,
        Some((Tok::P('('), _)) => Some("select"),
        Some((Tok::Word(w, false), _)) if w == "with" => t.iter().find_map(|x| VERBS.into_iter().find(|v| is(Some(x), v, 0))),
        Some((Tok::Word(w, false), _)) => Some(w.as_str()),
        _ => None,
    };
    match verb.unwrap_or("") {
        "select" | "values" | "table" => {}
        v => return if VERBS.contains(&v) { Class::Unordered } else { Class::Exact },
    }
    let n = t.len();
    let ties = |k: usize| (k..n).any(|j| is(t.get(j), "with", 0) && is(t.get(j + 1), "ties", 0));
    let cut = (0..n).any(|k| {
        let (nw, next) = (|w: &str| is(t.get(k + 1), w, 0), t.get(k + 1).map(|x| &x.0));
        (is(t.get(k), "limit", 0) && !(nw("all") || nw("null")))
            || (is(t.get(k), "offset", 0) && !(nw("null") || next == Some(&Tok::Lit("0".into()))))
            || (is(t.get(k), "fetch", 0) && !ties(k))
    });
    let Some(ob) = (0..n).rev().find(|&k| is(t.get(k), "order", 0) && is(t.get(k + 1), "by", 0)) else {
        return if cut { Class::Limited(Vec::new()) } else { Class::Unordered };
    };
    let items = select_items(&t);
    let names: Option<Vec<Option<String>>> = match cols {
        Some(c) => Some(c.iter().cloned().map(Some).collect()),
        None => (!items.iter().any(|x| x.0)).then(|| items.iter().map(|x| x.2.clone()).collect()),
    };
    let keys = split(&t[ob + 2..until(&t, ob + 2, &["limit", "offset", "fetch", "for"])]);
    match keys.into_iter().map(|i| resolve(strip_dir(i), &items, names.as_deref(), ncols)).collect::<Option<Vec<usize>>>() {
        None => Class::Exact,
        Some(k) if cut => Class::Limited(k),
        Some(k) => Class::Ordered(k),
    }
}

/// An ORDER BY item without its trailing ASC | DESC | USING op and NULLS FIRST | LAST.
fn strip_dir(mut it: &[T]) -> &[T] {
    if ends(it, &[&["nulls"], &["first", "last"]]) { it = &it[..it.len() - 2] }
    if ends(it, &[&["asc", "desc"]]) { it = &it[..it.len() - 1] } else if it.len() >= 2 && ends(&it[..it.len() - 1], &[&["using"]]) { it = &it[..it.len() - 2] }
    it
}

/// The first depth-0 SELECT's items: (a star, the canonical text less a trailing `AS word`, the name the item gives).
fn select_items(t: &[T]) -> Vec<(bool, String, Option<String>)> {
    item_slices(t).into_iter().map(|(e, name)| (matches!(e.last(), Some((Tok::Lit(o), _)) if o == "*") && (e.len() == 1 || e[e.len() - 2].0 == Tok::P('.')), text(e), name)).collect()
}

/// The first depth-0 SELECT's items less a trailing `AS word`, with the name each gives.
fn item_slices(t: &[T]) -> Vec<(&[T], Option<String>)> {
    let Some(mut b) = t.iter().position(|x| is(Some(x), "select", 0)).map(|b| b + 1) else { return Vec::new() };
    b += is(t.get(b), "all", 0) as usize + is(t.get(b), "distinct", 0) as usize;
    if is(t.get(b - 1), "distinct", 0) && is(t.get(b), "on", 0) {
        b += 3 + t.get(b + 2..).and_then(|r| r.iter().position(|x| *x == (Tok::P(')'), 0))).unwrap_or(t.len());
    }
    let word = |x: &T| if let Tok::Word(w, _) = &x.0 { Some(w.clone()) } else { None };
    split(&t[b.min(t.len())..until(t, b, &ITEM_END)]).into_iter().map(|i| {
        let aliased = i.len() >= 2 && matches!(i[i.len() - 1].0, Tok::Word(..)) && ends(&i[..i.len() - 1], &[&["as"]]);
        let e = if aliased { &i[..i.len() - 2] } else { i };
        let name = match e {
            _ if aliased => word(&i[i.len() - 1]),
            [w] | [(Tok::Word(..), _), (Tok::P('.'), _), w] => word(w),
            _ => None,
        };
        (e, name)
    }).collect()
}

/// (1) an ordinal, (2) one word naming exactly one result column (naming none: go on to 3), (3) the text of exactly one
/// select item with no star item before it.
fn resolve(it: &[T], items: &[(bool, String, Option<String>)], names: Option<&[Option<String>]>, ncols: Option<usize>) -> Option<usize> {
    let fits = |k: usize| ncols.is_none_or(|n| k < n);
    match (it, names) {
        ([(Tok::Lit(s), _)], _) if s.bytes().all(|b| b.is_ascii_digit()) => return s.parse::<usize>().ok().filter(|&k| k >= 1 && fits(k - 1)).map(|k| k - 1),
        ([(Tok::Word(w, _), _)], Some(ns)) if ns.contains(&Some(w.clone())) => return ns.iter().position(|c| c.as_ref() == Some(w)).filter(|_| ns.iter().filter(|c| c.as_ref() == Some(w)).count() == 1),
        _ => {}
    }
    let want = text(it);
    match (0..items.len()).filter(|&k| items[k].1 == want).collect::<Vec<_>>()[..] {
        [k] if !items[..k].iter().any(|x| x.0) && fits(k) => Some(k),
        _ => None,
    }
}

pub fn bag(r: &[Row]) -> Vec<&Row> { let mut v: Vec<&Row> = r.iter().collect(); v.sort_unstable(); v }

fn key(r: &Row, k: &[usize]) -> Row { k.iter().map(|&c| r.get(c).cloned().flatten()).collect() }

/// D4019 (multiset.py numeric_type): a declared result type name that is numeric (numeric, numeric(12,3), decimal). Named apart
/// from `numeric_type` below (multiset.py's _numeric_type, D3927), which reads a cast's type tokens.
pub fn numeric_decl(t: &str) -> bool { matches!(t.trim().to_ascii_lowercase().split('(').next().unwrap_or("").trim_end(), "numeric" | "decimal") }

/// D4019 (multiset.py num_value): a numeric's output text -> its value's canonical text (0.000 / 0.00000000000000000000 -> 0,
/// 1.50 -> 1.5, -0.0 -> 0); NaN, +/-Infinity and any text no numeric prints stay as they are.
pub fn num_value(v: &str) -> String {
    let (neg, b) = v.strip_prefix('-').map_or((false, v), |b| (true, b));
    let (i, f) = b.split_once('.').unwrap_or((b, ""));
    if i.is_empty() || !i.bytes().all(|c| c.is_ascii_digit()) || !f.bytes().all(|c| c.is_ascii_digit()) {
        return v.to_owned();
    }
    let (i, f) = (Some(i.trim_start_matches('0')).filter(|i| !i.is_empty()).unwrap_or("0"), f.trim_end_matches('0'));
    format!("{}{i}{}{f}", if neg && (i != "0" || !f.is_empty()) { "-" } else { "" }, if f.is_empty() { "" } else { "." })
}

/// D4194: a key type whose peers under its `=` can print apart. `Num` (D4019): numeric's display scale; `Float`: float4 / float8
/// -0 and 0; `Interval`: interval_cmp's span, a month as 30 days and a day as 24 hours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Peer { Num, Float, Interval }

/// D4194 (multiset.py peer_type): a declared result type name's `Peer` kind (numeric_decl, real / float4 / double precision /
/// float8 / float(n), interval with or without a typmod or fields); None for every other type, arrays included.
pub fn peer_decl(t: &str) -> Option<Peer> {
    let t = t.trim().to_ascii_lowercase();
    let base = t.split('(').next().unwrap_or("").trim_end();
    match base {
        _ if t.ends_with("[]") => None,
        _ if numeric_decl(&t) => Some(Peer::Num),
        "real" | "float4" | "double precision" | "float8" | "float" => Some(Peer::Float),
        _ if base == "interval" || base.starts_with("interval ") => Some(Peer::Interval),
        _ => None,
    }
}

/// D4194 (multiset.py interval_span): interval_cmp's span of an interval printed in the default 'postgres' IntervalStyle
/// ('1 year 2 mons -3 days +04:05:06.5'), in microseconds: (months x 30 + days) x 86400 s + the time. None for any text it
/// cannot read ('infinity', another IntervalStyle, more than 6 fraction digits), which then stays strict text.
pub fn interval_span(v: &str) -> Option<i128> {
    let dig = |x: &str| !x.is_empty() && x.bytes().all(|c| c.is_ascii_digit());
    let (mut months, mut days, mut us, mut any) = (0i128, 0i128, 0i128, false);
    let mut it = v.split_whitespace();
    while let Some(w) = it.next() {
        any = true;
        if w.contains(':') {
            let (neg, b) = w.strip_prefix('-').map_or((false, w.strip_prefix('+').unwrap_or(w)), |b| (true, b));
            let p: Vec<&str> = b.split(':').collect();
            let [h, m, s] = p[..] else { return None };
            let (si, sf) = s.split_once('.').unwrap_or((s, ""));
            if !dig(h) || !dig(m) || !dig(si) || !(sf.is_empty() || (sf.len() <= 6 && dig(sf))) {
                return None;
            }
            let frac: i128 = if sf.is_empty() { 0 } else { format!("{sf:0<6}").parse().ok()? };
            let t = (h.parse::<i128>().ok()? * 3600 + m.parse::<i128>().ok()? * 60 + si.parse::<i128>().ok()?) * 1_000_000 + frac;
            us += if neg { -t } else { t };
        } else {
            let n: i128 = w.parse().ok()?;
            match it.next()? {
                "year" | "years" => months += n * 12,
                "mon" | "mons" => months += n,
                "day" | "days" => days += n,
                _ => return None,
            }
        }
    }
    any.then(|| (months * 30 + days) * 86_400_000_000 + us)
}

/// D4194 (multiset.py peer_value): a key cell's text -> its peer class's text under `p`: numeric by value (D4019's num_value),
/// float -0 -> 0, interval -> its span; any text the kind cannot read stays as it is.
pub fn peer_value(p: Peer, v: &str) -> String {
    match p {
        Peer::Num => num_value(v),
        Peer::Float if v == "-0" => "0".to_owned(),
        Peer::Float => v.to_owned(),
        Peer::Interval => interval_span(v).map_or_else(|| v.to_owned(), |s| s.to_string()),
    }
}

/// D4019 + D4194: the key columns (0-based) of an ordered / limited class whose declared type (`types`) has a `Peer` kind.
pub fn peer_keys(class: &Class, types: Option<&[String]>) -> Vec<(usize, Peer)> {
    match (class, types) {
        (Class::Ordered(k) | Class::Limited(k), Some(t)) => k.iter().filter_map(|&c| t.get(c).and_then(|x| peer_decl(x)).map(|p| (c, p))).collect(),
        _ => Vec::new(),
    }
}

/// A row's key with its `peer` columns (D4019, D4194) by their peer class.
fn nkey(r: &Row, k: &[usize], peer: &[(usize, Peer)]) -> Row {
    k.iter().map(|&c| r.get(c).cloned().flatten().map(|v| match peer.iter().find(|x| x.0 == c) { Some(&(_, p)) => peer_value(p, &v), None => v })).collect()
}

/// The rows of `a` that `b` does not cover, as multisets (empty = `a` ⊆ `b`).
pub fn beyond<'a>(a: &[&'a Row], b: &[&Row]) -> Vec<&'a Row> {
    let mut n: BTreeMap<&Row, i64> = BTreeMap::new();
    b.iter().for_each(|r| *n.entry(r).or_default() += 1);
    a.iter().copied().filter(|r| !n.get_mut(r).is_some_and(|e| *e > 0 && { *e -= 1; true })).collect()
}

// ------------------------------------------------------------------------------------------------ V4R-2 float columns
const FLOAT_AGGS: [&str; 19] = ["sum", "avg", "variance", "var_pop", "var_samp", "stddev", "stddev_pop", "stddev_samp", "covar_pop", "covar_samp",
    "corr", "regr_avgx", "regr_avgy", "regr_intercept", "regr_r2", "regr_slope", "regr_sxx", "regr_sxy", "regr_syy"];

fn w(t: &[T], i: usize) -> &str { if let Some((Tok::Word(s, false), _)) = t.get(i) { s } else { "" } }

/// A type name at `t[i]`: (float4 | float8 | another type, the index after it).
fn float_type(t: &[T], i: usize) -> (Option<F>, usize) {
    match (w(t, i), t.get(i + 1).map(|x| &x.0), t.get(i + 2).map(|x| &x.0), t.get(i + 3).map(|x| &x.0)) {
        ("float4" | "real", ..) => (Some(F::F4), i + 1),
        ("float8", ..) => (Some(F::F8), i + 1),
        ("double", ..) if w(t, i + 1) == "precision" => (Some(F::F8), i + 2),
        ("float", Some(Tok::P('(')), Some(Tok::Lit(n)), Some(Tok::P(')'))) => (Some(if n.parse::<u32>().is_ok_and(|n| n <= 24) { F::F4 } else { F::F8 }), i + 4),
        ("float", ..) => (Some(F::F8), i + 1),
        _ => (None, i + 1 + (i + 1..t.len()).take_while(|&j| !w(t, j).is_empty() && w(t, j) != "as").count()),
    }
}

fn strip_outer(mut it: &[T]) -> &[T] {
    while it.len() > 1 && it[0].0 == Tok::P('(') && it[1..].iter().position(|x| *x == (Tok::P(')'), it[0].1)) == Some(it.len() - 2) { it = &it[1..it.len() - 1] }
    it
}

/// float4 / float8 when select item `it` is an order-dependent float aggregate whose float type is evident: its outer cast,
/// the declared result type, or a float cast inside its arguments (sum of real is float4; the rest of real are float8).
fn agg_kind(mut it: &[T], declared: Option<&str>) -> Option<F> {
    let (mut outer, cc) = (None, Tok::Lit("::".into()));
    loop {
        it = strip_outer(it);
        let d = it.first()?.1;
        let (at, end) = match it.iter().rposition(|x| x.0 == cc && x.1 == d) {
            Some(c) => (c, it.len()),
            None if it.len() > 4 && is(it.first(), "cast", d) && it[1].0 == Tok::P('(') && it[it.len() - 1].0 == Tok::P(')') => ((2..it.len()).find(|&i| is(it.get(i), "as", d + 1))?, it.len() - 1),
            None => break,
        };
        let (k, e) = float_type(it, at + 1);
        if e != end || k.is_none() { return None }
        (outer, it) = (outer.or(k), if end == it.len() { &it[..at] } else { &it[2..at] });
    }
    let (d, f) = (it.first()?.1, w(it, 0));
    if !FLOAT_AGGS.contains(&f) || it.get(1).map(|x| &x.0) != Some(&Tok::P('(')) { return None }
    let close = (2..it.len()).find(|&i| it[i] == (Tok::P(')'), d))?;
    let rest = &it[close + 1..];
    let alias = rest.len() == 1 && matches!(&rest[0].0, Tok::Word(x, _) if x != "over" && x != "filter");
    if !rest.is_empty() && !alias && !(is(rest.first(), "filter", d) && rest[rest.len() - 1].0 == Tok::P(')') && !rest.iter().any(|x| is(Some(x), "over", d))) { return None }
    if let Some(t) = declared { return float_type(&lex(t), 0).0 }
    if outer.is_some() { return outer }
    let (args, mut inner) = (&it[2..close], None);
    // DAY-ORDER addendum 9 (USER 2026-10-05 ~18:52Z; multiset.py FLOAT8_PAIR_AGGS): a two-argument covar_* / corr / regr_*
    // casts both arguments to float8 per row and accumulates float8, so it is float8 whatever its argument types (regr_count
    // is a count, no FLOAT_AGG; OVER returned exact above)
    let pair = args.split(|x| *x == (Tok::P(','), d + 1)).collect::<Vec<_>>();
    if (f.starts_with("covar_") || f.starts_with("regr_") || f == "corr") && pair.len() == 2 && pair.iter().all(|c| !c.is_empty()) { return Some(F::F8) }
    for i in 0..args.len() {
        if args[i].0 == cc || w(args, i) == "as" { inner = float_type(args, i + 1).0.or(inner) }
        if args.get(i + 1).map(|x| &x.0) == Some(&Tok::P('(')) { inner = match w(args, i) { "float8" => Some(F::F8), "float4" => Some(F::F4), _ => inner } }
        if inner == Some(F::F8) { break }
    }
    Some(if inner? == F::F4 && f == "sum" { F::F4 } else { F::F8 })
}

/// `numeric` / `decimal`, with or without a (precision [, scale]) typmod (multiset.py's _numeric_type).
fn numeric_type(t: &[T]) -> bool {
    let lit = |k: usize| matches!(t.get(k), Some((Tok::Lit(n), _)) if n.bytes().all(|b| b.is_ascii_digit()));
    matches!(w(t, 0), "numeric" | "decimal") && match t.len() {
        1 => true,
        4 => t[1].0 == Tok::P('(') && lit(2) && t[3].0 == Tok::P(')'),
        6 => t[1].0 == Tok::P('(') && lit(2) && t[3].0 == Tok::P(',') && lit(4) && t[5].0 == Tok::P(')'),
        _ => false,
    }
}

/// D3927 (multiset.py's round_kind): `R(n)` when select item `it` is round(<float aggregate>::numeric [, n]) (n an integer
/// literal, 0 when absent, negative allowed; the cast `::numeric[(p[, s])]` or CAST(.. AS numeric[(p[, s])]); a bare alias
/// after it) and agg_kind takes the aggregate from the text (a declared result type is the round's numeric, not used); else
/// None. multiset.py's schema evidence (D432's float column, D3927's arithmetic over one) has no mirror here: no schema.
fn round_kind(it: &[T]) -> Option<F> {
    let mut it = strip_outer(it);
    if it.len() >= 2 && matches!(it[it.len() - 1].0, Tok::Word(..)) && it[it.len() - 2].0 == Tok::P(')') { it = &it[..it.len() - 1] }
    let d = it.first()?.1;
    if it.len() < 4 || !is(it.first(), "round", d) || it[1] != (Tok::P('('), d) || it[it.len() - 1] != (Tok::P(')'), d) { return None }
    let args = &it[2..it.len() - 1];
    if args.iter().any(|x| *x == (Tok::P(')'), d)) { return None }   // round(..) is not the whole item
    let parts = args.split(|x| *x == (Tok::P(','), d + 1)).collect::<Vec<_>>();
    if parts.len() > 2 || parts[0].is_empty() { return None }
    let n = match parts.get(1) {
        None => 0,
        Some(p) => {
            let (neg, p) = match p.first() { Some((Tok::Lit(o), _)) if o == "-" => (true, &p[1..]), _ => (false, *p) };
            let [(Tok::Lit(v), _)] = p else { return None };
            if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) { return None }
            let v = v.parse::<i32>().ok()?;
            if neg { -v } else { v }
        }
    };
    let (x, cc) = (strip_outer(parts[0]), Tok::Lit("::".into()));
    let xd = x.first()?.1;
    let inner = match x.iter().rposition(|t| t.0 == cc && t.1 == xd) {
        Some(c) if numeric_type(&x[c + 1..]) => &x[..c],
        Some(_) => return None,
        None if x.len() > 4 && is(x.first(), "cast", xd) && x[1].0 == Tok::P('(') && x[x.len() - 1].0 == Tok::P(')') => {
            let a = (2..x.len()).find(|&i| is(x.get(i), "as", xd + 1))?;
            if !numeric_type(&x[a + 1..x.len() - 1]) { return None }
            &x[2..a]
        }
        None => return None,
    };
    agg_kind(inner, None).map(|_| F::R(n))
}

/// D3927: |a - b| <= 10^-n for two decimal texts, exactly (multiset.py compares them as Decimals). A text that is not a plain
/// decimal (NaN, Infinity, an exponent past 4000) compares exactly.
pub fn dec_within(a: &str, b: &str, n: i32) -> bool {
    // (negative, digits most significant first, exponent): the value is digits x 10^exponent
    fn parse(s: &str) -> Option<(bool, Vec<u8>, i64)> {
        let s = s.trim();
        let (neg, s) = match s.as_bytes().first()? { b'-' => (true, &s[1..]), b'+' => (false, &s[1..]), _ => (false, s) };
        let (m, e) = match s.find(['e', 'E']) { Some(i) => (&s[..i], s[i + 1..].parse::<i64>().ok()?), None => (s, 0) };
        let (ip, fp) = m.split_once('.').unwrap_or((m, ""));
        if (ip.is_empty() && fp.is_empty()) || !ip.bytes().chain(fp.bytes()).all(|c| c.is_ascii_digit()) || e.abs() > 4000 { return None }
        Some((neg, ip.bytes().chain(fp.bytes()).map(|c| c - b'0').collect(), e - fp.len() as i64))
    }
    fn trim(v: &[u8]) -> &[u8] { &v[v.iter().position(|&x| x != 0).unwrap_or(v.len())..] }
    fn cmp(a: &[u8], b: &[u8]) -> Ordering { let (a, b) = (trim(a), trim(b)); a.len().cmp(&b.len()).then_with(|| a.cmp(b)) }
    // a + b, or a - b with a >= b
    fn op(a: &[u8], b: &[u8], add: bool) -> Vec<u8> {
        let (mut out, mut carry, k) = (Vec::new(), 0i32, a.len().max(b.len()));
        for i in 0..k {
            let x = if i < a.len() { a[a.len() - 1 - i] as i32 } else { 0 };
            let y = if i < b.len() { b[b.len() - 1 - i] as i32 } else { 0 };
            let mut v = if add { x + y + carry } else { x - y + carry };
            carry = if add { v / 10 } else if v < 0 { v += 10; -1 } else { 0 };
            out.push((v % 10) as u8);
        }
        if carry > 0 { out.push(carry as u8) }
        out.reverse();
        out
    }
    let (Some((na, mut da, ea)), Some((nb, mut db, eb))) = (parse(a), parse(b)) else { return a == b };
    let e = ea.min(eb).min(-(n as i64));
    da.resize(da.len() + (ea - e) as usize, 0);
    db.resize(db.len() + (eb - e) as usize, 0);
    let diff = if na != nb { op(&da, &db, true) } else if cmp(&da, &db).is_ge() { op(&da, &db, false) } else { op(&db, &da, false) };
    let mut bound = vec![1u8];
    bound.resize(1 + (-(n as i64) - e) as usize, 0);
    cmp(&diff, &bound).is_le()
}

/// The float-tolerant columns multiset.py's float_cols() reads off the SQL (`types`: declared result type names, if known).
pub fn float_cols(sql: &str, ncols: Option<usize>, types: Option<&[String]>) -> Tol {
    let t = lex(sql);
    let t = strip_outer(&t[..t.len() - t.last().is_some_and(|x| *x == (Tok::P(';'), 0)) as usize]);
    if !(t.first().is_some_and(|x| x.0 == Tok::P('(')) || ["select", "with"].contains(&w(t, 0))) { return Tol::new() }
    let items = item_slices(t);
    let star = |e: &[T]| matches!(e.last(), Some((Tok::Lit(o), _)) if o == "*");
    let n = items.iter().position(|(e, _)| star(e)).unwrap_or(items.len()).min(ncols.unwrap_or(usize::MAX));
    (0..n).filter_map(|k| agg_kind(items[k].0, types.and_then(|t| t.get(k)).map(String::as_str)).or_else(|| round_kind(items[k].0)).map(|f| (k, f))).collect()
}

fn tol_eq(a: &Option<String>, b: &Option<String>, f: F) -> bool {
    if let (F::R(n), Some(x), Some(y)) = (f, a, b) { return x == y || dec_within(x, y, n) }
    let (Some(x), Some(y)) = (a.as_deref().map(|s| s.trim().parse::<f64>()), b.as_deref().map(|s| s.trim().parse::<f64>())) else { return a == b };
    let (Ok(x), Ok(y)) = (x, y) else { return a == b };
    let ord = |v: f64| { let b = (v as f32).to_bits() as i32; if b >= 0 { b as i64 } else { -((b & 0x7fff_ffff) as i64) } };
    a == b || (x.is_finite() && y.is_finite() && (x == y || match f {
        F::F4 => (x as f32).is_finite() && (y as f32).is_finite() && (ord(x) - ord(y)).abs() <= 4,
        F::F8 => (x - y).abs() <= 1e-12 * x.abs().max(y.abs()),
        F::R(_) => false,
    }))
}

fn row_eq(a: &Row, b: &Row, tol: &Tol) -> bool {
    a.len() == b.len() && a.iter().zip(b).enumerate().all(|(k, (x, y))| tol.get(&k).map_or(x == y, |f| tol_eq(x, y, *f)))
}

/// The row with its tolerant cells reduced to NULL / not NULL: rows that may pair up share it.
fn mask(r: &Row, tol: &Tol) -> Row { r.iter().enumerate().map(|(k, v)| if tol.contains_key(&k) { Some(["v", "n"][v.is_none() as usize].into()) } else { v.clone() }).collect() }

/// The float order tolerant rows pair up in: numbers, then NaN, then any other text (multiset.py's _fkey).
fn fk(v: Option<&Option<String>>) -> (u8, f64, &str) {
    match v.and_then(|v| v.as_deref()).map(|s| (s, s.trim().parse::<f64>())) {
        Some((_, Ok(x))) if !x.is_nan() => (0, x, ""),
        Some((_, Ok(_))) => (1, 0.0, ""),
        o => (2, 0.0, o.map_or("", |x| x.0)),
    }
}

fn fcmp(a: &Row, b: &Row, tol: &Tol) -> Ordering {
    tol.keys().map(|c| { let (x, y) = (fk(a.get(*c)), fk(b.get(*c))); x.0.cmp(&y.0).then(x.1.total_cmp(&y.1)).then(x.2.cmp(y.2)) }).find(|o| o.is_ne()).unwrap_or(Ordering::Equal)
}

fn groups<'a>(r: &[&'a Row], tol: &Tol) -> BTreeMap<Row, Vec<&'a Row>> {
    let mut g: BTreeMap<Row, Vec<&Row>> = BTreeMap::new();
    r.iter().for_each(|x| g.entry(mask(x, tol)).or_default().push(x));
    g.values_mut().for_each(|v| v.sort_by(|a, b| fcmp(a, b, tol)));
    g
}

/// Multiset equality; tolerant rows pair up in float order within rows equal on the exact columns.
fn bag_eq(a: &[&Row], b: &[&Row], tol: &Tol) -> bool {
    let (ga, gb) = (groups(a, tol), groups(b, tol));
    ga.len() == gb.len() && ga.iter().zip(&gb).all(|((m, x), (n, y))| m == n && x.len() == y.len() && x.iter().zip(y).all(|(p, q)| row_eq(p, q, tol)))
}

/// Every row of `sub` pairs with its own row of `sup`.
fn bag_within(sub: &[&Row], sup: &[&Row], tol: &Tol) -> bool {
    let mut pool = groups(sup, tol);
    let mut s = sub.to_vec();
    s.sort_by(|a, b| fcmp(a, b, tol));
    s.iter().all(|r| pool.get_mut(&mask(r, tol)).and_then(|c| c.iter().position(|x| row_eq(r, x, tol)).map(|i| c.remove(i))).is_some())
}

/// The tolerance over a key's positions.
fn kt(k: &[usize], tol: &Tol) -> Tol { k.iter().enumerate().filter_map(|(i, c)| tol.get(c).map(|f| (i, *f))).collect() }

/// Maximal runs of consecutive rows whose keys (the `peer` columns by peer class, D4019 / D4194) equal the run's first key.
fn runs<'a>(r: &[&'a Row], k: &[usize], tol: &Tol, num: &[(usize, Peer)]) -> Vec<(Row, Vec<&'a Row>)> {
    let mut out: Vec<(Row, Vec<&Row>)> = Vec::new();
    for x in r {
        let kx = nkey(x, k, num);
        match out.last_mut() {
            Some((f, v)) if row_eq(f, &kx, &kt(k, tol)) => v.push(x),
            _ => out.push((kx, vec![x])),
        }
    }
    out
}

fn class_ok(class: &Class, p: &[&Row], full: Option<&[&Row]>, g: &[&Row], tol: &Tol, num: &[(usize, Peer)]) -> bool {
    match (class, full) {
        (Class::Exact, _) => p.len() == g.len() && p.iter().zip(g).all(|(a, b)| row_eq(a, b, tol)),
        (Class::Unordered, _) => bag_eq(p, g, tol),
        (Class::Limited(_), _) if p.len() != g.len() => false,
        (Class::Limited(k), None) if k.is_empty() => bag_eq(p, g, tol),
        (Class::Ordered(k), _) | (Class::Limited(k), None) => {
            let (a, b) = (runs(p, k, tol, num), runs(g, k, tol, num));
            a.len() == b.len() && a.iter().zip(&b).all(|(x, y)| row_eq(&x.0, &y.0, &kt(k, tol)) && bag_eq(&x.1, &y.1, tol))
        }
        (Class::Limited(k), Some(f)) if k.is_empty() => bag_within(g, f, tol),
        (Class::Limited(k), Some(f)) => {
            p.iter().zip(g).all(|(a, b)| row_eq(&nkey(a, k, num), &nkey(b, k, num), &kt(k, tol)))
                && runs(g, k, tol, num).iter().all(|(kk, rows)| bag_within(rows, &f.iter().copied().filter(|r| row_eq(&nkey(r, k, num), kk, &kt(k, tol))).collect::<Vec<_>>(), tol))
        }
    }
}

/// PG's result (`pg`, and `pg_full` = PG's result without the top-level LIMIT / OFFSET / FETCH, when known) against v4's;
/// `tol`: the float-tolerant columns (V4R-2), every other column exact.
pub fn compare(class: &Class, pg: &Outcome, pg_full: Option<&[Row]>, got: &Outcome, tol: &Tol) -> Verdict {
    compare_num(class, pg, pg_full, got, tol, &[])
}

/// `compare` with the peer key columns (`peer_keys`, D4019 / D4194): their tie runs and key sequence go by peer class.
fn compare_num(class: &Class, pg: &Outcome, pg_full: Option<&[Row]>, got: &Outcome, tol: &Tol, num: &[(usize, Peer)]) -> Verdict {
    fn refs(r: &[Row]) -> Vec<&Row> { r.iter().collect() }
    match (pg, got) {
        (Outcome::Rows(p), Outcome::Rows(g)) if class_ok(class, &refs(p), pg_full.map(refs).as_deref(), &refs(g), tol, num) => Verdict::Match,
        (Outcome::Rows(p), Outcome::Rows(g)) if bag_eq(&refs(p), &refs(g), tol) => Verdict::MismatchOrder,
        _ if pg == got => Verdict::Match,
        _ => Verdict::Mismatch,
    }
}

fn canon_rows(r: &[Row], e: &Eqk) -> Vec<Row> {
    r.iter().map(|x| x.iter().enumerate().map(|(k, v)| v.as_ref().map(|v| e.get(&k).and_then(|m| m.get(v)).unwrap_or(v).clone())).collect()).collect()
}

/// `compare` with V4R-270's key classes: each `eqk` column's cells map to their class on every side first (none: `compare`).
pub fn compare_by(class: &Class, pg: &Outcome, pg_full: Option<&[Row]>, got: &Outcome, tol: &Tol, eqk: &Eqk) -> Verdict {
    compare_typed(class, pg, pg_full, got, tol, eqk, None)
}

/// `compare_by` with the declared result types (`types`, as multiset.py's verdict() takes them): ORDER BY keys of a `Peer`
/// type (D4019 numeric, D4194 float / interval) group their runs by peer class (none: `compare_by`).
pub fn compare_typed(class: &Class, pg: &Outcome, pg_full: Option<&[Row]>, got: &Outcome, tol: &Tol, eqk: &Eqk, types: Option<&[String]>) -> Verdict {
    let canon = |o: &Outcome| match o { Outcome::Rows(r) => Outcome::Rows(canon_rows(r, eqk)), x => x.clone() };
    compare_num(class, &canon(pg), pg_full.map(|f| canon_rows(f, eqk)).as_deref(), &canon(got), tol, &peer_keys(class, types))
}

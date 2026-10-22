// The effects home (v5 interfaces §2.2): a call acts when its pg_proc row says VOLATILE or the catalog cannot answer it
// (fail closed); STABLE and IMMUTABLE calls never act (DECISIONS.md 3 (C1), ruled 2026-10-01 17:32Z), whoever wrote them;
// the licence follows. regress-full aggregates-015 is `SELECT newcnt(*) FROM onek`, newcnt a user aggregate (oid 20506).
use std::fs;

use gravity_decompose::effects::{self, Cx, Departure, Effects, FuncProps, Parallel, Volatility, PLAIN};
use gravity_decompose::catalog::AggSupport;
use gravity_decompose::{corpus, Facts};
use gravity_program::Arena;
use mcx::MemoryContext;
use types_nodes::Node;

mod harness;

const NEWCNT: u32 = 20506;
/// int8inc_any, newcnt's transition function in the regression suite (IMMUTABLE).
const INT8INC_ANY: u32 = 2804;
/// A user function standing for a transition or final function.
const USER_FN: u32 = 99_001;

/// The fixture's facts, but for newcnt's pg_proc row (`user`; none: the catalog cannot answer it), its pg_aggregate row
/// (`support`) and a user function `USER_FN` (`user_fn`).
struct One<'f> {
    fx: &'f corpus::FixtureFacts,
    user: Option<Volatility>,
    support: Option<AggSupport>,
    user_fn: Volatility,
}

impl Facts for One<'_> {
    fn rel(&self, relid: u32) -> Option<gravity_decompose::RelFact> {
        self.fx.rel(relid)
    }
    fn att(&self, relid: u32, attno: i16) -> Option<gravity_decompose::AttFact> {
        self.fx.att(relid, attno)
    }
    fn op_image_eq(&self, opno: u32) -> Option<(u32, u32)> {
        self.fx.op_image_eq(opno)
    }
    fn index(&self, indexid: u32) -> Option<gravity_decompose::IndexFact> {
        self.fx.index(indexid)
    }
    fn indexes_of(&self, relid: u32, f: &mut dyn FnMut(gravity_decompose::IndexFact)) {
        self.fx.indexes_of(relid, f)
    }
    fn index_expr(&self, indexid: u32, col: u8) -> Option<gravity_decompose::ExprFp> {
        self.fx.index_expr(indexid, col)
    }
    fn func(&self, funcid: u32) -> Option<FuncProps> {
        match funcid {
            NEWCNT => self.user.map(|volatility| FuncProps { volatility, strict: false, leakproof: false, parallel: Parallel::Unsafe }),
            USER_FN => Some(FuncProps { volatility: self.user_fn, strict: false, leakproof: false, parallel: Parallel::Unsafe }),
            f => self.fx.func(f),
        }
    }
    fn agg_support(&self, aggfnoid: u32) -> Option<AggSupport> {
        self.support.filter(|_| aggfnoid == NEWCNT)
    }
}

/// newcnt as the regression suite creates it: CREATE AGGREGATE stores its own row IMMUTABLE.
const AS_CREATED: Option<Volatility> = Some(Volatility::Immutable);
const PLAIN_SUPPORT: AggSupport = AggSupport { trans: INT8INC_ANY, final_: 0, combine: 0, serial: 0, deserial: 0, mtrans: 0, minv: 0, mfinal: 0 };

/// The first node under the plan's target lists that `pick` takes.
fn find<'p>(plan: Node<'p>, pick: &dyn Fn(Node<'p>) -> bool) -> Option<Node<'p>> {
    let p = plan.as_plan()?;
    let mut hit = None;
    for t in p.targetlist.iter() {
        gravity_decompose::expr::walk(t, &mut |n| {
            if hit.is_none() && pick(n) {
                hit = Some(n);
            }
            hit.is_none()
        });
    }
    hit.or_else(|| p.lefttree.and_then(|l| find(l, pick)))
}

/// The effects of aggregates-015's newcnt(*) call and of its whole statement, under `user`.
fn newcnt(user: Option<Volatility>) -> (Effects, Effects) {
    with_support(user, Some(PLAIN_SUPPORT), Volatility::Immutable)
}

fn with_support(user: Option<Volatility>, support: Option<AggSupport>, user_fn: Volatility) -> (Effects, Effects) {
    let text = fs::read_to_string(harness::plans_root().join("regress-full/aggregates-015.plan")).unwrap();
    let ctx = MemoryContext::new("fixture");
    let fx = corpus::load(ctx.mcx(), &text).unwrap();
    let arena = Arena::new(1 << 20);
    let facts = One { fx: &fx.facts, user, support, user_fn };
    let cx = Cx::new(fx.stmt, &facts, arena.handle());
    let call = find(fx.stmt.planTree.unwrap(), &|n| n.as_aggref().is_some_and(|a| a.aggfnoid == NEWCNT)).expect("the fixture calls newcnt");
    (effects::of_expr(&cx, call, &PLAIN), effects::of_statement(&cx))
}

#[test]
fn a_catalog_miss_acts() {
    let (call, stmt) = newcnt(None);
    assert_eq!(call, Effects::UNKNOWN, "a function the catalog cannot answer: fail closed");
    assert!(stmt.acts && !stmt.same_rows);
    for d in [Departure::Ahead, Departure::Fewer, Departure::More, Departure::Skip] {
        assert!(effects::licence(call, d).is_err(), "{d:?}: unknown code gets no licence");
    }
}

#[test]
fn stable_and_immutable_calls_do_not_act() {
    for v in [Volatility::Immutable, Volatility::Stable] {
        let (call, stmt) = newcnt(Some(v));
        assert!(!call.acts && call.same_rows, "{v:?}: a user call that is not VOLATILE does not act (C1)");
        assert!(call.may_raise, "{v:?}: a user aggregate is not on the quiet list");
        assert!(!stmt.acts, "{v:?}: a SELECT calling nothing VOLATILE does not act");
        assert!(effects::licence(call, Departure::Ahead).is_ok_and(|l| l.recheck()));
        assert!(effects::licence(call, Departure::Fewer).is_ok_and(|l| !l.recheck()));
        assert!(effects::licence(call, Departure::Skip).is_err(), "a skip would lose its error");
    }
}

#[test]
fn a_volatile_call_acts() {
    let (call, stmt) = newcnt(Some(Volatility::Volatile));
    assert!(call.acts && !call.same_rows);
    assert!(stmt.acts);
    assert!(effects::licence(call, Departure::Fewer).is_err());
}

#[test]
fn a_user_aggregate_acts_as_its_support_functions() {
    // the D2268 / D2310 class: the aggregate's own row says IMMUTABLE, its transition or final function is VOLATILE
    let trans = AggSupport { trans: USER_FN, ..PLAIN_SUPPORT };
    let fin = AggSupport { final_: USER_FN, ..PLAIN_SUPPORT };
    for s in [trans, fin] {
        let (call, stmt) = with_support(AS_CREATED, Some(s), Volatility::Volatile);
        assert!(call.acts && stmt.acts, "{s:?}: a VOLATILE support function acts");
        let (call, _) = with_support(AS_CREATED, Some(s), Volatility::Stable);
        assert!(!call.acts, "{s:?}: a STABLE support function does not act (C1)");
    }
    let (call, _) = with_support(AS_CREATED, Some(PLAIN_SUPPORT), Volatility::Volatile);
    assert!(!call.acts, "builtin IMMUTABLE support functions: newcnt does not act");
}

#[test]
fn a_user_aggregate_without_its_pg_aggregate_row_acts() {
    let (call, stmt) = with_support(AS_CREATED, None, Volatility::Immutable);
    assert!(call.acts && stmt.acts, "no pg_aggregate row: unknown, fail closed");
}

/// OID-24: PostgreSQL 18.6's builtins that are not VOLATILE and run a query through SPI over catalogs alone (the catalog
/// each one queries): no user relation's rows, so nothing a user wrote runs.
const CATALOG_ONLY: &[(u32, &str)] = &[
    (1573, "pg_get_ruledef(oid): pg_rewrite"),
    (2504, "pg_get_ruledef(oid, bool): pg_rewrite"),
    (1640, "pg_get_viewdef(text): pg_rewrite"),
    (1641, "pg_get_viewdef(oid): pg_rewrite"),
    (2505, "pg_get_viewdef(text, bool): pg_rewrite"),
    (2506, "pg_get_viewdef(oid, bool): pg_rewrite"),
    (3159, "pg_get_viewdef(oid, int4): pg_rewrite"),
    (2934, "schema_to_xmlschema: pg_class (the schema's tables; each one's columns from its own descriptor)"),
    (2937, "database_to_xmlschema: pg_namespace and pg_class (likewise)"),
];

fn reference() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset: run under cargo test"))
        .join("../../postgres-18.6-reference/src")
}

/// pg_proc.dat's rows, each a key -> value map (quoted values with backslash escapes, an array value's braces inside its
/// quotes, `#` comment lines skipped).
fn proc_rows() -> Vec<std::collections::BTreeMap<String, String>> {
    let text = fs::read_to_string(reference().join("include/catalog/pg_proc.dat")).expect("the reference pg_proc.dat");
    let (mut rows, mut cur, mut key) = (Vec::new(), None::<std::collections::BTreeMap<String, String>>, String::new());
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        let Some(row) = cur.as_mut() else {
            match c {
                '#' => while it.next_if(|&c| c != '\n').is_some() {},
                '{' => (cur, key) = (Some(Default::default()), String::new()),
                _ => {}
            }
            continue;
        };
        match c {
            '\'' => {
                let mut v = String::new();
                while let Some(c) = it.next() {
                    match c {
                        '\\' => v.extend(it.next()),
                        '\'' => break,
                        c => v.push(c),
                    }
                }
                row.insert(std::mem::take(&mut key), v);
            }
            '}' => rows.extend(cur.take()),
            ',' => key.clear(),
            c if c.is_ascii_alphanumeric() || c == '_' => key.push(c),
            _ => {}
        }
    }
    rows
}

/// Text `t` names the SPI interface (an `SPI_` function or variable: SPI_connect, SPI_execute, SPI_tuptable).
fn names_spi(t: &str) -> bool {
    t.match_indices("SPI_").any(|(at, _)| {
        t[..at].chars().next_back().is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_')) && t[at + 4..].starts_with(|c: char| c.is_ascii_lowercase())
    })
}

/// The identifiers C text `t` calls (an identifier, then blanks, then `(`).
fn called(t: &str) -> std::collections::BTreeSet<&str> {
    let (b, mut out, mut i) = (t.as_bytes(), std::collections::BTreeSet::new(), 0);
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    while i < b.len() {
        if !word(b[i]) {
            i += 1;
            continue;
        }
        let s = i;
        while i < b.len() && word(b[i]) {
            i += 1;
        }
        let j = (i..b.len()).find(|&j| b[j] != b' ' && b[j] != b'\t').unwrap_or(b.len());
        if j < b.len() && b[j] == b'(' && !b[s].is_ascii_digit() {
            out.insert(&t[s..i]);
        }
    }
    out
}

/// The C functions of the reference backend that run a query through SPI: a definition (PostgreSQL's layout: its name at
/// the start of a line, its body from a line `{` to a line `}`) whose body names SPI, or calls a function of its own file
/// that does; by name, with the file.
fn spi_users() -> std::collections::BTreeMap<String, String> {
    let (root, mut out, mut dirs) = (reference(), std::collections::BTreeMap::new(), vec![reference().join("backend")]);
    while let Some(d) = dirs.pop() {
        for e in fs::read_dir(&d).unwrap_or_else(|e| panic!("{}: {e}", d.display())) {
            let p = e.unwrap().path();
            if p.is_dir() {
                dirs.push(p);
                continue;
            }
            if p.extension().is_none_or(|x| x != "c") {
                continue;
            }
            let text = String::from_utf8_lossy(&fs::read(&p).unwrap()).into_owned();
            if !names_spi(&text) {
                continue;
            }
            let lines: Vec<&str> = text.lines().collect();
            let mut defs: Vec<(String, String)> = Vec::new();
            let mut i = 0;
            while i < lines.len() {
                let name: String = lines[i].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                let open = (!name.is_empty() && lines[i][name.len()..].starts_with('('))
                    .then(|| (i..lines.len()).find(|&j| lines[j] == "{" || lines[j].trim_end().ends_with(';')))
                    .flatten()
                    .filter(|&j| lines[j] == "{");
                match open {
                    Some(open) => {
                        let close = (open..lines.len()).find(|&j| lines[j] == "}").unwrap_or(lines.len());
                        defs.push((name, lines[open..close].join("\n")));
                        i = close;
                    }
                    None => i += 1,
                }
            }
            let calls: Vec<std::collections::BTreeSet<&str>> = defs.iter().map(|(_, b)| called(b)).collect();
            let mut on: std::collections::BTreeSet<&str> = defs.iter().filter(|(_, b)| names_spi(b)).map(|(n, _)| n.as_str()).collect();
            loop {
                let more: Vec<&str> = defs.iter().zip(&calls).filter(|((n, _), c)| !on.contains(n.as_str()) && c.iter().any(|x| on.contains(x))).map(|((n, _), _)| n.as_str()).collect();
                if more.is_empty() {
                    break;
                }
                on.extend(more);
            }
            let file = p.strip_prefix(&root).unwrap_or(&p).display().to_string();
            out.extend(on.into_iter().map(|n| (n.to_owned(), file.clone())));
        }
    }
    out
}

/// OID-24: the builtins that run a query of their own, from PostgreSQL 18.6's pg_proc.dat and the reference backend
/// sources. A builtin whose C function (its prosrc) runs a query through SPI is VOLATILE (its volatility already makes
/// the statement loud), answers `effects::runs_own_query` (STABLE, it reads the rows of the relations its arguments
/// name), or is on `CATALOG_ONLY`; `runs_own_query` answers no other oid, and no SQL-language builtin calls one (it
/// would run that query too). A PostgreSQL upgrade that adds such a builtin, or a list gone stale, fails here.
#[test]
fn the_builtins_that_run_a_query_of_their_own_are_known() {
    use std::collections::{BTreeMap, BTreeSet};
    let spi = spi_users();
    assert!(spi.contains_key("query_to_xml") && spi.contains_key("ts_stat1"), "the SPI scan finds PostgreSQL's own: {spi:?}");
    let rows = proc_rows();
    let catalog_only: BTreeMap<u32, &str> = CATALOG_ONLY.iter().copied().collect();
    let (mut bad, mut runs, mut oids, mut sql) = (Vec::new(), BTreeSet::new(), BTreeSet::new(), String::new());
    for r in &rows {
        let oid: u32 = r["oid"].parse().unwrap();
        oids.insert(oid);
        let lang = r.get("prolang").map_or("internal", String::as_str);
        let src = r.get("prosrc").unwrap_or(&r["proname"]);
        if lang == "sql" {
            sql.push_str(src);
            sql.push('\n');
        }
        let volatile = r.get("provolatile").is_some_and(|v| v == "v");
        let reads = matches!(lang, "internal" | "c") && spi.contains_key(src.as_str()) && !volatile;
        let (says, listed) = (effects::runs_own_query(oid), catalog_only.contains_key(&oid));
        if says {
            runs.insert(r["proname"].as_str());
        }
        if reads != (says || listed) || (says && listed) {
            let at = spi.get(src.as_str()).map_or("no SPI".to_owned(), |f| format!("SPI in {f}"));
            bad.push(format!("{oid} {} (prosrc {src}, {at}, provolatile {}): runs_own_query {says}, CATALOG_ONLY {listed}", r["proname"], r.get("provolatile").map_or("i (default)", String::as_str)));
        }
    }
    assert!(rows.len() > 3000 && !runs.is_empty(), "pg_proc.dat rows {}, runs_own_query names {runs:?}", rows.len());
    bad.extend((0..100_000).filter(|o| !oids.contains(o) && effects::runs_own_query(*o)).map(|o| format!("{o}: no pg_proc.dat row, runs_own_query true")));
    sql.push_str(&fs::read_to_string(reference().join("backend/catalog/system_functions.sql")).expect("the reference system_functions.sql"));
    bad.extend(called(&sql).intersection(&runs).map(|n| format!("a SQL-language builtin calls {n}")));
    assert!(bad.is_empty(), "builtins whose own query the effects answers misread:\n{}", bad.join("\n"));
}

/// C1 (one effects answer): every row of the one builtin fact table (`types_core::quiet::TABLES` and `DISPATCH`) is a
/// PostgreSQL 18.6 builtin with the proname the row states, and the place the row cites holds what it claims: the source
/// file exists under the reference src/, the named function is defined there (a C function at column 0, PostgreSQL's
/// style; a macro-made one: the macro's use; for a .sql file its name appears), a NOTICE row's function body raises a
/// NOTICE or WARNING itself, a HASH_NEEDS_COLLATION row's raises the missing-collation error itself, and a
/// COLLATION_FREE row's reads no collation (no PG_GET_COLLATION, check_collation_set or collid). A row whose oid,
/// name or source goes stale, or a new row with no source, fails here.
#[test]
fn every_builtin_fact_row_names_its_function_and_its_postgres_source() {
    let rows = proc_rows();
    let names: std::collections::BTreeMap<u32, &str> = rows.iter().map(|r| (r["oid"].parse::<u32>().unwrap(), r["proname"].as_str())).collect();
    let mut bad = Vec::new();
    let dispatch: Vec<types_core::quiet::Fact> = types_core::quiet::DISPATCH.iter().map(|d| d.0).collect();
    let mut tables: Vec<(&str, &[types_core::quiet::Fact])> = types_core::quiet::TABLES.to_vec();
    tables.push(("DISPATCH", &dispatch));
    for (table, facts) in tables {
        assert!(!facts.is_empty(), "{table} is empty");
        let mut seen = std::collections::BTreeSet::new();
        for &(oid, proname, place) in facts {
            if !seen.insert(oid) {
                bad.push(format!("{table} {oid}: listed twice"));
            }
            if names.get(&oid) != Some(&proname) {
                bad.push(format!("{table} {oid} {proname}: pg_proc.dat names {:?}", names.get(&oid)));
            }
            let mut words = place.split(' ');
            let (Some(path), Some(func)) = (words.next(), words.next().map(|f| f.trim_end_matches(':'))) else {
                bad.push(format!("{table} {oid}: the place is '<file> <function>: <what>', not {place:?}"));
                continue;
            };
            let Ok(text) = fs::read_to_string(reference().join(path)) else {
                bad.push(format!("{table} {oid}: no reference file {path}"));
                continue;
            };
            if path.ends_with(".sql") {
                if !text.contains(func) {
                    bad.push(format!("{table} {oid}: {path} never names {func}"));
                }
                continue;
            }
            let lines: Vec<&str> = text.lines().collect();
            // a macro-made function (pseudotypes.c's dummy inputs): the macro's use at column 0
            if func.contains('(') {
                if !lines.iter().any(|l| l.strip_suffix(';') == Some(func)) {
                    bad.push(format!("{table} {oid}: {path} never expands {func}"));
                }
                continue;
            }
            let def = format!("{func}(");
            let Some(start) = lines.iter().position(|l| l.starts_with(&def)) else {
                bad.push(format!("{table} {oid}: {path} defines no {func}"));
                continue;
            };
            let open = (start..lines.len()).find(|&i| lines[i] == "{");
            let close = open.and_then(|o| (o..lines.len()).find(|&i| lines[i] == "}"));
            let (Some(o), Some(c)) = (open, close) else {
                bad.push(format!("{table} {oid}: {path} {func} has no body"));
                continue;
            };
            let body: String = lines[o..c].concat().split_whitespace().collect();
            if table == "NOTICE" && !["ereport(NOTICE", "ereport(WARNING", "elog(NOTICE", "elog(WARNING"].iter().any(|k| body.contains(k)) {
                bad.push(format!("{table} {oid}: {path} {func} raises no NOTICE or WARNING itself"));
            }
            if table == "HASH_NEEDS_COLLATION" && !body.replace('"', "").contains("couldnotdeterminewhichcollationtouseforstringhashing") {
                bad.push(format!("{table} {oid}: {path} {func} raises no collation error itself"));
            }
            if table == "COLLATION_FREE" && ["PG_GET_COLLATION", "check_collation_set", "collid"].iter().any(|k| body.contains(k)) {
                bad.push(format!("{table} {oid}: {path} {func} reads a collation"));
            }
        }
    }
    assert!(bad.is_empty(), "builtin fact rows that do not hold:\n{}", bad.join("\n"));
}

/// C1 (one effects answer): no VOLATILE builtin raises on no input by the home's per-call answer (`effects::quiet_call`:
/// `pg_proc.proleakproof` or `types_core::quiet::call`). lowerv4's D449 rule (effects.rs `expr_quiet` over props
/// `ops_can_raise`) promises "no user or VOLATILE call" through the raise side alone, which holds only while this does:
/// a builtin that pg_proc marks VOLATILE and that still raises nothing (D3816's disk sizes and lock status) may join
/// `quiet::call` only together with a reading that keeps its volatility there.
#[test]
fn no_volatile_builtin_is_raise_free() {
    let rows = proc_rows();
    let volatile: Vec<_> = rows.iter().filter(|r| r.get("provolatile").map(String::as_str) == Some("v")).collect();
    assert!(volatile.len() > 150, "only {} VOLATILE rows: the reference pg_proc.dat was not read", volatile.len());
    let bad: Vec<String> = volatile
        .iter()
        .filter_map(|r| {
            let oid: u32 = r["oid"].parse().expect("oid");
            let leakproof = r.get("proleakproof").map(String::as_str) == Some("t");
            (leakproof || types_core::quiet::call(oid)).then(|| format!("{oid} {}: {}", r["proname"], if leakproof { "proleakproof" } else { "types_core::quiet::call" }))
        })
        .collect();
    assert!(bad.is_empty(), "VOLATILE builtins read as raise-free (expr_quiet would read them as calling no VOLATILE function):\n{}", bad.join("\n"));
}

/// The call-local acts a held error may cover (`types_core::quiet::CALL_LOCAL_ACT`) are VOLATILE in pg_proc.dat (else
/// the mark decides nothing: the effects answer reads a STABLE or IMMUTABLE call as not acting) and in no other table of
/// calls that act or reach other code (`NOTICE`, `RUNS_OWN_QUERY`, `INPUT_VALIDATORS`, `DISPATCH`), and none of them reads
/// or sets the state they move (`CALL_LOCAL_STATE`).
#[test]
fn a_call_local_act_is_volatile_and_in_no_other_act_table() {
    use types_core::quiet::{CALL_LOCAL_ACT, CALL_LOCAL_STATE, DISPATCH, INPUT_VALIDATORS, NOTICE, RUNS_OWN_QUERY};
    let rows = proc_rows();
    let vol: std::collections::BTreeMap<u32, &str> = rows.iter().map(|r| (r["oid"].parse::<u32>().unwrap(), r.get("provolatile").map_or("i", String::as_str))).collect();
    let mut bad = Vec::new();
    for &(oid, proname, _) in &CALL_LOCAL_ACT {
        if vol.get(&oid) != Some(&"v") {
            bad.push(format!("{oid} {proname}: provolatile {:?}, not 'v'", vol.get(&oid)));
        }
        let others = [("NOTICE", &NOTICE[..]), ("RUNS_OWN_QUERY", &RUNS_OWN_QUERY[..]), ("INPUT_VALIDATORS", &INPUT_VALIDATORS[..]), ("CALL_LOCAL_STATE", &CALL_LOCAL_STATE[..])];
        bad.extend(others.iter().filter(|(_, t)| t.iter().any(|r| r.0 == oid)).map(|(t, _)| format!("{oid} {proname}: also in {t}")));
        if DISPATCH.iter().any(|d| d.0.0 == oid) {
            bad.push(format!("{oid} {proname}: also in DISPATCH"));
        }
    }
    assert!(bad.is_empty(), "call-local act rows that do not hold:\n{}", bad.join("\n"));
}

/// D4144: the draw rows (`types_core::quiet::CALL_LOCAL_DRAW`) are `CALL_LOCAL_ACT` rows whose C function draws at
/// random (pg_proc.dat prosrc drandom, int4random, int8random, numeric_random, drandom_normal, gen_random_uuid); every other
/// call-local act (a sequence's next number, a clock reading) is no draw.
#[test]
fn a_call_local_draw_is_a_random_draw() {
    use types_core::quiet::{CALL_LOCAL_ACT, CALL_LOCAL_DRAW};
    let rows = proc_rows();
    let src: std::collections::BTreeMap<u32, &str> = rows.iter().map(|r| (r["oid"].parse::<u32>().unwrap(), r.get("prosrc").or_else(|| r.get("proname")).map_or("", String::as_str))).collect();
    const DRAWS: [&str; 6] = ["drandom", "drandom_normal", "int4random", "int8random", "numeric_random", "gen_random_uuid"];
    let mut bad = Vec::new();
    for &(oid, proname, _) in &CALL_LOCAL_ACT {
        let draw = src.get(&oid).is_some_and(|s| DRAWS.contains(s));
        if draw != CALL_LOCAL_DRAW.contains(&oid) {
            bad.push(format!("{oid} {proname}: prosrc {:?}, CALL_LOCAL_DRAW {}", src.get(&oid), CALL_LOCAL_DRAW.contains(&oid)));
        }
    }
    bad.extend(CALL_LOCAL_DRAW.iter().filter(|o| !CALL_LOCAL_ACT.iter().any(|r| r.0 == **o)).map(|o| format!("{o}: not a CALL_LOCAL_ACT row")));
    assert!(bad.is_empty(), "draw rows that do not hold:\n{}", bad.join("\n"));
}

/// The F5 walk marks what the held-error allowlist reads, and `effects::call_local` answers over regress statements:
/// nextval and random alone are call-local; lastval reads the state nextval moves and setval moves it (both act past
/// their call); no statement fails closed.
#[test]
fn the_call_local_proof_over_regress_statements() {
    let walk = |name: &str| {
        let text = fs::read_to_string(harness::plans_root().join(format!("regress-full/{name}.plan"))).unwrap();
        let ctx = MemoryContext::new("fixture");
        let fx = corpus::load(ctx.mcx(), &text).unwrap();
        let arena = Arena::new(1 << 20);
        let cx = Cx::new(fx.stmt, &fx.facts, arena.handle());
        let root = fx.stmt.planTree.unwrap();
        let unit = gravity_decompose::rerun::of_plan(fx.stmt, &fx.facts, arena.handle(), root);
        let lone = effects::call_local(&Cx::without_stmt(&fx.facts, arena.handle()), [root]);
        (unit, effects::call_local(&cx, [root]), lone)
    };
    let (f, local, lone) = walk("constraints-002"); // SELECT 'one' AS one, nextval('insert_seq')
    assert!(!f.acts_past_call && f.volatility == Volatility::Volatile && !f.local_state, "{f:?}");
    assert!(f.order_call, "D4144: nextval's place shows: {f:?}");
    assert!(local.is_some(), "nextval alone is granted");
    assert!(lone.is_none(), "no statement: fail closed");
    let (f, local, _) = walk("random-017"); // SELECT random() FROM generate_series(1, 10)
    assert!(!f.acts_past_call && local.is_some(), "{f:?}");
    assert!(!f.order_call, "D4144: a random draw's place shows in no value: {f:?}");
    let (f, local, _) = walk("sequence-001"); // SELECT lastval()
    assert!(f.acts_past_call && f.local_state && local.is_none(), "{f:?}");
    assert!(f.order_call, "D4144: lastval reads the state nextval moves: {f:?}");
    let (f, local, _) = walk("sequence-003"); // SELECT setval('sequence_test'::text, 32)
    assert!(f.acts_past_call && f.local_state && local.is_none(), "{f:?}");
    let (f, local, _) = walk("sequence-005"); // SELECT last_value, .. FROM foo_seq_new: the sequence's row, read
    assert!(f.local_state && local.is_none(), "{f:?}");
}

/// A fill inside a SubPlan or a CTE holds the call-local proof as one in the main tree does: the statement's one walk
/// (`rerun::of_all_plans`) reaches the SubPlan's plan through its call and does not walk it again from the subplans
/// list.
#[test]
fn a_subplans_nextval_holds_the_call_local_proof() {
    // cte-076: SELECT a, (SELECT nextval('facts_seq') + x FROM u WHERE u.x = t.a) FROM t WHERE a < 3
    let text = fs::read_to_string(harness::plans_root().join("cte-076.plan")).unwrap();
    let ctx = MemoryContext::new("fixture");
    let fx = corpus::load(ctx.mcx(), &text).unwrap();
    let arena = Arena::new(1 << 20);
    let all = gravity_decompose::rerun::of_all_plans(fx.stmt, &fx.facts, arena.handle());
    assert!(!all.acts_past_call && !all.local_state && !all.user_call, "{all:?}");
    assert!(all.order_call, "D4144: the SubPlan's nextval is a call whose place shows: {all:?}");
    let sub = gravity_decompose::rerun::subplan_root(fx.stmt, 1).expect("the scalar SubPlan's plan");
    assert!(effects::call_local(&Cx::new(fx.stmt, &fx.facts, arena.handle()), [sub]).is_some(), "the SubPlan's nextval is call-local");
}

/// D4583: `effects::runs_catalog_query` (`types_core::quiet::CATALOG_QUERY`) answers exactly `CATALOG_ONLY`'s builtins, the
/// ones `the_builtins_that_run_a_query_of_their_own_are_known` derives from pg_proc.dat and the SPI users, and none of
/// them answers `runs_own_query`.
#[test]
fn runs_catalog_query_answers_the_catalog_only_builtins() {
    use std::collections::BTreeSet;
    let listed: BTreeSet<u32> = CATALOG_ONLY.iter().map(|r| r.0).collect();
    let says: BTreeSet<u32> = (0..100_000).filter(|o| effects::runs_catalog_query(*o)).collect();
    assert_eq!(says, listed, "runs_catalog_query against CATALOG_ONLY");
    assert!(listed.iter().all(|o| !effects::runs_own_query(*o)), "a CATALOG_ONLY builtin answers runs_own_query");
}

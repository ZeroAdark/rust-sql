// FACTS-1b (M8-plan §7.1; M8-FACTS): the facts M8's ROAD tier reads, each a holds / does-not-hold pair over captured plans
// (tests/plans/cte.sql's FACTS-1b section, scripts/gravity-capture-plans.sh cte):
//   - F1 rescan invariance (`NestFacts::invariant`, `SubPlanDesc::invariant`): a nest's inner / a nested SubPlan's plan that
//     reads no changing param and holds nothing volatile or rescan-unstable; false for a parameterised inner, a volatile
//     call in a qual, a projection, an index qual (a user function reads as its fixture's `-- func` line says, unknown
//     without one: volatile) or a CTE the inner reads, a data-modifying CTE; a heap BERNOULLI / SYSTEM TABLESAMPLE holds; the zero-column and
//     dropped-column inners (D870) and a STABLE user function's index qual hold;
//   - F2 the equality conjunct (`NestFacts::eq`): the first hashable `outer = inner_col`, its opfamily, both sides' types and
//     the operator's input collation (a cross-type int8 = int4, a COLLATE "C" over default-collated columns); none after a
//     raising conjunct, for an inner expression, a non-equality qual or a parameterised inner (no join qual); a nest whose
//     inner holds and whose equality keys it (a raise-free inner expression too) is built once instead (D3014: the keyed
//     form, its clause operator and collation);
//   - F5 volatility / raise-free on InitPlans, SubPlans and nest inners;
//   - F6 the Rep of every PARAM_EXEC id a nest or a SubPlan binds (by value, by reference with a collation, a numeric typmod);
//   - over every fixture: the facts' invariants (an invariant inner has nothing volatile and no parameter-driven rerun set,
//     only one re-running what PG re-executes at a rescan (main's ruling (i), D1018) or a kept one run once (item 9 (b));
//     `eq.pos` names a conjunct with that operator) and the census mirror (tools/graphprops_facts.py --check agrees with
//     every golden);
//   - src/procfacts.rs is the generator's output, with the catalog headers' BKI defaults for every omitted field (main 07:21Z).
//   - the pipeline an inner is embedded in reads the inner's slots (main 07:52Z, M8-NEST's nest.spool): pinned on bench-013,
//     a sink with two Material inners and a nested SubPlan's aggregate, and checked over every fixture.
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use gravity_abi::breaker::BreakerKind;
use gravity_decompose::effects::{builtin_func, builtin_oper, Volatility};
use gravity_decompose::graph::{Op, ParamOwner, PipelineGraph, PipelineNode, Source};
use gravity_decompose::facts::nest::NestFacts;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::agg::GroupForm;
use gravity_decompose::ops::shared::subplan::{SubForm, SubLink};
use gravity_decompose::{corpus, decompose_in};
use gravity_program::{Arena, End, Rep};
use mcx::MemoryContext;

mod harness;

fn root() -> PathBuf {
    std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../..")
}

/// The cte fixture whose `-- sql:` statement is `sql` (exactly one).
fn fixture(sql: &str) -> PathBuf {
    let hits: Vec<PathBuf> = harness::fixtures_in(&harness::plans_root())
        .into_iter()
        // a nested plan (`cte-<n>.<k>`, a function body PostgreSQL planned under the statement) carries its outer's sql
        .filter(|p| p.file_name().and_then(|f| f.to_str()).is_some_and(|f| f.starts_with("cte-") && f.matches('.').count() == 1))
        .filter(|p| fs::read_to_string(p).unwrap().lines().any(|l| l.strip_prefix("-- sql: ").is_some_and(|q| q.trim().trim_end_matches(';') == sql)))
        .collect();
    assert_eq!(hits.len(), 1, "cte fixtures of {sql}: {hits:?} (re-capture: scripts/gravity-capture-plans.sh cte)");
    hits.into_iter().next().unwrap()
}

fn with_graph<R>(sql: &str, f: impl FnOnce(&PipelineGraph<'_>) -> R) -> R {
    let text = fs::read_to_string(fixture(sql)).unwrap();
    let ctx = MemoryContext::new("facts_m8b");
    let fx = corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let arena = Arena::new(1 << 20);
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|r| panic!("{sql}: refused {r}"));
    f(&g)
}

fn all_pipes<'a, 'p>(p: &'a PipelineNode<'p>, out: &mut Vec<&'a PipelineNode<'p>>) {
    out.push(p);
    for op in p.ops.iter() {
        match op {
            Op::Join(JoinDesc::Nest { inner, .. }) => all_pipes(inner, out),
            Op::SubPlan(d) => {
                if let SubForm::Nested { inner } = &d.form {
                    all_pipes(inner, out);
                }
            }
            _ => {}
        }
    }
}

fn pipes<'a, 'p>(g: &'a PipelineGraph<'p>) -> Vec<&'a PipelineNode<'p>> {
    let mut out = Vec::new();
    for p in g.pipelines.iter() {
        all_pipes(p, &mut out);
    }
    out
}

/// The statement's one nest: its facts, its join qual's length.
#[derive(Clone, Copy, Debug)]
struct Nest {
    invariant: bool,
    volatility: Volatility,
    raise_free: bool,
    /// (pos, opno, opfamily, types, coll, outer_left, outer_raise_free)
    eq: Option<(u16, u32, u32, (u32, u32), u32, bool, bool)>,
    qual: usize,
    params: usize,
}

/// The form the statement's one nest takes: the Nest (its facts), or the keyed form (D3014 build-once, self_spool.rs
/// `keyed`): an inner that reads nothing of the outer row built once into a hash table and probed by the join's first
/// hashable equality (the probe's clause operator and input collation).
#[derive(Clone, Copy, Debug)]
enum Form {
    Nest(Nest),
    Keyed { opno: u32, coll: u32 },
}

fn form(sql: &str) -> Form {
    with_graph(sql, |g| {
        let (mut nests, mut keyed) = (Vec::new(), Vec::new());
        for p in pipes(g) {
            for op in p.ops.iter() {
                match op {
                    Op::Join(JoinDesc::Nest { facts: NestFacts { invariant, volatility, raise_free, eq, .. }, qual, params, .. }) => {
                        let eq = eq.map(|e| (e.pos, e.opno, e.opfamily, e.types, e.coll, e.outer_left, e.outer_raise_free));
                        nests.push(Nest { invariant: *invariant, volatility: *volatility, raise_free: *raise_free, eq, qual: qual.len(), params: params.len() });
                    }
                    Op::Join(JoinDesc::Probe { catalog: true, clauses, .. }) => {
                        let o = clauses.iter().next().and_then(|c| c.as_op_expr());
                        keyed.push(Form::Keyed { opno: o.map_or(0, |o| o.opno), coll: o.map_or(0, |o| o.inputcollid) });
                    }
                    _ => {}
                }
            }
        }
        match (nests.as_slice(), keyed.as_slice()) {
            ([n], []) => Form::Nest(*n),
            ([], [k]) => *k,
            _ => panic!("{sql}: nests {nests:?}, keyed {keyed:?}"),
        }
    })
}

fn nest(sql: &str) -> Nest {
    match form(sql) {
        Form::Nest(n) => n,
        k => panic!("{sql}: {k:?}, not a Nest"),
    }
}

const TEXT_EQ: u32 = 98;
const TEXT_HASH_OPS: u32 = 1995;
const INT48_EQ: u32 = 15;
const INTEGER_HASH_OPS: u32 = 1977;

#[test]
fn f1_rescan_invariance_holds_and_does_not() {
    let cases: [(&str, bool); 13] = [
        ("SELECT t.a, u.x FROM t JOIN u ON u.z = t.c WHERE t.a < 3", true),
        // a stable call in the inner's index qual keeps the rows fixed for the statement
        ("SELECT t.a, u.z FROM t LEFT JOIN u ON u.z = t.c AND u.x = length(now()::text) WHERE t.a < 3", true),
        ("WITH c AS MATERIALIZED (SELECT x, z FROM u) SELECT t.a, c.x FROM t JOIN c ON c.z = t.c WHERE t.a < 3", true),
        // D870: a zero-column inner and an inner over a table with a dropped column
        ("SELECT t.a FROM t CROSS JOIN z WHERE t.a < 3", true),
        ("SELECT t.a, w.k FROM t JOIN w ON w.s = t.c COLLATE \"C\" WHERE t.a < 3", true),
        // the nest's own param drives the inner's index scan
        ("SELECT t.a, u.z FROM t JOIN u ON u.x = t.a WHERE t.a < 3", false),
        // a volatile call in the inner's quals, in its projection, inside the CTE it reads
        ("SELECT t.a, u.x FROM t JOIN u ON u.z = t.c WHERE t.a < 3 AND u.y < random() * 1000", false),
        ("SELECT t.a, s.r FROM t JOIN (SELECT z, random() AS r FROM u) s ON s.z = t.c WHERE t.a < 3", false),
        ("WITH c AS MATERIALIZED (SELECT x, z, random() AS r FROM u) SELECT t.a, c.x FROM t JOIN c ON c.z = t.c WHERE t.a < 3", false),
        // a STABLE user function in the inner's index qual: past the builtin table, its fixture's `-- func` line says
        // stable (decision 20), so the rows stay fixed for the statement as with now()
        ("SELECT t.a, u.z FROM t LEFT JOIN u ON u.z = t.c AND u.x = facts_user(7) WHERE t.a < 3", true),
        // a data-modifying CTE's rows
        ("WITH d AS (UPDATE w SET v = v WHERE k < 3 RETURNING k, s) SELECT t.a, d.s FROM t JOIN d ON d.k = t.b WHERE t.a < 3", false),
        // D1920 / D1644: a heap SYSTEM sample samples the same rows at every run of its plan node (its REPEATABLE value's
        // seed; without one, the node's one seed in the execution)
        ("SELECT t.a, s.x FROM t JOIN u s TABLESAMPLE SYSTEM (50) REPEATABLE (7) ON s.z = t.c WHERE t.a < 3", true),
        ("SELECT t.a, w FROM t JOIN w ON w.k = t.b WHERE t.a < 3", true),
    ];
    let (mut bad, mut keyed) = (Vec::new(), 0);
    for (sql, want) in cases {
        match form(sql) {
            Form::Nest(n) => {
                println!("facts_m8b F1\t{sql}\t{n:?}");
                if n.invariant != want {
                    bad.push(format!("{sql}: invariant {}, want {want} ({n:?})", n.invariant));
                }
                if n.invariant && (n.params > 0 || n.volatility == Volatility::Volatile) {
                    bad.push(format!("{sql}: an invariant inner with params or a volatile call ({n:?})"));
                }
            }
            // D3014 build-once: only an inner that holds (reads nothing of the outer row, nothing volatile or
            // rescan-unstable) is built once
            k => {
                println!("facts_m8b F1\t{sql}\t{k:?}");
                keyed += 1;
                if !want {
                    bad.push(format!("{sql}: built once ({k:?}), but its inner does not hold"));
                }
            }
        }
    }
    if keyed == 0 {
        bad.push("no holding inner took the keyed form (D3014 build-once)".into());
    }
    // now() in the index qual: a stable inner, built once or a Nest that reads it as stable
    if let Form::Nest(n) = form("SELECT t.a, u.z FROM t LEFT JOIN u ON u.z = t.c AND u.x = length(now()::text) WHERE t.a < 3") {
        if n.volatility != Volatility::Stable {
            bad.push("now() in the index qual: the inner is not stable".into());
        }
    }
    if nest("SELECT t.a, u.x FROM t JOIN u ON u.z = t.c WHERE t.a < 3 AND u.y < random() * 1000").volatility != Volatility::Volatile {
        bad.push("random() in the inner's qual: the inner is not volatile".into());
    }
    // a nested SubPlan's plan: uncorrelated and immutable holds; volatile or correlated does not
    for (sql, want) in [
        ("SELECT a FROM t WHERE a < 3 AND b > ALL (SELECT y FROM u WHERE x < 5)", true),
        ("SELECT a FROM t WHERE a < 3 AND b > ALL (SELECT y + (random() * 0)::int FROM u WHERE x < 5)", false),
        ("SELECT a, (SELECT count(*) FROM u WHERE u.y = t.d) FROM t WHERE a < 3", false),
    ] {
        let got = with_graph(sql, |g| {
            pipes(g).iter().flat_map(|p| p.ops.iter()).filter_map(|o| match o {
                Op::SubPlan(d) if matches!(d.form, SubForm::Nested { .. }) => Some(d.invariant),
                _ => None,
            }).collect::<Vec<_>>()
        });
        println!("facts_m8b F1 subplan\t{sql}\t{got:?}");
        if got != [want] {
            bad.push(format!("{sql}: nested SubPlans' invariant {got:?}, want [{want}]"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn f2_the_equality_conjunct_holds_and_does_not() {
    let mut bad = Vec::new();
    // (sql, want eq: pos, opno, opfamily, (outer type, inner type), coll, outer_left)
    let holds: [(&str, (u16, u32, u32, (u32, u32), u32, bool)); 4] = [
        ("SELECT t.a, u.x FROM t JOIN u ON u.z = t.c WHERE t.a < 3", (0, TEXT_EQ, TEXT_HASH_OPS, (25, 25), 100, true)),
        // cross-type: int4 = int8 in integer_ops (the planner puts the outer int4 on the left)
        ("SELECT t.a, w FROM t JOIN w ON w.k = t.b WHERE t.a < 3", (0, INT48_EQ, INTEGER_HASH_OPS, (23, 20), 0, true)),
        // the operator's collation (COLLATE "C"), not the outer column's (the database default)
        ("SELECT t.a, w.k FROM t JOIN w ON w.s = t.c COLLATE \"C\" WHERE t.a < 3", (0, TEXT_EQ, TEXT_HASH_OPS, (25, 25), 950, true)),
        // a volatile filter on the inner's scan leaves the join qual's equality as it is (F1 is false: what a road reads)
        ("SELECT t.a, u.x FROM t JOIN u ON u.z = t.c WHERE t.a < 3 AND u.y < random() * 1000", (0, TEXT_EQ, TEXT_HASH_OPS, (25, 25), 100, true)),
    ];
    for (sql, want) in holds {
        match form(sql) {
            Form::Nest(n) => {
                println!("facts_m8b F2\t{sql}\t{n:?}");
                match n.eq {
                    Some((pos, op, fam, types, coll, left, _)) if (pos, op, fam, types, coll, left) == want => {}
                    other => bad.push(format!("{sql}: eq {other:?}, want {want:?}")),
                }
            }
            // D3014 build-once: the keyed form probes by that conjunct, its operator and input collation
            Form::Keyed { opno, coll } => {
                println!("facts_m8b F2\t{sql}\tkeyed op {opno} coll {coll}");
                if (opno, coll) != (want.1, want.4) {
                    bad.push(format!("{sql}: keyed by op {opno} coll {coll}, want {want:?}"));
                }
            }
        }
    }
    for (sql, why) in [
        ("SELECT t.a, u.x FROM t JOIN u ON u.y / t.b > 0 AND u.z = t.c || '' WHERE t.a < 3", "a raising conjunct (int4div) before the equality"),
        ("SELECT t.a, u.x FROM t JOIN u ON u.z || '' = t.c WHERE t.a < 3", "the inner side is an expression"),
        ("SELECT t.a, u.x FROM t JOIN u ON u.z < t.c WHERE t.a < 3 AND u.x < 3", "no equality"),
        ("SELECT t.a, u.z FROM t JOIN u ON u.x = t.a WHERE t.a < 3", "a parameterised inner: no join qual"),
    ] {
        match form(sql) {
            Form::Nest(n) => {
                println!("facts_m8b F2\t{sql}\t{n:?}");
                if n.eq.is_some() || (why.starts_with("a raising") && n.qual != 2) {
                    bad.push(format!("{sql}: {why}: eq {:?}, join qual of {}", n.eq, n.qual));
                }
            }
            // D3014 build-once (facts/nest.rs `eq_keyed`): a raise-free inner expression keys the build; the Nest's F2
            // reads a bare inner column only
            Form::Keyed { opno, coll } => {
                println!("facts_m8b F2\t{sql}\tkeyed op {opno} coll {coll}");
                if why != "the inner side is an expression" || (opno, coll) != (TEXT_EQ, 100) {
                    bad.push(format!("{sql}: {why}: keyed by op {opno} coll {coll}"));
                }
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn f5_volatility_and_raise_free() {
    let mut bad = Vec::new();
    for (sql, want) in [
        // EXISTS over a leakproof index qual: stable to run at open
        ("SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE x = 42) AND a < 3", (Volatility::Immutable, true)),
        // a scalar sublink over an index scan may see a second row: `more than one row returned by a subquery`
        ("SELECT a FROM t WHERE b = (SELECT y FROM u WHERE x = 7)", (Volatility::Immutable, false)),
    ] {
        let got = with_graph(sql, |g| g.init_plans.iter().map(|i| (i.volatility, i.raise_free)).collect::<Vec<_>>());
        println!("facts_m8b F5 init\t{sql}\t{got:?}");
        if got != [want] {
            bad.push(format!("{sql}: InitPlans {got:?}, want [{want:?}]"));
        }
    }
    for (sql, want) in [
        ("SELECT a, (SELECT nextval('facts_seq') + x FROM u WHERE u.x = t.a) FROM t WHERE a < 3", Volatility::Volatile),
        ("SELECT a, (SELECT count(*) FROM u WHERE u.y = t.b AND u.z < now()::text) FROM t WHERE a < 3", Volatility::Stable),
        ("SELECT a FROM t WHERE a < 3 AND b > ALL (SELECT y FROM u WHERE x < 5)", Volatility::Immutable),
    ] {
        let got = with_graph(sql, |g| {
            pipes(g).iter().flat_map(|p| p.ops.iter()).filter_map(|o| match o {
                Op::SubPlan(d) => Some((d.volatility, d.raise_free, d.link)),
                _ => None,
            }).collect::<Vec<_>>()
        });
        println!("facts_m8b F5 subplan\t{sql}\t{got:?}");
        if got.len() != 1 || got[0].0 != want {
            bad.push(format!("{sql}: SubPlans {got:?}, want volatility {want:?}"));
        }
        // a count / nextval call is no leakproof function: never raise-free
        if got.first().is_some_and(|g| g.1 && g.2 == SubLink::Expr) {
            bad.push(format!("{sql}: a scalar SubPlan over a non-leakproof call is raise-free"));
        }
    }
    // a plain inner over a CTE's rows with no equality to key it by (a plain seq scan inner is built once, D3014, and a
    // keyed CTE Scan inner too, B6: no Nest to read)
    let n = nest("WITH c AS MATERIALIZED (SELECT a, b FROM t WHERE b = 1) SELECT c1.a FROM c c1, c c2 WHERE c1.a < c2.a");
    if n.volatility != Volatility::Immutable {
        bad.push(format!("a CTE scan inner: {n:?}"));
    }
    // B6: the same CTE Scan inner under a hashable equality is built once, by the clause's own operator
    match form("WITH c AS MATERIALIZED (SELECT x, z FROM u) SELECT t.a, c.x FROM t JOIN c ON c.z = t.c WHERE t.a < 3") {
        Form::Keyed { opno: TEXT_EQ, .. } => {}
        k => bad.push(format!("a keyed CTE scan inner (B6): {k:?}")),
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn f6_every_bound_param_has_its_owners_rep() {
    let mut bad = Vec::new();
    // (sql, the owner is a nest, typid, typmod, coll, rep is a word). The collation is the value's own (`exprCollation` of
    // the paramval: u.z's default), not the one the inner's index qual compares it under (COLLATE "C": the qual's own).
    for (sql, nest_owner, typid, typmod, coll, word) in [
        ("SELECT t.a, u.z FROM t JOIN u ON u.x = t.a WHERE t.a < 3", true, 23, -1, 0, true),
        ("SELECT u.x, w.k FROM u JOIN w ON w.s = u.z COLLATE \"C\" WHERE u.x < 3", true, 25, -1, 100, false),
        // numeric(12,2): ((12 << 16) | 2) + 4
        ("SELECT a, (SELECT count(*) FROM u WHERE u.y = t.d) FROM t WHERE a < 3", false, 1700, (12 << 16 | 2) + 4, 0, false),
    ] {
        let got = with_graph(sql, |g| g.params.iter().map(|(id, p)| (*id, *p)).collect::<Vec<_>>());
        println!("facts_m8b F6\t{sql}\t{got:?}");
        let ok = got.len() == 1 && {
            let p = got[0].1;
            matches!(p.owner, ParamOwner::Nest { .. }) == nest_owner && (p.typid, p.typmod, p.coll) == (typid, typmod, coll)
                && p.out.is_some_and(|o| matches!(o.rep, Rep::Word { .. }) == word)
        };
        if !ok {
            bad.push(format!("{sql}: params {got:?}"));
        }
    }
    // a statement without a nest or SubPlan param binds none
    if with_graph("SELECT t.a, u.x FROM t JOIN u ON u.z = t.c WHERE t.a < 3", |g| g.params.len()) != 0 {
        bad.push("an unparameterised nest lists params".into());
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// Whether rerun set pipeline `q` fills what PG re-executes at a rescan that changes no param (main's ruling (i), D1018):
/// a plain aggregate, a group not hashed (a sorted Agg, Group or Unique; the last pipeline feeding the slot declares it),
/// or the partition buffer of the window `inner` runs. A hash build, a sort, a top-k or a hashed group PG keeps.
fn reexecuted(g: &PipelineGraph<'_>, inner: &PipelineNode<'_>, q: u16) -> bool {
    let End::Breaker(b) = g.pipelines[q as usize].desc.ends_in else { return false };
    match b.kind {
        BreakerKind::PlainAgg => true,
        BreakerKind::Materialize => matches!(inner.source, Source::Breaker { slot } if slot == b.slot) && inner.ops.iter().any(|op| matches!(op, Op::Window(_))),
        BreakerKind::HashAgg | BreakerKind::Distinct => g.groups.iter().filter(|d| d.slot == b.slot && d.producer.0 >= q).min_by_key(|d| d.producer.0).is_some_and(|d| !matches!(d.form, GroupForm::Hashed | GroupForm::Pass1)),
        _ => false,
    }
}

/// Over every fixture: an invariant nest has no params, nothing volatile and no rerun set but one PG re-executes; an
/// equality conjunct names a conjunct of the join qual with its operator; every param a nest binds is listed once per op
/// of that nest (a grouping sets rewrite maps the input once per arm: each copy binds it).
#[test]
fn the_facts_hold_their_invariants_over_every_fixture() {
    let mut files = harness::fixtures_in(&harness::plans_root());
    files.extend(harness::fixtures_in(&harness::plans_root().join("regress-full")));
    let rows = harness::par_map(&files, |path| {
        let text = fs::read_to_string(path).unwrap();
        let ctx = MemoryContext::new("facts_m8b");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { return (BTreeMap::new(), Vec::new()) };
        let arena = Arena::new(1 << 20);
        let Ok(g) = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { return (BTreeMap::new(), Vec::new()) };
        let (mut n, mut bad) = (BTreeMap::new(), Vec::new());
        let mut copies: BTreeMap<i32, usize> = BTreeMap::new();
        for p in pipes(&g) {
            for op in p.ops.iter() {
                if let Op::Join(JoinDesc::Nest { facts, .. }) = op {
                    *copies.entry(facts.node).or_insert(0) += 1;
                }
            }
        }
        for p in pipes(&g) {
            for op in p.ops.iter() {
                let Op::Join(JoinDesc::Nest { facts, qual, params, inner, .. }) = op else { continue };
                *n.entry("nests").or_insert(0) += 1;
                *n.entry("invariant").or_insert(0) += facts.invariant as usize;
                *n.entry("eq").or_insert(0) += facts.eq.is_some() as usize;
                // main's ruling (i) on D1018: an invariant nest has no parameter-driven rerun set (it has no params); a set
                // re-running what PG re-executes at a rescan (a plain or sorted Agg, Unique, Group, WindowAgg) is allowed, and
                // so is item 9 (b)'s kept set (`g.kept`: it runs at the first re-open only, as PG fills a kept node once)
                let kept = g.reruns.iter().filter(|r| r.node_id == facts.node && r.plan_id == 0).any(|r| !r.pipelines.iter().all(|&q| reexecuted(&g, inner, q) || g.kept.contains(&q)));
                if facts.invariant && (!params.is_nil() || facts.volatility == Volatility::Volatile || kept) {
                    bad.push(format!("{}: nest n{} invariant with params / a volatile call / a rerun set PG would not re-execute", path.display(), facts.node));
                }
                if let Some(e) = facts.eq {
                    let c = qual.iter().nth(e.pos as usize).and_then(|c| c.as_op_expr());
                    if c.is_none_or(|c| c.opno != e.opno) {
                        bad.push(format!("{}: nest n{} eq #{} is no conjunct with op{}", path.display(), facts.node, e.pos, e.opno));
                    }
                }
                for np in params.iter().filter_map(|x| x.as_nest_loop_param()) {
                    let owned = g.params.iter().filter(|(id, r)| *id == np.paramno && r.owner == ParamOwner::Nest { node: facts.node }).count();
                    if owned != copies[&facts.node] {
                        bad.push(format!("{}: nest n{} param $exec{} listed {owned} times for {} ops", path.display(), facts.node, np.paramno, copies[&facts.node]));
                    }
                }
            }
        }
        (n, bad)
    });
    let mut total: BTreeMap<&str, usize> = BTreeMap::new();
    for (n, _) in rows.iter() {
        for (k, v) in n {
            *total.entry(k).or_insert(0) += v;
        }
    }
    eprintln!("facts_m8b over every fixture: {total:?}");
    let bad: Vec<&String> = rows.iter().flat_map(|r| r.1.iter()).collect();
    assert!(bad.is_empty(), "{bad:#?}");
}

/// The census mirror (M8-plan §7.1): tools/graphprops_facts.py derives F1 / F2 / F5 / F6 from each plan fixture and the
/// reference catalog on its own and must print what every golden says.
#[test]
fn the_census_mirror_agrees_with_every_golden() {
    let out = std::process::Command::new("python3").arg(root().join("crates/gravity/decompose/testdata/gravity/lowerv4/tools/graphprops_facts.py")).arg("--check").output().expect("python3 runs graphprops_facts.py");
    let err = String::from_utf8_lossy(&out.stderr);
    let text = String::from_utf8_lossy(&out.stdout);
    println!("{text}");
    assert!(out.status.success(), "graphprops_facts.py --check: {}", err.lines().take(40).collect::<Vec<_>>().join("\n"));
}

/// src/procfacts.rs is what gen/procfacts.py writes today (a hand edit or a stale table fails), and the generator's selftest
/// (the BKI defaults over synthetic rows) passes.
#[test]
fn procfacts_is_the_generators_output() {
    let gen = root().join("crates/gravity/decompose/gen/procfacts.py");
    for arg in ["--check", "--selftest"] {
        let out = std::process::Command::new("python3").arg(&gen).arg(arg).output().expect("python3 runs the generator");
        assert!(out.status.success(), "procfacts.py {arg}: {}", String::from_utf8_lossy(&out.stderr));
    }
}

/// One .dat file's rows as (key, value) lists (quoted values with backslash escapes; `#` comment lines skipped).
fn dat(name: &str) -> Vec<Vec<(String, String)>> {
    let text = fs::read_to_string(root().join("crates/postgres-18.6-reference/src/include/catalog").join(name)).unwrap();
    let b: Vec<char> = text.chars().collect();
    let (mut rows, mut cur, mut key, mut i) = (Vec::new(), None::<Vec<(String, String)>>, String::new(), 0);
    while i < b.len() {
        let c = b[i];
        match &mut cur {
            None if c == '#' => {
                while i < b.len() && b[i] != '\n' {
                    i += 1;
                }
            }
            None if c == '{' => (cur, key) = (Some(Vec::new()), String::new()),
            None => {}
            Some(row) => match c {
                '\'' => {
                    let mut v = String::new();
                    i += 1;
                    while b[i] != '\'' {
                        if b[i] == '\\' {
                            i += 1;
                        }
                        v.push(b[i]);
                        i += 1;
                    }
                    row.push((std::mem::take(&mut key), v));
                }
                '}' => rows.push(cur.take().unwrap()),
                ',' => key.clear(),
                c if c.is_alphanumeric() || c == '_' => key.push(c),
                _ => {}
            },
        }
        i += 1;
    }
    rows
}

fn get<'a>(row: &'a [(String, String)], k: &str) -> Option<&'a str> {
    row.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
}

/// Main 07:21Z: a .dat row that omits provolatile / proisstrict / proleakproof / oprcanhash / oprcanmerge takes pg_proc.h's /
/// pg_operator.h's BKI_DEFAULT ('i', 't', 'f', 'f', 'f'). Every row agrees with the table, and rows omitting each field exist.
#[test]
fn procfacts_takes_the_bki_default_of_an_omitted_field() {
    let (mut omitted, mut bad) = (BTreeMap::<&str, usize>::new(), Vec::new());
    for row in dat("pg_proc.dat") {
        let Some(oid) = get(&row, "oid").and_then(|o| o.parse::<u32>().ok()) else { continue };
        for k in ["provolatile", "proisstrict", "proleakproof"] {
            *omitted.entry(k).or_insert(0) += get(&row, k).is_none() as usize;
        }
        let vol = match get(&row, "provolatile").unwrap_or("i") {
            "i" => Volatility::Immutable,
            "s" => Volatility::Stable,
            _ => Volatility::Volatile,
        };
        let want = (vol, get(&row, "proisstrict").unwrap_or("t") == "t", get(&row, "proleakproof").unwrap_or("f") == "t");
        match builtin_func(oid) {
            Some(f) if (f.volatility, f.strict, f.leakproof) == want => {}
            other => bad.push(format!("pg_proc {oid}: {other:?}, want {want:?}")),
        }
    }
    for row in dat("pg_operator.dat") {
        let Some(oid) = get(&row, "oid").and_then(|o| o.parse::<u32>().ok()) else { continue };
        for k in ["oprcanhash", "oprcanmerge"] {
            *omitted.entry(k).or_insert(0) += get(&row, k).is_none() as usize;
        }
        let want = (get(&row, "oprcanhash").unwrap_or("f") == "t", get(&row, "oprcanmerge").unwrap_or("f") == "t");
        match builtin_oper(oid) {
            Some(o) if (o.canhash, o.canmerge) == want && (!o.canhash || o.poly || o.hash_family != 0 || oid == 1070) => {}
            other => bad.push(format!("pg_operator {oid}: {other:?}, want {want:?}")),
        }
    }
    eprintln!("facts_m8b omitted fields: {omitted:?}");
    for k in ["provolatile", "proisstrict", "proleakproof", "oprcanhash", "oprcanmerge"] {
        if omitted.get(k).copied().unwrap_or(0) == 0 {
            bad.push(format!("no .dat row omits {k}: the default path is untested"));
        }
    }
    // an oid outside the builtin catalog has no properties: every fact that reads it is false
    assert!(builtin_func(16384).is_none() && builtin_oper(16384).is_none());
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

fn with_path<R>(path: &std::path::Path, f: impl FnOnce(&PipelineGraph<'_>) -> R) -> R {
    with_path_roads_off(path, &[], f)
}

/// `with_path` with the on-by-default roads `off` named off (`FixtureFacts::roads_off`).
fn with_path_roads_off<R>(path: &std::path::Path, off: &[&'static str], f: impl FnOnce(&PipelineGraph<'_>) -> R) -> R {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let ctx = MemoryContext::new("facts_m8b");
    let mut fx = corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    fx.facts.roads_off = off.to_vec();
    let arena = Arena::new(1 << 20);
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|r| panic!("{}: refused {r}", path.display()));
    f(&g)
}

/// (slots, lives) a descriptor reads.
fn reads(p: &PipelineNode<'_>) -> (Vec<u8>, Vec<u16>) {
    (p.desc.consumes.to_vec(), p.desc.consumes_from.iter().map(|l| l.0).collect())
}

/// Each pipeline carrying an embedded inner (the pipeline index, then every inner's reads in embedding order).
fn carriers(g: &PipelineGraph<'_>) -> Vec<(usize, (Vec<u8>, Vec<u16>), Vec<(Vec<u8>, Vec<u16>)>)> {
    let mut out = Vec::new();
    for (i, p) in g.pipelines.iter().enumerate() {
        let mut all = Vec::new();
        all_pipes(p, &mut all);
        if all.len() > 1 {
            out.push((i, reads(p), all[1..].iter().map(|q| reads(q)).collect()));
        }
    }
    out
}

/// main 07:52Z (M8-NEST's nest.spool, B1s): the pipeline an inner is embedded in reads every slot the inner reads (a nest's
/// Material / CTE / aggregate inner, a nested SubPlan's), after its own and from the inner's life, the inner keeping its
/// entry. bench-013 (Q13, the Material inner of a breaker-ending outer, with the four nest.keep roads named off: at
/// their default, on since the D4497 / D3227 flip, its nest is kept keyed and embeds no inner), regress-full join-009
/// (a sink outer with two Material inners), a nested SubPlan over a PlainAgg (cte-077's statement); an index-scan inner
/// adds nothing.
#[test]
fn an_outer_pipeline_reads_the_slots_its_embedded_inners_read() {
    let plans = harness::plans_root();
    let keep = ["nest.keep.initplan", "nest.keep.subquery", "nest.keep.semi", "nest.keep.material"];
    let q13 = with_path_roads_off(&plans.join("bench-013.plan"), &keep, carriers);
    assert_eq!(q13, vec![(1, (vec![0], vec![0]), vec![(vec![0], vec![0])])], "bench-013: p1 reads slot0@p0 beside its inner");
    assert!(with_path(&plans.join("bench-013.plan"), carriers).is_empty(), "bench-013 at the nest.keep defaults: the nest is kept keyed");
    let j9 = with_path(&plans.join("regress-full/join-009.plan"), carriers);
    assert_eq!(j9, vec![(2, (vec![0, 1], vec![0, 1]), vec![(vec![0], vec![0]), (vec![1], vec![1])])], "join-009: the sink reads both Material slots, in embedding order");
    let sub = with_graph("SELECT a, (SELECT count(*) FROM u WHERE u.y = t.b AND u.z < now()::text) FROM t WHERE a < 3", carriers);
    assert_eq!(sub, vec![(1, (vec![0], vec![0]), vec![(vec![0], vec![0])])], "a nested SubPlan's aggregate slot");
    let none = with_graph("SELECT t.a, u.z FROM t JOIN u ON u.x = t.a WHERE t.a < 3", carriers);
    assert_eq!(none, vec![(0, (vec![], vec![]), vec![(vec![], vec![])])], "an index-scan inner reads no slot");
}

/// Over every fixture: a pipeline reads every slot each inner embedded in it (at any depth) reads, from the same life,
/// each slot once.
#[test]
fn every_embedding_pipeline_reads_its_inners_slots() {
    let mut files = harness::fixtures_in(&harness::plans_root());
    files.extend(harness::fixtures_in(&harness::plans_root().join("regress-full")));
    let rows = harness::par_map(&files, |path| {
        let text = fs::read_to_string(path).unwrap();
        let ctx = MemoryContext::new("facts_m8b");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { return (0, Vec::new()) };
        let arena = Arena::new(1 << 20);
        let Ok(g) = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { return (0, Vec::new()) };
        let (mut n, mut bad) = (0, Vec::new());
        for (i, (slots, lives), inners) in carriers(&g) {
            let mut seen = slots.clone();
            seen.sort_unstable();
            seen.dedup();
            if seen.len() != slots.len() {
                bad.push(format!("{}: p{i} reads a slot twice: {slots:?}", path.display()));
            }
            for (s, l) in inners {
                for (j, slot) in s.iter().enumerate() {
                    n += 1;
                    match slots.iter().position(|x| x == slot) {
                        None => bad.push(format!("{}: p{i} does not read slot{slot}, which an inner reads", path.display())),
                        Some(k) if lives.len() == slots.len() && l.len() == s.len() && lives[k] != l[j] => {
                            bad.push(format!("{}: p{i} reads slot{slot} from p{}, its inner from p{}", path.display(), lives[k], l[j]))
                        }
                        Some(_) => {}
                    }
                }
            }
        }
        (n, bad)
    });
    let n: usize = rows.iter().map(|r| r.0).sum();
    eprintln!("inner slot reads checked: {n}");
    assert!(n > 400, "the corpus's embedded inners read {n} slots (443 in the 09-27 corpus)");
    let bad: Vec<&String> = rows.iter().flat_map(|r| r.1.iter()).collect();
    assert!(bad.is_empty(), "{bad:#?}");
}

/// PARAM_EXEC ids an expression reads (not below a nested SubPlan's own test).
fn exec_params(n: types_nodes::Node<'_>, out: &mut Vec<i32>) {
    if let Some(p) = n.as_param().filter(|p| p.paramkind == types_nodes::primnodes::ParamKind::PARAM_EXEC) {
        out.push(p.paramid);
    }
    gravity_decompose::expr::kids(n, &mut |c| exec_params(c, out));
}

/// M8-SUB (B2's nest.subplan): `SubPlanDesc::ids` is PostgreSQL's `paramIds`, the subplan's output columns as its test reads
/// them. Pinned on `b = ANY (SELECT y ..)` (column 1 = $exec1, the test `op96(v1.2, $exec1)`); over every fixture, an ANY /
/// ALL / ROWCOMPARE SubPlan's test reads every id and has at least one, and an EXISTS / EXPR / ARRAY one has none.
#[test]
fn a_subplans_ids_are_the_output_params_its_test_reads() {
    let ids = with_graph("SELECT a, b = ANY (SELECT y FROM u WHERE u.x = t.a) FROM t WHERE a < 3", |g| {
        pipes(g).iter().flat_map(|p| p.ops.iter()).find_map(|op| if let Op::SubPlan(d) = op { Some(d.ids.iter().collect::<Vec<i32>>()) } else { None })
    });
    assert_eq!(ids, Some(vec![1]));
    let mut files = harness::fixtures_in(&harness::plans_root());
    files.extend(harness::fixtures_in(&harness::plans_root().join("regress-full")));
    let rows = harness::par_map(&files, |path| {
        let text = fs::read_to_string(path).unwrap();
        let ctx = MemoryContext::new("facts_m8b");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { return (0, Vec::new()) };
        let arena = Arena::new(1 << 20);
        let Ok(g) = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { return (0, Vec::new()) };
        let (mut n, mut bad) = (0, Vec::new());
        for op in pipes(&g).iter().flat_map(|p| p.ops.iter()) {
            let Op::SubPlan(d) = op else { continue };
            let ids: Vec<i32> = d.ids.iter().collect();
            let mut read = Vec::new();
            if let Some(t) = d.test {
                exec_params(t, &mut read);
            }
            let compares = matches!(d.link, SubLink::Any | SubLink::All | SubLink::RowCompare);
            n += compares as usize;
            if compares && (ids.is_empty() || ids.iter().any(|i| !read.contains(i))) {
                bad.push(format!("{}: subplan {} {:?} ids {ids:?}, its test reads {read:?}", path.display(), d.plan_id, d.link));
            }
            if matches!(d.link, SubLink::Exists | SubLink::Expr | SubLink::Array) && !ids.is_empty() {
                bad.push(format!("{}: subplan {} {:?} has ids {ids:?}", path.display(), d.plan_id, d.link));
            }
        }
        (n, bad)
    });
    let n: usize = rows.iter().map(|r| r.0).sum();
    assert!(n > 20, "ANY / ALL / ROWCOMPARE SubPlans over the corpus: {n}");
    let bad: Vec<&String> = rows.iter().flat_map(|r| r.1.iter()).collect();
    assert!(bad.is_empty(), "{bad:#?}");
}

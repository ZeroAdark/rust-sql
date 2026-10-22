//! D2113 (GUARD-G1): the shared user-function guard over the guardx plan fixtures (tests/fixtures/guard/guardx.sql: the
//! probe kit's K1 / K2 / K3 / K6 forms, the S1 box key, the controls; captured by `scripts/gravity-capture-plans.sh
//! guardx`), through the test-only admission point `admit_all`: every plan node of the statement (subplans included), each
//! expression node of their target lists and quals, and the statement's prune infos, each through
//! `guard::user_fn::support_of`. A `/* G.err.<form> */` statement answers `Err(USER_FN)`, a `/* G.ok.<form> */` one `Ok`.
//! The catalog is PostgreSQL 18.6's .dat files (pg_amop / pg_amproc / pg_opfamily / pg_operator / pg_proc / pg_type) plus
//! the fixture's own `-- op` lines (each ordering operator's btree families); a user family or operator (oid >= 16384)
//! answers itself as the function, a user function: the kit's user opclasses all carry a plpgsql FUNCTION 1 / 2. So the
//! `osa_` / `hsa_` forms (a user = beside a built-in FUNCTION 1) pin the statement's answer here; the unit tests and the
//! real-catalog DEV trace pin that the eqop alone decides it.
use gravity_lowerv4_it as common;

use std::collections::HashMap;
use std::path::PathBuf;

use gravity_decompose::rerun::each_plan_node;
use gravity_decompose::{NoFacts, PipelineGraph};
use gravity_expr::catalog::{DomainFacts, FuncFacts, IoFacts, Support, TextFacts, TypeFacts};
use gravity_expr::Catalog;
use gravity_fn::{Shim, Tier};
use gravity_lowerv4::graph::Gx;
use gravity_lowerv4::effects::{support_of, DISPATCH_OK, USER_FN};
use gravity_lowerv4::admit::refusal::ShapeWhy;
use gravity_program::Arena;
use mcx::MemoryContext;
use types_core::Oid;
use types_error::PgResult;
use types_nodes::plannodes::PlannedStmt;
use types_nodes::Node;

const USER: Oid = 16384;

type Member = (Oid, Oid, Oid, i16, bool);

/// PG 18.6's btree / hash operator families, read the way lsyscache reads them.
struct Families {
    /// pg_amop by operator: (family, lefttype, righttype, strategy, hash), lowest family first (the catcache list order)
    amop: HashMap<Oid, Vec<Member>>,
    /// pg_amop by (family, lefttype, righttype, strategy)
    member: HashMap<(Oid, Oid, Oid, i16), Oid>,
    /// pg_amproc by (family, lefttype, righttype, procnum)
    amproc: HashMap<(Oid, Oid, Oid, i16), Oid>,
}

impl Families {
    fn get() -> &'static Families {
        static F: std::sync::OnceLock<Families> = std::sync::OnceLock::new();
        F.get_or_init(Families::load)
    }

    fn load() -> Families {
        let oid = |e: &HashMap<String, String>| e.get("oid").and_then(|o| o.parse::<Oid>().ok());
        let mut types: HashMap<String, Oid> = HashMap::new();
        for e in common::dat("pg_type.dat") {
            let (Some(n), Some(o)) = (e.get("typname"), oid(&e)) else { continue };
            types.insert(n.clone(), o);
        }
        let t = |n: &str| types.get(n).copied().unwrap_or(0);
        let mut procs: HashMap<String, Oid> = HashMap::new();
        for e in common::dat("pg_proc.dat") {
            let (Some(n), Some(o)) = (e.get("proname"), oid(&e)) else { continue };
            procs.entry(n.clone()).or_insert(o);
            let args: Vec<&str> = e.get("proargtypes").map_or(Vec::new(), |a| a.split_whitespace().collect());
            procs.insert(format!("{n}({})", args.join(",")), o);
        }
        let mut ops: HashMap<String, Oid> = HashMap::new();
        for e in common::dat("pg_operator.dat") {
            let (Some(n), Some(o), Some(l), Some(r)) = (e.get("oprname"), oid(&e), e.get("oprleft"), e.get("oprright")) else { continue };
            ops.insert(format!("{n}({l},{r})"), o);
        }
        let fams: HashMap<String, Oid> = common::dat("pg_opfamily.dat").iter().filter_map(|e| Some((format!("{}/{}", e.get("opfmethod")?, e.get("opfname")?), oid(e)?))).collect();
        let (mut amop, mut member, mut amproc) = (HashMap::new(), HashMap::new(), HashMap::new());
        for e in common::dat("pg_amop.dat") {
            let m = e.get("amopmethod").map_or("", String::as_str);
            let (Some(&f), Some(&op)) = (e.get("amopfamily").and_then(|f| fams.get(f)), e.get("amopopr").and_then(|o| ops.get(o))) else { continue };
            if m != "btree" && m != "hash" {
                continue;
            }
            let (l, r, s) = (t(&e["amoplefttype"]), t(&e["amoprighttype"]), e["amopstrategy"].parse::<i16>().expect("amopstrategy"));
            amop.entry(op).or_insert_with(Vec::new).push((f, l, r, s, m == "hash"));
            member.insert((f, l, r, s), op);
        }
        for e in common::dat("pg_amproc.dat") {
            let (Some(&f), Some(&p)) = (e.get("amprocfamily").and_then(|f| fams.get(f)), e.get("amproc").and_then(|p| procs.get(p))) else { continue };
            amproc.insert((f, t(&e["amproclefttype"]), t(&e["amprocrighttype"]), e["amprocnum"].parse::<i16>().expect("amprocnum")), p);
        }
        amop.values_mut().for_each(|v: &mut Vec<Member>| v.sort());
        Families { amop, member, amproc }
    }
}

/// The rig catalog (tests/common) plus `support` from the families and the fixture's `-- op` lines.
struct Rig {
    /// the fixture's btree ordering operators: opno -> families, lowest first
    ops: HashMap<Oid, Vec<Oid>>,
}

impl Rig {
    fn of(text: &str) -> Rig {
        let mut ops: HashMap<Oid, Vec<Oid>> = HashMap::new();
        for l in text.lines().filter_map(|l| l.strip_prefix("-- op ")) {
            let mut w = l.split_whitespace();
            let op = w.next().and_then(|o| o.parse().ok()).expect("-- op <opno>");
            let fam = w.find_map(|x| x.strip_prefix("family=")).and_then(|f| f.parse().ok()).expect("-- op family=");
            ops.entry(op).or_default().push(fam);
        }
        ops.values_mut().for_each(|v| v.sort());
        Rig { ops }
    }

    /// get_ordering_op_properties: the lowest btree family `op` orders by (strategy 1 / 5, one input type), and that type.
    fn sort_family(&self, op: Oid) -> Option<(Oid, Oid)> {
        let dat = Families::get().amop.get(&op).into_iter().flatten().find(|m| !m.4 && matches!(m.3, 1 | 5) && m.1 == m.2).map(|m| (m.0, m.1));
        let fixture = self.ops.get(&op).and_then(|fs| fs.first()).map(|&fam| (fam, dat.map_or(0, |d| d.1)));
        match (fixture, dat) {
            (Some(x), Some(d)) => Some(if x.0 < d.0 { x } else { d }),
            (x, d) => x.or(d).or((op >= USER).then_some((op, 0))),
        }
    }

    /// The hash family `op` is the equality member of: (family, lefttype, righttype).
    fn hash_member(&self, op: Oid) -> Option<(Oid, Oid, Oid)> {
        Families::get().amop.get(&op).into_iter().flatten().find(|m| m.4 && m.3 == 1).map(|m| (m.0, m.1, m.2))
    }
}

impl<'m> Catalog<'m> for Rig {
    fn support(&self, q: Support) -> PgResult<Oid> {
        let f = Families::get();
        let proc = |fam: Oid, l: Oid, r: Oid, n: i16| if fam >= USER { fam } else { f.amproc.get(&(fam, l, r, n)).copied().unwrap_or(0) };
        let eq_of = |fam: Oid, t: Oid| if fam >= USER { fam } else { f.member.get(&(fam, t, t, 3)).copied().unwrap_or(0) };
        Ok(match q {
            Support::Sort { op, num } => self.sort_family(op).map_or(0, |(fam, t)| proc(fam, t, t, num)),
            Support::Merge { op, family, num } => f.amop.get(&op).into_iter().flatten().find(|m| m.0 == family).map_or_else(|| proc(family, 0, 0, num), |m| proc(family, m.1, m.2, num)),
            Support::Hash { op, rhs } => match self.hash_member(op) {
                Some((fam, l, r)) => proc(fam, if rhs { r } else { l }, if rhs { r } else { l }, 1),
                None if op >= USER => op,
                None => 0,
            },
            Support::EqOfSort(op) => self.sort_family(op).map_or(0, |(fam, t)| eq_of(fam, t)),
            Support::HashRhsEq(op) => self.hash_member(op).map_or(0, |(fam, l, r)| if l == r { op } else { f.member.get(&(fam, r, r, 1)).copied().unwrap_or(0) }),
            _ => 0,
        })
    }
    fn func(&self, f: Oid) -> PgResult<FuncFacts> {
        Catalog::<'m>::func(common::RigCatalog::get(), f)
    }
    fn opcode(&self, o: Oid) -> PgResult<Oid> {
        Catalog::<'m>::opcode(common::RigCatalog::get(), o)
    }
    fn op_input_types(&self, o: Oid) -> PgResult<(Oid, Oid)> {
        Catalog::<'m>::op_input_types(common::RigCatalog::get(), o)
    }
    fn btree_cmp_proc(&self, f: Oid, l: Oid, r: Oid) -> PgResult<Oid> {
        Catalog::<'m>::btree_cmp_proc(common::RigCatalog::get(), f, l, r)
    }
    fn opclass(&self, o: Oid) -> PgResult<(Oid, Oid)> {
        Catalog::<'m>::opclass(common::RigCatalog::get(), o)
    }
    fn type_facts(&self, t: Oid) -> PgResult<TypeFacts> {
        Catalog::<'m>::type_facts(common::RigCatalog::get(), t)
    }
    fn type_cmp_proc(&self, t: Oid) -> PgResult<Oid> {
        Catalog::<'m>::type_cmp_proc(common::RigCatalog::get(), t)
    }
    fn base_type(&self, t: Oid, m: &mut i32) -> PgResult<Oid> {
        Catalog::<'m>::base_type(common::RigCatalog::get(), t, m)
    }
    fn domain(&self, t: Oid) -> PgResult<DomainFacts<'m>> {
        Catalog::<'m>::domain(common::RigCatalog::get(), t)
    }
    fn io(&self, t: Oid) -> PgResult<IoFacts> {
        Catalog::<'m>::io(common::RigCatalog::get(), t)
    }
    fn text(&self, c: Oid) -> PgResult<TextFacts> {
        Catalog::<'m>::text(common::RigCatalog::get(), c)
    }
    fn tier(&self, f: Oid) -> Tier {
        gravity_fn::tier_of(f)
    }
    fn shim(&self, f: Oid) -> Option<&'static Shim> {
        gravity_fn::shim_of(f)
    }
}

/// The test-only admission point: the guard over every plan node, each expression node of their target lists and quals,
/// and the statement's prune infos; the first refusal.
fn admit_all<'p>(gx: &Gx<'_, '_>, stmt: &'p PlannedStmt<'p>) -> Result<(), ShapeWhy> {
    let mut r = Ok(());
    let mut visit = |n: Node<'_>| {
        if r.is_ok() {
            r = support_of(gx, n);
        }
    };
    each_plan_node(stmt, &mut |p| {
        visit(p);
        for x in p.as_plan().into_iter().flat_map(|pl| pl.targetlist.iter().chain(pl.qual.iter())) {
            gravity_decompose::expr::walk(x, &mut |e| {
                visit(e);
                true
            });
        }
    });
    for p in stmt.partPruneInfos.iter() {
        visit(p);
    }
    r
}

fn manifest() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR").map(|d| std::path::Path::new(&d).with_file_name("lowerv4")).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)")
}

/// Each tagged top-level guardx fixture's verdict: `(tag, Ok / Err)` (a plpgsql body's nested plan is no statement here).
fn verdicts() -> Vec<(String, Result<(), ShapeWhy>)> {
    common::RigCatalog::install_seams();
    let mut out = Vec::new();
    for (name, text) in common::files(&manifest().join("tests/fixtures/guard"), "plan") {
        let tag = text.lines().find_map(|l| l.strip_prefix("-- sql: ")).and_then(|s| s.split("/* G.").nth(1)).and_then(|s| s.split(" */").next());
        let (Some(tag), false) = (tag, text.lines().any(|l| l.starts_with("-- nested:"))) else { continue };
        let (ctx, arena) = (MemoryContext::new("guardx"), Arena::new(1 << 16));
        let fx = gravity_decompose::corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let graph = PipelineGraph::new(arena.handle());
        let rig = Rig::of(&text);
        let plan_rig = mcx::MemoryContext::new("GravityPlanRig");
        let gx = Gx { graph: &graph, facts: &NoFacts, catalog: &rig, over: None, mcx: plan_rig.mcx() };
        out.push((tag.to_owned(), admit_all(&gx, fx.stmt)));
    }
    out
}

/// K1 / K2 / K6 support, K3 Prune and the S1 box key's sorted SetOp / MergeAppend / Incremental Sort answer USER_FN by
/// name, as do an int4range, int4[] and real[] key while DISPATCH_OK is false (S4 / D2093: their `err.dispatch_` tags
/// answer Ok once GUARD-DISPATCH-OK sets it); the controls (enum, domain, built-in int4 / text keys, int4's < beside a
/// user family, a hashed SubPlan, built-in pruning, mode() over int4) answer Ok. An ordered-set / hypothetical-set
/// aggregate's user eqop answers USER_FN.
#[test]
fn guardx_fixtures_answer_by_the_guards_name() {
    let vs = verdicts();
    let want = [
        "err.k1_sort", "err.k1_aggorder", "err.k1_merge", "err.k1_box", "err.s1_setop", "err.s1_mergeappend", "err.s1_incsort", "err.k2_sort", "err.k2_hashjoin",
        "err.k6_group", "err.dispatch_int4range", "err.dispatch_int4arr", "err.dispatch_realarr", "err.k3_prune", "err.k3_prune_hash", "ok.int4_merge", "ok.int4_sort",
        "ok.text_hashagg", "ok.text_sort", "ok.enum_group", "ok.domain_sort", "ok.int4_hashjoin", "ok.int4_setop", "ok.int4_window", "ok.hashed_subplan",
        "ok.prune_builtin", "ok.int4_distinct_agg", "err.osa_mode_eqop", "err.hsa_dense_rank_eqop", "ok.osa_mode_int4",
    ];
    let mut bad = Vec::new();
    for w in want {
        let got: Vec<_> = vs.iter().filter(|v| v.0 == w).map(|v| v.1).collect();
        let expect = if w.starts_with("ok.") || DISPATCH_OK && w.starts_with("err.dispatch_") { Ok(()) } else { Err(USER_FN) };
        if got.is_empty() {
            bad.push(format!("{w}: no fixture"));
        } else if got.iter().any(|g| *g != expect) {
            bad.push(format!("{w}: {got:?}, want {expect:?}"));
        }
    }
    assert!(bad.is_empty(), "{bad:#?}");
    assert_eq!(vs.len(), want.len(), "one fixture per tagged statement: {:?}", vs.iter().map(|v| &v.0).collect::<Vec<_>>());
}

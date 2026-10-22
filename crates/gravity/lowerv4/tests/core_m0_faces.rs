//! What M0 froze (plan §5.3), scoped to what is still unbuilt once later milestones register rows: no kind or road
//! declared at M0 is built; every declared kind (99) and road (126)
//! not yet built refuses `NotBuilt` with its milestone; every census plan fixture decompose takes lowers to a named
//! refusal at census/serve.tsv's milestone, or serves when that milestone is built; `applies.rs` agrees with
//! `v4plan.py pred()` on the census (`census/applies.tsv`); the rig's comparator agrees with multiset.py on the shared
//! fixtures. The census files, v4plan.py and multiset.py are part of the tree: a missing one fails (a removal never
//! disables an assert silently).
use gravity_lowerv4_it as common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use gravity_lowerv4::applies::{self, Prop, PropSet};
use gravity_lowerv4::face::kinds::{KindId, KindSet, KINDS};
use gravity_lowerv4::face::strategy::{Strategy, StrategySet, STRATEGIES, WRITE_LITERALS, WRITE_ROUTE};
use gravity_lowerv4::{catch, ops as registry, Milestone, Reason, Variant};

/// Repo file `rel`'s text; a missing file fails the test that reads it.
fn need(rel: &str) -> String {
    let p = manifest_dir().join("../../..").join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{rel}: {e} (part of the tree since M0; a removal disables this assert)"))
}

/// The latest milestone with a built kind (a strategy has none): census fixtures first served at or before it may serve.
fn frontier() -> Milestone {
    let kinds = registry::built_kinds();
    let built = KINDS.iter().filter(|d| kinds.has(d.id)).map(|d| d.milestone);
    built.max().unwrap_or(Milestone::M0)
}

#[test]
fn m0_builds_nothing() {
    let kinds = registry::built_kinds();
    for k in kinds.iter() {
        assert!(k.decl().milestone > Milestone::M0, "{} is built, declared at M0", k.name());
    }
    assert_eq!((KINDS.len(), Strategy::COUNT, STRATEGIES.len()), (104, 166, 165), "the plan's declared kinds, road bits and roads (D4941: run.measure deleted, bit 126 free; D-06c: twopass.count; twopass.marks deleted, join.dedup in its bit; code.ties deleted, text.substr.ascii in its bit; join.sorted and nest.key.wide deleted, join.payload_fp and text.set.bpchar in their bits; distinct.identity deleted, join.drain.fp in its bit; genreg P6: join.payload_fp, join.drain.fp; D4794: text.set.bpchar; D4822: text.substr.ascii; D4821: join.dedup; brfix: mask.rows; D3336 A1a: win.partfill; D4723: agg.num_cte; D3336 A2 (iii): cte.wholefill; D4567 (a): agg.num_div; D4620: agg.gsets.cascade; D4628: spool.seal; textkeys: text.lean, text.set, key.canon; D4676: group.hashchain; D3227 (b): nest.key.eq; D3168: join.probe.staged; D3323: join.rowfilter.single, join.rowfilter.cost; D4567: agg.num_scale; D4565: agg.stat_reg; D3233: case.select; D3227: nest.keep.semi, nest.keep.material; D4569: append.nest, join.rtfilter.arms, drain.spool.wide; D4497: nest.keep.initplan, nest.keep.subquery; D3336: merge.wideemit, emit.parmerge; D3324: key.carry; D4585: scan.mergefree.build; D3256: cte.claims, cte.batch; D3024 (b): emit.marks; D4568: scan.mergefree; D3245: join.rtfilter.multi; D3194: join.rowfilter; D3148: join.domain; D3024: num.aggcmp; the stub roads heap.checksum, sort.elide, gen.invariant, cte.fold and write.multi deleted; V4R-53: class.num_wide; V4R-106: code.sparse, code.front_light; V4R-120: nest.catalog_left; V4R-142: nest.catalog; V4R-183: keyed.distinct_multi; V4R-379: runs.group_distinct; V4R-386: rep.hosted.order.fixed; V4R-472: num.lean; V4R-488: num.partial; V4R-500: num.pair; V4R-503: num.share; V4R-504: num.narrow)");
}

#[test]
fn every_kind_refuses_with_its_milestone() {
    let built = registry::built_kinds();
    for d in KINDS {
        assert!(d.milestone > Milestone::M0, "{} is declared at M0, which serves nothing", d.name);
        assert_eq!(KindId::from_name(d.name), Some(d.id));
        if built.has(d.id) {
            continue;
        }
        let r = catch(3, || { let _ = gravity_lowerv4::frame::iface::need_kind::<()>(&3, d.id); }).expect_err("an unbuilt kind refuses");
        assert_eq!(r.reason, Reason::NotBuilt { hook: d.face.name(), variant: Variant::Kind(d.id), milestone: d.milestone }, "{}", d.name);
        assert_eq!((r.node, r.to_seam().what), (3, format!("NotBuilt {}: {} {}", d.milestone.name(), d.face.name(), d.name).as_str()));
    }
}

#[test]
fn every_unbuilt_strategy_refuses_a_reader_by_name() {
    let built = registry::built_strategies();
    for &d in STRATEGIES {
        assert_eq!(Strategy::from_name(d.name()), Some(d));
        if built.has(d) {
            continue;
        }
        let r = catch(5, || { let _ = gravity_lowerv4::frame::iface::need_strategy::<()>(&5, d); }).expect_err("an unbuilt road refuses");
        assert_eq!(r.reason, Variant::Strategy(d).not_built(), "{}", d.name());
        assert_eq!(r.to_seam().what, format!("NotBuilt M11: strategy {}", d.name()).as_str());
    }
}

/// The census plan fixtures through decompose and `lower_graph`: every one decompose takes refuses by name (`NotBuilt` of a
/// kind or road declared after M0, or a typed reason) or serves, none is a Bug, each refusal's milestone equals
/// census/serve.tsv's first-serve milestone (main 09-24), and only a fixture whose first-serve milestone has a table
/// serves. To stderr: the refusal histogram.
/// Admission names the latest unbuilt kind or road, so the two agree when props.rs and the census agree: EARLY = props.rs
/// misses a kind or token the census counts; LATE = props.rs needs one the census does not (the statement would miss the
/// census's milestone bar).
#[test]
fn the_census_refuses_by_name() {
    let first: BTreeMap<String, (Milestone, String)> = need("crates/gravity/lowerv4/testdata/gravity/lowerv4/census/serve.tsv").lines().skip(1).filter_map(|l| {
        let f: Vec<&str> = l.split('\t').collect();
        Some((f.first()?.to_string(), (Milestone::from_name(f.get(1)?)?, f.get(3).unwrap_or(&"").to_string())))
    }).collect();
    let (mut by, mut vs, mut early, top) = (BTreeMap::<String, usize>::new(), [0usize; 3], Vec::new(), frontier());
    let fixtures = common::files(&common::plans_dir(), "plan");
    for (name, text) in &fixtures {
        match common::lower_fixture(text) {
            None => continue,
            Some(Ok(_)) => {
                match first.get(name) {
                    Some((c, _)) if *c <= top => *by.entry(format!("served (first serves at {})", c.name())).or_default() += 1,
                    Some((c, needs)) => early.push(format!("EARLY {name}: serves (census first serves at {}: {needs})", c.name())),
                    None => early.push(format!("EARLY {name}: serves with no census/serve.tsv row")),
                }
            }
            Some(Err(r)) => {
                assert_eq!(r.module, "lowerv4", "{name}: {r}");
                assert!(!r.what.starts_with("Bug") && !r.what.starts_with("NotBuilt M0"), "{name}: {r}");
                *by.entry(r.what.to_owned()).or_default() += 1;
                let m = r.what.strip_prefix("NotBuilt ").and_then(|t| t.split(':').next()).and_then(Milestone::from_name);
                if let (Some(m), Some((c, needs))) = (m, first.get(name)) {
                    vs[(m.cmp(c) as i8 + 1) as usize] += 1;
                    // an earlier milestone's wall once the census's own needs are built kinds or strategies (an unbuilt
                    // strategy never walls): the later milestone built first (M8-SUB B1's nest.initplan, tpch-022's M7 join.right)
                    let met = |n: &str| KindId::from_name(n).map_or_else(|| Strategy::from_name(n).is_some(), |k| registry::built_kinds().has(k));
                    if m != *c && !(m < *c && needs.split(' ').filter(|n| !n.is_empty()).all(met)) {
                        let tag = if m < *c { "EARLY" } else { "LATE" };
                        early.push(format!("{tag} {name}: refuses {} (census first serves at {}: {needs})", r.what, c.name()));
                    }
                }
            }
        }
    }
    let lowered: usize = by.values().sum();
    assert!(lowered * 2 > fixtures.len(), "{lowered} of {} fixtures reached lower_graph", fixtures.len());
    let mut rows: Vec<_> = by.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    eprintln!("{lowered} of {} census fixtures served or refused by name; top outcomes:", fixtures.len());
    rows.iter().take(15).for_each(|(what, n)| eprintln!("  {n:5}  {what}"));
    eprintln!("vs census/serve.tsv: {} earlier, {} same, {} later milestone", vs[0], vs[1], vs[2]);
    assert!(early.is_empty(), "{} refusals at another milestone than census/serve.tsv:\n{}", early.len(), early.join("\n"));
}

/// `props.rs` against the census (main 09-24): per plan fixture, the kinds and properties admission reads off the graph
/// equal the ones census/fixtures.tsv has for it (v4plan.py, from the same golden graph), and every fixture decompose takes
/// has a row. A census-only kind would admit a statement before a road it needs is built; a props.rs-only kind holds it
/// past the census's milestone. `LOWERV4_CENSUS=write` (as `census_parity_with_v4plan_pred`'s) drops a prop no longer
/// declared from the rows' props column; nothing else moves.
#[test]
fn props_against_the_census() {
    let text = need("crates/gravity/lowerv4/testdata/gravity/lowerv4/census/fixtures.tsv");
    if std::env::var("LOWERV4_CENSUS").is_ok_and(|v| v == "write") {
        let mut out = String::new();
        for (i, line) in text.lines().enumerate() {
            let mut f: Vec<String> = line.split('\t').map(str::to_owned).collect();
            if i > 0 && f.len() >= 3 {
                f[2] = f[2].split(',').filter(|n| n.trim().is_empty() || Prop::from_name(n.trim()).is_some()).collect::<Vec<_>>().join(",");
            }
            out.push_str(&f.join("\t"));
            out.push('\n');
        }
        std::fs::write(manifest_dir().join("../../../crates/gravity/lowerv4/testdata/gravity/lowerv4/census/fixtures.tsv"), out).expect("write fixtures.tsv");
        return;
    }
    let set = |s: &str| s.split(',').map(str::trim).filter(|t| !t.is_empty()).map(str::to_owned).collect::<BTreeSet<_>>();
    let want: BTreeMap<&str, (BTreeSet<String>, BTreeSet<String>)> =
        text.lines().skip(1).filter_map(|l| { let f: Vec<&str> = l.split('\t').collect(); Some((f[0], (set(f.get(1).copied()?), set(f.get(2).copied()?)))) }).collect();
    // UNSEEN tokens (props.rs sets them for every graph: no graph fact decides them yet) are left out of both sides
    let unseen: BTreeSet<String> = gravity_lowerv4::props::UNSEEN.iter().map(|t| t.name().to_owned()).collect();
    let (mut same, mut diff, mut toks) = (0, Vec::new(), BTreeMap::<String, usize>::new());
    for (name, text) in &common::files(&common::plans_dir(), "plan") {
        let Some((k, p)) = common::props_fixture(text) else { continue };
        let Some((wk, wp)) = want.get(name.as_str()) else {
            diff.push(format!("{name}: no census/fixtures.tsv row"));
            continue;
        };
        let side = |a: &BTreeSet<String>, b: &BTreeSet<String>, tag: &str| a.difference(b).map(|x| format!("{tag}{x}")).collect::<Vec<_>>();
        let (p, wp) = (&p - &unseen, wp - &unseen);
        let d = [side(&k, wk, "+"), side(wk, &k, "-"), side(&p, &wp, "+prop:"), side(&wp, &p, "-prop:")].concat();
        if d.is_empty() {
            same += 1;
        } else {
            d.iter().for_each(|t| *toks.entry(t.clone()).or_default() += 1);
            diff.push(format!("{name}: {}", d.join(" ")));
        }
    }
    let mut toks: Vec<_> = toks.into_iter().collect();
    toks.sort_by(|a, b| b.1.cmp(&a.1));
    eprintln!("props.rs vs census/fixtures.tsv: {same} agree, {} differ (+ props.rs only, - census only); top differences:", diff.len());
    toks.iter().take(25).for_each(|(t, n)| eprintln!("  {n:5}  {t}"));
    diff.iter().for_each(|d| eprintln!("  DIFF {d}"));
    assert!(diff.is_empty(), "{} fixtures differ from census/fixtures.tsv (see DIFF lines)", diff.len());
}

/// `applies.rs` against census/applies.tsv, the roads v4plan.py's `pred()` wrote at M0 (ruling 6, frozen there; v4plan.py
/// and its model are deleted in v5): per distinct (kinds, props) of the census, `strategies_for` gives exactly those roads. Every
/// name must be a declared kind, prop or road. `LOWERV4_CENSUS=write` re-records the census rows after a premise change: a
/// prop no longer declared leaves its column and the roads column is `strategies_for`'s, both in name order; nothing else moves.
#[test]
fn census_parity_with_v4plan_pred() {
    let text = need("crates/gravity/lowerv4/testdata/gravity/lowerv4/census/applies.tsv");
    let list = |s: &str| s.split(',').map(str::trim).filter(|t| !t.is_empty()).map(str::to_owned).collect::<Vec<_>>();
    if std::env::var("LOWERV4_CENSUS").is_ok_and(|v| v == "write") {
        let mut out = String::new();
        for (i, line) in text.lines().enumerate() {
            let f: Vec<&str> = line.split('\t').collect();
            if i == 0 || line.trim().is_empty() || f.len() != 4 {
                out.push_str(line);
                out.push('\n');
                continue;
            }
            let (mut k, mut p) = (KindSet::EMPTY, PropSet::default());
            list(f[1]).iter().for_each(|n| k.add(KindId::from_name(n).unwrap_or_else(|| panic!("{}: unknown kind {n}", f[0]))));
            let props: Vec<String> = list(f[2]).into_iter().filter(|n| Prop::from_name(n).is_some()).collect();
            props.iter().for_each(|n| p.add(Prop::from_name(n).expect("declared")));
            let mut roads: Vec<&str> = applies::strategies_for(k, p).iter().map(|r| r.name()).collect();
            roads.sort_unstable();
            out.push_str(&format!("{}\t{}\t{}\t{}\n", f[0], f[1], props.join(","), roads.join(",")));
        }
        std::fs::write(manifest_dir().join("../../../crates/gravity/lowerv4/testdata/gravity/lowerv4/census/applies.tsv"), out).expect("write applies.tsv");
        return;
    }
    let (mut rows, mut bad) = (0, Vec::new());
    for line in text.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        let f: Vec<&str> = line.split('\t').collect();
        assert_eq!(f.len(), 4, "applies.tsv row {line:?}");
        let (mut k, mut p, mut want) = (KindSet::EMPTY, PropSet::default(), StrategySet::EMPTY);
        list(f[1]).iter().for_each(|n| k.add(KindId::from_name(n).unwrap_or_else(|| panic!("{}: unknown kind {n}", f[0]))));
        list(f[2]).iter().for_each(|n| p.add(Prop::from_name(n).unwrap_or_else(|| panic!("{}: unknown prop {n}", f[0]))));
        list(f[3]).iter().for_each(|n| want.add(Strategy::from_name(n).unwrap_or_else(|| panic!("{}: unknown road {n}", f[0]))));
        let got = applies::strategies_for(k, p);
        if got != want {
            let names = |s: StrategySet| s.iter().map(|r| r.name()).collect::<Vec<_>>().join(",");
            bad.push(format!("{}: applies.rs only [{}], applies.tsv only [{}]", f[0], names(got.minus(want)), names(want.minus(got))));
        }
        rows += 1;
    }
    assert!(rows > 0, "applies.tsv has no rows");
    assert!(bad.is_empty(), "{} of {rows} census (K, P) rows disagree:\n{}", bad.len(), bad.join("\n"));
}

/// The rig's comparator against multiset.py's fixtures (plan §5.6): every class and verdict as the case states.
#[test]
fn comparator_agrees_with_multiset_py() {
    need("scripts/gravity-gate/multiset.py");
    let n = common::selftest(&common::comparator_fixtures());
    assert!(n >= 4, "{n} comparator cases: the plan's four classes each need one");
}

/// D3927 (user ruling 2026-10-11, main 13:26:21Z): multiset.py's float smoke for round(<float aggregate>::numeric, n), as far
/// as this mirror reads it (it has no schema typing).
#[test]
fn d3927_round_numeric_float_aggregate() {
    use common::multiset::{dec_within, float_cols, F};
    let forms = "SELECT round(avg(x::float8)::numeric), round(sum(x::float8)::numeric, -2), round(CAST(stddev(f::float8) AS numeric), 3), \
                 round(variance(f::float8)::numeric(30, 6), 4) v, round(sum(f::float4) FILTER (WHERE f > 0)::decimal, 1) FROM r";
    let want: BTreeMap<usize, F> = [(0, F::R(0)), (1, F::R(-2)), (2, F::R(3)), (3, F::R(4)), (4, F::R(1))].into();
    assert_eq!(float_cols(forms, None, None), want);
    // these stay exact under the rule too: no float evidence, a call in the argument, a window, no numeric cast, a cast
    // outside the round, a float round, a non-literal n, the round inside an expression
    let exact = "SELECT round(variance(e_id)::numeric, 4), round(sum(abs(e_val))::numeric, 2), round(sum(e_val::float8) OVER ()::numeric, 2), \
                 round(sum(e_val::float8), 2), round(sum(e_val::float8)::numeric, 2)::text, round(sum(e_val::float8)::float8), \
                 round(sum(e_val::float8)::numeric, k), round(sum(e_val::float8)::numeric, 2) + 1 FROM ev";
    assert_eq!(float_cols(exact, None, None), BTreeMap::new());
    // one unit in the n-th decimal, exactly (multiset.py's Decimal compare); NaN and Infinity only equal themselves
    let within = |a: &str, ys: &[&str], n: i32| ys.iter().map(|y| dec_within(a, y, n)).collect::<Vec<_>>();
    assert_eq!(within("2.50", &["2.51", "2.49", "2.511", "2.489", "2.48", "NaN"], 2), [true, true, false, false, false, false]);
    assert_eq!(within("1200", &["1300", "1400", "1.3e3", "1100.0"], -2), [true, false, true, true]);
    assert_eq!(within("-0.01", &["0.00", "0.01", "-0.02", "-0.030"], 2), [true, false, true, false]);
    assert_eq!(within("135973297.5995", &["135973297.5994", "135973297.5993", "135973297.59955"], 4), [true, false, true]);
    assert_eq!(within("NaN", &["NaN", "Infinity", "0"], 2), [true, false, false]);
}

/// D4019 (main 2026-10-11T14:56:26Z, seed 17 rnd-4115): an ORDER BY key whose declared type is numeric groups its tie runs
/// by value, as multiset.py's num_value / numeric_type do (numeric_decl here); their d4019-*.case fixtures (once the pin carries them) go through
/// comparator_agrees_with_multiset_py. Self-contained, so it holds under either pin: the texts multiset.py's helpers were
/// probed with, and the peers swapped with and without declared types.
#[test]
fn d4019_numeric_key_runs_by_value() {
    use common::multiset::{compare_typed, num_value, numeric_decl, Class, Eqk, Outcome, Tol, Verdict};
    let texts = ["0.000", "0.00000000000000000000", "-0.0", "-0.50", "1.", "1.50", "100", "007.10", "NaN", "Infinity", "-Infinity", "1.2.3", ".5", "-", "", "1e5", "+1"];
    assert_eq!(texts.map(num_value), ["0", "0", "0", "-0.5", "1", "1.5", "100", "7.1", "NaN", "Infinity", "-Infinity", "1.2.3", ".5", "-", "", "1e5", "+1"]);
    assert_eq!(["numeric", "NUMERIC(12, 3)", " decimal ", "numeric[]", "integer", "", "numericx"].map(numeric_decl), [true, true, true, false, false, false, false]);
    let rows = |r: &[(&str, &str)]| Outcome::Rows(r.iter().map(|(n, i)| vec![Some(n.to_string()), Some(i.to_string())]).collect());
    let pg = rows(&[("0.001", "1"), ("0.000", "2"), ("0.00000", "3")]);
    let (peers, non_peer) = (rows(&[("0.001", "1"), ("0.00000", "3"), ("0.000", "2")]), rows(&[("0.000", "2"), ("0.001", "1"), ("0.00000", "3")]));
    let (c, tol, eqk, ty) = (Class::Ordered(vec![0]), Tol::new(), Eqk::new(), ["numeric".to_owned(), "integer".to_owned()]);
    assert_eq!(compare_typed(&c, &pg, None, &peers, &tol, &eqk, Some(&ty)), Verdict::Match);
    assert_eq!(compare_typed(&c, &pg, None, &non_peer, &tol, &eqk, Some(&ty)), Verdict::MismatchOrder);
    assert_eq!(compare_typed(&c, &pg, None, &peers, &tol, &eqk, None), Verdict::MismatchOrder);
    assert_eq!(compare_typed(&c, &pg, None, &peers, &tol, &eqk, Some(&["text".to_owned(), "integer".to_owned()])), Verdict::MismatchOrder);
}

/// D4194 (main 2026-10-12T16:02:05Z GO; wfprove's same-shape-float-negzero / same-shape-interval cases, FAIL mismatch-order on
/// the pin 007d65c27a4 with PG and pgrust giving the same multiset): D4019's peer runs for the two other key types whose `=` is
/// coarser than their text, as multiset.py's peer_value will (the tools half follows, then the shared .case fixtures):
/// float4 / float8 -0 = 0 and interval by interval_cmp's span. Self-contained: wfprove's two cases inline, a LIMIT boundary
/// between interval peers, the non-peer controls, and the same rows with no declared type (strict text, as before).
#[test]
fn d4194_float_interval_key_peers() {
    use common::multiset::{compare_typed, interval_span, peer_decl, peer_value, Class, Eqk, Outcome, Peer, Tol, Verdict};
    let (f, i, n) = (Some(Peer::Float), Some(Peer::Interval), Some(Peer::Num));
    assert_eq!(
        ["double precision", "REAL", "float8", "float4", "float(24)", "interval", "interval(3)", "interval day to second", "numeric(5,2)",
         "interval[]", "double precision[]", "text", "integer", "jsonb"].map(peer_decl),
        [f, f, f, f, f, i, i, i, n, None, None, None, None, None]
    );
    assert_eq!(["-0", "0", "-0.5", "NaN", "-Infinity", "1e-05"].map(|v| peer_value(Peer::Float, v)), ["0", "0", "-0.5", "NaN", "-Infinity", "1e-05"]);
    let day = 86_400_000_000i128;
    assert_eq!(
        ["1 mon", "30 days", "1 day", "24:00:00", "1 year", "360 days", "-1 days +24:00:00", "00:00:00",
         "1 year 2 mons -3 days +04:05:06.5", "-00:00:00.000001", "100:00:00", "infinity", "@ 1 mon", "1 fortnight", "", "1:2",
         "1 mon 25:00:00.1234567", "1 mon 2"].map(interval_span),
        [Some(30 * day), Some(30 * day), Some(day), Some(day), Some(360 * day), Some(360 * day), Some(0), Some(0),
         Some(417 * day + 14_706_500_000), Some(-1), Some(100 * 3_600_000_000), None, None, None, None, None, None, None]
    );
    assert_eq!(peer_value(Peer::Interval, "infinity"), "infinity");
    let col = |r: &[&str]| Outcome::Rows(r.iter().map(|x| vec![Some(x.to_string())]).collect());
    let (c, tol, eqk) = (Class::Ordered(vec![0]), Tol::new(), Eqk::new());
    let (fl, iv) = (["double precision".to_owned()], ["interval".to_owned()]);
    // same-shape-float-negzero: -0 and 0 are peers, their order is free; -1 / -0 are not
    let (fpg, fgot) = (col(&["-1", "-0", "0", "-0", "1"]), col(&["-1", "0", "-0", "-0", "1"]));
    assert_eq!(compare_typed(&c, &fpg, None, &fgot, &tol, &eqk, Some(&fl)), Verdict::Match);
    assert_eq!(compare_typed(&c, &fpg, None, &fgot, &tol, &eqk, None), Verdict::MismatchOrder);
    assert_eq!(compare_typed(&c, &fpg, None, &col(&["-0", "-1", "0", "-0", "1"]), &tol, &eqk, Some(&fl)), Verdict::MismatchOrder);
    // same-shape-interval: '1 mon' and '30 days' are peers; '1 day' / '1 mon' are not
    let (ipg, igot) = (col(&["1 day", "1 mon", "30 days", "2 mons"]), col(&["1 day", "30 days", "1 mon", "2 mons"]));
    assert_eq!(compare_typed(&c, &ipg, None, &igot, &tol, &eqk, Some(&iv)), Verdict::Match);
    assert_eq!(compare_typed(&c, &ipg, None, &igot, &tol, &eqk, None), Verdict::MismatchOrder);
    assert_eq!(compare_typed(&c, &ipg, None, &col(&["1 mon", "1 day", "30 days", "2 mons"]), &tol, &eqk, Some(&iv)), Verdict::MismatchOrder);
    // LIMIT 2 cuts between the peers '1 mon' / '30 days': either is PG's answer (with PG's full result); the rows stay exact text
    let (lc, full) = (Class::Limited(vec![0]), [["1 day"], ["1 mon"], ["30 days"], ["2 mons"]].map(|r| vec![Some(r[0].to_owned())]));
    assert_eq!(compare_typed(&lc, &col(&["1 day", "1 mon"]), Some(&full[..]), &col(&["1 day", "30 days"]), &tol, &eqk, Some(&iv)), Verdict::Match);
    assert_eq!(compare_typed(&lc, &col(&["1 day", "1 mon"]), Some(&full[..]), &col(&["1 day", "30 days"]), &tol, &eqk, None), Verdict::Mismatch);
    assert_eq!(compare_typed(&lc, &col(&["1 day", "1 mon"]), Some(&full[..]), &col(&["1 day", "720:00:00"]), &tol, &eqk, Some(&iv)), Verdict::Mismatch);
}

/// DAY-ORDER addendum 9 (USER 2026-10-05 ~18:52Z, main 18:54:38Z): a two-argument covar_* / corr / regr_* compares within
/// V4R-2's float8 tolerance whatever its argument types, as multiset.py does: its two addendum-9 fixtures (schema/; the rule
/// needs no schema typing) and its float smoke lines (OVER, regr_count, sum over numeric, one argument stay exact; 1e-12
/// relative, not looser).
#[test]
fn addendum9_pair_aggregates_are_float8() {
    need("scripts/gravity-gate/multiset.py");
    let fx: Vec<_> = common::files(&common::comparator_fixtures().join("schema"), "case").into_iter()
        .filter(|(n, _)| n.ends_with("-addendum9") || n.ends_with("-agg_shop-0211")).collect();
    assert_eq!(fx.len(), 2, "multiset.py's addendum-9 fixtures: {:?}", fx.iter().map(|c| &c.0).collect::<Vec<_>>());
    common::selftest_cases(&fx);
    use common::multiset::{float_cols, F};
    let pairs = "SELECT regr_slope(total, item_count), covar_samp(n, i) FILTER (WHERE i > 0), corr(a + 1, b), regr_r2(x, y) AS r2 FROM t";
    assert_eq!(float_cols(pairs, None, None), (0..4).map(|k| (k, F::F8)).collect::<BTreeMap<_, _>>());
    assert_eq!(float_cols("SELECT corr(a, b) OVER (), regr_count(a, b), sum(total), regr_slope(a) FROM t", None, None), BTreeMap::new());
    let q9 = "SELECT regr_slope(total, item_count) AS slope, regr_intercept(total, item_count) AS intercept, regr_r2(total, item_count) AS r2 FROM orders";
    let case = |v: &str, got: &str| (format!("addendum9-smoke-{v}"), format!(
        "sql: {q9}\nclass: unordered\nverdict: {v}\n[pg]\n0.001999999999999973\t250.97999999999996\t2.4000000009599255e-10\n[got]\n{got}\n"));
    common::selftest_cases(&[case("match", "0.002000000000000152\t250.98\t2.4000000009603603e-10"),
                             case("mismatch", "0.002000000000010152\t250.98\t2.4000000009603603e-10")]);
}

/// V4R-5: applies::WIDENED widens write.route past its roads.rs predicate (`insert`) to multi_target; write.literals,
/// which shares that predicate, is not widened: a multi-target DELETE takes write.route alone. V4R-204 (M4-plan
/// Decision 2) narrows write.route to a routed INSERT: a plain INSERT takes write.literals alone, a routed one both.
#[test]
fn widened_write_route_is_not_literals() {
    let mut mt = PropSet::default();
    mt.add(Prop::MultiTarget);
    let del = applies::strategies_for(KindSet::of(&[KindId::WriteDelete]), mt);
    assert!(del.has(WRITE_ROUTE) && !del.has(WRITE_LITERALS), "{del:?}");
    let ins = applies::strategies_for(KindSet::of(&[KindId::WriteInsert]), PropSet::default());
    assert!(!ins.has(WRITE_ROUTE) && ins.has(WRITE_LITERALS), "{ins:?}");
    let mut rt = PropSet::default();
    rt.add(Prop::Route);
    let routed = applies::strategies_for(KindSet::of(&[KindId::WriteInsert]), rt);
    assert!(routed.has(WRITE_ROUTE) && routed.has(WRITE_LITERALS), "{routed:?}");
}

/// V4R-5 (the user 09-24): a cross-partition UPDATE is multi_target with ONE result relation. publication-003 is
/// `UPDATE pub_testpart1.parent1 set a = 1`: a partitioned parent pruned to its one leaf, `route=... cross-partition`.
/// Decomposed as the server does (`decompose_in`).
#[test]
fn one_target_cross_partition_update_is_multi_target() {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    use gravity_lowerv4::{graph::Gx, props::Props};
    let rel = "crates/gravity/decompose/tests/plans/regress-full/publication-003.plan";
    let golden = need("crates/gravity/decompose/tests/golden/regress-full/publication-003.graph");
    assert!(golden.contains(" cross-partition") && !golden.contains(" tableoid=") && !golden.contains("\n    target rti="), "{golden}");
    let ctx = mcx::MemoryContext::new("v4 cross-partition");
    let fx = gravity_decompose::corpus::load(ctx.mcx(), &need(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
    let arena = gravity_program::Arena::new(1 << 20);
    let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx)
        .unwrap_or_else(|r| panic!("{rel}: {r:?}"));
    let p = Props::of(&Gx { graph: &graph, facts: &fx.facts, catalog: common::RigCatalog::get(), over: None, mcx: rig.mcx() });
    assert!(p.kinds.has(KindId::WriteUpdate) && p.props.has(Prop::MultiTarget), "{rel}: {:?} {:?}", p.kinds, p.props);
    let roads = applies::strategies_for(p.kinds, p.props);
    assert!(roads.has(WRITE_ROUTE) && !roads.has(WRITE_LITERALS), "{roads:?}");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(|d| std::path::Path::new(&d).with_file_name("lowerv4")).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}

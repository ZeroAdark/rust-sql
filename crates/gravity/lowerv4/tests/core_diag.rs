//! The all-blockers diagnostic mode (frame/diag.rs, docs/gravity/gate.md §16): over the census plan fixtures first served
//! at M3, the mode refuses exactly as the mode-off lowering does and lowers exactly what it lowers; a refused statement
//! lists every blocker (admission's unbuilt kinds and roads, each maker's refusal past a placeholder); a placeholder is
//! never emitted. Pinned: a graph behind two milestones' kinds, and a cascade (a maker's refusal after an earlier
//! blocker's placeholder, flagged, never a sole blocker). The mode's record is each lowering's own (`lower_graph_diag`'s
//! report, D542): the tests share no state.
use gravity_lowerv4_it as common;

use std::path::PathBuf;

use gravity_lowerv4::frame::diag;
use gravity_lowerv4::Reason;

fn repo(rel: &str) -> PathBuf {
    manifest_dir().join("../../..").join(rel)
}

/// census/serve.tsv ids whose first serve is `m`.
fn ids(m: &str) -> Vec<String> {
    let t = std::fs::read_to_string(repo("crates/gravity/lowerv4/testdata/gravity/lowerv4/census/serve.tsv")).expect("serve.tsv");
    t.lines().skip(1).filter_map(|l| {
        let mut f = l.split('\t');
        let id = f.next()?;
        (f.next()? == m).then(|| id.to_owned())
    }).collect()
}

fn plan_text(id: &str) -> Option<String> {
    let rel = match id.strip_prefix("regress-full-") {
        Some(stem) => format!("regress-full/{stem}.plan"),
        None => format!("{id}.plan"),
    };
    std::fs::read_to_string(common::plans_dir().join(rel)).ok()
}

/// A statement's SQL as both captures print it: `;` and surrounding blanks trimmed (tests/m3.rs, D505).
fn sql_key(s: &str) -> String {
    s.trim().trim_end_matches(';').trim().to_owned()
}

/// D542 (as tests/m3.rs m3_ids, D505): the crate's regress-full capture numbers a test's statements apart from the census
/// (lowerv3 census/shapes.tsv), so `plans/regress-full/<stem>.plan` is often another statement than census id
/// `regress-full-<stem>`. -> for each regress-full census id, the crate stem that captured the census row's SQL (the first
/// of its test, by stem order), or None when no crate plan of its test has that SQL. Other ids are not in the map.
fn regress_remap() -> std::collections::BTreeMap<String, Option<String>> {
    use std::collections::BTreeMap;
    let t = std::fs::read_to_string(repo("crates/gravity/lowerv4/testdata/gravity/lowerv3/census/shapes.tsv")).expect("shapes.tsv");
    let census: BTreeMap<String, String> = t.lines().filter(|l| !l.starts_with('#'))
        .filter_map(|l| Some((l.split('\t').next()?.to_owned(), sql_key(l.rsplit('\t').next()?))))
        .filter(|(id, _)| id.starts_with("regress-full-")).collect();
    let mut by_sql: BTreeMap<(String, String), String> = BTreeMap::new();
    for (stem, text) in common::files(&common::plans_dir().join("regress-full"), "plan") {
        let Some(sql) = text.lines().find_map(|l| l.strip_prefix("-- sql: ")) else { continue };
        by_sql.entry((stem.rsplit_once('-').map_or(stem.as_str(), |x| x.0).to_owned(), sql_key(sql))).or_insert(stem);
    }
    census.into_iter().map(|(id, sql)| {
        let stem = &id["regress-full-".len()..];
        let test = stem.rsplit_once('-').map_or(stem, |x| x.0).to_owned();
        let hit = by_sql.get(&(test, sql)).cloned();
        (id, hit)
    }).collect()
}

/// A plan fixture lowered in the mode (`lower_graph_diag`, as common's `lower_fixture` lowers it): served or the refusal's
/// text, and the report; `None` when the statement does not decompose.
fn lower_diag(text: &str) -> Option<(Result<(), String>, diag::Report)> {
    common::RigCatalog::install_seams();
    let (ctx, arena) = (mcx::MemoryContext::new("v4 rig fixture"), gravity_program::Arena::new(1 << 20));
    let fx = gravity_decompose::corpus::load(ctx.mcx(), text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).ok()?;
    gravity_expr::nodes::func::acl_defer_begin();
    let (r, report) = diag::lower_graph_diag(arena.handle(), &graph, &fx.facts, common::RigCatalog::get(), &mut gravity_lower::Sequential::new(arena.handle()), gravity_lower::LowerOpts::default());
    gravity_expr::nodes::func::acl_defer_end();
    Some((r.map(|_| ()).map_err(|e| e.to_string()), report))
}

struct Seen {
    id: String,
    blockers: Vec<diag::Blocker>,
    lines: Vec<String>,
}

/// Lower `text` with the mode off, then on: the outcome must match; the mode's blockers and lines.
fn both(id: &str, text: &str) -> Option<Seen> {
    let off = common::lower_fixture(text)?.map(|_| ()).map_err(|e| e.to_string());
    let (on, diag::Report { blockers, lines }) = lower_diag(text)?;
    match (&off, &on) {
        (Ok(_), Ok(_)) => assert!(lines.is_empty(), "{id}: served, yet blocker lines {lines:?}"),
        (Err(a), Err(b)) => {
            assert_eq!(a, b, "{id}: the mode refuses as the mode-off lowering does");
            assert!(!lines.is_empty(), "{id}: refused ({b}) with no blocker line");
        }
        _ => panic!("{id}: mode off {:?} vs on {:?}", off.as_ref().map(|_| "served"), on.as_ref().map(|_| "served")),
    }
    for (i, l) in lines.iter().enumerate() {
        assert!(l.starts_with("gravity: blocker pipe="), "{id}: {l}");
        assert!(l.contains(&format!(" k={}/{} ", i + 1, lines.len())), "{id}: {l}");
        assert!(!l.contains("diag placeholder"), "{id}: a placeholder was emitted: {l}");
    }
    Some(Seen { id: id.to_owned(), blockers, lines })
}

/// Every census fixture first served at `ms`, lowered both ways.
fn corpus(ms: &[&str]) -> Vec<Seen> {
    ms.iter().flat_map(|m| ids(m)).filter_map(|id| plan_text(&id).and_then(|t| both(&id, &t))).collect()
}

#[test]
fn every_blocker_and_the_same_outcome() {
    // M1 / M2 fixtures: the mode lowers what the mode-off lowering lowers, refuses what it refuses, with its refusal
    let early = corpus(&["M1", "M2"]);
    assert!(early.len() > 1000, "M1 / M2 fixtures lowered: {}", early.len());
    assert!(early.iter().any(|s| s.lines.is_empty()), "a served M1 / M2 fixture");
    // M3's: each refused, with every blocker
    let m3 = corpus(&["M3"]);
    assert!(m3.len() > 300, "M3 fixtures lowered: {}", m3.len());
    let multi = m3.iter().filter(|s| s.blockers.len() >= 2).count();
    let lower = m3.iter().filter(|s| s.blockers.iter().any(|b| b.stage == "lower")).count();
    let cascade = m3.iter().filter(|s| s.blockers.iter().any(|b| b.cascade)).count();
    eprintln!("diag: M1/M2 fixtures {}, M3 {}: {} with >= 2 blockers, {} past admission, {} with a cascade", early.len(), m3.len(), multi, lower, cascade);
    assert!(multi > 0 && lower > 0 && cascade > 0);
    for s in &m3 {
        // a cascade is never a NotBuilt, and follows a line naming a blocked pipeline <= its own (an admission line
        // names the first pipeline its kind's maker refused in)
        for (i, b) in s.blockers.iter().enumerate().filter(|(_, b)| b.cascade) {
            assert!(!matches!(b.refusal.reason, Reason::NotBuilt { .. }), "{}: a NotBuilt cascade", s.id);
            let before = &s.blockers[..i];
            assert!(b.pipe.is_none() || before.iter().any(|a| a.pipe.zip(b.pipe).is_some_and(|(x, y)| x <= y)), "{}: {:?}", s.id, s.lines);
        }
    }
}

/// Pinned: regress-full with-087 (`WITH simpletup AS (..), upsert_cte AS (INSERT INTO withz .. ON CONFLICT (k) DO UPDATE
/// SET (k, v) = (SELECT k, v FROM simpletup ..) RETURNING k, v) INSERT INTO withz .. ON CONFLICT (k) DO UPDATE SET (k, v)
/// = (SELECT k, v FROM upsert_cte ..) RETURNING k, v`, regress with#234) lowers with no blocker line since ctescan D2644
/// built its M8 nest.cte wall. It was the last plan fixture behind a NotBuilt of M7 or later (a scan of every fixture
/// under decompose/tests/plans found it alone on trunk abb2efb361c and none after), so the later-milestone wall this test
/// pinned has nothing left to pin (merge-073, with-096, with-094 and with-074 held it before, as their walls were built).
/// with-036 (`explain (verbose, costs off) with recursive test as (...) search depth first by x set y select * from
/// test limit 5`) is behind a maker's Shape (an output type the catalog does not know) whose later pipelines' Shapes are
/// cascades. (with-036 held both until setop P6 built its M9 setop.recursive wall; enum-008, the pin before it, lost
/// its M8 wall to M8-SUB B1, nest.initplan: main 07:13Z; rowsecurity-008 lost its maker's Shape to M8 L0-w, V4R-344;
/// aggs-029 serves since the M3 grouping landings.)
#[test]
fn two_walls_and_a_cascade() {
    let diag_of = |id: &str| both(id, &plan_text(id).unwrap_or_else(|| panic!("{id}.plan"))).expect("decomposes");
    let (built, s) = (diag_of("regress-full-with-087"), diag_of("regress-full-with-036"));
    assert!(built.blockers.is_empty() && built.lines.is_empty(), "{:?}", built.lines);
    let shape = |c: bool| s.blockers.iter().any(|b| matches!(b.refusal.reason, Reason::Shape { .. }) && b.stage == "lower" && b.cascade == c);
    assert!(shape(false) && shape(true), "{:?}", s.lines);
    // one line per wall: no repeated (pipe, stage, reason, node)
    let mut seen = std::collections::BTreeSet::new();
    assert!(s.blockers.iter().all(|b| seen.insert(format!("{:?}{}{:?}", b.pipe, b.stage, b.refusal))), "{:?}", s.lines);
}

/// D788: a refusal raised while the program is emitted (after every maker ran and `stop` found no blocker, e.g. a key
/// image's `not_built!` from runs/keylane.rs) is a blocker line like a maker's: the mode used to mark the record stopped
/// at `stop` whatever it found, so `lower` dropped the emission refusal and a refused statement listed nothing. The two
/// census fixtures the all-milestone dump found refused with no line on trunk 5a7bb6d64c7 (by D505's re-map, as `dump`).
#[test]
fn an_emission_refusal_has_its_blocker_line() {
    let remap = regress_remap();
    // partition_aggregate-015 left the list with D771: props names its Sort's rep.hosted.order, so it refuses at admission
    for id in ["regress-full-tuplesort-020"] {
        let fixture = match remap.get(id) {
            Some(Some(stem)) => format!("regress-full-{stem}"),
            _ => id.to_owned(),
        };
        let text = plan_text(&fixture).unwrap_or_else(|| panic!("{id}: no plan {fixture}"));
        let (r, rep) = lower_diag(&text).unwrap_or_else(|| panic!("{id}: {fixture} does not decompose"));
        let Err(e) = r else { continue }; // served once its wall is built: nothing to pin
        assert!(!rep.lines.is_empty(), "{id} ({fixture}): refused ({e}) with no blocker line");
        assert!(rep.lines.iter().all(|l| l.starts_with("gravity: blocker pipe=")), "{id}: {:?}", rep.lines);
        assert!(rep.blockers.iter().any(|b| b.stage == "lower" && b.pipe.is_none()), "{id}: the emission refusal ({e}) is not a line: {:?}", rep.lines);
    }
}

/// The dry run's crate half (M3-SCOPE): every census fixture first served at `$DIAG_MS` (default M3; a comma list) lowered
/// in the mode, one TSV row per blocker into `$DIAG_OUT` (id, outcome, the §16 fields); a statement without a plan
/// fixture (e2e ids, bar ids, oversize plans) or whose graph does not decompose is one row naming why. The server half
/// (rig.py --diag, gravity-bar-dryrun.sh DIAG=1) runs what has no fixture and checks the live first refusals.
///   DIAG_OUT=/tmp/m3-blockers.tsv cargo test -p gravity_lowerv4 --test diag -- --ignored dump
#[test]
#[ignore]
fn dump() {
    let Ok(out) = std::env::var("DIAG_OUT") else { return };
    let ms = std::env::var("DIAG_MS").unwrap_or_else(|_| "M3".into());
    let mut rows = vec!["id\tplaced\toutcome\tpipe\tnode\tstage\tmilestone\treason\tcascade\ttext".to_string()];
    let remap = regress_remap();
    let (mut remapped, mut unmatched) = (0, 0);
    for m in ms.split(',') {
        for id in ids(m) {
            // D542: a regress-full id lowers the crate plan that captured ITS statement (D505's re-map), never the plan
            // the crate happened to number the same; with no such plan it is a fixture-less id (the server half has it)
            let fixture = match remap.get(&id) {
                Some(Some(stem)) if format!("regress-full-{stem}") != id => { remapped += 1; format!("regress-full-{stem}") }
                Some(None) => { unmatched += 1; rows.push(format!("{id}\t{m}\tno-fixture\t-\t-\t-\t-\t-\t-\tD505: no crate plan with the census SQL")); continue }
                _ => id.clone(),
            };
            let Some(text) = plan_text(&fixture) else {
                rows.push(format!("{id}\t{m}\tno-fixture\t-\t-\t-\t-\t-\t-\t-"));
                continue;
            };
            let (r, blockers) = match lower_diag(&text) {
                Some((r, rep)) => (Some(r), rep.blockers),
                None => (None, Vec::new()),
            };
            let outcome = match &r {
                None => "no-graph",
                Some(Ok(_)) => "served",
                Some(Err(_)) => "refused",
            };
            if blockers.is_empty() || r.is_none() {
                rows.push(format!("{id}\t{m}\t{outcome}\t-\t-\t-\t-\t-\t-\t-"));
            }
            for b in blockers.iter().filter(|_| r.is_some()) {
                let s = b.refusal.to_seam();
                let bm = match b.refusal.reason {
                    Reason::NotBuilt { milestone, .. } => milestone.name(),
                    _ => "-",
                };
                let pipe = b.pipe.map_or("-".to_string(), |k| k.to_string());
                rows.push(format!("{id}\t{m}\t{outcome}\t{pipe}\t{}\t{}\t{bm}\t{}\t{}\t{}", b.refusal.node, b.stage, b.refusal.reason.variant(), b.cascade as u8, s.what));
            }
        }
    }
    std::fs::write(&out, rows.join("\n") + "\n").expect("DIAG_OUT");
    eprintln!("diag dump: {} rows -> {out} (D542: {remapped} regress-full ids re-mapped to the crate plan of their SQL, {unmatched} without one)", rows.len() - 1);
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(|d| std::path::Path::new(&d).with_file_name("lowerv4")).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}

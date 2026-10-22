// The partition write faces over the corpus (src/partwrite.rs): every target under a routing root
// carries a column map into the root within the root's width, the root itself the identity, and the
// counts are pinned in tests/golden/partwrite.txt (REGEN=1 rewrites).
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::graph::{Op, Source};
use gravity_decompose::{corpus, decompose_in};
use gravity_decompose::ops::modifytable::partwrite;
use gravity_program::{Arena, Cmd};
use mcx::MemoryContext;

fn plans() -> Vec<PathBuf> {
    let root = manifest_dir().join("tests/plans");
    let mut v: Vec<PathBuf> = [root.clone(), root.join("regress-full")].iter().flat_map(|d| fs::read_dir(d).unwrap().map(|e| e.unwrap().path())).filter(|p| p.extension().is_some_and(|e| e == "plan")).collect();
    v.sort();
    v
}

#[test]
fn routed_targets_map_into_the_root() {
    let arena = Arena::new(1 << 22);
    let (mut routed, mut cross, mut under, mut holes, mut nonid, mut byc) = (0usize, 0usize, 0usize, 0usize, 0usize, [0usize; 4]);
    let (mut streamed, mut joined) = (0usize, 0usize);
    for path in plans() {
        let text = fs::read_to_string(&path).unwrap();
        let ctx = MemoryContext::new("fixture");
        let fx = corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let h = arena.handle();
        let m = h.mark();
        match decompose_in(h, fx.stmt, &fx.facts, fx.mcx) {
            Err(r) => assert!(!r.reason.contains("routing root"), "{}: {}", path.display(), r.reason),
            Ok(g) => {
                // a write streamed into its union's arms (V4R-214) is one write, counted at arm 0's copy; each arm writes
                // the target its own scan reads (its rti and relid), or joins first (counted, pinned)
                let copy = |p: &&gravity_decompose::graph::PipelineNode<'_>| p.ops.iter().any(|o| matches!(o, Op::Append(AppendDesc::Arm { i, .. }) if *i > 0));
                for p in g.pipelines.iter().filter(|p| p.ops.iter().any(|o| matches!(o, Op::Append(AppendDesc::Arm { .. })))) {
                    let Some(d) = p.ops.iter().find_map(|o| if let Op::Write(d) = o { Some(d) } else { None }) else { continue };
                    streamed += 1;
                    let scan = match &p.source {
                        Source::Range { scanrelid, relid, .. } | Source::Index { scanrelid, relid, .. } | Source::Bitmap { scanrelid, relid, .. } | Source::Tid { scanrelid, relid, .. } => Some((u32::from(*scanrelid), *relid)),
                        _ => None,
                    };
                    match scan.filter(|_| !p.ops.iter().any(|o| matches!(o, Op::Join(_)))) {
                        Some((rti, relid)) => assert!(d.targets.iter().any(|t| t.rti == rti && t.relid == relid), "{}: an arm writes a target its scan does not read", path.display()),
                        None => joined += 1,
                    }
                }
                for d in g.pipelines.iter().filter(|p| !copy(p)).flat_map(|p| p.ops.iter()).filter_map(|o| if let Op::Write(d) = o { Some(d) } else { None }) {
                    let Some(r) = &d.route else {
                        assert!(d.targets.iter().all(|t| t.to_root.is_empty()), "{}: a map without a route", path.display());
                        continue;
                    };
                    routed += 1;
                    cross += r.cross_partition as usize;
                    byc[match d.cmd {
                        Cmd::Insert => 0,
                        Cmd::Update => 1,
                        Cmd::Delete => 2,
                        _ => 3,
                    }] += 1;
                    assert_eq!(partwrite::root_site(r).natts(), r.root_natts as usize);
                    for t in d.targets.iter() {
                        if t.rti == r.root_rti {
                            assert!(t.to_root.is_empty(), "{}: the root with a map", path.display());
                            continue;
                        }
                        under += 1;
                        assert_eq!(t.to_root.len(), t.site.natts(), "{}: map width", path.display());
                        assert!(t.to_root.iter().all(|&a| a >= 0 && a as u16 <= r.root_natts), "{}: map out of the root", path.display());
                        holes += t.to_root.iter().filter(|&&a| a == 0).count();
                        nonid += !partwrite::identity(t) as usize;
                        if let Some(&a) = t.to_root.first() {
                            assert_eq!(partwrite::root_attno(t, 1), a);
                        }
                    }
                }
            }
        }
        h.reset_to(m);
    }
    let mut got = String::new();
    writeln!(got, "routed writes {routed}: insert {} update {} delete {} merge {}", byc[0], byc[1], byc[2], byc[3]).unwrap();
    writeln!(got, "cross-partition {cross}; targets under a root {under}; non-identity maps {nonid}; unmapped child columns {holes}").unwrap();
    writeln!(got, "streamed arms {streamed}; arms joining before the write {joined}").unwrap();
    let golden = manifest_dir().join("tests/golden/partwrite.txt");
    if std::env::var("REGEN").is_ok_and(|v| !v.is_empty()) {
        fs::write(&golden, &got).unwrap();
    }
    assert_eq!(fs::read_to_string(&golden).unwrap_or_default(), got, "{}", golden.display());
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}

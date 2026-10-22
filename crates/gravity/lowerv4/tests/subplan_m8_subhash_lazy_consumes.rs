//! M8-SUB B2a (V4R-482; main 09-29): a LAZY hashed SubPlan's set runs at its first probe, so its readers leave its slot
//! (frame/program.rs). Only the set's own life leaves: decompose reuses slot numbers (tsrf-015: the Sort's slot 0 and
//! the set's Distinct slot 0; the Sort's reader lost its run and the server crashed), and an empty `consumes_from` names
//! the nearest producer (a reader of the set that also reads a hash build, tpch-016 / subselect-055, keeps the build).
//! tpch-016 (a grouping beside a hash join) refuses by the no-widen line's D2194 name until that name comes off the list
//! at its root fix (main 02:53Z item 5); it then lowers with a LAZY set again and returns to the fixtures that must.
use gravity_lowerv4_it as common;

use gravity_program::protos::{ProtoKind, SUBHASH_LAZY};
use gravity_program::LifeId;

#[test]
fn a_lazy_sets_readers_leave_only_its_own_life() {
    unsafe extern "C-unwind" fn noop(_: *mut gravity_abi::Ctx) -> i64 {
        0
    }
    fn walk(d: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for p in std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.path()) {
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "plan") {
                out.push(p);
            }
        }
    }
    let dir = common::plans_dir();
    let mut files = Vec::new();
    walk(&dir, &mut files);
    files.sort();
    let (mut checked, mut bad) = (Vec::new(), Vec::new());
    for p in &files {
        let rel = p.strip_prefix(&dir).unwrap().display().to_string();
        let text = std::fs::read_to_string(p).expect("fixture");
        let Some(Ok(l)) = common::lower_fixture(&text) else { continue };
        let d = &l.descs;
        let protos = &d.v4.protos[..];
        let lazy: Vec<usize> = protos.iter().filter(|x| x.kind == ProtoKind::SubHash && x.arg & SUBHASH_LAZY != 0).map(|x| x.pipeline as usize).collect();
        if lazy.is_empty() {
            continue;
        }
        let (codes, w) = (d.pipelines.iter().map(|_| gravity_program::Code::hand(noop)), d.write.copy_in(*d.sites.allocator()));
        let prog = gravity_program::Program::with_declared_pipelines(d.layout, &d.sites, &d.out, w, d.pipelines.iter().copied().zip(codes));
        let on_demand = |q: usize| protos.iter().any(|x| x.pipeline as usize == q && matches!(x.kind, ProtoKind::SubHash | ProtoKind::Init | ProtoKind::Cte | ProtoKind::Rerun));
        for &s in &lazy {
            if let Some(r) = prog.consumers_of(LifeId(s as u16)).next() {
                bad.push(format!("{rel}: pipeline {r} still reads the LAZY set p{s}'s slot"));
            }
        }
        for life in prog.lives().filter(|x| !on_demand(x.producer)) {
            if prog.consumers_of(life.id).next().is_none() {
                bad.push(format!("{rel}: p{}'s slot {} has no reader left", life.producer, life.slot));
            }
        }
        checked.push(rel);
    }
    for must in ["regress-full/tsrf-015.plan", "regress-full/subselect-055.plan"] {
        assert!(checked.iter().any(|c| c == must), "{must}: not lowered with a LAZY set ({} fixtures were)", checked.len());
    }
    // v5 subplan 12 lifted D2194's no-widen name: a grouping over a lazily run build lowers
    match common::lower_fixture(&std::fs::read_to_string(dir.join("tpch-016.plan")).expect("fixture")) {
        Some(Ok(_)) => {}
        Some(Err(e)) => panic!("tpch-016: {e}"),
        None => panic!("tpch-016: not lowered"),
    }
    assert!(bad.is_empty(), "{} LAZY-set programs lost a reader:\n{}", bad.len(), bad.join("\n"));
}

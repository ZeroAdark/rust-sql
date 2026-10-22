//! M8-RERUN R3 store_reopen (V4R-439; srf/store_read.rs `reopen` / `pass`): a store in a rerun set whose calls read an
//! exec param passes it at every open (`store_param`) before its rewind. Over the decompose fixtures with a FunctionScan
//! or TableFuncScan and an exec param: every pipeline whose code passes a store's params, and every pipeline whose code
//! re-runs a set (`m8_rerun`), runs on the leader (D1035): the ScanStore's params are one state no worker shares.
use gravity_lowerv4_it as common;

/// Whether function `f` of `ir`, or a function it calls, calls host `name` (`seen`: the functions on the walk).
fn calls_host(ir: &gravity_ir::Program, f: usize, name: &str, seen: &mut Vec<bool>) -> bool {
    if std::mem::replace(&mut seen[f], true) {
        return false;
    }
    ir.funcs[f].blocks.iter().flat_map(|b| b.insts.iter()).any(|i| match i.op {
        gravity_ir::Op::CallHost { callee, .. } => ir.host_fns[callee.index()].name == name,
        gravity_ir::Op::Call { callee, .. } => calls_host(ir, callee.index(), name, seen),
        _ => false,
    })
}

/// Every fixture under `d` (recursively) whose plan has a function or table function scan and an exec param.
fn candidates(d: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for p in std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.path()) {
        if p.is_dir() {
            candidates(&p, out);
        } else if p.extension().is_some_and(|x| x == "plan") {
            let t = std::fs::read_to_string(&p).unwrap_or_default();
            if (t.contains("{FUNCTIONSCAN") || t.contains("{TABLEFUNCSCAN")) && t.contains(":paramkind 1") {
                out.push(p);
            }
        }
    }
}

/// D1035 over store_reopen: no pipeline that passes a store's params or re-runs a set is eligible for a worker, and at
/// least one lowered fixture passes a store's params.
#[test]
fn a_reopened_store_and_its_owner_sit_on_the_leader() {
    let mut files = Vec::new();
    candidates(&common::plans_dir(), &mut files);
    files.sort();
    let mut passing = Vec::new();
    for p in &files {
        let text = std::fs::read_to_string(p).expect("fixture");
        let Some(Ok(l)) = common::lower_fixture(&text) else { continue };
        let rel = p.strip_prefix(common::plans_dir()).unwrap().display().to_string();
        let mut passes = false;
        for (i, f) in l.funcs.iter().enumerate() {
            for host in ["store_param", "m8_rerun"] {
                if calls_host(&l.ir, f.index(), host, &mut vec![false; l.ir.funcs.len()]) {
                    passes |= host == "store_param";
                    assert!(!l.descs.pipelines[i].is_eligible(), "{rel}: p{i} calls {host} on a pool thread: {:?}", l.descs.pipelines[i].parallel);
                }
            }
        }
        if passes {
            passing.push(rel);
        }
    }
    assert!(!passing.is_empty(), "no fixture of {} lowers a store that passes its params", files.len());
}

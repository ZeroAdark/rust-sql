//! D478 (V4R-143, M3-MERGE): a MergeAppend under a LIMIT over index arms lowers in the pull form (runs/merge_pull.rs). The
//! reader pulls every arm's rows through the arm's own site, so an arm's pipeline must read nothing, or a row would be read
//! (and its site's rows counted) twice. On a fresh call an arm's function goes from the open to `done` with no seek and no
//! advance reachable; only the resume dispatch leads into the frame's window loop, and a pipeline that emits no row never
//! stops, so it never resumes. The reader's function seeks and advances every arm's site.
use gravity_lowerv4_it as common;

use gravity_ir::{Block, Function, Op, Program, Terminator};

/// The host functions called in the blocks of `f` reachable from `from`.
fn reachable_calls(ir: &Program, f: &Function, from: Block) -> Vec<String> {
    let (mut seen, mut stack, mut out) = (vec![false; f.blocks.len()], vec![from], Vec::new());
    while let Some(b) = stack.pop() {
        if std::mem::replace(&mut seen[b.index()], true) {
            continue;
        }
        let bb = &f.blocks[b.index()];
        for i in bb.insts.iter() {
            if let Op::CallHost { callee, .. } = &i.op {
                out.push(ir.host_fn(*callee).name.to_string());
            }
        }
        stack.extend(bb.term.edges().map(|e| e.target));
    }
    out
}

#[test]
fn pull_arms_read_nothing_and_the_reader_reads_every_arm() {
    for id in ["part-030", "part-031", "part-032", "part-033"] {
        let text = std::fs::read_to_string(common::plans_dir().join(format!("{id}.plan"))).expect("plan fixture");
        let l = common::lower_fixture(&text).expect("decomposes").unwrap_or_else(|e| panic!("{id}: {e:?}"));
        let (reader, arms) = l.ir.funcs.split_last().expect("functions");
        assert!(arms.len() >= 2, "{id}: {} arm functions", arms.len());
        for f in arms {
            let Terminator::CondBr { then_edge, .. } = &f.blocks[f.entry.index()].term else { panic!("{id} {}: no resume dispatch at the entry", f.name) };
            let calls = reachable_calls(&l.ir, f, then_edge.target);
            assert!(!calls.iter().any(|c| c == "seek" || c == "advance"), "{id} {}: a fresh call reaches {calls:?}", f.name);
            assert!(calls.iter().any(|c| c == "done"), "{id} {}: a fresh call never reaches done: {calls:?}", f.name);
        }
        let calls = reachable_calls(&l.ir, reader, reader.entry);
        for h in ["seek", "advance"] {
            let n = calls.iter().filter(|c| *c == h).count();
            assert!(n >= arms.len(), "{id} {}: {n} {h} calls for {} arms", reader.name, arms.len());
        }
    }
}

//! D1029 (main 07:10Z): a hash build's row retry (join/build.rs l_try) grew its region through l_grow and retried with no
//! cancel check, registered as a bounded loop on the premise that each grow at least doubles the region. At the driver's
//! MAX_REGION the region reopened at its own length, and a row whose payload images were what did not fit retried for
//! ever. The grow path now checks cancel: every cycle through a `region_grow` call passes a block that loads ctx.cancel
//! (a frame check). Over hash builds the census serves (M1 TPC-H SF10 q14 and q17's, M2 regress and bench joins).
#[allow(dead_code)]
use gravity_lowerv4_it as common;

use gravity_abi::ctx::hdr;
use gravity_ir::{Function, Op, Program, Type};

const IDS: &[&str] = &["tpch-sf10-014", "tpch-sf10-017", "regress-043", "bench-077"];

/// The hosts a hash build's l_grow calls: `region_grow`, or `m7_spill_grow` for a build that may spill (V4R-359: bench-077).
const GROWS: &[&str] = &["region_grow", "m7_spill_grow"];

/// The blocks of `f` that call a `GROWS` host and lie on a cycle no block loading ctx.cancel breaks.
fn unchecked_grows(p: &Program, f: &Function) -> Vec<usize> {
    let Some(&ctx) = f.blocks.get(f.entry.index()).and_then(|e| e.params.first()) else { return Vec::new() };
    let checks = |v: usize| f.blocks[v].insts.iter().any(|i| matches!(i.op, Op::Load { ty: Type::Ptr, ptr, offset, .. } if ptr == ctx && offset == hdr::CANCEL as i32));
    let grows = |v: usize| f.blocks[v].insts.iter().any(|i| matches!(&i.op, Op::CallHost { callee, .. } if GROWS.contains(&p.host_fn(*callee).name.as_str())));
    let succ = |v: usize| f.blocks[v].term.edges().map(|e| e.target.index()).collect::<Vec<_>>();
    (0..f.blocks.len()).filter(|&s| grows(s)).filter(|&s| {
        let (mut seen, mut stack) = (vec![false; f.blocks.len()], succ(s));
        while let Some(v) = stack.pop() {
            if v == s {
                return true;
            }
            if !seen[v] && !checks(v) {
                seen[v] = true;
                stack.extend(succ(v));
            }
        }
        false
    }).collect()
}

/// Each fixture's lowering calls `region_grow` in a hash build, and every such call's retry passes a cancel check.
#[test]
fn a_hash_builds_grow_retry_passes_a_cancel_check() {
    for id in IDS {
        let text = std::fs::read_to_string(common::plans_dir().join(format!("{id}.plan"))).unwrap_or_else(|e| panic!("{id}: {e}"));
        let l = common::lower_fixture(&text).unwrap_or_else(|| panic!("{id}: no graph")).unwrap_or_else(|e| panic!("{id}: refused: {e:?}"));
        let grows: usize = l.ir.funcs.iter().map(|f| f.blocks.iter().filter(|b| b.insts.iter().any(|i| matches!(&i.op, Op::CallHost { callee, .. } if GROWS.contains(&l.ir.host_fn(*callee).name.as_str())))).count()).sum();
        assert!(grows > 0, "{id}: no grow call (not a hash build any more: pick another fixture)");
        let bad: Vec<(usize, Vec<usize>)> = l.ir.funcs.iter().enumerate().map(|(k, f)| (k, unchecked_grows(&l.ir, f))).filter(|(_, b)| !b.is_empty()).collect();
        assert!(bad.is_empty(), "{id}: a grow on a cycle with no cancel check (function, blocks): {bad:?}");
    }
}

// V4R-481: a hash build carries the rows its Hash receives (`Pipeline.build_rows`, PostgreSQL's estimate at the Hash node),
// what admission and the grant estimate size its table by. Its `predicted_rows` stays its driving scan's: a build fed
// through a join (pgrc TH09: lineitem JOIN part under partsupp's probe) receives the join's rows (1.37 M), not the scan's
// (60 M), and was refused as a 5.9 GB build past one region (D272). Over the plan corpus: every HashJoin's build names its
// Hash's rows, and some builds fed through a join name fewer rows than their driving scan.
use std::collections::HashMap;
use std::fs;

use gravity_abi::breaker::BreakerKind;
use gravity_decompose::{corpus, decompose_in, rerun as rescan};
use gravity_program::{Arena, End};
use mcx::MemoryContext;

mod harness;

fn rows(plan_rows: f64) -> u64 {
    if plan_rows.is_finite() && plan_rows > 0.0 {
        plan_rows as u64
    } else {
        0
    }
}

#[test]
fn a_hash_build_carries_the_rows_its_hash_receives_not_its_driving_scans() {
    let mut files = harness::fixtures_in(&harness::plans_root());
    files.extend(harness::fixtures_in(&harness::plans_root().join("regress-full")));
    let (mut builds, mut fewer) = (0usize, 0usize);
    for f in files {
        let text = fs::read_to_string(&f).unwrap();
        let ctx = MemoryContext::new("fixture");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { continue };
        let mut hashes: HashMap<i32, u64> = HashMap::new();
        rescan::each_plan_node(fx.stmt, &mut |n| {
            if let Some(h) = n.as_hash() {
                hashes.insert(h.plan.plan_node_id, rows(h.plan.plan_rows));
            }
        });
        let arena = Arena::new(1 << 20);
        let Ok(g) = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { continue };
        let name = &fx.name;
        for (i, p) in g.pipelines.iter().enumerate() {
            let End::Breaker(b) = p.desc.ends_in else { continue };
            if b.kind != BreakerKind::HashBuild {
                continue;
            }
            // the Hash is its build's last node (map_hash_join's close_node); a catalog nest loop's build ends in its inner
            let Some((hid, &want)) = p.node_ops.last().and_then(|o| hashes.get_key_value(&o.node_id)) else { continue };
            builds += 1;
            assert_eq!(p.desc.build_rows, want, "{name} p{i}: the build names its Hash's rows (node {hid})");
            assert_eq!(p.desc.build_estimate(), if want != 0 { want } else { p.desc.predicted_rows }, "{name} p{i}");
            if want != 0 && want < p.desc.predicted_rows {
                fewer += 1;
            }
        }
    }
    println!("v4r481: {builds} hash builds over a Hash node, {fewer} sized under their driving scan's rows");
    assert!(builds > 100, "the corpus's hash joins ({builds})");
    assert!(fewer > 0, "some build is fed through a join or a filter ({fewer})");
}

// D17: every breaker's row bytes are the lowering's, with the planner's width (`plan_width`, from `stawidth`) of the rows it
// holds as the floor of a by-reference column's word. The walk publishes the width per pipeline (`Pipeline::row_width`:
// at each node's close, the planner width of the rows the pipeline carries from that node on; 0 when no plan node names
// them) and the cost home's breaker record reads it (`cost::breaker::record`, the mechanism of DR-1 / DR-4, 00215f29363).
// Over the plan corpus: a Sort's and an Agg's breaker floors its record by the planner width of a node its pipeline
// carries, or names none (rows a mapping synthesizes: a two-pass distinct's first pass projects its key alone); its record
// bytes are the lowering's (the owner layout, else `FALLBACK_RECORD`) with that width as the floor; some breakers' rows
// are wider than the lowering's record.
use std::collections::HashMap;
use std::fs;

use gravity_abi::breaker::BreakerKind;
use gravity_decompose::{corpus, decompose_in, rerun as rescan};
use gravity_program::cost::{breaker, FALLBACK_RECORD};
use gravity_program::{Arena, End};
use mcx::MemoryContext;

mod harness;

#[test]
fn a_sort_or_agg_breaker_record_takes_its_rows_planner_width_as_the_floor() {
    let mut files = harness::fixtures_in(&harness::plans_root());
    files.extend(harness::fixtures_in(&harness::plans_root().join("regress-full")));
    let (mut sorts, mut aggs, mut raised, mut unnamed) = (0usize, 0usize, 0usize, 0usize);
    for f in files {
        let text = fs::read_to_string(&f).unwrap();
        let ctx = MemoryContext::new("fixture");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { continue };
        let mut width: HashMap<i32, u32> = HashMap::new();
        rescan::each_plan_node(fx.stmt, &mut |n| {
            if let Some(p) = n.as_plan() {
                width.insert(p.plan_node_id, p.plan_width.max(0) as u32);
            }
        });
        let arena = Arena::new(1 << 20);
        let Ok(g) = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { continue };
        let name = &fx.name;
        for (i, p) in g.pipelines.iter().enumerate() {
            let End::Breaker(b) = p.desc.ends_in else { continue };
            let sort = matches!(b.kind, BreakerKind::Sort | BreakerKind::TopK);
            if !sort && !matches!(b.kind, BreakerKind::HashAgg | BreakerKind::PlainAgg | BreakerKind::Distinct) {
                continue;
            }
            let lowered = p.desc.owner.as_ref().map_or(FALLBACK_RECORD, |o| o.layout.size() as u64);
            let w = p.desc.row_width;
            assert_eq!(breaker::record(&p.desc), lowered.max(w as u64).max(1), "{name} p{i} {:?}: the lowering's record, the planner width its floor", b.kind);
            if w == 0 {
                unnamed += 1;
                continue;
            }
            let ids: Vec<i32> = p.node_ops.iter().map(|o| o.node_id).collect();
            assert!(ids.iter().any(|n| width.get(n) == Some(&w)), "{name} p{i} {:?}: width {w} is no node's on the pipeline (ops {ids:?})", b.kind);
            if sort {
                sorts += 1;
            } else {
                aggs += 1;
            }
            raised += (w as u64 > lowered) as usize;
        }
    }
    println!("d17: {sorts} sort and {aggs} agg breakers floor their record by a planner width ({unnamed} name none); {raised} asked it over the lowering's record");
    assert!(sorts > 20 && aggs > 20, "the corpus's sorts ({sorts}) and aggregates ({aggs}) name their rows' width");
    assert!(raised > 0, "some breaker's rows are wider than the lowering's record ({raised})");
}

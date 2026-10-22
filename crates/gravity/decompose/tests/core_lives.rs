// Breaker lives across the plan corpus: every consumer names the nearest earlier producer of the slot it reads, and every
// group is found by its life.
use std::fs;

use gravity_decompose::{corpus, decompose_in};
use gravity_program::{Arena, LifeId};
use mcx::MemoryContext;

mod harness;

#[test]
fn every_consumer_names_the_life_it_reads() {
    let mut files = harness::fixtures_in(&harness::plans_root());
    files.extend(harness::fixtures_in(&harness::plans_root().join("regress-full")));
    let (mut named, mut unnamed, mut groups) = (0, 0, 0);
    for f in files {
        let text = fs::read_to_string(&f).unwrap();
        let ctx = MemoryContext::new("fixture");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { continue };
        let arena = Arena::new(1 << 20);
        let Ok(g) = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { continue };
        let name = &fx.name;
        for (i, p) in g.pipelines.iter().enumerate() {
            if p.desc.consumes_from.is_empty() {
                unnamed += p.desc.consumes.len();
                continue;
            }
            assert_eq!(p.desc.consumes_from.len(), p.desc.consumes.len(), "{name} p{i}");
            for (&slot, &LifeId(l)) in p.desc.consumes.iter().zip(p.desc.consumes_from.iter()) {
                named += 1;
                let l = l as usize;
                assert!(l < i, "{name} p{i}: life p{l} is an earlier pipeline");
                assert_eq!(g.pipelines[l].desc.produces(), Some(slot), "{name} p{i}: life p{l} produces slot {slot}");
                assert!((l + 1..i).all(|m| g.pipelines[m].desc.produces() != Some(slot)), "{name} p{i}: no producer of slot {slot} between p{l} and p{i}");
            }
        }
        for gd in g.groups.iter() {
            groups += 1;
            assert_eq!(g.pipelines[gd.producer.0 as usize].desc.produces(), Some(gd.slot), "{name}: a group's life produces its slot");
            assert!(core::ptr::eq(g.group_of(gd.producer).unwrap(), gd), "{name}: group_of finds the group by its life");
        }
    }
    println!("lives: {named} named reads, {unnamed} unnamed (recursive work tables), {groups} groups");
    assert!(named > 1000 && groups > 100);
}

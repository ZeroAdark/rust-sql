//! No value changes state inside a branch region (`tpde-phi-coalescing.md`
//! "Open defect", found by columnar6): the phi moves of a conditional
//! branch's edges are emitted between `RegionBegin` and `RegionEnd`, on one
//! edge's path only, so a spill or a demotion there lands on that edge
//! while the compiler's linear state ("on the stack") flows into the other
//! edge's path too. `set_branch_region` set the region flag inside a
//! `tpde_assert_ne!`, compiled out of release builds: the release
//! compiler demoted a holder inside the edge `b19 -> b37` of the fixture
//! (the columnar e2e's heap twin `count(*) FROM (SELECT DISTINCT k FROM
//! h)` with the word probe as a diamond: `str x6, [x29, #0xd8]` between
//! the `cbnz` and the `b`, then `ldr x0, [x29, #0xd8]` after the join on
//! both paths — the fault at address 0). The fixture is that pipeline's
//! program as the lowerer dumped it; the compiler now records every
//! region and every demotion as regalloc events, and refuses (an error,
//! not a debug assertion) to spill or evict inside a region.
#![cfg(target_arch = "aarch64")]

use gravity_ir::text::parse_program;
use gravity_ir::*;
use mcx::MemoryContext;
use tpde_compiler::RegallocEvent;

fn fixture() -> Program {
    let text = include_str!("fixtures/keyfold_diamond_twin.txt");
    parse_program(text, &|_| Some(0x1000)).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn no_spill_or_demotion_inside_a_branch_region() {
    let p = fixture();
    let ctx = MemoryContext::new("gravity-tpde-test");
    let c = gravity_tpde::compile_recording(ctx.mcx(), &p).unwrap_or_else(|e| panic!("compile: {e:?}"));
    let mut regions = 0usize;
    let mut depth = 0i32;
    let mut inside = vec![];
    for e in c.regalloc() {
        match e.event {
            RegallocEvent::RegionBegin => {
                assert_eq!(depth, 0, "nested branch region");
                depth += 1;
                regions += 1;
            }
            RegallocEvent::RegionEnd => {
                assert_eq!(depth, 1, "region end without a begin");
                depth -= 1;
            }
            RegallocEvent::Spill | RegallocEvent::Demote if depth > 0 => inside.push((e.func, e.block, e.event)),
            _ => {}
        }
    }
    assert_eq!(depth, 0, "a branch region never ended");
    assert!(regions > 100, "the fixture's pipeline has many conditional branches (saw {regions} regions)");
    assert!(inside.is_empty(), "value state changed inside a branch region: {inside:?}");
}

/// The demotion the release compiler performed inside the edge is now a
/// demotion before the branch or none: the fixture compiles, and every
/// demotion it does perform sits outside a region.
#[test]
fn demotions_happen_outside_regions_only() {
    let p = fixture();
    let ctx = MemoryContext::new("gravity-tpde-test");
    let c = gravity_tpde::compile_recording(ctx.mcx(), &p).unwrap_or_else(|e| panic!("compile: {e:?}"));
    let mut depth = 0i32;
    for e in c.regalloc() {
        match e.event {
            RegallocEvent::RegionBegin => depth += 1,
            RegallocEvent::RegionEnd => depth -= 1,
            RegallocEvent::Demote => assert_eq!(depth, 0, "demotion inside a branch region (block {:?})", e.block),
            _ => {}
        }
    }
}

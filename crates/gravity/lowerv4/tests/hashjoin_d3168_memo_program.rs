//! D3168: the staged probe's memo (gravity_program `stage_memo`) never changes the program a lowering makes. A shape's
//! later lowerings (the memo's `Want`: a staged function no call chose; `Plain`: one that took no block) make its first
//! lowering's program, the one the code cache keys and the front key's IR check (exec admit `front_check`, a lowering
//! with no memo) compares; a later lowering unlike it missed the code cache once and failed that check (TPC-DS q77,
//! regress generated_virtual).
use gravity_lowerv4_it as common;

use gravity_lowerv4::face::strategy;
use std::sync::atomic::Ordering::Relaxed;

#[test]
fn a_shapes_memo_lowerings_make_its_memo_free_program() {
    let (was, mut on) = (strategy::off(), strategy::off());
    on.remove(strategy::JOIN_PROBE_STAGED); // the road on, whatever its default (this thread's lowerings)
    strategy::set_off(on);
    let (mut live, mut want, mut plain, mut bad) = (0, 0, 0, Vec::new());
    let fixtures = common::files(&common::plans_dir(), "plan").into_iter().filter(|(n, _)| n.starts_with("tpch") || n.starts_with("regress"));
    for (i, (name, text)) in fixtures.enumerate() {
        let lowered = |shape| {
            let l = common::lower_fixture_opts(&text, gravity_lower::LowerOpts { shape, ..Default::default() })?.ok()?;
            Some((common::pin_ir(&l), l.staged.as_slice().iter().filter(|s| s.1.is_some()).count(), l.staged.as_slice().iter().filter(|s| s.1.is_none()).count()))
        };
        let shape = Some(0xd316_8000_0000_0000_0000_0000_0000_0000 | i as u128);
        let Some((none, ..)) = lowered(None) else { continue };
        let Some((first, made, _)) = lowered(shape) else { continue };
        let skipped = gravity_program::code::STAGED_SKIPPED.load(Relaxed);
        let Some((later, _, wants)) = lowered(shape) else { continue };
        let skipped = (gravity_program::code::STAGED_SKIPPED.load(Relaxed) - skipped) as usize;
        (live, want, plain) = (live + made, want + wants, plain + skipped - wants);
        for (which, ir) in [("first", &first), ("later", &later)] {
            if *ir != none {
                bad.push(format!("{name}: its {which} lowering under the memo makes a program unlike the memo-free one"));
            }
        }
    }
    strategy::set_off(was);
    eprintln!("stage memo: {live} staged functions lowered, then {want} wanted and {plain} plain lowerings");
    assert!(bad.is_empty(), "{}", bad.join("\n"));
    assert!(live > 0 && want > 0 && plain > 0, "a memo arm went unchecked: {live} lowered, {want} wanted, {plain} plain");
}

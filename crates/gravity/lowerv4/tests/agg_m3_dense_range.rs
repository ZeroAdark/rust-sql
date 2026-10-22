//! dense.range R1 (V4R-151, M3-WORDS): the road engages only while on, and a road-off lowering is M1's. bench-065
//! (`id % 1000`, `id % 100000`, `id / 2` over the bench table) is lowered with its `id` NOT NULL (the fixture's `att` line
//! rewritten) and as captured (nullable), with the road on and off:
//! - on, NOT NULL: `id % 1000` and `id / 2` key one image word (the IR differs from the road-off lowering); `id % 100000`
//!   (a constant span past any T1) keeps the flags word and lowers as with the road off (face/keyed.rs `range_x`);
//! - off: the NOT NULL lowering's IR is `fixtures/m3/dense-range-off-<name>.ir` (LOWERV4_PIN=write re-records it after
//!   review). Before V4R-151 these statements refused `NotBuilt dense.range`, so no earlier IR exists; the road-off
//!   lowering takes M1's key image and table (`range_key` answers `None`), and the M1 / M2 IR pins stay byte-identical;
//! - nullable: the road changes nothing (a nullable key keeps its flags word and the hashed table).
use gravity_lowerv4_it as common;

use gravity_lowerv4::face::strategy::{self, StrategySet};

/// Each fixture and whether the road changes its NOT NULL lowering.
const FIXTURES: [(&str, bool); 3] = [("bench-065", true), ("bench-008", false), ("bench-069", true)];

fn ir(text: &str, off: &str) -> String {
    strategy::set_off(StrategySet::parse(off).unwrap_or_else(|e| panic!("road_off {off:?}: {e}")));
    let l = common::lower_fixture(text).expect("decomposes").unwrap_or_else(|e| panic!("lowers: {e}"));
    let t = gravity_ir::text::print_program(&l.ir);
    strategy::set_off(StrategySet::EMPTY);
    t
}

#[test]
fn dense_range_engages_only_while_on() {
    let write = std::env::var("LOWERV4_PIN").is_ok_and(|v| v == "write");
    let mut bad = Vec::new();
    for (name, moves) in FIXTURES {
        let text = std::fs::read_to_string(common::plans_dir().join(format!("{name}.plan"))).expect("fixture");
        let att = "-- att 16395 1 id typid=23 typmod=-1 len=4 byval=t notnull=f";
        assert!(text.contains(att), "{name}: the bench table's id attribute line");
        let notnull = text.replace(att, &att.replace("notnull=f", "notnull=t"));
        let (on, off) = (ir(&notnull, ""), ir(&notnull, "dense.range"));
        if (on != off) != moves {
            bad.push(format!("{name}: the NOT NULL key's lowering {} with dense.range on", if moves { "did not move" } else { "moved" }));
        }
        if ir(&text, "") != ir(&text, "dense.range") {
            bad.push(format!("{name}: the nullable key's lowering moved with dense.range on"));
        }
        let pin = common::milestone_dir(3).join(format!("dense-range-off-{name}.ir"));
        match std::fs::read_to_string(&pin) {
            _ if write => std::fs::write(&pin, &off).unwrap_or_else(|e| panic!("{}: {e}", pin.display())),
            Ok(p) if p == off => {}
            Ok(_) => bad.push(format!("{name}: the road-off IR differs from its pin {}", pin.display())),
            Err(e) => bad.push(format!("{name}: {}: {e}", pin.display())),
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// `id + 2`, `id - 2` and `2 + id` (bench-069's `id / 2` with int4pl / int4mi in its one OpExpr; `2 + id` swaps the
/// arguments): a NOT NULL key of each keys one image word while dense.range is on, and the nullable key lowers as with
/// the road off.
#[test]
fn shift_key_engages_only_while_on() {
    let text = std::fs::read_to_string(common::plans_dir().join("bench-069.plan")).expect("fixture");
    let div = ":opno 528 :opfuncid 154 ";
    assert_eq!(text.matches(div).count(), 1, "bench-069: the one int4div OpExpr");
    let att = "-- att 16395 1 id typid=23 typmod=-1 len=4 byval=t notnull=f";
    let mut bad = Vec::new();
    for (name, op, swap) in [("id + 2", ":opno 551 :opfuncid 177 ", false), ("id - 2", ":opno 555 :opfuncid 181 ", false), ("2 + id", ":opno 551 :opfuncid 177 ", true)] {
        let mut t = text.replace(div, op);
        if swap {
            let at = t.find(op).expect("the OpExpr");
            let (var, cst) = (at + t[at..].find("{VAR ").expect("the OpExpr's Var"), at + t[at..].find("{CONST ").expect("the OpExpr's Const"));
            assert!(var < cst, "{name}: the Var before the Const");
            let end = |at: usize| at + t[at..].find('}').expect("node end") + 1;
            let (v, c) = (t[var..end(var)].to_owned(), t[cst..end(cst)].to_owned());
            t = format!("{}{c}{}{v}{}", &t[..var], &t[end(var)..cst], &t[end(cst)..]);
        }
        let notnull = t.replace(att, &att.replace("notnull=f", "notnull=t"));
        if ir(&notnull, "") == ir(&notnull, "dense.range") {
            bad.push(format!("{name}: the NOT NULL key's lowering did not move with dense.range on"));
        }
        if ir(&t, "") != ir(&t, "dense.range") {
            bad.push(format!("{name}: the nullable key's lowering moved with dense.range on"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

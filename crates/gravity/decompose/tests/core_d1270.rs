//! D1270 (agg.rs `Shape::duplicate_keys`): `SELECT c1.y, c1.x FROM group_agg_pk c1 JOIN group_agg_pk c2 ON c1.x = c2.x
//! GROUP BY c1.y, c1.x, c2.x` (regress aggregates #110). The planner drops c2.x's pathkey (it equals c1.x through the
//! join's EquivalenceClass), so the Group node groups by c1.x, c1.y, and its target list carries c2.x as a resjunk
//! entry. `duplicate_keys` keyed resjunk entries only under an aggregate, so the group's row carried resnos 1 and 2 and
//! the lowerer refused "a column the row does not carry" on the emit's outer.3, with merge joins on or off. A Group node
//! is always a GROUP BY (DISTINCT plans a Unique or an Agg), so its junk entries are held to the grouping: c2.x joins the
//! keys (the same groups; its word is the value PostgreSQL emits).
use std::fs;

use gravity_decompose::{corpus, decompose_in, dump};
use gravity_program::Arena;
use mcx::MemoryContext;

fn graph(rel: &str) -> String {
    let path = manifest_dir().join("tests/plans").join(rel);
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let ctx = MemoryContext::new("d1270");
    let fx = corpus::load(ctx.mcx(), &text).unwrap();
    let arena = Arena::new(1 << 20);
    let mut out = String::new();
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).expect("decomposes");
    dump::dump(&g, &mut out).unwrap();
    out
}

/// Each `group slot=` block's key resnos, in slot order.
fn group_keys(dump: &str) -> Vec<Vec<u32>> {
    let mut out: Vec<Vec<u32>> = Vec::new();
    let mut open = false;
    for line in dump.lines() {
        if line.starts_with("group slot=") {
            out.push(Vec::new());
            open = true;
        } else if open && line.starts_with("  key ") {
            let resno = line.split_whitespace().find_map(|w| w.strip_prefix("resno=")).unwrap();
            out.last_mut().unwrap().push(resno.parse().unwrap());
        } else {
            open = false;
        }
    }
    out
}

#[test]
fn a_group_nodes_junk_column_equal_to_a_key_is_a_key() {
    let g = graph("regress-full/aggregates-110.plan");
    // grpColIdx (2 1): c1.x, c1.y; the junk c2.x (input column 3) joins them
    assert_eq!(group_keys(&g), vec![vec![2, 1, 3]], "{g}");
}

fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}

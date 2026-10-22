//! D1214 (V4R-393, passenger.rs `pk_determined` + agg.rs `Shape::pk_keys`): PostgreSQL's parser admits a bare column
//! beside the grouping keys when its relation's PRIMARY KEY is grouped (check_functional_grouping: get_primary_key_attnos,
//! contype 'p', not deferrable, the conkey among the columns every grouping set groups). The grouping-sets union rewrite
//! gives each set an Agg arm whose target list carries no sortgroupref, so `Shape::duplicate_keys` never saw the
//! dependency and the bare column refused "a column the row does not carry" in the lowerer (regress groupingsets #36).
//! Each arm now takes a bare Var of the key's own relation as a key when that key's columns are all the arm's keys.
//! Positives (captured, tests/d1214/sets-*.plan and plain-group-by-key.plan): the dependent columns are keys of every
//! arm. Negatives, hand-edited from them: the index not primary (unique-not-primary.plan: a UNIQUE key never admits)
//! and a second arm grouped by c alone (key-in-one-set.plan: the key covered by one set only); each keeps the bare
//! column, so the lowerer's "a column the row does not carry" refusal stands.
use std::fs;

use gravity_decompose::{corpus, decompose_in, dump};
use gravity_program::Arena;
use mcx::MemoryContext;

fn graph(name: &str) -> String {
    let path = manifest_dir().join("tests/d1214").join(format!("{name}.plan"));
    let text = fs::read_to_string(&path).unwrap();
    let ctx = MemoryContext::new("d1214");
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
fn a_column_of_a_primary_key_every_arm_groups_becomes_a_key_of_each_arm() {
    // SELECT a, d, grouping(a, b, c) FROM gp GROUP BY GROUPING SETS ((a, b), (a, c)); gp PRIMARY KEY (a). The planner
    // groups each arm by a alone (remove_useless_groupby_columns); b, c and d ride as keys through the key.
    let sorted = graph("sets-sorted");
    assert_eq!(group_keys(&sorted), vec![vec![1, 2, 3], vec![1, 2, 3]], "{sorted}");
    let hashed = graph("sets-hashed");
    assert_eq!(group_keys(&hashed), vec![vec![1, 4, 2], vec![1, 4, 3]], "{hashed}");
    // PRIMARY KEY (x, y), both columns in every set: d (resno 4) is a key of each arm.
    let two = graph("sets-two-column-key");
    assert_eq!(group_keys(&two), vec![vec![1, 2, 4], vec![1, 2, 4, 3]], "{two}");
    // gp JOIN gr: gp.d through the join (varnosyn names gp's range entry), keyed beside gp.a.
    let join = graph("sets-join");
    let keys = group_keys(&join);
    assert_eq!(keys.len(), 2, "{join}");
    assert!(keys.iter().all(|k| k.starts_with(&[1, 2])), "{join}");
    // Plain GROUP BY a: the target list's sortgroupref path (duplicate_keys) keys d already; pk_keys adds nothing.
    let plain = graph("plain-group-by-key");
    assert_eq!(group_keys(&plain), vec![vec![1, 4]], "{plain}");
}

#[test]
fn a_unique_key_or_a_key_one_set_leaves_out_keeps_the_bare_column() {
    // The same plan with the index UNIQUE but not primary: only the grouped a is a key; b / c / d stay bare.
    let unique = graph("unique-not-primary");
    assert_eq!(group_keys(&unique), vec![vec![1], vec![1]], "{unique}");
    // The hashed plan with its second arm grouped by c (resno 3), not a: the first arm still admits, the second keys c alone.
    let one = graph("key-in-one-set");
    assert_eq!(group_keys(&one), vec![vec![1, 4, 2], vec![3]], "{one}");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}

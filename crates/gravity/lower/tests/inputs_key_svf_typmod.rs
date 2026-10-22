// D1101 (D1099's program-cache audit, main 09:44Z / 10:17Z): the content key hashes a SQLValueFunction's precision. gravity_expr
// bakes the typmod of CURRENT_TIMESTAMP(p) / LOCALTIMESTAMP(p) / CURRENT_TIME(p) / LOCALTIME(p) into the program (an
// immediate), so two statements differing only in it must key apart; the key hashed the op alone, and CURRENT_TIMESTAMP(0)
// after CURRENT_TIMESTAMP(3) ran the precision-3 program (e2e m1_d1101_sqlvaluefunction_typmod).
use std::path::Path;

use gravity_decompose::{corpus, decompose_in};
use gravity_lower::inputs::key::key;
use gravity_program::{Arena, ContentKey};
use mcx::MemoryContext;

/// `INSERT INTO pgbench_history (tid, bid, aid, delta, mtime) VALUES (3, 1, 4242, 10, CURRENT_TIMESTAMP)`.
const PLAN: &str = "../decompose/tests/plans/oltpc-045.plan";
const SVF: &str = "{SQLVALUEFUNCTION :op 3 :type 1184 :typmod -1 ";

fn key_with(to: &str) -> ContentKey {
    let text = std::fs::read_to_string(manifest_dir().join(PLAN)).unwrap();
    assert_eq!(text.matches(SVF).count(), 1, "the fixture holds CURRENT_TIMESTAMP once");
    let text = text.replacen(SVF, to, 1);
    let ctx = MemoryContext::new("svf key");
    let fx = corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let arena = Arena::new(1 << 20);
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|r| panic!("decompose: {r}"));
    key(&g)
}

#[test]
fn the_precision_keys_apart() {
    // op 4 = SVFOP_CURRENT_TIMESTAMP_N: CURRENT_TIMESTAMP(3) and CURRENT_TIMESTAMP(0)
    let (p3, p0) = (key_with("{SQLVALUEFUNCTION :op 4 :type 1184 :typmod 3 "), key_with("{SQLVALUEFUNCTION :op 4 :type 1184 :typmod 0 "));
    assert_ne!(p3, p0, "CURRENT_TIMESTAMP(3) and CURRENT_TIMESTAMP(0) share a key");
    assert_eq!(p3, key_with("{SQLVALUEFUNCTION :op 4 :type 1184 :typmod 3 "), "one precision, one key");
    // the op keys apart as before: CURRENT_TIMESTAMP (op 3, no precision) is not CURRENT_TIMESTAMP(p)
    assert_ne!(key_with(SVF), p3);
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process).
fn manifest_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}

// lockrows queue-0012 (held-out tour-scans queue-0012; main's rule-4 consent for decompose ops/modifytable/lockrows.rs,
// condition 2): a LockRows whose marks are all REFERENCE or COPY (FOR UPDATE of a subquery PG plans apart: the outer
// LockRows carries only the subquery's COPY mark, the subquery's own LockRows takes the locks) maps to its child and its
// projection, with no lock op, as ExecInitLockRows leaves its lr_arowMarks empty there and never starts EvalPlanQual.
// A LockRows with a locking mark is unchanged: its graph is still byte-equal to the golden, lock op included.
use std::fs;
use std::path::PathBuf;

use gravity_decompose::graph::Op;
use gravity_decompose::{corpus, decompose_in, dump};
use gravity_program::Arena;
use mcx::MemoryContext;

/// FOR SHARE OF t SKIP LOCKED over t JOIN u: t's mark ROW_MARK_SHARE (locking), u's ROW_MARK_REFERENCE.
const FIXTURE: &str = "regress-068";
const SHARE_MARK: &str = ":markType 2 :allMarkTypes 4 :strength 2 :waitPolicy 1";
const REFERENCE_MARK: &str = ":markType 4 :allMarkTypes 16 :strength 0 :waitPolicy 0";

fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)")
}

/// The graph text (dump.rs, as tests/golden has it) and the number of lock ops over every pipeline; a refusal's line and 0.
fn decompose(text: &str) -> (String, usize) {
    let ctx = MemoryContext::new("fixture");
    let fx = corpus::load(ctx.mcx(), text).unwrap_or_else(|e| panic!("{FIXTURE}: {e}"));
    let arena = Arena::new(1 << 20);
    let mut out = format!("-- {}\n-- {}\n", fx.name, fx.sql);
    let r = match decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) {
        Ok(g) => {
            dump::dump(&g, &mut out).unwrap();
            let locks = g.pipelines.iter().flat_map(|p| p.ops.iter()).filter(|o| matches!(o, Op::Lock(_))).count();
            (out, locks)
        }
        Err(r) => {
            dump::dump_refusal(&r, &mut out).unwrap();
            (out, 0)
        }
    };
    r
}

fn fixture() -> String {
    fs::read_to_string(manifest_dir().join("tests/plans").join(format!("{FIXTURE}.plan"))).unwrap()
}

#[test]
fn a_lock_rows_with_a_locking_mark_keeps_its_lock_op() {
    let (graph, locks) = decompose(&fixture());
    let golden = fs::read_to_string(manifest_dir().join("tests/golden").join(format!("{FIXTURE}.graph"))).unwrap();
    assert_eq!(graph, golden, "{FIXTURE}: a LockRows with a locking mark decomposes as its golden");
    assert_eq!(locks, 1, "{FIXTURE}: one lock op\n{graph}");
    assert!(graph.contains("  lock rel=16395 Share Skip marks=(rti1:ROW_MARK_SHARE, rti2:ROW_MARK_REFERENCE)\n"), "{graph}");
}

#[test]
fn a_lock_rows_whose_every_mark_is_reference_passes_its_child() {
    let text = fixture();
    assert_eq!(text.matches(SHARE_MARK).count(), 2, "{FIXTURE}: t's mark in the LockRows and in the statement's rowMarks");
    let (graph, locks) = decompose(&text.replace(SHARE_MARK, REFERENCE_MARK));
    assert!(graph.contains("\ngraph pipelines="), "a LockRows with no locking mark decomposes:\n{graph}");
    assert_eq!(locks, 0, "no lock op (ExecLockRows locks nothing there):\n{graph}");
    assert!(!graph.contains("  lock "), "{graph}");
    assert!(graph.contains(" serial site=1 reason=Lock "), "the statement stays serial past the LockRows:\n{graph}");
    assert!(graph.contains("mapped: IndexScan NestLoop NestLoopParam LockRows PlanRowMark\n"), "{graph}");
}

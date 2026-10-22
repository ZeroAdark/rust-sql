//! D3347 (decompose self_spool.rs `spool_whole`): a nested loop over a CTE Scan of a store a UNION ALL fills, whose body
//! may raise (TPC-DS q04 / q11 / q74's year_total: `sum(amt - net)`, an int4 minus), is built once (D3014 B6, the keyed
//! form, probed by the join's own equality) when the nest's outer is a chain of such nests down to a CTE Scan of the same
//! CTE and above them a full Sort, then constant LIMITs, to the root: lowerv4's spool rule then reads the union whole
//! (rerun/union_reads.rs `spool_read`, `spool_build`), as PostgreSQL's first inner scan reads the store to its end. Was
//! per outer row: a store a union fills took B6 only where a Sort read it whole before the nest's first outer row.
//! Fixtures tests/d3347 (their `-- sql:` lines are the statements), captured by scripts/gravity-capture-plans.sh (corpus
//! srf over a fresh initdb holding ys (cust_id char(16), yr, kind, amt, net), 16200 rows; enable_hashjoin and
//! enable_mergejoin off, W0): chain-sorted.plan, ORDER BY .. LIMIT 15 over two nests (both keyed, by bpchareq: TPC-DS
//! customer_id is char(16)); chain-unsorted.plan, the same chain to the root with no sort (the Nests stay: the spool
//! rule reads a union whole only into a sort); count.plan, an aggregate over one nest (the Nest stays).
use std::fs;
use std::path::PathBuf;

use gravity_decompose::graph::{Op, PipelineNode};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::{corpus, decompose_in};
use gravity_program::Arena;
use mcx::MemoryContext;

/// bpchareq: the keyed probe's clause operator over the char(16) keys
const BPCHAR_EQ: u32 = 1054;

fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test")
}

fn walk<'a, 'p>(p: &'a PipelineNode<'p>, out: &mut Vec<&'a PipelineNode<'p>>) {
    out.push(p);
    for op in p.ops.iter() {
        if let Op::Join(JoinDesc::Nest { inner, .. }) = op {
            walk(inner, out);
        }
    }
}

/// The fixture's nest forms: the Nests kept, and each keyed probe's first clause operator.
fn forms(name: &str) -> (usize, Vec<u32>) {
    let path = manifest_dir().join("tests/d3347").join(format!("{name}.plan"));
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let ctx = MemoryContext::new("d3347");
    let fx = corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("{name}: {e}"));
    let arena = Arena::new(1 << 20);
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|r| panic!("{name}: refused {r}"));
    let mut ps = Vec::new();
    for p in g.pipelines.iter() {
        walk(p, &mut ps);
    }
    let (mut nests, mut keyed) = (0, Vec::new());
    for op in ps.iter().flat_map(|p| p.ops.iter()) {
        match op {
            Op::Join(JoinDesc::Nest { .. }) => nests += 1,
            Op::Join(JoinDesc::Probe { catalog: true, clauses, .. }) => {
                keyed.push(clauses.iter().next().and_then(|c| c.as_op_expr()).map_or(0, |o| o.opno));
            }
            _ => {}
        }
    }
    (nests, keyed)
}

#[test]
fn a_union_store_chain_into_a_sort_is_built_once() {
    // Limit -> Sort -> NestLoop (s1 x s2) x c1, every inner a CTE Scan of yt, the outermost read a CTE Scan of it too
    assert_eq!(forms("chain-sorted"), (0, vec![BPCHAR_EQ, BPCHAR_EQ]));
}

#[test]
fn a_union_store_chain_with_no_sort_above_keeps_its_nests() {
    assert_eq!(forms("chain-unsorted"), (2, vec![]));
}

#[test]
fn an_aggregate_over_a_union_store_nest_keeps_it() {
    assert_eq!(forms("count"), (1, vec![]));
}

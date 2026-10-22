//! D2592: a rerun set's marking hash probe (RIGHT / FULL / RIGHT SEMI / RIGHT ANTI: `marks`, which the join's drain reads)
//! over a table kept across the set's re-runs (its build in no set holding the probe). PG 18.6's ExecReScanHashJoin
//! reuses the one-batch table and resets its match flags at every rescan (ExecHashTableResetMatchFlags). v4 keeps the
//! marks in the build's region (L0-d, build.rs `MARKS`: one byte per record, the keyed ones then the NULL-key ones), laid
//! out once, so the probe zeroes them at the call that opens (the call's resume header still RESUME_NONE: frame.rs's
//! `stop::dispatch` reads the same word, after the prologue), in its prologue: before its first row can mark a record,
//! whatever rows the outer then has (an outer with no rows runs the prologue too). Not an open word: the frame writes those
//! after the prologue, so a resumed call (a probe of many windows) would read the open's fresh value and zero the marks
//! the first call set. The set's run re-opens its members and runs them on the leader, never as a section (parallel's
//! `members`: the lazy arm), and the probe's pipeline is serial (`StepMech::serial`: one call per run, where an eligible
//! one runs claim by claim and each claim's call opens, so a later claim would zero the marks the earlier ones set), so
//! the open call zeroes them once per run. A region not laid out yet (a lazy build still pending) holds no marks. A kept
//! table that may spill (a spill pair) keeps marks this does not reach: refused by name (`KEPT_SPILL`).
use gravity_abi::ctx::{hdr, RESUME_NONE};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use mcx::GrowIn;

use crate::frame::env::Env;
use crate::graph::Gx;
use crate::ops::hashjoin::build::{region, ALLOW, A_MARKS, KEPT, MARKS, N, PHASE};

/// The Shape of a member's marking probe over a kept table that may spill.
pub(crate) const KEPT_SPILL: &str = "a rerun set's RIGHT / FULL / RIGHT SEMI / RIGHT ANTI hash probe over a table kept across re-runs that may spill (its spilled partitions' match marks outlive a run, where PG's rescan resets them: ExecHashTableResetMatchFlags; D2592)";

/// A member's marking probe over a kept table: it zeroes the table's marks at each open.
pub(crate) struct Reset;

/// Whether probe `j` of pipeline `q` over build pipeline `b` marks a table kept across the re-runs of a set holding it.
pub(crate) fn over_kept(gx: &Gx<'_, '_>, q: usize, j: usize, b: usize) -> bool {
    let Some(Op::Join(JoinDesc::Probe { marks: true, catalog: false, .. })) = gx.pipelines()[q].ops.get(j) else { return false };
    gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(q as u16)) && !r.pipelines.contains(&(b as u16)))
}

impl Reset {
    /// At every call: the call that opens zeroes the marks of the build `consumed` reads, when its region is laid out
    /// (PHASE 2) with marks (ALLOW's A_MARKS) at a MARKS offset: N + KEPT bytes, words first, then the bytes past them.
    pub(crate) fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, consumed: usize) {
        let w = b.load(Type::I64, env.ctx, hdr::RESUME as i32);
        let none = b.iconst(Type::I64, RESUME_NONE);
        let fresh = b.icmp(IntCond::Eq, w, none);
        let (l_reset, l_on) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(fresh, (l_reset, &[]), (l_on, &[]));
        b.switch_to(l_reset);
        let r = region(b, env, Some(consumed));
        let phase = b.load_zext(Type::I8, r, PHASE as i32);
        let two = b.iconst(Type::I64, 2);
        let laid = b.icmp(IntCond::Eq, phase, two);
        let allow = b.load_zext(Type::I8, r, ALLOW as i32);
        let am = b.iconst(Type::I64, A_MARKS as i64);
        let bit = b.int_bin(IntBinOp::And, allow, am);
        let marked = b.icmp(IntCond::Ne, bit, env.zero);
        let at = b.load(Type::I64, r, MARKS as i32);
        let placed = b.icmp(IntCond::Ne, at, env.zero);
        let ok = b.int_bin(IntBinOp::And, laid, marked);
        let ok = b.int_bin(IntBinOp::And, ok, placed);
        let (n, kept) = (b.load(Type::I64, r, N as i32), b.load(Type::I64, r, KEPT as i32));
        let len = b.int_bin(IntBinOp::Add, n, kept);
        let len = b.select(ok, len, env.zero);
        let p = b.ptr_add(r, at, 1, 0);
        let three = b.iconst(Type::I64, 3);
        let words = b.int_bin(IntBinOp::Ushr, len, three);
        let wend = b.int_bin(IntBinOp::Shl, words, three);
        let z64 = b.iconst(Type::I64, 0);
        let zero = env.zero;
        let l_bytes = zero_loop(b, env, (p, zero, words, 8), z64);
        b.switch_to(l_bytes);
        let z8 = b.iconst(Type::I8, 0);
        let l_done = zero_loop(b, env, (p, wend, len, 1), z8);
        b.switch_to(l_done);
        b.br(l_on, &[]);
        b.switch_to(l_on);
    }
}

/// A loop storing `zero` (one `scale`-byte unit) at `p + scale * i` for `i` in `[from, to)`, entered from the current
/// block; the block after it (not switched to). Bounded by the table's records (`env.bounded`, as a probe's chain walk or a
/// function store's row loop): a store per unit, no call.
fn zero_loop(b: &mut FunctionBuilder<'_>, env: &mut Env, (p, from, to, scale): (Value, Value, Value, u8), zero: Value) -> gravity_ir::Block {
    let (l_head, l_body, l_out) = (b.create_block(&[Type::I64]), b.create_block(&[]), b.create_block(&[]));
    b.br(l_head, &[from]);
    b.switch_to(l_head);
    b.mark_loop_header(l_head);
    env.bounded.push_in(l_head);
    let i = b.block_params(l_head)[0];
    let more = b.icmp(IntCond::Ult, i, to);
    b.cond_br(more, (l_body, &[]), (l_out, &[]));
    b.switch_to(l_body);
    let q = b.ptr_add(p, i, scale, 0);
    b.store_unaligned(q, 0, zero);
    let next = b.int_bin(IntBinOp::Add, i, env.one);
    b.br(l_head, &[next]);
    l_out
}

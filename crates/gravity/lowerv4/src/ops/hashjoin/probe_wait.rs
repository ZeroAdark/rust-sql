//! The outer-first probe's waits on its build (join.inner, probe_inner.rs): op 14's held error raised at the first row to
//! reach the probe (`raise_failed`), and D824's lazy build (V4R-298): the first row stops the call with `BUILD_WAIT` while
//! the build is pending, the runner runs it and resumes the row (`wait`). Split from join/probe_inner.rs (D824). Owner M1-JOIN.
use gravity_abi::ctx::{hdr, FLAG_PENDING_SHIFT};
use gravity_abi::status::{build_wait, raise};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, Type, Value};
use mcx::{GrowIn, PgVec};

use crate::ops::shared::lazy::{LazyImpl, LazyMech};
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::frame::row::RowVals;

use crate::ops::hashjoin::build::FAILED;

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::JoinInner, &LazyImpl(&Lazy))], ..crate::ops::NONE };

/// D824's lazy build for another kind (merge.join's lazy inner, MERGE R3 (ii)): build_class.rs's group, this file's wait.
struct Lazy;

impl LazyMech for Lazy {
    fn group<'g>(&self, px: &Px<'g, '_>, b: usize, q: usize, consumed: usize, slot: u8, first: &[u8]) -> Option<PgVec<'g, usize>> {
        let before = crate::ops::hashjoin::build_class::feed(px.gx, q, first.iter().copied());
        // D2026: a UNION ALL's arms all fill the merge's inner run (PG's Sort reads its Append whole at its first fetch), so
        // every arm waits with its producers: each arm's group is taken with the other arms beside the outer's feeders
        // (they run in the same wait, not ahead of it), and the run's group is their union
        let arms = px.gx.run_arms(b);
        let mut group = PgVec::new_in(px.mcx());
        for &x in arms.iter().chain([&b]) {
            let mut by = mcx::to_vec_in(&before[..], px.mcx());
            by.extend_in(arms.iter().chain([&b]).copied().filter(|&y| y != x));
            group.extend_in(crate::ops::hashjoin::build_class::lazy_group(px, x, q, consumed, slot, &by[..])?);
        }
        group.sort_unstable();
        group.dedup();
        Some(group)
    }

    fn wait(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, offset: i32, consumed: usize) {
        wait(b, env, row, offset, consumed)
    }
}

/// D824: the offset word's tag while a lazy build runs: the row re-enters (its held values read back), no match handed on.
const WAIT: i64 = 1 << 62;

/// Op 14: a build whose merge held its error (FORM FAILED) raises it here, at the first row to reach the probe, where PG's
/// outer-first hash join would have built; the driver re-stashes the held error for this participant (`raise_held`).
pub(crate) fn raise_failed(b: &mut FunctionBuilder<'_>, env: &Env, form: Value) {
    let ff = b.iconst(Type::I64, FAILED as i64);
    let failed = b.icmp(IntCond::Eq, form, ff);
    let (l_fail, l_go) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(failed, (l_fail, &[]), (l_go, &[]));
    b.switch_to(l_fail);
    b.mark_cold(l_fail);
    let flags = HostFlags { may_error: true, ..HostFlags::default() };
    let f = gravity_abi::host_words!(gravity_kernels::host_abi::raise_held(ctx));
    let h = b.declare_host_once("raise_held", &[Type::Ptr], Some(Type::I64), f, flags);
    b.call_host(h, &[env.ctx], Some(Type::I64));
    let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
    b.br(env.l_raise, &[e]);
    b.switch_to(l_go);
}

/// D824 (V4R-298): the lazy build still pending, the row stops the call inside itself (`BUILD_WAIT`, the offset word
/// tagged `WAIT` so its held volatile values are read back, never re-run); the runner runs the build and resumes here,
/// where the tag is cleared and the row probes the built table (an empty one ends the pipeline: `prefix_safe` is off).
pub(crate) fn wait(b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, offset: i32, consumed: usize) {
    let fl = b.load_zext(Type::I32, env.ctx, hdr::FLAGS as i32);
    let bit = b.iconst(Type::I64, 1i64 << (FLAG_PENDING_SHIFT as usize + consumed));
    let m = b.int_bin(IntBinOp::And, fl, bit);
    let pending = b.icmp(IntCond::Ne, m, env.zero);
    let (l_wait, l_go) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(pending, (l_wait, &[]), (l_go, &[]));
    b.switch_to(l_wait);
    b.mark_cold(l_wait);
    let tag = b.iconst(Type::I64, WAIT);
    env.store_word(b, offset, tag);
    let st = b.iconst(Type::I64, build_wait(consumed as u8));
    env.stop(b, &row.carry, row.at, st);
    b.switch_to(l_go);
    let off = env.load_word(b, offset);
    let tag = b.iconst(Type::I64, WAIT);
    let tagged = b.icmp(IntCond::Eq, off, tag);
    let off = b.select(tagged, env.zero, off);
    env.store_word(b, offset, off);
}

//! The group walk's row skip (V4R-177, D518 lever D; the ladder's `skip_past`). Under a group walk (runs/walk.rs,
//! `WalkMech::group`), the keyed end drops a row whose first key lies past the worker's bound: the row does not fold.
//! - The bound is the `n`-th smallest distinct first key the worker's table holds, in its records and its partition
//!   buffers (abi `T1::nth_smallest_key_held`, the partial's bound the runner stops the walk at). It is read once, at each
//!   claim's open. Within a section the held keys only grow, so the bound only falls, and a claim's stale bound is still
//!   at least the owners' cut (exec `v4/m3_walk.rs`: the `n`-th smallest distinct first key of the merged runs, which is at
//!   most every worker's bound). A skipped row's group lies past the cut, and the cut drops it: its rows never show.
//! - The skip comes after the pipeline's steps (the WHERE ran). It applies only when nothing the fold evaluates past the
//!   first key can raise (D449, the effects home's `visible::agg::raise_free`): every later key and every argument and
//!   FILTER is a column or an expression that raises on no row and calls no user or VOLATILE code; no call has a
//!   DISTINCT or ORDER BY; every class is one whose row fold cannot raise (`folds`); and the group is not a DISTINCT,
//!   two-pass, grouping-set, marked or num.window group. Otherwise every row folds, as before.
//! - GP-1b: a descending walk (`ops::shared::agg::group_desc`) passes `flip = !0`: the bound is the `n`-th smallest NOT of the key
//!   and the row compares the NOT of its key image, so the ascending tests hold unchanged.
//!
//! Host: `walk_bound` (raw.rs). Registers nothing. V4R-177. Budget 120 lines.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, Type, Value};
use mcx::{CollectIn, PgVec};

use crate::table::face::TableDecl;
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::rep::{self, CellRef, Scope};

use crate::ops::agg::hashagg::{Arg, GroupRec};

/// A walked group's row skip: the walk's `n`, the first key's column in the producer's row, and the offset of the bound
/// word in the end's state, and the key flip (GP-1b: `!0` for a descending walk, else 0).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Skip {
    n: u32,
    col: i16,
    off: i32,
    flip: i64,
}

/// The skip for `rec`'s end on pipeline `k`, which has no code front, admit or split: `Some` when the pipeline walks
/// the group and the fold is raise-free. `off` is the end's state bytes before the bound word.
pub(crate) fn plan(px: &Px<'_, '_>, k: usize, rec: &GroupRec<'_, '_>, off: u32) -> Option<Skip> {
    let (n, _) = crate::face::consts::walk_group(px, k, rec.g)?;
    let Some(Some(Arg::Col(col))) = rec.ksrc.first() else { return None };
    let flip = if crate::ops::shared::agg::group_desc(rec.g) { !0 } else { 0 };
    crate::effects::visible::agg::raise_free(px.gx, rec).then_some(Skip { n, col: *col, off: off as i32, flip })
}

impl Skip {
    /// At the claim's open, after the table's: the bound into the end's state.
    pub(crate) fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl) {
        let st = crate::table::bypass::state(b, env, t);
        let flags = HostFlags { touches_memory: true, ..HostFlags::default() };
        // CONVERT (cvagg): the ctx names the breaker slot whose operator the host's scratch is charged to
        let h = b.declare_host_once("walk_bound", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64], Some(Type::I64), super::raw::walk_bound as *const () as usize, flags);
        let (n, flip) = (b.iconst(Type::I64, self.n as i64), b.iconst(Type::I64, self.flip));
        let bound = b.call_host(h, &[env.ctx, st, n, flip], Some(Type::I64)).expect("walk_bound returns the bound");
        let base = env.state.expect("the end's state slot is laid before its open");
        b.store(base, self.off, bound);
    }

    /// The row through `fold` unless its first key's image lies past the bound (signed, as `nth_smallest_key_held`
    /// orders the image words; GP-1b: the image XOR the flip); a skipped row leaves the carry as it was, like a filtered row.
    pub(crate) fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, rec: &GroupRec<'_, '_>, row: &RowVals, fold: impl FnOnce(&mut FunctionBuilder<'_>, &mut Env<'e, '_>) -> Carry<'e>) -> Carry<'e> {
        let c = row.col(self.col).unwrap_or_else(|| crate::refuse!(rec.node, crate::admit::refusal::Reason::Bug { payload: "a walked group's first key the row does not carry" }));
        let mut cx = env.lx(b, rec.node);
        let w = rep::hash_image(&mut cx, CellRef { null: None, ..c }, &rec.tfs[0], Scope::Program).w[0];
        env.memo = cx.memo;
        let w: Value = w.expect("an integer key's image is one word");
        let base = env.state.expect("the end's state slot is laid before its rows");
        let bound = b.load(Type::I64, base, self.off);
        // GP-1b: a descending walk compares the NOT of the image (ascending emits nothing: its IR is unchanged)
        let w = if self.flip != 0 { let f = b.iconst(Type::I64, self.flip); b.int_bin(IntBinOp::Xor, w, f) } else { w };
        let past = b.icmp(IntCond::Sgt, w, bound);
        let types: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let (l_join, l_fold) = (b.create_block(&types[..]), b.create_block(&[]));
        b.cond_br(past, (l_join, &row.carry.params()), (l_fold, &[]));
        b.switch_to(l_fold);
        let c = fold(b, env);
        b.br(l_join, &c.params());
        b.switch_to(l_join);
        env.memo.clear(); // l_fold's values do not reach here
        Carry::from_params(b.block_params(l_join), env.mcx)
    }
}

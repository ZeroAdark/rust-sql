//! The fill of an outer probe's miss: psql's catalog lookup (V4R-142, nest.catalog; M3 decision 16) and the outer join
//! types (M7 L0-c: LEFT and FULL null-extend; ANTI hands the outer row on null-extended too, PG's hj_NullInnerTupleSlot, so
//! an inner column read above it, a write's mark ctid (D1933B), is NULL). join/probe_inner.rs walks the build and sends
//! every row that met no record here: a NULL key, an EMPTY build, a walk that handed nothing on (for ANTI: found no
//! record passing the join qual). A fill type never ends early (R5): an empty build ends nothing, every outer row runs on
//! and misses. The offset word (probe_inner's match count, H17 c) is set before the filled row goes on, so a stop inside it
//! resumes at the row, finds the word set, and does not hand it on twice (R4); the caller zeroes the word after. Under a
//! join qual with volatile calls the word takes `FILLED`, and a row re-entered for a later step with `FILLED` set goes
//! straight to its fill: the walk never runs the qual again (PG ran it once per candidate). Owner M1-JOIN (M3-KEYED's
//! split; M7-PROBE's L0-c). Budget 90 lines.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};
use types_nodes::jointype::JoinType;
use gravity_program::LifeId;
use mcx::{CollectIn, GrowIn, PgVec};

use crate::cell::{Cell, Nullable};
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::row::{Carry, RowVals};

use crate::ops::hashjoin::build_side::Shape;

/// The offset word of a row whose fill went on under a volatile join qual (`Fill::tag`).
pub(crate) const FILLED: i64 = 1 << 61;

/// A fill's form: LEFT / FULL hand the null row on a miss, ANTI the outer row; a catalog INNER drops it. `tag`: `FILLED`.
/// `keys` (D1908): LEFT / FULL / ANTI outer keys that are not `keys_quiet` run for every outer row, an EMPTY build's too.
#[derive(Clone, Copy)]
pub(crate) struct Fill {
    pub left: bool,
    pub anti: bool,
    pub tag: bool,
    pub keys: bool,
}

impl Fill {
    /// A probe's fill (`None` for INNER and SEMI) and its `prefix_safe`: a fill type never ends early. `held`: the join
    /// qual's volatile words; `quiet`: keys.rs `keys_quiet`.
    pub(crate) fn of(catalog: bool, jt: JoinType, prefix_safe: bool, held: u8, quiet: bool) -> (Option<Fill>, bool) {
        let (left, anti) = (matches!(jt, JoinType::JOIN_LEFT | JoinType::JOIN_FULL), jt == JoinType::JOIN_ANTI);
        let fill = (catalog || left || anti).then_some(Fill { left, anti, tag: held > 0 && !catalog, keys: !quiet && !catalog && (left || anti) });
        (fill, prefix_safe && fill.is_none())
    }
}

/// The block a fill type's misses meet at (`l_join` for every other probe); an EMPTY build (`form` 0) goes there, and a
/// tagged row re-entered with `FILLED` set (`offset`: the word). Under `Fill::keys` only a row re-entered after its fill
/// went on (the word is 0 at a first entry) goes now, its keys ran then; the EMPTY test returns for a miss after the keys.
pub(crate) fn entry(b: &mut FunctionBuilder<'_>, env: &mut Env, fill: Option<Fill>, (form, offset): (Value, i32), l_join: Block, ctypes: &[Type], params: &[Value]) -> (Block, Option<Value>) {
    let Some(fill) = fill else { return (l_join, None) };
    let (l_miss, l_go) = (b.create_block(ctypes), b.create_block(&[]));
    let mut miss = b.icmp(IntCond::Eq, form, env.zero);
    if let Some(later) = fill.tag.then(|| crate::ops::hashjoin::probe::later_set(b, env)).flatten() {
        let (off, t) = (env.load_word(b, offset), b.iconst(Type::I64, FILLED));
        let filled = b.icmp(IntCond::Eq, off, t);
        let again = b.int_bin(IntBinOp::And, filled, later);
        miss = b.int_bin(IntBinOp::Or, miss, again);
    }
    let on = fill.keys.then(|| env.load_word(b, offset)).map(|o| b.icmp(IntCond::Ne, o, env.zero));
    let first = on.map_or(miss, |on| b.int_bin(IntBinOp::And, miss, on));
    b.cond_br(first, (l_miss, params), (l_go, &[]));
    b.switch_to(l_go);
    (l_miss, fill.keys.then_some(miss))
}

/// The walk's end (its carry `ce`) and the miss block: for a fill type the walk's end joins the misses, and a LEFT / FULL /
/// ANTI row goes on with every payload cell NULL unless the offset word says it already went on (a resumed
/// row, or a row the walk handed on or, for ANTI, matched); the carry after. Every other probe: `ce`.
pub(crate) fn miss<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, (fill, node): (Option<(Fill, i32, &Shape, LifeId)>, i32), (ce, l_miss): (Carry<'e>, Block), row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
    let Some((fill, offset, sh, life)) = fill else { return ce };
    b.br(l_miss, &ce.params());
    b.switch_to(l_miss);
    let carry = Carry::from_params(b.block_params(l_miss), env.mcx);
    if !fill.left && !fill.anti {
        return carry;
    }
    let ctypes: PgVec<'_, Type> = carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
    let (l_fill, l_after) = (b.create_block(&[]), b.create_block(&ctypes[..]));
    let off = env.load_word(b, offset);
    let fresh = b.icmp(IntCond::Eq, off, env.zero);
    // D688: a later expanding step stopped inside the null row (its offset word set): the row goes on again
    let fresh = crate::ops::hashjoin::probe::later_set(b, env).map_or(fresh, |x| b.int_bin(IntBinOp::Or, fresh, x));
    b.cond_br(fresh, (l_fill, &[]), (l_after, &carry.params()));
    b.switch_to(l_fill);
    let mark = if fill.tag { b.iconst(Type::I64, FILLED) } else { env.one };
    env.store_word(b, offset, mark);
    let null = b.bconst(true);
    let m = *row.vals.allocator();
    let mut r = RowVals { row: crate::cell::Row { cells: mcx::to_vec_in(&row.row.cells[..], m) }, vals: mcx::to_vec_in(&row.vals[..], m), cols: mcx::to_vec_in(&row.cols[..], m), carry: carry.copy_in(m), next: row.at, at: row.at };
    let base = r.row.cells.len();
    // join.payload_fp: a code column whose Fp128 the record carries goes on linked to `[0, 0]`, a NULL record's words, as the
    // hit's goes on linked to its record's (walk.rs `joined`): the steps after see one cell form on both paths
    let fps = env.fps;
    for (i, c) in sh.kept.borrow().iter().enumerate() {
        r.row.cells.push_in(Cell::of_record(c.rep, c.typid, c.num, crate::ops::hashjoin::build_side::home_of(c, life), i as u16, Nullable::Maybe));
        let w = match sh.fp_at(i) {
            Some(_) => {
                let w = b.iconst(Type::I64, 0);
                env.fps.add(w, [env.zero, env.zero]);
                w
            }
            None => env.zero,
        };
        r.vals.push_in((w, Some(null)));
    }
    let width = sh.payload.iter().copied().max().unwrap_or(0).max(0) as usize;
    let mut map = mcx::pg_vec![in env.mcx; None; width];
    for (i, &a) in sh.payload.iter().enumerate() {
        map[a as usize - 1] = Some((base + i) as u16);
    }
    // D3747: a LEFT row's NULL-extended record stays in reach of a write's EvalPlanQual redo (epq_ir.rs `Redo::Probe`
    // `left`): no build row matched, so no test tuple, and the redo hands the newest version on NULL-extended again
    let held = (fill.left && !sh.swapped).then(|| (mcx::to_vec_in(&r.row.cells[base..], env.mcx), mcx::to_vec_in(&r.vals[base..], env.mcx), map.iter().map(|c| c.map(|x| x - base as u16)).collect_in(env.mcx), crate::frame::env::Filled::Yes));
    let held = held.map(|h| env.redo_nests.push_in((node, h))).is_some();
    let saved = env.inner.replace(map);
    let c = next(b, env, &r);
    env.inner = saved;
    env.fps = fps;
    if held {
        env.redo_nests.pop();
    }
    b.br(l_after, &c.params());
    b.switch_to(l_after);
    Carry::from_params(b.block_params(l_after), env.mcx)
}

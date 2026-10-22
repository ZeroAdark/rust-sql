//! The checks a Limit makes and the feeders above it repeat, which the result gate and the limit step both read (lint RA):
//! a bound as the program reads it (`Lim`), a soft verdict on one that raises nothing (`soft`), a feeder's bind-time
//! bound (`bind`), whether a gate qual may not be evaluated twice (`calls`), and the checks of the nodes above an op
//! (`Above`), and the cap a CTE's Limit over its row lock takes from the INSERT that reads the CTE under a LIMIT
//! (`cte_lock_cap`, read by the lock's admission and the limit step). PostgreSQL: nodeLimit.c `recompute_limits`,
//! nodeResult.c's one-time qual.
use gravity_decompose::graph::Bound;
use gravity_decompose::Op;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use gravity_lower::Place;
use mcx::{CollectIn, GrowIn, Mcx, PgVec};

use crate::frame::env::Env;
use crate::frame::px::{ExId, Px};
use crate::frame::row::{Carry, RowVals};
use crate::lifetime::bound::{self, Feed};

/// A bound as the program reads it.
#[derive(Clone, Copy)]
pub(crate) enum Lim {
    All,
    Const(i64),
    /// A bind-time parameter word (its null byte beside it).
    Word(u16),
    Open(crate::rerun::params_bound::Computed), // D1016 row 5: over executor parameters, read once per open past it (`late`)
}

/// D1914 / D1755: the checks of the nodes above op `j` in pipeline `k` itself, outermost first, as PG makes them before it
/// pulls the node below: a Limit whose check may stop the call (`frame::iface::limit_stopping`: PG's Limit evaluates its bounds first and, at a
/// zero count, never pulls its subplan) and a one-time gate (PG's Result evaluates its qual first and, false or NULL, never
/// pulls its outer). The steps of one pipeline make their prologues in op order, innermost first, so op `j`'s step (a gate:
/// result/gate.rs; a Limit whose bound may raise) makes these before its own evaluation: below a stopped node nothing is
/// evaluated (no raise, no volatile call, no InitPlan run). Soft: a negative bound stops here and raises nothing (that
/// Limit's own step raises it), a bound read per open is left to its own step (`bind`). A gate's quals are evaluated here
/// and again by its own step, so a gate whose qual calls (`calls`, D2571: a second evaluation would show) is left out. The
/// pipelines below are the feeder's (result/gate.rs `limit_feeder`).
pub(crate) struct Above<'g>(PgVec<'g, Pre<'g>>);

/// One check above: a Limit's soft (count, offset), or a one-time gate's quals compiled at its node.
enum Pre<'g> {
    Limit(Lim, Lim),
    Gate(PgVec<'g, ExId>, i32),
}

impl<'g> Above<'g> {
    pub(crate) fn none(mcx: Mcx<'g>) -> Above<'g> {
        Above(PgVec::new_in(mcx))
    }

    pub(crate) fn of(px: &mut Px<'g, '_>, k: usize, j: usize) -> Above<'g> {
        let (gx, mcx) = (px.gx, px.mcx());
        let p = &gx.pipelines()[k];
        let mut pre = PgVec::new_in(mcx);
        for i in (j + 1..p.ops.len()).rev() {
            let node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= i).map_or(p.node_id, |n| n.node_id);
            match &p.ops[i] {
                Op::Gate(n) if !crate::effects::visible::result::qual_calls(gx, *n) => {
                    let quals: PgVec<'g, ExId> = match n.as_list() {
                        Some(l) => l.iter().map(|q| crate::step::expr::compile(px, q, None, node)).collect_in(mcx),
                        None => mcx::pg_vec![in mcx; crate::step::expr::compile(px, *n, None, node)],
                    };
                    pre.push_in(Pre::Gate(quals, node));
                }
                op => pre.extend_in(crate::frame::iface::limit_stopping(op).and_then(|(c, o)| bind(px, k, c).zip(bind(px, k, o))).map(|(c, o)| Pre::Limit(c, o))),
            }
        }
        Above(pre)
    }

    /// The outer checks' stop, a Bool: the feeder's (`env.stopped`, D2056), else each of these, outermost first while none
    /// has stopped; `None` when there is neither.
    pub(crate) fn stop(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Option<Value> {
        if self.0.is_empty() {
            return env.stopped;
        }
        let l_stop = b.create_block(&[Type::Bool]);
        if let Some(s) = env.stopped {
            let (l_more, t) = (b.create_block(&[]), b.bconst(true));
            b.cond_br(s, (l_stop, &[t]), (l_more, &[]));
            b.switch_to(l_more);
        }
        let none = RowVals { row: crate::cell::Row::new_in(env.mcx), vals: mcx::PgVec::new_in(env.mcx), cols: mcx::PgVec::new_in(env.mcx), carry: Carry::new(env.zero, env.mcx), next: env.zero, at: env.zero };
        for c in self.0.iter() {
            match c {
                Pre::Limit(count, offset) => {
                    let (o, n) = (soft(b, env, *offset, IntCond::Slt), soft(b, env, *count, IntCond::Sle));
                    let (stop, l_more, t) = (b.int_bin(IntBinOp::Or, o, n), b.create_block(&[]), b.bconst(true));
                    b.cond_br(stop, (l_stop, &[t]), (l_more, &[]));
                    b.switch_to(l_more);
                }
                // each qual only while every earlier one was true, a false or NULL one stopping
                Pre::Gate(quals, node) => {
                    for &q in quals.iter() {
                        let p = crate::step::expr::pred(b, env, *node, &none, q);
                        let (l_more, t) = (b.create_block(&[]), b.bconst(true));
                        b.cond_br(p, (l_more, &[]), (l_stop, &[t]));
                        b.switch_to(l_more);
                    }
                }
            }
        }
        let f = b.bconst(false);
        b.br(l_stop, &[f]);
        b.switch_to(l_stop);
        Some(b.block_params(l_stop)[0])
    }
}


/// A feeder's bound in pipeline `q`: a constant or a bind-time word (a per-open parameter is read only after the feeders
/// run: `None`). A bound a rerun set re-reads per open (D1634 `rebinds`) is never a bind-time word: PG evaluates it only
/// when the Limit runs, so a word bound at bind would evaluate it once more per statement.
pub(crate) fn bind<'p>(px: &mut Px<'_, 'p>, q: usize, b: &Bound<'p>) -> Option<Lim> {
    match *b {
        Bound::All => Some(Lim::All),
        Bound::Const(v) => Some(Lim::Const(v)),
        Bound::Expr(_) if bound::rebinds(px.gx, q, b) => None,
        Bound::Expr(e) => {
            let node = px.gx.pipelines()[q].node_id;
            if let Place::Word(w) = bound::word(px, Feed::Limit, q, e, node) { Some(Lim::Word(w)) } else { None }
        }
    }
}

/// A soft check's verdict on one bound, raising nothing: set (a NULL LIMIT is none, a NULL OFFSET 0) and `cond` 0.
pub(crate) fn soft(b: &mut FunctionBuilder<'_>, env: &Env, l: Lim, cond: IntCond) -> Value {
    let (v, set) = match l {
        Lim::All | Lim::Open(_) => return b.bconst(false), // `bind` never makes an Open (D1016 row 5: read past the feeders)
        Lim::Const(n) => (b.iconst(Type::I64, n), b.bconst(true)),
        Lim::Word(w) => {
            let nb = b.load_zext(Type::I8, env.params_null, w as i32);
            (b.load(Type::I64, env.params, w as i32 * 8), b.icmp(IntCond::Eq, nb, env.zero))
        }
    };
    let hit = b.icmp(cond, v, env.zero);
    b.int_bin(IntBinOp::And, hit, set)
}

/// CTELIMIT (lockrows_ctelock: `WITH c AS (SELECT id FROM job .. ORDER BY prio, id LIMIT 8 FOR UPDATE) INSERT INTO log
/// SELECT id, 'w' FROM c LIMIT 2`): `(j, n)` when pipeline `k` is a CTE's that writes nothing and holds its row lock with a
/// Limit at op `j` after it (only projections between and after them; constant bounds of 0 or more, no WITH TIES), and the
/// statement reads the CTE's store only through the INSERT's own pipeline: that pipeline scans the store as its source (no
/// other pipeline or nest inner consumes it) and runs only projections before a Limit of such bounds, reading `n` = offset
/// + count rows. PostgreSQL's Limit fetches no row past its count (nodeLimit.c tests it before the next fetch; a count of 0
/// never pulls its child), so the CteScan pulls the CTE's Limit, and through it the LockRows, for those `n` rows alone: the
/// CTE's Limit counting no further than `n` (limit/lower.rs `make`) locks the same rows and evaluates no row PostgreSQL does
/// not. A negative or run-time bound in the CTE (an error or a value only PostgreSQL's pull reads) gets no cap.
pub(crate) fn cte_lock_cap(gx: &crate::graph::Gx<'_, '_>, k: usize) -> Option<(usize, i64)> {
    use crate::frame::iface::demand;
    let ps = gx.pipelines();
    let rows = |o: &Op<'_>| demand(o).filter(|d| !d.ties).and_then(|d| d.rows());
    let proj = |o: &Op<'_>| matches!(o, Op::Project(_));
    if k >= ps.len() || !gx.cte_write(k) || ps[k].ops.iter().any(|o| matches!(o, Op::Write(_))) {
        return None;
    }
    let s = ps[k].desc.produces()?;
    let ops = &ps[k].ops;
    let lock = ops.iter().position(|o| matches!(o, Op::Lock(_)))?;
    let j = lock + 1 + ops[lock + 1..].iter().position(|o| !proj(o))?;
    if rows(&ops[j]).is_none() || !ops[j + 1..].iter().all(proj) {
        return None;
    }
    let mut readers = (0..ps.len()).filter(|&q| q != k && ps[q].desc.consumes.contains(&s));
    let (Some(r), None) = (readers.next(), readers.next()) else { return None };
    let p = &ps[r];
    let insert = p.ops.iter().any(|o| matches!(o, Op::Write(w) if w.cmd == gravity_program::Cmd::Insert));
    let inits = gx.with_producers(gx.graph.init_plans.iter().map(|i| i.pipeline as usize));
    if inits.contains(&r) || !insert || !matches!(p.source, gravity_decompose::Source::Breaker { slot } if slot == s) || ps.inners().iter().any(|x| x.1.desc.consumes.contains(&s)) {
        return None;
    }
    let n = p.ops.iter().find(|o| !proj(o)).and_then(rows)?;
    Some((j, i64::try_from(n).unwrap_or(i64::MAX)))
}

//! RIGHT / FULL / RIGHT ANTI merge join (MERGE-2, D1267; MERGE-3, D1349; nodeMergejoin.c's MJFillInner and
//! EXEC_MJ_ENDOUTER) and the rows merge.join's step hands on besides its machine's states (merge/mergejoin.rs). The
//! inner is filled where PG fills it: before a fetch moves past a current inner tuple nothing matched (the
//! `mj_MatchedInner` word), as INITIALIZE_INNER meets a non-matchable one, and at the outer's end (ENDOUTER, the step's
//! flush: the frame's input-end path) for the rest. One word is exact: the planner makes a RIGHT / FULL / RIGHT ANTI
//! merge join only when every join clause is a merge clause or a constant (select_mergejoin_clauses), so a restore to
//! the mark rescans tuples the outer's group matched already. The filled row is the inner's cells after the outer's,
//! each outer cell NULL in the shape its row came. Owner M11-MERGE. Budget 160 lines.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntCond, Type, Value};
use mcx::{CollectIn, GrowIn};
use types_nodes::jointype::JoinType;
use types_nodes::Node;

use crate::ops::mergejoin::lower::{later_set, Merge, DONE, ENDINNER, ENDOUTER, FILLED, FILLED_INNER, INIT_OUTER, JOINED, NEXTOUTER, SKIPOUTER_ADVANCE, W_IP, W_MI, W_MO, W_ST};
use crate::cell::Row;
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::row::{Carry, RowVals};
use crate::admit::refusal::Reason;

/// nodeMergejoin's mj_FillInner: RIGHT, RIGHT ANTI (MERGE-3, D1349) and FULL fill the inner's unmatched tuples.
pub(crate) fn fills_inner(jt: JoinType) -> bool {
    matches!(jt, JoinType::JOIN_RIGHT | JoinType::JOIN_RIGHT_ANTI | JoinType::JOIN_FULL)
}

/// A RIGHT / FULL join qual past the merge clauses is a constant (FULL JOIN ON false) or the plan is not the planner's.
pub(crate) fn qual_ok<'p>(fill_inner: bool, mut qual: impl Iterator<Item = Node<'p>>) -> Result<(), Reason> {
    match !fill_inner || qual.all(|q| q.as_const().is_some()) {
        true => Ok(()),
        false => Err(Reason::Bug { payload: "a RIGHT / FULL merge join with a join qual past its merge clauses (select_mergejoin_clauses plans none)" }),
    }
}

impl Merge<'_> {
    /// MJFillInner: inner tuple `ip` with every outer cell NULL, handed on.
    pub(crate) fn inner_fill<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, c: &Carry, ip: Value, (nx, at): (Value, Value), next: Next<'_, 'e>) -> Carry<'e> {
        let (cells, vals, cols) = {
            let shape = self.outer.borrow();
            let Some((cells, types, cols)) = shape.as_ref() else { crate::refuse!(self.node, Reason::Bug { payload: "a merge's inner fill before its outer row" }) };
            let (cells, vals) = crate::ops::mergejoin::strategies::markable::null_cells(b, env, &cells[..], &types[..]);
            (cells, vals, mcx::to_vec_in(&cols[..], env.mcx))
        };
        let outer = RowVals { row: Row { cells }, vals, cols, carry: c.copy_in(env.mcx), next: nx, at };
        let p = self.run.fetch(b, env, ip);
        let r = self.run.vals(b, env, p, c.copy_in(env.mcx), nx, at);
        let (r2, map) = crate::ops::mergejoin::strategies::markable::join_rows(&outer, (&r.row.cells, &r.vals, &r.cols), c, env.mcx);
        env.memo.clear();
        let saved = env.inner.replace(map);
        let c2 = next(b, env, &r2);
        env.inner = saved;
        c2
    }

    /// EXEC_MJ_ENDOUTER at the input's end, from a state that would fetch the next outer tuple (INITIALIZE_OUTER, NEXTOUTER,
    /// SKIPOUTER_ADVANCE) or ENDOUTER: each inner tuple from the current one that nothing matched, filled (ENDINNER and DONE
    /// have none left). A stop inside a fill leaves the words where they are; a later step's stop hands it on again (D688).
    pub(crate) fn end_outer<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, carry: Carry<'e>, next: Next<'_, 'e>) -> Carry<'e> {
        let Some(n) = self.n.get() else { crate::refuse!(self.node, Reason::Bug { payload: "a merge's flush before its window" }) };
        let types: mcx::PgVec<'_, Type> = carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let [l_walk, l_adv, l_end] = [(); 3].map(|_| b.create_block(&types[..]));
        let [l_go, l_chk, l_fill] = [(); 3].map(|_| b.create_block(&[]));
        let cp = carry.params();
        let st = self.word(b, env, W_ST);
        b.switch(st, [INIT_OUTER, NEXTOUTER, SKIPOUTER_ADVANCE, ENDOUTER].map(|v| (v, l_go, [])), (l_end, &cp));
        b.switch_to(l_go);
        self.st(b, env, ENDOUTER);
        match later_set(b, env) {
            Some(x) => {
                let (off, f) = (env.load_word(b, self.offset), b.iconst(Type::I64, FILLED_INNER));
                let (was, l_x, l_again) = (b.icmp(IntCond::Eq, off, f), b.create_block(&[]), b.create_block(&[]));
                b.cond_br(was, (l_x, &[]), (l_walk, &cp));
                b.switch_to(l_x);
                b.cond_br(x, (l_again, &[]), (l_walk, &cp));
                b.switch_to(l_again);
                let ip = self.word(b, env, W_IP);
                let c2 = self.inner_fill(b, env, &carry, ip, (env.one, env.zero), next);
                b.br(l_adv, &c2.params());
            }
            None => b.br(l_walk, &cp),
        }
        b.switch_to(l_walk);
        b.mark_loop_header(l_walk);
        env.bounded.extend_in([l_walk, l_adv]); // a later set's resume enters the walk at l_adv
        let c = Carry::from_params(b.block_params(l_walk), env.mcx);
        let ip = self.word(b, env, W_IP);
        let ended = b.icmp(IntCond::Sge, ip, n);
        b.cond_br(ended, (l_end, &c.params()), (l_chk, &[]));
        b.switch_to(l_chk);
        let mi = self.word(b, env, W_MI);
        let none = b.icmp(IntCond::Eq, mi, env.zero);
        self.set(b, env, W_MI, 1);
        b.cond_br(none, (l_fill, &[]), (l_adv, &c.params()));
        b.switch_to(l_fill);
        let f = b.iconst(Type::I64, FILLED_INNER);
        env.store_word(b, self.offset, f);
        let c2 = self.inner_fill(b, env, &c, ip, (env.one, env.zero), next);
        b.br(l_adv, &c2.params());
        b.switch_to(l_adv);
        let p = mcx::to_vec_in(b.block_params(l_adv), env.mcx);
        self.fetch_inner(b, env);
        b.br(l_walk, &p[..]);
        b.switch_to(l_end);
        env.store_word(b, self.offset, env.zero);
        Carry::from_params(b.block_params(l_end), env.mcx)
    }

    /// The row's hand-ons and exits: a joined row (`l_emit`, back to the machine), the outer row's fill (`l_fill`, the row
    /// done), an outer tuple done (`l_odone`: filled when it matched nothing), the inner at its end (`l_ieoj`: ENDINNER
    /// fills the outer rows from here on, else the join is DONE), DONE (`l_done`: the pipeline ends at this row) and RIGHT /
    /// FULL's inner fill before a fetch (`l_ifill`, back to the machine).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn tails<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, [l_emit, l_fill, l_odone, l_ieoj, l_done, l_mach, l_out]: [Block; 7], l_ifill: Option<Block>, next: Next<'_, 'e>) {
        b.switch_to(l_emit);
        let c = Carry::from_params(b.block_params(l_emit), env.mcx);
        // D2967: the joined row's by-reference bytes go back once it is through (PG's per-tuple reset per joined row);
        // what a later step or the end keeps it copies, as across windows
        let mark = env.hir.mark(b);
        let j = b.iconst(Type::I64, JOINED);
        env.store_word(b, self.offset, j);
        let ip = self.word(b, env, W_IP);
        let ri = self.inner_at(b, env, row, &c, ip);
        let (r2, map) = crate::ops::mergejoin::strategies::markable::join_rows(row, (&ri.row.cells, &ri.vals, &ri.cols), &c, env.mcx);
        env.memo.clear();
        // D4664: the inner row stays in reach of a write's EvalPlanQual redo in the IR (epq_ir.rs `Redo::Nest`), as nest.loop's
        env.redo_nests.push_in((self.node, (mcx::to_vec_in(&ri.row.cells[..], env.mcx), mcx::to_vec_in(&ri.vals[..], env.mcx), mcx::to_vec_in(&ri.cols[..], env.mcx), crate::frame::env::Filled::No)));
        // every type hands on the joined row, SEMI too: PG's JOINTUPLES ExecProject reads the matched inner tuple (a nested
        // IN's semi join plans RHS columns in its target list; the resume position stays the outer row's)
        let saved = env.inner.replace(map);
        let c2 = next(b, env, &r2);
        env.inner = saved;
        env.redo_nests.pop();
        env.memo.clear();
        if let Some(m) = mark {
            env.hir.give_back(b, m);
        }
        b.br(l_mach, &c2.params());
        b.switch_to(l_fill);
        let c = Carry::from_params(b.block_params(l_fill), env.mcx);
        match self.jt {
            JoinType::JOIN_LEFT | JoinType::JOIN_ANTI | JoinType::JOIN_FULL => {
                let f = b.iconst(Type::I64, FILLED);
                env.store_word(b, self.offset, f);
                let c2 = crate::ops::mergejoin::strategies::markable::fill_row(b, env, row, &c, next, self.jt == JoinType::JOIN_ANTI, (self.node, self.cells.borrow().as_ref()));
                b.br(l_out, &c2.params());
            }
            _ => b.br(l_out, &c.params()),
        }
        b.switch_to(l_odone);
        let cp = mcx::to_vec_in(b.block_params(l_odone), env.mcx);
        match self.fill_outer() {
            true => {
                let mo = self.word(b, env, W_MO);
                let none = b.icmp(IntCond::Eq, mo, env.zero);
                b.cond_br(none, (l_fill, &cp[..]), (l_out, &cp[..]));
            }
            false => b.br(l_out, &cp[..]),
        }
        b.switch_to(l_ieoj);
        let cp = mcx::to_vec_in(b.block_params(l_ieoj), env.mcx);
        match self.fill_outer() {
            true => {
                self.st(b, env, ENDINNER);
                b.br(l_odone, &cp[..]);
            }
            false => {
                self.st(b, env, DONE);
                b.br(l_done, &cp[..]);
            }
        }
        b.switch_to(l_done);
        let c = Carry::from_params(b.block_params(l_done), env.mcx);
        env.full(b, &c, row.next);
        if let Some(l) = l_ifill {
            b.switch_to(l);
            let c = Carry::from_params(b.block_params(l), env.mcx);
            let f = b.iconst(Type::I64, FILLED_INNER);
            env.store_word(b, self.offset, f);
            let ip = self.word(b, env, W_IP);
            let c2 = self.inner_fill(b, env, &c, ip, (row.at, row.at), next);
            b.br(l_mach, &c2.params());
        }
    }
}

#[cfg(test)]
mod tests {
    use mcx::MemoryContext;
    use types_nodes::list::NodeList;
    use types_nodes::primnodes::OpExpr;
    use types_nodes::Node;
    use datum::Datum;

    use crate::admit::refusal::Reason;

    /// A RIGHT / FULL merge join's join qual past its merge clauses: a constant is PG's FULL JOIN ON false, anything else a
    /// plan the planner never makes (Reason::Bug); INNER / LEFT / SEMI / ANTI keep theirs.
    #[test]
    fn a_right_or_full_join_qual_that_is_not_a_constant_is_a_bug() {
        let cx = MemoryContext::new("fill_inner");
        let m = cx.mcx();
        let k = |v| Node::mk_const(m, 20, -1, 0, 8, Datum::from_i64(v), false, true).expect("a const node");
        let op = Node::mk(m, OpExpr { opno: 410, opfuncid: 467, opresulttype: 16, opretset: false, opcollid: 0, inputcollid: 0, args: NodeList::from_slice(m, &[k(1), k(2)]).unwrap(), location: -1 }).expect("an int8eq");
        assert!(super::qual_ok(true, [k(0)].into_iter()).is_ok(), "FULL JOIN ON false");
        assert!(super::qual_ok(true, core::iter::empty()).is_ok());
        assert!(matches!(super::qual_ok(true, [op].into_iter()), Err(Reason::Bug { .. })));
        assert!(super::qual_ok(false, [op].into_iter()).is_ok(), "a LEFT join's qual is its own");
    }

    /// D1349: nodeMergejoin's mj_FillInner is RIGHT, RIGHT ANTI and FULL (ExecInitMergeJoin); the rest fill no inner, so
    /// a RIGHT ANTI join takes the RIGHT join's qual rule and fill.
    #[test]
    fn right_anti_fills_the_inner_as_right_and_full_do() {
        use types_nodes::jointype::JoinType as J;
        for (jt, fills) in [(J::JOIN_RIGHT, true), (J::JOIN_RIGHT_ANTI, true), (J::JOIN_FULL, true), (J::JOIN_INNER, false), (J::JOIN_LEFT, false), (J::JOIN_SEMI, false), (J::JOIN_ANTI, false)] {
            assert_eq!(super::fills_inner(jt), fills, "{jt:?}");
        }
    }
}

//! merge.join's state machine (nodeMergejoin.c's ExecMergeJoin states that run inside a row) and the inner-side helpers
//! it calls: the inner tuple test, the fetch, RIGHT / FULL's fill before a fetch, and PG's "mergejoin input data is out of
//! order" raise. Moved verbatim from merge/mergejoin.rs (MERGE R3 (ii)) so the kind's file keeps its budget with the lazy
//! inner's hooks. MERGE R3 (ii) (V4R-494): INITIALIZE_INNER waits on a lazy inner (D824's `wait`). Owner M11-MERGE.
//! Budget 190 lines.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::mergejoin::entries as h11;
use mcx::GrowIn;
use types_nodes::jointype::JoinType;

use crate::ops::mergejoin::lower::*;
use crate::frame::env::Env;
use crate::frame::row::{Carry, RowVals};

impl Merge<'_> {
    /// PG's "mergejoin input data is out of order" (elog ERROR, XX000), raised at the row.
    fn out_of_order(&self, b: &mut FunctionBuilder<'_>, env: &Env) {
        let h = b.declare_host_once("m11_raise", &[Type::Ptr, Type::I64], Some(Type::I64), h11::m11_raise as *const () as usize, gravity_ir::HostFlags { may_error: true, ..Default::default() });
        let code = b.iconst(Type::I64, h11::RAISE_OUT_OF_ORDER as i64);
        let st = b.call_host(h, &[env.ctx, code], Some(Type::I64)).expect("m11_raise returns its status");
        b.br(env.x.l_exit, &[st]);
    }

    /// The inner tuple `ip` tested as the machine's current one (INITIALIZE_INNER / SKIPINNER_ADVANCE after their fetch,
    /// TESTOUTER past the mark): past the run's end or ENDOFJOIN `l_ieoj`, MATCHABLE state `on_m`, NONMATCHABLE `on_nm`.
    #[allow(clippy::too_many_arguments)]
    fn test_inner(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, c: &Carry, (ip, n): (Value, Value), (on_m, on_nm): (u64, u64), (l_mach, l_ieoj): (Block, Block)) {
        let cp = c.params();
        let (ended, l_go) = (self.ended(b, env, ip, n), b.create_block(&[]));
        b.cond_br(ended, (l_ieoj, &cp), (l_go, &[]));
        b.switch_to(l_go);
        let (r2, map) = self.joined(b, env, row, c, ip);
        let r = self.eval(b, env, &r2, Some(&map), false);
        let [l_m, l_nm] = [(); 2].map(|_| b.create_block(&[]));
        let l_e = b.create_block(&[]);
        three(b, r, (l_m, l_nm, l_e), false, &[]);
        b.switch_to(l_m);
        self.to(b, env, on_m, l_mach, &cp);
        b.switch_to(l_nm);
        self.to(b, env, on_nm, l_mach, &cp);
        b.switch_to(l_e);
        b.br(l_ieoj, &cp);
    }

    /// The inner index after the current one, stored as current.
    pub(crate) fn fetch_inner(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> Value {
        let ip = self.word(b, env, W_IP);
        let ip = b.int_bin(IntBinOp::Add, ip, env.one);
        env.store_word(b, self.w[W_IP as usize], ip);
        if self.fill_inner() {
            self.set(b, env, W_MI, 0);
        }
        ip
    }

    /// RIGHT / FULL: before a fetch, the current inner tuple filled when nothing matched it (`l_ifill`, back to the machine).
    fn fill_first(&self, b: &mut FunctionBuilder<'_>, env: &Env, c: &Carry, l_ifill: Option<Block>) {
        let Some(l) = l_ifill else { return };
        let mi = self.word(b, env, W_MI);
        let (none, l_go) = (b.icmp(IntCond::Eq, mi, env.zero), b.create_block(&[]));
        self.set(b, env, W_MI, 1);
        b.cond_br(none, (l, &c.params()), (l_go, &[]));
        b.switch_to(l_go);
    }

    /// The states that run inside a row (`l_mach` dispatches on `W_ST`); an outer-fetching state ends the row (`l_odone`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn machine(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, n: Value, [l_mach, l_emit, l_odone, l_ieoj]: [Block; 4], l_ifill: Option<Block>, ctypes: &[Type]) {
        b.switch_to(l_mach);
        b.mark_loop_header(l_mach);
        env.bounded.push_in(l_mach);
        let cp = mcx::to_vec_in(b.block_params(l_mach), env.mcx);
        let st = self.word(b, env, W_ST);
        let own = [INIT_INNER, SKIP_TEST, SKIPINNER_ADVANCE, JOINTUPLES, NEXTINNER, TESTOUTER];
        let ls = own.map(|_| b.create_block(ctypes));
        let cases = own.into_iter().zip(ls).chain([NEXTOUTER, SKIPOUTER_ADVANCE, ENDINNER].map(|v| (v, l_odone)));
        b.switch(st, cases.map(|(v, t)| (v, t, &cp[..])), (l_odone, &cp));
        let [l_init, l_skip, l_skipin, l_join, l_next, l_test] = ls;
        let mcx = env.mcx;
        let carry = |b: &mut FunctionBuilder<'_>, l: Block| {
            b.switch_to(l);
            Carry::from_params(b.block_params(l), mcx)
        };
        // INITIALIZE_INNER / SKIPINNER_ADVANCE: fetch the next inner tuple and test it
        for (l, on_nm) in [(l_init, INIT_INNER), (l_skipin, SKIPINNER_ADVANCE)] {
            let c = carry(b, l);
            if on_nm == INIT_INNER {
                self.run.first_fetch(b, env); // W8-EQUIVCLASS-SORTOP: PG's first inner call, before a lazy inner runs
            }
            if let Some((consumed, lz)) = self.lazy.filter(|_| on_nm == INIT_INNER) {
                // MERGE R3 (ii): the lazy inner still pending, the row stops here and resumes built (D824's wait)
                let r = RowVals { row: row.row.clone(), vals: row.vals.clone(), cols: row.cols.clone(), carry: c.copy_in(env.mcx), next: row.next, at: row.at };
                lz.wait(b, env, &r, self.offset, consumed);
            }
            self.fill_first(b, env, &c, l_ifill);
            let ip = self.fetch_inner(b, env);
            self.test_inner(b, env, row, &c, (ip, n), (SKIP_TEST, on_nm), (l_mach, l_ieoj));
        }
        // SKIP_TEST: the outer against the current inner
        let c = carry(b, l_skip);
        let ip = self.word(b, env, W_IP);
        let (r2, map) = self.joined(b, env, row, &c, ip);
        let v = self.compare(b, env, &r2, &map);
        let [l_eq, l_lt, l_gt] = [(); 3].map(|_| b.create_block(&[]));
        three(b, v, (l_eq, l_lt, l_gt), true, &[]);
        b.switch_to(l_eq);
        env.store_word(b, self.w[W_MARK as usize], ip);
        self.run.mark(b, env, ip); // D3105 (b): a window over put runs keeps the mark's rows
        self.to(b, env, JOINTUPLES, l_mach, &c.params());
        b.switch_to(l_lt);
        self.st(b, env, SKIPOUTER_ADVANCE);
        b.br(l_odone, &c.params());
        b.switch_to(l_gt);
        self.to(b, env, SKIPINNER_ADVANCE, l_mach, &c.params());
        // JOINTUPLES: the join qual in PG's order; a match is handed on (ANTI's drops the outer row, RIGHT ANTI's is never)
        let c = carry(b, l_join);
        // D2967: the pair's by-reference bytes (the join qual's) go back once the qual is decided, kept or dropped (PG's
        // per-tuple reset per inner tuple): the hand-on (`tails`' l_emit) makes the joined row again, the memo cleared
        let mark = env.hir.mark(b);
        self.st(b, env, NEXTINNER);
        let ip = self.word(b, env, W_IP);
        let (r2, map) = self.joined(b, env, row, &c, ip);
        let saved = env.inner.replace(map);
        let l_drop = (mark.is_some() && !self.qual.is_empty()).then(|| b.create_block(&[]));
        for &q in self.qual.iter() {
            env.memo.clear();
            let pass = self.ex.0.pred(b, env, self.node, &r2, q);
            let l_yes = b.create_block(&[]);
            match l_drop {
                Some(l) => b.cond_br(pass, (l_yes, &[]), (l, &[])),
                None => b.cond_br(pass, (l_yes, &[]), (l_mach, &c.params())),
            }
            b.switch_to(l_yes);
        }
        env.inner = saved;
        env.memo.clear();
        if let Some(m) = mark {
            env.hir.give_back(b, m);
            if let Some(l) = l_drop {
                let l_on = b.current();
                b.switch_to(l);
                env.hir.give_back(b, m);
                b.br(l_mach, &c.params());
                b.switch_to(l_on);
            }
        }
        self.set(b, env, W_MO, 1);
        if self.fill_inner() {
            self.set(b, env, W_MI, 1);
        }
        match (self.jt, self.single) {
            (JoinType::JOIN_ANTI, _) | (JoinType::JOIN_RIGHT_ANTI, true) => self.to(b, env, NEXTOUTER, l_mach, &c.params()),
            (JoinType::JOIN_RIGHT_ANTI, false) => b.br(l_mach, &c.params()), // stays on the outer, the state NEXTINNER
            (_, true) => {
                self.st(b, env, NEXTOUTER);
                b.br(l_emit, &c.params());
            }
            _ => b.br(l_emit, &c.params()),
        }
        // NEXTINNER: the next inner tuple against the outer (above it is out of order)
        let c = carry(b, l_next);
        self.fill_first(b, env, &c, l_ifill);
        let ip = self.fetch_inner(b, env);
        let (ended, l_go, l_to_next) = (self.ended(b, env, ip, n), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(ended, (l_to_next, &[]), (l_go, &[]));
        b.switch_to(l_go);
        let (r2, map) = self.joined(b, env, row, &c, ip);
        let r = self.eval(b, env, &r2, Some(&map), false);
        let (m, l_cmp) = (b.icmp(IntCond::Eq, r, env.zero), b.create_block(&[]));
        b.cond_br(m, (l_cmp, &[]), (l_to_next, &[]));
        b.switch_to(l_cmp);
        let v = self.compare(b, env, &r2, &map);
        let [l_eq, l_lt, l_gt] = [(); 3].map(|_| b.create_block(&[]));
        three(b, v, (l_eq, l_lt, l_gt), true, &[]);
        b.switch_to(l_eq);
        self.to(b, env, JOINTUPLES, l_mach, &c.params());
        b.switch_to(l_lt);
        b.br(l_to_next, &[]);
        b.switch_to(l_gt);
        self.out_of_order(b, env);
        b.switch_to(l_to_next);
        self.to(b, env, NEXTOUTER, l_mach, &c.params());
        // TESTOUTER: the new outer against the marked inner (below it is out of order)
        let c = carry(b, l_test);
        let mk = self.word(b, env, W_MARK);
        let (r2, map) = self.joined(b, env, row, &c, mk);
        let v = self.compare(b, env, &r2, &map);
        let [l_eq, l_lt, l_gt] = [(); 3].map(|_| b.create_block(&[]));
        three(b, v, (l_eq, l_lt, l_gt), true, &[]);
        b.switch_to(l_eq);
        env.store_word(b, self.w[W_IP as usize], mk);
        self.to(b, env, JOINTUPLES, l_mach, &c.params());
        b.switch_to(l_lt);
        self.out_of_order(b, env);
        b.switch_to(l_gt);
        let ip = self.word(b, env, W_IP);
        self.test_inner(b, env, row, &c, (ip, n), (SKIP_TEST, SKIPINNER_ADVANCE), (l_mach, l_ieoj));
    }
}

//! `nest.subplan`, hashed over several key columns (B2a on RT-2d; a helper of nest/subplan_hash.rs, split from it for its
//! budget; registers nothing; owner M8-SUB). The probe's row (its key words, then a null word per column: 1 NULL, 2 a
//! cross-type value outside the key's type) goes to `m8_subhash_probe`, which answers as `ExecHashSubPlan` with its partial
//! matches. An all-NULL probe first asks whether the set is empty, so an empty set's left side is never evaluated, as in PG.
//! A LAZY set runs its build through `m8_subhash_run` first (D1035: that host pins the pipeline to the leader).
//! D2849: a one-column array key whose hash raises at a non-NULL element (`KeyCol::Canon { elems }`) raises at the probe
//! where ExecHashSubPlan hashes the left side: it is not NULL, the set holds a row with no NULL key (`m8_subhash_exact`,
//! PG's `havehashrows`) and the left array holds a non-NULL element (`m6_raise_elems`).
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::shared::object::entries as m8;
use gravity_lower::ops::subplan::keyset::width_mask;
use mcx::{CollectIn, PgVec};

use crate::ops::subplan::hashed::{coll_fp, float_key, fp, host, object, raise_if, KeyCol, Lhs};
use crate::face::step::{ExprFace, StepMech};
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::row::{Carry, RowVals};
use crate::rerun::params::{NULL, SET};

/// RT-2d's probe: the row goes to `m8_subhash_probe` (a LAZY set built there first).
pub(crate) struct Multi<'g> {
    pub(crate) node: i32,
    pub(crate) plan_id: i32,
    pub(crate) h: u16,
    /// Each column's left side and how its key is held (a word: the left side's width and the key's, a cross-type
    /// integer equality when they differ; or its canonical form's two Fp128 words).
    pub(crate) cols: PgVec<'g, (Lhs, KeyCol)>,
    pub(crate) ex: &'static ExprFace,
    pub(crate) unknown_eq_false: bool,
    pub(crate) lend: crate::ops::subplan::site::Lend,
    pub(crate) early: crate::ops::subplan::site::Early,
    /// A LAZY set runs its build through `m8_subhash_run` (D1035: that host pins the pipeline to the leader).
    pub(crate) lazy_set: bool,
    pub(crate) val: i32,
    pub(crate) flags: i32,
    /// W6-D1112: a check option's, host-fed (`Lhs::Fed`): evaluated at every read of the host's.
    pub(crate) fed: bool,
}

impl Multi<'_> {
    /// D2849: PG hashes a left side with no NULL column only against a set holding a row with no NULL key, and hash_array
    /// raises 42P22 at its first non-NULL element: the left array `v` (when `valid`) raises there.
    fn raise_elems_at_probe(&self, b: &mut FunctionBuilder<'_>, env: &Env, v: Value, valid: Value) {
        let (l_set, l_hash, l_go) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(valid, (l_set, &[]), (l_go, &[]));
        b.switch_to(l_set);
        let o = object(b, env, self.h);
        let f = host(b, "m8_subhash_exact", &[Type::Ptr, Type::Ptr], gravity_abi::host_words!(m8::m8_subhash_exact(ctx, h)));
        let r = b.call_host(f, &[env.ctx, o], Some(Type::I64)).unwrap_or(env.zero);
        let bad = b.icmp(IntCond::Slt, r, env.zero);
        raise_if(b, env, bad);
        let exact = b.icmp(IntCond::Sgt, r, env.zero);
        b.cond_br(exact, (l_hash, &[]), (l_go, &[]));
        b.switch_to(l_hash);
        let st = crate::ops::shared::hash_raise::elems(b, env, v);
        let bad = b.icmp(IntCond::Slt, st, env.zero);
        raise_if(b, env, bad);
        b.br(l_go, &[]);
        b.switch_to(l_go);
    }

    fn probe(&self, b: &mut FunctionBuilder<'_>, env: &Env, at: Value) -> Value {
        let o = object(b, env, self.h);
        let f = host(b, "m8_subhash_probe", &[Type::Ptr, Type::Ptr, Type::Ptr], gravity_abi::host_words!(m8::m8_subhash_probe(ctx, h, row)));
        let r = b.call_host(f, &[env.ctx, o, at], Some(Type::I64)).unwrap_or(env.zero);
        let bad = b.icmp(IntCond::Slt, r, env.zero);
        raise_if(b, env, bad);
        r
    }
}

impl crate::ops::subplan::per_row::Eval for Multi<'_> {
    fn eval(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) {
        let n = self.cols.iter().map(|c| c.1.widths().len()).sum::<usize>() as i32;
        let slot = b.declare_stack_slot(16 * n as u32, 8);
        let at = b.stack_addr(slot);
        if self.lazy_set {
            let f = host(b, "m8_subhash_run", &[Type::Ptr, Type::Ptr], gravity_abi::host_words!(m8::m8_subhash_run(ctx, h)));
            let o = object(b, env, self.h);
            let r = b.call_host(f, &[env.ctx, o], Some(Type::I64)).unwrap_or(env.zero);
            let bad = b.icmp(IntCond::Slt, r, env.zero);
            raise_if(b, env, bad);
        }
        // an all-NULL probe answers FALSE only for an empty set (PG decides that before it projects the left side)
        for i in 0..n {
            b.store(at, 8 * i, env.zero);
            b.store(at, 8 * (n + i), env.one);
        }
        let r = self.probe(b, env, at);
        let (l_put, l_lhs) = (b.create_block(&[Type::I64]), b.create_block(&[]));
        let empty = b.icmp(IntCond::Eq, r, env.zero);
        b.cond_br(empty, (l_put, &[env.zero]), (l_lhs, &[]));
        b.switch_to(l_lhs);
        // W6-D1112: a fed left side the host's evaluation raised: 3 (NULL whatever unknownEqFalse says; the host raises)
        let mcx = env.mcx;
        let raised: PgVec<'_, Value> = self.cols.iter().filter_map(|c| c.0.raised(b, env)).collect_in(mcx);
        let t = b.bconst(true);
        if let Some(any) = raised.into_iter().reduce(|x, y| b.select(x, t, y)) {
            let (three, l_go) = (b.iconst(Type::I64, 3), b.create_block(&[]));
            b.cond_br(any, (l_put, &[three]), (l_go, &[]));
            b.switch_to(l_go);
        }
        let mut i = 0;
        for &(lhs, col) in &self.cols {
            let (v, valid) = lhs.emit(self.ex, b, env, self.node, row);
            if let KeyCol::Canon { elems: true, .. } = col {
                self.raise_elems_at_probe(b, env, v, valid);
            }
            let (v, lw, w) = match col {
                KeyCol::Word { lw, w } => (v, lw, w),
                // D3676: the float's word image, as the set's build lays it
                KeyCol::Float { l32, .. } => (float_key(b, v, l32), 8, 8),
                KeyCol::Canon { .. } | KeyCol::Coll { .. } | KeyCol::User { .. } | KeyCol::Cross { .. } => {
                    let (lo, hi) = match col {
                        KeyCol::Canon { form, .. } => fp(b, env, v, valid, form),
                        KeyCol::Coll { coll, tf } => coll_fp(b, env, self.node, (v, valid), coll, &tf),
                        KeyCol::User { spec, .. } => crate::ops::subplan::hashed::user_fp(b, env, v, valid, spec),
                        // the probe's left side: its own form
                        KeyCol::Cross { lform, .. } => fp(b, env, v, valid, lform),
                        KeyCol::Word { .. } | KeyCol::Float { .. } => unreachable!("a word key"),
                    };
                    let nw = b.select(valid, env.zero, env.one);
                    for (x, word) in [lo, hi].into_iter().enumerate() {
                        b.store(at, 8 * (i + x as i32), word);
                        b.store(at, 8 * (n + i + x as i32), nw);
                    }
                    i += 2;
                    continue;
                }
            };
            let (lsh, ksh) = (b.iconst(Type::I64, 64 - i64::from(lw) * 8), b.iconst(Type::I64, 64 - i64::from(w) * 8));
            let x = b.int_bin(IntBinOp::Shl, v, lsh);
            let x = b.int_bin(IntBinOp::Sshr, x, lsh);
            // a wider left value outside the key's type equals no key: null word 2
            let y = b.int_bin(IntBinOp::Shl, x, ksh);
            let y = b.int_bin(IntBinOp::Sshr, y, ksh);
            let fits = b.icmp(IntCond::Eq, x, y);
            let (two, one) = (b.iconst(Type::I64, 2), env.one);
            let nw = b.select(fits, env.zero, two);
            let nw = b.select(valid, nw, one);
            let m = b.iconst(Type::I64, width_mask(w) as i64);
            let key = b.int_bin(IntBinOp::And, x, m);
            b.store(at, 8 * i, key);
            b.store(at, 8 * (n + i), nw);
            i += 1;
        }
        let r = self.probe(b, env, at);
        b.br(l_put, &[r]);
        b.switch_to(l_put);
        // 1 TRUE, 0 FALSE, 2 NULL (FALSE under unknownEqFalse), 3 a fed left side raised (NULL)
        let r = b.block_params(l_put)[0];
        let v = b.icmp(IntCond::Eq, r, env.one);
        let v = b.conv(ConvOp::Zext, Type::I64, v);
        env.store_word(b, self.val, v);
        let null = if self.unknown_eq_false { b.bconst(false) } else { let two = b.iconst(Type::I64, 2); b.icmp(IntCond::Eq, r, two) };
        let null = if self.fed { let (three, t) = (b.iconst(Type::I64, 3), b.bconst(true)); let raised = b.icmp(IntCond::Eq, r, three); b.select(raised, t, null) } else { null };
        let (is_null, not_null) = (b.iconst(Type::I64, SET | NULL), b.iconst(Type::I64, SET));
        let fw = b.select(null, is_null, not_null);
        env.store_word(b, self.flags, fw);
    }

    fn fed(&self) -> bool {
        self.fed
    }
}

impl StepMech for Multi<'_> {
    fn prologue(&self, _: &mut FunctionBuilder<'_>, env: &mut Env, _: &crate::frame::stop::Exits) {
        crate::ops::subplan::site::prologue(self, (self.lend, &self.early), (self.plan_id, self.val, self.flags), env);
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        crate::ops::subplan::site::lend(self, self.lend, (self.plan_id, self.val, self.flags), b, env, row, next)
    }
}

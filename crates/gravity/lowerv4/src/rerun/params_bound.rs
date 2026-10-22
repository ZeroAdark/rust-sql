//! D1016 row 5 (V4R-564): a LIMIT / OFFSET bound computed from executor parameters (a nest's, a SubPlan's or an InitPlan's
//! parameter, bare or under a cast or arithmetic), evaluated in the IR by expr.core over no row once per open, as
//! PostgreSQL's recompute_limits does at LIMIT_INITIAL and at ExecReScanLimit (window/rows.rs's frame offsets and
//! nest/inner.rs's gates are the same evaluation over no row). The Limit step (step/limit.rs) keeps it in two per-open
//! words `[value, flags]` (params.rs `SET` / `NULL`), written the first time its open reads it (its first window, before
//! any row: a nest inner's prologue runs once per call of the owner, before the owner writes the parameters); the top-k
//! end (runs/topk_rows.rs) evaluates it at its producer's open for its cut; a MergeAppend's arm cap skips it (props/merge.rs
//! `merge_cap`). A statement it newly serves refuses row B's forms (params_key.rs `beside`) by name; main 02:53Z's no-widen
//! names (D2178 / D2179, D2194 / D2147 / D2201, D2213) left with their root fixes (D2194's fence D2325 is fixed, CAND-M39
//! closed not reproduced). Owner M8-NEST.
use gravity_decompose::graph::Bound;
use gravity_decompose::ops::shared::sort::SortForm;
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use types_nodes::Node;

use crate::face::step::ExprFace;
use crate::frame::env::Env;
use crate::face::kinds::Face;
use crate::frame::px::{runs_word, ExId, Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::graph::Gx;
use crate::rerun::params::{NULL, SET};
use crate::admit::refusal::{Reason, ShapeWhy};

/// A computed bound: expr.core's expression, its plan node, and the per-open words `[value, flags]` a step keeps it in
/// (`None`: evaluated where it is read, which the reader does once per open: the top-k end's open).
#[derive(Clone, Copy)]
pub struct Computed {
    ex: ExId,
    node: i32,
    words: Option<[i32; 2]>,
}

/// Whether bound `b` is computed from executor parameters (no bind-time word holds it: words.rs `key_word`).
pub fn computed(b: &Bound<'_>) -> bool {
    matches!(*b, Bound::Expr(e) if gravity_lower::rerun::words::has_exec(e))
}

/// D1634: top pipeline `p`'s words `[value, flags]` for its bound `which` (0 count, 1 offset), reset at `p`'s open (its end
/// declares them) and written by the first read: one evaluation per open of the top.
pub fn top_words(px: &mut Px<'_, '_>, p: usize, which: u8) -> [i32; 2] {
    let mut w = |i: u8| px.frame_word(WordKey::Open { face: Face::Runs, k: p, j: u64::MAX, i }) as i32 * 8;
    let i = runs_word::TOP_BOUND + 2 * which;
    [w(i), w(i + 1)]
}

/// D1634: the top pipeline whose bound `which` (0 count, 1 offset) is bound `b` of the Limit in pipeline `k`, when `k` reads
/// that top's breaker and the top computes the bound (lifetime/bound.rs `rebinds` / `computed`). PostgreSQL's Limit evaluates its bound once
/// per rescan and hands it to the Sort below (ExecSetTupleBound), so the Limit reads the top's words (`top_words`) instead
/// of evaluating it a second time.
pub fn top_below(gx: &Gx<'_, '_>, k: usize, b: &Bound<'_>, which: u8) -> Option<usize> {
    let Bound::Expr(e) = *b else { return None };
    let Source::Breaker { slot } = gx.pipelines()[k].source else { return None };
    let p = gx.life_of(k, slot)?.0 as usize;
    let top = gx.pipelines()[p].ops.iter().find_map(|op| match op {
        Op::Sort(sd) => match &sd.form {
            SortForm::TopK { count, offset, .. } | SortForm::Walk { count, offset, .. } => Some(if which == 0 { count } else { offset }),
            SortForm::Runs => None,
        },
        _ => None,
    })?;
    let same = matches!(*top, Bound::Expr(t) if t.ptr_eq(e));
    // D3538: a top whose words a reader's frame sees once (lifetime/bound.rs `open_words`: a sectioned member) is not read:
    // the Limit then reads its own words, reset at its own open, so the bound is evaluated twice per run, over no row and
    // no call that shows (`admit`'s forms)
    let at_each_run = crate::lifetime::bound::open_words(gx, p, k) == crate::lifetime::bound::BoundAt::Rescan;
    (same && at_each_run && (crate::lifetime::bound::rebinds(gx, p, top) || computed(top))).then_some(p)
}

/// ops/limit/lower.rs `make`: the first WITH TIES key word's index when the Limit's four computed-bound words take 3..=6 (a
/// key's three words each; CAP-30: keys past the step's `u8` indices take further chunks, so no key count refuses).
pub const TIE_BASE: u8 = 7;

/// Bound `e` (it reads an executor parameter, or `rebinds`) of plan node `node`, compiled by expr.core: a SubPlan inside it
/// refuses by name (no step evaluates it before its reader); then, over executor parameters, the statement's row B names (a
/// rebinding bound is in statements served today: they do not apply).
pub fn admit<'p>(px: &mut Px<'_, 'p>, e: Node<'p>, node: i32, words: Option<[i32; 2]>) -> Computed {
    if gravity_decompose::expr::has_subplan(e) {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy("a LIMIT / OFFSET computed by a SubPlan over executor parameters or in a rerun set (D1016 row 5; D1634)") })
    }
    if gravity_lower::rerun::words::has_exec(e) {
        crate::rerun::params_key::beside(px.gx, node);
    }
    let ex: &'static ExprFace = crate::frame::iface::expr_face(&node);
    Computed { ex: ex.0.compile(px, e, None, node), node, words }
}

impl Computed {
    /// The step's per-open words (step/limit.rs `open_words` declares them with its counters).
    pub fn words(&self) -> Option<[i32; 2]> {
        self.words
    }

    /// Its value (`I64`) and NULL flag (`Bool`): evaluated here (no words), or once per open into the words and read back.
    pub fn read(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> (Value, Option<Value>) {
        let Some([wv, wf]) = self.words else {
            let (v, null) = self.eval(b, env);
            return (v, Some(null));
        };
        let (flags, set) = (env.load_word(b, wf), b.iconst(Type::I64, SET));
        let seen = b.int_bin(IntBinOp::And, flags, set);
        let (fresh, l_eval, l_read) = (b.icmp(IntCond::Eq, seen, env.zero), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(fresh, (l_eval, &[]), (l_read, &[]));
        b.switch_to(l_eval);
        let memo = env.memo.set_aside(); // the evaluation's reads do not dominate `l_read`
        let (v, null) = self.eval(b, env);
        env.memo.restore(memo);
        env.store_word(b, wv, v);
        let (is_null, not_null) = (b.iconst(Type::I64, SET | NULL), b.iconst(Type::I64, SET));
        let f = b.select(null, is_null, not_null);
        env.store_word(b, wf, f);
        b.br(l_read, &[]);
        b.switch_to(l_read);
        let (v, f, n) = (env.load_word(b, wv), env.load_word(b, wf), b.iconst(Type::I64, NULL));
        let null = b.int_bin(IntBinOp::And, f, n);
        (v, Some(b.icmp(IntCond::Ne, null, env.zero)))
    }

    /// expr.core's value over no row (an executor parameter from its words, an InitPlan's run at first use), raising there.
    fn eval(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> (Value, Value) {
        let none = RowVals { row: crate::cell::Row::new_in(env.mcx), vals: mcx::PgVec::new_in(env.mcx), cols: mcx::PgVec::new_in(env.mcx), carry: Carry::new(env.zero, env.mcx), next: env.zero, at: env.zero };
        let ex: &'static ExprFace = crate::frame::iface::expr_face(&self.node);
        let (v, valid) = ex.0.emit(b, env, self.node, &none, self.ex);
        let t = b.bconst(true);
        let null = b.int_bin(IntBinOp::Xor, valid, t);
        (b.select(null, env.zero, v), null)
    }
}


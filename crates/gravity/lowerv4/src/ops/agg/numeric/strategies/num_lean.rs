//! The lean numeric adds of packed windows (D619, V4R-223). Hooks / levers: -. Budget 192 lines (files.tsv, +25 %
//! gate). Owner V2REG. Registers nothing (road num.lean, whose form `word` is, registers in class_num.rs).
//!
//! pgrc packs a `numeric(p, s)` chunk only when every value fits finite at the elected scale (`fixed_scale_fit`,
//! |m| < 2^55; a special demotes the chunk to verbatim), so a packed window's word needs none of the specials algebra
//! a verbatim image's does. A grouped (Slot) step of a bare class.num_packed sum / avg (`bare`) or a class.num_wide
//! product (`wide`) branches on its lanes' kind: packed, it adds the exact term and one range check on the new top word
//! (D299's [-2^62, 2^62), `capped` / `add256`'s test) sends a sticky sentinel state (top word i64::MIN) and a sum leaving
//! the range to the cold generic fold, which recomputes the row from the untouched slot; verbatim, the caller's generic
//! step runs (between the lean branch and `join`, its datum memo set aside). Either way the slot ends as the generic
//! step leaves it: the same words, `n`, validity mark and OVERFLOW / sentinel codes.
//! Kept as is: the premise is the chunk's own packed fact, read per window at run time, with the generic step as its
//! fallback. Does not apply (e2e v4/m3_v4r223_lean.sql): a 30-digit dscale column, a negative scale and a 36-digit
//! grouped product stay class.num_varlena.
use gravity_decompose::ops::shared::agg::{AggDecl, Register as R, NONE};
use gravity_fn::agg_final::SUM_HI_LIMIT;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};
use mcx::{CollectIn, GrowIn, PgVec};

use crate::face::fold::{Acc, FoldClass};
use crate::frame::env::Env;
use crate::frame::row::RowVals;
use crate::rep::CellRef;

use crate::ops::agg::numeric::strategies::num_tree::{digits_of, scale_of, Tree};
use crate::ops::agg::numeric::strategies::num_window::Val;

/// An open lean branch: the join, and the row's datum memo the generic step's block runs without.
pub(crate) struct Lean<'e> {
    l_join: Block,
    memo: crate::lx::SetAside<'e>,
}

/// The lean branch of a bare class.num_packed sum / avg over `c` into `acc`; `None` (nothing emitted) where it does not
/// apply: min / max, a dscale companion (D256), join.mult, a spilled sum (D830), a register state, a cell not a packed
/// `Window` lane at the scale.
pub(crate) fn bare<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, a: &AggDecl<'_>, acc: &Acc<'_>, c: CellRef<'_>) -> Option<Lean<'e>> {
    let avg = match a.reg {
        R::SumNum => false,
        R::AvgNum => true,
        _ => return None,
    };
    let &Acc::Slot { base, off, valid } = acc else { return None };
    if a.dscale_slot != NONE || env.mult.is_some() || crate::ops::agg::numeric::num_carry::spilled(a) || crate::rep::num_of(c.cell.rep).scale != a.scale {
        return None;
    }
    let (verbatim, w) = window(b, env, c)?;
    let (l_lean, l_join) = (b.create_block(&[]), b.create_block(&[]));
    let l_gen = b.create_block(&[]);
    b.cond_br(verbatim, (l_gen, &[]), (l_lean, &[]));
    b.switch_to(l_lean);
    if let Some(null) = c.null {
        let l_row = b.create_block(&[]);
        b.cond_br(null, (l_join, &[]), (l_row, &[]));
        b.switch_to(l_row);
    }
    // D3224 A2: a code front's record holds this window's rows alone: its sum adds with no range test (a packed word is
    // under 2^55, WINDOW_ROWS of them under 2^68), as one i64 where the typmod bounds the window's sum under 2^63
    let rw = env.rec_window.filter(|_| env.rec_zeroed); // hashagg row_in's own record alone
    if let Some(f) = rw.filter(|_| bounded(b, env, c)) {
        let (l_win, l_any) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(f, (l_win, &[]), (l_any, &[]));
        b.switch_to(l_win);
        fires::fire("num.lean.window"); // fire signal (D3224 A2: a code front record's sum without the range test)
        let narrow = narrow_fits(crate::rep::num_of(c.cell.rep).precision);
        window_add(b, env, (base, off, valid), &[w], narrow, avg.then_some(16), 2);
        b.br(l_join, &[]);
        b.switch_to(l_any);
    }
    let s63 = b.iconst(Type::I64, 63);
    let t = [w, b.int_bin(IntBinOp::Sshr, w, s63)];
    let (ok, s) = add(b, env, (base, off, valid), &t);
    let l_slow = past(b, s[1]);
    store(b, env, (base, off, valid), &s[..], avg.then_some(16), ok);
    b.br(l_join, &[]);
    b.switch_to(l_slow);
    // A1 (D3224): the cold fold reads the word again, so the hot path keeps no copy of it across the range test
    let w = window(b, env, c).map_or(w, |(_, w)| w);
    let yes = b.bconst(true);
    let t = crate::ops::agg::numeric::num_word::Term { lo: w, hi: None, sp: None, ok: yes, ds: None };
    crate::ops::agg::numeric::class_num::fold(b, env, a, Acc::Slot { base, off, valid }, &t, None);
    Some(open(b, env, l_join, l_gen))
}

/// The lean branch of a bare class.num_packed sum / avg into a Slot `acc` over a word `m` already decoded
/// (a verbatim or heap image: fold/num_word.rs `word`), under the same limits as `bare`. A non-NULL finite word (not a
/// sentinel: `word`'s decode proved it exact at the aggregate's scale) adds exactly into the slot's i128 with D299's
/// range test, a sum leaving the range recomputed by the generic fold from the untouched slot (cold); a NULL or a
/// special (NaN, +/-Inf, a word the decode could not take) runs the caller's generic step, which the builder is left in.
pub(crate) fn word<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, a: &AggDecl<'_>, acc: &Acc<'_>, m: Value, null: Value) -> Option<Lean<'e>> {
    let avg = match a.reg {
        R::SumNum => false,
        R::AvgNum => true,
        _ => return None,
    };
    let &Acc::Slot { base, off, valid } = acc else { return None };
    if !env.applies(crate::face::strategy::NUM_LEAN) || a.dscale_slot != NONE || env.mult.is_some() || crate::ops::agg::numeric::num_carry::spilled(a) {
        return None;
    }
    fires::fire("num.lean"); // shapecheck fire signal (REGISTRY TH-5, BN-119/120/126)
    let yes = b.bconst(true);
    let (fin, special) = (b.int_bin(IntBinOp::Xor, null, yes), crate::ops::agg::numeric::num_word::sentinel(b, env, m, yes).1);
    let finite = b.int_bin(IntBinOp::Xor, special, yes);
    let go = b.int_bin(IntBinOp::And, fin, finite);
    let (l_lean, l_gen, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
    b.cond_br(go, (l_lean, &[]), (l_gen, &[]));
    b.switch_to(l_lean);
    let s63 = b.iconst(Type::I64, 63);
    let t = [m, b.int_bin(IntBinOp::Sshr, m, s63)];
    let (ok, s) = add(b, env, (base, off, valid), &t);
    let l_slow = past(b, s[1]);
    store(b, env, (base, off, valid), &s[..], avg.then_some(16), ok);
    b.br(l_join, &[]);
    b.switch_to(l_slow);
    let t = crate::ops::agg::numeric::num_word::Term { lo: m, hi: None, sp: None, ok: yes, ds: None };
    crate::ops::agg::numeric::class_num::fold(b, env, a, Acc::Slot { base, off, valid }, &t, None);
    Some(open(b, env, l_join, l_gen))
}

/// D830's spilled sum / avg of a column past one word, whose datum folds in class.num_wide's i256 slot (num_carry.rs
/// `datum`: `a` its class.num_wide decl, `class` that class), over a word `m` already decoded (fold/num_word.rs `word`):
/// a non-NULL finite word (the decode proved it exact at the aggregate's scale) adds exactly into a Slot `acc`,
/// sign-extended, with D299's range test (past it the class's wide fold, cold); a NULL, a special or a value past one
/// word runs the caller's generic step (the datum's host term), which the builder is left in.
pub(crate) fn spilled<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, a: &AggDecl<'_>, class: &dyn FoldClass, acc: &Acc<'_>, m: Value, null: Value) -> Option<Lean<'e>> {
    let avg = match a.reg {
        R::SumNumWide => false,
        R::AvgNumWide => true,
        _ => return None,
    };
    let &Acc::Slot { base, off, valid } = acc else { return None };
    if !env.applies(crate::face::strategy::NUM_LEAN) || a.dscale_slot != NONE || env.mult.is_some() {
        return None;
    }
    fires::fire("num.lean.spill"); // shapecheck fire signal (A-24c (d): a column past one word adds its word)
    let yes = b.bconst(true);
    let (fin, special) = (b.int_bin(IntBinOp::Xor, null, yes), crate::ops::agg::numeric::num_word::sentinel(b, env, m, yes).1);
    let finite = b.int_bin(IntBinOp::Xor, special, yes);
    let go = b.int_bin(IntBinOp::And, fin, finite);
    let (l_lean, l_gen, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
    b.cond_br(go, (l_lean, &[]), (l_gen, &[]));
    b.switch_to(l_lean);
    let s63 = b.iconst(Type::I64, 63);
    let s = b.int_bin(IntBinOp::Sshr, m, s63);
    let (ok, w) = add(b, env, (base, off, valid), &[m, s, s, s]);
    let l_slow = past(b, w[3]);
    store(b, env, (base, off, valid), &w[..], avg.then_some(32), ok);
    b.br(l_join, &[]);
    b.switch_to(l_slow);
    class.fold_wide(b, env, a, Acc::Slot { base, off, valid }, &crate::face::fold::WideTerm { w: [m, s, s, s], sp: None, ok: yes });
    Some(open(b, env, l_join, l_gen))
}

/// The lean branch of a class.num_wide product `t` (every leaf a NOT NULL packed column, a constant or an int column)
/// into `acc`: its exact i256 term from the words (`wide_term` over a memo seeded with them: no leaf is special).
#[allow(clippy::too_many_arguments)]
pub(crate) fn wide<'t, 'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, a: &AggDecl<'_>, class: &dyn FoldClass, t: &'t Tree, acc: &Acc<'_>) -> Option<Lean<'e>> {
    let avg = match a.reg {
        R::SumNumWide => false,
        R::AvgNumWide => true,
        _ => return None,
    };
    let &Acc::Slot { base, off, valid } = acc else { return None };
    let mut cols = PgVec::new_in(env.mcx);
    if env.mult.is_some() || !leaves(t, &mut cols) {
        return None;
    }
    let (mut seeds, mut any, mut all_bounded) = (PgVec::new_in(env.mcx), b.bconst(false), true);
    for leaf in cols {
        let Tree::Col { c, scale, .. } = leaf else { unreachable!("leaves keeps columns") };
        let cell = row.col(*c).filter(|x| x.null.is_none() && crate::rep::num_of(x.cell.rep).scale == *scale)?;
        let (verbatim, w) = window(b, env, cell)?;
        all_bounded &= bounded(b, env, cell);
        any = b.int_bin(IntBinOp::Or, any, verbatim);
        seeds.push_in((leaf, w));
    }
    let word = env.rec_window.filter(|_| env.rec_zeroed && all_bounded && word_form(t));
    let (l_lean, l_gen, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
    b.cond_br(any, (l_gen, &[]), (l_lean, &[]));
    b.switch_to(l_lean);
    if let Some(f) = word {
        // D3224 A2: in a code front's record (this window's rows alone) the row's term as one word (num.share's checked
        // evaluator: a node past 18 digits tested), added with no range test; a term past the word takes the i256 form
        let (l_word, l_add, l_wide) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(f, (l_word, &[]), (l_wide, &[]));
        b.switch_to(l_word);
        fires::fire("num.lean.word"); // fire signal (D3224 A2: a code front record's wide sum one word per row)
        let mut bad = b.bconst(false);
        let mut leaf = |_: &mut FunctionBuilder<'_>, c: i16, s: i16, _: &mut Value| seed(&seeds[..], c, s);
        let w = crate::ops::agg::numeric::strategies::num_share::eval(b, t, &mut bad, &mut leaf, &mut Some(PgVec::new_in(env.mcx)));
        b.cond_br(bad, (l_wide, &[]), (l_add, &[]));
        b.switch_to(l_add);
        word_add(b, env, (base, off, valid), w, avg.then_some(32));
        b.br(l_join, &[]);
        b.switch_to(l_wide);
    }
    let term = lean_term(b, env, row, t, &seeds[..]);
    let (ok, s) = add(b, env, (base, off, valid), &term.w);
    let l_slow = past(b, s[3]);
    store(b, env, (base, off, valid), &s[..], avg.then_some(32), ok);
    b.br(l_join, &[]);
    b.switch_to(l_slow);
    // A1 (D3224): the cold fold reads the leaves and makes the term again, so the hot path keeps none of it across the test
    let mcx = env.mcx;
    let seeds = seeds.iter().map(|&(leaf, w)| (leaf, leaf_word(b, env, row, leaf).unwrap_or(w))).collect_in(mcx);
    let term = lean_term(b, env, row, t, &seeds[..]);
    class.fold_wide(b, env, a, Acc::Slot { base, off, valid }, &term);
    Some(open(b, env, l_join, l_gen))
}

/// D3224 A1: the lean branch of a class.num_packed tree `t` (an i128 or carry-form term) into a Slot `acc`: every leaf a
/// NOT NULL packed `Window` column at its scale or a constant, a window with no verbatim leaf folds the finite term of
/// the words (pgrc packs only finite values: no leaf's sentinel test, specials code or select), through class.num_packed's
/// fold under the same guard `g`; a verbatim window runs the caller's generic step.
pub(crate) fn tree<'t, 'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, a: &AggDecl<'_>, t: &'t Tree, acc: &Acc<'_>, g: Option<Value>) -> Option<Lean<'e>> {
    let &Acc::Slot { base, off, valid } = acc else { return None };
    let mut cols = PgVec::new_in(env.mcx);
    if env.mult.is_some() || !leaves(t, &mut cols) || cols.is_empty() || int_leaf(t) {
        return None;
    }
    let (mut seeds, mut any, mut all_bounded) = (PgVec::new_in(env.mcx), b.bconst(false), true);
    for leaf in cols {
        let Tree::Col { c, scale, .. } = leaf else { unreachable!("leaves keeps columns") };
        let cell = row.col(*c).filter(|x| x.null.is_none() && crate::rep::num_of(x.cell.rep).scale == *scale)?;
        let (verbatim, w) = window(b, env, cell)?;
        all_bounded &= bounded(b, env, cell);
        any = b.int_bin(IntBinOp::Or, any, verbatim);
        seeds.push_in((leaf, w));
    }
    fires::fire("num.lean.tree"); // fire signal (D3224 A1: R-KEEP's fire count of the lean tree branch)
    let (l_lean, l_gen, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
    b.cond_br(any, (l_gen, &[]), (l_lean, &[]));
    b.switch_to(l_lean);
    let (yes, mcx) = (b.bconst(true), env.mcx);
    let mut memo = crate::ops::agg::numeric::strategies::memo_num::Memo::new(true, mcx);
    for &(leaf, w) in &seeds {
        memo.put(leaf, Val { lo: w, hi: None, sp: None, ok: yes, kids: PgVec::new_in(mcx), sel: PgVec::new_in(mcx), ds: None, raw: None });
    }
    let term = crate::ops::agg::numeric::strategies::num_window::term(b, env, row, t, &mut memo);
    // D3224 A2: in a code front's record (this window's rows alone) a term under 10^33 at the slot's scale adds with no
    // range or overflow test: WINDOW_ROWS terms stay under 2^126 (D299's range, the i128)
    let fits = a.dscale_slot == NONE && matches!(a.reg, R::SumNum | R::AvgNum) && scale_of(t) == a.scale && digits_of(t) <= 33;
    if let Some(f) = env.rec_window.filter(|_| fits && all_bounded && env.rec_zeroed) {
        let (l_win, l_any) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(f, (l_win, &[]), (l_any, &[]));
        b.switch_to(l_win);
        fires::fire("num.lean.window"); // fire signal (D3224 A2: a code front record's sum without the range test)
        let s63 = b.iconst(Type::I64, 63);
        let hi = term.hi.unwrap_or_else(|| b.int_bin(IntBinOp::Sshr, term.lo, s63));
        let (words, n_at) = if crate::ops::agg::numeric::num_carry::spilled(a) { (4, 32) } else { (2, 16) };
        window_add(b, env, (base, off, valid), &[term.lo, hi], false, matches!(a.reg, R::AvgNum).then_some(n_at), words);
        b.br(l_join, &[]);
        b.switch_to(l_any);
    }
    crate::ops::agg::numeric::class_num::fold(b, env, a, Acc::Slot { base, off, valid }, &term, g);
    Some(open(b, env, l_join, l_gen))
}

/// D3224 A2: whether a window's sum of `precision`-digit words (|m| <= 10^p - 1) stays under 2^63 over a granule's rows
/// (cost's `PGRC_GRANULE_ROWS`, pgrc lower's WINDOW_ROWS: the bound `num_kinds` tests)
/// (p <= 15: the i64 form, no high word read).
fn narrow_fits(precision: u16) -> bool {
    let rows = gravity_program::cost::PGRC_GRANULE_ROWS as u128;
    precision > 0 && precision <= 18 && (10u128.pow(precision as u32) - 1) * rows < 1u128 << 63
}

/// D3224 A2: term `t` (one word, or `[lo, hi]`) added into a code front record's sum / avg slot of `words` words (the
/// i128 `[lo, hi]`, or the i256 one sign-extended), `n` + 1 at `n_at` (avg), marked valid. Untested: the record holds one
/// window's rows (`Env::rec_window`) and its caller bounded their sum; `narrow`: the sum is one i64 (`narrow_fits`),
/// its high word the sign.
fn window_add(b: &mut FunctionBuilder<'_>, env: &Env, (base, off, valid): (Value, i32, gravity_expr::emit::CellValid), t: &[Value], narrow: bool, n_at: Option<i32>, words: usize) {
    let s63 = b.iconst(Type::I64, 63);
    let (lo, hi) = if narrow {
        let lo = b.load(Type::I64, base, off);
        let s = b.int_bin(IntBinOp::Add, lo, t[0]);
        (s, b.int_bin(IntBinOp::Sshr, s, s63))
    } else {
        let (l, h) = (b.load(Type::I64, base, off), b.load(Type::I64, base, off + 8));
        let th = t.get(1).copied().unwrap_or_else(|| b.int_bin(IntBinOp::Sshr, t[0], s63));
        crate::ops::agg::trans::class_word::add128(b, l, h, t[0], th)
    };
    let top = (words > 2).then(|| b.int_bin(IntBinOp::Sshr, hi, s63));
    let mut s = mcx::pg_vec![in env.mcx; lo, hi];
    s.extend_in(top.into_iter().chain(top).take(words - 2));
    store(b, env, (base, off, valid), &s[..], n_at, None);
}

/// D3224 A2: whether num.share's word evaluator (`eval`) takes tree `t`: columns, constants, products, and sums /
/// differences whose sides rescale by at most 10^18 (an i64 power of ten).
fn word_form(t: &Tree) -> bool {
    crate::stack_check();
    match t {
        Tree::Col { .. } | Tree::Const { .. } => true,
        Tree::Mul(x, y) => word_form(x) && word_form(y),
        Tree::Add(x, y) | Tree::Sub(x, y) => [x, y].iter().all(|z| scale_of(t) - scale_of(z) <= 18 && word_form(z)),
        Tree::Int { .. } | Tree::PastWord { .. } | Tree::Case(_) | Tree::Cast { .. } | Tree::Div { .. } => false,
    }
}

/// D3224 A2: the packed word of leaf column `(c, scale)` among `wide`'s `seeds` (every column leaf is one).
fn seed(seeds: &[(&Tree, Value)], c: i16, scale: i16) -> Value {
    let hit = seeds.iter().find(|(l, _)| matches!(l, Tree::Col { c: lc, scale: ls, .. } if *lc == c && *ls == scale));
    hit.map(|x| x.1).unwrap_or_else(|| unreachable!("wide's seeds hold every column leaf"))
}

/// D3224 A2: word `t` added into a code front record's i256 sum / avg slot (`n` + 1 at `n_at`), marked valid: the low
/// pair's 128-bit add of `t` sign-extended, stored; the high pair moves by that add's carry out less `t`'s sign (+1 or
/// -1) only when the two differ, which a sum crossing zero or 2^128 does: a cold block that reads nothing the hot path
/// keeps. Exact with no range test: the record holds one window's rows (`Env::rec_window`, at most a granule's), from
/// zero (`Env::rec_zeroed`).
fn word_add(b: &mut FunctionBuilder<'_>, env: &Env, (base, off, valid): (Value, i32, gravity_expr::emit::CellValid), t: Value, n_at: Option<i32>) {
    let s63 = b.iconst(Type::I64, 63);
    let st = b.int_bin(IntBinOp::Sshr, t, s63);
    let (w0, w1) = (b.load(Type::I64, base, off), b.load(Type::I64, base, off + 8));
    let (s0, s1) = crate::ops::agg::trans::class_word::add128(b, w0, w1, t, st);
    store(b, env, (base, off, valid), &[s0, s1], n_at, None);
    // the carry out of the high words' top bit (w1 + st + carry in = s1): (w1 & st) | ((w1 | st) & !s1); its xor with
    // st's top bit is set exactly when the high pair moves
    let (a, o, ns) = (b.int_bin(IntBinOp::And, w1, st), b.int_bin(IntBinOp::Or, w1, st), b.int_un(gravity_ir::IntUnOp::Not, s1));
    let k = b.int_bin(IntBinOp::And, o, ns);
    let c = b.int_bin(IntBinOp::Or, a, k);
    let x = b.int_bin(IntBinOp::Xor, c, st);
    let moves = b.icmp(IntCond::Slt, x, env.zero);
    let (l_hi, l_done) = (b.create_block(&[]), b.create_block(&[]));
    b.mark_cold(l_hi);
    b.cond_br(moves, (l_hi, &[]), (l_done, &[]));
    b.switch_to(l_hi);
    let (w2, w3) = (b.load(Type::I64, base, off + 16), b.load(Type::I64, base, off + 24));
    let d = b.int_bin(IntBinOp::Or, st, env.one); // +1 (a carry, t >= 0) or -1 (a borrow, t < 0)
    let (h2, h3) = crate::ops::agg::trans::class_word::add128(b, w2, w3, d, st);
    b.store(base, off + 16, h2);
    b.store(base, off + 24, h3);
    b.br(l_done, &[]);
    b.switch_to(l_done);
}

/// D3224 A2: whether cell `c`'s kind is its window-start test (rep/packednum/datum.rs `window_bounded`).
fn bounded(b: &mut FunctionBuilder<'_>, env: &mut Env, c: CellRef<'_>) -> bool {
    let cx = env.lx(b, env.node);
    let r = crate::rep::packednum::datum::window_bounded(&cx, c);
    env.memo = cx.memo;
    r
}

/// Whether tree `t` reads an int column (its datum goes through the row memo, which a lean branch must not feed).
fn int_leaf(t: &Tree) -> bool {
    crate::stack_check();
    match t {
        Tree::Int { .. } => true,
        Tree::Mul(x, y) | Tree::Add(x, y) | Tree::Sub(x, y) => int_leaf(x) || int_leaf(y),
        _ => false,
    }
}

/// The exact i256 term of `t` over its column leaves' packed words `seeds` (no leaf is special: `wide_term` over a memo
/// seeded with them).
fn lean_term<'t>(b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, t: &'t Tree, seeds: &[(&'t Tree<'t>, Value)]) -> crate::face::fold::WideTerm {
    let (yes, mcx) = (b.bconst(true), env.mcx);
    let mut memo = crate::ops::agg::numeric::strategies::memo_num::Memo::new(true, mcx);
    for &(leaf, w) in seeds {
        memo.put(leaf, Val { lo: w, hi: None, sp: None, ok: yes, kids: PgVec::new_in(mcx), sel: PgVec::new_in(mcx), ds: None, raw: None });
    }
    crate::ops::agg::numeric::strategies::num_keyed::wide_term(b, env, row, t, &mut memo)
}

/// A column leaf's packed word read again (`wide`'s cold path); `None` for a leaf whose cell is gone.
fn leaf_word(b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, leaf: &Tree) -> Option<Value> {
    let Tree::Col { c, .. } = leaf else { return None };
    window(b, env, row.col(*c)?).map(|(_, w)| w)
}

/// Closes a lean branch after the generic step: its block joins, the row's memo is back.
pub(crate) fn join<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, lean: Option<Lean<'e>>) {
    if let Some(l) = lean {
        b.br(l.l_join, &[]);
        b.switch_to(l.l_join);
        env.memo.restore(l.memo);
    }
}

fn open<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, l_join: Block, l_gen: Block) -> Lean<'e> {
    b.br(l_join, &[]);
    b.switch_to(l_gen);
    Lean { l_join, memo: env.memo.set_aside() }
}

/// The column leaves of a tree the lean form reads (false: a CASE, which keeps the generic step).
fn leaves<'t>(t: &'t Tree<'t>, out: &mut PgVec<'_, &'t Tree<'t>>) -> bool {
    crate::stack_check();
    match t {
        Tree::Col { .. } => {
            out.push_in(t);
            true
        }
        Tree::Const { .. } | Tree::Int { .. } => true,
        Tree::Mul(x, y) | Tree::Add(x, y) | Tree::Sub(x, y) => leaves(x, out) && leaves(y, out),
        Tree::Case(_) | Tree::PastWord { .. } | Tree::Cast { .. } | Tree::Div { .. } => false,
    }
}

fn window(b: &mut FunctionBuilder<'_>, env: &mut Env, c: CellRef<'_>) -> Option<(Value, Value)> {
    let mut cx = env.lx(b, env.node);
    let got = crate::rep::packednum::datum::window_word(&mut cx, c);
    env.memo = cx.memo;
    got
}

/// The slot's words `[w..]` (an invalid slot the identity, as the classes' `load` reads it) plus term `t`, sign-extended:
/// the validity and the exact sum (the carry out of each word pair added into the next). A1 (D3224): in a zeroed record
/// (`Env::rec_zeroed`) an invalid slot's words are the identity already: no test (`None`).
fn add<'e>(b: &mut FunctionBuilder<'_>, env: &Env<'e, '_>, (base, off, valid): (Value, i32, gravity_expr::emit::CellValid), t: &[Value]) -> (Option<Value>, PgVec<'e, Value>) {
    let ok = (!env.rec_zeroed).then(|| crate::ops::shared::slots::slot_ok(b, env, base, valid));
    if ok.is_none() {
        fires::fire("num.lean.zeroed"); // fire signal (D3224 A1: a zeroed record's slot added untested)
    }
    let mut w: PgVec<'_, Value> = PgVec::new_in(env.mcx);
    for i in 0..t.len() as i32 {
        let v = b.load(Type::I64, base, off + 8 * i);
        w.push_in(ok.map_or(v, |ok| b.select(ok, v, env.zero)));
    }
    let (s0, s1) = crate::ops::agg::trans::class_word::add128(b, w[0], w[1], t[0], t[1]);
    if t.len() == 2 {
        return (ok, mcx::pg_vec![in env.mcx; s0, s1]);
    }
    // the carry out of the low pair: s1 below w1, or equal with t1 nonzero (then t1 + carry wrapped)
    let (lt, eq, nz) = (b.icmp(IntCond::Ult, s1, w[1]), b.icmp(IntCond::Eq, s1, w[1]), b.icmp(IntCond::Ne, t[1], env.zero));
    let wrap = b.int_bin(IntBinOp::And, eq, nz);
    let c = b.int_bin(IntBinOp::Or, lt, wrap);
    let c = b.conv(gravity_ir::ConvOp::Zext, Type::I64, c);
    let (s2, s3) = crate::ops::agg::trans::class_word::add128(b, w[2], w[3], t[2], t[3]);
    let (s2, s3) = crate::ops::agg::trans::class_word::add128(b, s2, s3, c, env.zero);
    (ok, mcx::pg_vec![in env.mcx; s0, s1, s2, s3])
}

/// Branches to a new cold block (returned) where top word `hi` left D299's range; the builder continues in the in-range one.
/// A1 (D3224): `hi` is in `[-SUM_HI_LIMIT, SUM_HI_LIMIT)` (2^62) exactly when `(hi >> 62) + 1` is 0 or 1, a test of small
/// immediates (no 64-bit constants per row and aggregate).
fn past(b: &mut FunctionBuilder<'_>, hi: Value) -> Block {
    const _: () = assert!(SUM_HI_LIMIT == 1 << 62, "past()'s shift is D299's limit");
    let (s62, one, two) = (b.iconst(Type::I64, 62), b.iconst(Type::I64, 1), b.iconst(Type::I64, 2));
    let top = b.int_bin(IntBinOp::Sshr, hi, s62);
    let h = b.int_bin(IntBinOp::Add, top, one);
    let out = b.icmp(IntCond::Uge, h, two);
    let (l_ok, l_slow) = (b.create_block(&[]), b.create_block(&[]));
    b.mark_cold(l_slow);
    b.cond_br(out, (l_slow, &[]), (l_ok, &[]));
    b.switch_to(l_ok);
    l_slow
}

/// Stores the sum words, `n` + 1 at `n_at` (avg), and marks the slot valid.
fn store(b: &mut FunctionBuilder<'_>, env: &Env, (base, off, valid): (Value, i32, gravity_expr::emit::CellValid), s: &[Value], n_at: Option<i32>, ok: Option<Value>) {
    for (i, &v) in s.iter().enumerate() {
        b.store(base, off + 8 * i as i32, v);
    }
    if let Some(at) = n_at {
        let n = b.load(Type::I64, base, off + at);
        let n = ok.map_or(n, |ok| b.select(ok, n, env.zero));
        let n = b.int_bin(IntBinOp::Add, n, env.one);
        b.store(base, off + at, n);
    }
    let yes = b.bconst(true);
    crate::ops::shared::slots::mark(b, env, base, valid, yes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_expr::emit::CellValid;
    use gravity_fn::agg_final::{SENT_HI, SP_NAN};
    use gravity_ir::builder::ProgramBuilder;

    /// The lean add over a slot `[lo, hi]` (valid) and a word `w`: its words, and whether it took the cold block, run
    /// through the interpreter. The in-range answer is the exact i128 sum; a sentinel state or a sum past D299's range
    /// takes the cold block (where the generic fold, `add_sum`, runs from the untouched slot).
    fn run(lo: i64, hi: i64, w: i64) -> (i64, i64, bool) {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let mut pb = ProgramBuilder::new();
        let core = crate::frame::env::declare(&mut pb);
        let f = pb.declare_func("t", &[Type::Ptr, Type::Ptr, Type::I64], &[Type::I64, Type::I64, Type::I64]);
        let mut b = pb.define(f);
        let p = b.params().to_vec();
        let x = crate::frame::stop::exits(&mut b);
        let env = Env::new(&mut b, core, crate::hir::Ir::none(), &[], x, rig.mcx());
        let s63 = b.iconst(Type::I64, 63);
        let wh = b.int_bin(IntBinOp::Sshr, p[2], s63);
        let (_, s) = add(&mut b, &env, (p[1], 0, CellValid::Always), &[p[2], wh]);
        let l_slow = past(&mut b, s[1]);
        let zero = b.iconst(Type::I64, 0);
        b.ret(&[s[0], s[1], zero]);
        b.switch_to(l_slow);
        let one = b.iconst(Type::I64, 1);
        b.ret(&[s[0], s[1], one]);
        for blk in [x.l_finish, x.l_stop_in, x.l_full, x.l_status, x.l_done, x.l_raise, x.l_cancel, x.l_exit] {
            b.switch_to(blk);
            let z = b.iconst(Type::I64, 0);
            b.ret(&[z, z, z]);
        }
        let prog = pb.finish();
        let mut ctx = vec![0u64; gravity_abi::ctx::ctx_size(1) / 8];
        let mut slot = [lo as u64, hi as u64];
        let args = [ctx.as_mut_ptr() as u64, slot.as_mut_ptr() as u64, w as u64];
        let got = gravity_interp::Interp::new(&prog).run(f, &args).expect("runs");
        (got[0] as i64, got[1] as i64, got[2] == 1)
    }

    /// D619: finite states add exactly and stay lean; a NaN state (SENT_HI) and a sum past D299's cap go cold, so the
    /// generic fold decides them (a verbatim NaN, then packed rows, still answers NaN).
    #[test]
    fn a_lean_add_is_exact_in_range_and_cold_for_a_sentinel_or_past_the_cap() {
        let m = (1i64 << 55) - 1;
        for (lo, hi, w) in [(0, 0, 5), (-3, -1, 2), (u64::MAX as i64, 7, 1), (5, 0, -m), (0, 0, m), (0, (1 << 62) - 2, m)] {
            let exact = (((hi as i128) << 64) | lo as u64 as i128) + w as i128;
            assert_eq!(run(lo, hi, w), (exact as i64, (exact >> 64) as i64, false), "{lo} {hi} {w}");
        }
        assert!(run(SP_NAN, SENT_HI, 7).2, "a NaN state takes the cold fold");
        assert!(run(SP_NAN, SENT_HI, -7).2, "a NaN state takes the cold fold (a borrow)");
        assert!(run(u64::MAX as i64, (1 << 62) - 1, 1).2, "past the cap: cold (the generic fold's OVERFLOW)");
        assert!(run(0, -(1 << 62), -1).2, "below the cap: cold");
        // D3224 A1: `past`'s shift form at each end of [-2^62, 2^62): the last word in, the first out
        assert!(!run(u64::MAX as i64 - 1, (1 << 62) - 1, 1).2, "the top of the range stays lean");
        assert!(!run(1, -(1 << 62), -1).2, "the bottom of the range stays lean");
        assert!(run(-1, -(1 << 62) - 1, 0).2 && run(0, i64::MAX, 0).2 && run(0, 1 << 62, 0).2, "each side out goes cold");
    }

    /// D3224 A2: `word_add` of word `t` into an avg's i256 slot `w` (its count `n` after it): the five words after it,
    /// through the interpreter.
    fn run_word(w: [u64; 4], t: i64) -> [u64; 5] {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let mut pb = ProgramBuilder::new();
        let core = crate::frame::env::declare(&mut pb);
        let f = pb.declare_func("t", &[Type::Ptr, Type::Ptr, Type::I64], &[Type::I64]);
        let mut b = pb.define(f);
        let p = b.params().to_vec();
        let x = crate::frame::stop::exits(&mut b);
        let env = Env::new(&mut b, core, crate::hir::Ir::none(), &[], x, rig.mcx());
        word_add(&mut b, &env, (p[1], 0, CellValid::Always), p[2], Some(32));
        let zero = b.iconst(Type::I64, 0);
        b.ret(&[zero]);
        for blk in [x.l_finish, x.l_stop_in, x.l_full, x.l_status, x.l_done, x.l_raise, x.l_cancel, x.l_exit] {
            b.switch_to(blk);
            let z = b.iconst(Type::I64, 0);
            b.ret(&[z]);
        }
        let prog = pb.finish();
        let mut ctx = vec![0u64; gravity_abi::ctx::ctx_size(1) / 8];
        let mut slot = [w[0], w[1], w[2], w[3], 41];
        let args = [ctx.as_mut_ptr() as u64, slot.as_mut_ptr() as u64, t as u64];
        gravity_interp::Interp::new(&prog).run(f, &args).expect("runs");
        slot
    }

    /// D3224 A2: the word add is the exact i256 sum (the words rippled, `t` sign-extended) and counts the row: a carry
    /// into the high pair, a borrow out of it (a sum crossing zero either way, the low pair wrapping at 2^128), states
    /// past 2^128 of either sign, and `t` at both of its extremes.
    #[test]
    fn a_word_add_is_the_exact_i256_sum() {
        let exact = |w: [u64; 4], t: i64| {
            let (mut out, mut c) = ([0u64; 5], 0u128);
            for (i, o) in out.iter_mut().take(4).enumerate() {
                let tw = if i == 0 { t as u64 } else { (t >> 63) as u64 };
                let s = w[i] as u128 + tw as u128 + c;
                *o = s as u64;
                c = s >> 64;
            }
            out[4] = 42;
            out
        };
        let m = u64::MAX;
        let slots = [[0, 0, 0, 0], [5, 0, 0, 0], [3, 0, 0, 0], [m, m, m, m], [m - 4, m, m, m], [m, m, 0, 0], [0, 0, 1, 0], [m, 0, 0, 0], [0, 1, 0, 0], [1, 2, 3, 4], [0, 0, m, m], [7, m, m - 1, m], [m, i64::MAX as u64, 0, 0]];
        let ts = [0, 1, -1, 5, -5, 1 << 55, -(1 << 55), i64::MAX, i64::MIN];
        for w in slots {
            for t in ts {
                assert_eq!(run_word(w, t), exact(w, t), "slot {w:?} word {t}");
            }
        }
    }
}

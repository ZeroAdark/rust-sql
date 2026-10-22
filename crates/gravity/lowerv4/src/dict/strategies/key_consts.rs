//! Road `key.codeif`'s constant-arm form (RD-key.codeif): a GROUP BY key `CASE WHEN <p> THEN '<a>' .. [ELSE '<z>'] END`
//! whose every arm is a text constant (key_codeif.rs `consts`) keys by the chosen arm's Fp128, not the row's text hash. The
//! image reads the projection's own CASE entry, so the WHENs run once a row (a volatile one too): its datum is the chosen
//! constant's word (the constants compile under the entry's root, D390), whose Fp128 a row table caches (host `text_fp`
//! once a window, its flag zeroed by the bind); NULL keys NULL, and a datum that is no constant's word hashes its row. The
//! projection carries the image in a hidden column (table/emit_group.rs `image_keys`) beside the key's text, which the
//! record keeps, orders and renders. Equal bytes hash equal whichever arm made them (two arms of one constant are one
//! group), so only under a collation whose equality is its bytes. Owner M3-WORDS. Budget 150.
use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;

use gravity_abi::breaker::KeyKind;
use gravity_abi::status::raise;
use gravity_decompose::ops::shared::agg::{GroupForm, KeyDecl};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, HostFn, IntCond, Type, Value};
use gravity_program::{DictRef, NumTypmod, Rep, Until};
use mcx::{CollectIn, PgVec};
use types_nodes::Node;

use crate::admit::refusal::Reason;
use crate::cell::{At, Cell, CellHome, CellLife, Nullable};
use crate::dict::percode::Tables;
use crate::dict::strategies::percode_key::{as_ptr, row_at, text_fp};
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px};
use crate::frame::row::RowVals;
use crate::graph::Gx;

/// The form's `Ex::ext`: row table `r` the image, `r + i` constant `i`'s cached Fp128 (1-based), the projection entry
/// whose value is the key's datum (`datum`), and, compiled after the key (`compile`), the non-NULL constants in row order.
struct ArmsFp {
    r: u8,
    datum: u16,
    consts: RefCell<Vec<ExId>>,
    tabs: Rc<RefCell<Tables>>,
}

/// Whether key `kd` (projection entry `e`) of a `form` group on pipeline `k` keys by its arms' constants (road key.codeif
/// on): every arm a text constant, a projected key (resno > 0) under a bytes-equal collation (D823), a hashed group over a
/// pgrcolumnar source (its window bind holds the rows), the key keeping its own datum (`rep_src`; a CASE over one code
/// column is percode.table's). table/emit_group.rs `image_keys` / `image_col` read it.
pub(crate) fn applies(gx: &Gx<'_, '_>, k: usize, form: GroupForm, kd: &KeyDecl<'_>, e: Node<'_>) -> bool {
    let p = &gx.pipelines()[k];
    let pgrc = matches!(p.source, gravity_decompose::Source::Range { am: gravity_decompose::Am::Pgrcolumnar, .. });
    let on = crate::table::keyimg::code_if(&p.node_id).is_some();
    let ok = on && pgrc && form == GroupForm::Hashed && kd.kind == KeyKind::Fp128 && kd.resno > 0 && gx.facts.coll(kd.coll).bytes_eq;
    ok && crate::dict::strategies::key_codeif::consts(crate::table::emit_group::bare(e)).is_some() && crate::table::emit_group::rep_src(gx, k, kd, p.node_id).is_some_and(|r| r.datum)
}

/// Key `e` (its CASE under any relabel) as its image on pipeline `k`, read off projection entry `datum`: its rows
/// declared, its constants compiled after it (inside the attach they would take the key's id) under `e`, the entry's root,
/// so each is the entry's own word (D390, exec admit.rs's template). Past the row slots it refuses (D1747's bound).
pub(crate) fn compile<'p>(px: &mut Px<'_, 'p>, k: usize, e: Node<'p>, datum: u16, node: i32) -> ExId {
    let mcx = px.mcx();
    let arms = crate::dict::strategies::key_codeif::consts(crate::table::emit_group::bare(e)).and_then(|c| crate::dict::strategies::key_codeif::arms(c, mcx));
    let Some(arms) = arms else { crate::refuse!(node, Reason::Bug { payload: "a constant-arm key image over another form" }) };
    let lits: PgVec<'_, Node<'p>> = arms.into_iter().filter(|a| a.as_const().is_some_and(|x| !x.constisnull)).collect_in(mcx);
    let tabs = crate::dict::percode::tables(px, k);
    let r = tabs.borrow().rows;
    let r2 = u8::try_from(lits.len() + 1).ok().and_then(|m| r.checked_add(m));
    let Some(r2) = r2.filter(|&r2| crate::dict::strategies::percode_guard::room(tabs.borrow().lanes.len(), r2.into())) else { crate::refuse!(node, Reason::Bug { payload: "a constant-arm key past the row slots" }) };
    tabs.borrow_mut().rows = r2;
    let g = Rc::new(ArmsFp { r, datum, consts: RefCell::new(Vec::new()), tabs }); // alloc: Program
    let ex = crate::step::expr::compile_ext(px, e, None, node, |_, _, _| Some(Rc::clone(&g) as Rc<dyn Any>));
    *g.consts.borrow_mut() = lits.iter().map(|&a| crate::step::expr::compile_under(px, a, None, node, e)).collect(); // alloc: Program
    fires::fire("dict.key_codeif.consts"); // shapecheck fire signal (RD-key.codeif): a constant-arm key keys by its constants
    ex
}

/// The projection entry whose value is the datum of image `id`, when `id` is a constant-arm key's image.
pub(crate) fn datum_of(env: &Env, id: ExId) -> Option<u16> {
    env.placed(id).ext.as_deref()?.downcast_ref::<ArmsFp>().map(|g| g.datum)
}

/// The image cell of `id` from its key's datum `v` (NULL where `null`), with its value and NULL: the cached Fp128 of the
/// constant whose word `v` is, else (no constant's word) `v`'s own hash.
pub(crate) fn cell(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals, id: ExId, v: Value, null: Value) -> (Cell, Value, Value) {
    let Some(g) = env.placed(id).ext.as_deref().and_then(|x| x.downcast_ref::<ArmsFp>()) else { crate::refuse!(node, Reason::Bug { payload: "a constant-arm key image without its form" }) };
    let mcx = env.mcx;
    let (r, consts) = (g.r, mcx::to_vec_in(&g.consts.borrow()[..], mcx));
    let (n, rows) = (g.tabs.borrow().lanes.len(), g.tabs.borrow().rows);
    let (Some(tables), false) = (env.tables, consts.is_empty()) else { crate::refuse!(node, Reason::Bug { payload: "a constant-arm key image with no rows bound or no constant" }) };
    let (slot, e) = row_at(n, rows, r);
    let entry = b.ptr_add(tables, env.zero, 8, (e * 8) as i32);
    let (t, f, h) = (b.bconst(true), b.bconst(false), text_fp(b));
    let (l_null, l_row, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::Bool]));
    let (dp, dw) = (as_ptr(b, v), word(b, v));
    // the constants' words: a load each, no read of the row
    let memo = env.memo.set_aside();
    let words: PgVec<'_, Value> = consts.iter().map(|&kx| { let w = crate::step::expr::emit(b, env, node, row, kx).0; word(b, w) }).collect_in(mcx);
    env.memo.restore(memo);
    let ks: PgVec<'_, [Block; 3]> = consts.iter().map(|_| [(); 3].map(|_| b.create_block(&[]))).collect_in(mcx);
    let l_val = b.create_block(&[]);
    b.cond_br(null, (l_null, &[]), (l_val, &[]));
    b.switch_to(l_val);
    for (i, &w) in words.iter().enumerate() {
        let next = if i + 1 == words.len() { l_row } else { b.create_block(&[]) };
        let hit = b.icmp(IntCond::Eq, dw, w);
        b.cond_br(hit, (ks[i][0], &[]), (next, &[]));
        b.switch_to(next);
    }
    for (i, &[l_arm, l_hash, l_cached]) in ks.iter().enumerate() {
        let (_, ce) = row_at(n, rows, r + 1 + i as u8);
        b.mark_cold(l_hash);
        b.switch_to(l_arm);
        let cache = b.ptr_add(tables, env.zero, 8, (ce * 8) as i32);
        let set = b.load(Type::I64, cache, 16);
        let set = b.icmp(IntCond::Ne, set, env.zero); // the flag word: hashed this window
        b.cond_br(set, (l_cached, &[]), (l_hash, &[]));
        b.switch_to(l_hash);
        hash(b, env, row, h, dp, cache); // the datum is this constant's word: its bytes
        b.store(cache, 16, env.one);
        b.br(l_cached, &[]);
        b.switch_to(l_cached);
        let (lo, hi) = (b.load(Type::I64, cache, 0), b.load(Type::I64, cache, 8));
        b.store(entry, 0, lo);
        b.store(entry, 8, hi);
        b.br(l_join, &[f]);
    }
    b.switch_to(l_row);
    hash(b, env, row, h, dp, entry);
    b.br(l_join, &[f]);
    b.switch_to(l_null);
    b.store(entry, 0, env.zero);
    b.store(entry, 8, env.zero);
    b.br(l_join, &[t]);
    b.switch_to(l_join);
    let null = b.block_params(l_join)[0];
    let life = CellLife { until: Until::Row, home: CellHome::ImageOnly };
    // no lane holds this image: its own row table (`slot`) at code 0, the entry's code word, which the bind zeroes; `at` names
    // no site lane (an image-only cell is never rendered, ranked or read as a word: rep/code/datum.rs)
    let rep = Rep::Code { dict: DictRef { lane: DictRef::NO_TABLE, fp_table: slot as u8 } };
    let at = At::Lane(crate::cell::LaneId::col(u16::MAX));
    let cell = Cell { rep, typid: env.placed(id).c.result_type, num: NumTypmod::NONE, life, at, null: Nullable::Maybe };
    (cell, b.ptr_add(entry, env.zero, 8, 16), null)
}

/// Datum `v` as an integer, its pointer.
fn word(b: &mut FunctionBuilder<'_>, v: Value) -> Value {
    let p = as_ptr(b, v);
    b.conv(ConvOp::PtrToInt, Type::I64, p)
}

/// The Fp128 of the text at `p` into `dst` by host `h`; a failed hash raises where PG does.
fn hash(b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, h: HostFn, p: Value, dst: Value) {
    let st = b.call_host(h, &[p, dst], Some(Type::I64)).unwrap_or(env.zero);
    let (failed, bit) = (b.icmp(IntCond::Ne, st, env.zero), b.iconst(Type::I64, raise::bit(raise::HOST) as i64));
    let err = b.select(failed, bit, env.zero);
    crate::step::expr::raise_if(b, env, row, err);
}

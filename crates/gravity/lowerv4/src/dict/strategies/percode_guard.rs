//! Road `key.codeif`'s key form (keyed/key_codeif.rs, V4R row of its landing; KEYED-31): a GROUP BY key `CASE WHEN <pred>
//! THEN <arm> ... [ELSE <arm>] END` whose arms are one text code column or constants (any number; BG-07) keys by the
//! Fp128 of its result without rendering text, as the percode key form's row table (step/percode_key.rs `cell`): where
//! the arm taken is the column, the lane's fingerprint entry at the row's code (`TableDecl::fingerprint`, bound per
//! window); where it is a constant, that constant text's Fp128, hashed by host `text_fp` once per window into its own
//! row table (one per non-NULL constant arm, after the image's; its flag word zeroed by the bind), then copied; where it is a NULL constant (a CASE with no ELSE), NULL with
//! no hash. A NULL code, or an entry the table lacks, re-evaluates the key over the row and hashes it (NULL: zero words,
//! the cell NULL), as the percode form's. A NULL pred is false and the WHENs run in order (PostgreSQL's CASE). The emit
//! renders a group from its image (`render`): a constant where the record's image is its Fp128 (equal bytes, whichever
//! arm made them), else the representative code.
//! Owner M3-WORDS. Budget 160.
use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;

use gravity_abi::status::raise;
use gravity_expr::Compiled;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_kernels::host_abi::bind_flag_slots;
use gravity_program::{DictRef, NumTypmod, Rep, TableDecl, Until};
use mcx::{CollectIn, PgVec};
use types_nodes::primnodes::{Var, OUTER_VAR};
use types_nodes::Node;

use crate::dict::percode::{Class, Tables};
use crate::dict::strategies::percode_key::{as_ptr, lookup, row_at, text_fp};
use crate::cell::{At, Cell, CellHome, CellLife, Nullable};
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px};
use crate::frame::row::RowVals;
use crate::graph::Gx;
use crate::admit::refusal::Reason;

/// What a guarded key's arm keys: the code column's entry, the `i`-th non-NULL constant arm (KEYED-31, BG-07: any
/// number), NULL.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Arm {
    Column,
    Const(u8),
    Null,
}

/// A guarded key's form (its `Ex::ext`): the code lane's fingerprint table `t`, row table `r` (the image; `r + 1 + i`
/// caches non-NULL constant arm `i`'s Fp128), the code column `col`, each arm's key (the WHENs' in order, then the ELSE's), the WHEN predicates
/// in order (compiled after the key: `finish`).
pub(crate) struct GuardFp {
    t: u16,
    r: u8,
    col: i16,
    arms: Vec<Arm>,
    preds: RefCell<Vec<ExId>>,
    tabs: Rc<RefCell<Tables>>,
}

/// D1747: `n` site tables and `rows` row tables leave every row slot (a `u8` `fp_table`) below `DictRef::NO_TABLE`; a
/// table or row past it is not declared, so its expression reads the column as its datum.
pub(crate) fn room(n: usize, rows: usize) -> bool {
    n + bind_flag_slots(n) + rows <= usize::from(DictRef::NO_TABLE)
}

/// D1747: `more` new tables on pipeline `k`'s site keep their site lanes (a `u8`) and the row slots (`room`) in range.
pub(crate) fn fits(px: &mut Px<'_, '_>, k: usize, more: usize) -> bool {
    let Some((nlanes, n)) = px.site_mut(k).map(|s| (s.nlanes(), s.tables.len())) else { return false };
    nlanes + more <= usize::from(u8::MAX) + 1 && room(n + more, crate::dict::percode::tables(px, k).borrow().rows.into())
}

/// Key `e` (collation `coll`, compiled `c`) over row step `j` of pipeline `k` in the key.codeif form, its code column a
/// code lane of the site and its equality by bytes: the form's tables declared.
pub(crate) fn attach(px: &mut Px<'_, '_>, k: usize, j: usize, c: &Compiled<'_>, e: Node<'_>, coll: u32, node: i32) -> Option<Rc<dyn Any>> {
    let v = then_var(px.gx, k, node, e).filter(|v| px.gx.facts.coll(coll).bytes_eq && c.result_type == v.vartype)?;
    let mut nconst = 0usize;
    let arms: Vec<Arm> = crate::dict::strategies::key_codeif::arms(e.as_case_expr()?, px.mcx())?.into_iter().map(|a| match a.as_const() {
        Some(k) if k.constisnull => Some(Arm::Null),
        Some(_) => {
            nconst += 1;
            u8::try_from(nconst - 1).ok().map(Arm::Const)
        }
        None => Some(Arm::Column),
    }).collect::<Option<_>>()?; // alloc: Program
    let &(attno, lane, _) = crate::step::project::code_cols(px, k, j).iter().find(|x| x.0 == v.varattno)?;
    if crate::dict::strategies::percode_key::keeps_datum(px.gx, k, j, e) {
        return None; // D3840: its image-only cell is no datum
    }
    let t = crate::dict::percode::declare_table(px, k, TableDecl::fingerprint(lane), Class::Fp)?;
    let tabs = crate::dict::percode::tables(px, k);
    let r = {
        let mut tb = tabs.borrow_mut();
        let r = tb.rows;
        // the image's row, then one per non-NULL constant arm; D1747: the row slot is a u8 fp_table
        tb.rows = u8::try_from(1 + nconst).ok().and_then(|n| r.checked_add(n)).filter(|&r2| room(tb.lanes.len(), r2.into()))?;
        r
    };
    fires::fire("dict.percode_guard"); // shapecheck fire signal (REGISTRY STEP-06)
    Some(Rc::new(GuardFp { t, r, col: attno, arms, preds: RefCell::new(Vec::new()), tabs })) // alloc: Program
}

/// Key `e` compiled by `key`; with the form, its WHEN predicates compiled after it over the same row step (a compile
/// inside the key's `attach` would take the key's own id).
pub(crate) fn finish<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, j: usize, e: Node<'p>, node: i32, key: impl FnOnce(&mut Px<'g, 'p>) -> ExId) -> ExId {
    let ex = key(px);
    let Some(g) = px.placed(ex).ext.clone().filter(|x| x.is::<GuardFp>()) else { return ex };
    let Some(c) = crate::table::keyimg::code_if(&node).and_then(|m| m.form(e)).and_then(|f| f[0].as_case_expr()) else { return ex };
    let ids: Vec<ExId> = c.args.iter().filter_map(|w| w.as_case_when()?.expr).map(|p| crate::step::expr::compile_at(px, k, j, p, None, node)).collect(); // alloc: Program
    g.downcast_ref::<GuardFp>().map(|g| *g.preds.borrow_mut() = ids);
    ex
}

fn fail(b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, st: Value) {
    let failed = b.icmp(IntCond::Ne, st, env.zero);
    let bit = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
    let err = b.select(failed, bit, env.zero);
    crate::step::expr::raise_if(b, env, row, err);
}

fn copy(b: &mut FunctionBuilder<'_>, from: Value, to: Value) {
    let (lo, hi) = (b.load(Type::I64, from, 0), b.load(Type::I64, from, 8));
    b.store(to, 0, lo);
    b.store(to, 8, hi);
}

/// The key cell of `id` over `row` when `id` has the guarded form (`None` otherwise), with its value and NULL.
pub(crate) fn cell(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals, id: ExId) -> Option<(Cell, Value, Value)> {
    let g: &GuardFp = env.placed(id).ext.as_deref()?.downcast_ref()?;
    let (n, rows) = (g.tabs.borrow().lanes.len(), g.tabs.borrow().rows);
    let preds = mcx::to_vec_in(&g.preds.borrow()[..], env.mcx);
    let (false, Some(tables), Some(src)) = (preds.is_empty(), env.tables, row.col(g.col)) else { crate::refuse!(node, Reason::Bug { payload: "a guarded key with no predicate, tables or code column" }) };
    let (Rep::Code { dict }, At::Lane(_)) = (src.cell.rep, src.cell.at) else { crate::refuse!(node, Reason::Bug { payload: "a guarded key whose THEN column is not read by code" }) };
    let (slot, e) = row_at(n, rows, g.r);
    let entry = b.ptr_add(tables, env.zero, 8, (e * 8) as i32);
    let (t, f) = (b.bconst(true), b.bconst(false));
    let [l_then, l_slow, l_val, l_null] = [(); 4].map(|_| b.create_block(&[]));
    let nconst = g.arms.iter().filter(|a| matches!(a, Arm::Const(_))).count();
    // per non-NULL constant arm: its block, its cache row's address
    let consts: PgVec<'_, (gravity_ir::Block, Value)> = (0..nconst).map(|i| {
        let (_, ce) = row_at(n, rows, g.r + 1 + i as u8);
        (b.create_block(&[]), b.ptr_add(tables, env.zero, 8, (ce * 8) as i32))
    }).collect_in(env.mcx);
    let l_join = b.create_block(&[Type::Bool]);
    for l in [l_slow, l_val, l_null] {
        b.mark_cold(l);
    }
    let at = |a: Arm| match a {
        Arm::Column => l_then,
        Arm::Const(i) => consts[i as usize].0,
        Arm::Null => l_null,
    };
    if g.arms.len() != preds.len() + 1 {
        crate::refuse!(node, Reason::Bug { payload: "a guarded key whose arms are not its WHENs and ELSE" });
    }
    for (i, &pred) in preds.iter().enumerate() {
        // The WHENs in order, as PostgreSQL's CASE: a later predicate runs only where every earlier one is not true.
        let next = if i + 1 == preds.len() { at(g.arms[preds.len()]) } else { b.create_block(&[]) };
        let p = crate::step::expr::pred(b, env, node, row, pred);
        b.cond_br(p, (at(g.arms[i]), &[]), (next, &[]));
        if i + 1 < preds.len() {
            b.switch_to(next);
        }
    }
    b.switch_to(l_then);
    let [lo, hi] = lookup(b, env.zero, tables, g.t, src.val, src.null, l_slow);
    b.store(entry, 0, lo);
    b.store(entry, 8, hi);
    b.br(l_join, &[f]);
    let memo = env.memo.set_aside();
    for &(l_else, cache) in &consts {
        // the arm's constant: the CASE's value on this row (its arm is taken), hashed once per window
        let [l_cached, l_hash] = [(); 2].map(|_| b.create_block(&[]));
        b.mark_cold(l_hash);
        b.switch_to(l_else);
        let set = b.load(Type::I64, cache, 16);
        let set = b.icmp(IntCond::Ne, set, env.zero);
        b.cond_br(set, (l_cached, &[]), (l_hash, &[]));
        b.switch_to(l_hash);
        env.memo.clear();
        let (v, _) = crate::step::expr::emit(b, env, node, row, id);
        let (v, h) = (as_ptr(b, v), text_fp(b));
        let st = b.call_host(h, &[v, cache], Some(Type::I64)).unwrap_or(env.zero);
        fail(b, env, row, st);
        b.store(cache, 16, env.one);
        b.br(l_cached, &[]);
        b.switch_to(l_cached);
        copy(b, cache, entry);
        b.br(l_join, &[f]);
    }
    b.switch_to(l_slow);
    env.memo.clear(); // l_hash's values do not reach here
    let (v, valid) = crate::step::expr::emit(b, env, node, row, id);
    env.memo.restore(memo);
    b.cond_br(valid, (l_val, &[]), (l_null, &[]));
    b.switch_to(l_val);
    let (v, h) = (as_ptr(b, v), text_fp(b));
    let st = b.call_host(h, &[v, entry], Some(Type::I64)).unwrap_or(env.zero);
    fail(b, env, row, st);
    b.br(l_join, &[f]);
    b.switch_to(l_null);
    b.store(entry, 0, env.zero);
    b.store(entry, 8, env.zero);
    b.br(l_join, &[t]);
    b.switch_to(l_join);
    let null = b.block_params(l_join)[0];
    let life = CellLife { until: Until::Row, home: CellHome::ImageOnly };
    let rep = Rep::Code { dict: DictRef { lane: dict.lane, fp_table: slot as u8 } };
    let cell = Cell { rep, typid: env.placed(id).c.result_type, num: NumTypmod::NONE, life, at: src.cell.at, null: Nullable::Maybe };
    Some((cell, b.ptr_add(entry, env.zero, 8, 16), null))
}


/// The code column of group key `e` of pipeline `k` in the guarded form (keyed_emit_group's representative), else `None`.
/// KEYED-31: `None` too where the per-entry image does not pay over the row's text key (`image_pays`): the key is then the
/// row's text with its datum kept, as any derived key's. `attach` and the emit's representative both read the form here,
/// so a declined key is the text key's whole road (its code representative would render each group through its part).
pub(crate) fn then_var<'a>(gx: &Gx<'_, '_>, k: usize, node: i32, e: Node<'a>) -> Option<&'a Var<'a>> {
    let v = crate::table::keyimg::code_if(&node).and_then(|m| m.form(e)).and_then(|f| f[1].as_var())?;
    if image_pays(gx, k, v, e) {
        return Some(v);
    }
    fires::fire("dict.key_codeif.text_key"); // shapecheck fire signal (KEYED-31): the cost home kept the row's text key
    None
}

/// KEYED-31: whether code column `v` of pipeline `k`'s key `e` keys by its per-entry image: the cost home's rule
/// (`cost::codeif_keys_by_image`) at the relation's rows, the column's entries, the server's widest width, the form and
/// whether any emit renders the key's groups (emit_group `unread_key`: count(DISTINCT)'s argument pays no cost an entry)
/// (`Facts::rel_rows`, `att_entries`, `width`: the program serves every session, whatever width each runs at, so the rule
/// judges it at both ends, the leader alone and the widest), under `PERCODE_DATUM_ROWS_MIN` rows always. Without those
/// facts (no pgrc relation, a column projected from no scan column) the image stays: a stale count or width only picks
/// the slower of two forms with the same answers.
fn image_pays(gx: &Gx<'_, '_>, k: usize, v: &Var<'_>, e: Node<'_>) -> bool {
    use gravity_program::cost::{codeif_keys_by_image, PERCODE_DATUM_ROWS_MIN};
    let p = &gx.pipelines()[k];
    let gravity_decompose::graph::Source::Range { relid, .. } = p.source else { return true };
    // a key over the projection (outer.N): the projection's entry N, a scan column
    let attno = if v.varno == OUTER_VAR { crate::table::emit_group::entry_var(&p.ops, v.varattno).map(|s| s.varattno) } else { Some(v.varattno) };
    let (Some(attno), Some(rows)) = (attno, gx.facts.rel_rows(relid)) else { return true };
    let Some(entries) = gx.facts.att_entries(relid, attno) else { return true };
    let rendered = !crate::table::emit_group::unread_key(gx, k, e);
    rows < PERCODE_DATUM_ROWS_MIN || codeif_keys_by_image(entries, rows, gx.facts.width().unwrap_or(0), column_then(e), rendered)
}

/// The column-THEN form, `CASE WHEN p THEN col [WHEN p THEN col ...] ELSE 'x' END`: every WHEN's result the column, the
/// ELSE the constant.
fn column_then(e: Node<'_>) -> bool {
    e.as_case_expr().is_some_and(|c| {
        c.args.iter().all(|w| w.as_case_when().and_then(|w| w.result).is_some_and(|r| r.as_var().is_some())) && c.defresult.is_some_and(|d| d.as_const().is_some())
    })
}

/// A group key's emit expressions: the key over its representative column, or in the guarded form (`guard`) the code
/// column and each non-NULL constant arm (`render` picks by the image; a NULL constant's rows key NULL, and a NULL group
/// renders NULL from the record's flag).
pub(crate) fn render_exs<'g, 'p>(px: &mut Px<'g, 'p>, e: Node<'p>, node: i32, guard: bool) -> (ExId, PgVec<'g, ExId>) {
    let form = crate::table::keyimg::code_if(&node).and_then(|m| m.form(e)).filter(|_| guard);
    let mcx = px.mcx();
    let arms = form.and_then(|f| crate::dict::strategies::key_codeif::arms(f[0].as_case_expr()?, mcx));
    match (form, arms) {
        (Some([_, then, _]), Some(arms)) => {
            let then = crate::step::expr::compile(px, then, None, node);
            let consts = arms.into_iter().filter(|a| a.as_const().is_some_and(|k| !k.constisnull)).map(|a| crate::step::expr::compile(px, a, None, node)).collect_in(mcx);
            (then, consts)
        }
        _ => (crate::step::expr::compile(px, e, None, node), PgVec::new_in(mcx)),
    }
}

/// A group key's output over its representative `src`: `ex`'s value, or a constant of `els` where the record `rec`'s
/// image words at `word` are that constant text's Fp128.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, src: &RowVals, ex: ExId, els: &[ExId], rec: Value, word: usize) -> (Value, Value) {
    let (mut v, mut valid) = crate::step::expr::emit(b, env, node, src, ex);
    for &els in els {
        (v, valid) = render_const(b, env, node, src, (v, valid), els, rec, word);
    }
    (v, valid)
}

/// `render`'s pick for one constant `els`: it where the record's image is its Fp128, else `(v, valid)`.
#[allow(clippy::too_many_arguments)]
fn render_const(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, src: &RowVals, (v, valid): (Value, Value), els: ExId, rec: Value, word: usize) -> (Value, Value) {
    let (ev, _) = crate::step::expr::emit(b, env, node, src, els);
    let at = { let s = b.declare_stack_slot(16, 8); b.stack_addr(s) };
    let (p, h) = (as_ptr(b, ev), text_fp(b));
    let st = b.call_host(h, &[p, at], Some(Type::I64)).unwrap_or(env.zero);
    fail(b, env, src, st);
    let [a, bb, r0, r1] = [(at, 0), (at, 8), (rec, 8 * word as i32), (rec, 8 * word as i32 + 8)].map(|(p, o)| b.load(Type::I64, p, o));
    let (x, y) = (b.icmp(IntCond::Eq, a, r0), b.icmp(IntCond::Eq, bb, r1));
    let same = b.int_bin(IntBinOp::And, x, y);
    let ev = match (b.value_type(v), b.value_type(ev)) {
        (Type::Ptr, Type::I64) => b.conv(ConvOp::IntToPtr, Type::Ptr, ev),
        (Type::I64, Type::Ptr) => b.conv(ConvOp::PtrToInt, Type::I64, ev),
        _ => ev,
    };
    let t = b.bconst(true);
    (b.select(same, ev, v), b.select(same, t, valid))
}

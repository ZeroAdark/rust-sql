//! A jsonb path group key's representative: its code, or its text where no lane row reached the group
//! (V4R-468, main 09-28 "datum when no code"). Owner M1-KEYED. Budget 170 lines.
//!
//! A key over a path's code lane (source/jsonb_keys.rs) keeps, as a derived text key does, its source row's code
//! `part << 32 | code` in its MinCode slot (keyed_emit_group `rep_words`, over the hidden column `hidden_reps` carries), so
//! the code fronts take it (code.front_dense's derived shape: a record per code of the window, flushed into the table).
//! A NULL lane row (its value a number, or in a part that did not elect the path) has no code: at its record's open
//! (`keep`, before keyed_emit_key `keep_datums`) the record keeps a copy of the row's text instead, tagged `COPY | copy`
//! in the same slot. Codes sit below `COPY`, copies at or over it and below `MINCODE_NONE`, so the slot's min fold (the
//! owner merge, a front's flush: `Rec::fold`) keeps a code whenever any member of the group has one. The emit (`cell`)
//! renders a code through its part's dictionary, a copy as itself: either is the group's value (equal images, equal
//! bytes). A slot still `MINCODE_NONE` under a non-NULL key is unreachable (a form that opened the record without `keep`):
//! a debug assert and a named Bug (`nocode_unset`), never a row. The table keeps its copies resident (keyed_emit_key `copies`,
//! `resident`) and runs no run carry.
use gravity_abi::breaker::MINCODE_NONE;
use gravity_abi::status::raise;
use gravity_decompose::ops::shared::agg::{GroupDecl, KeyDecl};
use gravity_decompose::AttFact;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_program::{DictRef, NumTypmod, Rep};

use crate::cell::{Cell, Nullable};
use crate::frame::env::Env;
use crate::frame::row::RowVals;
use crate::graph::Gx;
use crate::lx::CellRef;
use crate::admit::refusal::{Reason, ShapeWhy};

use crate::table::emit_group::RepSrc;
use crate::ops::agg::hashagg::GroupRec;

/// W9 PERF C5 S5: a path key's count front (code.front_light, face/keyed.rs `count_front`): the valid lane rows' codes counted
/// per part, each touched code's image its fingerprint table's entry (the table step/percode_key.rs `attach` declared: one
/// lane, `declare_table` dedups), its representative the code; a NULL lane row takes the table arm (the key re-evaluated).
pub(crate) fn light<'g>(px: &mut crate::frame::px::Px<'g, '_>, k: usize, rec: &GroupRec<'_, '_>, t: &crate::table::face::TableDecl) -> Option<(&'static dyn crate::table::face::CountFrontMech, crate::table::face::CountShape<'g>, mcx::PgVec<'g, i16>)> {
    let g = rec.g;
    let src = rec.reps.first().copied().flatten().filter(|r| r.nocode && px.gx.facts.coll(g.keys[0].coll).bytes_eq)?;
    if rec.nums.as_ref().is_some_and(|n| !n.is_empty()) || g.two_pass.is_some() || g.set.is_some() || g.aggs.len() != rec.aggs.len() || !matches!(g.form, gravity_decompose::ops::shared::agg::GroupForm::Hashed | gravity_decompose::ops::shared::agg::GroupForm::Sorted) {
        return None;
    }
    let Rep::Code { dict } = src.rep else { return None };
    let fp = crate::dict::percode::declare_table(px, k, gravity_program::TableDecl::fingerprint(dict.lane), crate::dict::percode::Class::Fp)?;
    let m = crate::table::face::count_front(&rec.node)?;
    m.plan(t, g, &[crate::table::face::CountKey { col: src.col, lane: dict.lane, fp_table: u8::try_from(fp).ok()? }], px.mcx()).map(|c| (m, c, mcx::pg_vec![in px.mcx(); src.col]))
}

/// A kept copy's tag: over every `part << 32 | code`, under `MINCODE_NONE` with any user-space address below it (abi's,
/// which an in-memory compact's reclaim reads, D3312).
const COPY: u64 = gravity_abi::breaker::IMG_TAG_COPY;

/// Key `k` of `producer` as a path key's representative (keyed_emit_group `rep_src`): the path lane's code column.
pub(crate) fn rep_src(gx: &Gx<'_, '_>, producer: usize, k: &KeyDecl<'_>) -> Option<RepSrc> {
    let (at, lane) = key_path(gx, producer, k)?;
    let col = crate::table::emit_group::hidden_col(gx, producer, k, at)?;
    let rep = Rep::Code { dict: DictRef::lane(lane) };
    Some(RepSrc { col, attno: at, typid: types_core::TEXTOID, rep, derived: true, guard: false, datum: false, imaged: false, nocode: true })
}

/// `reps` (keyed_emit_group `rep_words`) with each path key's word, at a `fresh` record's open from a NULL lane row of a
/// non-NULL key, the tagged copy of the row's text: the path form's stash (row table `r + 1`'s entry word, past the key
/// cell's `Code` entry: step/percode_key.rs `cell`), or the key's own text where the key compiled as its value.
#[allow(clippy::too_many_arguments)]
pub(crate) fn keep<'e>(rec: &GroupRec<'_, '_>, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, fresh: Value, images: &mut dyn FnMut(&mut FunctionBuilder<'_>, &Env) -> Option<Value>, mut reps: mcx::PgVec<'e, Option<Value>>) -> mcx::PgVec<'e, Option<Value>> {
    for i in (0..rec.reps.len()).filter(|&i| rec.reps[i].is_some_and(|r| r.nocode)) {
        let (Some(w), Some(c)) = (reps[i], row.col(rec.g.keys[i].resno)) else { bug(rec.node, "a path key without its word or its column") };
        let none = b.const_raw(Type::I64, MINCODE_NONE);
        let mut go = b.icmp(IntCond::Eq, w, none);
        go = b.int_bin(IntBinOp::And, go, fresh);
        if let Some(n) = c.null {
            let t = b.bconst(true);
            let set = b.int_bin(IntBinOp::Xor, n, t);
            go = b.int_bin(IntBinOp::And, go, set);
        }
        let (l_copy, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
        b.mark_cold(l_copy);
        b.cond_br(go, (l_copy, &[]), (l_join, &[w]));
        b.switch_to(l_copy);
        let memo = env.memo.set_aside();
        let p = match c.cell.rep {
            Rep::Code { .. } => b.load(Type::I64, c.val, 8),
            Rep::VarlenaPtr => {
                let mut cx = env.lx(b, rec.node);
                let (p, _) = crate::rep::datum(&mut cx, c, &rec.tfs[i]);
                env.memo = cx.memo;
                p
            }
            _ => bug(rec.node, "a path key's cell neither its code form nor its text"),
        };
        env.memo.restore(memo);
        let Some(images) = images(b, env) else { bug(rec.node, "a path key's copy with no images cursor") };
        let h = b.declare_host_once("key_copy", &[Type::Ptr, Type::Ptr, Type::I64], Some(Type::I64), crate::table::key_copy::key_copy as *const () as usize, HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() });
        let kept = b.call_host(h, &[env.ctx, images, p], Some(Type::I64)).expect("returns the copy");
        let (failed, l_ok) = (b.icmp(IntCond::Slt, kept, env.zero), b.create_block(&[]));
        let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
        b.cond_br(failed, (env.l_raise, &[e]), (l_ok, &[]));
        b.switch_to(l_ok);
        let tag = b.const_raw(Type::I64, COPY);
        let tagged = b.int_bin(IntBinOp::Or, kept, tag);
        b.br(l_join, &[tagged]);
        b.switch_to(l_join);
        reps[i] = Some(b.block_params(l_join)[0]);
    }
    reps
}

/// Group key `key` of producer `k` as a jsonb path (face/codes.rs `PathKey`): its row column and its drafted lane.
pub(crate) fn key_path(gx: &Gx<'_, '_>, k: usize, key: &KeyDecl<'_>) -> Option<(i16, u8)> {
    crate::dict::face::path_key(gx.pipelines()[k].node_id).and_then(|f| (f.key)(gx, k, key))
}

/// A path column's facts (step/project.rs's hidden representative, `SHRED_AT + n`): a text.
pub(crate) fn att(a: i16) -> Option<AttFact> {
    (a >= crate::frame::site::SHRED_AT).then_some(AttFact::plain(types_core::TEXTOID, -1, -1, false, false))
}

/// A path key's emit (keyed_emit_key `KeyOut::Nocode`): its record slot, its code's rep and its output cell.
pub(crate) struct Out {
    slot: u16,
    rep: Rep,
    out: Cell,
}

pub(crate) fn out(gx: &Gx<'_, '_>, g: &GroupDecl<'_>, kd: &KeyDecl<'_>, rep: Rep, node: i32) -> Out {
    let Some(o) = gx.out_of(kd.expr.node()) else { crate::refuse!(node, Reason::Shape { why: ShapeWhy("a jsonb path group key without an output rep") }) };
    Out { slot: crate::frame::pairs_view::rep_slot(g, kd.rep), rep, out: Cell::of_expr(o, 0, 0, Nullable::Maybe) }
}

/// Key `i`'s cell of the record at `at` (`null`: the key's flag): its code rendered, or its kept copy.
pub(crate) fn cell(rec: &GroupRec<'_, '_>, o: &Out, b: &mut FunctionBuilder<'_>, env: &mut Env, i: usize, at: Value, null: Option<Value>) -> (Cell, Value, Option<Value>) {
    let w = b.load(Type::I64, at, rec.layout.slot_off(o.slot as usize) as i32);
    let (none, tag, t) = (b.const_raw(Type::I64, MINCODE_NONE), b.const_raw(Type::I64, COPY), b.bconst(true));
    let unset = b.icmp(IntCond::Eq, w, none);
    let flag = null.unwrap_or_else(|| b.bconst(false));
    let (l_set, l_code, l_copy, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64, Type::Bool]));
    let (l_unset, l_bug) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(unset, (l_unset, &[]), (l_set, &[]));
    b.switch_to(l_unset);
    let zero = env.zero;
    b.cond_br(flag, (l_join, &[zero, t]), (l_bug, &[]));
    b.switch_to(l_bug);
    b.mark_cold(l_bug);
    let h = b.declare_host_once("nocode_unset", &[Type::Ptr], Some(Type::I64), super::raw::nocode_unset as *const () as usize, HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() });
    b.call_host(h, &[env.ctx], Some(Type::I64));
    let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
    b.br(env.l_raise, &[e]);
    b.switch_to(l_set);
    let copy = b.icmp(IntCond::Uge, w, tag);
    b.cond_br(copy, (l_copy, &[]), (l_code, &[]));
    b.switch_to(l_copy);
    let p = b.int_bin(IntBinOp::Xor, w, tag);
    b.br(l_join, &[p, flag]);
    b.switch_to(l_code);
    let code = Cell::of_record(o.rep, rec.g.keys[i].tf, NumTypmod::NONE, rec.home, o.slot, Nullable::Never);
    let memo = env.memo.set_aside();
    let mut cx = env.lx(b, rec.node);
    let (p, _) = crate::rep::datum(&mut cx, CellRef { cell: &code, val: w, null: None }, &rec.tfs[i]);
    env.memo = cx.memo;
    env.memo.restore(memo);
    b.br(l_join, &[p, flag]);
    b.switch_to(l_join);
    let (v, n) = (b.block_params(l_join)[0], b.block_params(l_join)[1]);
    (o.out, v, Some(n))
}

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

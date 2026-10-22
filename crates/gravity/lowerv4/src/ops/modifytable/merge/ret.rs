//! D789 (M4-RET): MERGE .. RETURNING's row for write.returning (`RetMech::lower_merge`, write/merge.rs MergeStep). A
//! MERGE's images differ by action, so the written row is the scan row (plain `t.col`: an UPDATE's new row, a DELETE's
//! deleted version) and the inner row carries OLD at attnos 1..=natts, NEW after them and `merge_action()`'s text last
//! (write/ret_cols.rs `images`, gravity_expr `Returning`). Helper file of write.returning: no rows of its own.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;
use gravity_program::{ExprLife, ExprOut, Rep};
use mcx::{GrowIn, PgVec};

use crate::cell::{Cell, Nullable, Row};
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::instr::{self, Ctr};
use crate::frame::row::{Carry, RowVals};
use crate::admit::refusal::Reason;

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

/// D789: `merge_action()`'s text datums (a 4-byte varlena header, as PostgreSQL's `CStringGetTextDatum`), static so a
/// cached program's address stays valid for the process.
#[repr(C, align(8))]
struct Text10([u8; 10]);
static MERGE_UPDATE: Text10 = Text10([40, 0, 0, 0, b'U', b'P', b'D', b'A', b'T', b'E']);
static MERGE_DELETE: Text10 = Text10([40, 0, 0, 0, b'D', b'E', b'L', b'E', b'T', b'E']);
static MERGE_INSERT: Text10 = Text10([40, 0, 0, 0, b'I', b'N', b'S', b'E', b'R', b'T']);

/// D789: the text of `merge_action()` for a MERGE action's command.
pub(crate) fn merge_action(delete: bool) -> &'static [u8] {
    if delete { &MERGE_DELETE.0 } else { &MERGE_UPDATE.0 }
}

/// W6: the text of `merge_action()` for a NOT MATCHED INSERT.
pub(crate) fn merge_action_insert() -> &'static [u8] {
    &MERGE_INSERT.0
}

/// The row of one MERGE action: `got` written, OLD `old` (an UPDATE's replaced version, empty when unread; a DELETE's is
/// `got`), NEW `got` when `new` (UPDATE) else NULLs, then `action`'s text; handed to `next` as write.returning's row.
/// `sys` (writesubplan, gen mg-0178): the written row's system columns (D774, the host's sys site), at their lane attnos
/// of the written row, or empty.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lower<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, (atts, node, write_node, outer): (&[Option<ExprOut>], i32, i32, &(Vec<i16>, i16)), (got, sys): (&[(Value, Value)], &[(Value, Value)]), old: &[(Value, Value)], new: bool, action: &'static [u8], next: Next<'_, 'e>) -> Carry<'e> {
    if got.len() != atts.len() || (!new && !old.is_empty()) || (new && old.len() != atts.len() && !old.is_empty()) {
        bug(node, "a MERGE action's written row or old version whose cells are not the target's attributes");
    }
    let (zero, null, f) = (b.iconst(gravity_ir::Type::I64, 0), b.bconst(true), b.bconst(false));
    // OLD: an UPDATE's replaced version (NULLs when the list reads no OLD: no fetch), a DELETE's deleted row; NEW: an
    // UPDATE's written row, a DELETE's NULLs; then merge_action()'s text
    let old: &[(Value, Value)] = if new { old } else { got };
    let img = |k: usize, of: &[(Value, Value)]| of.get(k).copied().unwrap_or((zero, null));
    let mcx = env.mcx;
    let (mut cells, mut vals, mut cols) = (PgVec::new_in(mcx), PgVec::new_in(mcx), PgVec::new_in(mcx));
    let mut map = PgVec::new_in(mcx);
    let lay = |out: ExprOut, v: (Value, Value), cells: &mut PgVec<'_, Cell>, vals: &mut PgVec<'_, (Value, Option<Value>)>| {
        let at = cells.len() as u16;
        cells.push_in(Cell::of_expr(out, at, at, Nullable::Maybe));
        vals.push_in((v.0, Some(v.1)));
        at
    };
    for (k, att) in atts.iter().enumerate() {
        cols.push_in(att.map(|out| lay(out, got[k], &mut cells, &mut vals)));
    }
    if !sys.is_empty() {
        crate::ops::modifytable::ret_cols::lay_sys(sys, false, &mut cols, &mut cells, &mut vals);
    }
    for of in [old, if new { got } else { &[] }] {
        for (k, att) in atts.iter().enumerate() {
            map.push_in(att.map(|out| lay(out, img(k, of), &mut cells, &mut vals)));
        }
    }
    let text = ExprOut { rep: Rep::of_storage(-1, false), typid: 25, life: ExprLife::Row };
    let addr = b.iconst(gravity_ir::Type::I64, action.as_ptr() as i64);
    map.push_in(Some(lay(text, (addr, f), &mut cells, &mut vals)));
    // whether OLD and NEW exist for this action (a ReturningExpr's test, ret_cols.rs `images`: EEO_FLAG_OLD_IS_NULL /
    // NEW_IS_NULL negated): an INSERT has no OLD, a DELETE no NEW
    let flag = ExprOut { rep: Rep::of_storage(1, true), typid: 16, life: ExprLife::Row };
    for exists in [action != merge_action_insert(), new] { // D2916: the text by value, never its address
        let v = b.iconst(gravity_ir::Type::I64, exists as i64);
        map.push_in(Some(lay(flag, (v, f), &mut cells, &mut vals)));
    }
    // D669: the source's columns the list reads, the child's row's, after the action's text
    if !outer.0.is_empty() {
        crate::ops::modifytable::ret_cols::lay_outer(node, row, outer, &mut map, &mut cells, &mut vals);
    }
    if write_node != node {
        instr::count_one(b, env.ctx, write_node, Ctr::Rows);
    }
    let out = RowVals { row: Row { cells }, vals, cols, carry: row.carry.copy_in(env.mcx), next: row.next, at: row.at };
    env.memo.clear();
    let inner = core::mem::replace(&mut env.inner, Some(map));
    let c = next(b, env, &out);
    env.inner = inner;
    env.memo.clear();
    c
}

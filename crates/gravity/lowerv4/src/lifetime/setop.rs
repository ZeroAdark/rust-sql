//! setop's lifetime decisions, which the family asks here: what each generation of a recursive union starts with.
//! PG's ExecRecursiveUnion, past the working table's last row, swaps the working and intermediate tables, empties the new
//! intermediate table (tuplestore_clear) and rescans the term with `wtParam` changed, so the term's WorkTableScan reads the
//! new working table from its first row (ExecReScanWorkTableScan). Lowered (ops/setop/recursive.rs `Scan::generation`),
//! the union's two tables are param words (a handle and a count each: rerun/spool/rec.rs `Rec`) and the driver's cursor a
//! frame word, all reset here at each generation: `next` emptied before the term re-runs into it (`next_emptied`), then
//! the two tables' words swapped, `next` empty again and the cursor back at the first row (`swapped`).
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Type, Value};

use crate::frame::env::Env;

fn word(b: &mut FunctionBuilder<'_>, env: &Env, w: u16) -> Value {
    b.load(Type::I64, env.params, w as i32 * 8)
}

fn set_word(b: &mut FunctionBuilder<'_>, env: &Env, w: u16, v: Value) {
    b.store(env.params, w as i32 * 8, v);
}

/// `next`'s count word `nn` emptied before the term re-runs into it (PG's intermediate table starts each generation
/// empty).
pub(crate) fn next_emptied(b: &mut FunctionBuilder<'_>, env: &Env, nn: u16) {
    set_word(b, env, nn, env.zero);
}

/// The two tables' words swapped (`work`'s handle and count words `(wh, wn)`, `next`'s `(nh, nn)`): `work` takes `next`'s
/// handle and count, `next` takes `work`'s handle with no row, and the driver's `cursor` frame word goes back to the first
/// row. The answer is the new generation's row count (0 ends the union).
pub(crate) fn swapped(b: &mut FunctionBuilder<'_>, env: &Env, (wh, wn): (u16, u16), (nh, nn): (u16, u16), cursor: i32) -> Value {
    let (w, n, c) = (word(b, env, wh), word(b, env, nh), word(b, env, nn));
    set_word(b, env, wh, n);
    set_word(b, env, wn, c);
    set_word(b, env, nh, w);
    set_word(b, env, nn, env.zero);
    env.store_word(b, cursor, env.zero);
    c
}

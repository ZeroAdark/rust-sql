//! Road `write.literals` (H16a; R12-L07; cards/M4.md L3 `write/literals.rs`; NEW-29): a VALUES list's Const cells are a
//! per-execution literal table the runner fills, never literals baked into the program (literal-free), so one program is
//! keyed by the statement's shape and serves any list of that shape. Owner M4-INSERT.
//!
//! The protocol is `ProtoKind::Literals` (exec v4/m4_proto.rs, M4-HOSTS): at each execution's prepare the runner copies
//! the ValuesScan's Const cells in row order into Exec-lived arrays and points three words at them: the cells (row-major
//! `u64` datums), the null bytes (one per cell) and the row count. The VALUES source (ops/values/lower.rs) asks this road
//! for the table (`LiteralsRoad`); with one, its window is `[resume, rows)` read from the words and its row `j` loads cell
//! `(j, c)`: no per-row code, whatever the list's length, in every statement (an INSERT's own list, a selecting or a
//! locking statement's, a writing statement's other lists).
//!
//! A cell that is not a Const (a Param the admission may not refresh, a function or a cast that could raise, a VOLATILE
//! call, PG's per row) is never a table cell: the runner evaluates nothing before the first row, so an error keeps its
//! row (PG raises the error of the first failing row; error-vs-error order is WRONG otherwise). Its row's first null byte
//! carries `LITERALS_CODED` and the source's code answers that row, its Const cells read from the table. The protocol's
//! arg carries the coded cells' `literals_mark`; exec's admission refuses a plan whose list marks other cells.
//!
//! CAP-29, the stream form: a list whose rows holding an expression pass `cost::VALUES_CODED_ROWS_MAX` declares one more
//! word, the execution's `LiteralsStream` hook (exec ops/modifytable/raw.rs), and the source calls it at each coded row's
//! turn (`stream`, through `raw::stream_row`): PostgreSQL's evaluator writes the row's other cells into the table, so the
//! row is read as a row of Consts, with no code of its own.
use gravity_decompose::Source;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_program::protos::{literals_mark, Proto, ProtoKind, LITERALS_CODED, LITERALS_STREAM_PARAMS};

use crate::face::end::{LitTable, LiteralsMech, LiteralsRoad};
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::ops::Table;

crate::strategies! {
    /// Bind-time cells + per-execution literal table.
    WRITE_LITERALS = 119, "write.literals", Insert;
}

pub(crate) const ROWS: Table = Table { strategies: &[(WRITE_LITERALS, &LiteralsRoad(&Literals))], ..crate::ops::NONE };

/// The table's words (exec v4/m4_proto.rs `LIT_CELLS` / `LIT_NULLS` / `LIT_ROWS`, `LIT_WORDS`).
const CELLS: u16 = 0;
const NULLS: u16 = 1;
const ROWS_WORD: u16 = 2;
const WORDS: u16 = 3;
/// CAP-29: the stream form's hook word (exec `LIT_STREAM`) and its word count (`LIT_STREAM_WORDS`).
const STREAM: u16 = 3;
const STREAM_WORDS: u16 = 4;

mod raw;

pub(crate) struct Literals;

impl LiteralsMech for Literals {
    fn table(&self, px: &mut Px<'_, '_>, k: usize, node: i32, cols: usize, stream: bool) -> Option<LitTable> {
        let gx = px.gx;
        let Source::Values { lists, .. } = &gx.pipelines()[k].source else { return None };
        if cols == 0 || cols > u16::MAX as usize {
            return None;
        }
        let coded = lists.iter().enumerate().flat_map(|(r, l)| {
            l.as_list().into_iter().flat_map(|l| l.iter().enumerate()).filter(|(_, n)| n.as_const().is_none()).map(move |(c, _)| (r, c))
        });
        // D4449: a list of Consts alone marks no coded cell, and its cells are not walked to learn it
        let mark = if crate::ops::shared::values::consts_only(gx.graph, lists) { literals_mark(core::iter::empty()) } else { literals_mark(coded) };
        let n = if stream { STREAM_WORDS } else { WORDS };
        let mut base = px.proto_words(n);
        if base == 0 {
            // word 0 stays "none" (keyed/table_size.rs)
            base = px.proto_words(n);
        }
        let arg = node as u32 as u64 | (cols as u64) << 32 | (mark as u64) << 48;
        px.declare_proto(Proto::new(ProtoKind::Literals, k as u16).words(base, n).arg(arg));
        Some(LitTable { base, cols: cols as u16, stream })
    }

    fn stream(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &LitTable, j: Value, params: &[(Value, Value)]) {
        let hook = word(b, env, t, STREAM);
        if !params.is_empty() {
            // D3374: the executor parameters' slots (protos `LiteralsStream::params`): parameter i's datum at 16 * i, its
            // null flag (1 NULL) at 16 * i + 8, read by the evaluation as es_param_exec_vals
            let hp = b.conv(ConvOp::IntToPtr, Type::Ptr, hook);
            let slots = b.load(Type::I64, hp, LITERALS_STREAM_PARAMS);
            let sp = b.conv(ConvOp::IntToPtr, Type::Ptr, slots);
            let one = b.iconst(Type::I64, 1);
            for (i, &(v, valid)) in params.iter().enumerate() {
                let null = b.select(valid, env.zero, one);
                b.store(sp, 16 * i as i32, v);
                b.store(sp, 16 * i as i32 + 8, null);
            }
        }
        // not a worker's host: PostgreSQL's evaluator runs on the execution's thread
        let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
        let h = b.declare_host_once("values::stream_row", &[Type::I64, Type::I64], Some(Type::I64), raw::stream_row as *const () as usize, flags);
        let r = b.call_host(h, &[hook, j], Some(Type::I64)).expect("returns its status");
        let failed = b.icmp(IntCond::Eq, r, env.zero);
        let l_ok = b.create_block(&[]);
        let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
        b.cond_br(failed, (env.l_raise, &[e]), (l_ok, &[]));
        b.switch_to(l_ok);
    }

    fn rows(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &LitTable) -> Value {
        word(b, env, t, ROWS_WORD)
    }

    fn coded(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &LitTable, j: Value) -> Value {
        let nulls = word(b, env, t, NULLS);
        let cols = b.iconst(Type::I64, t.cols as i64);
        let at = b.int_bin(IntBinOp::Mul, j, cols);
        let np = b.conv(ConvOp::IntToPtr, Type::Ptr, nulls);
        let bp = b.ptr_add(np, at, 1, 0);
        let n = b.load_zext(Type::I8, bp, 0);
        let bit = b.iconst(Type::I64, LITERALS_CODED as i64);
        let m = b.int_bin(IntBinOp::And, n, bit);
        b.icmp(IntCond::Ne, m, env.zero)
    }

    fn cell(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &LitTable, j: Value, c: usize) -> (Value, Value) {
        let (cells, nulls) = (word(b, env, t, CELLS), word(b, env, t, NULLS));
        let cols = b.iconst(Type::I64, t.cols as i64);
        let row = b.int_bin(IntBinOp::Mul, j, cols);
        let col = b.iconst(Type::I64, c as i64);
        let at = b.int_bin(IntBinOp::Add, row, col);
        let (cp, np) = (b.conv(ConvOp::IntToPtr, Type::Ptr, cells), b.conv(ConvOp::IntToPtr, Type::Ptr, nulls));
        let (vp, bp) = (b.ptr_add(cp, at, 8, 0), b.ptr_add(np, at, 1, 0));
        let v = b.load(Type::I64, vp, 0);
        let n = b.load_zext(Type::I8, bp, 0);
        // bit 0 is the null; a coded row's first byte also carries LITERALS_CODED
        let one = b.iconst(Type::I64, 1);
        let null = b.int_bin(IntBinOp::And, n, one);
        (v, b.icmp(IntCond::Eq, null, env.zero))
    }
}

/// Word `i` of the table's words.
fn word(b: &mut FunctionBuilder<'_>, env: &Env, t: &LitTable, i: u16) -> Value {
    b.load(Type::I64, env.params, 8 * (t.base + i) as i32)
}

#[cfg(test)]
mod tests {
    /// The words are the protocol's (exec ops/modifytable/proto.rs): cells, nulls, rows; CAP-29's stream hook after them.
    #[test]
    fn words_are_the_protocols() {
        assert_eq!((super::CELLS, super::NULLS, super::ROWS_WORD, super::WORDS), (0, 1, 2, 3));
        assert_eq!((super::STREAM, super::STREAM_WORDS), (3, 4));
    }
}

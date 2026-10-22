//! Road `percode.length` (H09, R2, G12): `octet_length` / `char_length` of a dictionary column read per code from the
//! dictionary index's length fields (the user's R4 ruling: no length tables). The declaration names the length the site's
//! source publishes over the code lane; the bind returns the index in place with its form (bytes, char counts, or deltas
//! between offsets), resolved once per window into the row's linear read (`len_table`). Also the reads both per-code
//! roads share (step/percode.rs finds and declares the roots, percode_bind.rs binds them): the row's code references
//! and bound tables, and the per-value mechanism (the 64-row word mask, a code's value). Budget 160 lines. Owner M1-STEP.
use gravity_abi::ctx::lane::KIND_CODES;
use gravity_expr::emit::percode::{len_table, CodeRef, TableRef};
use gravity_expr::emit::word::WordRefs;
use gravity_expr::emit::{ColsRef, LaneRef, RowRefs};
use gravity_expr::percode::Known;
use gravity_expr::{ExprNode, Source as VarSource};
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use gravity_kernels::host_abi::BIND_OUT_WORDS;
use gravity_program::{table_fn, TableDecl};
use mcx::{CollectIn, GrowIn, PgVec};

use crate::dict::percode::{Class, Plan};
use crate::cell::LaneId as CellLane;
use crate::dict::face::{PerValueImpl, PerValueMech};
use crate::frame::env::Env;
use crate::frame::frame::{Mask, WordWin};
use crate::frame::px::{ExId, Px};
use crate::frame::row::RowVals;
use crate::admit::refusal::Reason;

crate::strategies! {
    /// Lengths from the dictionary index.
    PERCODE_LENGTH = 22, "percode.length", CodeLength;
}

pub(crate) const ROWS: crate::ops::Table =
    crate::ops::Table { strategies: &[(PERCODE_LENGTH, &PerValueImpl(&ROAD))], ..crate::ops::NONE };

/// The site table of a length root over code lane `over`: the byte length or the character count.
pub(crate) fn decl(over: u8, known: &Known) -> TableDecl {
    let func = if matches!(known, Known::OctetLength) { table_fn::OCTET_LENGTH } else { table_fn::CHAR_LENGTH };
    TableDecl { over, func, flags: 0, collation: 0, encoding: 0, args: [(0, 0); 2] }
}

/// The bound length table at `data` in its form `form` (`BIND_FORM_*`) as the row reads it.
pub(crate) fn table(b: &mut FunctionBuilder<'_>, data: Value, form: Value) -> TableRef {
    len_table(b, data, form)
}

/// The row's code references and bound tables for plan `pl`: each code column's code read from its cell (the element's
/// address; its NULL from the cell), the window's part from `Env::codes`, each table from `Env::tables`
/// (a length table's read as that window's bind built it, percode_bind.rs).
pub(crate) fn refs<'e>(b: &mut FunctionBuilder<'_>, env: &Env<'e, '_>, node: i32, row: &RowVals, pl: &Plan) -> (PgVec<'e, CodeRef>, PgVec<'e, TableRef>) {
    let mut codes = mcx::reserved_in(pl.cols.len(), env.mcx);
    for &(attno, ordinal, width) in pl.cols.iter() {
        let (Some(cr), Some(w)) = (row.col(attno), env.codes.iter().find(|w| w.ordinal == ordinal)) else {
            crate::refuse!(node, Reason::Bug { payload: "a per-code column the row or the window does not carry" })
        };
        let lane = match cr.null {
            Some(null) => {
                // a cell's null is a Bool (a pgrc row) or an I64 word
                let ok = if b.value_type(null) == Type::Bool { let t = b.bconst(true); b.int_bin(IntBinOp::Xor, null, t) } else { b.icmp(IntCond::Eq, null, env.zero) };
                LaneRef { ptr: cr.val, valid: zero_word(b, env), valid_null: Some(ok), width, kind: KIND_CODES, all_valid: false }
            }
            None => LaneRef { ptr: cr.val, valid: cr.val, valid_null: None, width, kind: KIND_CODES, all_valid: true },
        };
        codes.push_in(CodeRef { attno, lane, code: None, ordinal, part: w.part });
    }
    let tp = match env.tables {
        Some(tp) => tp,
        None if pl.tables.is_empty() => env.zero,
        None => crate::refuse!(node, Reason::Bug { payload: "a per-code table read before the window's bind" }),
    };
    let tables = pl.tables.iter().map(|&(t, class)| pl.tabs.borrow().lens.iter().find(|x| class == Class::Len && x.0 == tp && x.1 == t).map_or_else(|| table_ref(b, tp, t, class, env.body.clean), |x| x.2)).collect_in(env.mcx);
    (codes, tables)
}

fn zero_word(b: &mut FunctionBuilder<'_>, env: &Env) -> Value {
    let s = b.declare_stack_slot(8, 8);
    let p = b.stack_addr(s);
    b.store(p, 0, env.zero);
    p
}

/// Table `t` of the bound array at `tp` read as `class`, with its fault table unless `clean` (a clean bind has none).
pub(crate) fn table_ref(b: &mut FunctionBuilder<'_>, tp: Value, t: u16, class: Class, clean: bool) -> TableRef {
    let at = (BIND_OUT_WORDS * t as usize * 8) as i32;
    let data = b.load(Type::Ptr, tp, at);
    let faults = (!clean && matches!(class, Class::Bits | Class::Words)).then(|| b.load(Type::Ptr, tp, at + 8));
    match class {
        Class::Bits => TableRef::Bits { words: data, faults },
        Class::Set => TableRef::Bits { words: data, faults: None },
        Class::Words => TableRef::Words { vals: data, faults },
        Class::Len => {
            let form = b.load(Type::I64, tp, at + 16);
            crate::dict::percode_len::table(b, data, form)
        }
        Class::Fp => TableRef::Fp128 { vals: data },
    }
}

/// The per-value mechanism of roads `percode.table` and `percode.length` (percode_len.rs registers it too).
pub(crate) struct Road;
pub(crate) static ROAD: Road = Road;

impl PerValueMech for Road {
    /// The rows of block `[base, base + 64)` within `m` that pass per-code qual `expr` (the guard `word_open` held).
    fn mask(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, w: &WordWin, base: Value, _lane: CellLane, expr: ExId, m: Mask) -> Mask {
        let pl = crate::step::expr::plan_of(env, expr);
        let Some((pl, codes)) = pl.and_then(|p| Some((p, window_codes(env, p)?))) else { crate::refuse!(env.node, Reason::Bug { payload: "a per-code mask outside its window" }) };
        let tables = word_tables(b, env, pl);
        let (cv, cn) = crate::frame::hoist::words(b, env, env.node, expr);
        let refs = WordRefs { lanes: &w.lanes, consts: &cv, consts_null: &cn, tables: &tables, scratch: env.mcx };
        // a sparse table read walks the block's set bits (at most 64), inside the window's checked block loop
        let n0 = b.func().hints.loop_headers.len();
        let Ok(word) = env.ex(expr).emit_word_codes(b, &refs, &codes, base) else { crate::refuse!(env.node, Reason::Bug { payload: "a per-code word form the probe accepted" }) };
        env.bounded.extend_from_slice_in(&b.func().hints.loop_headers[n0..]);
        let attnos: PgVec<'_, i16> = pl.cols.iter().map(|c| c.0).collect_in(env.mcx);
        let word = crate::frame::block::valid_and(b, env, w, &attnos, base, word);
        Mask(b.int_bin(IntBinOp::And, word, m.0))
    }

    /// Expression `expr`, one per-code root over one code column, for code word `code`: `(value, null)`.
    fn value(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _lane: CellLane, expr: ExId, code: Value) -> (Value, Value) {
        let Some(pl) = crate::step::expr::plan_of(env, expr).filter(|p| p.cols.len() == 1) else {
            crate::refuse!(env.node, Reason::Bug { payload: "a per-code value of an expression with no one code column" })
        };
        let (attno, ordinal, width) = pl.cols[0];
        let Some(w) = env.codes.iter().find(|w| w.ordinal == ordinal) else { crate::refuse!(env.node, Reason::Bug { payload: "a per-code value outside its window" }) };
        let m = b.iconst(Type::I64, u32::MAX as i64);
        let low = b.int_bin(IntBinOp::And, code, m);
        let lane = LaneRef { ptr: env.ctx, valid: env.ctx, valid_null: None, width, kind: KIND_CODES, all_valid: true };
        let codes = [CodeRef { attno, lane, code: Some(low), ordinal, part: w.part }];
        let tp = env.tables.unwrap_or(env.zero);
        let tables: PgVec<'_, TableRef> = pl.tables.iter().map(|&(t, class)| table_ref(b, tp, t, class, env.body.clean)).collect_in(env.mcx);
        let (cv, cn) = crate::frame::hoist::words(b, env, env.node, expr);
        let refs = RowRefs { codes: &codes, tables: &tables, ctx: Some(env.ctx), ..RowRefs::scan(ColsRef::Lanes { row: env.zero, lanes: &[] }, &cv, &cn, env.mcx) };
        let refs = env.hir.with_ex(b, env.placed(expr), refs);
        let Ok(l) = env.ex(expr).emit(b, &refs) else { crate::refuse!(env.node, Reason::Bug { payload: "a per-code value with no emit" }) };
        let raised = b.icmp(IntCond::Ne, l.err, env.zero);
        let l_ok = b.create_block(&[]);
        b.cond_br(raised, (env.l_raise, &[l.err]), (l_ok, &[]));
        b.switch_to(l_ok);
        let t = b.bconst(true);
        (l.value, b.int_bin(IntBinOp::Xor, l.valid, t))
    }
}

/// The window's code references for plan `pl`: `Env::codes` renumbered to the expression's columns.
fn window_codes<'g>(env: &Env<'g, '_>, pl: &Plan) -> Option<PgVec<'g, CodeRef>> {
    let mut out: PgVec<'g, CodeRef> = mcx::reserved_in(pl.cols.len(), env.mcx);
    for &(attno, ordinal, _) in pl.cols.iter() {
        out.push_in(CodeRef { attno, ..*env.codes.iter().find(|w| w.ordinal == ordinal)? });
    }
    Some(out)
}

/// The word form's tables, read with no fault test (the block runs only under a clean bind).
fn word_tables<'g>(b: &mut FunctionBuilder<'_>, env: &Env<'g, '_>, pl: &Plan) -> PgVec<'g, TableRef> {
    let tp = env.tables.unwrap_or(env.zero);
    pl.tables.iter().map(|&(t, class)| table_ref(b, tp, t, class, true)).collect_in(env.mcx)
}

/// The block guard of per-code qual `q` over window `w`: its word form holds (its constants fit), the site's tables
/// bound clean (`clean`), and the window's rows are its lanes' rows. `None`: no word form in this window.
pub(crate) fn word_open(b: &mut FunctionBuilder<'_>, env: &mut Env, w: &WordWin, q: ExId, clean: Value) -> Option<Mask> {
    let pl = crate::step::expr::plan_of(env, q)?;
    let codes = window_codes(env, pl)?;
    let tables = word_tables(b, env, pl);
    let (cv, cn) = crate::frame::hoist::words(b, env, env.node, q);
    let ok = env.ex(q).emit_word_ok_codes(b, &WordRefs { lanes: &w.lanes, consts: &cv, consts_null: &cn, tables: &tables, scratch: env.mcx }, &codes).ok()?;
    let fits = b.icmp(IntCond::Ne, ok, env.zero);
    let plain = if b.value_type(w.plain) == Type::Bool { w.plain } else { b.icmp(IntCond::Ne, w.plain, env.zero) };
    let g = b.int_bin(IntBinOp::And, fits, plain);
    Some(Mask(b.int_bin(IntBinOp::And, g, clean)))
}

/// Whether per-code qual `q` has the 64-row word form: every column read by code, probed in a scratch function when
/// the filter is made.
pub(crate) fn has_word(px: &Px<'_, '_>, q: ExId) -> bool {
    let e = px.placed(q);
    let Some(pl) = e.ext.as_deref().and_then(|x| x.downcast_ref::<Plan>()) else { return false };
    if e.c.prog.nodes.iter().any(|n| matches!(n, ExprNode::Var(v) if v.src != VarSource::Scan)) {
        return false;
    }
    let mcx = px.mcx();
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("percode_word_probe", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let p = b.params()[0];
    let (zero, fl) = (b.iconst(Type::I64, 0), b.bconst(false));
    let mut lanes = mcx::pg_vec![in mcx; None; pl.cols.iter().map(|c| c.0 as usize).max().unwrap_or(0)];
    let mut codes: PgVec<'_, CodeRef> = PgVec::new_in(mcx);
    for &(attno, ordinal, width) in pl.cols.iter() {
        let lane = LaneRef { ptr: p, valid: p, valid_null: None, width, kind: KIND_CODES, all_valid: true };
        lanes[attno as usize - 1] = Some(lane);
        codes.push_in(CodeRef { attno, lane, code: None, ordinal, part: Some(zero) });
    }
    let tables: PgVec<'_, TableRef> = pl.tables.iter().map(|&(_, c)| match c {
        Class::Bits | Class::Set => TableRef::Bits { words: p, faults: None },
        Class::Len => TableRef::Len { base: p, stride: zero, sa: zero, ma: zero, mb: zero },
        _ => TableRef::Words { vals: p, faults: None },
    }).collect_in(mcx);
    let (cv, cn) = (mcx::pg_vec![in mcx; zero; e.places.len()], mcx::pg_vec![in mcx; fl; e.places.len()]);
    let wr = WordRefs { lanes: &lanes, consts: &cv, consts_null: &cn, tables: &tables, scratch: mcx };
    let ok = e.c.emit_word_ok_codes(&mut b, &wr, &codes).is_ok() && e.c.emit_word_codes(&mut b, &wr, &codes, zero).is_ok();
    b.ret(&[zero]);
    ok
}

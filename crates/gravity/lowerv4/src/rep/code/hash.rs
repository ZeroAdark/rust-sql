//! Code hash (`rep.code.hash`, plan §3.2; hash-contract.md §3.2 rule A, RULED 09-23): a code hashes AS RENDERED.
//! - Program scope (every table that may meet another dictionary, a heap row, a merge, a spill, a join side): the image is
//!   the entry's Fp128, two words read from the dictionary's Fp128 table (built once when the dictionary loads, kept per
//!   part: Michael 09-13) at `code × 16`: one 16-byte load per row, no hash, no render. A code and heap text of the same
//!   bytes therefore take the same home and route, and equal Fp128s are equal keys (full-strength SipHash-2-4-128 over the
//!   canonical bytes, `exact`).
//! - Same-dictionary scope (`Scope::SameDict`, an operator inside one part of one site: a run collapse, a per-part front):
//!   the code itself, exact, because a part's dictionary holds each value once. Codes are never translated between
//!   dictionaries.
//! Equality is PG's for a deterministic collation (bytewise); a non-deterministic one never reaches this arm (the census
//! routes it to `key.coll`). The window's table words come from the source (`Frame::tables`, 24 bytes per table).
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_program::{DictRef, Rep, Until};

use crate::cell::{At, Cell, CellHome, CellLife};
use crate::face::kinds::KindId;
use crate::admit::refusal::Reason;
use crate::ops::Table;
use crate::rep::{CellRef, HashArm, Lx, Scope, StoredRef, Words};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::RepCodeHash, &ARM)], ..crate::ops::NONE };

pub(crate) const ARM: HashArm = HashArm { image, eq };

/// A bound table's bytes in the window's table words (the kernels' `agg_bind` layout: data, fault words, length form).
pub(crate) const TABLE_WORDS_BYTES: i32 = 3 * core::mem::size_of::<u64>() as i32;
/// An Fp128 entry: `[lo, hi]`.
pub(crate) const FP_BYTES: u8 = core::mem::size_of::<[u64; 2]>() as u8;

fn same_dict(c: &Cell, d: DictRef) -> bool {
    matches!(c.rep, Rep::Code { dict } if dict == d) && !c.image_only()
}

fn image(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, scope: Scope) -> Words {
    let fp = linked(cx, c).filter(|_| !matches!(scope, Scope::SameDict(d) if same_dict(c.cell, d)));
    if let Some([lo, hi]) = fp {
        fires::fire("key.payload_fp"); // shapecheck fire signal (join.payload_fp): a record code keyed by its record's Fp128 words
        return Words { w: [Some(lo), Some(hi)], exact: true };
    }
    match scope {
        Scope::SameDict(d) if same_dict(c.cell, d) => Words { w: [Some(crate::rep::code::datum::code(cx, c)), None], exact: true },
        _ if matches!(c.cell.rep, Rep::Code { dict } if dict.fp_table == DictRef::NO_TABLE) => rendered(cx, c, tf),
        _ => {
            let [lo, hi] = table_fp(cx, c);
            Words { w: [Some(lo), Some(hi)], exact: true }
        }
    }
}

/// A code whose lane declares no Fp128 table (a join key: no keyed end declared one) keys by its render: rep.varlena.hash's
/// image of the rendered bytes (bpchar trimmed), the Fp128 its entry would hold (M6 Decision 3a); `NotBuilt` until M6.
fn rendered(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts) -> Words {
    let (p, null) = crate::rep::datum(cx, c, tf);
    let cell = Cell { rep: Rep::VarlenaPtr, life: CellLife { until: Until::Row, home: CellHome::RowCx }, at: At::Value(0), ..*c.cell };
    let f = crate::rep::arm::<HashArm>(cx, KindId::RepVarlenaHash).image;
    f(cx, CellRef { cell: &cell, val: p, null: Some(null) }, tf, Scope::Program)
}

/// Equality against a stored image: a same-dictionary code as one word, else the two Fp128 words.
fn eq(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, stored: StoredRef, _tf: &TypeFacts) -> Value {
    let p = ptr(cx.b, stored.ptr);
    if let Rep::Code { dict } = stored.rep {
        if same_dict(c.cell, dict) {
            let code = crate::rep::code::datum::code(cx, c);
            let s = cx.b.load(Type::I64, p, stored.off);
            return cx.b.icmp(IntCond::Eq, code, s);
        }
    }
    let [lo, hi] = linked(cx, c).unwrap_or_else(|| table_fp(cx, c));
    let (slo, shi) = (cx.b.load(Type::I64, p, stored.off), cx.b.load(Type::I64, p, stored.off + 8));
    let (a, b) = (cx.b.int_bin(IntBinOp::Xor, lo, slo), cx.b.int_bin(IntBinOp::Xor, hi, shi));
    let d = cx.b.int_bin(IntBinOp::Or, a, b);
    let zero = cx.b.iconst(Type::I64, 0);
    cx.b.icmp(IntCond::Eq, d, zero)
}

/// join.payload_fp (ops/hashjoin/strategies/payload_fp.rs): the Fp128 words a hash probe's record carries for this record
/// code word (`Lx::fps`): its build's image of the code (`FpTable[code]`, `[0, 0]` for a NULL), the table's entry, so the
/// same words its lane's table or its render gives. `None` for a lane cell and an unlinked word.
fn linked(cx: &Lx<'_, '_, '_>, c: CellRef<'_>) -> Option<[Value; 2]> {
    if matches!(c.cell.at, At::Lane(_)) || c.cell.image_only() {
        return None;
    }
    cx.fps.get(c.val)
}

fn ptr(b: &mut FunctionBuilder<'_>, v: Value) -> Value {
    if b.value_type(v) == Type::Ptr { v } else { b.conv(ConvOp::IntToPtr, Type::Ptr, v) }
}

/// The address of `code`'s Fp128 entry in table `t` of the window's table words (prefetch.fp prefetches rows ahead by it).
pub(crate) fn fp_entry(b: &mut FunctionBuilder<'_>, tables: Value, t: u8, code: Value) -> Value {
    let tables = ptr(b, tables);
    let data = b.load(Type::Ptr, tables, t as i32 * TABLE_WORDS_BYTES);
    b.ptr_add(data, code, FP_BYTES, 0)
}

/// The lane cell's Fp128 `[lo, hi]`, loaded from its dictionary's table (dict table.rs builds it; none is made here): `[0, 0]`
/// for a NULL row (its code is never read into the table; the image's flags word marks it). Past its window: a Bug.
fn table_fp(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>) -> [Value; 2] {
    let t = match c.cell.rep {
        Rep::Code { dict } if dict.fp_table != DictRef::NO_TABLE => dict.fp_table,
        _ => crate::refuse!(cx, Reason::Bug { payload: "a code key whose lane declares no Fp128 table" }),
    };
    let Some(tables) = cx.at.and_then(|f| f.tables) else {
        crate::refuse!(cx, Reason::Bug { payload: "a code key image with no table words bound" })
    };
    if !matches!(c.cell.at, At::Lane(_)) {
        crate::refuse!(cx, Reason::Bug { payload: "a code key image past its window (keep the image, not the code)" });
    }
    let zero = cx.b.iconst(Type::I64, 0);
    let (l_load, l_out) = (cx.b.create_block(&[]), cx.b.create_block(&[Type::I64, Type::I64]));
    match c.null {
        Some(n) => cx.b.cond_br(n, (l_out, &[zero, zero]), (l_load, &[])),
        None => cx.b.br(l_load, &[]),
    }
    cx.b.switch_to(l_load);
    let code = crate::rep::code::datum::code(cx, c);
    let e = fp_entry(cx.b, tables, t, code);
    let (lo, hi) = (cx.b.load(Type::I64, e, 0), cx.b.load(Type::I64, e, 8));
    cx.b.br(l_out, &[lo, hi]);
    cx.b.switch_to(l_out);
    let o = cx.b.block_params(l_out);
    [o[0], o[1]]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{CellHome, CellLife, LaneId, Nullable};
    use crate::rep::{Frame, RepHosts};
    use gravity_decompose::catalog::typefacts::row_of;
    use gravity_interp::Interp;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_program::{LaneLife, NumTypmod};

    const DICT: DictRef = DictRef { lane: 1, fp_table: 2 };

    /// The image of code `code` under `scope` with table 2's data at `fps`: its words.
    fn img(scope: Scope, code: u32, fps: &[[u64; 2]]) -> Vec<u64> {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let c = Cell { rep: Rep::Code { dict: DICT }, typid: 25, num: NumTypmod::NONE, life: CellLife::of_lane(LaneLife::DICTIONARY), at: At::Lane(LaneId::col(1)), null: Nullable::Maybe };
        let mut pb = ProgramBuilder::new();
        let render = pb.declare_host("agg_render", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::Ptr), 0, Default::default());
        let f = pb.declare_func("h", &[Type::Ptr, Type::Ptr, Type::Ptr], &[Type::I64, Type::I64]);
        let mut b = pb.define(f);
        let (ctx, elem, tables) = (b.params()[0], b.params()[1], b.params()[2]);
        let w = {
            let mut cx = Lx::new(&mut b, 1, rig.mcx());
            cx.at = Some(Frame { ctx, site: Some(0), hosts: RepHosts { render }, tables: Some(tables) });
            let w = image(&mut cx, CellRef { cell: &c, val: elem, null: None }, row_of(25).unwrap(), scope);
            assert!(w.exact);
            w.w
        };
        let zero = b.iconst(Type::I64, 0);
        b.ret(&[w[0].unwrap(), w[1].unwrap_or(zero)]);
        let prog = pb.finish();
        let mut words = [0u64; 9];
        words[2 * 3] = fps.as_ptr() as u64;
        let cw = [code];
        Interp::new(&prog).run(f, &[0, cw.as_ptr() as u64, words.as_ptr() as u64]).expect("runs")
    }

    #[test]
    fn program_scope_is_the_entry_fp128_and_same_dict_the_code() {
        let fps = [[1, 2], [0xAAAA, 0xBBBB], [5, 6]];
        assert_eq!(img(Scope::Program, 1, &fps), vec![0xAAAA, 0xBBBB]);
        assert_eq!(img(Scope::SameDict(DICT), 2, &fps), vec![2, 0]);
        assert_eq!(img(Scope::SameDict(DictRef { lane: 1, fp_table: 3 }), 2, &fps), vec![5, 6], "another dictionary keys by Fp128");
    }

    #[test]
    fn a_record_code_has_no_program_image() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let c = Cell { rep: Rep::Code { dict: DICT }, typid: 25, num: NumTypmod::NONE, life: CellLife { until: gravity_program::Until::Life, home: CellHome::FrameSlot }, at: At::Slot(0), null: Nullable::Maybe };
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("h", &[Type::Ptr], &[]);
        let mut b = pb.define(f);
        let p = b.params()[0];
        let mut cx = Lx::new(&mut b, 4, rig.mcx());
        let r = crate::catch(4, || image(&mut cx, CellRef { cell: &c, val: p, null: None }, row_of(25).unwrap(), Scope::Program)).expect_err("refused");
        assert!(matches!(r.reason, Reason::Bug { .. }));
    }
}

//! Code datum (`rep.code.datum`, plan §3.2 / v3 design §3.3; KEEP lowerv3 rep/code.rs, v2 text/render.rs `render_datum`). A
//! code stands only for its render: its datum is the dictionary's resident varlena image of `part << 32 | code`, no copy.
//! - `datum`: a lane cell's code (u32 at its element) with the window's part (the site's `part_id`), rendered through the
//!   site's dictionary (`agg_render`); a record or kept cell already holds `part << 32 | code`, rendered through the site of
//!   the pipeline that produced it (`Lx::site_of`). A NULL row renders nothing; a code the dictionary has no image for
//!   renders NULL; a failed dictionary read raises its stashed error at once, never a NULL (D344). The render is
//!   memoised per (row, cell) (`rep::datum`), so a cell read twice renders once; a pure function of one code lane is
//!   evaluated per dictionary entry by the per-code roads (face/codes.rs), never here per row.
//! - `retain`: keeps the word `part << 32 | code`: the dictionaries stay resident for the statement, so nothing is copied.
//! - `block` (H01): a code lane's elements are u32 codes, the block form the per-code mask roads read.
use gravity_abi::ctx::site_off;
use gravity_abi::site::site;
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_program::{Home, Rep, Until};

use crate::cell::{At, Cell, CellHome, CellLife};
use crate::face::kinds::KindId;
use crate::admit::refusal::Reason;
use crate::ops::Table;
use crate::rep::{self, BlockLane, BlockRef, CellRef, DatumArm, Kept, Lx, RetainHome};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::RepCodeDatum, &ARM)], ..crate::ops::NONE };

pub(crate) const ARM: DatumArm = DatumArm { datum, retain, block: Some(block) };

/// A code lane's element width (decompose `lane_of`: `KIND_CODES`, 4 bytes).
pub(crate) const CODE_BYTES: u8 = core::mem::size_of::<u32>() as u8;

/// A kept code's life: the word itself, its image resident with the dictionary for the statement.
const KEPT_LIFE: CellLife = CellLife { until: Until::Statement, home: CellHome::Lane(Home::Dictionary) };

/// An image-only key cell (`CellHome::ImageOnly`) has no render, rank or kept word: reading one is a lowering Bug.
pub(crate) fn not_image_only(cx: &Lx<'_, '_, '_>, c: &Cell, form: &'static str) {
    if c.image_only() {
        crate::refuse!(cx, Reason::Bug { payload: form });
    }
}

/// The site whose dictionary a code cell renders through, or a Bug.
pub(crate) fn site(cx: &Lx<'_, '_, '_>, c: &Cell) -> usize {
    match cx.site_of(c) {
        Some(k) => k,
        None => crate::refuse!(cx, Reason::Bug { payload: "a Code cell with no site to render through" }),
    }
}

/// The dictionary lane of a code cell.
pub(crate) fn lane(cx: &Lx<'_, '_, '_>, c: &Cell) -> u8 {
    match c.rep {
        Rep::Code { dict } => dict.lane,
        _ => crate::refuse!(cx, Reason::Bug { payload: "a code arm over a cell that is not a Code" }),
    }
}

/// A lane cell's code (`I64`, zero-extended u32).
pub(crate) fn code(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>) -> Value {
    crate::rep::code_word(cx, c)
}

/// The cell's word `part << 32 | code`: a lane cell's code with the window's part; any other cell's `val`.
pub(crate) fn word(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>) -> Value {
    not_image_only(cx, c.cell, "an image-only key cell read as a word");
    if !matches!(c.cell.at, At::Lane(_)) {
        // M7-SPILL B4b-1 (V4R-404, main 09-27: the B4b split, PlainAgg + Sort first): only a code's record / kept word passes as a word; a datum never reads as a code
        if !matches!(c.cell.rep, Rep::Code { .. }) {
            crate::refuse!(cx, Reason::Bug { payload: "a datum cell read as a code word" });
        }
        return c.val;
    }
    let k = site(cx, c.cell);
    let ctx = cx.frame().ctx;
    let code = code(cx, c);
    let pid = cx.b.load_zext(Type::I32, ctx, site_off(k) as i32 + site::PART_ID as i32);
    let s32 = cx.b.iconst(Type::I64, 32);
    let part = cx.b.int_bin(IntBinOp::Shl, pid, s32);
    cx.b.int_bin(IntBinOp::Or, code, part)
}

fn datum(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, _tf: &TypeFacts) -> (Value, Value) {
    not_image_only(cx, c.cell, "an image-only key cell read as a datum");
    // a NULL a fill made (a merge join's NULL-extended side, mergejoin strategies/markable.rs null_cells) renders nothing,
    // so it asks for no site: its frame slot home names none in a pipeline without one (D4445)
    if let Some(n) = c.null.filter(|&n| cx.b.const_of(n).is_some_and(|(_, v)| v != 0)) {
        return (cx.b.iconst(Type::I64, 0), n);
    }
    if matches!(c.cell.life.home, CellHome::Record(_)) {
        fires::fire("code.render"); // shapecheck fire signal (RD-code.render): a record's code rendered at the emit, not per row
    }
    let (k, lane) = (site(cx, c.cell), lane(cx, c.cell));
    let f = cx.frame();
    let null = match c.null {
        Some(n) => n,
        None => cx.b.bconst(false),
    };
    let zero = cx.b.iconst(Type::I64, 0);
    let l_render = cx.b.create_block(&[]);
    let l_out = cx.b.create_block(&[Type::I64]);
    cx.b.cond_br(null, (l_out, &[zero]), (l_render, &[]));
    cx.b.switch_to(l_render);
    let w = word(cx, c);
    let (kv, lv) = (cx.b.iconst(Type::I64, k as i64), cx.b.iconst(Type::I64, lane as i64));
    let p = cx.b.call_host(f.hosts.render, &[f.ctx, kv, lv, w], Some(Type::Ptr)).expect("render returns a pointer");
    let pi = cx.b.conv(ConvOp::PtrToInt, Type::I64, p);
    raise_failed(cx, pi, zero);
    cx.b.br(l_out, &[pi]);
    cx.b.switch_to(l_out);
    let pi = cx.b.block_params(l_out)[0];
    let none = cx.b.icmp(IntCond::Eq, pi, zero);
    (pi, none)
}

/// A render's result `pi` below zero (the failed dictionary read's status, its error stashed in the node) leaves through
/// the function's raise block with the HOST bit (D344); the builder continues in a new block.
fn raise_failed(cx: &mut Lx<'_, '_, '_>, pi: Value, zero: Value) {
    let Some(l_raise) = cx.raise else { crate::refuse!(cx, Reason::Bug { payload: "a code render outside a function with a raise block" }) };
    let failed = cx.b.icmp(IntCond::Slt, pi, zero);
    let e = cx.b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    let l_ok = cx.b.create_block(&[]);
    cx.b.cond_br(failed, (l_raise, &[e]), (l_ok, &[]));
    cx.b.switch_to(l_ok);
}

fn retain(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, _tf: &TypeFacts, _until: Until, _home: RetainHome) -> Kept {
    // before the Kept cell: its KEPT_LIFE would drop the marker
    not_image_only(cx, c.cell, "an image-only key cell read as a kept word");
    let w = word(cx, c);
    Kept { cell: Cell { life: KEPT_LIFE, at: crate::rep::word::datum::off_lane(c.cell.at), ..*c.cell }, val: w, null: c.null }
}

fn block(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, _tf: &TypeFacts, at: BlockRef) -> BlockLane {
    not_image_only(cx, c.cell, "an image-only key cell read as a block");
    if !matches!(c.cell.at, At::Lane(_)) {
        crate::refuse!(cx, Reason::Bug { payload: "the block form of a code cell that is not a lane" });
    }
    BlockLane { base: at.elem, bytes: CODE_BYTES }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use crate::cell::{LaneId, Nullable};
    use gravity_decompose::catalog::typefacts::row_of;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_program::{DictRef, NumTypmod};

    /// A kept lane code is the word `part << 32 | code`, read from the window's part, and lives past its window.
    #[test]
    fn a_kept_code_is_its_part_qualified_word() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let c = Cell { rep: Rep::Code { dict: DictRef::lane(2) }, typid: 25, num: NumTypmod::NONE, life: CellLife::of_lane(gravity_program::LaneLife::DICTIONARY), at: At::Lane(LaneId::col(2)), null: Nullable::Maybe };
        let mut pb = ProgramBuilder::new();
        let render = pb.declare_host("agg_render", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::Ptr), 0, Default::default());
        let f = pb.declare_func("k", &[Type::Ptr, Type::Ptr], &[Type::I64]);
        let mut b = pb.define(f);
        let (ctx, elem) = (b.params()[0], b.params()[1]);
        let v = {
            let mut cx = Lx::new(&mut b, 1, rig.mcx());
            cx.at = Some(rep::Frame { ctx, site: Some(0), hosts: rep::RepHosts { render }, tables: None });
            let k = rep::retain(&mut cx, CellRef { cell: &c, val: elem, null: None }, row_of(25).unwrap(), Until::Life, RetainHome::Frame);
            assert_eq!((k.cell.life, k.cell.at), (KEPT_LIFE, At::Value(2)));
            k.val
        };
        b.ret(&[v]);
        let prog = pb.finish();
        let mut ctxw = vec![0u64; site_off(1) / 8 + 1];
        // SAFETY: the words are the ctx image; PART_ID is a u32 field inside site 0.
        unsafe { (ctxw.as_mut_ptr().cast::<u8>().add(site_off(0) + site::PART_ID) as *mut u32).write_unaligned(7) };
        let code = [41u32];
        let got = gravity_interp::Interp::new(&prog).run(f, &[ctxw.as_ptr() as u64, code.as_ptr() as u64]).expect("runs")[0];
        assert_eq!(got, 7 << 32 | 41);
    }

    /// One accessor over `c` in a bare function with a frame (site 0, table words bound), under the one catch.
    fn run<T>(c: &Cell, f: impl FnOnce(&mut Lx<'_, '_, '_>, CellRef<'_>) -> T) -> Result<T, crate::Refusal> {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        crate::catch(9, || {
            let mut pb = ProgramBuilder::new();
            let render = pb.declare_host("agg_render", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::Ptr), 0, Default::default());
            let fid = pb.declare_func("k", &[Type::Ptr, Type::Ptr, Type::Ptr], &[Type::I64]);
            let mut b = pb.define(fid);
            let (ctx, elem, tables) = (b.params()[0], b.params()[1], b.params()[2]);
            let mut cx = Lx::new(&mut b, 9, rig.mcx());
            cx.at = Some(rep::Frame { ctx, site: Some(0), hosts: rep::RepHosts { render }, tables: Some(tables) });
            f(&mut cx, CellRef { cell: c, val: elem, null: None })
        })
    }

    /// B4b-1: a datum cell off a lane (a spill pair's kept text, a frame slot) never reads as a code word; a code's record
    /// word does.
    #[test]
    fn a_datum_cell_off_a_lane_refuses_to_read_as_a_code_word() {
        let dict = DictRef { lane: 2, fp_table: 1 };
        let life = CellLife { until: Until::Part, home: CellHome::FrameSlot };
        let datum = Cell { rep: Rep::VarlenaPtr, typid: 25, num: NumTypmod::NONE, life, at: At::Slot(0), null: Nullable::Maybe };
        match run(&datum, |cx, r| drop(super::word(cx, r))) {
            Err(crate::Refusal { reason: Reason::Bug { payload }, node: 9, .. }) => assert_eq!(payload, "a datum cell read as a code word"),
            other => panic!("not a Bug at node 9: {other:?}"),
        }
        let code = Cell { rep: Rep::Code { dict }, ..datum };
        assert!(run(&code, |cx, r| drop(super::word(cx, r))).is_ok(), "a code's record word reads as its word");
    }

    /// An image-only key cell (percode_key's derived key: its code indexes an Fp128 table, not a render) refuses every
    /// form but its hash image, by name at its node, before a kept cell could drop the marker; under a same-dictionary
    /// scope its image is the Fp128 pair, never the code; a FrameSlot code cell of the same dictionary keeps the code.
    #[test]
    fn an_image_only_key_cell_refuses_every_form_but_its_hash_image() {
        let dict = DictRef { lane: 2, fp_table: 1 };
        let c = Cell { rep: Rep::Code { dict }, typid: 25, num: NumTypmod::NONE, life: CellLife { until: Until::Row, home: CellHome::ImageOnly }, at: At::Lane(LaneId::col(2)), null: Nullable::Maybe };
        let plain = Cell { life: CellLife { until: Until::Row, home: CellHome::FrameSlot }, ..c };
        assert!(c.image_only() && !plain.image_only());
        let tf = row_of(25).unwrap();
        let coll = gravity_decompose::catalog::typefacts::KeyColl { oid: 950, bytes_eq: true, bytes_order: true };
        let bug = |r: Result<(), crate::Refusal>| match r {
            Err(crate::Refusal { reason: Reason::Bug { payload }, node: 9, .. }) => payload,
            other => panic!("not a Bug at node 9: {other:?}"),
        };
        assert_eq!(bug(run(&c, |cx, r| drop(rep::datum(cx, r, tf)))), "an image-only key cell read as a datum");
        assert_eq!(bug(run(&c, |cx, r| drop(rep::retain(cx, r, tf, Until::Life, RetainHome::Frame)))), "an image-only key cell read as a kept word");
        assert_eq!(bug(run(&c, |cx, r| drop(rep::block(cx, r, tf, BlockRef { elem: r.val, rows: r.val })))), "an image-only key cell read as a block");
        assert_eq!(bug(run(&c, |cx, r| drop(super::word(cx, r)))), "an image-only key cell read as a word");
        assert_eq!(bug(run(&c, |cx, r| drop(crate::rep::code::order::rank(cx, r, coll)))), "an image-only key cell read as a rank");
        assert_eq!(bug(run(&c, |cx, r| drop(rep::order_words(cx, r, tf, coll)))), "an image-only key cell read as an order image");
        let words = |c: &Cell| run(c, |cx, r| rep::hash_image(cx, r, tf, rep::Scope::SameDict(dict)).w.map(|w| w.is_some())).expect("a hash image");
        assert_eq!(words(&c), [true, true], "the Fp128 pair");
        assert_eq!(words(&plain), [true, false], "the same-dictionary code");
    }
}

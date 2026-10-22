//! D4054: the scan forms a spill pair pass `P'` keeps when its probe pipeline `q` ends in a hash build (`Kept`'s
//! `scan_forms`, pair.rs). `P'` then finishes that build and `q`'s own end continues it (build.rs `run_of`: a pair run),
//! so a kept cell must come back as the cell `q`'s arm hands the build. A code keeps the word `retain` makes of it
//! (`part << 32 | code`), a packed numeric its mantissa word: one 8-byte word of the kept row each, a by-value cell
//! (kind 0), so the spill file's format does not move. Every other cell keeps its datum, as any pair pass does.
//! - A kept code's dictionary is held as a build record's is: the runner keeps a hash build's dictionary images in the
//!   relation's cells for the statement (exec runner/drive.rs `keeps_dict_images`), and a site `P'` also drives is not
//!   read once (`read_once`), so its parts stay in use to the end. No part pin is taken here.
//! - The home rule: a code read from a record keeps that record's home, so it renders through the site of the pipeline
//!   that made the record; any other code renders through `P'`'s frame site, which is `q`'s (`KeptSource::nominal_site`).
//! - D3324 R2: under any end, a build record's code with no fingerprint table (`record_code`: road key.dependent's
//!   forwarded column, step/project.rs) keeps its word too, so `P'` hands the group the cell `q` hands it (one record
//!   word form per column; the build's dictionary is held for the statement, as above).
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_ir::Value;
use gravity_program::{Rep, Until};

use crate::cell::{At, Cell, CellHome, CellLife};
use crate::rep::{CellRef, Lx, RetainHome};

/// Kept row cell `i`'s scan form: the word `retain` makes of `c` and the cell `P'` reloads it as (`Rep::Code`, or
/// `Rep::PackedNum` in its `Word` form; in kept slot `i`, for the statement), or `None` for a cell that keeps its datum
/// (a `PackedNumDyn` lane among them: its word has no scale of its own).
pub(crate) fn scan_form(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, i: u16) -> Option<(Value, Cell)> {
    if !matches!(c.cell.rep, Rep::Code { .. } | Rep::PackedNum { .. }) {
        return None;
    }
    let k = crate::rep::retain(cx, c, tf, Until::Statement, RetainHome::Frame);
    let home = match c.cell.life.home {
        CellHome::Record(l) => CellHome::Record(l),
        _ => k.cell.life.home,
    };
    Some((k.val, Cell { life: CellLife { until: Until::Statement, home }, at: At::Slot(i), ..k.cell }))
}

/// Whether `c` is a build record's code with no fingerprint table (the module's D3324 R2 rule): any pass keeps its word.
pub(crate) fn record_code(c: &Cell) -> bool {
    matches!(c.rep, Rep::Code { dict } if dict.fp_table == gravity_program::DictRef::NO_TABLE) && matches!(c.life.home, CellHome::Record(_))
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use crate::cell::{LaneId, Nullable};
    use crate::rep::{self, Frame, RepHosts};
    use gravity_abi::ctx::site_off;
    use gravity_abi::site::site;
    use gravity_decompose::catalog::typefacts::row_of;
    use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
    use gravity_ir::Type;
    use gravity_program::{DictRef, Home, LaneLife, LifeId, NumForm, NumTypmod};

    const DICT: DictRef = DictRef { lane: 2, fp_table: DictRef::NO_TABLE };
    const NUM: NumTypmod = NumTypmod::of(10, 2);

    /// A text lane of codes and a `numeric(10, 2)` lane of packed mantissas, both of site 0.
    fn lanes() -> [Cell; 2] {
        let code = Cell { rep: Rep::Code { dict: DICT }, typid: 25, num: NumTypmod::NONE, life: CellLife::of_lane(LaneLife::DICTIONARY), at: At::Lane(LaneId::col(2)), null: Nullable::Maybe };
        let num = Cell { rep: Rep::PackedNum { facts: NUM, form: NumForm::Window }, typid: 1700, num: NUM, life: CellLife::of_lane(LaneLife::PGRC), at: At::Lane(LaneId::col(3)), null: Nullable::Maybe };
        [code, num]
    }

    /// `f` over an `Lx` of function `b` with a frame of site 0 (ctx `ctx`); a packed window is read as packed (A1's test,
    /// made once at the window's start); record life 3's producer is site 5.
    fn lx<'a, 'f>(b: &'a mut FunctionBuilder<'f>, ctx: Value, render: gravity_ir::HostFn, mcx: mcx::Mcx<'a>) -> Lx<'a, 'f, 'a> {
        let packed = b.bconst(false);
        let mut cx = Lx::new(b, 9, mcx);
        cx.at = Some(Frame { ctx, site: Some(0), hosts: RepHosts { render }, tables: None });
        cx.num_kinds = Some(std::rc::Rc::from([(0usize, 3u16, packed)]));
        cx.pipe_sites = Some(std::rc::Rc::from([None, None, None, Some(5)]));
        cx
    }

    /// The frame's render host, at its own address (no test here renders; lint host_decls pairs the declaration).
    fn render(pb: &mut ProgramBuilder) -> gravity_ir::HostFn {
        pb.declare_host("agg_render", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::Ptr), gravity_kernels::host_abi::agg_render as *const () as usize, Default::default())
    }

    /// A kept row of a code and a packed numeric holds their words, kind 0 (`kept_put`'s row, as `exit` stages it), and
    /// `P'` reloads the same words (`KeptSource::row`). `retain` of a reloaded cell, either way a build's lower calls it
    /// (a record's life, or D2854's copy), is that word again, so `q`'s arm and `P'`'s write one record word.
    #[test]
    fn a_kept_code_and_packed_numeric_round_trip_as_words() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let cells = lanes();
        let tfs = [row_of(25).unwrap(), row_of(1700).unwrap()];
        let mut pb = ProgramBuilder::new();
        let r = render(&mut pb);
        let keep = pb.declare_func("keep", &[Type::Ptr, Type::Ptr, Type::Ptr], &[Type::I64, Type::I64]);
        let mut reloaded = Vec::new();
        {
            let mut b = pb.define(keep);
            let (ctx, e0, e1) = (b.params()[0], b.params()[1], b.params()[2]);
            let ws = {
                let mut cx = lx(&mut b, ctx, r, rig.mcx());
                [(0, e0), (1, e1)].map(|(i, e)| {
                    let (w, c) = scan_form(&mut cx, CellRef { cell: &cells[i], val: e, null: None }, tfs[i], i as u16).expect("a scan form");
                    reloaded.push(c);
                    w
                })
            };
            b.ret(&ws);
        }
        let life = CellLife { until: Until::Statement, home: CellHome::Lane(Home::Dictionary) };
        assert_eq!(reloaded[0], Cell { life, at: At::Slot(0), ..cells[0] });
        let life = CellLife { until: Until::Statement, home: CellHome::Lane(Home::Word) };
        assert_eq!(reloaded[1], Cell { rep: Rep::PackedNum { facts: NUM, form: NumForm::Word }, life, at: At::Slot(1), ..cells[1] });
        let back = pb.declare_func("back", &[Type::Ptr, Type::Ptr], &[Type::I64, Type::I64, Type::I64, Type::I64, Type::I64]);
        {
            let mut b = pb.define(back);
            let (ctx, row) = (b.params()[0], b.params()[1]);
            let rp = b.conv(gravity_ir::ConvOp::PtrToInt, Type::I64, row);
            let out = {
                let mut cx = lx(&mut b, ctx, r, rig.mcx());
                let mut out = Vec::new();
                for (i, c) in reloaded.iter().enumerate() {
                    let w = crate::ops::hashjoin::spill::pair::cell_word(cx.b, row, rp, 1, i, 0);
                    let at = CellRef { cell: c, val: w, null: None };
                    let k = rep::retain(&mut cx, at, tfs[i], Until::Life, RetainHome::Record(LifeId(7)));
                    assert_eq!((k.val, k.cell), (w, *c), "a reloaded cell reaches every record as it is");
                    out.extend([w, rep::retain_copy(&mut cx, at, tfs[i], Until::Statement, RetainHome::Record(LifeId(7))).val]);
                }
                let code = CellRef { cell: &reloaded[0], val: out[0], null: None };
                out.push(rep::code_word(&mut cx, code));
                out
            };
            b.ret(&out);
        }
        let prog = pb.finish();
        let mut ctxw = vec![0u64; site_off(1) / 8 + 1];
        // SAFETY: the words are the ctx image; PART_ID is a u32 field inside site 0.
        unsafe { (ctxw.as_mut_ptr().cast::<u8>().add(site_off(0) + site::PART_ID) as *mut u32).write_unaligned(7) };
        let (code, mantissa) = ([41u32], [12345i64]);
        let it = gravity_interp::Interp::new(&prog);
        let words = it.run(keep, &[ctxw.as_ptr() as u64, code.as_ptr() as u64, mantissa.as_ptr() as u64]).expect("keeps");
        assert_eq!(words, [7 << 32 | 41, 12345]);
        // SAFETY: two `[kind, datum, null]` cells, both words.
        let row = unsafe { crate::ops::hashjoin::spill::load::kept_row(&[0, words[0], 0, 0, words[1], 0], 0x1234) };
        let got = it.run(back, &[ctxw.as_ptr() as u64, row.as_ptr() as u64]).expect("reloads");
        assert_eq!(got, [7 << 32 | 41, 7 << 32 | 41, 12345, 12345, 41], "the code's word twice, the mantissa twice, the code");
    }

    /// The home rule: a code read from a record (life 3, made by site 5's pipeline) still renders through that site once
    /// kept; a scan code of `q` renders through the frame's site, `P'`'s (`q`'s). A datum keeps its datum, and so does a
    /// dynamic-scale packed lane.
    #[test]
    fn a_kept_code_renders_through_the_site_it_came_from() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let [lane, num] = lanes();
        let record = Cell::of_record(Rep::Code { dict: DICT }, 25, NumTypmod::NONE, LifeId(3), 4, Nullable::Maybe);
        let text = Cell { rep: Rep::VarlenaPtr, ..record };
        let dynamic = Cell { rep: Rep::PackedNumDyn, ..num };
        let mut pb = ProgramBuilder::new();
        let r = render(&mut pb);
        let f = pb.declare_func("k", &[Type::Ptr, Type::Ptr], &[]);
        let mut b = pb.define(f);
        let (ctx, e) = (b.params()[0], b.params()[1]);
        let mut cx = lx(&mut b, ctx, r, rig.mcx());
        let tf = row_of(25).unwrap();
        let kept = |cx: &mut Lx<'_, '_, '_>, c: &Cell| scan_form(cx, CellRef { cell: c, val: e, null: None }, tf, 0);
        let (w, from_record) = kept(&mut cx, &record).expect("a record's code keeps its word");
        assert_eq!((w, from_record.life), (e, CellLife { until: Until::Statement, home: CellHome::Record(LifeId(3)) }));
        let (_, from_scan) = kept(&mut cx, &lane).expect("a scan code keeps its word");
        assert_eq!((cx.site_of(&from_record), cx.site_of(&from_scan)), (Some(5), Some(0)));
        let k = rep::retain(&mut cx, CellRef { cell: &from_record, val: w, null: None }, tf, Until::Life, RetainHome::Record(LifeId(7)));
        assert_eq!(cx.site_of(&k.cell), Some(5), "a build's record of it renders through the site it came from");
        assert!(kept(&mut cx, &text).is_none() && kept(&mut cx, &dynamic).is_none());
    }
}

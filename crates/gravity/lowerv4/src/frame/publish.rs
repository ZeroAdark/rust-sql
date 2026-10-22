//! D2798: the row an identity program is on, published before its quals and projection (`LowerOpts::publish`). User
//! code the cursor's own query evaluates may run UPDATE / DELETE ... WHERE CURRENT OF that cursor; C execCurrentOf
//! answers from the scan's current row (the scan slot ExecScanFetch filled before the qual and the projection), which a
//! served run otherwise keeps only in registers. Under the option every pipeline that ends in the client's sink (one, or
//! an Append's arms) gets a first step that stores its source row's `ctid` cell (the identity copy reads it at the scan:
//! a heap row's tid, an index-only scan's index entry heap tid) in one RowOf site of the first such source site (a late
//! site, so the driver's settle hands it to that source's position at the call's end), with the arm's relation. The
//! entry reads it while the run is suspended in that user code (exec admit/entry `exec_current_of`). A source row
//! without the cell stores nothing, and the entry answers that it cannot position on the row.
use gravity_abi::ctx::{site_off, POS_ROW, POS_VALID};
use gravity_abi::site::site;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, Type};
use gravity_program::{Parallel, Reason, SiteDesc};
use mcx::GrowIn;
use std::rc::Rc;

use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{Mechs, Next};
use crate::frame::iface::ends_in_sink;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::site::CTID_AT;

/// The first step of a pipeline that publishes: the row's `ctid` into RowOf site `site` with relation `relid`.
struct Publish {
    site: Rc<core::cell::Cell<usize>>,
    relid: u32,
}

impl StepMech for Publish {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        if let Some(c) = row.cols.get(CTID_AT as usize - 1).copied().flatten() {
            // the ItemPointerData image (bi_hi, bi_lo, ip_posid) into the site's 12-byte position (`ScanPos`): flags |
            // offset << 16, the relation, the block
            let v = row.vals[c as usize].0;
            let img = if b.value_type(v) == Type::Ptr { v } else { b.conv(ConvOp::IntToPtr, Type::Ptr, v) };
            let bi = b.load(Type::I32, img, 0);
            let sixteen = b.iconst(Type::I32, 16);
            let hi = b.int_bin(IntBinOp::Shl, bi, sixteen);
            let lo = b.int_bin(IntBinOp::Ushr, bi, sixteen);
            let blk = b.int_bin(IntBinOp::Or, hi, lo);
            let off64 = b.load_zext(Type::I16, img, 4);
            let off = b.conv(ConvOp::Trunc, Type::I32, off64);
            let up = b.int_bin(IntBinOp::Shl, off, sixteen);
            let flags = b.iconst(Type::I32, (POS_VALID | POS_ROW) as i64);
            let w0 = b.int_bin(IntBinOp::Or, up, flags);
            let s = site_off(self.site.get()) as i32;
            b.store(env.ctx, s + site::POS_FLAGS as i32, w0);
            let rel = b.iconst(Type::I32, self.relid as i32 as i64);
            b.store(env.ctx, s + site::POS_REL as i32, rel);
            b.store(env.ctx, s + site::POS_BLOCK as i32, blk);
        }
        next(b, env, row)
    }

    /// The row lives in the leader's context block, where the entry reads it.
    fn serial(&self) -> bool {
        true
    }
}

/// Under `publish`: every pipeline ending in the client's sink over a source site (no condition-cache cover, no late
/// decode: their step numbers are the source's) takes `Publish` first, all into one RowOf site, and runs serial.
pub fn insert<'g>(px: &mut Px<'g, '_>, mechs: &mut [Mechs<'g>], publish: bool) {
    if !publish {
        return;
    }
    let (gx, mcx) = (px.gx, px.mcx());
    let n = gx.pipelines().len();
    let mut site: Option<Rc<core::cell::Cell<usize>>> = None;
    for m in mechs.iter_mut().filter(|m| m.k < n && ends_in_sink(gx, m.k)) {
        let Some(s) = m.site.filter(|_| m.source.covers() == 0 && m.source.late_at() == 0 && !m.source.once()) else { continue };
        let Some(relid) = px.sites.get(s).map(|d| d.relation).filter(|&r| r != 0) else { continue };
        let at = site.get_or_insert_with(|| px.add_late_site(SiteDesc::row_of(px.arena, s as u16, relid))).clone();
        m.steps.push_in(mcx::pg_box_dyn!(mcx, Publish { site: at, relid } => dyn StepMech + 'g));
        m.steps.rotate_right(1); // first, as `insert(0, ..)` (a raising push, then the turn)
        if m.desc.is_eligible() {
            m.desc.parallel = Parallel::Ineligible { site: m.desc.driving_site, reason: Reason::Host };
        }
    }
}

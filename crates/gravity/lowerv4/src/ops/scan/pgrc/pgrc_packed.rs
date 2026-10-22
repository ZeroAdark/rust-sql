//! The pgrc half of the packed body (H06 b, body.form's `SourceMech::packed`; FRAME frame.rs `packed_views`, mask_vec.rs
//! `packed_leaf`; HOSTS 1f96f29921f): the first step masks a block on a lane's raw ByteFor deltas, undecoded. The
//! candidates are the by-value integer lanes the pipeline's first op reads only through `packed_leaf`'s form (one compare
//! `lane cond literal` in its packed image's order); the host packs a window only when every candidate is ByteFor at width
//! 1 / 2 or constant there (`VERDICT_PACKED`, slot 4 = the candidates), leaving them undecoded with exact validity. Each
//! window asks `lane_fill(site, k | LANE_PACKED)` for candidate k's `PackedFrames` (0: not packed; else its byte offset
//! from the lane's `ptr`); `WinConds::packed` holds when the window is packed and its blocks start on a 64-row boundary.
//! Every other read of a candidate (a survivor, the tail, a row body, a covered window) comes after the source's
//! `fill_late`, whose fill set takes the candidates beside the late lanes; an off-boundary packed window's (`Packs::window`)
//! after their decode at its open. A filter-only lane is a candidate: the host
//! elides it only in an `AllMatch` window, which it never packs (the frame skips the covered steps there). Offered only where the frame's packed body
//! runs on the window's block path: `body.form` and `mask.vec` both apply. Budget 160 lines. Owner M1-SOURCE.
use core::cell::RefCell;

use gravity_abi::ctx::lane;
use gravity_decompose::{Op, Source};
use gravity_expr::emit::ParamNeed;
use gravity_expr::{ExprNode, Source as VarSource};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use gravity_abi::site::{PackedFrame, PackedFrames, LANE_PACKED};
use gravity_program::{LaneId, Rep, SiteDesc};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};
use types_nodes::Node;

use crate::face::strategy::{Strategy, BODY_FORM, MASK_VEC};
use crate::face::source::Packed;
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px};
use crate::admit::refusal::Variant;

fn applies(px: &Px<'_, '_>, r: Strategy) -> bool {
    px.strategies.has(r) && crate::face::source::on(Variant::Strategy(r))
}

/// The candidates of pipeline `k`'s drafted site (bit l: site lane l), outside `skip` (the late lanes).
pub(crate) fn candidates(px: &mut Px<'_, '_>, k: usize, site: &SiteDesc, skip: u64) -> u64 {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let (Source::Range { scanrelid, .. }, Some(Op::Filter(q))) = (&p.source, p.ops.first()) else { return 0 };
    if !applies(px, BODY_FORM) || !applies(px, MASK_VEC) {
        return 0;
    }
    let qs = crate::step::expr::quals(px, k, 0);
    let nodes: PgVec<'_, Node<'_>> = q.as_list().map_or_else(|| mcx::pg_vec![in px.mcx(); *q], |l| l.iter().collect_in(px.mcx()));
    let n = site.natts().min(64);
    let lane_of = |a: i16| site.lanes()[..n].iter().position(|l| l.id == LaneId::Attr(a));
    let (mut leaf, mut other) = (0u64, 0u64);
    for (i, &node) in nodes.iter().enumerate() {
        let form = qs.get(i).and_then(|&e| leaf_attno(px, site, e));
        let known = gravity_decompose::expr::walk(node, &mut |x| {
            if let Some(v) = x.as_var().filter(|v| v.varno == *scanrelid as i32 && v.varlevelsup == 0) {
                match (lane_of(v.varattno), form) {
                    (Some(l), Some(a)) if a == v.varattno => leaf |= 1 << l,
                    (Some(l), _) => other |= 1 << l,
                    (None, _) => other = u64::MAX,
                }
            }
            true
        });
        if !known {
            return 0;
        }
    }
    let ok = |l: usize| {
        let d = &site.lanes()[l];
        d.kind == lane::KIND_BYVAL && matches!(d.width, 2 | 4 | 8) && matches!(d.rep, Rep::Word { .. })
    };
    (0..n).filter(|&l| leaf & !other & !skip & (1 << l) != 0 && ok(l)).fold(0, |m, l| m | 1 << l)
}

/// The column of qual `q` when it has `packed_leaf`'s form over a site lane: one literal, one compare of the column and
/// the literal in its packed image's order (mask_vec.rs `packed_cond`).
fn leaf_attno(px: &Px<'_, '_>, site: &SiteDesc, q: ExId) -> Option<i16> {
    let e = px.placed(q);
    if e.places.len() != 1 || !matches!(e.needs.first(), Some(ParamNeed::Const(_))) || e.ext.is_some() {
        return None;
    }
    let p = &e.c.prog;
    let mut id = e.c.root;
    while let ExprNode::Relabel(r) = &p.nodes[id.0 as usize] {
        id = r.arg;
    }
    let (ExprNode::Op(n) | ExprNode::Func(n)) = &p.nodes[id.0 as usize] else { return None };
    let call = &p.sites[n.site.0 as usize];
    let kids = &p.kids[call.args.start as usize..][..call.args.len as usize];
    let (v, swapped) = match (kids.len(), kids.first().map(|a| &p.nodes[a.0 as usize]), kids.get(1).map(|a| &p.nodes[a.0 as usize])) {
        (2, Some(ExprNode::Var(v)), Some(ExprNode::Const(_))) => (v, false),
        (2, Some(ExprNode::Const(_)), Some(ExprNode::Var(v))) => (v, true),
        _ => return None,
    };
    let d = site.lanes().iter().find(|l| l.id == LaneId::Attr(v.attno) && v.src == VarSource::Scan)?;
    crate::frame::mask_vec::packed_cond(call.funcid, d.width, swapped, v.typ).map(|_| v.attno)
}

/// A pgrc source's candidates and, per window, their frame tables.
pub(crate) struct Packs<'g> {
    /// `(site lane, attno)` per candidate.
    lanes: PgVec<'g, (usize, i16)>,
    win: RefCell<PgVec<'g, (i16, Value)>>,
}

impl<'g> Packs<'g> {
    pub(crate) fn new(site: &SiteDesc, mask: u64, mcx: Mcx<'g>) -> Packs<'g> {
        let lanes = (0..site.natts().min(64)).filter(|&l| mask & (1 << l) != 0).filter_map(|l| match site.lanes()[l].id {
            LaneId::Attr(a) => Some((l, a)),
            _ => None,
        });
        Packs { lanes: lanes.collect_in(mcx), win: RefCell::new(PgVec::new_in(mcx)) }
    }

    /// The window's packed fact: each candidate's table asked of the host through `fill` (`lane_fill`), `lane_ptr` its
    /// lane's `ptr`; `None` without candidates. D2844: a packed window off a 64-row boundary (a call resumed inside it, a
    /// code range's cursor) runs the plain block body, whose first step reads the candidates decoded: decoded here first.
    pub(crate) fn window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cursor: Value, lane_ptr: &dyn Fn(usize) -> Value, fill: &dyn Fn(&mut FunctionBuilder<'_>, &mut Env, u32) -> Value) -> Option<Value> {
        let mut win = self.win.borrow_mut();
        win.clear();
        let mut first = None;
        for &(l, a) in self.lanes.iter() {
            let off = fill(b, env, l as u32 | LANE_PACKED);
            let t = b.ptr_add(lane_ptr(l), off, 1, 0);
            win.push_in((a, t));
            first.get_or_insert(off);
        }
        let off = first?;
        let packed = b.icmp(IntCond::Ne, off, env.zero);
        let m63 = b.iconst(Type::I64, 63);
        let low = b.int_bin(IntBinOp::And, cursor, m63);
        let aligned = b.icmp(IntCond::Eq, low, env.zero);
        let unaligned = b.icmp(IntCond::Ne, low, env.zero);
        let decode = b.int_bin(IntBinOp::And, packed, unaligned);
        let (l_decode, l_on) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(decode, (l_decode, &[]), (l_on, &[]));
        b.switch_to(l_decode);
        for &(l, _) in self.lanes.iter() {
            fill(b, env, l as u32);
        }
        b.br(l_on, &[]);
        b.switch_to(l_on);
        Some(b.int_bin(IntBinOp::And, packed, aligned))
    }

    /// Column `attno`'s block at `base` as its frame's raw deltas (frames of 1024 rows from the window's row 0).
    pub(crate) fn packed(&self, b: &mut FunctionBuilder<'_>, attno: i16, base: Value) -> Option<Packed> {
        let t = self.win.borrow().iter().find(|w| w.0 == attno)?.1;
        let ten = b.iconst(Type::I64, 10);
        let f = b.int_bin(IntBinOp::Ushr, base, ten);
        let fr = b.ptr_add(t, f, core::mem::size_of::<PackedFrame>() as u8, core::mem::offset_of!(PackedFrames, frames) as i32);
        let at = |o: usize| o as i32;
        let d0 = b.load(Type::Ptr, fr, at(core::mem::offset_of!(PackedFrame, deltas)));
        let width = b.load(Type::I64, fr, at(core::mem::offset_of!(PackedFrame, width)));
        let reference = b.load(Type::I64, fr, at(core::mem::offset_of!(PackedFrame, reference)));
        let fmax = b.load(Type::I64, fr, at(core::mem::offset_of!(PackedFrame, fmax)));
        let first = b.load(Type::I64, fr, at(core::mem::offset_of!(PackedFrame, first_row)));
        let rel = b.int_bin(IntBinOp::Sub, base, first);
        let off = b.int_bin(IntBinOp::Mul, rel, width);
        let deltas = b.ptr_add(d0, off, 1, 0);
        Some(Packed { deltas, width, reference, fmax: Some(fmax) })
    }
}

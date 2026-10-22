//! The code-range verdict (H02, `verdict.coderange`, R2-L15; the hand kernels' `code_range` and
//! its range pre-check variant): a leading filter qual that step/percode.rs reads whole from a per-code `Bits` table (a
//! LIKE verdict resident from the hosts, a swept body) skips a window none of whose codes the table passes. At the
//! window's open, after the tables' bind, the window's code range `[lo, hi]` is read from its rows' codes (four a turn);
//! a window whose range holds no passing and no faulting entry is `NoMatch`, and the source hands the frame an empty
//! window. Only where that is exact: no selection (a PSMA window or deletes keep the host's rows), the qual strict over
//! the code column and constants (a NULL row, whose code the table never answers, fails it), every earlier qual
//! raise-free (join/rtfilter.rs `raise_free`, D237: a row PG raises on is never skipped), and inside the condition
//! cache's covered ops when the cache is on (a skipped window publishes no survivor). A range wider than
//! `cost::CODERANGE_WORDS_MAX` table words answers `Maybe` unread. An AB road (its B arm: `gravity_road_off`). Registers road
//! `verdict.coderange`. Owner M1-SOURCE.
use gravity_expr::percode::{CodeCol, Form};
use gravity_expr::{Compiled, ExprNode, NodeId, Source as VarSource};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value, VecBinOp};
use gravity_program::SiteDesc;
use mcx::GrowIn;

use crate::face::source::{VerdictImpl, VerdictMech};
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px};
use crate::ops::Table;

crate::strategies! {
    /// Code-range verdict against a per-code table (the table may stay resident across statements, R4).
    VERDICT_CODERANGE = 14, "verdict.coderange", CodeFilter;
}

pub(crate) const ROWS: Table = Table { strategies: &[(VERDICT_CODERANGE, &VerdictImpl(&CodeRange))], ..crate::ops::NONE };

struct CodeRange;

impl VerdictMech for CodeRange {
    fn declare(&self, px: &mut Px<'_, '_>, _site: &mut SiteDesc, qual: ExId) -> bool {
        of(px, qual).is_some()
    }
}

/// A qual's code-range verdict: its whole-qual `Bits` table `t` (`Env::tables` entry `t`) over code column `attno`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Range {
    pub t: u16,
    pub attno: i16,
}

/// Qual `q`'s range verdict: one code column, the whole qual one tabled per-code root, strict over the column (an
/// operator or function root: the window skip reads each window's code range, which a list over common values never
/// narrows, so a list stays per row here; measured at a condition-cache miss, a two-value list over a low-cardinality
/// column ran 1.3x with the skip).
pub(crate) fn of(px: &Px<'_, '_>, q: ExId) -> Option<Range> {
    verdict(px, q, false)
}

/// `of`, a list test included (`strict`'s ScalarArrayOp arm): code.sparse's table form, whose host walk drops a granule
/// whose base the table rejects, reads it.
pub(crate) fn of_list(px: &Px<'_, '_>, q: ExId) -> Option<Range> {
    verdict(px, q, true)
}

fn verdict(px: &Px<'_, '_>, q: ExId, lists: bool) -> Option<Range> {
    let e = px.placed(q);
    let pl = e.ext.as_deref().and_then(|x| x.downcast_ref::<crate::dict::percode::Plan>())?;
    let &[(attno, _, _)] = &pl.cols[..] else { return None };
    let c = &e.c;
    let plan = c.percode(&[CodeCol { attno }], px.mcx()).ok()?;
    let root = unrelabel(c, c.root);
    let r = plan.root_at(c.root).or_else(|| plan.root_at(root))?;
    let (t, class) = *pl.tables.get(r.table? as usize)?;
    let bits = matches!(r.form, Form::Bits) && matches!(class, crate::dict::percode::Class::Bits | crate::dict::percode::Class::Set);
    (bits && strict(c, root, attno, lists)).then_some(Range { t, attno })
}

fn unrelabel(c: &Compiled<'_>, mut id: NodeId) -> NodeId {
    while let ExprNode::Relabel(r) = &c.prog.nodes[id.0 as usize] {
        id = r.arg;
    }
    id
}

/// `id` is a strict operator or function over the code column and constants: a NULL row makes it NULL. With `lists`, a
/// list test (`col IN (..)`, `col NOT IN (..)`: a ScalarArrayOp) counts too, only over a non-NULL constant array of one
/// or more elements, as gravity_expr percode's body rule (D1812): PostgreSQL answers `x = ANY ('{}')` false and
/// `x <> ALL ('{}')` true without reading `x`, a NULL row included.
fn strict(c: &Compiled<'_>, id: NodeId, attno: i16, lists: bool) -> bool {
    let p = &c.prog;
    let site = match &p.nodes[id.0 as usize] {
        ExprNode::Op(n) | ExprNode::Func(n) => n.site,
        ExprNode::ScalarArrayOp(n) if lists => {
            let call = &p.sites[n.site.0 as usize];
            if call.args.len != 2 {
                return false;
            }
            let arr = unrelabel(c, p.kids[call.args.start as usize + 1]);
            if !matches!(&p.nodes[arr.0 as usize], ExprNode::Const(k) if k.list_len().is_some_and(|l| l > 0)) {
                return false;
            }
            n.site
        }
        _ => return false,
    };
    let call = &p.sites[site.0 as usize];
    let kids = &p.kids[call.args.start as usize..][..call.args.len as usize];
    call.strict
        && kids.iter().all(|&k| match &p.nodes[unrelabel(c, k).0 as usize] {
            ExprNode::Var(v) => v.src == VarSource::Scan && v.attno == attno,
            ExprNode::Const(_) => true,
            _ => false,
        })
}

/// Whether every row of `[cursor, end)` fails range `r` (a window without a selection): no entry of the table's bits or
/// faults between the rows' lowest and highest code (the lane at `codes`, a `u32` per row) is set. `false` for a range
/// wider than `cost::CODERANGE_WORDS_MAX` words; the loops are bounded by the window and by that cap.
pub(crate) fn none(b: &mut FunctionBuilder<'_>, env: &mut Env, r: Range, codes: Value, cursor: Value, end: Value) -> Value {
    let Some(tabs) = env.tables else { return b.bconst(false) };
    // the window test is emitted: the source's scan (ops/scan/pgrc/lower.rs) takes `of`, never `declare`
    fires::fire("verdict.coderange");
    let (i64t, v4) = (Type::I64, Type::V4I32);
    let words = b.load(Type::Ptr, tabs, 24 * r.t as i32);
    let faults = b.load(Type::Ptr, tabs, 24 * r.t as i32 + 8);
    let (four, m4) = (b.iconst(i64t, 4), b.iconst(i64t, !3));
    let n = b.int_bin(IntBinOp::Sub, end, cursor);
    let n4 = b.int_bin(IntBinOp::And, n, m4);
    let e4 = b.int_bin(IntBinOp::Add, cursor, n4);
    let (ones32, zero32) = (b.iconst(Type::I32, -1), b.iconst(Type::I32, 0));
    let (vlo0, vhi0) = (b.vsplat(v4, ones32), b.vsplat(v4, zero32));
    // four codes a turn
    let (l_vh, l_vb, l_vd) = (b.create_block(&[i64t, v4, v4]), b.create_block(&[]), b.create_block(&[v4, v4]));
    b.br(l_vh, &[cursor, vlo0, vhi0]);
    b.mark_loop_header(l_vh);
    env.bounded.push_in(l_vh);
    b.switch_to(l_vh);
    let (i, vlo, vhi) = (b.block_params(l_vh)[0], b.block_params(l_vh)[1], b.block_params(l_vh)[2]);
    let more = b.icmp(IntCond::Ult, i, e4);
    b.cond_br(more, (l_vb, &[]), (l_vd, &[vlo, vhi]));
    b.switch_to(l_vb);
    let p = b.ptr_add(codes, i, 4, 0);
    let v = b.vload_unaligned(v4, p, 0);
    let (vlo1, vhi1) = (b.vbin(VecBinOp::Umin, vlo, v), b.vbin(VecBinOp::Umax, vhi, v));
    let i4 = b.int_bin(IntBinOp::Add, i, four);
    b.br(l_vh, &[i4, vlo1, vhi1]);
    b.switch_to(l_vd);
    let (vlo, vhi) = (b.block_params(l_vd)[0], b.block_params(l_vd)[1]);
    let (mut lo, mut hi) = (b.iconst(i64t, u32::MAX as i64), env.zero);
    for k in 0..4 {
        let (x, y) = (b.vextract(vlo, k), b.vextract(vhi, k));
        let (x, y) = (b.conv(ConvOp::Zext, i64t, x), b.conv(ConvOp::Zext, i64t, y));
        (lo, hi) = (umin(b, lo, x), umax(b, hi, y));
    }
    // the tail, a code a turn
    let (l_sh, l_sb, l_sd) = (b.create_block(&[i64t, i64t, i64t]), b.create_block(&[]), b.create_block(&[i64t, i64t]));
    b.br(l_sh, &[e4, lo, hi]);
    b.mark_loop_header(l_sh);
    env.bounded.push_in(l_sh);
    b.switch_to(l_sh);
    let (i, lo, hi) = (b.block_params(l_sh)[0], b.block_params(l_sh)[1], b.block_params(l_sh)[2]);
    let more = b.icmp(IntCond::Ult, i, end);
    b.cond_br(more, (l_sb, &[]), (l_sd, &[lo, hi]));
    b.switch_to(l_sb);
    let p = b.ptr_add(codes, i, 4, 0);
    let c = b.load_zext(Type::I32, p, 0);
    let (lo1, hi1) = (umin(b, lo, c), umax(b, hi, c));
    let i1 = b.int_bin(IntBinOp::Add, i, env.one);
    b.br(l_sh, &[i1, lo1, hi1]);
    b.switch_to(l_sd);
    let (lo, hi) = (b.block_params(l_sd)[0], b.block_params(l_sd)[1]);
    // the table's words from lo's to hi's, the ends masked to [lo, hi]; an empty window (lo > hi) fails nothing
    let l_out = b.create_block(&[i64t]);
    let (six, m63, all) = (b.iconst(i64t, 6), b.iconst(i64t, 63), b.iconst(i64t, -1));
    let (w0, w1) = (b.int_bin(IntBinOp::Ushr, lo, six), b.int_bin(IntBinOp::Ushr, hi, six));
    let span = b.int_bin(IntBinOp::Sub, w1, w0);
    let cap = b.iconst(i64t, gravity_program::cost::CODERANGE_WORDS_MAX);
    let (empty, wide) = (b.icmp(IntCond::Ugt, lo, hi), b.icmp(IntCond::Sge, span, cap));
    let (l_rd, l_ok) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(empty, (l_out, &[env.one]), (l_ok, &[]));
    b.switch_to(l_ok);
    b.cond_br(wide, (l_out, &[env.zero]), (l_rd, &[]));
    b.switch_to(l_rd);
    let lo_s = b.int_bin(IntBinOp::And, lo, m63);
    let m_lo = b.int_bin(IntBinOp::Shl, all, lo_s);
    let hi_s = b.int_bin(IntBinOp::And, hi, m63);
    let hi_r = b.int_bin(IntBinOp::Sub, m63, hi_s);
    let m_hi = b.int_bin(IntBinOp::Ushr, all, hi_r);
    let (l_wh, l_wb, l_wd) = (b.create_block(&[i64t, i64t]), b.create_block(&[]), b.create_block(&[i64t]));
    b.br(l_wh, &[w0, env.zero]);
    b.mark_loop_header(l_wh);
    env.bounded.push_in(l_wh);
    b.switch_to(l_wh);
    let (w, acc) = (b.block_params(l_wh)[0], b.block_params(l_wh)[1]);
    let more = b.icmp(IntCond::Ule, w, w1);
    b.cond_br(more, (l_wb, &[]), (l_wd, &[acc]));
    b.switch_to(l_wb);
    let (pb, pf) = (b.ptr_add(words, w, 8, 0), b.ptr_add(faults, w, 8, 0));
    let (xb, xf) = (b.load(i64t, pb, 0), b.load(i64t, pf, 0));
    let x = b.int_bin(IntBinOp::Or, xb, xf);
    let (first, last) = (b.icmp(IntCond::Eq, w, w0), b.icmp(IntCond::Eq, w, w1));
    let xl = b.int_bin(IntBinOp::And, x, m_lo);
    let x = b.select(first, xl, x);
    let xh = b.int_bin(IntBinOp::And, x, m_hi);
    let x = b.select(last, xh, x);
    let acc1 = b.int_bin(IntBinOp::Or, acc, x);
    let w_1 = b.int_bin(IntBinOp::Add, w, env.one);
    b.br(l_wh, &[w_1, acc1]);
    b.switch_to(l_wd);
    let acc = b.block_params(l_wd)[0];
    let clear = b.icmp(IntCond::Eq, acc, env.zero);
    let clear = b.select(clear, env.one, env.zero);
    b.br(l_out, &[clear]);
    b.switch_to(l_out);
    let out = b.block_params(l_out)[0];
    b.icmp(IntCond::Ne, out, env.zero)
}

fn umin(b: &mut FunctionBuilder<'_>, x: Value, y: Value) -> Value {
    let lt = b.icmp(IntCond::Ult, y, x);
    b.select(lt, y, x)
}

fn umax(b: &mut FunctionBuilder<'_>, x: Value, y: Value) -> Value {
    let gt = b.icmp(IntCond::Ugt, y, x);
    b.select(gt, y, x)
}

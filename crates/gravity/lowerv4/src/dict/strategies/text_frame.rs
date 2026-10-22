//! Road `text.frame` (H09): `col LIKE 'pattern'` / `NOT LIKE` over a verbatim text lane (a
//! datum pointer per row, the images tiled in the lane's arena) is decided for the whole window by ONE substring pass for
//! the pattern's required literal over the window's bytes, each hit mapped to its row by the offsets and confirmed by
//! PG's matcher (host `like_frame`, kernels host_abi v4 m1). The filter's 64-row blocks then read the window's verdict
//! bits. A window the host declines (a selection, a code lane, an image not stored plainly, a pattern with no literal)
//! runs its rows through the per-row test, so nothing changes but the speed. Fires for a Const pattern straight under
//! `textlike` / `textnlike` (under at most one NOT) over a text column of the source's row (a character type or a domain
//! over one), under UTF-8 and a deterministic collation (where LIKE is PG's over bytes). NULL rows and a NULL pattern
//! pass neither form. Until M7 it fires on heap and other non-code text lanes only: pgrc publishes every text column as
//! a code lane (decompose `lane_of`; a verbatim part's are ranked codes) until the source and hosts choose the lane per
//! part. Over a source with no 64-row block path (heap, bitmap, TID, sample: no word window, so the frame pass never
//! runs) it declines a qual in text.memmem's word form when that road is on: the word form serves it per row without
//! reading the pattern.
//! Budget 160 lines. Owner M1-STEP.
use std::cell::Cell;
use std::rc::Rc;

use gravity_abi::ctx::lane::KIND_CODES;
use gravity_decompose::catalog::Am;
use gravity_decompose::{Op, Source};
use gravity_expr::emit::ParamNeed;
use gravity_expr::{Compiled, ExprNode, NodeId, Source as VarSource};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_lower::Place;
use gravity_program::LaneId;
use types_nodes::primnodes::BoolExprType;

use crate::cell::LaneId as CellLane;
use crate::dict::face::{PerValue, PerValueImpl, PerValueMech};
use crate::frame::env::Env;
use crate::frame::frame::{Mask, WordWin};
use crate::frame::px::{ExId, Px};
use crate::hir::Ex;
use crate::admit::refusal::Reason;

crate::strategies! {
    /// LIKE over a verbatim pgrc lane: one substring pass per frame and anchor, hits mapped to rows by the offsets.
    TEXT_FRAME = 60, "text.frame", PgrcLike;
}

pub(crate) const ROWS: crate::ops::Table =
    crate::ops::Table { strategies: &[(TEXT_FRAME, &PerValueImpl(&ROAD))], ..crate::ops::NONE };

const PG_UTF8: i32 = 6;
/// The verdict words of one window: a granule's 8192 rows, the word past the last a straddling read touches, one spare.
const CAP: usize = 8192 / 64 + 2;

/// A qual this road decides (its `Ex::ext`): site lane `lane` LIKE the pattern, `negated` for NOT LIKE; the pattern is
/// the qual's one word, bound per execution (D683). `out`: the verdict words of the window `open` passed last.
pub(crate) struct Frame {
    lane: u8,
    negated: bool,
    out: Cell<Option<Value>>,
}

/// The frame form of qual `c` read by step `j` of pipeline `k` over its site's own columns (only filters and gates
/// before it), `None` when it is not this road's shape.
pub(crate) fn attach(px: &mut Px<'_, '_>, k: usize, j: usize, c: &Compiled<'_>) -> Option<Rc<Frame>> {
    if !PerValue::Frame.on() || gravity_fn::typesem::collation::database_encoding() != PG_UTF8 {
        return None;
    }
    if !px.gx.pipelines()[k].ops[..j].iter().all(|o| matches!(o, Op::Filter(_) | Op::Gate(_))) {
        return None;
    }
    let p = &c.prog;
    let (root, not) = match &p.nodes[c.root.0 as usize] {
        ExprNode::Bool(n) if n.op == BoolExprType::NOT_EXPR => (p.kids[n.args.start as usize], true),
        _ => (c.root, false),
    };
    let (ExprNode::Op(n) | ExprNode::Func(n)) = &p.nodes[root.0 as usize] else { return None };
    let call = &p.sites[n.site.0 as usize];
    // LIKE / NOT LIKE by the catalog body (B2-17: every spelling of textlike / textnlike), not ILIKE
    let nlike = match gravity_expr::percode::like_body(call.funcid)? {
        (false, neg) => neg,
        (true, _) => return None,
    };
    // typesem's collation-bytes property: LIKE over bytes is PG's LIKE under a deterministic collation (PG 18 matches a
    // nondeterministic one per character)
    if call.args.len != 2 || !gravity_fn::typesem::claims_coll_bytes(call.collation) {
        return None;
    }
    let (attno, typ) = scan_col(c, p.kids[call.args.start as usize])?;
    // a character type, or a domain over one by the column's base type in the catalog facts (D2700: a domain stores
    // its base's bytes)
    let relid = px.gx.pipelines()[k].source.relation().map(|(_, relid, _)| relid);
    let base = relid.and_then(|r| px.gx.facts.att(r, attno)).map(|f| f.base);
    if !gravity_fn::typesem::character_type(typ) && !base.is_some_and(gravity_fn::typesem::character_type) {
        return None;
    }
    let ExprNode::Const(pat) = &p.nodes[p.kids[call.args.start as usize + 1].0 as usize] else { return None };
    if pat.byval {
        return None;
    }
    // the sources whose mechanisms open word windows (frame.rs's block path); the decline is by shape, never the pattern
    let blocks = matches!(px.gx.pipelines()[k].source, Source::Index { .. } | Source::Range { am: Am::Pgrcolumnar, .. });
    if !blocks && PerValue::Memmem.on() && crate::step::text_like::word_shape(c, root, &crate::step::text_like::domain_bases(px, c), px.mcx()).is_some() {
        return None;
    }
    let lanes = px.site_mut(k)?.lanes();
    let lane = lanes.iter().position(|l| l.id == LaneId::Attr(attno) && l.kind != KIND_CODES)?;
    let lane = u8::try_from(lane).ok()?;
    fires::fire("text.frame"); // shapecheck fire signal (REGISTRY STEP-11 / RUNS-39 / BG-13 / OIDW-57..59)
    Some(Rc::new(Frame { lane, negated: not != nlike, out: Cell::new(None) })) // alloc: Program
}

/// The column of the source's row at `id` (through any RelabelType) and the Var's type.
fn scan_col(c: &Compiled<'_>, id: NodeId) -> Option<(i16, u32)> {
    crate::stack_check();
    match &c.prog.nodes[id.0 as usize] {
        ExprNode::Var(v) if v.src == VarSource::Scan && v.attno > 0 => Some((v.attno, v.typ)),
        ExprNode::Relabel(r) => scan_col(c, r.arg),
        _ => None,
    }
}

/// Qual `e`'s frame form, if it has one.
pub(crate) fn frame_of<'a>(e: &'a Ex<'_>) -> Option<&'a Frame> {
    e.ext.as_deref().and_then(|x| x.downcast_ref())
}

/// The frame pass of this window for frame qual `q` into a stack slot of verdict words (read by `mask`), and the guard
/// that the host decided the window (`None`: no frame form, no site, or no one pattern word: the row path).
pub(crate) fn open(b: &mut FunctionBuilder<'_>, env: &mut Env, q: ExId) -> Option<Mask> {
    let f = frame_of(env.placed(q))?;
    let site = env.site?;
    let e = env.placed(q);
    if e.places.len() != 1 || !matches!(e.needs.first(), Some(ParamNeed::Const(_))) || !matches!(e.places[0], Place::Word(_)) {
        return None;
    }
    // D683: the pattern's word as this execution bound it, never its image's address baked (a front-key hit ran it freed)
    let (cv, cn) = crate::frame::hoist::words(b, env, env.node, q);
    let pattern = b.select(cn[0], env.zero, cv[0]);
    let slot = b.declare_stack_slot((CAP * 8) as u32, 8);
    let out = b.stack_addr(slot);
    let flags = HostFlags { touches_memory: true, ..HostFlags::default() };
    let params = [Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64, Type::Ptr, Type::I64];
    let h = b.declare_host_once("like_frame", &params, Some(Type::I64), gravity_abi::host_words!(gravity_kernels::ops::agg::entries::like_frame(ctx, site, lane, pattern, negated, out, cap)), flags);
    let words = [site as i64, f.lane as i64, f.negated as i64];
    let [s, l, n] = words.map(|w| b.iconst(Type::I64, w));
    let cap = b.iconst(Type::I64, CAP as i64);
    let st = b.call_host(h, &[env.ctx, s, l, pattern, n, out, cap], Some(Type::I64)).unwrap_or(env.zero);
    f.out.set(Some(out));
    Some(Mask(b.icmp(IntCond::Eq, st, env.zero)))
}

/// The 64 bits of the word array at `words` from bit `base` on: the two words it straddles, shifted together.
fn bits_at(b: &mut FunctionBuilder<'_>, words: Value, base: Value) -> Value {
    let (six, m63, one) = (b.iconst(Type::I64, 6), b.iconst(Type::I64, 63), b.iconst(Type::I64, 1));
    let wi = b.int_bin(IntBinOp::Ushr, base, six);
    let s = b.int_bin(IntBinOp::And, base, m63);
    let p = b.ptr_add(words, wi, 8, 0);
    let (lo, hi) = (b.load(Type::I64, p, 0), b.load(Type::I64, p, 8));
    let lo = b.int_bin(IntBinOp::Ushr, lo, s);
    // `hi << (64 - s)` as two shifts, so `s == 0` shifts it out rather than by 64
    let hi = b.int_bin(IntBinOp::Shl, hi, one);
    let back = b.int_bin(IntBinOp::Sub, m63, s);
    let hi = b.int_bin(IntBinOp::Shl, hi, back);
    b.int_bin(IntBinOp::Or, lo, hi)
}

/// The per-value mechanism of road `text.frame`.
pub(crate) struct Road;
pub(crate) static ROAD: Road = Road;

impl PerValueMech for Road {
    /// The rows of block `[base, base + 64)` within `m` that pass frame qual `expr`: the window's verdict bits.
    fn mask(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _w: &WordWin, base: Value, _lane: CellLane, expr: ExId, m: Mask) -> Mask {
        let Some(out) = frame_of(env.placed(expr)).and_then(|f| f.out.get()) else {
            crate::refuse!(env.node, Reason::Bug { payload: "a frame mask before its window's pass" })
        };
        let bits = bits_at(b, out, base);
        Mask(b.int_bin(IntBinOp::And, bits, m.0))
    }

    /// A frame qual is decided per window, never per value.
    fn value(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _lane: CellLane, _expr: ExId, _code: Value) -> (Value, Value) {
        crate::refuse!(env.node, Reason::Bug { payload: "a per-value read of a frame qual" })
    }
}

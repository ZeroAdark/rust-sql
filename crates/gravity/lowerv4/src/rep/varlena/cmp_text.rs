//! Road `text.eq` (H09, R13-L06): texteq / textne / bpchareq / bpcharne of a text / varchar / bpchar column and a constant,
//! and text `= ANY` of a constant array, open-coded (face/step.rs `ExprExtMech`). Budget 230 (+25 %). Owner M2-REP. Only
//! comparisons under the root's AND / OR / NOT (PG evaluates them with the expression; they raise nothing), under a collation
//! whose equality is byte equality (typesem::coll_bytes: no collation, C, POSIX, the default, any deterministic one). The constant is an operand (`sub`): its word is bound per
//! execution and read at run time, never baked (V4R-18: the front key is literal-free; V4R-72). Length first (bpchar's
//! trimmed: host `text_trim`) against the constant's; equal lengths compare 8-byte words, the last overlapping. `= ANY`
//! walks the array in host `text_any_eq`. A compressed / external datum, or a bpchar constant with trailing blanks
//! longer than the column, takes `text_eq_cold`, cold. No block form until the heap lanes (L4); name is not claimed.
//! D3151: the text side may be a view of the column (`rtrim1`, `substring`; text_view.rs) rather than the column.
use std::rc::Rc;
use mcx::{CollectIn, GrowIn, PgVec};
use gravity_decompose::catalog::typefacts;
use gravity_expr::nodes::{array::ElemFacts, var::Var};
use gravity_expr::{Compiled, ExprNode, NodeId, Program, Source as VarSource};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use types_nodes::Node;
use crate::face::step::{Claim, ExprExtImpl, ExprExtMech, ExtPlan, SubEmit, EXT_AT};
use crate::frame::{env::Env, px::{ExId, Px}, row::RowVals};
use crate::admit::refusal::Reason;
use crate::ops::Table;
use super::text_view::{self, View};

crate::strategies! {
    /// Open-coded text = / IN, length first.
    TEXT_EQ = 59, "text.eq", HeapTextFilter;
    /// textkeys step 1: text.eq's cheap per-row form (text_lean.rs): the constant decoded once per call, the column's header
    /// and trimmed length once per row, the trim open-coded. On by default since its flip (not on road_off.rs
    /// `DEFAULT_OFF`): off under a plain `text.lean`.
    TEXT_LEAN = 156, "text.lean", HeapTextFilter;
}

pub(crate) const ROWS: Table = Table { strategies: &[(TEXT_EQ, &ExprExtImpl(&EQ)), (TEXT_LEAN, &())], ..crate::ops::NONE };
struct Eq;
static EQ: Eq = Eq;

/// (funcid, negated, bpchar): texteq, textne, bpchareq, bpcharne.
const OPS: [(u32, bool, bool); 4] = [(67, false, false), (157, true, false), (1048, false, true), (1053, true, true)];

/// A claimed comparison: column `attno` (or D3151 its `view`) against the constant operand `sub` (its word), `negated`
/// for `<>`, `any`: an `= ANY` array's element alignment; `word`: road text.lean's frame words for the decoded constant
/// (text_lean.rs `words`; `None`: text.eq's form).
pub(super) struct Site { pub attno: i16, pub typ: u32, pub sub: u32, pub negated: bool, pub bpchar: bool, pub any: Option<i8>, pub view: View, pub word: Option<(i32, i32)> }

pub(super) fn trim(b: &[u8], bpchar: bool) -> &[u8] { &b[..if bpchar { b.iter().rposition(|&c| c != b' ').map_or(0, |i| i + 1) } else { b.len() }] }

/// The text column of the row at `id` (through any RelabelType): attno, type.
pub(super) fn col(p: &Program<'_>, id: NodeId) -> Option<(i16, u32)> {
    crate::stack_check();
    match &p.nodes[id.0 as usize] {
        ExprNode::Var(v) if v.src != VarSource::Inner && (1..EXT_AT).contains(&v.attno) && [25, 1042, 1043].contains(&v.typ) => Some((v.attno, v.typ)),
        ExprNode::Relabel(r) => col(p, r.arg),
        _ => None,
    }
}

/// Node `id` as a claimable comparison (`sub` unset) and its constant's argument index. The constant's value is not read.
fn site_of(p: &Program<'_>, id: NodeId) -> Option<(Site, usize)> {
    let (call, any) = match &p.nodes[id.0 as usize] {
        ExprNode::Op(n) | ExprNode::Func(n) => (&p.sites[n.site.0 as usize], None),
        ExprNode::ScalarArrayOp(s) if s.use_or => (&p.sites[s.site.0 as usize], Some(s.elem)),
        _ => return None,
    };
    let &(_, negated, bpchar) = OPS.iter().find(|o| o.0 == call.funcid)?;
    // D2046: no collation (0, two conflicting implicit ones) is left to the fmgr call, which raises 42P22 as PG's texteq
    if call.args.len != 2 || call.collation == 0 || !gravity_fn::typesem::claims_coll_bytes(call.collation) || (any.is_some() && (negated || bpchar)) {
        return None;
    }
    let kid = |i: u32| p.kids[(call.args.start + i) as usize];
    // D3151: a column, or (texteq / textne) a view of one: `rtrim1(c)`, `substring(..)` (text_view.rs `shape`)
    let operand = |id: NodeId| col(p, id).map(|(a, t)| (a, t, View::default())).or_else(|| {
        let id = relabeled(p, id);
        text_view::shape(p, id).filter(|_| !bpchar).map(|v| (v.attno, v.typ, View { trim: v.trim, substr: v.substr.map(|(_, n)| (0, n.map(|_| 0))) }))
    });
    let ((attno, typ, view), k) = operand(kid(0)).map(|a| (a, 1)).or_else(|| operand(kid(1)).filter(|_| any.is_none()).map(|a| (a, 0)))?;
    let ExprNode::Const(c) = &p.nodes[kid(k).0 as usize] else { return None };
    let any = match any {
        _ if c.is_null() || c.byval => return None,
        Some(ElemFacts { len: -1, byval: false, align, .. }) => Some(align),
        Some(_) => return None,
        None => None,
    };
    Some((Site { attno, typ, sub: 0, negated, bpchar, any, view, word: None }, k as usize))
}

/// Node `id` through any RelabelType.
fn relabeled(p: &Program<'_>, id: NodeId) -> NodeId {
    crate::stack_check();
    match &p.nodes[id.0 as usize] {
        ExprNode::Relabel(r) => relabeled(p, r.arg),
        _ => id,
    }
}

/// The plan's constant argument `i` of the call `n` (through a RelabelType).
fn plan_const(n: Node<'_>, i: usize) -> Option<Node<'_>> {
    let args = n.as_op_expr().map(|o| &o.args).or_else(|| n.as_func_expr().map(|f| &f.args)).or_else(|| n.as_scalar_array_op_expr().map(|s| &s.args))?;
    let a = *args.as_slice().get(i)?;
    let a = a.as_relabel_type().map_or(a, |r| r.arg);
    a.as_const().map(|_| a)
}

/// D3151: a view site's substring start / count plan nodes (`Some(None)`: no substring; `None`: the plan does not show
/// them, so the site is not claimed). `i` is the constant's argument index; the text side is the other one.
fn view_args<'p>(n: Node<'p>, i: usize, s: &Site) -> Option<Option<(Node<'p>, Option<Node<'p>>)>> {
    let Some((_, count)) = s.view.substr else { return Some(None) };
    let (st, c) = text_view::plan_args(n, 1 - i)?;
    (c.is_some() == count.is_some()).then_some(Some((st, c)))
}

/// expr.saop's decline (step/expr_saop.rs): whether `attach` claims the `= ANY` at `id` (the root's AND / OR / NOT walked).
pub(crate) fn claims_any(p: &Program<'_>, root: NodeId, id: NodeId) -> bool {
    crate::stack_check();
    let ExprNode::Bool(b) = &p.nodes[root.0 as usize] else { return root == id && site_of(p, id).is_some() };
    p.kids[b.args.start as usize..(b.args.start + b.args.len) as usize].iter().any(|&k| claims_any(p, k, id))
}

/// D4794: whether `attach` claims a comparison of the expression at `root` (the root's AND / OR / NOT walked). One
/// mechanism claims an expression (step/expr_ext.rs `claim`): expr.saop's bpchar byte set declines there, so these keep
/// their open-coded compare and the list its lazy walk.
pub(crate) fn claims_some(p: &Program<'_>, root: NodeId) -> bool {
    crate::stack_check();
    let ExprNode::Bool(b) = &p.nodes[root.0 as usize] else { return site_of(p, root).is_some() };
    p.kids[b.args.start as usize..(b.args.start + b.args.len) as usize].iter().any(|&k| claims_some(p, k))
}

impl ExprExtMech for Eq {
    fn attach<'p>(&self, px: &mut Px<'_, 'p>, at: (usize, usize), node: Node<'p>, c: &mut Compiled<'p>, ex: ExId, sub: &mut dyn FnMut(Node<'p>) -> u32) -> Option<Claim> {
        // road text.lean: a `=` / `<>` site decodes its constant at the call's entry into its frame words (keyed by the
        // expression, so a step-less compile, at `(usize::MAX, _)`: a fold's argument, a join's qual, takes them too)
        let lean = super::text_lean::on();
        // the plan tree and the program walked together (gravity_expr compiles a BoolExpr's args one kid each, in order):
        // a claimed comparison's plan node is `skip`ped, its constant an operand of its own (its word under the root)
        let mut stack = mcx::pg_vec![in px.mcx(); (c.root, node)];
        let (mut sites, mut skip) = (Vec::new(), Vec::new()); // alloc: Program
        while let Some((id, n)) = stack.pop() {
            match (&c.prog.nodes[id.0 as usize], n.as_bool_expr()) {
                (ExprNode::Bool(b), Some(e)) if e.args.len() == b.args.len as usize => stack.extend_in(c.children(b.args).iter().copied().zip(e.args.iter())),
                (ExprNode::Bool(_), _) => {}
                _ => if let Some((s, k, args)) = site_of(&c.prog, id).and_then(|(s, i)| plan_const(n, i).map(|k| { let a = view_args(n, i, &s); (s, k, a) })) {
                    // D3151: a view's substring start / count are operands of their own, after the constant
                    let Some(args) = args else { continue };
                    c.prog.nodes[id.0 as usize] = ExprNode::Var(Var { src: VarSource::Scan, attno: EXT_AT + sites.len() as i16, typ: 16, desc: 0 });
                    let k = sub(k);
                    let substr = args.map(|(st, n)| (sub(st), n.map(&mut *sub)));
                    if s.view != View::default() {
                        fires::fire("text.eq.view"); // D3151 witness signal: a comparison over a view of its column claimed
                    }
                    let word = (lean && s.any.is_none() && sites.len() < 64).then(|| super::text_lean::words(px, at, ex, sites.len() as u8));
                    if word.is_some() {
                        fires::fire("text.lean"); // textkeys witness signal: a site took text.lean's form
                    }
                    (sites.push(Site { sub: k, view: View { substr, ..s.view }, word, ..s }), skip.push(n.as_raw().as_ptr() as usize)); // alloc: Program
                },
            }
        }
        (!sites.is_empty()).then(|| Claim { plan: Rc::new(sites), skip })
    }

    fn emit(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, ex: ExId, site: u32, sub: SubEmit<'_>) -> [Value; 3] {
        let node = env.node;
        let sites = ExtPlan::of(env.placed(ex)).and_then(|x| x.plan.downcast_ref::<Vec<Site>>());
        let Some(s) = sites.and_then(|v| v.get(site as usize)) else { crate::refuse!(node, Reason::Bug { payload: "a text.eq site without its plan" }) };
        let Some(cr) = row.col(s.attno) else { crate::refuse!(node, Reason::Bug { payload: "a text.eq column the row does not carry" }) };
        let (tf, mut cx) = (typefacts::row_of(s.typ).copied().unwrap_or(typefacts::opaque(s.typ, s.typ, -1, false, false)), env.lx(b, node));
        let (p, null) = crate::rep::datum(&mut cx, cr, &tf);
        env.memo = cx.memo;
        let [k, kok, _] = sub(b, env, s.sub);
        // road text.lean: the site's constant decoded at the entry, where its operand is a bind word and the function has
        // the words site (`entry` asks the same); else the per-row decode
        let subs = ExtPlan::of(env.placed(ex)).map_or_else(|| PgVec::new_in(env.mcx), |x| mcx::to_vec_in(&x.subs.borrow()[..], env.mcx));
        let lean = s.word.filter(|_| env.words.is_some() && super::text_lean::konst_word(env, &subs, s).is_some());
        if s.view == View::default() {
            if let Some(w) = lean {
                return super::text_lean::compare(b, env, p, null, k, kok, s, w);
            }
            return compare(b, env, p, null, k, kok, s);
        }
        // D3151: the substring's start / count, non-NULL constants (text_view.rs `shape`)
        let words = s.view.substr.map(|(st, n)| (sub(b, env, st)[0], n.map(|n| sub(b, env, n)[0])));
        text_view::compare(b, env, p, null, k, kok, s, words, lean)
    }

    /// Road text.lean: each lean site's constant decoded once per call into its frame words (text_lean.rs `entry`).
    fn entry(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, ex: ExId) {
        let Some(x) = ExtPlan::of(env.placed(ex)).filter(|_| env.words.is_some()) else { return };
        let Some(sites) = x.plan.downcast_ref::<Vec<Site>>() else { return };
        let subs = mcx::to_vec_in(&x.subs.borrow()[..], env.mcx);
        for s in sites.iter() {
            if let (Some(w), Some(kw)) = (s.word, super::text_lean::konst_word(env, &subs, s)) {
                // bpchareq trims both sides; a view (texteq over rtrim1 / substring) compares the constant as it is
                super::text_lean::entry(b, env, kw, s.bpchar, w);
            }
        }
    }
}

/// The varlena at `p`: `(plain or 1-byte header, payload length, payload address)`; under a 1-byte header the word is
/// read from its aligned word (never past a short image).
pub(super) fn header(b: &mut FunctionBuilder<'_>, env: &Env, p: Value) -> (Value, Value, Value) {
    let (z, one) = (env.zero, env.one);
    let bin = |b: &mut FunctionBuilder<'_>, op: IntBinOp, x: Value, y: i64| { let y = b.iconst(Type::I64, y); b.int_bin(op, x, y) };
    let at = |b: &mut FunctionBuilder<'_>, v: Value| b.conv(ConvOp::IntToPtr, Type::Ptr, v);
    let (pp, al) = (at(b, p), bin(b, IntBinOp::And, p, !3));
    let b0 = b.load_zext(Type::I8, pp, 0);
    let (lo, ext) = (bin(b, IntBinOp::And, b0, 1), b.icmp(IntCond::Ne, b0, one));
    let (is1, four) = (b.icmp(IntCond::Ne, lo, z), b.iconst(Type::I64, 4));
    let a4 = { let a = b.select(is1, al, p); at(b, a) };
    let w = b.load_zext_unaligned(Type::I32, a4, 0);
    let (flags, l1, l4) = (bin(b, IntBinOp::And, w, 3), bin(b, IntBinOp::Ushr, b0, 1), bin(b, IntBinOp::Ushr, w, 2));
    let (plain, l1, l4) = (b.icmp(IntCond::Eq, flags, z), bin(b, IntBinOp::Sub, l1, 1), bin(b, IntBinOp::Sub, l4, 4));
    let (ok, len, off) = (b.select(is1, ext, plain), b.select(is1, l1, l4), b.select(is1, one, four));
    (ok, len, b.int_bin(IntBinOp::Add, p, off))
}

/// Site `s` over the datum `p` (`null`: its NULL flag) and the constant `k` (`kok`: not NULL): `[value, valid, err]`.
fn compare(b: &mut FunctionBuilder<'_>, env: &mut Env, p: Value, null: Value, k: Value, kok: Value, s: &Site) -> [Value; 3] {
    let (z, f, t) = (env.zero, b.bconst(false), b.bconst(true));
    let c = |b: &mut FunctionBuilder<'_>, v: i64| b.iconst(Type::I64, v);
    let (l_out, l_read, l_cold) = (b.create_block(&[Type::I64, Type::Bool, Type::I64]), b.create_block(&[]), b.create_block(&[]));
    let (l_len, l_bytes, l_res) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::Bool]));
    b.mark_cold(l_cold);
    // D1812: `= ANY` of an empty array is false whatever the datum, a NULL one included, so only a NULL array is absent
    // there; the host gets a NULL datum as 0
    let kn = b.int_bin(IntBinOp::Xor, kok, t);
    let absent = if s.any.is_some() { kn } else { b.int_bin(IntBinOp::Or, null, kn) };
    b.cond_br(absent, (l_out, &[z, f, z]), (l_read, &[]));
    b.switch_to(l_read);
    if let Some(align) = s.any {
        let h = b.declare_host_once("text_any_eq", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::I64), text_any_eq as *const () as usize, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() });
        let p = b.select(null, z, p);
        let r = { let a = c(b, align as i64); b.call_host(h, &[env.ctx, p, k, a], Some(Type::I64)).expect("returns") };
        let (hit, err, two) = (b.icmp(IntCond::Eq, r, env.one), b.icmp(IntCond::Slt, r, z), c(b, 2));
        let (nul, raise) = (b.icmp(IntCond::Eq, r, two), c(b, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64));
        let (v, ok, e) = (b.conv(ConvOp::Zext, Type::I64, hit), b.int_bin(IntBinOp::Xor, nul, t), b.select(err, raise, z));
        b.br(l_out, &[v, ok, e]);
    } else {
        let ((ok, len, data), (kok, klen, kdata)) = (header(b, env, p), header(b, env, k));
        let ok = b.int_bin(IntBinOp::And, ok, kok);
        b.cond_br(ok, (l_len, &[]), (l_cold, &[]));
        b.switch_to(l_len);
        let len = match s.bpchar {
            true => {
                let h = b.declare_host_once("text_trim", &[Type::I64; 2], Some(Type::I64), gravity_kernels::host_abi::text_trim as *const () as usize, HostFlags { touches_memory: true, pure: true, ..HostFlags::default() });
                b.call_host(h, &[data, len], Some(Type::I64)).expect("returns")
            }
            false => len,
        };
        let same = b.icmp(IntCond::Eq, len, klen);
        if s.bpchar {
            // a longer constant ending in a blank may trim to the column: the cold host trims it
            let l_long = b.create_block(&[]);
            let (l_blank, longer) = (b.create_block(&[]), b.icmp(IntCond::Ult, len, klen));
            b.cond_br(same, (l_bytes, &[]), (l_long, &[]));
            b.switch_to(l_long);
            b.cond_br(longer, (l_blank, &[]), (l_res, &[f]));
            b.switch_to(l_blank);
            let last = { let e = b.int_bin(IntBinOp::Add, kdata, klen); let e = b.conv(ConvOp::IntToPtr, Type::Ptr, e); b.load_zext(Type::I8, e, -1) };
            let blank = { let sp = c(b, b' ' as i64); b.icmp(IntCond::Eq, last, sp) };
            b.cond_br(blank, (l_cold, &[]), (l_res, &[f]));
        } else {
            b.cond_br(same, (l_bytes, &[]), (l_res, &[f]));
        }
        b.switch_to(l_bytes);
        let m = bytes_eq(b, env, data, kdata, len);
        b.br(l_res, &[m]);
    }
    b.switch_to(l_cold);
    let h = b.declare_host_once("text_eq_cold", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::I64), text_eq_cold as *const () as usize, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() });
    let r = { let bp = c(b, s.bpchar as i64); b.call_host(h, &[env.ctx, p, k, bp], Some(Type::I64)).expect("returns") };
    let (hit, bad) = (b.icmp(IntCond::Eq, r, env.one), b.icmp(IntCond::Slt, r, z));
    let raise = c(b, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    let (e, (v, ok)) = (b.select(bad, raise, z), result(b, s, hit, t));
    b.br(l_out, &[v, ok, e]);
    b.switch_to(l_res);
    let (v, ok) = result(b, s, b.block_params(l_res)[0], t);
    (b.br(l_out, &[v, ok, z]), b.switch_to(l_out));
    [0, 1, 2].map(|i| b.block_params(l_out)[i])
}

/// The value word and validity from `m` (the constant matched): negated for `<>`.
pub(super) fn result(b: &mut FunctionBuilder<'_>, s: &Site, m: Value, t: Value) -> (Value, Value) {
    let m = if s.negated { b.int_bin(IntBinOp::Xor, m, t) } else { m };
    (b.conv(ConvOp::Zext, Type::I64, m), t)
}

/// Whether the `n` bytes (a run-time length) at addresses `x` and `y` are equal: 8-byte words, the last overlapping the
/// one before; under 8 bytes two overlapping 4- or 2-byte words, or the one byte. The word loop is bounded (the length;
/// join/multiplicity.rs's walk): it runs inside the row loop's checked window.
pub(crate) fn bytes_eq(b: &mut FunctionBuilder<'_>, env: &mut Env, x: Value, y: Value, n: Value) -> Value {
    let (z, t) = (env.zero, b.bconst(true));
    let c = |b: &mut FunctionBuilder<'_>, v: i64| b.iconst(Type::I64, v);
    let eq = |b: &mut FunctionBuilder<'_>, w: usize, o: Value| {
        let ty = [Type::I8, Type::I16, Type::I16, Type::I32, Type::I32, Type::I32, Type::I32, Type::I64][w - 1];
        let [u, v] = [x, y].map(|a| { let a = b.int_bin(IntBinOp::Add, a, o); let a = b.conv(ConvOp::IntToPtr, Type::Ptr, a); if w == 8 { b.load_unaligned(ty, a, 0) } else { b.load_zext_unaligned(ty, a, 0) } });
        b.icmp(IntCond::Eq, u, v)
    };
    let l_done = b.create_block(&[Type::Bool]);
    let (l_loop, l_word, l_tail) = (b.create_block(&[Type::I64]), b.create_block(&[]), b.create_block(&[]));
    let blocks: PgVec<'_, _> = (0..6).map(|_| b.create_block(&[])).collect_in(env.mcx);
    b.mark_loop_header(l_loop);
    env.bounded.push_in(l_loop);
    let (eight, four, two) = (c(b, 8), c(b, 4), c(b, 2));
    let big = b.icmp(IntCond::Uge, n, eight);
    let last = b.int_bin(IntBinOp::Sub, n, eight);
    b.cond_br(big, (l_loop, &[z]), (blocks[0], &[]));
    b.switch_to(l_loop);
    let o = b.block_params(l_loop)[0];
    let more = b.icmp(IntCond::Slt, o, last);
    b.cond_br(more, (l_word, &[]), (l_tail, &[]));
    b.switch_to(l_word);
    let (e, o8) = (eq(b, 8, o), b.int_bin(IntBinOp::Add, o, eight));
    let f = b.bconst(false);
    b.cond_br(e, (l_loop, &[o8]), (l_done, &[f]));
    b.switch_to(l_tail);
    let e = eq(b, 8, last);
    b.br(l_done, &[e]);
    // under 8: blocks[0] tests 4, [1] reads two 4s, [2] tests 2, [3] reads two 2s, [4] tests 1, [5] reads the byte
    for (i, w, at) in [(0, 4usize, four), (2, 2, two)] {
        b.switch_to(blocks[i]);
        let ge = b.icmp(IntCond::Uge, n, at);
        b.cond_br(ge, (blocks[i + 1], &[]), (blocks[i + 2], &[]));
        b.switch_to(blocks[i + 1]);
        let (a, e) = (eq(b, w, z), { let o = b.int_bin(IntBinOp::Sub, n, at); eq(b, w, o) });
        let both = b.int_bin(IntBinOp::And, a, e);
        b.br(l_done, &[both]);
    }
    b.switch_to(blocks[4]);
    let empty = b.icmp(IntCond::Eq, n, z);
    b.cond_br(empty, (l_done, &[t]), (blocks[5], &[]));
    b.switch_to(blocks[5]);
    let e = eq(b, 1, z);
    (b.br(l_done, &[e]), b.switch_to(l_done));
    b.block_params(l_done)[0]
}

use super::raw::cmp_text::{text_eq_cold, text_any_eq};

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use crate::frame::stop::tests::run1;

    fn v1(b: &[u8]) -> Vec<u8> {
        let mut v = vec![((b.len() as u8 + 1) << 1) | 1]; // alloc: Program
        v.extend_from_slice(b);
        v
    }

    fn v4(b: &[u8]) -> Vec<u8> {
        let mut v = (((b.len() as u32 + 4) << 2).to_le_bytes()).to_vec(); // alloc: Program
        v.extend_from_slice(b);
        v
    }

    /// A one-dimensional text[] of `elems` (`None`: NULL): header, ndim, dataoffset, elemtype, dim, lbound, the null
    /// bitmap when one is NULL, then the 4-byte-header elements aligned 4.
    fn array(elems: &[Option<&[u8]>]) -> Vec<u8> {
        let nulls = elems.iter().any(|e| e.is_none());
        let data = if nulls { (24 + elems.len().div_ceil(8)).next_multiple_of(8) } else { 24 };
        let mut arr = Vec::new(); // alloc: Program
        for w in [0u32, 1, if nulls { data as u32 } else { 0 }, 25, elems.len() as u32, 1] {
            arr.extend_from_slice(&w.to_le_bytes());
        }
        if nulls {
            arr.extend((0..elems.len().div_ceil(8)).map(|i| (0..8).filter(|j| elems.get(i * 8 + j).is_some_and(|e| e.is_some())).fold(0u8, |a, j| a | 1 << j)));
        }
        arr.resize(data, 0);
        for e in elems.iter().flatten() {
            arr.resize(arr.len().next_multiple_of(4), 0);
            arr.extend_from_slice(&v4(e));
        }
        let n = arr.len() as u32;
        arr[..4].copy_from_slice(&(n << 2).to_le_bytes());
        arr
    }

    fn site(negated: bool, bpchar: bool, any: bool) -> Site {
        Site { attno: 1, typ: if bpchar { 1042 } else { 25 }, sub: 0, negated, bpchar, any: any.then_some(b'i' as i8), view: View::default(), word: None }
    }

    /// `(value, valid)` of site `s` over the datum `d` (`None`: NULL) and the constant image `k`, through the interpreter.
    fn eval(s: &Site, k: &[u8], d: Option<&[u8]>) -> (i64, bool) {
        let p = d.map_or(0, |d| d.as_ptr() as i64);
        let one = |valid: bool| {
            run1(&[], &mut |b, env| {
                let (pv, null, kv, t) = (b.iconst(Type::I64, p), b.bconst(d.is_none()), b.iconst(Type::I64, k.as_ptr() as i64), b.bconst(true));
                let [v, ok, _] = compare(b, env, pv, null, kv, t, s);
                if valid { b.conv(ConvOp::Zext, Type::I64, ok) } else { v }
            })
        };
        (one(false), one(true) != 0)
    }

    /// Equal and unequal lengths, 1-byte and 4-byte headers on both sides, lengths across the 8-byte words and the 4 / 2
    /// / 1 splits, the empty string, NULL, `<>`: PG's texteq / textne over bytes. One site serves every constant.
    #[test]
    fn text_eq_and_ne() {
        let words = ["", "a", "ab", "abc", "abcd", "abcdefg", "abcdefgh", "abcdefghi", "abcdefghijklmnopq", "abcdefghijklmnopr", "abcdefgh abcdefgh abcdefgh", "abcdefgh abcdefgh abcdefgi"];
        for neg in [false, true] {
            let s = site(neg, false, false);
            for n in words {
                for needle in [v4(n.as_bytes()), v1(n.as_bytes())] {
                    for d in words {
                        let want = (d == n) != neg;
                        for img in [v1(d.as_bytes()), v4(d.as_bytes())] {
                            assert_eq!(eval(&s, &needle, Some(&img)), (want as i64, true), "{d:?} vs {n:?} neg {neg}");
                        }
                    }
                    assert!(!eval(&s, &needle, None).1, "NULL is NULL");
                }
            }
        }
    }

    /// bpchar compares without trailing blanks on both sides (bpchareq): a constant ending in blanks through the cold host.
    #[test]
    fn bpchar_ignores_trailing_blanks() {
        let s = site(false, true, false);
        for needle in [v4(b"ab  "), v4(b"ab")] {
            for (d, want) in [("ab", 1), ("ab      ", 1), ("ab x", 0), ("a", 0), ("", 0), (" ab", 0), ("abc", 0)] {
                assert_eq!(eval(&s, &needle, Some(&v1(d.as_bytes()))).0, want, "{d:?}");
            }
        }
        let blank = v4(b"   ");
        assert_eq!((eval(&s, &blank, Some(&v1(b""))).0, eval(&s, &blank, Some(&v1(b" "))).0, eval(&s, &blank, Some(&v1(b"x"))).0), (1, 1, 0));
    }

    /// `= ANY` of 1, 16 and 17 constants of mixed lengths, with a NULL element: a match is true, no match NULL; without
    /// one, false. The array is read at run time (`text_any_eq`).
    #[test]
    fn any_lists() {
        let s = site(false, false, true);
        for n in [1usize, 16, 17] {
            let words: Vec<String> = (0..n).map(|i| "k".repeat(1 + i % 5) + &i.to_string()).collect(); // alloc: Program
            for null_elem in [false, true] {
                let mut elems: Vec<Option<&[u8]>> = words.iter().map(|w| Some(w.as_bytes())).collect(); // alloc: Program
                if null_elem {
                    elems.insert(n / 2, None);
                }
                let arr = array(&elems);
                for w in words.iter() {
                    assert_eq!(eval(&s, &arr, Some(&v1(w.as_bytes()))), (1, true), "{w}");
                }
                for miss in ["k", "kk99", "zzzzzzzzzzzz"] {
                    assert_eq!(eval(&s, &arr, Some(&v1(miss.as_bytes()))), (0, !null_elem), "{miss}");
                }
                assert!(!eval(&s, &arr, None).1);
            }
        }
        // D1812: an empty array is false for every datum, NULL included
        let empty = array(&[]);
        assert_eq!((eval(&s, &empty, Some(&v1(b"k1"))), eval(&s, &empty, None)), ((0, true), (0, true)));
    }

    /// A compressed datum takes the cold host (detoasted, then compared); so does the host alone for a bpchar pair.
    #[test]
    fn cold_host() {
        let needle = v4(b"hello");
        let (a, b2) = (v1(b"hello"), v4(b"hellO"));
        // SAFETY: every argument is a live varlena the test owns for the block.
        unsafe {
            assert_eq!(text_eq_cold(0, a.as_ptr() as u64, needle.as_ptr() as u64, 0), 1);
            assert_eq!(text_eq_cold(0, b2.as_ptr() as u64, needle.as_ptr() as u64, 0), 0);
            let (x, y) = (v1(b"hi  "), v4(b"hi"));
            assert_eq!(text_eq_cold(0, x.as_ptr() as u64, y.as_ptr() as u64, 1), 1);
            assert_eq!(text_eq_cold(0, x.as_ptr() as u64, y.as_ptr() as u64, 0), 0);
        }
    }

    /// A program `col OP 'x'` (`funcid`, `coll`; `any`: `col = ANY('{x,yy}')`), the column of `typ`.
    fn prog<'a>(funcid: u32, coll: u32, typ: u32, konst: &'a [u8], any: bool) -> Program<'a> {
        use gravity_expr::nodes::func::{Call, FuncNode, How};
        use gravity_expr::nodes::konst::ConstNode;
        use gravity_expr::nodes::op::ScalarArrayOp;
        use gravity_expr::{Kids, SiteId};
        let k = ConstNode::new(gravity_fn::NullableDatum::value(gravity_fn::Datum::from_usize(konst.as_ptr() as usize)), if any { 1009 } else { 25 }, -1, false);
        let call = Call { funcid, args: Kids { start: 0, len: 2 }, nargs: 2, strict: true, collation: coll, how: How::Fmgr(0), rettype: 16, volatile: false };
        let op = match any {
            true => ExprNode::ScalarArrayOp(ScalarArrayOp { site: SiteId(0), use_or: true, elem: ElemFacts { typ: 25, len: -1, byval: false, align: b'i' as i8 } }),
            false => ExprNode::Op(FuncNode { site: SiteId(0) }),
        };
        let v = ExprNode::Var(Var { src: VarSource::Scan, attno: 2, typ, desc: 0 });
        { let mut p = Program::empty_in(crate::step::expr::test_lists()); p.nodes.extend_in([v, ExprNode::Const(k), op]); p.kids.extend_in([NodeId(0), NodeId(1)]); p.sites.extend_in([call]); p } // alloc: Program
    }

    /// The claimed forms (text / varchar / bpchar =, <>, text = ANY) and G4 (does not apply): a nondeterministic or other
    /// collation, no collation (D2046), a name column, bpchar = ANY, an operator the road does not open-code (<, LIKE) are left to gravity_expr.
    /// V4R-18: deciding the claim reads no constant (the bake log stays empty), so the front key stays literal-free.
    #[test]
    fn claims_only_its_forms() {
        let (x, arr) = (v4(b"x"), array(&[Some(b"x"), Some(b"yy")]));
        for (f, coll, typ, any, claimed) in [
            (67, 100, 25, false, true),
            (67, 950, 1043, false, true),
            (157, 951, 25, false, true),
            (1048, 100, 1042, false, true),
            (67, 100, 25, true, true),
            (67, 12345, 25, false, false),
            (67, 0, 25, false, false),
            (67, 100, 19, false, false),
            (1048, 100, 1042, true, false),
            (664, 100, 25, false, false),
            (850, 100, 25, false, false),
        ] {
            let konst = if any { &arr } else { &x };
            let d = gravity_expr::bake::Scope::open();
            let s = site_of(&prog(f, coll, typ, konst, any), NodeId(2));
            assert_eq!(d.reads(), Vec::<usize>::new(), "funcid {f}: a constant was baked");
            drop(d);
            assert_eq!(s.is_some(), claimed, "funcid {f} coll {coll} typ {typ} any {any}");
            if let Some((s, k)) = s {
                assert_eq!((s.attno, s.negated, s.bpchar, s.any.is_some(), k), (2, f == 157, f == 1048, any, 1));
            }
        }
        assert_eq!(trim(b"ab  ", true), b"ab");
        assert_eq!(trim(b"ab  ", false), b"ab  ");
    }
}

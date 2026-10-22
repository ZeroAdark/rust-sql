//! Road `jsonb.inplace` (H14, R13-L07): `col ->> 'key' = 'v'` / `<> 'v'` (texteq / textne over
//! `jsonb_object_field_text` with constant key and value, a deterministic collation) reads the jsonb image in place: the
//! root object's keys walked in their stored order (length first, so a longer key ends the walk), the value's entry
//! compared where it lies; no `->>` text is made. A missing key or a JSON null is NULL, a string or boolean compares
//! as its text. A non-object root, more than 16 pairs, a number or container value, or a toasted / compressed image
//! takes PG's call cold (step/text_like.rs `claim_calls`). STEP-12: a constant path of 1..=JSONB_PATH_MAX keys reads
//! the same way, one unrolled object walk per key: `col #>> '{a,b}'` (jsonb_extract_path_text of a text[] constant, no
//! NULL element) and a chain of `->` ending in `->>` or `#>>`; a scalar (a JSON null too) above the last key is NULL, an
//! array or an object of more than 16 pairs at any level is cold. D476: `col ->> 'key' = ANY(array)` / `<> ALL` of
//! a constant text array (C12h's `IN (..)`) under the root's AND / OR / NOT walks to the entry the same way; the array
//! is an operand (`sub`), bound per execution, never read here (IN lists are literal-free): D564, its
//! elements decoded once per call at `entry` into frame words (host `field_any_decode`), compared per row in IR, length
//! first, unrolled over the list's count (its class, V4R-110); an array `entry` did not decode (not plain, a compressed
//! element, NULL, no frame words) is walked per row in host `field_any`. Budget 470 lines. Owner M2-FRAME.
use gravity_expr::nodes::func::Call;
use mcx::GrowIn;
use gravity_expr::{Compiled, ExprNode, NodeId, Source as VarSource};
use gravity_fn::typesem::pgrc as sem;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use types_nodes::Node;

use crate::step::text_like::{claim_calls, column, const_bytes, deterministic, frame_bytes, site, view, word};
use crate::step::text_case::memeq;
use crate::face::step::{Claim as ExtClaim, ExprExtImpl, ExprExtMech, SubEmit};
use crate::frame::env::Env;
use crate::face::kinds::Face;
use crate::face::step::ExtPlan;
use crate::frame::px::{ExId, Px, WordKey};
use crate::frame::row::RowVals;
use crate::ops::scan::pgrc::raw::{const_text_elems, field_any, field_any_decode};

crate::strategies! {
    /// ->> / -> shims read the image in place under the fmgr result contract.
    JSONB_INPLACE = 61, "jsonb.inplace", Jsonb;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(JSONB_INPLACE, &ExprExtImpl(&JSONB))], ..crate::ops::NONE };

const JSONB_OID: u32 = 3802;
pub(crate) const TEXT_OID: u32 = 25;
/// The most pairs walked inline: an object of n pairs has 2n entries, and up to `JB_OFFSET_STRIDE` entries only entry 0
/// stores an offset, which equals its length, so every entry's low 28 bits are its length.
const MAX_PAIRS: i64 = (gravity_abi::limits::JB_OFFSET_STRIDE / 2) as i64;
const JB_FOBJECT: i64 = 0x2000_0000;
const LEN_MASK: i64 = 0x0FFF_FFFF;
const TYPE_MASK: i64 = 0x7000_0000;
/// The value entry types: string, false, true, null.
const TYPES: [i64; 4] = [0, 0x2000_0000, 0x3000_0000, 0x4000_0000];
/// A container value's entry type (`JENTRY_ISCONTAINER`).
const JB_CONTAINER: i64 = 0x5000_0000;
/// The most keys a path walks (the cost home's choice).
const PATH_MAX: usize = gravity_program::cost::pgrc::JSONB_PATH_MAX;
/// A boolean value's text, as `->>` renders it, in one frame slot (`frame_bytes`, D627: never a static's address): "true"
/// at 0, "false" at 8.
const BOOL_TXT: [u8; 13] = *b"true\0\0\0\0false";

/// The compared text: a literal (`=` / `<>`; its image makes a V4R-18 variant), or an array operand (`= ANY` / `<> ALL`:
/// `sub` index, the varlena elements' alignment, never read; `words`: its frame words' byte offsets, the status then each
/// element's address and length, D564; none: the per-row host walk).
pub(crate) enum Val {
    Lit(Box<[u8]>),
    Any { sub: u32, align: i8, words: Vec<i32> },
}

/// The most elements decoded into frame words (the cost home's choice, beside PostgreSQL's hashed-list threshold).
const MAX_WORDS_LIST: usize = gravity_program::cost::pgrc::JSONB_ANY_LIST_MAX;

/// A claimed compare: the jsonb column, the path's keys (the root's first), the compared text, and `<>` (`<> ALL`).
pub(crate) struct Field {
    col: (VarSource, i16, u32),
    keys: Vec<Box<[u8]>>,
    val: Val,
    ne: bool,
}

/// `id`'s call site, when `id` is an operator or a function call of two arguments.
fn call2<'c>(c: &'c Compiled<'_>, id: NodeId) -> Option<&'c Call> {
    let (ExprNode::Op(n) | ExprNode::Func(n)) = &c.prog.nodes[id.0 as usize] else { return None };
    Some(&c.prog.sites[n.site.0 as usize]).filter(|s| s.args.len == 2)
}

/// `id` (through a RelabelType) as a constant path of a jsonb column read as text: `->>` (jsonb_object_field_text) of a
/// key or `#>>` (jsonb_extract_path_text) of a text[] constant, over the column or a chain of `->` (jsonb_object_field) of
/// keys over it; 1..=PATH_MAX keys, none NULL. The column and the keys, the root's first. A key is read (a logged bake:
/// the path makes a V4R-18 variant) only once the shape is whole; a `#>>` path is read before its count decides, so a
/// path refused for its count (none, past PATH_MAX) or a NULL element is the literal's variant too.
pub(crate) fn field(c: &Compiled<'_>, mut id: NodeId) -> Option<((VarSource, i16, u32), Vec<Box<[u8]>>)> {
    while let ExprNode::Relabel(r) = &c.prog.nodes[id.0 as usize] {
        id = Some(r.arg).filter(|_| r.transparent())?;
    }
    let s = call2(c, id)?;
    let path = sem::jsonb_path_text(s.funcid);
    if !path && !sem::jsonb_field_text(s.funcid) {
        return None;
    }
    let arg = |s: &Call, k: u32| c.prog.kids[(s.args.start + k) as usize];
    let (tail, mut at, mut chain) = (arg(s, 1), arg(s, 0), Vec::new()); // alloc: Program
    let col = loop {
        if let Some(col) = column(c, at, &[JSONB_OID]) {
            break col;
        }
        let s = call2(c, at).filter(|s| sem::jsonb_field(s.funcid) && chain.len() < PATH_MAX)?;
        chain.push(arg(s, 1));
        at = arg(s, 0);
    };
    // an empty path is the whole document as text (not a field); the count is the read image's (no key holds it)
    let elems = match (path, &c.prog.nodes[tail.0 as usize]) {
        (true, ExprNode::Const(k)) if chain.len() < PATH_MAX => Some(const_text_elems(k).filter(|e| !e.is_empty() && chain.len() + e.len() <= PATH_MAX)?),
        (false, _) if chain.len() < PATH_MAX => None,
        _ => return None,
    };
    let mut keys = chain.iter().rev().map(|&k| const_bytes(c, k).map(Box::<[u8]>::from)).collect::<Option<Vec<_>>>()?; // alloc: Program
    match elems {
        Some(e) => keys.extend(e.into_iter().map(Box::<[u8]>::from)),
        None => keys.push(const_bytes(c, tail)?.into()),
    }
    Some((col, keys))
}

/// The walk along `keys` to the value entry in the image at `p` (`null`: a NULL column): `(hot, text address, text
/// length)`, the length -1 for a NULL (the column, a missing key, a JSON null, a scalar above the last key), a boolean as
/// its text. Each key walks one object, unrolled: its keys in stored order (length first, so a longer key ends the walk),
/// then the value's offset summed from the entries' lengths; a container value above the last key is the next object, at
/// that offset padded to 4 (jsonb_util.c `fillJsonbValue`). Not hot: a non-object root, an array or an object of more than
/// 16 pairs at any level, a number or container value at the last (PG's call, cold).
fn entry(b: &mut FunctionBuilder<'_>, p: Value, null: Value, keys: &[Box<[u8]>]) -> (Value, Value, Value) {
    // the first key's walk is built in the one-key form's order: a one-key `->>` emits the IR it did (its pins)
    let k = |b: &mut FunctionBuilder<'_>, v: i64| b.iconst(Type::I64, v);
    let [zero, one, klen0, none] = [0, 1, keys[0].len() as i64, -1].map(|v| k(b, v));
    let (t, pw) = (b.bconst(true), b.conv(ConvOp::PtrToInt, Type::I64, p));
    let done = b.create_block(&[Type::Bool, Type::I64, Type::I64]);
    let mut obj_at = p;
    for (lv, key) in keys.iter().enumerate() {
        let klen = if lv == 0 { klen0 } else { k(b, key.len() as i64) };
        let h = b.load_zext_unaligned(Type::I32, obj_at, 0);
        let n = { let m = k(b, LEN_MASK); b.int_bin(IntBinOp::And, h, m) };
        let obj = { let m = k(b, JB_FOBJECT); let o = b.int_bin(IntBinOp::And, h, m); b.icmp(IntCond::Ne, o, zero) };
        let small = { let m = k(b, MAX_PAIRS); b.icmp(IntCond::Sle, n, m) };
        let ok = b.int_bin(IntBinOp::And, obj, small);
        let entries = b.ptr_add(obj_at, zero, 1, 4);
        let three = k(b, 3);
        let n8 = b.int_bin(IntBinOp::Shl, n, three);
        let base = b.ptr_add(entries, n8, 1, 0);
        let len_at = |b: &mut FunctionBuilder<'_>, j: Value| {
            let meta = b.ptr_add(entries, j, 4, 0);
            let meta = b.load_zext_unaligned(Type::I32, meta, 0);
            let m = b.iconst(Type::I64, LEN_MASK);
            (b.int_bin(IntBinOp::And, meta, m), meta)
        };
        let [kloop, kbody, kcmp, knext, sum, vread] = [2, 0, 0, 3, 3, 2].map(|n| b.create_block(&[Type::I64; 3][..n]));
        // the root not an object reads as NULL for a NULL column (its view), else cold; a deeper level is cold
        let cold = if lv == 0 { null } else { b.bconst(false) };
        b.cond_br(ok, (kloop, &[zero, zero]), (done, &[cold, pw, none]));
        // the keys, in stored order: an equal length compares bytes, a longer one ends the walk (a missing key: NULL)
        b.switch_to(kloop);
        b.mark_loop_header(kloop);
        let [i, off] = [b.block_params(kloop)[0], b.block_params(kloop)[1]];
        let more = b.icmp(IntCond::Slt, i, n);
        b.cond_br(more, (kbody, &[]), (done, &[t, pw, none]));
        b.switch_to(kbody);
        let (kl, _) = len_at(b, i);
        let eqlen = b.icmp(IntCond::Eq, kl, klen);
        b.cond_br(eqlen, (kcmp, &[]), (knext, &[i, off, kl]));
        b.switch_to(kcmp);
        let at = b.ptr_add(base, off, 1, 0);
        let e = memeq(b, at, key);
        b.cond_br(e, (sum, &[i, off, i]), (knext, &[i, off, kl]));
        b.switch_to(knext);
        let [i, off, kl] = [0, 1, 2].map(|x| b.block_params(knext)[x]);
        let past = b.icmp(IntCond::Sgt, kl, klen);
        let (i1, off1) = (b.int_bin(IntBinOp::Add, i, one), b.int_bin(IntBinOp::Add, off, kl));
        b.cond_br(past, (done, &[t, pw, none]), (kloop, &[i1, off1]));
        // key `i` found at `off`: the value's offset is the sum of every entry's length before entry `n + i`
        b.switch_to(sum);
        b.mark_loop_header(sum);
        let [j, off, i] = [0, 1, 2].map(|x| b.block_params(sum)[x]);
        let end = b.int_bin(IntBinOp::Add, n, i);
        let more = b.icmp(IntCond::Slt, j, end);
        let (jl, _) = len_at(b, j);
        let (j1, off1) = (b.int_bin(IntBinOp::Add, j, one), b.int_bin(IntBinOp::Add, off, jl));
        b.cond_br(more, (sum, &[j1, off1, i]), (vread, &[end, off]));
        b.switch_to(vread);
        let [ve, off] = [b.block_params(vread)[0], b.block_params(vread)[1]];
        let (vl, meta) = len_at(b, ve);
        let ty = { let m = k(b, TYPE_MASK); b.int_bin(IntBinOp::And, meta, m) };
        let is = |b: &mut FunctionBuilder<'_>, c: i64| { let c = b.iconst(Type::I64, c); b.icmp(IntCond::Eq, ty, c) };
        if lv + 1 < keys.len() {
            // above the last key a container is the next level; a scalar, a JSON null too, has no field (NULL)
            let (cont, next) = (is(b, JB_CONTAINER), b.create_block(&[]));
            let pad = { let x = b.int_bin(IntBinOp::Add, off, three); let m = k(b, !3); b.int_bin(IntBinOp::And, x, m) };
            obj_at = b.ptr_add(base, pad, 1, 0);
            b.cond_br(cont, (next, &[]), (done, &[t, pw, none]));
            b.switch_to(next);
            continue;
        }
        let [s, fa, tr, nu] = TYPES.map(|c| is(b, c));
        let at = { let a = b.ptr_add(base, off, 1, 0); b.conv(ConvOp::PtrToInt, Type::I64, a) };
        let bt = frame_bytes(b, &BOOL_TXT);
        let [eight, four, five] = [8, 4, 5].map(|v| k(b, v));
        let bf = b.int_bin(IntBinOp::Add, bt, eight);
        let (bp, bl, isb) = (b.select(tr, bt, bf), b.select(tr, four, five), b.int_bin(IntBinOp::Or, tr, fa));
        let (vp, vl) = (b.select(isb, bp, at), b.select(isb, bl, vl));
        let vl = b.select(nu, none, vl);
        let hot = { let x = b.int_bin(IntBinOp::Or, s, isb); b.int_bin(IntBinOp::Or, x, nu) };
        b.br(done, &[hot, vp, vl]);
    }
    b.switch_to(done);
    let d = b.block_params(done);
    (d[0], d[1], d[2])
}

/// `->> key = val` (`<>` when `ne`) over the image at `p`, `keys` the path: `(hot, value word, valid)`.
pub(crate) fn compare(b: &mut FunctionBuilder<'_>, p: Value, null: Value, keys: &[Box<[u8]>], val: &[u8], ne: bool) -> (Value, Value, Value) {
    let (hot, vp, vl) = entry(b, p, null, keys);
    let (cmp, out) = (b.create_block(&[]), b.create_block(&[Type::Bool]));
    let (want, none, f) = (b.iconst(Type::I64, val.len() as i64), b.iconst(Type::I64, -1), b.bconst(false));
    let same = b.icmp(IntCond::Eq, vl, want);
    b.cond_br(same, (cmp, &[]), (out, &[f]));
    b.switch_to(cmp);
    let at = b.conv(ConvOp::IntToPtr, Type::Ptr, vp);
    let e = memeq(b, at, val);
    b.br(out, &[e]);
    b.switch_to(out);
    let (eq, valid) = (b.block_params(out)[0], b.icmp(IntCond::Ne, vl, none));
    let v = if ne { let t = b.bconst(true); b.int_bin(IntBinOp::Xor, eq, t) } else { eq };
    let v = b.int_bin(IntBinOp::And, v, valid);
    (hot, word(b, v), valid)
}

/// D476: `->> key = ANY(arr)` (`<> ALL` when `ne`) over the image at `p` (`keys` the path), `arr` the array operand's
/// word: `(hot, value word, valid)`; host `field_any` walks the elements.
pub(crate) fn any(b: &mut FunctionBuilder<'_>, p: Value, null: Value, keys: &[Box<[u8]>], (arr, aok): (Value, Value), align: i8, ne: bool) -> (Value, Value, Value) {
    let (hot, vp, vl) = entry(b, p, null, keys);
    let k = |b: &mut FunctionBuilder<'_>, v: i64| b.iconst(Type::I64, v);
    let (call, out, cold, two) = (b.create_block(&[]), b.create_block(&[Type::I64]), k(b, -1), k(b, 2));
    // a NULL array (a later execution's bind) is NULL, never walked
    let (go, r0) = (b.int_bin(IntBinOp::And, hot, aok), b.select(hot, two, cold));
    b.cond_br(go, (call, &[]), (out, &[r0]));
    b.switch_to(call);
    let h = b.declare_host_once("jsonb_field_any", &[Type::I64; 4], Some(Type::I64), field_any as *const () as usize, HostFlags { touches_memory: true, pure: true, ..HostFlags::default() });
    let a = k(b, align as i64);
    let r = b.call_host(h, &[vp, vl, arr, a], Some(Type::I64)).expect("returns");
    b.br(out, &[r]);
    b.switch_to(out);
    let r = b.block_params(out)[0];
    let [zero, one, two, three] = [0, 1, 2, 3].map(|v| k(b, v));
    let (hot, valid) = (b.icmp(IntCond::Sge, r, zero), b.icmp(IntCond::Ne, r, two));
    // PG's ExecEvalScalarArrayOp: an empty array answers !useOr whatever the operand; else a NULL operand or no match
    // over a NULL element is NULL
    let v = match ne {
        true => { let (x, y) = (b.icmp(IntCond::Eq, r, zero), b.icmp(IntCond::Eq, r, three)); b.int_bin(IntBinOp::Or, x, y) }
        false => b.icmp(IntCond::Eq, r, one),
    };
    (hot, word(b, v), valid)
}

/// D564: `any` over the elements `entry` decoded into `words` (status 1, bit 2 a NULL element; another status, the host
/// walk): the key's value compared with each, length first, in IR.
fn any_words(b: &mut FunctionBuilder<'_>, env: &mut Env, p: Value, null: Value, keys: &[Box<[u8]>], arr: (Value, Value), (align, ne, words): (i8, bool, &[i32])) -> (Value, Value, Value) {
    let (out, host, inl) = (b.create_block(&[Type::Bool, Type::I64, Type::Bool]), b.create_block(&[]), b.create_block(&[]));
    let st = env.load_word(b, words[0]);
    let k = |b: &mut FunctionBuilder<'_>, v: i64| b.iconst(Type::I64, v);
    let (one, two, none) = (k(b, 1), k(b, 2), k(b, -1));
    let ok = { let x = b.int_bin(IntBinOp::And, st, one); b.icmp(IntCond::Ne, x, env.zero) };
    b.cond_br(ok, (inl, &[]), (host, &[]));
    b.switch_to(host);
    let (h, v, valid) = any(b, p, null, keys, arr, align, ne);
    b.br(out, &[h, v, valid]);
    b.switch_to(inl);
    let (hot, vp, vl) = entry(b, p, null, keys);
    let (t, f) = (b.bconst(true), b.bconst(false));
    let n = (words.len() - 1) / 2;
    if n == 0 {
        // PG: an empty array answers !useOr whatever the operand
        let v = word(b, if ne { t } else { f });
        b.br(out, &[hot, v, t]);
    } else {
        let (hit, miss) = (b.create_block(&[]), b.create_block(&[]));
        let nul = b.icmp(IntCond::Eq, vl, none);
        let first = b.create_block(&[]);
        b.cond_br(nul, (out, &[hot, env.zero, f]), (first, &[]));
        b.switch_to(first);
        for i in 0..n {
            let (bytes, next) = (b.create_block(&[]), if i + 1 < n { b.create_block(&[]) } else { miss });
            let (ep, el) = (env.load_word(b, words[1 + 2 * i]), env.load_word(b, words[2 + 2 * i]));
            let same = b.icmp(IntCond::Eq, el, vl);
            b.cond_br(same, (bytes, &[]), (next, &[]));
            b.switch_to(bytes);
            let e = crate::rep::varlena::cmp_text::bytes_eq(b, env, vp, ep, vl);
            b.cond_br(e, (hit, &[]), (next, &[]));
            if i + 1 < n {
                b.switch_to(next);
            }
        }
        b.switch_to(hit);
        let v = word(b, if ne { f } else { t });
        b.br(out, &[hot, v, t]);
        // no match: NULL over a NULL element, else = ANY false / <> ALL true
        b.switch_to(miss);
        let nulls = { let x = b.int_bin(IntBinOp::And, st, two); b.icmp(IntCond::Ne, x, env.zero) };
        let valid = b.int_bin(IntBinOp::Xor, nulls, t);
        let v = if ne { valid } else { f };
        let v = word(b, v);
        b.br(out, &[hot, v, valid]);
    }
    b.switch_to(out);
    let d = b.block_params(out);
    (d[0], d[1], d[2])
}

/// Node `i` of `c` as this road's claim: `texteq` / `textne` of a constant path's text (`field`) and a text constant.
fn claim(c: &Compiled<'_>, i: usize) -> Option<(Field, u32)> {
    let (ExprNode::Op(n) | ExprNode::Func(n)) = &c.prog.nodes[i] else { return None };
    let call = &c.prog.sites[n.site.0 as usize];
    let Some(ne) = gravity_fn::typesem::pgrc::text_eq_ne(call.funcid) else { return None };
    if call.args.len != 2 || !deterministic(call.collation) {
        return None;
    }
    let (a, b) = (c.prog.kids[call.args.start as usize], c.prog.kids[call.args.start as usize + 1]);
    // the constant is read (a logged bake: its image makes a V4R-18 variant) only once the other side is a field
    let ((col, keys), val) = match field(c, a) {
        Some(f) => (f, const_bytes(c, b)?),
        None => (field(c, b)?, const_bytes(c, a)?),
    };
    Some((Field { col, keys, val: Val::Lit(val.into()), ne }, 16))
}

/// Node `id` of `c` as a D476 site: a constant path's text (`field`) `= ANY` / `<> ALL` a non-NULL constant text array,
/// its operand index set by `attach`. The array is not read.
fn any_field(c: &Compiled<'_>, id: gravity_expr::NodeId) -> Option<Field> {
    let ExprNode::ScalarArrayOp(s) = &c.prog.nodes[id.0 as usize] else { return None };
    let call = &c.prog.sites[s.site.0 as usize];
    // `= ANY` is `=` under OR, `<> ALL` is `<>` under AND
    let ne = gravity_fn::typesem::pgrc::text_eq_ne(call.funcid);
    if ne != Some(!s.use_or) || call.args.len != 2 || !deterministic(call.collation) || s.elem.typ != TEXT_OID || s.elem.len != -1 || s.elem.byval {
        return None;
    }
    let (a, k) = (c.prog.kids[call.args.start as usize], c.prog.kids[call.args.start as usize + 1]);
    let ExprNode::Const(arr) = &c.prog.nodes[k.0 as usize] else { return None };
    if arr.is_null() || arr.byval {
        return None;
    }
    let (col, keys) = field(c, a)?;
    Some(Field { col, keys, val: Val::Any { sub: 0, align: s.elem.align, words: Vec::new() }, ne: !s.use_or })
}

/// The mechanism of road `jsonb.inplace`.
pub(crate) struct Jsonb;
pub(crate) static JSONB: Jsonb = Jsonb;

impl ExprExtMech for Jsonb {
    fn attach<'p>(&self, px: &mut Px<'_, 'p>, at: (usize, usize), node: Node<'p>, c: &mut Compiled<'p>, ex: ExId, sub: &mut dyn FnMut(Node<'p>) -> u32) -> Option<ExtClaim> {
        // D476: the `= ANY` / `<> ALL` sites under the root's AND / OR / NOT (the plan tree and the program walked together,
        // cmp_text.rs's way), each array compiled as an operand of its own; the CASE's cold arm keeps the call
        let (mut stack, mut anys) = (mcx::pg_vec![in px.mcx(); (c.root, node)], mcx::PgVec::new_in(px.mcx()));
        while let Some((id, n)) = stack.pop() {
            match (&c.prog.nodes[id.0 as usize], n.as_bool_expr()) {
                (ExprNode::Bool(b), Some(e)) if e.args.len() == b.args.len as usize => stack.extend_in(c.children(b.args).iter().copied().zip(e.args.iter())),
                (ExprNode::Bool(_), _) => {}
                _ => {
                    let arr = n.as_scalar_array_op_expr().and_then(|s| s.args.as_slice().get(1).copied());
                    if let (Some(Field { col, keys, val: Val::Any { align, .. }, ne }), Some(a)) = (any_field(c, id), arr) {
                        // D564: 1 + 2n frame words, n the list's count (its class, V4R-110: not a bake)
                        let n = list_len(c, id).filter(|&n| n <= MAX_WORDS_LIST && at.0 != usize::MAX && (anys.len() + 1) * (1 + 2 * MAX_WORDS_LIST) <= 256);
                        let base = anys.len() * (1 + 2 * MAX_WORDS_LIST);
                        let words = n.map_or_else(Vec::new, |n| (0..1 + 2 * n).map(|i| px.frame_word(WordKey::Open { face: Face::Step, k: at.0, j: crate::frame::px::key_j(at.1) ^ ((ex.0 as u64 + 1) << 40), i: (base + i) as u8 }) as i32 * 8).collect()); // alloc: Program
                        anys.push_in((id.0 as usize, Field { col, keys, val: Val::Any { sub: sub(a), align, words }, ne }));
                    }
                }
            }
        }
        claim_calls(c, |c, i| {
            let r = claim(c, i).or_else(|| anys.iter().position(|a| a.0 == i).map(|j| (anys.swap_remove(j).1, 16)));
            if r.is_some() {
                fires::fire("jsonb.inplace"); // shapecheck fire signal (REGISTRY STEP-12 / NEW-lowerv4-30 / a2-244..250)
            }
            r
        })
    }

    fn emit(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, ex: ExId, s: u32, sub: SubEmit<'_>) -> [Value; 3] {
        site(b, env, ex, s, |b, env, f: &Field| {
            let [p, _, hot, null] = view(b, env, row, f.col);
            let (ok, v, valid) = match f.val {
                Val::Lit(ref val) => compare(b, p, null, &f.keys, val, f.ne),
                Val::Any { sub: i, align, ref words } => {
                    let [arr, aok, _] = sub(b, env, i);
                    match words.is_empty() {
                        true => any(b, p, null, &f.keys, (arr, aok), align, f.ne),
                        false => any_words(b, env, p, null, &f.keys, (arr, aok), (align, f.ne, words)),
                    }
                }
            };
            [b.int_bin(IntBinOp::And, hot, ok), v, valid]
        })
    }

    /// D564: each decoded site's array (its bind word) into its frame words once per call; a NULL array writes nothing
    /// (status 0: the host walk, which answers NULL).
    fn entry(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, ex: ExId) {
        let Some(x) = ExtPlan::of(env.placed(ex)) else { return };
        let (subs, plan) = (mcx::to_vec_in(&x.subs.borrow()[..], env.mcx), x.plan.clone());
        for f in plan.downcast_ref::<crate::step::text_like::Plan<Field>>().into_iter().flat_map(|p| p.claims.iter()) {
            let Val::Any { sub, align, ref words } = f.val else { continue };
            let Some(&gravity_lower::Place::Word(w)) = subs.get(sub as usize).and_then(|&id| env.placed(id).places.first()) else { continue };
            if words.is_empty() {
                continue;
            }
            let (p, nb) = (b.load(Type::I64, env.params, w as i32 * 8), b.load_zext(Type::I8, env.params_null, w as i32));
            let (go, done) = (b.create_block(&[]), b.create_block(&[]));
            let ok = b.icmp(IntCond::Eq, nb, env.zero);
            b.cond_br(ok, (go, &[]), (done, &[]));
            b.switch_to(go);
            let sl = b.declare_stack_slot(8 * words.len() as u32, 8);
            let a = { let a = b.stack_addr(sl); b.conv(ConvOp::PtrToInt, Type::I64, a) };
            let h = b.declare_host_once("jsonb_field_any_decode", &[Type::I64; 4], None, field_any_decode as *const () as usize, HostFlags { touches_memory: true, ..HostFlags::default() });
            let (al, n) = (b.iconst(Type::I64, align as i64), b.iconst(Type::I64, (words.len() as i64 - 1) / 2));
            let _ = b.call_host(h, &[p, al, n, a], None);
            let sa = b.stack_addr(sl);
            for (i, &off) in words.iter().enumerate() {
                let v = b.load(Type::I64, sa, 8 * i as i32);
                env.store_word(b, off, v);
            }
            b.br(done, &[]);
            b.switch_to(done);
        }
    }
}

/// The element count of site `id`'s constant array: its class (V4R-110, konst.rs `list_count`), not a bake.
fn list_len(c: &Compiled<'_>, id: gravity_expr::NodeId) -> Option<usize> {
    let ExprNode::ScalarArrayOp(s) = &c.prog.nodes[id.0 as usize] else { return None };
    let k = c.prog.kids[c.prog.sites[s.site.0 as usize].args.start as usize + 1];
    let ExprNode::Const(arr) = &c.prog.nodes[k.0 as usize] else { return None };
    arr.list_count(s.elem)
}

#[cfg(test)]
mod tests {
    use gravity_ir::{IntBinOp, Type};
    use mcx::GrowIn;

    use crate::step::text_like::tests::{long, run, short};
    use super::{any, compare, Field, Val};

    /// A jsonb value entry: a string, `true` / `false`, `null`, a number (its bytes are never read here), a nested object,
    /// or the array `["x"]`.
    #[derive(Clone, Copy)]
    enum J {
        S(&'static str),
        B(bool),
        Null,
        Num,
        O(&'static [(&'static str, J)]),
        Arr,
    }

    /// PG's binary image of the object `pairs` (keys unique): the keys sorted length first, entry 0 storing its end
    /// offset (`JENTRY_HAS_OFF`), every other entry its length; a number's or a container's entry padded to 4 as PG pads
    /// it (the padding is part of the entry's length).
    fn object(pairs: &[(&str, J)]) -> Vec<u8> {
        let mut ps = pairs.to_vec();
        ps.sort_by(|a, b| (a.0.len(), a.0).cmp(&(b.0.len(), b.0)));
        let (mut meta, mut data) = (Vec::new(), Vec::new());
        let mut push = |ty: u32, bytes: &[u8], pad: bool, meta: &mut Vec<u32>| {
            let start = data.len();
            if pad {
                data.resize(data.len().next_multiple_of(4), 0);
            }
            data.extend_from_slice(bytes);
            let len = (data.len() - start) as u32;
            meta.push(if meta.is_empty() { 0x8000_0000 | ty | data.len() as u32 } else { ty | len });
        };
        for (k, _) in &ps {
            push(0, k.as_bytes(), false, &mut meta);
        }
        for (_, v) in &ps {
            match v {
                J::S(s) => push(0, s.as_bytes(), false, &mut meta),
                J::B(t) => push(if *t { 0x3000_0000 } else { 0x2000_0000 }, &[], false, &mut meta),
                J::Null => push(0x4000_0000, &[], false, &mut meta),
                J::Num => push(0x1000_0000, &[0x0c, 0x80, 1, 0], true, &mut meta),
                J::O(ps) => push(0x5000_0000, &object(ps), true, &mut meta),
                J::Arr => push(0x5000_0000, &[&0x4000_0001u32.to_le_bytes()[..], &0x8000_0001u32.to_le_bytes(), b"x"].concat(), true, &mut meta),
            }
        }
        let mut out = (0x2000_0000u32 | ps.len() as u32).to_le_bytes().to_vec();
        meta.iter().for_each(|m| out.extend_from_slice(&m.to_le_bytes()));
        out.extend(data);
        out
    }

    /// A path's keys.
    fn keys(path: &[&str]) -> Vec<Box<[u8]>> {
        path.iter().map(|k| k.as_bytes().into()).collect()
    }

    /// `img ->> key = val` (`<>` when `ne`): `None` cold, `Some(None)` NULL, else the verdict.
    fn eq(img: Option<&[u8]>, key: &str, val: &str, ne: bool) -> Option<Option<bool>> {
        at(img, &[key], val, ne)
    }

    /// `img #>> path = val` (`<>` when `ne`): `None` cold, `Some(None)` NULL, else the verdict.
    fn at(img: Option<&[u8]>, path: &[&str], val: &str, ne: bool) -> Option<Option<bool>> {
        let path = keys(path);
        let [hot, v] = run(img, &|b, [p, _, hot], null| {
            let (ok, v, valid) = compare(b, p, null, &path, val.as_bytes(), ne);
            let hot = b.int_bin(IntBinOp::And, hot, ok);
            let nul = b.iconst(Type::I64, 2);
            let v = b.select(valid, v, nul);
            [crate::step::text_like::word(b, hot), v]
        });
        (hot == 1).then_some((v != 2).then_some(v == 1))
    }

    /// Present, absent, NULL, boolean, unicode and longer-than-any keys, both header forms, `=` and `<>`; a number, a
    /// non-object root, more than 16 pairs, or a compressed image runs cold.
    #[test]
    fn a_field_compare_reads_the_image_as_pg() {
        let o = object(&[("id", J::S("42")), ("name", J::S("alice")), ("a", J::S("")), ("flag", J::B(true)), ("off", J::B(false)), ("gone", J::Null), ("n", J::Num), ("ключ", J::S("значение"))]);
        for img in [short(&o), long(&o)] {
            let img = Some(&img[..]);
            assert_eq!(eq(img, "name", "alice", false), Some(Some(true)));
            assert_eq!(eq(img, "name", "alic", false), Some(Some(false)));
            assert_eq!(eq(img, "name", "alice", true), Some(Some(false)));
            assert_eq!(eq(img, "id", "42", false), Some(Some(true)));
            assert_eq!(eq(img, "a", "", false), Some(Some(true)));
            assert_eq!(eq(img, "flag", "true", false), Some(Some(true)));
            assert_eq!(eq(img, "flag", "false", false), Some(Some(false)));
            assert_eq!(eq(img, "off", "false", false), Some(Some(true)));
            assert_eq!(eq(img, "gone", "x", false), Some(None), "a JSON null is NULL");
            assert_eq!(eq(img, "gone", "x", true), Some(None));
            assert_eq!(eq(img, "missing", "x", false), Some(None), "a missing key is NULL");
            assert_eq!(eq(img, "a_key_longer_than_any", "x", false), Some(None));
            assert_eq!(eq(img, "", "x", false), Some(None));
            assert_eq!(eq(img, "ключ", "значение", false), Some(Some(true)));
            assert_eq!(eq(img, "n", "1", false), None, "a number renders through PG's call");
        }
        assert_eq!(eq(None, "id", "42", false), Some(None));
        let arr = [0x4000_0001u32.to_le_bytes(), 0x8000_0001u32.to_le_bytes()].concat();
        assert_eq!(eq(Some(&long(&[&arr[..], b"x"].concat())), "id", "x", false), None, "an array root runs cold");
        let keys: Vec<String> = (0..17).map(|i| format!("k{i:02}")).collect();
        let many: Vec<(&str, J)> = keys.iter().map(|k| (k.as_str(), J::S("v"))).collect();
        assert_eq!(eq(Some(&long(&object(&many))), "k16", "v", false), None, "17 pairs run cold");
        assert_eq!(eq(Some(&long(&object(&many[..16]))), "k15", "v", false), Some(Some(true)));
        let compressed = [&((40u32 << 2) | 2).to_le_bytes()[..], &[0; 36]].concat();
        assert_eq!(eq(Some(&compressed), "id", "42", false), None);
    }

    /// STEP-12: a path walks one object per key as PG's jsonb_get_element: nested strings, booleans and JSON nulls, a
    /// missing key at any level, an empty object, a scalar (a JSON null too) above the last key (NULL), values padded
    /// behind numbers and containers; an array level or more than 16 pairs at a deeper level, and a number or a
    /// container at the last key, run cold.
    #[test]
    fn a_path_compare_reads_each_level_as_pg() {
        const DEEP: &[(&str, J)] = &[("x", J::S("y")), ("ключ", J::S("знач"))];
        const INNER: &[(&str, J)] = &[("s", J::S("v")), ("t", J::B(true)), ("z", J::Null), ("n", J::Num), ("deep", J::O(DEEP))];
        let o = object(&[("n", J::Num), ("a", J::O(INNER)), ("b", J::S("top")), ("nul", J::Null), ("arr", J::Arr), ("e", J::O(&[]))]);
        for img in [short(&o), long(&o)] {
            let img = Some(&img[..]);
            assert_eq!(at(img, &["a", "s"], "v", false), Some(Some(true)));
            assert_eq!(at(img, &["a", "s"], "v", true), Some(Some(false)));
            assert_eq!(at(img, &["a", "s"], "w", false), Some(Some(false)));
            assert_eq!(at(img, &["a", "t"], "true", false), Some(Some(true)), "a boolean is its text");
            assert_eq!(at(img, &["a", "z"], "x", false), Some(None), "a JSON null at the last key");
            assert_eq!(at(img, &["a", "deep", "x"], "y", false), Some(Some(true)));
            assert_eq!(at(img, &["a", "deep", "ключ"], "знач", true), Some(Some(false)));
            assert_eq!(at(img, &["a", "missing"], "v", false), Some(None), "a missing key at a deeper level");
            assert_eq!(at(img, &["a", "deep", "missing", "x"], "v", false), Some(None));
            assert_eq!(at(img, &["missing", "s"], "v", false), Some(None));
            assert_eq!(at(img, &["b", "s"], "v", false), Some(None), "a string above the last key has no field");
            assert_eq!(at(img, &["nul", "s"], "v", true), Some(None), "a JSON null above the last key");
            assert_eq!(at(img, &["a", "n", "x"], "v", false), Some(None), "a number above the last key");
            assert_eq!(at(img, &["a", "t", "x"], "v", false), Some(None));
            assert_eq!(at(img, &["e", "s"], "v", false), Some(None), "an empty object");
            assert_eq!(at(img, &["b"], "top", false), Some(Some(true)));
            assert_eq!(at(img, &["a", "n"], "1", false), None, "a number at the last key renders through PG's call");
            assert_eq!(at(img, &["a", "deep"], "{}", false), None, "so does a container");
            assert_eq!(at(img, &["arr", "0"], "x", false), None, "an array level is cold (PG reads an index)");
        }
        assert_eq!(at(None, &["a", "s"], "v", false), Some(None), "a NULL column");
        let many: Vec<(&'static str, J)> = (0..17).map(|i| (&*Box::leak(format!("k{i:02}").into_boxed_str()), J::S("v"))).collect();
        let many: &'static [(&'static str, J)] = Box::leak(many.into_boxed_slice());
        let wide = long(&object(&[("w", J::O(many)), ("x", J::O(&many[..16]))]));
        assert_eq!(at(Some(&wide), &["w", "k16"], "v", false), None, "17 pairs at a deeper level run cold");
        assert_eq!(at(Some(&wide), &["x", "k15"], "v", false), Some(Some(true)));
        assert_eq!(at(Some(&wide), &["x", "k16"], "v", false), Some(None));
    }

    /// `texteq` / `textne` over `->>` with constant key and value is claimed, either side; `->>` alone, a `->>` over a
    /// non-constant key, and a compare of two fields are left alone (G4). STEP-12: a constant path of 1..=4 keys too
    /// (`#>>`, a `->` chain, a relabel), and a path it does not walk is not read.
    #[test]
    fn claims_only_a_fused_field_compare() {
        use gravity_expr::nodes::func::{Call, FuncNode, How};
        use gravity_expr::nodes::konst::ConstNode;
        use gravity_expr::nodes::var::Var;
        use gravity_expr::{ExprNode, Kids, NodeId, Program, SiteId, State};
        enum A { Col, Text(&'static str), Path(&'static [Option<&'static str>]), Relabel(Box<A>), Call(u32, Vec<A>) }
        fn add(p: &mut Program<'static>, a: &A) -> NodeId {
            let n = match a {
                A::Col => ExprNode::Var(Var { src: gravity_expr::Source::Scan, attno: 1, typ: 3802, desc: 0 }),
                A::Text(s) => {
                    let img: &'static [u8] = Box::leak(long(s.as_bytes()).into_boxed_slice());
                    ExprNode::Const(ConstNode::new(gravity_expr::NullableDatum::value(datum::Datum::from_usize(img.as_ptr() as usize)), 25, -1, false))
                }
                A::Path(elems) => {
                    let img: &'static [u8] = Box::leak(array(elems).into_boxed_slice());
                    ExprNode::Const(ConstNode::new(gravity_expr::NullableDatum::value(datum::Datum::from_usize(img.as_ptr() as usize)), 1009, -1, false))
                }
                A::Relabel(a) => ExprNode::Relabel(gravity_expr::nodes::cast::Relabel { arg: add(p, a), canon: gravity_expr::nodes::cast::Canon::Keep }),
                A::Call(f, args) => {
                    let ids: Vec<NodeId> = args.iter().map(|x| add(p, x)).collect();
                    let start = p.kids.len() as u32;
                    p.kids.extend_in(ids);
                    p.sites.push_in(Call { funcid: *f, args: Kids { start, len: args.len() as u32 }, nargs: args.len() as u8, strict: true, collation: 950, how: How::Fmgr(0), rettype: 25, volatile: false });
                    ExprNode::Func(FuncNode { site: SiteId(p.sites.len() as u32 - 1) })
                }
            };
            p.nodes.push_in(n);
            NodeId(p.nodes.len() as u32 - 1)
        }
        let claimed = |a: A| {
            let mut prog = Program::empty_in(crate::step::expr::test_lists());
            let root = add(&mut prog, &a);
            let mut c = gravity_expr::Compiled { prog, state: State::empty(), root, params: types_portal::params::ParamBind::NONE, callback_arg: core::ptr::null_mut(), result_type: 16 };
            let plan = crate::step::text_like::claim_calls(&mut c, super::claim)?.plan;
            let p = plan.downcast_ref::<crate::step::text_like::Plan<Field>>().expect("the plan");
            let Val::Lit(v) = &p.claims[0].val else { panic!("a literal claim") };
            let path: Vec<String> = p.claims[0].keys.iter().map(|k| String::from_utf8(k.to_vec()).unwrap()).collect();
            Some((path.join("/"), String::from_utf8(v.to_vec()).unwrap(), p.claims[0].ne))
        };
        let field = |k: &'static str| A::Call(3214, vec![A::Col, A::Text(k)]);
        assert_eq!(claimed(A::Call(67, vec![field("name"), A::Text("alice")])), Some(("name".into(), "alice".into(), false)));
        assert_eq!(claimed(A::Call(157, vec![A::Text("alice"), field("name")])), Some(("name".into(), "alice".into(), true)));
        assert_eq!(claimed(field("name")), None, "->> alone");
        assert_eq!(claimed(A::Call(67, vec![A::Call(3214, vec![A::Col, A::Call(3214, vec![A::Col, A::Text("k")])]), A::Text("v")])), None, "a non-constant key");
        assert_eq!(claimed(A::Call(67, vec![field("a"), field("b")])), None, "two fields");
        // STEP-12: a `->` chain ending in `->>` or `#>>`, a `#>>` path, through a relabel; 1..=4 keys, none NULL
        let chain = |ks: &[&'static str], last: A| {
            let arrows = ks.iter().fold(A::Col, |a, &k| A::Call(3478, vec![a, A::Text(k)]));
            A::Call(if matches!(last, A::Path(_)) { 3940 } else { 3214 }, vec![arrows, last])
        };
        let v = || A::Text("v");
        assert_eq!(claimed(A::Call(67, vec![chain(&["a"], A::Text("b")), v()])), Some(("a/b".into(), "v".into(), false)));
        assert_eq!(claimed(A::Call(67, vec![chain(&["a", "b", "c"], A::Text("d")), v()])), Some(("a/b/c/d".into(), "v".into(), false)));
        assert_eq!(claimed(A::Call(157, vec![v(), A::Call(3940, vec![A::Col, A::Path(&[Some("a"), Some("ключ")])])])), Some(("a/ключ".into(), "v".into(), true)));
        assert_eq!(claimed(A::Call(67, vec![A::Call(3940, vec![A::Col, A::Path(&[Some("a"), Some("b"), Some("c"), Some("d")])]), v()])), Some(("a/b/c/d".into(), "v".into(), false)));
        assert_eq!(claimed(A::Call(67, vec![chain(&["a"], A::Path(&[Some("b"), Some("c")])), v()])), Some(("a/b/c".into(), "v".into(), false)), "-> then #>>");
        assert_eq!(claimed(A::Call(67, vec![A::Relabel(Box::new(field("name"))), v()])), Some(("name".into(), "v".into(), false)), "through a relabel");
        assert_eq!(claimed(A::Call(67, vec![A::Call(3940, vec![A::Col, A::Path(&[Some("a"), None])]), v()])), None, "a NULL path element");
        assert_eq!(claimed(A::Call(67, vec![A::Call(3940, vec![A::Col, A::Col]), v()])), None, "a path not a constant");
        assert_eq!(claimed(A::Call(67, vec![A::Call(3214, vec![A::Call(3215, vec![A::Col, A::Text("0")]), A::Text("b")]), v()])), None, "an array element step");
        assert_eq!(claimed(A::Call(67, vec![A::Call(3217, vec![A::Col, A::Path(&[Some("a")])]), v()])), None, "#> is jsonb");
        // a path refused for its count is read (baked) first: no key holds its count, so the variant is the literal's
        // (V4R-18: a lowering's bakes are a function of its key and the literals it logged)
        for (path, why) in [(A::Path(&[]), "an empty path is the whole document"), (A::Path(&[Some("a"), Some("b"), Some("c"), Some("d"), Some("e")]), "5 keys")] {
            let d = gravity_expr::bake::Scope::open();
            assert_eq!(claimed(A::Call(67, vec![A::Call(3940, vec![A::Col, path]), v()])), None, "{why}");
            assert_eq!(d.reads().len(), 1, "{why}: the path is read");
            drop(d);
        }
        assert_eq!(claimed(A::Call(67, vec![chain(&["a", "b", "c"], A::Path(&[Some("d"), Some("e")])), v()])), None, "3 + 2 keys");
        // a chain past 4 keys is refused by its shape, before a key is read
        let d = gravity_expr::bake::Scope::open();
        assert_eq!(claimed(A::Call(67, vec![chain(&["a", "b", "c", "d"], A::Text("e")), v()])), None, "a chain of 5 keys");
        assert_eq!(claimed(A::Call(67, vec![chain(&["a", "b", "c", "d"], A::Path(&[Some("e")])), v()])), None, "4 + 1 keys");
        // a text = const of no field reads (bakes) no literal: V4R-18's front key keeps one variant (m1_front, e3)
        assert_eq!(claimed(A::Call(67, vec![A::Col, A::Text("b1401")])), None, "text = const");
        assert_eq!(d.reads(), Vec::<usize>::new(), "no literal baked by an unclaimed compare");
        drop(d);
    }

    /// A one-dimensional text[] of `elems` (`None`: NULL), PG's layout: header, ndim, dataoffset, elemtype, dim, lbound,
    /// the null bitmap when one is NULL, then the 4-byte-header elements aligned 4.
    fn array(elems: &[Option<&str>]) -> Vec<u8> {
        let nulls = elems.iter().any(|e| e.is_none());
        let data = if nulls { (24 + elems.len().div_ceil(8)).next_multiple_of(8) } else { 24 };
        let dims: &[u32] = if elems.is_empty() { &[] } else { &[elems.len() as u32, 1] };
        let mut arr: Vec<u8> = [0u32, dims.len() as u32 / 2, if nulls { data as u32 } else { 0 }, 25].iter().chain(dims).flat_map(|w| w.to_le_bytes()).collect();
        if nulls {
            arr.extend((0..elems.len().div_ceil(8)).map(|i| (0..8).filter(|j| elems.get(i * 8 + j).is_some_and(|e| e.is_some())).fold(0u8, |a, j| a | 1 << j)));
        }
        arr.resize(if elems.is_empty() { 16 } else { data }, 0);
        for e in elems.iter().flatten() {
            arr.resize(arr.len().next_multiple_of(4), 0);
            arr.extend_from_slice(&long(e.as_bytes()));
        }
        let n = arr.len() as u32;
        arr[..4].copy_from_slice(&(n << 2).to_le_bytes());
        arr
    }

    /// `img ->> key = ANY(arr)` (`<> ALL` when `ne`): `None` cold, `Some(None)` NULL, else the verdict.
    fn in_list(img: Option<&[u8]>, key: &str, arr: &[u8], ne: bool) -> Option<Option<bool>> {
        let [hot, v] = run(img, &|b, [p, _, hot], null| {
            let a = b.iconst(Type::I64, arr.as_ptr() as i64);
            let t = b.bconst(true);
            let (ok, v, valid) = any(b, p, null, &keys(&[key]), (a, t), b'i' as i8, ne);
            let hot = b.int_bin(IntBinOp::And, hot, ok);
            let nul = b.iconst(Type::I64, 2);
            let v = b.select(valid, v, nul);
            [crate::step::text_like::word(b, hot), v]
        });
        (hot == 1).then_some((v != 2).then_some(v == 1))
    }

    /// D476: `->> 'k' IN (..)` / `NOT IN (..)` as PG's ExecEvalScalarArrayOp: a match, no match, a NULL element (NULL
    /// unless matched), a NULL operand (a missing key, a JSON null, a NULL column: NULL, but an empty array's answer is
    /// !useOr), a boolean as its text; a number or a non-plain array runs cold.
    #[test]
    fn a_field_in_list_reads_the_image_as_pg() {
        let o = object(&[("k", J::S("gamma")), ("flag", J::B(true)), ("gone", J::Null), ("n", J::Num), ("e", J::S(""))]);
        let (gb, gn, empty, en) = (array(&[Some("gamma"), Some("beta")]), array(&[Some("beta"), None]), array(&[]), array(&[Some("gamma"), None]));
        for img in [short(&o), long(&o)] {
            let img = Some(&img[..]);
            assert_eq!(in_list(img, "k", &gb, false), Some(Some(true)));
            assert_eq!(in_list(img, "k", &gb, true), Some(Some(false)));
            assert_eq!(in_list(img, "k", &array(&[Some("beta"), Some("gam")]), false), Some(Some(false)));
            assert_eq!(in_list(img, "k", &array(&[Some("beta")]), true), Some(Some(true)));
            assert_eq!(in_list(img, "k", &gn, false), Some(None), "no match over a NULL element is NULL");
            assert_eq!(in_list(img, "k", &gn, true), Some(None));
            assert_eq!(in_list(img, "k", &en, false), Some(Some(true)), "a match over a NULL element");
            assert_eq!(in_list(img, "k", &en, true), Some(Some(false)));
            assert_eq!(in_list(img, "k", &empty, false), Some(Some(false)));
            assert_eq!(in_list(img, "k", &empty, true), Some(Some(true)));
            assert_eq!(in_list(img, "missing", &gb, false), Some(None), "a missing key is NULL");
            assert_eq!(in_list(img, "missing", &empty, false), Some(Some(false)), "an empty array answers !useOr");
            assert_eq!(in_list(img, "missing", &empty, true), Some(Some(true)));
            assert_eq!(in_list(img, "gone", &gb, true), Some(None), "a JSON null is NULL");
            assert_eq!(in_list(img, "flag", &array(&[Some("true")]), false), Some(Some(true)), "a boolean is its text");
            assert_eq!(in_list(img, "flag", &array(&[Some("false")]), false), Some(Some(false)));
            assert_eq!(in_list(img, "e", &array(&[Some("")]), false), Some(Some(true)));
            assert_eq!(in_list(img, "n", &gb, false), None, "a number renders through PG's call");
        }
        assert_eq!(in_list(None, "k", &gb, false), Some(None), "a NULL column");
        assert_eq!(in_list(None, "k", &empty, true), Some(Some(true)));
        let short_arr = [&[((3u8 + 1) << 1) | 1][..], b"abc"].concat();
        assert_eq!(in_list(Some(&long(&o)), "k", &short_arr, false), None, "an array image not plain runs cold");
    }

    /// D476: a `->>` `= ANY` / `<> ALL` of a constant text array is a site; the array is not read (no bake), and `= ALL`,
    /// `<> ANY` and a non-text array are not.
    #[test]
    fn claims_a_field_in_list_without_reading_it() {
        use gravity_expr::nodes::array::ElemFacts;
        use gravity_expr::nodes::func::{Call, How};
        use gravity_expr::nodes::konst::ConstNode;
        use gravity_expr::nodes::op::ScalarArrayOp;
        use gravity_expr::nodes::var::Var;
        use gravity_expr::{ExprNode, Kids, NodeId, Program, SiteId};
        let arr: &'static [u8] = Box::leak(array(&[Some("gamma"), Some("beta")]).into_boxed_slice());
        let key: &'static [u8] = Box::leak(long(b"k").into_boxed_slice());
        let site = |funcid: u32, use_or: bool, elem: u32| {
            let mut p = Program::empty_in(crate::step::expr::test_lists());
            let konst = |img: &'static [u8], typ| ExprNode::Const(ConstNode::new(gravity_expr::NullableDatum::value(datum::Datum::from_usize(img.as_ptr() as usize)), typ, -1, false));
            p.nodes.extend_in([ExprNode::Var(Var { src: gravity_expr::Source::Scan, attno: 1, typ: 3802, desc: 0 }), konst(key, 25)]);
            p.kids.extend_in([NodeId(0), NodeId(1)]);
            p.sites.push_in(Call { funcid: 3214, args: Kids { start: 0, len: 2 }, nargs: 2, strict: true, collation: 100, how: How::Fmgr(0), rettype: 25, volatile: false });
            p.nodes.push_in(ExprNode::Func(gravity_expr::nodes::func::FuncNode { site: SiteId(0) }));
            p.nodes.push_in(konst(arr, 1009));
            p.kids.extend_in([NodeId(2), NodeId(3)]);
            p.sites.push_in(Call { funcid, args: Kids { start: 2, len: 2 }, nargs: 2, strict: true, collation: 100, how: How::Fmgr(0), rettype: 16, volatile: false });
            p.nodes.push_in(ExprNode::ScalarArrayOp(ScalarArrayOp { site: SiteId(1), use_or, elem: ElemFacts { typ: elem, len: -1, byval: false, align: b'i' as i8 } }));
            let c = gravity_expr::Compiled { prog: p, state: gravity_expr::State::empty(), root: NodeId(4), params: types_portal::params::ParamBind::NONE, callback_arg: core::ptr::null_mut(), result_type: 16 };
            let d = gravity_expr::bake::Scope::open();
            let f = super::any_field(&c, NodeId(4)).map(|f| (f.keys.concat(), matches!(f.val, Val::Any { sub: 0, align, .. } if align == b'i' as i8), f.ne));
            let baked = d.reads();
            drop(d);
            assert!(!baked.contains(&(arr.as_ptr() as usize)), "the array is not read");
            f
        };
        assert_eq!(site(67, true, 25), Some((b"k".to_vec(), true, false)));
        assert_eq!(site(157, false, 25), Some((b"k".to_vec(), true, true)));
        assert_eq!(site(67, false, 25), None, "= ALL");
        assert_eq!(site(157, true, 25), None, "<> ANY");
        assert_eq!(site(67, true, 1043), None, "a varchar array");
    }
}

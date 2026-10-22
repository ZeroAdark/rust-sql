//! Road text.eq's view operand (D3151): the text a `=` / `<>` / `= ANY` site compares may be a view of its column
//! rather than the column itself: `rtrim1(c)` (`text(bpchar)`, `rtrim(text)`: trailing blanks dropped) and
//! `substring(v FROM s [FOR n])` (text_substr / text_substr_no_len) over the column or that trim, read over the column's
//! image in place, never copied. The trim walks the image's last bytes; the substring skips `s - 1` characters and keeps
//! `n` by UTF-8 lead bytes (pg_utf_mblen; the database is UTF-8), `s` / `n` the call's constants read at run time as
//! operands (V4R-18: literal-free). `=` / `<>` compare as the column's form does; `= ANY` walks the array once in host
//! `text_any_view`. A row the walk does not take (a NULL datum of an `= ANY`, a compressed / external image, a negative
//! count: PostgreSQL's error) takes the cold host `text_view_cold`, which applies PostgreSQL's rtrim1 / text_substring
//! and compares. Owner percore.
use gravity_expr::{ExprNode, NodeId, Program};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use mcx::GrowIn;

use super::cmp_text::{bytes_eq, col, header, result, Site};
use super::raw::text_view::{text_any_view, text_view_cold};
use crate::frame::env::Env;

/// PostgreSQL's `PG_UTF8` encoding id.
const PG_UTF8: i32 = 6;

/// The view a site compares: `trim` (rtrim1 in the chain) and the substring's start / count operands (`sub` indexes;
/// a count `None`: text_substr_no_len), set by `attach` from the plan's arguments.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct View {
    pub trim: bool,
    pub substr: Option<(u32, Option<u32>)>,
}

/// A view operand in the program: its column (attno, type), whether rtrim1 is in the chain, the substring's start and
/// count nodes when there is one (`(start, None)`: no count).
pub(super) struct Shape {
    pub attno: i16,
    pub typ: u32,
    pub trim: bool,
    pub substr: Option<(NodeId, Option<NodeId>)>,
}

/// Node `id` as a view of a column: `rtrim1(c)`, `substring(c | rtrim1(c) FROM s [FOR n])` whose `s` and `n` are
/// non-NULL constants (each value read at run time); a bare column is `cmp_text::col`'s, not a view.
pub(super) fn shape(p: &Program<'_>, id: NodeId) -> Option<Shape> {
    crate::stack_check();
    let ExprNode::Func(n) = &p.nodes[id.0 as usize] else { return None };
    let call = &p.sites[n.site.0 as usize];
    let kid = |i: u32| p.kids[(call.args.start + i) as usize];
    let konst = |i: u32| matches!(&p.nodes[kid(i).0 as usize], ExprNode::Const(c) if !c.is_null() && c.byval);
    match (gravity_fn::row_of(call.funcid)?.name, call.args.len) {
        ("rtrim1", 1) => col(p, kid(0)).map(|(attno, typ)| Shape { attno, typ, trim: true, substr: None }),
        (name @ ("text_substr" | "text_substr_no_len"), len) if len == 2 + (name == "text_substr") as u32 && (1..len).all(konst) => {
            // the character walk is UTF-8's; the server is UTF8-only (SQL_ASCII refused), so this is a guard
            if gravity_fn::typesem::collation::database_encoding() != PG_UTF8 {
                return None;
            }
            let inner = col(p, kid(0)).map(|(attno, typ)| Shape { attno, typ, trim: false, substr: None }).or_else(|| shape(p, kid(0)).filter(|s| s.substr.is_none()))?;
            Some(Shape { substr: Some((kid(1), (len == 3).then(|| kid(2)))), ..inner })
        }
        _ => None,
    }
}

/// The plan's substring start / count arguments under comparison argument `a` of the plan node `n` (through a
/// RelabelType), in `shape`'s order.
pub(super) fn plan_args(n: types_nodes::Node<'_>, a: usize) -> Option<(types_nodes::Node<'_>, Option<types_nodes::Node<'_>>)> {
    let args = n.as_op_expr().map(|o| &o.args).or_else(|| n.as_scalar_array_op_expr().map(|s| &s.args))?;
    let x = *args.as_slice().get(a)?;
    let f = x.as_relabel_type().map_or(x, |r| r.arg).as_func_expr()?;
    let at = |i: usize| f.args.as_slice().get(i).map(|a| a.as_relabel_type().map_or(*a, |r| r.arg));
    Some((at(1)?, at(2)))
}

/// Site `s` over the datum `p` of its column (`null`: its NULL flag), the constant `k` (`kok`: not NULL) and the
/// substring's start / count words: `[value, valid, err]`. Road text.lean (`lean`: the constant's frame words, text_lean.rs):
/// the column's header and rtrim1 length come from the row memo, the `=` / `<>` constant from its words.
#[allow(clippy::too_many_arguments)]
pub(super) fn compare(b: &mut FunctionBuilder<'_>, env: &mut Env, p: Value, null: Value, k: Value, kok: Value, s: &Site, sub: Option<(Value, Option<Value>)>, lean: Option<(i32, i32)>) -> [Value; 3] {
    let (z, f, t) = (env.zero, b.bconst(false), b.bconst(true));
    let c = |b: &mut FunctionBuilder<'_>, v: i64| b.iconst(Type::I64, v);
    let int4 = |b: &mut FunctionBuilder<'_>, v: Value| { let v = b.conv(ConvOp::Trunc, Type::I32, v); b.conv(ConvOp::Sext, Type::I64, v) };
    let sub = sub.map(|(st, n)| (int4(b, st), n.map(|n| int4(b, n))));
    // road text.lean: the column's header (and rtrim1's length) once per row, before any branch (the row memo's rule)
    let memo = lean.map(|_| super::text_lean::text_memo(b, env, p, null, s.view.trim));
    let l_out = b.create_block(&[Type::I64, Type::Bool, Type::I64]);
    let (l_read, l_head, l_hot, l_cold) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
    b.mark_cold(l_cold);
    // as the column's form: `= ANY` of an empty array is false whatever the datum (D1812), so only a NULL array is
    // absent there; the cold host gets a NULL datum as 0
    let kn = b.int_bin(IntBinOp::Xor, kok, t);
    let absent = if s.any.is_some() { kn } else { b.int_bin(IntBinOp::Or, null, kn) };
    b.cond_br(absent, (l_out, &[z, f, z]), (l_read, &[]));
    b.switch_to(l_read);
    b.cond_br(null, (l_cold, &[]), (l_head, &[]));
    b.switch_to(l_head);
    let (ok, len, data) = match memo {
        Some([ok, len, data, _]) => (ok, len, data),
        None => header(b, env, p),
    };
    // a negative count raises in PostgreSQL's text_substring: the cold host's call names it
    let ok = match sub {
        Some((_, Some(n))) => { let nn = b.icmp(IntCond::Sge, n, z); b.int_bin(IntBinOp::And, ok, nn) }
        _ => ok,
    };
    b.cond_br(ok, (l_hot, &[]), (l_cold, &[]));
    b.switch_to(l_hot);
    let len = match memo {
        Some([.., tlen]) => tlen,
        None if s.view.trim => rtrim(b, env, data, len),
        None => len,
    };
    let (data, len) = match sub {
        Some((st, n)) => substr(b, env, data, len, st, n),
        None => (data, len),
    };
    if let Some(align) = s.any {
        let h = b.declare_host_once("text_any_view", &[Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64], Some(Type::I64), text_any_view as *const () as usize, HostFlags { touches_memory: true, may_error: true, worker: true, ..HostFlags::default() });
        let r = { let a = c(b, align as i64); b.call_host(h, &[env.ctx, data, len, k, a], Some(Type::I64)).expect("returns") };
        let out = decode(b, env, s, r);
        b.br(l_out, &out);
    } else {
        let (l_cmp, l_bytes, l_res) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::Bool]));
        let (kok, klen, kdata) = match lean {
            // the entry's words: 1 + the payload length (-1 NULL, -2 not plain, 0 no entry: both cold), its address
            Some((w0, w1)) => {
                let (kw, kdata) = (env.load_word(b, w0), env.load_word(b, w1));
                (b.icmp(IntCond::Sgt, kw, z), b.int_bin(IntBinOp::Sub, kw, env.one), kdata)
            }
            None => header(b, env, k),
        };
        b.cond_br(kok, (l_cmp, &[]), (l_cold, &[]));
        b.switch_to(l_cmp);
        let same = b.icmp(IntCond::Eq, len, klen);
        b.cond_br(same, (l_bytes, &[]), (l_res, &[f]));
        b.switch_to(l_bytes);
        let m = bytes_eq(b, env, data, kdata, len);
        b.br(l_res, &[m]);
        b.switch_to(l_res);
        let (v, ok) = result(b, s, b.block_params(l_res)[0], t);
        b.br(l_out, &[v, ok, z]);
    }
    b.switch_to(l_cold);
    let h = b.declare_host_once("text_view_cold", &[Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64, Type::I64], Some(Type::I64), text_view_cold as *const () as usize, HostFlags { touches_memory: true, may_error: true, worker: true, ..HostFlags::default() });
    let form = c(b, form_word(s));
    let (st, n) = sub.map_or((z, z), |(st, n)| (st, n.unwrap_or(z)));
    let pd = b.select(null, z, p);
    let r = b.call_host(h, &[env.ctx, pd, k, form, st, n], Some(Type::I64)).expect("returns");
    let out = decode(b, env, s, r);
    (b.br(l_out, &out), b.switch_to(l_out));
    [0, 1, 2].map(|i| b.block_params(l_out)[i])
}

/// The cold host's form word: bit 0 trim, bit 1 a substring, bit 2 its count, bit 3 `= ANY`, the array's alignment from
/// bit 8 (`text_view_cold`).
fn form_word(s: &Site) -> i64 {
    let (sub, count) = (s.view.substr.is_some(), s.view.substr.is_some_and(|x| x.1.is_some()));
    s.view.trim as i64 | (sub as i64) << 1 | (count as i64) << 2 | (s.any.is_some() as i64) << 3 | (s.any.unwrap_or(0) as u8 as i64) << 8
}

/// A host answer `r` (1 matched, 0 not, 2 NULL, negative: the error stashed) as `[value, valid, err]`; `<>` negated.
fn decode(b: &mut FunctionBuilder<'_>, env: &Env, s: &Site, r: Value) -> [Value; 3] {
    let (z, t) = (env.zero, b.bconst(true));
    let (hit, bad, two) = (b.icmp(IntCond::Eq, r, env.one), b.icmp(IntCond::Slt, r, z), b.iconst(Type::I64, 2));
    let nul = b.icmp(IntCond::Eq, r, two);
    let raise = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    let (v, _) = result(b, s, hit, t);
    let (ok, e) = (b.int_bin(IntBinOp::Xor, nul, t), b.select(bad, raise, z));
    [v, ok, e]
}

/// The length of the `len` bytes at `data` without their trailing blanks (rtrim1): a bounded walk from the end.
pub(super) fn rtrim(b: &mut FunctionBuilder<'_>, env: &mut Env, data: Value, len: Value) -> Value {
    let z = env.zero;
    let (l_loop, l_byte, l_done) = (b.create_block(&[Type::I64]), b.create_block(&[]), b.create_block(&[Type::I64]));
    b.mark_loop_header(l_loop);
    env.bounded.push_in(l_loop);
    b.br(l_loop, &[len]);
    b.switch_to(l_loop);
    let n = b.block_params(l_loop)[0];
    let empty = b.icmp(IntCond::Eq, n, z);
    b.cond_br(empty, (l_done, &[n]), (l_byte, &[]));
    b.switch_to(l_byte);
    let last = { let a = b.int_bin(IntBinOp::Add, data, n); let a = b.conv(ConvOp::IntToPtr, Type::Ptr, a); b.load_zext(Type::I8, a, -1) };
    let (sp, m) = (b.iconst(Type::I64, b' ' as i64), b.int_bin(IntBinOp::Sub, n, env.one));
    let blank = b.icmp(IntCond::Eq, last, sp);
    b.cond_br(blank, (l_loop, &[m]), (l_done, &[n]));
    b.switch_to(l_done);
    b.block_params(l_done)[0]
}

/// `substring` of the `len` bytes at `data` from character `st` (a run-time int4) for `n` characters (`None`: to the
/// end), PostgreSQL's text_substring over a UTF-8 value whose count is not negative: `(data, len)` of the result.
pub(super) fn substr(b: &mut FunctionBuilder<'_>, env: &mut Env, data: Value, len: Value, st: Value, n: Option<Value>) -> (Value, Value) {
    let (z, one) = (env.zero, env.one);
    // S1 = max(S, 1); with a count, E = S + L (no int4 overflow in 64 bits: past int4 it is the rest, as PostgreSQL's)
    // and the result empty when E <= 1, else L1 = E - S1 characters
    let lo = b.icmp(IntCond::Slt, st, one);
    let s1 = b.select(lo, one, st);
    let skip = b.int_bin(IntBinOp::Sub, s1, one);
    let at = walk(b, env, data, z, len, skip);
    let end = match n {
        Some(n) => {
            let e = b.int_bin(IntBinOp::Add, st, n);
            let l1 = b.int_bin(IntBinOp::Sub, e, s1);
            let none = b.icmp(IntCond::Sle, e, one);
            let l1 = b.select(none, z, l1);
            walk(b, env, data, at, len, l1)
        }
        None => len,
    };
    (b.int_bin(IntBinOp::Add, data, at), b.int_bin(IntBinOp::Sub, end, at))
}

/// The byte offset `count` characters past `from` in the `len` bytes at `data` (at most `len`): each step a lead byte's
/// pg_utf_mblen (1; 2 from 0xC0, 3 from 0xE0, 4 from 0xF0; 1 from 0xF8). A bounded walk.
fn walk(b: &mut FunctionBuilder<'_>, env: &mut Env, data: Value, from: Value, len: Value, count: Value) -> Value {
    let (z, one) = (env.zero, env.one);
    let (l_loop, l_step, l_done) = (b.create_block(&[Type::I64, Type::I64]), b.create_block(&[]), b.create_block(&[Type::I64]));
    b.mark_loop_header(l_loop);
    env.bounded.push_in(l_loop);
    b.br(l_loop, &[from, z]);
    b.switch_to(l_loop);
    let (i, k) = (b.block_params(l_loop)[0], b.block_params(l_loop)[1]);
    let (more, chars) = (b.icmp(IntCond::Slt, i, len), b.icmp(IntCond::Slt, k, count));
    let go = b.int_bin(IntBinOp::And, more, chars);
    let at = b.select(more, i, len);
    b.cond_br(go, (l_step, &[]), (l_done, &[at]));
    b.switch_to(l_step);
    let byte = { let a = b.int_bin(IntBinOp::Add, data, i); let a = b.conv(ConvOp::IntToPtr, Type::Ptr, a); b.load_zext(Type::I8, a, 0) };
    let mut step = one;
    for (from, d) in [(0xC0, 1), (0xE0, 1), (0xF0, 1), (0xF8, -3)] {
        let ge = { let x = b.iconst(Type::I64, from); b.icmp(IntCond::Uge, byte, x) };
        let ge = b.conv(ConvOp::Zext, Type::I64, ge);
        let ge = if d == 1 { ge } else { let m = b.iconst(Type::I64, d); b.int_bin(IntBinOp::Mul, ge, m) };
        step = b.int_bin(IntBinOp::Add, step, ge);
    }
    let (i, k) = (b.int_bin(IntBinOp::Add, i, step), b.int_bin(IntBinOp::Add, k, one));
    b.br(l_loop, &[i, k]);
    b.switch_to(l_done);
    b.block_params(l_done)[0]
}

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

    /// A one-dimensional text[] of `elems` (`None`: NULL), 4-byte-header elements aligned 4.
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

    /// PostgreSQL's view (rtrim1, then text_substring over UTF-8 characters) of `d`; `None`: a negative count's error.
    fn reference(d: &str, trim: bool, sub: Option<(i64, Option<i64>)>) -> Option<String> {
        let d = if trim { d.trim_end_matches(' ') } else { d };
        let Some((st, n)) = sub else { return Some(d.to_owned()) };
        let chars: Vec<char> = d.chars().collect(); // alloc: Program
        let s1 = st.max(1);
        let (from, to) = match n {
            Some(n) if n < 0 => return None,
            Some(n) if st + n <= 1 => (0, 0),
            Some(n) => (s1 - 1, s1 - 1 + (st + n - s1)),
            None => (s1 - 1, i64::MAX),
        };
        let clip = |x: i64| x.clamp(0, chars.len() as i64) as usize;
        Some(chars[clip(from)..clip(to).max(clip(from))].iter().collect())
    }

    fn site(trim: bool, sub: Option<Option<()>>, negated: bool, any: bool) -> Site {
        Site { attno: 1, typ: 25, sub: 0, negated, bpchar: false, any: any.then_some(b'i' as i8), view: View { trim, substr: sub.map(|n| (1, n.map(|_| 2))) }, word: None }
    }

    /// `(value, valid, raised)` of view site `s` over the datum `d` (`None`: NULL), the constant image `k` and the
    /// substring's start / count words, through the interpreter. Road text.lean's form (the constant decoded by its entry
    /// into two frame words, the column's header through the row memo) answers the same for every `=` / `<>` site.
    fn eval(s: &Site, k: &[u8], d: Option<&[u8]>, words: Option<(i64, Option<i64>)>) -> (i64, bool, bool) {
        let p = d.map_or(0, |d| d.as_ptr() as i64);
        let one = |i: usize, lean: bool| {
            crate::frame::stop::tests::run_with(&[], &[k.as_ptr() as u64], &mut |b, env| {
                let lean = lean.then(|| {
                    let slot = b.declare_stack_slot(16, 8);
                    env.words = Some(b.stack_addr(slot));
                    crate::rep::varlena::text_lean::entry(b, env, 0, false, (0, 8));
                    (0, 8)
                });
                let (pv, null, kv, t) = (b.iconst(Type::I64, p), b.bconst(d.is_none()), b.iconst(Type::I64, k.as_ptr() as i64), b.bconst(true));
                let w = words.map(|(st, n)| (b.iconst(Type::I64, st), n.map(|n| b.iconst(Type::I64, n))));
                let out = compare(b, env, pv, null, kv, t, s, w, lean);
                if i == 0 { out[0] } else if i == 1 { b.conv(ConvOp::Zext, Type::I64, out[1]) } else { out[2] }
            })
        };
        let got = (one(0, false), one(1, false) != 0, one(2, false) != 0);
        if s.any.is_none() {
            assert_eq!((one(0, true), one(1, true) != 0, one(2, true) != 0), got, "text.lean's form of the view site");
        }
        got
    }

    const VALUES: [&str; 12] = ["", " ", "13-555-0101", "13", "1", "31-x  ", "Ä3-12", "€€€", "a😀b", "abc   ", "  ", "ÄÖü xyz"];
    const SUBS: [(i64, Option<i64>); 13] = [(1, Some(2)), (2, Some(3)), (0, Some(2)), (-3, Some(5)), (-3, Some(2)), (1, Some(0)), (3, Some(0)), (4, None), (1, None), (-5, None), (100, Some(2)), (1, Some(100)), (2147483647, Some(2147483647))];

    /// `view = 'k'` and `<>` over every value, trimmed or not, under every start / count, both header forms: as
    /// PostgreSQL's rtrim1 / text_substring then texteq / textne (the constants are every view the cases make).
    #[test]
    fn eq_over_views() {
        for trim in [false, true] {
            for (st, n) in SUBS {
                let needles: Vec<String> = VALUES.iter().filter_map(|v| reference(v, trim, Some((st, n)))).chain(["Ä".to_owned(), "zz".to_owned()]).collect(); // alloc: Program
                for neg in [false, true] {
                    let s = site(trim, Some(n.map(|_| ())), neg, false);
                    for k in &needles {
                        let kimg = v4(k.as_bytes());
                        for d in VALUES {
                            let want = reference(d, trim, Some((st, n))).expect("a count >= 0") == *k;
                            for img in [v1(d.as_bytes()), v4(d.as_bytes())] {
                                assert_eq!(eval(&s, &kimg, Some(&img), Some((st, n))), ((want != neg) as i64, true, false), "{d:?} trim {trim} sub ({st}, {n:?}) vs {k:?} neg {neg}");
                            }
                        }
                        assert!(!eval(&s, &kimg, None, Some((st, n))).1, "NULL is NULL");
                    }
                }
            }
            // rtrim1 alone
            let s = site(trim, None, false, false);
            for d in VALUES {
                let k = v1(reference(d, trim, None).expect("no count").as_bytes());
                assert_eq!(eval(&s, &k, Some(&v1(d.as_bytes())), None).0, 1, "{d:?} trim {trim}");
            }
        }
    }

    /// `view = ANY('{..}')` with and without a NULL element, a NULL datum, an empty array (D1812: false, a NULL datum
    /// included).
    #[test]
    fn any_over_views() {
        let elems: [&[u8]; 4] = [b"13", b"31", "Ä3".as_bytes(), b""];
        for null_elem in [false, true] {
            let mut list: Vec<Option<&[u8]>> = elems.iter().map(|e| Some(*e)).collect(); // alloc: Program
            if null_elem {
                list.insert(2, None);
            }
            let arr = array(&list);
            let s = site(true, Some(Some(())), false, true);
            for d in VALUES {
                let v = reference(d, true, Some((1, Some(2)))).expect("count 2");
                let hit = elems.iter().any(|e| *e == v.as_bytes());
                let want = if hit { (1, true, false) } else { (0, !null_elem, false) };
                assert_eq!(eval(&s, &arr, Some(&v1(d.as_bytes())), Some((1, Some(2)))), want, "{d:?} null elem {null_elem}");
            }
            assert!(!eval(&s, &arr, None, Some((1, Some(2)))).1, "a NULL datum over a non-empty array is NULL");
        }
        let (empty, s) = (array(&[]), site(true, Some(Some(())), false, true));
        assert_eq!(eval(&s, &empty, Some(&v1(b"13")), Some((1, Some(2)))), (0, true, false));
        assert_eq!(eval(&s, &empty, None, Some((1, Some(2)))), (0, true, false));
    }

    /// The form word the cold host reads.
    #[test]
    fn cold_form_word() {
        assert_eq!(form_word(&site(true, Some(Some(())), false, true)), 1 | 2 | 4 | 8 | (b'i' as i64) << 8);
        assert_eq!(form_word(&site(false, Some(None), false, false)), 2);
        assert_eq!(form_word(&site(true, None, true, false)), 1);
    }
}

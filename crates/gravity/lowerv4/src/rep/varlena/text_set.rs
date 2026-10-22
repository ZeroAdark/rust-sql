//! Road `text.set` (textkeys step 2, the iii-A / iii-B proposal; on by default since its flip, off under a plain
//! `text.set`): expr.saop's byte set (a constant text IN list / `= ANY` under a byte-equal collation, step/expr_saop.rs
//! `bytes_list`, built once per execution at bind: step/raw/saop_bytes.rs `build_bytes`) answered in line rather than
//! by a host call per row. Two parts, each generic over any such list:
//! - the set's images: at bind, a list whose every element is at most 15 bytes (hash_tagged.rs `INLINE`) also lays out
//!   each element's image (two words: bytes 0..8 and 8.. little-endian, the length in the top byte; saop_bytes.rs
//!   `inline_image`, key.tagged's inline form): the list in
//!   element order and an open-addressed table (`images`). A row's operand is imaged the same way from its bytes in line
//!   (two page-safe loads, hash_tagged.rs `tail`) and compared: two-word compares unrolled over the list when the keyed
//!   count is at most `UNROLL`, else one table probe. An operand longer than 15 bytes is in no such list.
//! - the operand may be a view of its column (`substring(c FROM s [FOR n])`, `rtrim1(c)`, text_view.rs `shape`): read
//!   over the column's image in place (text_view.rs `substr` / `rtrim`), never the gravity_expr call that copies it.
//!
//! PostgreSQL's answer as before: `= ANY` true on a match, else NULL when an element is NULL, else false (`<> ALL` the
//! negation); a NULL operand NULL (the list is never empty). A row the in-line form does not take (a NULL operand, a
//! compressed / external image, a negative substring count, a set without images) takes the host: the column operand
//! expr.saop's `saop_eval`, a view `text_set_view` (PostgreSQL's rtrim1 / text_substring, then the byte set's probe).
//! Owner textkeys (a3).
use gravity_expr::emit::hash::{emit_home_at, emit_key_hash_of, Bits};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use mcx::GrowIn;
use types_nodes::Node;

use crate::frame::env::Env;
use crate::ops::Table;
use crate::rep::varlena::hash_tagged::INLINE;
use crate::step::raw::saop_bytes::{IMG_HEAD, IMG_NONE};

crate::strategies! {
    /// textkeys step 2: expr.saop's byte set probed in line over the elements' images, its operand a column or a view of
    /// one. On by default since its flip (not on road_off.rs `DEFAULT_OFF`): off under a plain `text.set`.
    TEXT_SET = 158, "text.set", TextFilter;
    /// D4794: the byte set over bpchar lists too (bpchareq = ANY, bpcharne <> ALL of a column), elements trimmed of their
    /// trailing blanks at bind, the operand in line (text_lean.rs `trim_len`). Default off (road_off.rs `DEFAULT_OFF`): on
    /// under `+text.set.bpchar`.
    TEXT_SET_BPCHAR = 152, "text.set.bpchar", TextFilter;
}

pub(crate) const ROWS: Table = Table { strategies: &[(TEXT_SET, &()), (TEXT_SET_BPCHAR, &())], ..crate::ops::NONE };

/// The longest keyed list compared element by element (unrolled); a longer one probes the table.
pub(crate) const UNROLL: usize = 8;

pub(crate) fn on() -> bool {
    crate::face::source::on(crate::admit::refusal::Variant::Strategy(TEXT_SET))
}

/// D4794: road text.set.bpchar for this lowering (step/expr_saop.rs `list_form`).
pub(crate) fn bpchar_on() -> bool {
    crate::face::source::on(crate::admit::refusal::Variant::Strategy(TEXT_SET_BPCHAR))
}

/// A site's view operand, as text_view.rs `View`: rtrim1 in the chain, the substring's start / count operands (`sub`
/// indexes; a count `None`: text_substr_no_len).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SetView {
    pub trim: bool,
    pub substr: Option<(u32, Option<u32>)>,
    /// D4794: the column is bpchar's own (no rtrim1 in the plan): `trim` is bpchareq's, open-coded as text.lean's.
    pub bpchar: bool,
}

/// `n` through any RelabelType.
fn bare(mut n: Node<'_>) -> Node<'_> {
    while let Some(r) = n.as_relabel_type() {
        n = r.arg;
    }
    n
}

/// The plan side of a view operand `arg` (text_view.rs `shape` decided the program side): its column's plan node, whether
/// rtrim1 is in the chain, the substring's start / count plan nodes.
#[allow(clippy::type_complexity)]
pub(crate) fn view_plan<'p>(arg: Node<'p>) -> Option<(Node<'p>, bool, Option<(Node<'p>, Option<Node<'p>>)>)> {
    let f = bare(arg).as_func_expr()?;
    let args = f.args.as_slice();
    match gravity_fn::row_of(f.funcid)?.name {
        "rtrim1" => Some((bare(*args.first()?), true, None)),
        "text_substr" | "text_substr_no_len" => {
            let sub = Some((bare(*args.get(1)?), args.get(2).map(|n| bare(*n))));
            let inner = bare(*args.first()?);
            match inner.as_func_expr().map(|g| gravity_fn::row_of(g.funcid).map(|r| r.name)) {
                Some(Some("rtrim1")) => Some((bare(*inner.as_func_expr()?.args.as_slice().first()?), true, sub)),
                Some(_) => None,
                None => Some((inner, false, sub)),
            }
        }
        _ => None,
    }
}

/// Whether program node `id` is a view operand road text.set reads in place (text_view.rs `shape`).
pub(crate) fn is_view(p: &gravity_expr::Program<'_>, id: gravity_expr::NodeId) -> bool {
    super::text_view::shape(p, id).is_some()
}

/// The host form word of a view: text_view_cold's bits (0 trim, 1 a substring, 2 its count), bit 4 `= ANY` (else
/// `<> ALL`).
pub(crate) fn form_word(v: Option<SetView>, use_or: bool) -> i64 {
    let v = v.unwrap_or(SetView { trim: false, substr: None, bpchar: false });
    v.trim as i64 | (v.substr.is_some() as i64) << 1 | (v.substr.is_some_and(|(_, n)| n.is_some()) as i64) << 2 | (use_or as i64) << 4
}

/// The site's answer code (saop_host `FALSE` / `TRUE` / `NULL` / `FAILED`) for the operand datum `p` (`p_ok`: not NULL)
/// over the bound set `set` (a byte set's region address): in line when the set has images and the operand (its view,
/// `view`: the start / count words) is in line, else `cold`'s. `n`: the keyed element count (`None`: past the keyed
/// class), `use_or`: `= ANY` (else `<> ALL`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn probe(b: &mut FunctionBuilder<'_>, env: &mut Env, set: Value, p: Value, p_ok: Value, view: Option<(bool, Option<(Value, Option<Value>)>)>, bpchar: bool, n: Option<usize>, use_or: bool, cold: &mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env) -> Value) -> Value {
    let (z, one) = (env.zero, env.one);
    let c = |b: &mut FunctionBuilder<'_>, v: i64| b.iconst(Type::I64, v);
    let int4 = |b: &mut FunctionBuilder<'_>, v: Value| { let v = b.conv(ConvOp::Trunc, Type::I32, v); b.conv(ConvOp::Sext, Type::I64, v) };
    let l_out = b.create_block(&[Type::I64]);
    let (l_head, l_fit, l_img, l_cold) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
    let l_res = b.create_block(&[Type::Bool]);
    // the set's images: their offset in the head's second word (bits 32.., in words; 0: none, saop_bytes.rs `build_bytes`)
    let sp = b.conv(ConvOp::IntToPtr, Type::Ptr, set);
    let hn = b.load(Type::I64, sp, 8);
    let (s32, s3) = (c(b, 32), c(b, 3));
    let off = { let w = b.int_bin(IntBinOp::Ushr, hn, s32); b.int_bin(IntBinOp::Shl, w, s3) };
    let imaged = b.icmp(IntCond::Ne, off, z);
    let go = b.int_bin(IntBinOp::And, imaged, p_ok);
    b.cond_br(go, (l_head, &[]), (l_cold, &[]));
    b.switch_to(l_head);
    let (ok, len, data) = super::cmp_text::header(b, env, p);
    let sub = view.and_then(|(_, s)| s).map(|(st, n)| (int4(b, st), n.map(|n| int4(b, n))));
    // a negative count raises in PostgreSQL's text_substring: the host's call names it
    let ok = match sub {
        Some((_, Some(n))) => { let nn = b.icmp(IntCond::Sge, n, z); b.int_bin(IntBinOp::And, ok, nn) }
        _ => ok,
    };
    b.cond_br(ok, (l_fit, &[]), (l_cold, &[]));
    b.switch_to(l_fit);
    // D4794: a bpchar operand without its trailing blanks, eight bytes a step (text.lean's bcTruelen)
    let len = if bpchar {
        super::text_lean::trim_len(b, env, data, len)
    } else if view.is_some_and(|(trim, _)| trim) {
        super::text_view::rtrim(b, env, data, len)
    } else {
        len
    };
    let (data, len) = match sub {
        Some((st, n)) => super::text_view::substr(b, env, data, len, st, n),
        None => (data, len),
    };
    let max = c(b, INLINE);
    let fits = b.icmp(IntCond::Ule, len, max);
    let f = b.bconst(false);
    b.cond_br(fits, (l_img, &[]), (l_res, &[f]));
    b.switch_to(l_img);
    let [w0, w1] = image(b, env, sp, data, len);
    let sec = b.ptr_add(sp, off, 1, 0);
    match n.filter(|&n| n <= UNROLL) {
        // the list, element by element: the bound count is the keyed one (a list's class is keyed), checked all the same
        Some(n) => {
            let (l_lin, l_hash) = (b.create_block(&[]), b.create_block(&[]));
            let (cnt, want) = (b.load(Type::I64, sec, 0), c(b, n as i64));
            let same = b.icmp(IntCond::Eq, cnt, want);
            b.cond_br(same, (l_lin, &[]), (l_hash, &[]));
            b.switch_to(l_lin);
            let mut found = f;
            for i in 0..n {
                let at = (IMG_HEAD + 16 * i) as i32;
                let (e0, e1) = (b.load(Type::I64, sec, at), b.load(Type::I64, sec, at + 8));
                let (x, y) = (b.icmp(IntCond::Eq, w0, e0), b.icmp(IntCond::Eq, w1, e1));
                let both = b.int_bin(IntBinOp::And, x, y);
                found = b.int_bin(IntBinOp::Or, found, both);
            }
            b.br(l_res, &[found]);
            b.switch_to(l_hash);
            table(b, env, sec, w0, w1, l_res);
        }
        None => table(b, env, sec, w0, w1, l_res),
    }
    b.switch_to(l_res);
    let found = b.block_params(l_res)[0];
    // = ANY: TRUE on a match, else NULL beside a NULL element, else FALSE; <> ALL the negation of a match
    let nulls = { let m = b.int_bin(IntBinOp::And, hn, one); b.icmp(IntCond::Ne, m, z) };
    let (hit, miss) = (c(b, use_or as i64), c(b, !use_or as i64));
    let null = c(b, crate::step::raw::saop_host::NULL as i64);
    let miss = b.select(nulls, null, miss);
    let st = b.select(found, hit, miss);
    b.br(l_out, &[st]);
    b.switch_to(l_cold);
    let st = cold(b, env);
    b.br(l_out, &[st]);
    b.switch_to(l_out);
    b.block_params(l_out)[0]
}

/// The image (saop_bytes.rs `inline_image`) of the `len` bytes at `data` (`len` at most `INLINE`): two loads that never read
/// past the payload's page (hash_tagged.rs `tail`); an empty payload's loads read the set's head `safe` instead, their
/// words then zero.
fn image(b: &mut FunctionBuilder<'_>, env: &Env, safe: Value, data: Value, len: Value) -> [Value; 2] {
    let (z, one) = (env.zero, env.one);
    let c = |b: &mut FunctionBuilder<'_>, v: i64| b.iconst(Type::I64, v);
    let at = |b: &mut FunctionBuilder<'_>, v: Value| b.conv(ConvOp::IntToPtr, Type::Ptr, v);
    let (eight, s56) = (c(b, 8), c(b, 56));
    let empty = b.icmp(IntCond::Eq, len, z);
    let q0 = { let s = b.conv(ConvOp::PtrToInt, Type::I64, safe); let d = b.conv(ConvOp::PtrToInt, Type::I64, data); b.select(empty, s, d) };
    let k0 = { let lt = b.icmp(IntCond::Ult, len, eight); let k = b.select(lt, len, eight); b.select(empty, one, k) };
    let q0 = at(b, q0);
    let raw0 = super::hash_tagged::tail(b, q0, k0);
    let w0 = b.select(empty, z, raw0);
    let gt8 = b.icmp(IntCond::Ugt, len, eight);
    let q1 = { let s = b.conv(ConvOp::PtrToInt, Type::I64, safe); let d = b.conv(ConvOp::PtrToInt, Type::I64, data); let d8 = b.int_bin(IntBinOp::Add, d, eight); b.select(gt8, d8, s) };
    let k1 = { let k = b.int_bin(IntBinOp::Sub, len, eight); b.select(gt8, k, one) };
    let q1 = at(b, q1);
    let raw1 = super::hash_tagged::tail(b, q1, k1);
    let w1 = b.select(gt8, raw1, z);
    let top = b.int_bin(IntBinOp::Shl, len, s56);
    [w0, b.int_bin(IntBinOp::Or, w1, top)]
}

/// One probe of the images' table at `sec` for `[w0, w1]` (saop_bytes.rs `images`: linear from the image's KeyHash home;
/// the table is at most half full, so an empty slot ends the walk): branches to `l_res` with the match.
fn table(b: &mut FunctionBuilder<'_>, env: &mut Env, sec: Value, w0: Value, w1: Value, l_res: gravity_ir::Block) {
    let c = |b: &mut FunctionBuilder<'_>, v: i64| b.iconst(Type::I64, v);
    let (cnt, mask, shift) = (b.load(Type::I64, sec, 0), b.load(Type::I64, sec, 8), b.load(Type::I64, sec, 16));
    let (s4, head) = (c(b, 4), c(b, IMG_HEAD as i64));
    let tb = { let l = b.int_bin(IntBinOp::Shl, cnt, s4); let o = b.int_bin(IntBinOp::Add, l, head); b.ptr_add(sec, o, 1, 0) };
    let h = emit_key_hash_of(b, &[w0, w1]);
    let i0 = emit_home_at(b, h, shift, Bits::Const(0));
    let (l_loop, l_next) = (b.create_block(&[Type::I64]), b.create_block(&[]));
    b.mark_loop_header(l_loop);
    env.bounded.push_in(l_loop);
    b.br(l_loop, &[i0]);
    b.switch_to(l_loop);
    let i = b.block_params(l_loop)[0];
    let slot = b.ptr_add(tb, i, 16, 0);
    let (s0, s1) = (b.load(Type::I64, slot, 0), b.load(Type::I64, slot, 8));
    let hit = { let x = b.icmp(IntCond::Eq, s0, w0); let y = b.icmp(IntCond::Eq, s1, w1); b.int_bin(IntBinOp::And, x, y) };
    let t = b.bconst(true);
    b.cond_br(hit, (l_res, &[t]), (l_next, &[]));
    b.switch_to(l_next);
    let none = c(b, IMG_NONE as i64);
    let empty = b.icmp(IntCond::Eq, s1, none);
    let nx = { let one = env.one; let a = b.int_bin(IntBinOp::Add, i, one); b.int_bin(IntBinOp::And, a, mask) };
    let f = b.bconst(false);
    b.cond_br(empty, (l_res, &[f]), (l_loop, &[nx]));
}

/// The view host's call (raw.rs `text_set_view`): `[ctx, datum or 0, set, form, start, count]` -> the answer code.
pub(crate) fn view_cold(b: &mut FunctionBuilder<'_>, env: &mut Env, p: Value, p_ok: Value, set: Value, form: i64, sub: Option<(Value, Option<Value>)>) -> Value {
    let z = env.zero;
    let flags = HostFlags { touches_memory: true, may_error: true, pure: false, worker: true, state: gravity_ir::StateMask::NONE };
    let h = b.declare_host_once("text_set_view", &[Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64, Type::I64], Some(Type::I64), super::raw::text_set::text_set_view as *const () as usize, flags);
    let pd = b.select(p_ok, p, z);
    let fw = b.iconst(Type::I64, form);
    let (st, n) = sub.map_or((z, z), |(st, n)| (st, n.unwrap_or(z)));
    let r = b.call_host(h, &[env.ctx, pd, set, fw, st, n], Some(Type::I64)).expect("returns");
    // -1 (the error stashed) is the site's FAILED
    let bad = b.icmp(IntCond::Slt, r, z);
    let failed = b.iconst(Type::I64, crate::step::raw::saop_host::FAILED as i64);
    b.select(bad, failed, r)
}

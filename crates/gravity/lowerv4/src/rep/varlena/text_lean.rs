//! Road `text.lean` (textkeys step 1, the ii-B proposal; on by default since its flip, off under a plain `text.lean`):
//! road text.eq's per-row test made cheap. Three parts, each generic over any text = / <> site text.eq claims
//! (cmp_text.rs) and its views (text_view.rs):
//! - the constant operand (its bind word, V4R-18: literal-free) is decoded once per pipeline call at the extension's `entry`
//!   into two frame words: its payload's length (bpchar: without trailing blanks, as bpchareq trims both sides) and its
//!   payload address. A word no entry wrote reads as 0, which sends the row to the cold host, never to a wrong answer.
//! - the column's header (plain or not, length, payload address) and its length without trailing blanks are read once per
//!   row (`text_memo`, the row memo's `texts`): a CASE testing one column seven times read its header seven times.
//! - the trailing blanks (bpchar's bcTruelen, rtrim1's view) are found open-coded, eight bytes at a time from the end
//!   (`trim_len`), where text.eq called the host `text_trim` per test.
//!
//! Byte equality under deterministic collations as text.eq; a compressed / external datum or constant takes the cold host
//! (`text_eq_cold`, PostgreSQL's detoast and compare), as before. Owner textkeys (a3).
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, IntUnOp, Type, Value};
use mcx::GrowIn;

use crate::face::kinds::Face;
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px, WordKey};

use super::cmp_text::{bytes_eq, header, result, Site};

/// The key tag of text.lean's frame words (`words`): apart from num.konst's words of the same op and expression.
const LEAN_TAG: u64 = 1 << 63;

/// Eight blanks.
const BLANKS: i64 = 0x2020_2020_2020_2020;

/// Whether road text.lean is on for this lowering.
pub(super) fn on() -> bool {
    crate::face::source::on(crate::admit::refusal::Variant::Strategy(super::cmp_text::TEXT_LEAN))
}

/// Site `i`'s two frame words (the constant's length word and its payload address) of expression `ex` at step `(k, j)`.
pub(super) fn words(px: &mut Px<'_, '_>, (k, j): (usize, usize), ex: ExId, i: u8) -> (i32, i32) {
    let mut w = |i: u8| px.frame_word(WordKey::Open { face: Face::Rep, k, j: crate::frame::px::key_j(j) ^ ((ex.0 as u64 + 1) << 40) ^ LEAN_TAG, i }) as i32 * 8;
    (w(2 * i), w(2 * i + 1))
}

/// The bind word of site `s`'s constant operand (`subs`: the claim's operand expressions), when it has one: the lean form
/// reads the constant there (`entry`); without one the site keeps text.eq's form. `entry` and `emit` ask the same.
pub(super) fn konst_word(env: &Env, subs: &[ExId], s: &Site) -> Option<u32> {
    match env.placed(*subs.get(s.sub as usize)?).places.first() {
        Some(&gravity_lower::Place::Word(w)) => Some(w as u32),
        _ => None,
    }
}

/// At a call's entry: the constant in bind word `w` into the site's frame words `(w0, w1)`: `w0` = -1 for a NULL constant,
/// -2 for one that is not a plain image (compressed, external: cold), else 1 + its payload length (`trim`: without trailing
/// blanks); `w1` its payload address.
pub(super) fn entry(b: &mut FunctionBuilder<'_>, env: &mut Env, w: u32, trim: bool, (w0, w1): (i32, i32)) {
    let z = env.zero;
    let (p, nb) = (b.load(Type::I64, env.params, w as i32 * 8), b.load_zext(Type::I8, env.params_null, w as i32));
    let (l_dec, l_ok, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64, Type::I64]));
    let (null, minus1, minus2) = (b.icmp(IntCond::Ne, nb, z), b.iconst(Type::I64, -1), b.iconst(Type::I64, -2));
    b.cond_br(null, (l_join, &[minus1, z]), (l_dec, &[]));
    b.switch_to(l_dec);
    let (ok, len, data) = header(b, env, p);
    b.cond_br(ok, (l_ok, &[]), (l_join, &[minus2, z]));
    b.switch_to(l_ok);
    let len = if trim { trim_len(b, env, data, len) } else { len };
    let n = b.int_bin(IntBinOp::Add, len, env.one);
    b.br(l_join, &[n, data]);
    b.switch_to(l_join);
    let (n, data) = (b.block_params(l_join)[0], b.block_params(l_join)[1]);
    env.store_word(b, w0, n);
    env.store_word(b, w1, data);
}

/// The text datum `p` (`null`: its NULL flag) as `[plain, length, payload address, length without trailing blanks]`
/// (`trim`; without it the length again), read once per row: the row memo answers a second test of the same datum. A NULL
/// or a datum that is not a plain image reads no byte (its words are not used: the site goes cold or answers NULL).
pub(crate) fn text_memo(b: &mut FunctionBuilder<'_>, env: &mut Env, p: Value, null: Value, trim: bool) -> [Value; 4] {
    let key = (p, null, trim);
    if let Some(v) = env.memo.text(key) {
        return v;
    }
    let (z, f) = (env.zero, b.bconst(false));
    let (l_dec, l_trim, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::Bool, Type::I64, Type::I64, Type::I64]));
    b.cond_br(null, (l_join, &[f, z, z, z]), (l_dec, &[]));
    b.switch_to(l_dec);
    let (ok, len, data) = header(b, env, p);
    let t = b.bconst(true);
    b.cond_br(ok, (l_trim, &[]), (l_join, &[f, z, z, z]));
    b.switch_to(l_trim);
    let tlen = if trim { trim_len(b, env, data, len) } else { len };
    b.br(l_join, &[t, len, data, tlen]);
    b.switch_to(l_join);
    let v = [0, 1, 2, 3].map(|i| b.block_params(l_join)[i]);
    env.memo.put_text(key, v);
    v
}

/// The `len` bytes at `data` without their trailing blanks: from the end, eight bytes a step while all eight are blanks;
/// the first word that is not gives the count by its leading zero bits (`(w ^ BLANKS)`: a blank byte is zero, the payload's
/// last byte the word's top byte); under eight bytes left, byte by byte. Bounded walks (the length).
pub(crate) fn trim_len(b: &mut FunctionBuilder<'_>, env: &mut Env, data: Value, len: Value) -> Value {
    let (z, one) = (env.zero, env.one);
    let (l_loop, l_word, l_small, l_byte, l_done) = (b.create_block(&[Type::I64]), b.create_block(&[]), b.create_block(&[Type::I64]), b.create_block(&[]), b.create_block(&[Type::I64]));
    b.mark_loop_header(l_loop);
    b.mark_loop_header(l_small);
    env.bounded.push_in(l_loop);
    env.bounded.push_in(l_small);
    b.br(l_loop, &[len]);
    b.switch_to(l_loop);
    let n = b.block_params(l_loop)[0];
    let eight = b.iconst(Type::I64, 8);
    let big = b.icmp(IntCond::Sge, n, eight);
    b.cond_br(big, (l_word, &[]), (l_small, &[n]));
    b.switch_to(l_word);
    let w = { let a = b.int_bin(IntBinOp::Add, data, n); let a = b.conv(gravity_ir::ConvOp::IntToPtr, Type::Ptr, a); b.load_unaligned(Type::I64, a, -8) };
    let x = { let k = b.iconst(Type::I64, BLANKS); b.int_bin(IntBinOp::Xor, w, k) };
    let (all, n8) = (b.icmp(IntCond::Eq, x, z), b.int_bin(IntBinOp::Sub, n, eight));
    let l_part = b.create_block(&[]);
    b.cond_br(all, (l_loop, &[n8]), (l_part, &[]));
    b.switch_to(l_part);
    let lz = b.int_un(IntUnOp::Clz, x);
    let three = b.iconst(Type::I64, 3);
    let blanks = b.int_bin(IntBinOp::Ushr, lz, three);
    let m = b.int_bin(IntBinOp::Sub, n, blanks);
    b.br(l_done, &[m]);
    b.switch_to(l_small);
    let n = b.block_params(l_small)[0];
    let empty = b.icmp(IntCond::Sle, n, z);
    b.cond_br(empty, (l_done, &[z]), (l_byte, &[]));
    b.switch_to(l_byte);
    let last = { let a = b.int_bin(IntBinOp::Add, data, n); let a = b.conv(gravity_ir::ConvOp::IntToPtr, Type::Ptr, a); b.load_zext(Type::I8, a, -1) };
    let (sp, m) = (b.iconst(Type::I64, b' ' as i64), b.int_bin(IntBinOp::Sub, n, one));
    let blank = b.icmp(IntCond::Eq, last, sp);
    b.cond_br(blank, (l_small, &[m]), (l_done, &[n]));
    b.switch_to(l_done);
    b.block_params(l_done)[0]
}

/// Site `s` (`=` / `<>` of the column itself) over the datum `p` (`null`: its NULL flag), its constant `k` (`kok`: not
/// NULL; the cold host's operand) and the constant's frame words `(w0, w1)` (`entry`): `[value, valid, err]`, as cmp_text.rs
/// `compare`.
#[allow(clippy::too_many_arguments)]
pub(super) fn compare(b: &mut FunctionBuilder<'_>, env: &mut Env, p: Value, null: Value, k: Value, kok: Value, s: &Site, (w0, w1): (i32, i32)) -> [Value; 3] {
    let (z, f, t) = (env.zero, b.bconst(false), b.bconst(true));
    let c = |b: &mut FunctionBuilder<'_>, v: i64| b.iconst(Type::I64, v);
    let [ok, _, data, tlen] = text_memo(b, env, p, null, s.bpchar);
    let (kw, kdata) = (env.load_word(b, w0), env.load_word(b, w1));
    let (l_out, l_read, l_cold) = (b.create_block(&[Type::I64, Type::Bool, Type::I64]), b.create_block(&[]), b.create_block(&[]));
    let (l_len, l_bytes, l_res) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::Bool]));
    b.mark_cold(l_cold);
    // a NULL constant answers NULL whether or not an entry decoded it (`kok`, the bind word's own flag)
    let kn = b.int_bin(IntBinOp::Xor, kok, t);
    let absent = b.int_bin(IntBinOp::Or, null, kn);
    b.cond_br(absent, (l_out, &[z, f, z]), (l_read, &[]));
    b.switch_to(l_read);
    let hot = { let kp = b.icmp(IntCond::Sgt, kw, z); b.int_bin(IntBinOp::And, ok, kp) };
    b.cond_br(hot, (l_len, &[]), (l_cold, &[]));
    b.switch_to(l_len);
    let klen = b.int_bin(IntBinOp::Sub, kw, env.one);
    let same = b.icmp(IntCond::Eq, tlen, klen);
    b.cond_br(same, (l_bytes, &[]), (l_res, &[f]));
    b.switch_to(l_bytes);
    let m = bytes_eq(b, env, data, kdata, tlen);
    b.br(l_res, &[m]);
    b.switch_to(l_cold);
    let h = b.declare_host_once("text_eq_cold", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::I64), super::raw::cmp_text::text_eq_cold as *const () as usize, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() });
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

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use crate::frame::stop::tests::run_with;
    use crate::rep::varlena::text_view::View;

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

    /// `trim_len` against PostgreSQL's bcTruelen over lengths across the eight-byte steps: no blank, all blanks, blanks inside,
    /// a last non-blank byte at every position of its word, bytes that differ from a blank in one bit.
    #[test]
    fn trim_len_is_bctruelen() {
        let mut cases: Vec<Vec<u8>> = Vec::new(); // alloc: Program
        for n in 0..34usize {
            for k in 0..=n {
                let mut v = vec![b'x'; k]; // alloc: Program
                v.resize(n, b' ');
                cases.push(v.clone());
                if k > 1 {
                    v[k / 2] = b' ';
                    cases.push(v);
                }
            }
        }
        cases.extend([b"a!".to_vec(), b"ab0 ".to_vec(), b"\xe2\x82\xac  ".to_vec(), b"        !       ".to_vec(), b"\x20\x20\x20\x20\x20\x20\x20\x60".to_vec()]);
        for c in cases {
            let want = c.iter().rposition(|&x| x != b' ').map_or(0, |i| i + 1) as i64;
            let got = run_with(&[], &[], &mut |b, env| {
                let (d, n) = (b.iconst(Type::I64, c.as_ptr() as i64), b.iconst(Type::I64, c.len() as i64));
                trim_len(b, env, d, n)
            });
            assert_eq!(got, want, "{c:?}");
        }
    }

    /// The lean `=` / `<>` (the constant decoded by `entry` into two frame words, the column through the row memo) against
    /// PostgreSQL's texteq / bpchareq over 1- and 4-byte headers on both sides, constants with and without trailing blanks
    /// (bpchareq trims both), lengths across the word splits; a NULL datum is NULL; the second test of one datum reads the
    /// memo's words (one header decode, both answers right).
    #[test]
    fn lean_compare_is_texteq_and_bpchareq() {
        let words = ["", "a", "ab", "ab ", "abc", "abcd", "abcdefg", "abcdefgh", "abcdefgh  ", "abcdefghi", "Wednesday", "Sunday   ", "Sunday", "x y ", "abcdefgh abcdefgh abcdefgh"];
        for bpchar in [false, true] {
            for neg in [false, true] {
                let s = Site { attno: 1, typ: if bpchar { 1042 } else { 25 }, sub: 0, negated: neg, bpchar, any: None, view: View::default(), word: Some((0, 8)) };
                for k in words {
                    for kimg in [v4(k.as_bytes()), v1(k.as_bytes())] {
                        for d in words.iter().map(|d| Some(*d)).chain([None]) {
                            for dimg in [d.map(|d| v1(d.as_bytes())), d.map(|d| v4(d.as_bytes()))] {
                                let trim = |x: &str| if bpchar { x.trim_end_matches(' ').to_owned() } else { x.to_owned() };
                                let want = d.map(|d| ((trim(d) == trim(k)) != neg) as i64);
                                let p = dimg.as_ref().map_or(0, |v| v.as_ptr() as i64);
                                let one = |valid: bool, twice: bool| {
                                    run_with(&[], &[kimg.as_ptr() as u64], &mut |b, env| {
                                        let slot = b.declare_stack_slot(16, 8);
                                        env.words = Some(b.stack_addr(slot));
                                        entry(b, env, 0, bpchar, (0, 8));
                                        let (pv, null, kv, t) = (b.iconst(Type::I64, p), b.bconst(d.is_none()), b.iconst(Type::I64, kimg.as_ptr() as i64), b.bconst(true));
                                        let first = compare(b, env, pv, null, kv, t, &s, (0, 8));
                                        let [v, ok, _] = if twice { compare(b, env, pv, null, kv, t, &s, (0, 8)) } else { first };
                                        if valid { b.conv(gravity_ir::ConvOp::Zext, Type::I64, ok) } else { v }
                                    })
                                };
                                for twice in [false, true] {
                                    let (v, ok) = (one(false, twice), one(true, twice) != 0);
                                    assert_eq!(ok.then_some(v), want, "bpchar {bpchar} neg {neg} {d:?} vs {k:?} twice {twice}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// A constant `entry` declined (its words never written: 0) or a compressed datum go to the cold host, which answers as
    /// PostgreSQL does.
    #[test]
    fn unwritten_words_go_cold() {
        let s = Site { attno: 1, typ: 25, sub: 0, negated: false, bpchar: false, any: None, view: View::default(), word: Some((0, 8)) };
        let (k, d, e) = (v4(b"hello"), v1(b"hello"), v1(b"hellO"));
        for (img, want) in [(&d, 1), (&e, 0)] {
            let got = run_with(&[], &[k.as_ptr() as u64], &mut |b, env| {
                let slot = b.declare_stack_slot(16, 8);
                let at = b.stack_addr(slot);
                let z = env.zero;
                b.store(at, 0, z);
                b.store(at, 8, z);
                env.words = Some(at);
                let (pv, null, kv, t) = (b.iconst(Type::I64, img.as_ptr() as i64), b.bconst(false), b.iconst(Type::I64, k.as_ptr() as i64), b.bconst(true));
                compare(b, env, pv, null, kv, t, &s, (0, 8))[0]
            });
            assert_eq!(got, want);
        }
    }
}

//! Road `key.canon` (textkeys step 3; default off, on under `+key.canon`): ONE canonical text key image for every source
//! of a byte-equal text key (text, varchar, bpchar trimmed by `bcTruelen`, bytea), two words with one flag bit
//! (`gravity_dict::fp::canon_of`, the one constructor): a payload of at most 15 bytes is its own image `[b0..7, b8..14 |
//! len << 56]` (bit 63 clear, exact), a longer one its Fp128 with bit 63 set (127 bits of it). A heap or pgrc datum is
//! imaged here from its bytes (key.tagged's loads, hash_tagged.rs `image_as`: no SipHash for a short key); a code lane
//! reads its dictionary's image table, derived from the entries' bytes and the Fp128 table it already has (the lane's
//! table declared with `table_fn::FLAG_CANON`, ops/scan/pgrc/lower.rs; no second hash, no sidecar change), so a code and
//! a datum of the same bytes have one image wherever they meet: a build and its probe, a spill's pair, a partition merge,
//! W8's owner combine. Ahead of key.tagged (face/varkey.rs asks key.surrogate, key.canon, key.tagged, key.coll in that
//! order): under the road the hashed form is this one in every statement, heap-only or not. A key under a
//! nondeterministic collation keeps key.coll (the caller asks only for a byte-equal one). Declines (`None`) for any
//! other image and while the statement does not carry the road: its premise (applies.rs `VarlenaKeyCanon`) leaves out a
//! statement with an expression group key where code lanes are read, whose per-code key forms (percode_key, key.codeif,
//! countwalk) image the expression's results through their own tables. The copy (`bare_keys`): a hashed group whose emit
//! sorts nothing keeps no copy of a key value of at most 15 bytes (table/emit_key.rs `keep_datums`), the emit renders it
//! from the record's two image words (`render`); only a longer value keeps its arena copy.
//! Owner textkeys (a3).
use gravity_decompose::catalog::typefacts::{KeyColl, TypeFacts};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};

use crate::ops::Table;
use crate::rep::{self, CellRef, Lx, Scope, Words};

use crate::rep::varkey::{TailForm, VarKeyImpl, VarKeyMech};

crate::strategies! {
    /// textkeys step 3: one canonical text key image (inline up to 15 bytes, else the Fp128 under one flag bit) for every
    /// source. Default off (road_off.rs `DEFAULT_OFF`): on under `+key.canon`.
    KEY_CANON = 160, "key.canon", VarlenaKeyCanon;
}

pub(crate) const ROWS: Table = Table { strategies: &[(KEY_CANON, &IMPL)], ..crate::ops::NONE };

const IMPL: VarKeyImpl = VarKeyImpl(&Canon);

/// Whether the statement whose strategies are `s` images its text keys canonically (the road built, on, and applicable):
/// the code lanes' table declarations ask it, so a lane's table and a datum's image agree.
pub(crate) fn engaged(s: crate::face::strategy::StrategySet) -> bool {
    s.has(KEY_CANON) && crate::face::source::on(crate::admit::refusal::Variant::Strategy(KEY_CANON))
}

struct Canon;

impl VarKeyMech for Canon {
    fn hash(&self, lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, _: KeyColl, _: Scope) -> Option<Words> {
        if !lx.strategies.has(KEY_CANON) || !tf.image.is_bytes() {
            return None;
        }
        fires::fire("key.canon"); // textkeys witness signal: a datum key imaged canonically
        let (p, null) = rep::datum(lx, c, tf);
        let zero = lx.b.iconst(Type::I64, 0);
        let (l_val, l_out) = (lx.b.create_block(&[]), lx.b.create_block(&[Type::I64, Type::I64]));
        lx.b.cond_br(null, (l_out, &[zero, zero]), (l_val, &[]));
        lx.b.switch_to(l_val);
        let (w0, w1) = of_datum(lx, p, tf);
        lx.b.br(l_out, &[w0, w1]);
        lx.b.switch_to(l_out);
        let o = lx.b.block_params(l_out);
        Some(Words { w: [Some(o[0]), Some(o[1])], exact: true })
    }

    fn order(&self, _: &mut Lx<'_, '_, '_>, _: CellRef<'_>, _: &TypeFacts, _: KeyColl) -> Option<(Value, TailForm)> {
        None
    }
}

/// The canonical image of the non-NULL varlena datum `p` of type facts `tf` (a bpchar's trimmed): its payload read in
/// place, or detoasted into the Call's scratch and handed back once imaged.
pub(crate) fn of_datum(lx: &mut Lx<'_, '_, '_>, p: Value, tf: &TypeFacts) -> (Value, Value) {
    let p = crate::rep::varlena::hash::ptr(lx.b, p);
    let (bytes, len, mark) = crate::rep::varlena::hash::payload(lx, p, tf);
    let w = super::hash_tagged::image_as(lx, bytes, len, super::hash_tagged::Hashed::Canon);
    crate::rep::varlena::hash::release(lx, mark);
    w
}

/// The keys of group `g` (bit i: key i) whose record keeps NO copy of a value of at most 15 bytes (`INLINE`): its
/// canonical image is the value, so the emit renders it from the record's two key words (`render`, table/emit_key.rs
/// `key_cell`) and `keep_datums` copies only a longer value (or one whose header is not an inline one: `small`). A key
/// qualifies under the road for a hashed group (`GroupForm::Hashed`, its producer's records, no two-pass split) whose emit
/// orders no record by a key copy (`sorted_at_emit`), when it is a varlena datum key with no code, its image the bytes'
/// (text, varchar, bytea: a bpchar's image is its trimmed bytes, so it keeps its copy) under a byte-equal collation and
/// no user `=`, in two words. 0 for every other group.
#[allow(clippy::too_many_arguments)]
pub(crate) fn bare_keys(px: &crate::frame::px::Px<'_, '_>, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>, reps: &[Option<crate::table::emit_group::RepSrc>], tfs: &[TypeFacts], colls: &[KeyColl], wpk: &[u8], home: gravity_program::LifeId) -> u64 {
    use gravity_decompose::ops::shared::agg::GroupForm;
    if !engaged(px.strategies) || g.form != GroupForm::Hashed || g.two_pass.is_some() || home != g.producer || g.keys.len() > 64 || sorted_at_emit(px.gx, g) {
        return 0;
    }
    let mut m = 0u64;
    for (i, k) in g.keys.iter().enumerate() {
        let tf = &tfs[i];
        let datum = reps[i].is_some_and(|r| r.datum && !r.nocode && !r.derived && !r.imaged);
        if datum && k.resno != 0 && !k.image.is_float() && tf.image.is_bytes() && !tf.image.is_bpchar() && tf.user == 0 && tf.typlen == -1 && colls[i].bytes_eq && wpk[i] == 2 {
            m |= 1 << i;
        }
    }
    m
}

/// Whether group `g`'s emit orders its records by the keys' copies (table/emit_group.rs's `sort`: a top over the groups,
/// an in-place group sort, a sorted group read in key order, a grouped DISTINCT's key order), or no pipeline drains it.
fn sorted_at_emit(gx: &crate::graph::Gx<'_, '_>, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>) -> bool {
    let end = g.producer.0 as usize;
    let Some(q) = (end + 1..gx.pipelines().len()).find(|&q| matches!(gx.pipelines()[q].source, gravity_decompose::Source::Breaker { slot } if slot == g.slot)) else { return true };
    gx.end_need(q) == crate::face::kinds::KindId::RunsTopkGroups || crate::frame::iface::sorts_group_in_place(gx, q) || crate::frame::iface::order_read(gx, g) || crate::frame::iface::key_ordered(gx, g)
}

/// Whether the non-NULL varlena at `p` (an `I64` address) has an inline (1-byte or uncompressed 4-byte) header and at most
/// `INLINE` payload bytes: the values a `bare_keys` key keeps no copy of (an `I8` 0 / 1).
pub(crate) fn small(b: &mut FunctionBuilder<'_>, p: Value) -> Value {
    let ptr = b.conv(ConvOp::IntToPtr, Type::Ptr, p);
    let v = gravity_expr::siphash::varlena(b, ptr);
    let (zero, max) = (b.iconst(Type::I64, 0), b.iconst(Type::I64, super::hash_tagged::INLINE));
    let good = b.icmp(IntCond::Eq, v.bad, zero);
    let short = b.icmp(IntCond::Sle, v.len, max);
    b.int_bin(IntBinOp::And, good, short)
}

/// The value of an inline canonical image `[w0, w1]` (`[b0..7, b8..14 | len << 56]`) as a varlena (a 4-byte header and
/// the `len` bytes) in a frame slot valid for the row; the slot's address, an `I64`.
pub(crate) fn render(b: &mut FunctionBuilder<'_>, w0: Value, w1: Value) -> Value {
    let slot = b.declare_stack_slot(24, 8);
    let out = b.stack_addr(slot);
    let (s56, two, four) = (b.iconst(Type::I64, 56), b.iconst(Type::I64, 2), b.iconst(Type::I64, 4));
    let len = b.int_bin(IntBinOp::Ushr, w1, s56);
    let size = b.int_bin(IntBinOp::Add, len, four);
    let head = b.int_bin(IntBinOp::Shl, size, two); // SET_VARSIZE: the 4-byte uncompressed header's size << 2
    let head = b.conv(ConvOp::Trunc, Type::I32, head);
    b.store(out, 0, head);
    b.store_unaligned(out, 4, w0);
    b.store_unaligned(out, 12, w1); // its top byte (the length) lands at offset 19, past the at most 19 bytes the value uses
    b.conv(ConvOp::PtrToInt, Type::I64, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_interp::Interp;
    use gravity_ir::builder::ProgramBuilder;

    /// `render` of the image words `w`, its 24 bytes copied to `out` inside the run (the slot is the run's frame's).
    fn run_render(w: [u64; 2], out: &mut [u8; 24]) {
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("t", &[Type::I64, Type::I64, Type::I64], &[Type::I64]);
        let mut b = pb.define(f);
        let (w0, w1, o) = (b.params()[0], b.params()[1], b.params()[2]);
        let r = render(&mut b, w0, w1);
        let (r, o) = (b.conv(ConvOp::IntToPtr, Type::Ptr, r), b.conv(ConvOp::IntToPtr, Type::Ptr, o));
        for i in 0..3 {
            let v = b.load(Type::I64, r, 8 * i);
            b.store_unaligned(o, 8 * i, v);
        }
        let z = b.iconst(Type::I64, 0);
        b.ret(&[z]);
        let prog = pb.finish();
        Interp::new(&prog).run(f, &[w[0], w[1], out.as_mut_ptr() as u64]).expect("runs");
    }

    /// `small` over the varlena at `p`, as 0 / 1.
    fn run_small(p: *const u8) -> u64 {
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("t", &[Type::I64], &[Type::I64]);
        let mut b = pb.define(f);
        let p_ = b.params()[0];
        let s = small(&mut b, p_);
        let s = b.conv(ConvOp::Zext, Type::I64, s);
        b.ret(&[s]);
        let prog = pb.finish();
        Interp::new(&prog).run(f, &[p as u64]).expect("runs")[0]
    }

    /// One image per text key under the road: rep/varkey.rs asks key.canon ahead of key.tagged and key.coll in both its
    /// loops (the hash image and the order prefix), so no other form answers a byte-equal key while key.canon does.
    #[test]
    fn key_canon_is_asked_before_the_other_text_key_images() {
        let src = include_str!("../varkey.rs");
        let loops: Vec<&str> = src.lines().filter(|l| l.contains("for form in [")).collect();
        assert_eq!(loops.len(), 2, "varkey.rs asks the forms in two loops");
        for l in loops {
            let at = |f: &str| l.find(f).unwrap_or_else(|| panic!("{f} asked in {l:?}"));
            assert!(at("ImageForm::Canon") < at("ImageForm::Tagged") && at("ImageForm::Canon") < at("ImageForm::Coll"), "{l:?}");
        }
    }

    /// The emit's render of an inline canonical image (`gravity_dict::fp::canon_inline`, the one constructor) is the
    /// value's varlena: a 4-byte uncompressed header of `4 + len` and the bytes, for every length 0..=15, multibyte too.
    #[test]
    fn an_inline_image_renders_back_to_its_value() {
        for s in ["", "a", "abcdefgh", "abcdefghi", "abcdefghijklmno", "Ä-Öü €", "ab  "] {
            let b = s.as_bytes();
            let mut out = [0u8; 24];
            run_render(gravity_dict::fp::canon_inline(b), &mut out);
            let head = u32::from_le_bytes([out[0], out[1], out[2], out[3]]);
            assert_eq!((head & 3, head >> 2), (0, 4 + b.len() as u32), "{s:?}");
            assert_eq!(&out[4..4 + b.len()], b, "{s:?}");
        }
    }

    /// A value keeps no copy only behind an inline header and at most 15 bytes: a 1-byte or uncompressed 4-byte header
    /// of 0..=15 bytes does, 16 bytes and past, a compressed 4-byte header and a TOAST pointer (0x01) do not.
    #[test]
    fn a_small_value_is_one_of_at_most_15_bytes_behind_an_inline_header() {
        for len in 0..=20usize {
            let mut one = vec![((len as u8 + 1) << 1) | 1];
            one.extend(std::iter::repeat_n(b'x', len));
            assert_eq!(run_small(one.as_ptr()), u64::from(len <= 15), "1-byte header, {len}");
            let mut four = (((len as u32) + 4) << 2).to_le_bytes().to_vec();
            four.extend(std::iter::repeat_n(b'y', len));
            assert_eq!(run_small(four.as_ptr()), u64::from(len <= 15), "4-byte header, {len}");
            let mut packed = ((((len as u32) + 4) << 2) | 2).to_le_bytes().to_vec();
            packed.extend(std::iter::repeat_n(b'z', len + 4));
            assert_eq!(run_small(packed.as_ptr()), 0, "compressed, {len}");
        }
        let toast = [1u8, 18, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(run_small(toast.as_ptr()), 0, "a TOAST pointer");
    }
}

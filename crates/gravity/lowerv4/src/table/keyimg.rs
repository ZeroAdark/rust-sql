//! The key image face (plan §3.1; H21). A key's image is the words that the one KeyHash hashes and that equality compares.
//! It is built from each key cell's Rep hash arm (rep/mod.rs `hash_image`), in key order, with the flags words (one per 64
//! keys) when any key is nullable (`gravity_expr::emit::hash::emit_image`). Every keyed table, build, set op and partition
//! route keys through this file and `emit_key_hash`; none names a mixing constant or a hash bit (hash-contract.md §3.5,
//! M0-RIG's lint).
//!
//! Equality is PG's for the key's type and collation: an exact image decides a hit by its words. An inexact image (a tagged
//! head / tail of a long text, a hosted type) confirms a hit with each key's Rep `eq` against the stored key. The order
//! images (DESC / NULLS folded in, comparable across the two sides of a merge) are the Rep × order kinds.
use gravity_abi::breaker::KeyKind;
use gravity_decompose::ops::shared::agg::KeyDecl;
use gravity_decompose::catalog::keysem::image::{held_apart, held_form};
use gravity_decompose::catalog::typefacts::{KeyColl, TypeFacts};
use gravity_expr::emit::hash::{emit_image_then, ImageValues, KeyPart};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Type, Value};
use gravity_program::Rep;
use mcx::{CollectIn, GrowIn, PgVec};

use crate::graph::Gx;
use crate::rep::{self, CellRef, Lx, Scope, Words};

crate::strategies! {
    /// A low-cardinality varlena key as a small integer: pgrc codes mapped once per entry to a global index, heap images
    /// interned per worker (trimmed like bpcharcmp); carried through builds and folds, names rendered at the finish. No
    /// mechanism yet: its readers take the straight form (rep/varkey.rs asks it only while it is on).
    KEY_SURROGATE = 84, "key.surrogate", VarlenaKey;
}

crate::hook_variants! {
    /// The key image forms (H21), one or two words per key over every Rep.
    "H21 -" pub enum ImageForm {
        /// Int-width unification: every integer width compares as one word.
        Word = Kind(RepWordHash),
        /// Numeric at a common scale.
        Numeric = Kind(RepPackedHash),
        /// Code across dictionaries: the Fp128 of the render. Codes of the same dictionary compare as codes.
        Code = Kind(RepCodeHash),
        /// Text: ONE canonical image for every source (textkeys step 3): the bytes up to 15, else the Fp128 under one bit.
        Canon = Strategy(KEY_CANON),
        /// Text: a tagged image of the bytes' head and tail.
        Tagged = Strategy(KEY_TAGGED),
        /// A collation's sort-key image (strnxfrm).
        Coll = Strategy(KEY_COLL),
        /// A low-cardinality varlena key as a small integer surrogate: pgrc codes mapped once per entry to a global index,
        /// heap images interned per worker; the names are rendered at the finish.
        Surrogate = Strategy(KEY_SURROGATE),
        /// Fixed-width types by value: uuid, interval, name.
        Fixed = Kind(RepFixedHash),
        /// Other varlena types' images (bpchar, bytea, ...).
        Varlena = Kind(RepVarlenaHash),
        /// Types with no native image: the fmgr hash, with eq by the fmgr comparator.
        Hosted = Kind(RepHostedHash),
        /// Order images with DESC and NULLS folded in, comparable across sides.
        Order = Kind(RepWordOrder),
        /// The partition hash taken once, at a run's open.
        RunOpen = Strategy(HASH_RUNOPEN),
        /// KEY_CODE_IF: a key image in SSA over codes.
        CodeIf = Strategy(KEY_CODEIF),
    }
}

/// KEY_CODE_IF (road key.codeif, keyed/key_codeif.rs): a group key `CASE WHEN <pred> THEN <text column> [WHEN <pred> THEN
/// <the same column> ...] ELSE <text constant> END` (a NULL constant when there is no ELSE), `(the CaseExpr node, the THEN
/// Var node, the ELSE Const node)` when `e` has the form. Its image is `select(pred, fp[code],
/// fp(ELSE))` over the THEN lane's codes (step/percode_guard.rs), its representative the THEN code, rendered by the image.
pub trait CodeIfMech: Sync {
    fn form<'a>(&self, e: types_nodes::Node<'a>) -> Option<[types_nodes::Node<'a>; 3]>;
}

pub struct CodeIfImpl(pub &'static dyn CodeIfMech);

/// Road key.codeif's form reader while the road is on (built, not dropped, not switched off), else `None`.
pub fn code_if(cx: &impl crate::admit::refusal::Site) -> Option<&'static dyn CodeIfMech> {
    ImageForm::CodeIf.on().then(|| ImageForm::CodeIf.need::<CodeIfImpl>(cx).0)
}

/// How a sort key's tail compares in RowCmp: its bytes (memcmp, then length), through a collation (`varstr_cmp`, PG's
/// memcmp tie-break kept), or through a hosted comparator (`cmp` a btree support proc) under `coll`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TailForm {
    Bytes,
    Collated { coll: u32 },
    Hosted { cmp: u32, coll: u32 },
}

/// The varlena key roads' face (key.surrogate, key.tagged, key.coll; face/varkey.rs asks them in order). It lives here, as
/// CodeIfMech does, so the keyed ends outside M6's registry reach key.coll by name (D823).
pub trait VarKeyMech: Sync {
    /// The exact hash image under `coll`, or `None` when this road does not engage (the caller takes the next form).
    fn hash(&self, lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, coll: KeyColl, scope: Scope) -> Option<Words>;
    /// The order prefix word and the tail form, or `None`.
    fn order(&self, lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, coll: KeyColl) -> Option<(Value, TailForm)>;
}

pub struct VarKeyImpl(pub &'static dyn VarKeyMech);

/// One key of an image: its cell in the row, the cell type's facts, its equality scope and its collation. A key under a
/// collation whose equal bytes are not its equal values (nondeterministic, D823) images as key.coll's sort-key Fp128.
#[derive(Clone, Copy)]
pub struct KeyCell<'a> {
    pub cell: CellRef<'a>,
    pub tf: &'a TypeFacts,
    pub scope: Scope,
    pub coll: KeyColl,
}

/// A key image: its words (the KeyHash input, `emit_key_hash`), whether equal words decide a hit, and `code`: when the image
/// is exactly one `Rep::Code` key, that key's code in the current row (`I64`, the value its Fp128 load indexes by), all ones
/// (`u64::MAX`) on a NULL row, so a code-indexed front (code.front_part) sends NULL past its capacity with no extra test.
/// Every other image carries None: the image width alone cannot tell an Fp128 pair from a code plus its flags word.
/// `index` is code.front_dense's (`FrontIndex`). An unread code is a dead load `ir::promote::dce` removes.
#[derive(Clone, Debug)]
pub struct ImageWords {
    pub words: ImageValues,
    pub exact: bool,
    pub code: Option<Value>,
    pub index: Option<FrontIndex>,
    pub widths: Vec<u8>, // each key's words, in key order (`image`); empty for a plain image
}

/// What code.front_dense indexes a row by: an image of two or more `Rep::Code` keys carries each key's code (all ones on
/// its NULL row) and its site lane, whose dictionary size the front's index reads; for a key derived from one code lane
/// KEYED sets the source code after `image`, which cannot see derivedness.
#[derive(Clone, Debug)]
pub enum FrontIndex {
    Codes(Vec<(Value, u8)>),
    Source(Value),
}

impl ImageWords {
    /// An image built from words at hand (a carried open run, a lone word): no code, no pair.
    pub fn plain(words: ImageValues, exact: bool) -> ImageWords {
        ImageWords { words, exact, code: None, index: None, widths: Vec::new() }
    }
}

/// The image of `keys` in key order. Each key goes through its Rep's hash arm (at most two words), which refuses `NotBuilt`
/// by the arm's kind while that arm's milestone has not landed; a nullable key contributes its null flag to the flags words
/// (CAP-28: one per 64 keys, `emit_image`, so any number of nullable keys images apart).
pub fn image(lx: &mut Lx<'_, '_, '_>, keys: &[KeyCell<'_>]) -> ImageWords {
    image_skip(lx, keys, None, |_| None)
}

/// `image`, where `skip` = `(flag, mask)` gives each key of `mask` (bit i: key i) the words 0 on a row where `flag` holds
/// (D3324 (b), ops/agg/strategies/dependent.rs: a unique build key's dependent text keys) and its own image elsewhere,
/// its NULL flag kept either way; `tail`'s word, if any, ends the image after its flags words (`emit_image_then`).
pub fn image_skip(lx: &mut Lx<'_, '_, '_>, keys: &[KeyCell<'_>], skip: Option<(Value, u64)>, tail: impl FnOnce(&mut FunctionBuilder<'_>) -> Option<Value>) -> ImageWords {
    let mcx = lx.mcx;
    let mut exact = true;
    let mut words = mcx::reserved_in(keys.len(), mcx);
    for (i, k) in keys.iter().enumerate() {
        // D2640: a key with a user key (`TypeFacts::user`: a hashed group's under a user `=`) is its classes' image under
        // any collation (rep.hosted.hash `user_words` calls the operator's procs under it), never the collation's sort key
        let w = match skip.filter(|s| i < 64 && s.1 >> i & 1 == 1) {
            Some((flag, _)) => skipped(lx, k, flag),
            None if k.coll.bytes_eq || k.tf.user != 0 => rep::hash_image(lx, k.cell, k.tf, k.scope),
            None => coll_image(lx, k),
        };
        exact &= w.exact;
        let (mut a, mut n) = ([k.cell.val; 2], 0);
        for v in w.w.into_iter().flatten() {
            a[n] = v;
            n += 1;
        }
        words.push_in((a, n, k.cell.null));
    }
    let is_code = |k: &KeyCell<'_>| matches!(k.cell.cell.rep, Rep::Code { .. }) && !k.cell.cell.image_only() && k.coll.bytes_eq;
    let mut lanes: Option<PgVec<'_, u8>> = Some(mcx::reserved_in(keys.len(), mcx));
    for k in keys {
        let lane = match k.cell.cell.rep {
            Rep::Code { dict } if is_code(k) => dict.lane,
            _ => {
                lanes = None;
                break;
            }
        };
        if let Some(l) = lanes.as_mut() {
            l.push_in(lane);
        }
    }
    let (code, index) = match (keys, lanes) {
        ([k], _) if is_code(k) => (Some(one_code(lx, k, words[0].0[0])), None),
        (_, Some(l)) if l.len() >= 2 => (None, Some(FrontIndex::Codes(keys.iter().zip(&words).zip(l).map(|((k, w), l)| (one_code(lx, k, w.0[0]), l)).collect()))), // alloc: Program
        _ => (None, None),
    };
    let parts: PgVec<'_, KeyPart<'_>> = words.iter().map(|(a, n, null)| KeyPart { words: &a[..*n], null: *null }).collect_in(mcx);
    let widths = words.iter().map(|w| w.1 as u8).collect(); // alloc: Program
    ImageWords { words: emit_image_then(lx.b, &parts[..], tail), exact, code, index, widths }
}

/// Key `k`'s image words, 0 where `flag` holds: its hash image computed on the other branch alone (the memo forked around it).
fn skipped(lx: &mut Lx<'_, '_, '_>, k: &KeyCell<'_>, flag: Value) -> Words {
    let (l_img, l_zero) = (lx.b.create_block(&[]), lx.b.create_block(&[]));
    lx.b.cond_br(flag, (l_zero, &[]), (l_img, &[]));
    lx.b.switch_to(l_img);
    let outer = lx.memo.fork();
    let w = rep::hash_image(lx, k.cell, k.tf, k.scope);
    lx.memo.restore(outer);
    let (mut vals, mut n) = ([k.cell.val; 2], 0);
    for v in w.w.into_iter().flatten() {
        vals[n] = v;
        n += 1;
    }
    let tys = [vals[0], vals[1]].map(|v| lx.b.value_type(v));
    let l_join = lx.b.create_block(&tys[..n]);
    lx.b.br(l_join, &vals[..n]);
    lx.b.switch_to(l_zero);
    let mut zs = vals;
    for (z, &t) in zs.iter_mut().zip(&tys).take(n) {
        *z = lx.b.iconst(t, 0);
    }
    lx.b.br(l_join, &zs[..n]);
    lx.b.switch_to(l_join);
    let ps = lx.b.block_params(l_join);
    Words { w: [ps.first().copied(), ps.get(1).copied()], exact: w.exact }
}

/// D3224 A4: `image`'s front index of `keys` alone (its `code` for one key, its `index` for two or more), with no image word
/// read but the one a same-dictionary code is (`one_code`'s `w0`, as `image` takes it): `None` unless every key is a code
/// key `image` indexes. A front probes the record it holds by it (`TableMech::probe`); its `words` are empty, so it never
/// images a record.
pub fn codes(lx: &mut Lx<'_, '_, '_>, keys: &[KeyCell<'_>]) -> Option<ImageWords> {
    let is_code = |k: &KeyCell<'_>| matches!(k.cell.cell.rep, Rep::Code { .. }) && !k.cell.cell.image_only() && k.coll.bytes_eq;
    if keys.is_empty() || !keys.iter().all(is_code) {
        return None;
    }
    let mut cs = Vec::with_capacity(keys.len()); // alloc: Program
    for k in keys {
        let Rep::Code { dict } = k.cell.cell.rep else { return None };
        let same = matches!((k.scope, k.cell.cell.rep), (Scope::SameDict(d), Rep::Code { dict }) if d == dict);
        let w0 = if same { rep::hash_image(lx, k.cell, k.tf, k.scope).w.into_iter().flatten().next().unwrap_or(k.cell.val) } else { k.cell.val };
        cs.push((one_code(lx, k, w0), dict.lane)); // alloc: Program
    }
    let (code, index) = if cs.len() == 1 { (Some(cs[0].0), None) } else { (None, Some(FrontIndex::Codes(cs))) };
    Some(ImageWords { words: ImageValues { words: Vec::new() }, exact: true, code, index, widths: Vec::new() })
}

/// The record kind and words key `k`'s image takes in pipeline `producer`'s tables (every keyed table over the key set and
/// the grouped DISTINCT pair owner read these): decompose's, except two Fp128 words for a key under a nondeterministic
/// collation (key.coll's sort-key image, D823) and for a narrow numeric (decompose's one mantissa word, D197) whose cell is a
/// varlena, a heap column's lane or a computed numeric: `image` hashes it through rep.varlena.hash, the Fp128 of its
/// normalized digits (M6-REPS batch B). A packed lane's numeric keeps its mantissa word. D2365: a cell whose rep the
/// producer's row does not resolve (a column through a merge, nest or drain join) is a varlena too: only a packed lane's
/// cell images in one word, so one unresolved was the record one word short of its image.
/// D3857: or two Fp128 words for a packed lane's narrow numeric on a set operation's side whose other side keys the column
/// in another form (`packed_set_key`): both sides then key it by the scale-free image.
pub fn record_words(gx: &Gx<'_, '_>, producer: usize, k: &KeyDecl<'_>) -> (KeyKind, u8) {
    if packed_set_key(gx, producer, k) {
        return (KeyKind::Fp128, 2);
    }
    base_record_words(gx, producer, k)
}

/// `record_words` before D3857's set-side rule (the rule's own comparison of the two sides reads this one).
pub fn base_record_words(gx: &Gx<'_, '_>, producer: usize, k: &KeyDecl<'_>) -> (KeyKind, u8) {
    let varlena = k.image.is_mantissa() && k.words == 1 && !matches!(crate::cell::key_cell_rep(gx, producer, k), Some(Rep::PackedNum { .. }));
    if gx.facts.coll(k.coll).bytes_eq && !varlena { (k.kind, k.words) } else { (KeyKind::Fp128, 2) }
}

/// D3857 (c-random over pgrc copies: rnd-0313 / 0682 / 1869 / 2226 / 4123, a hashed INTERSECT / EXCEPT [ALL] whose
/// numeric column is a pgrc numeric(p, s) lane on one side and a heap numeric, another scale or a computed numeric on the
/// other): whether key `k` of the group pipeline `producer` is a packed lane's narrow mantissa (one word at its
/// lane's scale, copy_words.rs `packed_key`) on a grouped set operation's side whose other side holds the same column in
/// another numeric form (keysem `held_form` over `base_record_words`: a varlena's scale-free Fp128, or a packed lane at
/// another scale). Such a side keys the column by the scale-free Fp128 instead: its record keeps two words
/// (`record_words`), its cell images through
/// rep.packed.hash (the varlena Fp128 of the value PostgreSQL stores: 2.50 = 2.500, as numeric_eq), and the record keeps
/// one member's datum for the output (rep.packed.datum's image, emit_key.rs `keep_datums`), as D3848's key under the
/// scale-free decl. The two sides then compare one image (keysem `set_key`), as PG's hashed SetOp compares by
/// numeric_eq. A sorted set operation's walk (setop/stream.rs) keeps its own comparison.
pub(crate) fn packed_set_key(gx: &Gx<'_, '_>, producer: usize, k: &KeyDecl<'_>) -> bool {
    use gravity_decompose::ops::shared::setop::{SetDesc, SetForm};
    if !k.image.is_mantissa() || k.words != 1 || !matches!(crate::cell::key_cell_rep(gx, producer, k), Some(Rep::PackedNum { .. })) {
        return false;
    }
    let Ok(life) = u16::try_from(producer).map(gravity_program::LifeId) else { return false };
    let Some(g) = gx.graph_group(life) else { return false };
    let Some(i) = g.keys.iter().position(|x| x.word == k.word && x.resno == k.resno) else { return false };
    let ps = gx.pipelines();
    let peer = (0..ps.len()).find_map(|q| {
        ps[q].ops.iter().find_map(|op| match op {
            gravity_decompose::Op::SetOp(SetDesc::Set { left, right, form, .. }) if !matches!(form, SetForm::Stream { .. }) => {
                let (l, r) = (gx.life_of(q, *left), gx.life_of(q, *right));
                if l == Some(life) { r } else if r == Some(life) { l } else { None }
            }
            _ => None,
        })
    });
    let Some(pg) = peer.and_then(|p| gx.graph_group(p)) else { return false };
    let Some(pk) = pg.keys.get(i) else { return false };
    let a = held_form(k, base_record_words(gx, producer, k));
    let b = held_form(pk, base_record_words(gx, pg.producer.0 as usize, pk));
    held_apart(&a, &b)
}

/// A nondeterministic key's image: key.coll's Fp128 of the provider's sort key (PG's `hashtext` under the collation; equal
/// sort keys are equal values), exact, for a code (its render) and heap text alike, so every side of a table agrees.
fn coll_image(lx: &mut Lx<'_, '_, '_>, k: &KeyCell<'_>) -> Words {
    let m = ImageForm::Coll.need::<VarKeyImpl>(&*lx).0;
    match m.hash(lx, k.cell, k.tf, k.coll, Scope::Program) {
        Some(w) => w,
        None => crate::refuse!(*lx, crate::admit::refusal::Reason::Bug { payload: "key.coll declined a nondeterministic key" }),
    }
}

/// The one code key's code, all ones on a NULL row. A same-dictionary image IS the code (`w0`), so no word is re-read.
fn one_code(lx: &mut Lx<'_, '_, '_>, k: &KeyCell<'_>, w0: Value) -> Value {
    let same = matches!((k.scope, k.cell.cell.rep), (Scope::SameDict(d), Rep::Code { dict }) if d == dict);
    let c = if same { w0 } else { rep::code_word(lx, k.cell) };
    match k.cell.null {
        Some(n) => {
            let ones = lx.b.iconst(Type::I64, -1);
            lx.b.select(n, ones, c)
        }
        None => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{At, Cell, CellLife, LaneId, Nullable};
    use crate::rep::{Frame, RepHosts};
    use gravity_decompose::catalog::typefacts::row_of;
    use gravity_interp::Interp;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_program::{DictRef, LaneLife, NumTypmod};

    const DICT: DictRef = DictRef { lane: 1, fp_table: 2 };

    /// `ImageWords.code` of one nullable code key (code 2, table 2 at `fps`) under `scope`, and `index` of that key
    /// twice, for a valid and a NULL row.
    fn code_of(scope: Scope, null: u64) -> Vec<u64> {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let c = Cell { rep: Rep::Code { dict: DICT }, typid: 25, num: NumTypmod::NONE, life: CellLife::of_lane(LaneLife::DICTIONARY), at: At::Lane(LaneId::col(1)), null: Nullable::Maybe };
        let mut pb = ProgramBuilder::new();
        let render = pb.declare_host("agg_render", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::Ptr), 0, Default::default());
        let f = pb.declare_func("k", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::I64], &[Type::I64, Type::I64, Type::I64]);
        let mut b = pb.define(f);
        let (ctx, elem, tables, n) = (b.params()[0], b.params()[1], b.params()[2], b.params()[3]);
        let zero = b.iconst(Type::I64, 0);
        let isnull = b.icmp(gravity_ir::IntCond::Ne, n, zero);
        let code = {
            let mut cx = Lx::new(&mut b, 1, rig.mcx());
            cx.at = Some(Frame { ctx, site: Some(0), hosts: RepHosts { render }, tables: Some(tables) });
            let k = KeyCell { cell: CellRef { cell: &c, val: elem, null: Some(isnull) }, tf: row_of(25).unwrap(), scope, coll: KeyColl::NONE };
            let one = image(&mut cx, &[k]);
            assert!(one.index.is_none(), "one key carries no pair");
            let two = image(&mut cx, &[k, k]);
            assert!(two.code.is_none(), "two keys carry no code");
            let Some(FrontIndex::Codes(cs)) = two.index else { panic!("two code keys carry both codes") };
            assert_eq!(cs.iter().map(|c| c.1).collect::<Vec<_>>(), [DICT.lane; 2], "each code's lane");
            let three = image(&mut cx, &[k, k, k]);
            assert!(matches!(three.index, Some(FrontIndex::Codes(ref c)) if c.len() == 3), "three code keys carry three codes");
            (one.code.expect("one code key carries its code"), cs[0].0, cs[1].0)
        };
        b.ret(&[code.0, code.1, code.2]);
        let prog = pb.finish();
        let fps = [[1u64, 2], [3, 4], [5, 6]];
        let mut words = [0u64; 9];
        words[2 * 3] = fps.as_ptr() as u64;
        let cw = [2u32];
        Interp::new(&prog).run(f, &[0, cw.as_ptr() as u64, words.as_ptr() as u64, null]).expect("runs")
    }

    #[test]
    fn code_keys_carry_their_codes_and_all_ones_when_null() {
        for scope in [Scope::Program, Scope::SameDict(DICT)] {
            assert_eq!(code_of(scope, 0), [2, 2, 2], "{scope:?}");
            assert_eq!(code_of(scope, 1), [u64::MAX; 3], "{scope:?}");
        }
    }
}

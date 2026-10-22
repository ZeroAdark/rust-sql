//! The group sort's words over a key's kept copy (M6-HOSTED, V4R-440; split from runs/topk_groups.rs under the landing
//! budget): which copy the group sort (host_abi `agg_sort`) may order, and how. A text / varchar / bpchar or bytea copy
//! compares as text (keyed/coderep.rs `copy_word`, the callers' own arm); a `name`'s fixed 64-byte image compares as text
//! before its NUL (NM, `SK_NAME`); any other copy (an array, a record, a range, a numeric no packed lane holds, an inet)
//! compares by its type's btree comparator through fmgr (SK_CMP, D1165), resolved at lowering (`gravity_fn::resolved`).
//! A hosted key the record keeps only as its Fp128 has no copy to order and refuses D1165. Owner M6-HOSTED.
use gravity_abi::breaker::KeyKind;
use gravity_kernels::host_abi::{sk_key_cmp, sk_slot_cmp, SK_CMP, SK_HEAP, SK_NAME, SK_TEXT, SK_TEXT_COLL_SHIFT, SK_TEXT_SITE_SHIFT, SK_TEXT_SLOT_SHIFT};
use gravity_decompose::ops::shared::agg::KeyDecl;
use gravity_decompose::catalog::typefacts::{KeyColl, TypeFacts};
use gravity_ir::Value;
use gravity_program::{NumTypmod, Rep};
use mcx::CollectIn;
use types_core::catalog::NUMERICOID;

use crate::cell::{At, Cell};
use crate::face::kinds::KindId;
use crate::frame::px::Px;
use crate::graph::Gx;
use crate::rep::{CellRef, Lx};
use crate::admit::refusal::{Reason, ShapeWhy};

use crate::table::rec::GroupRec;

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// The sort word of key `i`'s kept copy in record slot `slot`: a text / bytea copy's (keyed/coderep.rs `copy_word`), a
/// name's (NM) or, ordered by its type's comparator, any other's (SK_CMP). `opno`: the sort operator (a final top's
/// key), `None` for the sorted group's own record sort (the key's default order).
pub(crate) fn word(px: &Px<'_, '_>, rec: &GroupRec<'_, '_>, i: usize, slot: u64, node: i32, opno: Option<u32>, coll: u32) -> u64 {
    if rec.bare >> i & 1 == 1 {
        // textkeys step 3: rep/varlena/key_canon.rs `bare_keys` keeps no copy only where no emit sorts by one
        crate::refuse!(node, crate::admit::refusal::Reason::Bug { payload: "a sorted group key whose record keeps no copy (key.canon bare_keys)" });
    }
    copy_word(px, rec, i, slot, node, opno, coll).unwrap_or_else(|| crate::dict::coderep::copy_word(slot, coll, crate::dict::coderep::sk_bpchar(px.gx, rec.g.keys[i].tf)))
}

fn copy_word(px: &Px<'_, '_>, rec: &GroupRec<'_, '_>, i: usize, slot: u64, node: i32, opno: Option<u32>, coll: u32) -> Option<u64> {
    let tf = &rec.tfs[i];
    if tf.typid == types_core::catalog::NAMEOID && tf.typlen == 64 && !tf.byval {
        // M6-HOSTED NM (D1165): a name's 64-byte fixed image compares as text, its bytes before the NUL, under `coll`
        return Some(SK_TEXT | SK_HEAP | SK_NAME | slot << SK_TEXT_SLOT_SHIFT | (coll as u64) << SK_TEXT_COLL_SHIFT);
    }
    let copy = if tf.typlen > 0 && !tf.byval { Rep::FixedPtr { len: tf.typlen as u16 } } else { Rep::VarlenaPtr };
    if crate::rep::arm_kind(copy, crate::rep::Accessor::Order, tf) == KindId::RepVarlenaOrder {
        return None; // a text-like or bytea copy: its bytes under the key's collation
    }
    Some(cmp_word(px, rec, i, slot, node, opno, coll))
}

/// SK_CMP (D1165): the copy ordered by its type's own comparator (its default btree opclass's
/// support function 1, the order the group's equality agrees with), called through fmgr under `coll` by the group sort
/// (host_abi `copy_cmp`). The word's site byte names the key (`procs`: at most 256 keys). A sort operator of another
/// opclass (any catalog answer but the default comparator) refuses by name, as does a comparator that is not internal /
/// C or does not resolve (KeySem's `keysem::rep::sort_cmp_site`); a type with no comparator stays D1165.
fn cmp_word(px: &Px<'_, '_>, rec: &GroupRec<'_, '_>, i: usize, slot: u64, node: i32, opno: Option<u32>, coll: u32) -> u64 {
    let (kd, own) = (&rec.g.keys[i], own_cmp(px, rec, i));
    if own == 0 {
        crate::rep::hosted_sorted(node, crate::rep::key_kind(kd.kind, Some(kd.image), false, crate::rep::Accessor::Order));
    }
    if let Some(op) = opno {
        match px.gx.catalog.ordering_cmp_proc(op) {
            Ok(p) if p == own => {}
            // no catalog answer (a catalog with no such face): a built-in ordering operator of its type's default opclass
            Ok(0) | Err(_) if gravity_decompose::catalog::opdir::builtin_dir(op).is_some() => {}
            _ => shape(node, "a sorted group key ordered by an operator class other than its type's default"),
        }
    }
    if i > 0xff {
        shape(node, "a sorted group key past the 256th (an SK_CMP word's site byte names its key)");
    }
    // D2039: past eight keys the descriptor runs on (table/emit_group.rs `desc_cmp`, wide) and this key
    // still orders by its type's comparator through fmgr, which runs the user code behind it (a user range's subtype
    // opclass, range_cmp's) as PG's Sort does: only the call count differs, and a call
    // count is not compared (D2039 closed), so the key reach past eight no longer refuses
    // D1391: a narrow numeric whose record keeps one mantissa word because its cell's rep is not
    // seen at layout (a join's inner column, face/keyimg.rs `record_words`) may image as a varlena's two Fp128 words
    if rec.tfs[i].typid == NUMERICOID && rec.g.keys[i].image.is_mantissa() && rec.key_words(i).1 == 1 {
        shape(node, "an SK_CMP narrow numeric key whose record keeps its mantissa word over a cell of unseen rep (D1391)");
    }
    if let Err(why) = gravity_fn::typesem::keysem::rep::sort_cmp_site(px.gx.catalog.func(own).ok().and_then(|f| f.prolang), own, rec.tfs[i].typid) {
        shape(node, why);
    }
    SK_TEXT | SK_HEAP | SK_CMP | slot << SK_TEXT_SLOT_SHIFT | (i as u64) << SK_TEXT_SITE_SHIFT | (coll as u64) << SK_TEXT_COLL_SHIFT
}

/// D1165 residual: the sort word of enum key `i`, whose record word is its oid. It orders by
/// its type's comparator (enum_cmp: an even oid by its value, an odd one, an `ADD VALUE .. BEFORE / AFTER`, by
/// pg_enum.enumsortorder), never by the word: `SK_CMP` on the key word, its key in the site byte (host_abi `sk_key_cmp`),
/// under `cmp_word`'s checks (the default opclass's comparator, a native one, the pipeline serial by its resolution).
/// Back: the bits to or into the key word's sort word.
pub(crate) fn enum_word(px: &Px<'_, '_>, rec: &GroupRec<'_, '_>, i: usize, node: i32, opno: Option<u32>) -> u64 {
    SK_CMP | (cmp_word(px, rec, i, 0, node, opno, 0) & 0xff << SK_TEXT_SITE_SHIFT)
}

/// A hosted key (its hash arm rep.hosted.hash: an array, a record, a range, ...) the record keeps only as its Fp128: those
/// words are a hash, never an order, and no copy is kept for its comparator (D1165). Decided by the key's facts; a text /
/// numeric key's Fp128 is not this (the caller orders it). Back: the key's record words (`GroupRec::key_words`).
pub(crate) fn fp128_hosted(rec: &GroupRec<'_, '_>, i: usize, node: i32) -> (usize, usize) {
    let kd = &rec.g.keys[i];
    if kept_as_fp128(rec.layout.key_kinds(), &rec.wpk, i) && crate::rep::key_kind(kd.kind, Some(kd.image), false, crate::rep::Accessor::Hash) == KindId::RepHostedHash {
        crate::rep::hosted_sorted(node, crate::rep::key_kind(kd.kind, Some(kd.image), false, crate::rep::Accessor::Order));
    }
    rec.key_words(i)
}

/// Whether key `i`'s record words are an Fp128: the layout's kinds are per WORD (`wpk`: words per key), so key `i`'s kind
/// is its first word's, never `kinds[i]` (a two-word key before it shifts every later key's words).
fn kept_as_fp128(kinds: &[KeyKind], wpk: &[u8], i: usize) -> bool {
    kinds.get(wpk[..i].iter().map(|&w| w as usize).sum::<usize>()) == Some(&KeyKind::Fp128)
}

/// A narrow numeric group key over a packed pgrc lane at the key's own scale (its
/// decl the mantissa word, decompose's `scale` the lane's): the lane's facts. Its record keeps rep.packed.order's word, the
/// mantissa at the lane's one scale (one exact signed word, equal words equal values, `MANT_NINF < finite < MANT_PINF <
/// MANT_NAN` cmp_numerics' order), and the group keys, sorts and renders by that word: no copy is kept. A join over a
/// packed lane keys through rep.packed.hash (the varlena Fp128); a set operation or memo key at another scale refuses by name.
pub(crate) fn packed_key(gx: &Gx<'_, '_>, producer: usize, k: &KeyDecl<'_>) -> Option<NumTypmod> {
    match crate::cell::key_cell_rep(gx, producer, k) {
        // D3857: not a set side's key the other side holds in another form (keyimg.rs `packed_set_key`: the scale-free Fp128)
        Some(Rep::PackedNum { facts, .. }) if k.image.is_mantissa() && k.words == 1 && facts.scale == k.scale && !crate::table::keyimg::packed_set_key(gx, producer, k) => Some(facts),
        _ => None,
    }
}

/// The cell key `c` images through in a keyed group (keyed/hashagg_rec.rs `image_of`): over a packed lane, an int8 word cell
/// holding its order word (`packed_key`'s form), `None` for any other cell. A key over a packed lane at a scale other than
/// its mantissa decl's refuses by name: no common scale.
/// D3848 (c-random over pgrc copies: `SELECT p.p_price .. UNION SELECT 0`, `GROUP BY c0` over `SELECT 1.5 UNION ALL
/// SELECT p.p_price ..`): or `None` under the scale-free numeric decl (decompose's `NumNormalized` Fp128, the key's
/// sources of no one form: lanes or values of different scales). The cell then images through its Rep's hash arm,
/// rep.packed.hash: rep.varlena.hash's exact Fp128 of the value PostgreSQL stores for it, the image a heap numeric of
/// the same value has at any dscale (`1.5 = 1.50`, hash_numeric's equality), and the record keeps one member's datum for
/// the output (table/emit_key.rs `keep_datums`: rep.packed.datum's image, the lane's dscale), as for a heap numeric key.
pub(crate) fn packed_cell(cx: &mut Lx<'_, '_, '_>, node: i32, c: CellRef<'_>, k: &KeyDecl<'_>, tf: &TypeFacts) -> Option<(Cell, Value)> {
    let Rep::PackedNum { facts, .. } = c.cell.rep else { return None };
    if gravity_decompose::catalog::keysem::image::scale_free(k) {
        return None;
    }
    if !k.image.is_mantissa() || k.words != 1 || facts.scale != k.scale {
        shape(node, "a numeric group key over a packed lane at a scale other than the key's (no common scale)");
    }
    let Some(w) = crate::rep::order_words(cx, c, tf, KeyColl::NONE).w[0] else { crate::refuse!(node, Reason::Bug { payload: "a packed order image with no word" }) };
    Some((Cell { rep: Rep::Word { bytes: 8 }, typid: types_core::INT8OID, num: NumTypmod::NONE, life: c.cell.life, at: At::Value(0), null: c.cell.null }, w))
}

/// Key `i`'s comparator: its type facts' (a built-in type's default opclass), else (D1165 residual: an enum, a user
/// array, a user composite, whose facts carry none) the type cache's `TYPECACHE_CMP_PROC`, PostgreSQL's own default
/// btree comparator of the type (enum_cmp, btarraycmp, btrecordcmp); 0 when the type has none.
fn own_cmp(px: &Px<'_, '_>, rec: &GroupRec<'_, '_>, i: usize) -> u32 {
    match rec.tfs[i].cmp {
        0 => px.gx.catalog.type_cmp_proc(rec.tfs[i].typid).unwrap_or(0),
        c => c,
    }
}

/// The descriptor's comparator words (host_abi `DESC_PROCS`), one per sort word: an `SK_CMP` word's key's (or order
/// expression's, N6a) comparator,
/// resolved (`gravity_fn::resolved`: `cmp_word` resolved it already, so this only reads the interned id), 0 for any other.
pub(crate) fn procs<'g>(px: &Px<'g, '_>, rec: &GroupRec<'_, '_>, words: &[u64]) -> mcx::PgVec<'g, u64> {
    words.iter().map(|&w| {
        if w & (SK_TEXT | SK_HEAP | SK_CMP) != SK_TEXT | SK_HEAP | SK_CMP && !sk_key_cmp(w) && !sk_slot_cmp(w) {
            return 0;
        }
        let i = (w >> SK_TEXT_SITE_SHIFT & 0xff) as usize;
        // N6a: a site past the group's keys names an order expression (order_slot.rs `sort_word`)
        let cmp = if i < rec.g.keys.len() { own_cmp(px, rec, i) } else { crate::ops::sort::topn::order_slot::expr_cmp(px.gx, rec, i - rec.g.keys.len()) };
        match gravity_fn::resolved::resolve(cmp) {
            Ok(Ok(id)) => id as u64,
            _ => crate::refuse!(rec.node, Reason::Bug { payload: "an SK_CMP comparator that no longer resolves" }),
        }
    }).collect_in(px.mcx())
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::KeyKind::{Fp128, I64};
    use gravity_expr::catalog::{DomainFacts, FuncFacts, IoFacts, TextFacts, TypeFacts};
    use gravity_expr::{Catalog, PgCatalog};
    use gravity_fn::{Shim, Tier};
    use types_core::Oid;
    use types_error::PgResult;

    #[test]
    fn a_hosted_key_after_a_two_word_key_is_read_at_its_first_word() {
        // a text key (Fp128: two words), an int (one), then a hosted array key kept only as its Fp128 (words 3, 4)
        let (kinds, wpk) = ([Fp128, Fp128, I64, Fp128, Fp128], [2u8, 1, 2]);
        assert!(super::kept_as_fp128(&kinds, &wpk, 2), "the hosted key is asked D1165");
        assert_ne!(kinds.get(2), Some(&Fp128), "the key index read the int's word: D1165 was skipped");
        assert!(!super::kept_as_fp128(&kinds, &wpk, 1) && super::kept_as_fp128(&kinds, &wpk, 0));
    }

    /// `PgCatalog` with a few types' facts (a unit test has no syscache): (typid, len, typtype, elem).
    struct Types(&'static [(u32, i16, u8, u32)]);

    impl<'m> Catalog<'m> for Types {
        fn type_facts(&self, t: Oid) -> PgResult<TypeFacts> {
            let &(_, len, typtype, elem) = self.0.iter().find(|x| x.0 == t).ok_or_else(|| Box::new(types_error::PgError::error("no such type")))?;
            Ok(TypeFacts { len, byval: len > 0, align: b'i' as i8, typtype: typtype as i8, collation: 0, elem })
        }
        fn func(&self, f: Oid) -> PgResult<FuncFacts> { Catalog::<'m>::func(&PgCatalog, f) }
        fn opcode(&self, o: Oid) -> PgResult<Oid> { Catalog::<'m>::opcode(&PgCatalog, o) }
        fn op_input_types(&self, o: Oid) -> PgResult<(Oid, Oid)> { Catalog::<'m>::op_input_types(&PgCatalog, o) }
        fn btree_cmp_proc(&self, f: Oid, l: Oid, r: Oid) -> PgResult<Oid> { Catalog::<'m>::btree_cmp_proc(&PgCatalog, f, l, r) }
        fn opclass(&self, o: Oid) -> PgResult<(Oid, Oid)> { Catalog::<'m>::opclass(&PgCatalog, o) }
        fn type_cmp_proc(&self, t: Oid) -> PgResult<Oid> { Catalog::<'m>::type_cmp_proc(&PgCatalog, t) }
        fn base_type(&self, t: Oid, m: &mut i32) -> PgResult<Oid> { Catalog::<'m>::base_type(&PgCatalog, t, m) }
        fn domain(&self, t: Oid) -> PgResult<DomainFacts<'m>> { Catalog::<'m>::domain(&PgCatalog, t) }
        fn io(&self, t: Oid) -> PgResult<IoFacts> { Catalog::<'m>::io(&PgCatalog, t) }
        fn text(&self, c: Oid) -> PgResult<TextFacts> { Catalog::<'m>::text(&PgCatalog, c) }
        fn tier(&self, f: Oid) -> Tier { Catalog::<'m>::tier(&PgCatalog, f) }
        fn shim(&self, f: Oid) -> Option<&'static Shim> { Catalog::<'m>::shim(&PgCatalog, f) }
    }

    /// W8-SETOP-WIDEKEYS review r1 (D2039 / D2041): a key type whose internal comparator may run user code, which the reach
    /// past eight group keys / sixteen sort keys refuses; stats_import#137-140's real[] (and a builtin range) stay served.
    #[test]
    fn a_key_that_may_reach_user_code() {
        const T: &[(u32, i16, u8, u32)] = &[
            (700, 4, b'b', 0), (1021, -1, b'b', 700), (25, -1, b'b', 0), (3904, -1, b'r', 0), (4451, -1, b'm', 0),
            (2249, -1, b'p', 0), (2287, -1, b'b', 2249), (2277, -1, b'p', 0), (83, -1, b'c', 0), (1186, 16, b'b', 0),
            (40001, -1, b'r', 0), (40002, -1, b'b', 40001), (40003, -1, b'm', 0), (40004, -1, b'c', 0), (40005, 4, b'e', 0), (40006, -1, b'b', 0), (40007, -1, b'b', 40006),
        ];
        let reach = |t: u32| crate::rep::user_reach(&Types(T), t);
        let served = [700, 1021, 25, 3904, 4451, 1186, 40005, 40006];
        assert!(served.iter().all(|&t| !reach(t)), "a builtin scalar / array / range / multirange, a user enum or base type");
        let refused = [2249, 2287, 2277, 83, 40001, 40002, 40003, 40004, 40007, 99999];
        assert!(refused.iter().all(|&t| reach(t)), "record, record[], anyarray, a composite, a user range / multirange / array, a miss");
    }
}

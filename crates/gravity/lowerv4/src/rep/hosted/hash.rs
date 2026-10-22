//! Kind `rep.hosted.hash` (`HashArm`: types with no native image; L3) (cards/M6.md, M6-plan.md §3.4). Owner M6-TYPES;
//! built by M6-HOSTED (main 09-28).
//! The keyed tables decide a hit by the image's words alone (no confirm path), so the image is exact: the Fp128 of the
//! value's canonical form (gravity_fn canon.rs, through the host `m6_hosted_fp`): an array's dims and elements, a
//! composite's columns, a range's flags and bounds, a multirange's ranges (V4R-508), each leaf in a form equal exactly
//! when its type's `=` says so (a float's -0 and NaNs folded, a numeric's scale dropped, an interval's span, a name to its
//! NUL, a bpchar without its trailing blanks, a varlena's payload under a deterministic collation). A key type with no
//! such form (an anonymous record, a user or geometric type, a nondeterministic column collation) refuses by a named
//! Shape at lowering: the form is decided from the catalog once, by the same function the host reads. A NULL row's
//! words are 0 (the image's flags word marks it).
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type};
use gravity_kernels::ops::agg::trans::entries::{m6_extract_word, m6_hosted_fp};
use types_core::catalog::{NUMERICOID, TIDOID};

use crate::face::kinds::KindId;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;
use crate::rep::{self, CellRef, HashArm, Lx, Scope, StoredRef, Words};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::RepHostedHash, &ARM)], ..crate::ops::NONE };

pub(crate) const ARM: HashArm = HashArm { image, eq };

/// The host `m6_hosted_fp`'s address, for another family that keys by the same exact image (a hashed SubPlan's
/// canonical key, ops/subplan/hashed.rs `fp`).
pub(crate) fn fp_entry() -> usize {
    m6_hosted_fp as *const () as usize
}

fn image(lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, _scope: Scope) -> Words {
    // D1326: decompose keys an integral `extract(field FROM ts)` as the field's int4 (agg.rs `integral_extract`: a
    // by-value type's facts, one record word) while the cell is extract's numeric result, a varlena. Exactly that pair
    // converts here (`extract_word`); any other width mismatch stays the record's Bug.
    // D3080: an integer-text key (decompose `KeyDecl::int_text`) whose projection kept the text (keyed/emit_group.rs
    // `int_text_word_key` found no entry to compute as the integer): never read the text as a numeric
    if matches!(c.cell.typid, types_core::TEXTOID | types_core::VARCHAROID) && tf.byval {
        crate::refuse!(lx, Reason::Shape { why: ShapeWhy("D3080: an integer-text group key whose producer computes the text (no projection entry to key by the integer)") });
    }
    if integral_extract_cell(c.cell.rep, tf) {
        return extract_word(lx, c);
    }
    // D2474: a macaddr / macaddr8 key: decompose keys it by one word (typefacts KeyKind::I64), and rep.hosted.order.fixed's
    // word (its bytes big-endian) is exact: equal words are equal values under macaddr_eq / macaddr8_eq (byte equality).
    // The group emit renders the key back from that word (order.rs `word_key_len` names the same types, a domain's by base).
    if matches!(c.cell.rep, gravity_program::Rep::FixedPtr { len } if Some(len) == crate::rep::hosted::order::word_key_len(tf)) {
        return crate::rep::hosted::order::fixed(lx, c, tf, gravity_decompose::catalog::typefacts::KeyColl::NONE);
    }
    // D1379: a tid keys here (decompose keys it by one word, typefacts KeyKind::I64): one exact word in bttidcmp's order; the
    // record keeps a copy of the datum that renders the key (table/emit_group.rs rep_src, key_copy.rs fixed_of)
    if tf.typid == TIDOID && matches!(c.cell.rep, gravity_program::Rep::FixedPtr { len: 6 }) {
        return tid_word(lx, c, tf);
    }
    // the exact form this lowering reads from its catalog, interned: the program carries its content id (the host resolves
    // it in any thread, a pool worker's with no catalog); DDL retires the program (the lowering's recorded reads, C3).
    // KeySem decides it (keysem::rep::hash_key): an anonymous record's is its columns' (`record_tf`); a user key's
    // classes (anonymous record and record[] keys, a hashed group under a user `=`) and a type PostgreSQL compares by
    // user code or a support function no byte image keeps (typesem `user_key`) key by `user_words`; any other reason
    // refuses
    use gravity_fn::typesem::keysem::rep::{hash_key, HashKey};
    let form = match hash_key(tf.typid, tf.rec, tf.user) {
        Ok(Ok(HashKey::Image(id))) => id,
        Ok(Ok(HashKey::User(spec))) => return user_words(lx, c, tf, spec),
        Ok(Err(why)) => crate::refuse!(lx, Reason::Shape { why: ShapeWhy(why) }),
        Err(_) => crate::refuse!(lx, Reason::Shape { why: ShapeWhy("rep.hosted.hash: a key type whose catalog rows could not be read") }),
    };
    fp_words(lx, c, tf, form)
}

/// The Fp128 of the cell's canonical form under interned form `form` (the host `m6_hosted_fp`); a NULL row's words are 0.
/// K2: a hash join keys a range by its bucket form (typesem `bucket_form`) through here (ops/hashjoin/keys.rs).
pub(crate) fn fp_words(lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, form: u64) -> Words {
    let (p, null) = rep::datum(lx, c, tf);
    let zero = lx.b.iconst(Type::I64, 0);
    let (l_val, l_out) = (lx.b.create_block(&[]), lx.b.create_block(&[Type::I64, Type::I64]));
    lx.b.cond_br(null, (l_out, &[zero, zero]), (l_val, &[]));
    lx.b.switch_to(l_val);
    let p = crate::rep::varlena::hash::ptr(lx.b, p);
    let out = crate::rep::varlena::hash::out2(lx.b);
    let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
    let f = lx.b.declare_host_once("m6_hosted_fp", &[Type::Ptr, Type::I64, Type::Ptr], Some(Type::I64), m6_hosted_fp as *const () as usize, flags);
    let form = lx.b.iconst(Type::I64, form as i64);
    let st = lx.b.call_host(f, &[p, form, out], Some(Type::I64)).expect("m6_hosted_fp returns its status");
    crate::rep::varlena::hash::raise_on(lx, st);
    let (lo, hi) = (lx.b.load(Type::I64, out, 0), lx.b.load(Type::I64, out, 8));
    lx.b.br(l_out, &[lo, hi]);
    lx.b.switch_to(l_out);
    let o = lx.b.block_params(l_out);
    Words { w: [Some(o[0]), Some(o[1])], exact: true }
}

/// USEROP: the Fp128 of the cell's class under user key `spec` (the host `m6_user_fp`: the type's hash proc and equality,
/// or its btree comparison, through fmgr over one representative per class, the run's classes behind the program's
/// `keys.user` handle, D4197); a NULL row's words are 0. The support
/// functions are user code that reaches SPI, so the pipeline runs on the leader (`resolved::leader_only`).
fn user_words(lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, spec: u64) -> Words {
    gravity_fn::typesem::keysem::rep::user_key_site();
    let (p, null) = rep::datum(lx, c, tf);
    let zero = lx.b.iconst(Type::I64, 0);
    let (l_val, l_out) = (lx.b.create_block(&[]), lx.b.create_block(&[Type::I64, Type::I64]));
    lx.b.cond_br(null, (l_out, &[zero, zero]), (l_val, &[]));
    lx.b.switch_to(l_val);
    let p = crate::rep::varlena::hash::ptr(lx.b, p);
    let ctx = lx.frame().ctx;
    let Some(keys) = crate::frame::env::user_keys(lx.b, ctx) else { crate::refuse!(lx, Reason::Bug { payload: crate::frame::env::NO_USER_KEYS }) };
    let out = crate::rep::varlena::hash::out2(lx.b);
    let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
    let host = gravity_kernels::contract::entry4::<gravity_kernels::typesem::userfp::UserFp> as *const () as usize;
    let f = lx.b.declare_host_once("m6_user_fp", &[Type::Ptr, Type::I64, Type::Ptr, Type::I64, Type::Ptr], Some(Type::I64), host, flags);
    let spec = lx.b.iconst(Type::I64, spec as i64);
    let st = lx.b.call_host(f, &[ctx, keys, p, spec, out], Some(Type::I64)).expect("m6_user_fp returns its status");
    crate::rep::varlena::hash::raise_on(lx, st);
    let (lo, hi) = (lx.b.load(Type::I64, out, 0), lx.b.load(Type::I64, out, 8));
    lx.b.br(l_out, &[lo, hi]);
    lx.b.switch_to(l_out);
    let o = lx.b.block_params(l_out);
    Words { w: [Some(o[0]), Some(o[1])], exact: true }
}

/// Key `k`'s facts `tf` with an anonymous record's form (`TypeFacts::rec`): the canonical form of the columns of the ROW()
/// decompose resolved as every producer's (KeyDecl `rec`), interned by typesem like a named type's; a column with no form
/// refuses by its reason. Every other key's facts, and a record whose columns are known only per row (`image` refuses it
/// by typesem's name), are `tf` as it is.
pub(crate) fn record_tf(facts: &dyn gravity_decompose::catalog::Facts, node: i32, tf: TypeFacts, k: &gravity_decompose::ops::shared::agg::KeyDecl<'_>) -> TypeFacts {
    // D2163: a key under a user `=` over byteaeq keys by its payload bytes: bytea's form (KeyDecl `bytes_eq`); an anonymous
    // record by its plan-known columns' form. KeySem decides it (keysem::rep::record_key_form)
    let cols = (!k.bytes_eq).then(|| k.rec.filter(|_| gravity_decompose::catalog::anonymous_record(tf.typid))).flatten().map(|row| gravity_decompose::facts::record::columns(facts, row));
    record_form_tf(node, tf, k.bytes_eq, cols.as_deref())
}

/// `record_tf`'s answer for a key whose ROW() columns `cols` are at hand (D4126: run.pergroup's DISTINCT ON key, decompose
/// TieDesc `rows`): their canonical form (`bytes_eq`: bytea's), or `tf` as it is; a column with no form refuses by its
/// reason.
pub(crate) fn record_form_tf(node: i32, tf: TypeFacts, bytes_eq: bool, cols: Option<&[(u32, u32)]>) -> TypeFacts {
    match gravity_fn::typesem::keysem::rep::record_key_form(bytes_eq, cols) {
        Ok(Some(rec)) => TypeFacts { rec, ..tf },
        Ok(None) => tf,
        Err(Some(why)) => crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) }),
        Err(None) => crate::refuse!(node, Reason::Shape { why: ShapeWhy("rep.hosted.hash: a key type whose catalog rows could not be read") }),
    }
}

/// with#62 / #65 / #82 / #86 (a recursive UNION's SEARCH / CYCLE dedup key): key facts `tf` of an anonymous `record` or
/// `record[]` key under equality `op` and collation `coll`, whose columns `cols` every producer of its values agrees on
/// (decompose `IterDesc::dedup_rows`), with the user key typesem resolves for it (`record_user_key`: KeySem's `By::Fmgr`
/// answer for `op`, record_eq / hash_record or array_eq / hash_array bound through fmgr); `image` keys it by that key's
/// classes (`user_words`). D3374 (e) (record keys E1): a key whose columns are known only per row (`cols` empty: the
/// seed's and the term's ROW()s of other column types, a function's record) keys by the per-row user key (typesem
/// `PER_ROW`); a key typesem gives no user key refuses by its reason.
pub(crate) fn user_record_tf(node: i32, tf: TypeFacts, op: u32, coll: u32, cols: &[(u32, u32)]) -> TypeFacts {
    use gravity_decompose::catalog::{anonymous_record, arrays::record_array};
    if cols.is_empty() && !anonymous_record(tf.typid) && !record_array(tf.typid) {
        return tf;
    }
    match gravity_fn::typesem::keysem::user::record_user_key(&gravity_fn::typesem::KeyReq::hash(op, coll), tf.typid, cols) {
        Ok(Ok(user)) => TypeFacts { user, ..tf },
        Ok(Err(why)) => crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) }),
        Err(_) => crate::refuse!(node, Reason::Shape { why: ShapeWhy("rep.hosted.hash: a key type whose catalog rows could not be read") }),
    }
}

/// D1326: an integral extract key's one exact word from its numeric cell: the host `m6_extract_word` (canon.rs
/// `integral_word`: the int4, PG's "integer out of range" past it, ±Infinity as the packed sentinels MANT_PINF / MANT_NINF). A NULL row's word
/// is 0 (the image's flags word marks it).
fn extract_word(lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>) -> Words {
    let Some(numeric) = gravity_decompose::catalog::typefacts::row_of(NUMERICOID) else { crate::refuse!(lx, Reason::Bug { payload: "numeric's type facts" }) };
    let (p, null) = rep::datum(lx, c, numeric);
    let zero = lx.b.iconst(Type::I64, 0);
    let (l_val, l_out) = (lx.b.create_block(&[]), lx.b.create_block(&[Type::I64]));
    lx.b.cond_br(null, (l_out, &[zero]), (l_val, &[]));
    lx.b.switch_to(l_val);
    let p = crate::rep::varlena::hash::ptr(lx.b, p);
    let out = crate::rep::varlena::hash::out2(lx.b);
    let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
    let f = lx.b.declare_host_once("m6_extract_word", &[Type::Ptr, Type::Ptr], Some(Type::I64), m6_extract_word as *const () as usize, flags);
    let st = lx.b.call_host(f, &[p, out], Some(Type::I64)).expect("m6_extract_word returns its status");
    crate::rep::varlena::hash::raise_on(lx, st);
    let w = lx.b.load(Type::I64, out, 0);
    lx.b.br(l_out, &[w]);
    lx.b.switch_to(l_out);
    let o = lx.b.block_params(l_out);
    Words { w: [Some(o[0]), None], exact: true }
}

/// D1379: a tid's one exact word, ordered as `bttidcmp` (block then offset, both unsigned): `bi_hi << 32 | bi_lo << 16 |
/// ip_posid`, as rep.hosted.order.fixed's (`tideq` is bytewise, so equal words are equal tids). The group's sort reads this
/// word as decompose's `KeyKind::I64`; the record's kept copy renders the key. A NULL row's word is 0 (the image's
/// flags word marks it).
fn tid_word(lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts) -> Words {
    let (p, null) = rep::datum(lx, c, tf);
    let zero = lx.b.iconst(Type::I64, 0);
    let (l_val, l_out) = (lx.b.create_block(&[]), lx.b.create_block(&[Type::I64]));
    lx.b.cond_br(null, (l_out, &[zero]), (l_val, &[]));
    lx.b.switch_to(l_val);
    let b = &mut *lx.b;
    let ptr = b.conv(ConvOp::IntToPtr, Type::Ptr, p);
    let (hi, lo, pos) = (b.load_zext(Type::I16, ptr, 0), b.load_zext(Type::I16, ptr, 2), b.load_zext(Type::I16, ptr, 4));
    let (s32, s16) = (b.iconst(Type::I64, 32), b.iconst(Type::I64, 16));
    let (hi, lo) = (b.int_bin(IntBinOp::Shl, hi, s32), b.int_bin(IntBinOp::Shl, lo, s16));
    let w = b.int_bin(IntBinOp::Or, hi, lo);
    let w = b.int_bin(IntBinOp::Or, w, pos);
    b.br(l_out, &[w]);
    b.switch_to(l_out);
    let o = b.block_params(l_out);
    Words { w: [Some(o[0]), None], exact: true }
}

/// A varlena cell under a by-value type's facts: the integral extract over its numeric result (D1326), and nothing else.
const fn integral_extract_cell(rep: gravity_program::Rep, tf: &TypeFacts) -> bool {
    matches!(rep, gravity_program::Rep::VarlenaPtr) && tf.byval
}

/// Equality against a stored image: the two Fp128 words.
fn eq(lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, stored: StoredRef, tf: &TypeFacts) -> gravity_ir::Value {
    let w = image(lx, c, tf, Scope::Program);
    let p = crate::rep::varlena::hash::ptr(lx.b, stored.ptr);
    // D1326: an integral extract's one word
    if let (Some(v), None) = (w.w[0], w.w[1]) {
        let s = lx.b.load(Type::I64, p, stored.off);
        return lx.b.icmp(IntCond::Eq, v, s);
    }
    let (Some(lo), Some(hi)) = (w.w[0], w.w[1]) else { crate::refuse!(lx, Reason::Bug { payload: "a hosted key image that is not two words" }) };
    let (slo, shi) = (lx.b.load(Type::I64, p, stored.off), lx.b.load(Type::I64, p, stored.off + 8));
    let (a, b) = (lx.b.int_bin(IntBinOp::Xor, lo, slo), lx.b.int_bin(IntBinOp::Xor, hi, shi));
    let d = lx.b.int_bin(IntBinOp::Or, a, b);
    let zero = lx.b.iconst(Type::I64, 0);
    lx.b.icmp(IntCond::Eq, d, zero)
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_a_varlena_cell_under_by_value_facts_is_the_d1326_conversion() {
        use gravity_decompose::catalog::typefacts::row_of as f;
        use gravity_program::Rep;
        let (int4, numeric) = (*f(23).expect("int4 facts"), *f(1700).expect("numeric facts"));
        assert!(super::integral_extract_cell(Rep::VarlenaPtr, &int4));
        assert!(!super::integral_extract_cell(Rep::Word { bytes: 8 }, &int4));
        assert!(!super::integral_extract_cell(Rep::VarlenaPtr, &numeric));
    }

    #[test]
    fn the_arm_is_registered_under_its_kind() {
        assert!(super::ROWS.kinds.iter().any(|(k, _)| *k == super::KindId::RepHostedHash));
    }
}

//! The group key copies (split from keyed/emit_key.rs by M6-HOSTED hh, V4R-440): the host calls that copy a kept key
//! datum into the table's arena (a varlena's payload behind a 4-byte header; a fixed by-reference image, its bytes),
//! the kept copy's width of a fixed by-reference key (uuid / interval / timetz images; M6-HOSTED: name, macaddr, ... by
//! `typlen`), whether a key sorts by its copy (SK_CMP, D1165), and D1326's integral extract keyed through its numeric.
//! Registers nothing. Owner M1-KEYED.
use gravity_abi::breaker::KeyKind;
use types_core::catalog::NUMERICOID;

use crate::face::kinds::KindId;

use crate::ops::agg::hashagg::GroupRec;

/// The copies' host entries live in the module's raw half; their readers name them here.
pub(crate) use super::raw::{key_copy, key_copy_fixed};

/// D1326: a key decompose keys as an int4 word (an integral extract, agg.rs `integral_extract`) whose expression is
/// extract's numeric (rep.hosted.hash converts the cell: canon.rs `integral_word`): its output is the word as a scale-0
/// numeric (`PackedNum` word, `agg_final::num_word`, whose MANT_PINF / MANT_NINF sentinels render ±Infinity), never the
/// word as an int4 (which a numeric consumer would read as a pointer).
pub(crate) fn extract_numeric_key(kind: KeyKind, tf: u32, e: types_nodes::Node<'_>) -> bool {
    kind == KeyKind::I32 && tf == 23 && gravity_decompose::catalog::expr_type::type_of(e) == Some(NUMERICOID)
}

/// M6-HOSTED SK_CMP: key `i` is imaged, its image has no order (not the word / fixed order arm), and its record keeps a
/// copy: the group sort orders it by that copy (`sorts_by_image` is false for it).
pub(crate) fn sorts_by_copy(rec: &GroupRec<'_, '_>, i: usize) -> bool {
    let (kd, imaged) = (&rec.g.keys[i], rec.reps[i].is_some_and(|r| r.imaged));
    let arm = crate::rep::key_kind(kd.kind, Some(kd.image), false, crate::rep::Accessor::Order);
    imaged && arm != KindId::RepWordOrder && arm != KindId::RepFixedOrder && crate::table::emit_key::copies(rec, i)
}

/// The kept copy's width of key `i` when its type is fixed-width by reference: `fixed_len`'s images, and (M6-HOSTED,
/// rep.hosted.hash) any other fixed by-reference type (name, macaddr, aclitem, ...): its `typlen` bytes.
pub(crate) fn fixed_of(rec: &GroupRec<'_, '_>, i: usize) -> Option<u16> {
    let tf = &rec.tfs[i];
    rec.g.keys[i].image.fixed_len().or_else(|| (tf.typlen > 0 && !tf.byval).then_some(tf.typlen as u16))
}

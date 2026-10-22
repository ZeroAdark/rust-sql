//! hj-0310 (D2640 serve; KeySem `op_user_key`), beside `record_tf` / `user_record_tf` (hash.rs): a hashed group's key under a user `=` (decompose `KeyDecl::user_eq`,
//! admitted by decompose facts/user_hash.rs) keys by the user key over its operator's own hash class procs, its
//! `TypeFacts::user`, which rep.hosted.hash keys by `m6_user_fp` (the operator's hash proc buckets, its procedure
//! confirms, as PostgreSQL's TupleHashTable does; the pipeline runs on the leader, since user code reaches SPI), and
//! rep.word.hash a by-value key by its class representative's word (`m6_user_class`, D2640). Every
//! other key, and every key of a group that is not hashed (a streamed group calls the `=` per run: ops/group/stream.rs),
//! is `tf` as it is.
use gravity_decompose::ops::shared::agg::{GroupDecl, GroupForm, KeyDecl};
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_program::Rep;

use crate::admit::refusal::{Reason, ShapeWhy};
use crate::graph::Gx;

/// A dictionary code or packed cell: keyimg's code fronts and the packed arms key it by its code or word, never by the
/// operator's procs (a word cell keys by its class representative's word: rep.word.hash `user_class`, D2640).
/// pg_operator `=` (record, record): record_eq, the type's own `=` (a key with another has `KeyDecl::user_eq`).
const RECORD_EQ: u32 = 2988;

const CELL: &str = "a hashed group key under a user = whose cell is a code or packed lane (hj-0310: only a datum or word cell keys by its user key)";

/// Key `k`'s facts `tf` (`record_tf`'s) in group `g` of node `node`, with the user key its hashed form keys by.
pub(crate) fn tf(gx: &Gx<'_, '_>, node: i32, g: &GroupDecl<'_>, k: &KeyDecl<'_>, tf: TypeFacts) -> TypeFacts {
    if k.user_eq == 0 {
        return road_tf(gx, g, k, tf);
    }
    if g.form != GroupForm::Hashed {
        return tf;
    }
    if crate::cell::key_cell_rep(gx, g.producer.0 as usize, k).is_some_and(|r| !matches!(r, Rep::VarlenaPtr | Rep::CstrPtr | Rep::FixedPtr { .. } | Rep::Word { .. })) {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy(CELL) });
    }
    match gravity_fn::typesem::keysem::user::op_user_key(&gravity_fn::typesem::KeyReq::hash(k.user_eq, k.coll), tf.typid) {
        Ok(Ok(user)) => TypeFacts { user, ..tf },
        Ok(Err(why)) => crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) }),
        Err(_) => crate::refuse!(node, Reason::Shape { why: ShapeWhy("rep.hosted.hash: a key type whose catalog rows could not be read") }),
    }
}

/// D1604 / D1889: a key under its type's own `=` whose type PostgreSQL compares by user code, where the hash and the `=`
/// (or the comparison and the `=`) disagree, so no one image serves a hashed and a sorted plan alike (typesem `user_key`
/// refuses it): it keys by the user key of its group's PostgreSQL strategy (keysem `group_user_key`), a HashAggregate's
/// by the hash proc and the `=` (TupleHashTable's rule), a GroupAggregate's, Group's or Unique's by the comparison (a
/// Sort, then adjacent rows grouped by the `=`, which the comparison then agrees with). A grouping-sets group (its sets
/// may hash and sort), a DISTINCT argument, a code, packed or word cell, a key with a user key or record form already,
/// and every key `group_user_key` does not answer are `tf` as they are (rep.hosted.hash keeps its refusal).
fn road_tf(gx: &Gx<'_, '_>, g: &GroupDecl<'_>, k: &KeyDecl<'_>, tf: TypeFacts) -> TypeFacts {
    use gravity_fn::typesem::keysem::user::Road;
    if tf.user != 0 || tf.rec != 0 || g.set.is_some() {
        return tf;
    }
    let road = match g.form {
        GroupForm::Hashed if sorted_set_right(gx, g) => Road::Sorted,
        GroupForm::Hashed => Road::Hashed,
        GroupForm::Sorted | GroupForm::Group | GroupForm::Unique => Road::Sorted,
        _ => return tf,
    };
    if crate::cell::key_cell_rep(gx, g.producer.0 as usize, k).is_some_and(|r| !matches!(r, Rep::VarlenaPtr | Rep::CstrPtr | Rep::FixedPtr { .. })) {
        return tf;
    }
    // D3374 (e) (record keys E2): an anonymous record whose columns are known only per row (decompose `KeyDecl::rec` none:
    // producers of other layouts) in a sorted group keys by the per-row user key (record_eq / hash_record through fmgr
    // over each row's own columns; kernels userfp.rs): two layouts are two classes, which adjacent rows of PostgreSQL's
    // Sort never are unless its comparison raised first (record_cmp raises on the pairs record_eq does)
    if road == Road::Sorted && k.rec.is_none() && !k.bytes_eq && gravity_decompose::catalog::anonymous_record(tf.typid) {
        return crate::rep::hosted::hash::user_record_tf(g.node_id, tf, RECORD_EQ, k.coll, &[]);
    }
    match gravity_fn::typesem::keysem::rep::group_user_key(tf.typid, road) {
        Ok(Some(user)) => TypeFacts { user, ..tf },
        _ => tf,
    }
}

/// The right side of a sorted SetOp (a SetOp op whose `hashed` is false reads group `g` as its `right`): decompose agg
/// `set_side` makes that side a hashed group whatever PostgreSQL's strategy, but PostgreSQL 18 sorts both inputs and
/// groups and matches them by the sort's comparison (nodeSetOp.c `setop_compare_slots`), so the side keys on the
/// comparison road, as its left side (a sorted group) does: both sides of one set step key alike.
fn sorted_set_right(gx: &Gx<'_, '_>, g: &GroupDecl<'_>) -> bool {
    gx.sorted_right(g.producer)
}

//! hj-0310 (D2640 serve; KeySem `op_user_key`): the grouping keys under a user `=` that a HASHED Agg keys by the user key
//! over the operator's own hash class procs, instead of refusing by `USER_GROUP_EQ`. PostgreSQL's hashed Agg
//! (execTuplesHashPrepare) hashes a key by its operator's hash support and confirms a bucket's entries by the operator's
//! procedure; the lowering keys such a key by `TypeFacts::user` (lowerv4 rep/hosted/user_op.rs), whose host `m6_user_fp`
//! buckets by that hash proc and confirms by that procedure over one representative per class, so its classes are
//! PostgreSQL's groups. The decision is typesem's, read through `Facts::op_key_hash` (no catalog read here).
use types_nodes::plannodes::Agg;

use crate::catalog::Facts;
use crate::facts::hints::Hint;
use crate::ops::shared::agg::{KeyDecl, AGG_HASHED};
use gravity_abi::breaker::KeyKind;

/// A key of type `typid` under user equality operator `op` and collation `coll` that the hashed form keys by its user key
/// (the operator has one; any collation: the user key's procs are called under it, and keyimg keys a key with a user key
/// by its classes, never by a nondeterministic collation's sort key): a by-reference type keyed by two Fp128 words whose
/// group value the record renders from a kept copy (a text, byte-string or other by-reference image, never one the emit
/// reads back from the image words: uuid, interval, numeric), or (D2640) a by-value integer-image or enum key under user
/// code, keyed by its class representative's word (lowerv4 rep.word.hash `user_class`, rendered as that member: the
/// equal-member ruling). A by-value key under a built-in other than its type's own `=` (int4ne) keeps the refusal (its
/// classes are no one value each), as does a key holding a multirange (the caller's D1901 test).
pub fn key(f: &dyn Facts, op: u32, typid: u32, coll: u32) -> bool {
    let image = |tf: crate::catalog::typefacts::TypeFacts| {
        ((tf.image.untrimmed_bytes() || !tf.image.equality()) && tf.kind == KeyKind::Fp128 && tf.words == 2 && !tf.byval)
            || ((tf.image.is_word() || tf.image.is_enum()) && tf.words == 1 && tf.byval && crate::catalog::keysem::eq_opaque(f, op, typid))
    };
    crate::catalog::typefacts::facts_of(f, typid).is_some_and(image) && f.op_key_hash(op, typid, coll)
}

/// A hashed Agg of no chain, grouping sets or hint (`map_agg`'s plain hashed form, the one that takes `TypeFacts::user`).
pub fn hashed(a: &Agg<'_>) -> bool {
    a.aggstrategy == AGG_HASHED && a.chain.iter().next().is_none() && !crate::ops::per_set(a) && Hint::of_agg(a).iter().all(Option::is_none)
}

/// `map`'s early test: Agg `a`'s key at input column `col` (type `typid`) under its operator `op` is hashed `key`'s.
pub fn agg_key(f: &dyn Facts, a: &Agg<'_>, op: u32, col: i16, typid: Option<u32>) -> bool {
    let Some(t) = typid else { return false };
    let Some(i) = a.grpColIdx.iter().zip(a.grpOperators.iter()).position(|(&c, &o)| c == col && o == op) else { return false };
    hashed(a) && key(f, op, t, a.grpCollations.get(i).copied().unwrap_or(0))
}

/// `map_agg`'s test for a group of no hint: each key under a user `=` (`KeyDecl::user_eq`) is hashed `key`'s.
pub fn keys(f: &dyn Facts, keys: &[KeyDecl<'_>], hints: &[Option<Hint>]) -> bool {
    hints.iter().all(Option::is_none) && keys.iter().all(|k| k.user_eq == 0 || key(f, k.user_eq, k.tf, k.coll))
}

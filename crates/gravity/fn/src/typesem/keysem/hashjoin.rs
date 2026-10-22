//! KeySem's hash join answers (decision 24, K4): what lowerv4's hash join keys (ops/hashjoin/keys.rs) read about a key
//! type, kept in the key-semantics home so the join decides nothing about a key's equality itself (rule RK). The
//! plan-time half, the clause's hash functions and D1901's multirange refusal, is decompose's (catalog/keysem.rs
//! `hash_join_procs` / `hash_join_multirange` over `Facts`, whose host answers from `hash_procs` and
//! `typesem::holds_multirange`).
use types_core::Oid;

/// K2 (D1889's hash join half): the bucket form id a range key of type `typid` keys by (typesem `bucket_form`: a range
/// whose subtype opclass the exact image cannot keep, an array of such ranges, a domain over either); none for any other
/// type, and when a catalog read fails (the key keeps its exact image, as before K4).
pub fn bucket(typid: Oid) -> Option<u64> {
    super::super::bucket_form(typid).ok().flatten().map(|f| f.0)
}

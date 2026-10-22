//! The one key request: what one key compares, hashes or orders by, as decompose's plan-time catalog (`Facts::key`) and
//! gravity_fn's key home (typesem `resolve`) both read it. A key's semantics come from its operator (and, only in place of
//! a polymorphic operator input type, the key's own types); the request is built only with the constructors, so a site
//! cannot type a key any other way. One definition for both crates (decompose reads no gravity_fn).

/// What the key is used for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyUse {
    Eq,
    Hash,
    Sort,
    Merge,
}

/// One key's request (PLAN-v5-keysem §2.2). A consumer builds it with `KeyReq::hash` / `KeyReq::sort` (never by its
/// fields) and asks typesem `resolve` (decompose: `Facts::key`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct KeyReq {
    pub use_: KeyUse,
    /// The clause's operator: an equality with hash support (`KeyUse::Hash`) or a btree ordering operator, `<` or `>`
    /// (`KeyUse::Sort`: a SortGroupClause's `sortop`, a Sort's / MergeAppend's / Append mark's sort operator).
    pub op: u32,
    /// The key's input collation (InvalidOid for none).
    pub coll: u32,
    /// The key's left and right types (PLAN-v5-keysem §2.2 `lt` / `rt`; 0: none given, the operator's own). Read only in
    /// place of a POLYMORPHIC operator input type (anyarray, anyrange, anyenum, record, ..), whose word width, canonical
    /// form and collatability are the key's own type's: PostgreSQL's executor calls array_eq, range_eq, enum_eq or
    /// record_eq on that type's values. A concrete operator input type stands as it is (a cross-type `=` keys by its own
    /// two types).
    pub lt: u32,
    pub rt: u32,
}

impl KeyReq {
    /// A hashed key under equality `op` and collation `coll` (a hashed SubPlan's, SetOp's or Agg's, a hash join's clause,
    /// a hashed SAOP's).
    pub const fn hash(op: u32, coll: u32) -> KeyReq {
        KeyReq { use_: KeyUse::Hash, op, coll, lt: 0, rt: 0 }
    }

    /// A sorted key under ordering operator `op` (strategy 1 `<` or 5 `>` of some btree family), no collation given
    /// (`under` gives one).
    pub const fn sort(op: u32) -> KeyReq {
        KeyReq { use_: KeyUse::Sort, op, coll: 0, lt: 0, rt: 0 }
    }

    /// This request under input collation `coll`.
    pub const fn under(self, coll: u32) -> KeyReq {
        KeyReq { coll, ..self }
    }

    /// This request for a key whose two sides have types `lt` and `rt` as the plan types them (exprType of each input;
    /// a sorted key's `rt` is its `lt`). Only a polymorphic operator input type reads them (`lt`'s doc).
    pub const fn typed(self, lt: u32, rt: u32) -> KeyReq {
        KeyReq { lt, rt, ..self }
    }
}

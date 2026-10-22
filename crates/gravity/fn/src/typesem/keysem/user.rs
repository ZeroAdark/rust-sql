//! USEROP (D1604 / D1889 serve: the user's opclass decides; hash and equality go through the opfamily support functions
//! via fmgr, no refusal): the key road of a type whose equality is not provably its canonical bytes.
//! `form` keys a type by an exact image only where PostgreSQL's `=` is a built-in equality of its bytes at every level.
//! Where it refuses because PostgreSQL compares by user code (a user DEFAULT btree or hash opclass at any level: D1604; a
//! range subtype opclass other than the subtype's own: D1889) or by a support function no byte image keeps (a user or
//! geometric type), the key is a USER KEY: the keyed tables still decide a hit by an exact image, the Fp128 of the
//! value's class under the type's own `=`, which the host (kernels typesem userfp.rs `m6_user_fp`) finds by calling the
//! type's support functions through fmgr: its hash proc to bucket and its equality to confirm, or with no hash proc its
//! btree comparison (FUNCTION 1) to binary-search, over one representative per class. Equal values under the type's `=`
//! (an equivalence, by the hash and btree opclass contracts PostgreSQL's own hashed and sorted grouping rely on) share
//! one class, unequal ones never do, so the image is exact and the user's functions are the ones that decide.
//!
//! The procs are the type cache's (the parser's grouping `=` and the planner's hash function are its eq_opr and
//! hash_proc; its cmp_proc is the btree FUNCTION 1 of the default family), read for the key type itself, which the
//! parser and planner already asked for. The spec is interned like a form (`intern` / `spec`): the program carries its
//! id, and the host resolves it in the thread that runs the key's pipeline, which is the leader's (the lowering marks
//! the pipeline serial: user code reaches SPI, which a pool worker has none of).
//!
//! A spec names what its resolution read, at the generations it read them (`stamp`, C3: gravity_deps
//! `Deps::committed_stamp` of the type cache's support lists, the types of every level, the operator and procs), so any
//! DDL that could change what the type's `=` means at any level (an element's or a column's opclass, a support function's
//! body) gives another. The program cache retires a program on the same reads (they are its recorded dependencies), so a
//! lowering after DDL interns a new spec, whose classes and bound calls start fresh: a container's classes made under an
//! element's old opclass are never met again (an array's or a record's own procs are the core ones whatever the element's
//! opclass is, so the procs alone do not tell them apart), nor is a bound call whose `fn_extra` holds the element's type
//! cache entry from before the DDL (array_eq's memo: its equality's call info is reset when the type cache sees the new
//! operator, "cache lookup failed for function 0", K15). A run in flight across the DDL (a cursor) keeps its own spec's
//! classes, as PostgreSQL's executor keeps the call info it bound at its start.
use super::super::R;

type Oid = u32;

/// A user key's support functions (each a `resolved` id, 0 for none) and its storage. `hash` and `cmp` are never both 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UserKey {
    pub typid: Oid,
    /// The equality operator's procedure (the type cache's eq_opr, through get_opcode).
    pub eq: u32,
    /// The type cache's hash_proc (HASHSTANDARD_PROC of the default hash family, valid only when it agrees with eq_opr).
    pub hash: u32,
    /// The type cache's cmp_proc (BTORDER_PROC of the default btree family).
    pub cmp: u32,
    /// The type's collation (typcollation), each call's.
    pub coll: Oid,
    /// typlen: -1 a varlena (copied detoasted), > 0 a fixed by-reference value's bytes, or a by-value word's width.
    pub len: i16,
    /// typbyval: the key is its datum word (D2723: an enum or other by-value type under a user equality), passed to the
    /// support functions as the word itself; its image is the class number (lowerv4 rep.word.hash).
    pub byval: bool,
    /// What the resolution read, at its generations (`stamped`): DDL between two lowerings that changes any of it gives two
    /// specs. Odd; an even one is this process's own (`OWN_STAMPS`: reads that may not be the committed catalog).
    pub stamp: u64,
    /// with#62 / #65 / #82 / #86 (`record_user_key`): an anonymous record key's column layout (an array of one's: its
    /// elements'), the SipHash of its columns' (type, collation) list; 0 for every other key. A spec's classes are one
    /// table, so two layouts never share one: `record_eq` raises on records of dissimilar column types, which one key's
    /// values never are (its columns are plan-known) but two statements' keys may be. D3374 (e): `PER_ROW` for a key whose
    /// columns are known only per row (kernels userfp.rs: two layouts are two classes).
    pub row: u64,
    /// A hashed site's own table (`subplan_user_key`): 0 for every key whose equality is an equivalence its hash agrees
    /// with (one table serves every site); else the site's id, so a site whose equality is not one (`=!` over int4ne)
    /// meets only the representatives its own set founded, and its first match in a bucket is a set member's.
    pub site: u64,
}

/// D3374 (e) (record keys E1): `UserKey::row` of an anonymous record (or `record[]`) key whose columns are known only per
/// row (`record_user_key` with no columns): its values' layouts may differ from row to row, and kernels userfp.rs takes
/// `record_eq`'s 42804 on two layouts as two classes. Even, so `row_id` (always odd) never gives it.
pub const PER_ROW: u64 = 2;

/// Whether user key `id` is a per-row record key (`PER_ROW`); false for an id no lowering interned.
pub fn per_row(id: u64) -> bool {
    spec(id).is_ok_and(|k| k.row == PER_ROW)
}

static SPECS: pgsync::RwLock<Vec<(u64, UserKey)>> = pgsync::RwLock::new(Vec::new()); // alloc: Cold

/// The even stamps (`UserKey::stamp`) of resolutions whose reads may not be the committed catalog (a lagged view, this
/// backend's own uncommitted DDL): each its own spec, never met again.
static OWN_STAMPS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(2);

/// `read`'s user key, recorded (gravity_deps: the reads join the lowering's recording too), stamped by what it read
/// (`UserKey::stamp`) and interned.
fn stamped(read: impl FnOnce() -> R<Result<UserKey, &'static str>>) -> R<Result<u64, &'static str>> {
    let (k, deps) = gravity_deps::recording(read);
    let stamp = match deps.committed_stamp() {
        Some(s) => (s as u64 ^ (s >> 64) as u64) | 1,
        None => OWN_STAMPS.fetch_add(2, core::sync::atomic::Ordering::Relaxed),
    };
    Ok(k?.map(|k| intern(UserKey { stamp, ..k })))
}

fn id_of(k: &UserKey) -> u64 {
    use core::hash::{Hash, Hasher};
    #[allow(deprecated)]
    let mut h = core::hash::SipHasher::new_with_keys(0x7573_6572_6b65_7973, 0x6d36_686f_7374_6564);
    k.hash(&mut h);
    h.finish() | 1
}

/// Whether `form`'s refusal `why` means PostgreSQL compares the key by support functions no byte image keeps (D1604's
/// user equality at any level, D1889's range subtype opclass, a user or geometric leaf), so the key is a user key. A
/// collation's refusal, a dropped column, an anonymous record or a nesting limit is not. Decides no effect: it says which
/// road a refused key takes (a keysem fact); whether its support code shows is the effects home's answer.
pub(crate) fn support_keyed(why: &str) -> bool {
    why == super::super::USER_EQUALITY
        || why.starts_with("a range key whose subtype opclass is not")
        || why.starts_with("a key type whose equality is not its canonical bytes")
}

/// The strategy of the PostgreSQL node a user key groups for (`user_key_for`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Road {
    /// Any strategy: one image every plan of the key agrees on (`user_key`).
    Either,
    /// PostgreSQL hashes the key (a HashAggregate, a hashed SetOp): its TupleHashTable buckets by the type's hash proc and
    /// confirms by its `=`.
    Hashed,
    /// PostgreSQL sorts the key by its btree comparison and groups adjacent rows by its `=` (a GroupAggregate, a Group, a
    /// Unique, a sorted SetOp).
    Sorted,
}

/// The user key of key type `typid`, interned: its id, or the reason it has none (`form`'s own refusal when that is not
/// a user-code one, or the support function the type lacks). A type `form` keeps has no user key. A by-value type's key
/// is its word (D2723), a by-reference one's its value; a type with neither a hash proc nor a comparison keeps its refusal, or
/// one whose procs run SECURITY DEFINER. The lowering's call (the leader: it reads the catalog).
pub(crate) fn user_key(typid: Oid) -> R<Result<u64, &'static str>> {
    user_key_for(typid, Road::Either)
}

/// `user_key` for a key PostgreSQL groups on road `road`. `Either` is `user_key`'s rule: a hash proc that agrees with the
/// `=` at every level (`hash_agrees`), else a comparison that does (`cmp_agrees`), since one image then serves a hashed
/// and a sorted plan alike. A node whose own strategy is known keys by that strategy's classes alone (D1604 / D1889: a
/// user opclass under which the type's hash and its `=` disagree, so PostgreSQL's hashed and sorted plans group the key
/// differently): `Hashed` by the hash proc and the `=`, TupleHashTable's own rule, whatever the two say of each other
/// (two values equal under the `=` whose hashes differ are two groups there too); `Sorted` by the comparison alone (the
/// spec's hash 0: the host binary-searches its representatives), when the comparison agrees with the `=` that PostgreSQL
/// groups adjacent sorted rows by (`cmp_agrees`); where it does not, PostgreSQL's groups depend on how its sort orders
/// the comparison's ties, and the key keeps its refusal.
pub(crate) fn user_key_for(typid: Oid, road: Road) -> R<Result<u64, &'static str>> {
    stamped(|| user_key_read(typid, road))
}

/// `user_key_for`'s key, unstamped.
fn user_key_read(typid: Oid, road: Road) -> R<Result<UserKey, &'static str>> {
    let why = match super::super::build(typid, 0)? {
        Ok(_) => return Ok(Err("a key its canonical form keeps (no user key)")),
        Err(why) => why,
    };
    if !support_keyed(why) {
        return Ok(Err(why));
    }
    let Some(t) = syscache_seams::lookup_pg_type_typcache_shape::call(typid)? else { return Ok(Err(why)) };
    if !(t.typlen == -1 || t.typlen > 0) || t.typbyval && t.typlen > 8 {
        return Ok(Err(why));
    }
    let flags = typcache::TYPECACHE_EQ_OPR | typcache::TYPECACHE_HASH_PROC | typcache::TYPECACHE_CMP_PROC;
    let e = typcache::lookup_type_cache(typid, flags)?;
    if e.eq_opr() == 0 || (e.hash_proc() == 0 && e.cmp_proc() == 0) {
        return Ok(Err(why));
    }
    // a hash that disagrees with the `=` at some level (a range under a subtype opclass other than its subtype's own `=`,
    // an element whose hash family's equality is not its `=`): PostgreSQL's hashed and sorted plans group such a key
    // differently, so no one image is its answer for `Either`, and the key keeps its refusal. A type with no hash proc
    // takes the comparison road (PostgreSQL can only sort it): its comparison must be its `=`'s own at every level
    // (`cmp_agrees`). `Hashed` / `Sorted`: the one road of the node's strategy.
    let (agrees, hash_proc) = match road {
        Road::Either if e.hash_proc() != 0 => (hash_agrees(typid, 0)?, e.hash_proc()),
        Road::Either => (e.cmp_proc() != 0 && cmp_agrees(typid, 0)?, 0),
        Road::Hashed => (e.hash_proc() != 0, e.hash_proc()),
        Road::Sorted => (e.cmp_proc() != 0 && cmp_agrees(typid, 0)?, 0),
    };
    if !agrees {
        return Ok(Err(why));
    }
    let resolve = |p: Oid| -> R<Result<u32, &'static str>> {
        if p == 0 {
            return Ok(Ok(0));
        }
        crate::resolved::resolve(p)
    };
    let eq = match resolve(lsyscache::get_opcode(e.eq_opr())?)? {
        Ok(id) if id != 0 => id,
        Ok(_) => return Ok(Err(why)),
        Err(shape) => return Ok(Err(shape)),
    };
    let (hash, cmp) = match (resolve(hash_proc)?, resolve(e.cmp_proc())?) {
        (Ok(h), Ok(c)) => (h, c),
        (Err(shape), _) | (_, Err(shape)) => return Ok(Err(shape)),
    };
    Ok(Ok(UserKey { typid, eq, hash, cmp, coll: t.typcollation, len: t.typlen, byval: t.typbyval, stamp: 0, row: 0, site: 0 }))
}

/// with#62 / #65 / #82 / #86 (recursive UNION SEARCH / CYCLE keys; PLAN-v5-keysem §3, "anonymous record": Call): the
/// user key of an anonymous `record` key, or of an array of one (`record[]`), whose columns
/// `cols` (each its type and collation) every producer of the key's values agrees on (decompose facts::record_union),
/// hashed and compared by PostgreSQL's own support functions through fmgr. KeySem resolves the key's operator `req`
/// (the type's default `=`: record_eq, or array_eq over record elements): it answers `By::Fmgr` over built-in procs, its
/// equality the type cache's `=`, its hash its hash family's (hash_record, hash_array), the ones PostgreSQL's hashed
/// RecursiveUnion calls (the type cache claims no hash proc for record; parse analysis takes a recursive UNION's record
/// and record[] columns hashable anyway).
/// hash_record and record_eq read each column's default hash function and equality under the column's collation
/// (hash_array and array_eq read the element's, a record's), so each column's hash must agree with its `=`
/// (`hash_agrees`); the classes kernels `m6_user_fp` finds through them are then PostgreSQL's equal rows. No image: the
/// columns only name the spec (`UserKey::row`). An untyped or anonymous record column, a collatable column with no
/// collation (hash_record raises 42P22: D1292's name) or one whose hash does not agree keep a refusal. D3374 (e) (record
/// keys E1): columns not known at plan time (`cols` empty: producers of other layouts, a function's record) give the
/// per-row key (`UserKey::row` `PER_ROW`): the same procs, no per-column check (hash_record and record_eq read each row's
/// own columns and raise as PostgreSQL's do, 42883 on a column type with no hash, say); two values of two layouts are
/// two classes (kernels userfp.rs: record_eq's 42804 there is kept, and raised only when the recursive UNION's set holds
/// the row PostgreSQL's table compares this one with). The lowering's call (the leader: it reads the catalog).
pub fn record_user_key(req: &super::KeyReq, typid: Oid, cols: &[(Oid, Oid)]) -> R<Result<u64, &'static str>> {
    stamped(|| record_user_key_read(req, typid, cols))
}

/// `record_user_key`'s key, unstamped.
fn record_user_key_read(req: &super::KeyReq, typid: Oid, cols: &[(Oid, Oid)]) -> R<Result<UserKey, &'static str>> {
    const UNKNOWN: &str = "an anonymous record key (its columns are known only per row)";
    let Some(t) = syscache_seams::lookup_pg_type_typcache_shape::call(typid)? else { return Ok(Err("a key type the catalog does not know")) };
    let array = t.typsubscript == super::super::F_ARRAY_SUBSCRIPT_HANDLER && t.typlen == -1 && t.typelem == super::super::RECORDOID;
    if typid != super::super::RECORDOID && !array {
        return Ok(Err("a key that is neither an anonymous record nor an array of one (no record user key)"));
    }
    for &(c, coll) in cols {
        if c == 0 || c == super::super::RECORDOID {
            return Ok(Err(UNKNOWN));
        }
        if coll == 0 && lsyscache::type_is_collatable(c)? {
            return Ok(Err(super::INDETERMINATE));
        }
        if !hash_agrees(c, 1)? {
            return Ok(Err("an anonymous record key's column whose hash function does not agree with its equality"));
        }
    }
    // a user `LANGUAGE internal` proc's prosrc is read into the context (`keysem::fmgr_proc`); the answer is plain data
    let mut cx = mcx::MemoryContext::new_bump("typesem record_user_key"); // alloc: Cold
    let k = super::resolve(cx.mcx(), req);
    cx.reset();
    let k = match k? {
        Ok(k) => k,
        Err(tc) => return Ok(Err(tc.why())),
    };
    // the type cache names no hash proc for record (or record[]: cache_record_field_properties claims no hashing for an
    // anonymous record); PostgreSQL's parse analysis of a recursive UNION takes the type hashable anyway and its executor
    // hashes by the operator's hash family (execTuplesHashPrepare: get_op_hash_functions), so the `=` must be the type's
    // own and the hash proc is KeySem's (hash_record; hash_array, which hashes record elements by hash_record)
    let own = typcache::lookup_type_cache(typid, typcache::TYPECACHE_EQ_OPR)?.eq_opr();
    let own_eq = own != 0 && lsyscache::get_opcode(own)? == k.procs.eq;
    if k.by != super::By::Fmgr || !k.native || k.lt != k.rt || k.raise_hash || !k.linear || k.procs.lhs_hash != k.procs.rhs_hash || !own_eq {
        return Ok(Err("an anonymous record key under an operator other than its type's own `=`"));
    }
    let (eq, hash) = match (crate::resolved::resolve(k.procs.eq)?, crate::resolved::resolve(k.procs.lhs_hash)?) {
        (Ok(e), Ok(h)) if e != 0 && h != 0 => (e, h),
        (Err(shape), _) | (_, Err(shape)) => return Ok(Err(shape)),
        _ => return Ok(Err(UNKNOWN)),
    };
    let row = if cols.is_empty() { PER_ROW } else { row_id(cols) };
    Ok(Ok(UserKey { typid, eq, hash, cmp: 0, coll: req.coll, len: -1, byval: false, stamp: 0, row, site: 0 }))
}

/// hj-0310 (D2640 serve; KeySem `KeyReq::hash`): the user key of a key of type `typid` hashed under user equality
/// operator `req.op` and collation `req.coll` (a hashed Agg's grouping key: nodeAgg's TupleHashTable buckets by the
/// operator's hash support and confirms by its procedure, execTuplesHashPrepare), keyed by the OPERATOR's own procs, not
/// the type cache's: KeySem resolves `req` to `By::Fmgr` over one type (`lt == rt == typid`) with one hash proc,
/// `linear` (user code: the hash class's contract), and no `raise_hash`; its equality and hash are bound through fmgr and
/// interned (cmp 0, the request's collation, the type's storage). `m6_user_fp` then buckets by that hash and confirms by
/// that equality over one representative per class, as PostgreSQL's table does. A record (or record[]) key is
/// `record_user_key`'s; a pseudo-type, a cstring, a by-value type wider than a word, an operator KeySem keys by a word or
/// an image (the type's own `=`: no user key needed), and a SECURITY DEFINER or otherwise unresolvable proc keep a
/// refusal. The lowering's call (the leader: it reads the catalog).
pub fn op_user_key(req: &super::KeyReq, typid: Oid) -> R<Result<u64, &'static str>> {
    stamped(|| op_user_key_read(req, typid))
}

/// `op_user_key`'s key, unstamped.
fn op_user_key_read(req: &super::KeyReq, typid: Oid) -> R<Result<UserKey, &'static str>> {
    const OTHER: &str = "a hashed key under a user operator KeySem does not key by its own procs (no user key)";
    let Some(t) = syscache_seams::lookup_pg_type_typcache_shape::call(typid)? else { return Ok(Err("a key type the catalog does not know")) };
    let array_of_record = t.typsubscript == super::super::F_ARRAY_SUBSCRIPT_HANDLER && t.typelem == super::super::RECORDOID;
    if typid == super::super::RECORDOID || array_of_record || t.typtype as u8 == b'p' || !(t.typlen == -1 || t.typlen > 0) || t.typbyval && t.typlen > 8 {
        return Ok(Err(OTHER));
    }
    // a user `LANGUAGE internal` proc's prosrc is read into the context (`keysem::fmgr_proc`); the answer is plain data
    let mut cx = mcx::MemoryContext::new_bump("typesem op_user_key"); // alloc: Cold
    let k = super::resolve(cx.mcx(), req);
    cx.reset();
    let k = match k? {
        Ok(k) => k,
        Err(tc) => return Ok(Err(tc.why())),
    };
    if k.by != super::By::Fmgr || k.lt != typid || k.rt != typid || k.raise_hash || !k.linear || k.procs.lhs_hash == 0 || k.procs.lhs_hash != k.procs.rhs_hash {
        return Ok(Err(OTHER));
    }
    let (eq, hash) = match (crate::resolved::resolve(k.procs.eq)?, crate::resolved::resolve(k.procs.lhs_hash)?) {
        (Ok(e), Ok(h)) if e != 0 && h != 0 => (e, h),
        (Err(shape), _) | (_, Err(shape)) => return Ok(Err(shape)),
        _ => return Ok(Err(OTHER)),
    };
    Ok(Ok(UserKey { typid, eq, hash, cmp: 0, coll: req.coll, len: t.typlen, byval: t.typbyval, stamp: 0, row: 0, site: 0 }))
}

/// A hashed SubPlan's key under operator `req.op` over one type `typid` that KeySem answers `By::Fmgr` with one hash proc
/// (a user operator class: SQL procs, a built-in wrapper such as `=!` over int4ne / hashint4, a by-value type's): the user
/// key of the OPERATOR's own procs, as PostgreSQL's buildSubPlanHash / ExecHashSubPlan bucket by the hash proc and
/// confirm by the equality. `m6_user_fp` classes the set's values in build order (each founds a class unless the
/// equality, called as PostgreSQL's TupleHashTableMatch calls it, `eq(new, member)`, finds a member of its bucket), and
/// the set is built before the first left side is classed (the set is LAZY), so a left side's class is the first member
/// of its bucket the equality accepts, in the set exactly when PostgreSQL's lookup finds one. Where the equality is not
/// an equivalence its hash agrees with (`linear` false), the key's table is the site's own (`UserKey::site`, `site`
/// non-zero), so no other site's representatives come first. A pseudo-type, a by-value type wider than a word, a
/// `raise_hash` key and an unresolvable proc keep a refusal. The lowering's call (the leader: it reads the catalog).
pub fn subplan_user_key(req: &super::KeyReq, typid: Oid, site: u64) -> R<Result<u64, &'static str>> {
    stamped(|| subplan_user_key_read(req, typid, site))
}

fn subplan_user_key_read(req: &super::KeyReq, typid: Oid, site: u64) -> R<Result<UserKey, &'static str>> {
    const OTHER: &str = "a hashed SubPlan's key under an operator with no one hash proc over one type (no user key)";
    let Some(t) = syscache_seams::lookup_pg_type_typcache_shape::call(typid)? else { return Ok(Err("a key type the catalog does not know")) };
    if t.typtype as u8 == b'p' || !(t.typlen == -1 || t.typlen > 0) || t.typbyval && t.typlen > 8 {
        return Ok(Err(OTHER));
    }
    let mut cx = mcx::MemoryContext::new_bump("typesem subplan_user_key"); // alloc: Cold
    let k = super::resolve(cx.mcx(), req);
    cx.reset();
    let k = match k? {
        Ok(k) => k,
        Err(tc) => return Ok(Err(tc.why())),
    };
    if k.by != super::By::Fmgr || k.lt != typid || k.rt != typid || k.raise_hash || k.procs.lhs_hash == 0 || k.procs.lhs_hash != k.procs.rhs_hash {
        return Ok(Err(OTHER));
    }
    let (eq, hash) = match (crate::resolved::resolve(k.procs.eq)?, crate::resolved::resolve(k.procs.lhs_hash)?) {
        (Ok(e), Ok(h)) if e != 0 && h != 0 => (e, h),
        (Err(shape), _) | (_, Err(shape)) => return Ok(Err(shape)),
        _ => return Ok(Err(OTHER)),
    };
    let site = if k.linear { 0 } else { site };
    Ok(Ok(UserKey { typid, eq, hash, cmp: 0, coll: req.coll, len: t.typlen, byval: t.typbyval, stamp: 0, row: 0, site }))
}

/// `UserKey::row` of columns `cols`: never 0 (every other key's).
fn row_id(cols: &[(Oid, Oid)]) -> u64 {
    use core::hash::{Hash, Hasher};
    #[allow(deprecated)]
    let mut h = core::hash::SipHasher::new_with_keys(0x7265_636f_7264_726f, 0x7773_7065_6331_3030);
    cols.hash(&mut h);
    h.finish() | 1
}

/// D2125 (KeySem slice 5's piece, lowerv4 expr.saop): the user key of key type `typid` when a hashed site's equality proc
/// `eq_proc` and hash proc `hash_proc` (pg_proc oids, as the plan names them) under collation `coll` are that key's own (a
/// user DEFAULT hash class: PostgreSQL's hashed ScalarArrayOp calls exactly them), so a table keyed by the key's classes
/// finds what PG's finds. The procs are user code that reaches SPI, so the pipeline that calls them runs on the leader
/// (`resolved::leader_only`). `None`: no user key, or other procs. The lowering's call.
pub fn hashed_user_key(typid: Oid, coll: Oid, eq_proc: Oid, hash_proc: Oid) -> R<Option<u64>> {
    let id = match user_key(typid)? {
        Ok(id) => id,
        Err(_) => return Ok(None),
    };
    let k = spec(id)?;
    let fmgr = |p: Oid| -> R<Option<u32>> { Ok(crate::resolved::resolve(p)?.ok().filter(|&r| r != 0)) };
    if k.hash == 0 || k.coll != coll || fmgr(eq_proc)? != Some(k.eq) || fmgr(hash_proc)? != Some(k.hash) {
        return Ok(None);
    }
    crate::resolved::leader_only();
    Ok(Some(id))
}

const F_HASH_ARRAY: Oid = 626;
const F_HASH_RECORD: Oid = 6192;
const F_HASH_RANGE: Oid = 3902;
const F_HASH_MULTIRANGE: Oid = 4278;
const HASHSTANDARD_PROC: i16 = 1;
const F_BTARRAYCMP: Oid = 382;
const F_BTRECORDCMP: Oid = 2987;

/// Whether type `typid`'s hash function agrees with its `=` at every level, so that equal values hash alike (and no user
/// opclass, family or comparison sits under a core `=`: sorted plans would group by that comparison instead): the equality
/// member of its default hash family is its own `=` (`own_eq`, read from the catalog, no type cache eq_opr state: the
/// check lookup_type_cache makes before it names a hash proc), and where that family's hash function is a container's
/// (hash_array, hash_record, hash_range, hash_multirange) every element type agrees too, a range's bounds compared by a
/// subtype opclass that equates as the subtype's own `=` (`subtype_equality`: hash_range hashes a bound by the subtype's
/// default hash, range_eq compares it by the declared opclass). A domain is its base. A type nested deeper than 16
/// levels, or one the catalog does not know, does not agree.
fn hash_agrees(typid: Oid, depth: u32) -> R<bool> {
    if depth > 16 {
        return Ok(false);
    }
    let Some(t) = syscache_seams::lookup_pg_type_typcache_shape::call(typid)? else { return Ok(false) };
    if t.typtype as u8 == b'd' {
        let Some(b) = syscache_seams::pg_type_base_shape::call(typid)? else { return Ok(false) };
        return hash_agrees(b.typbasetype, depth + 1);
    }
    let (_, _, eq) = super::super::own_eq(typid)?;
    // a core `=` under a user default btree opclass, family or comparison (a user opclass put in a core family with its own
    // FUNCTION 1, m6_multirange_keys' mrk_q1_ops / mrk_q2_ops in text_ops: a SQL comparator, or bpcharcmp for varchar):
    // PostgreSQL's sorted plans group by that comparison, its hashed ones by the `=`. A user `=` owns its comparison.
    if !(eq != 0 && !super::builtin(eq)) && super::super::user_equality(typid)? {
        return Ok(false);
    }
    let h = typcache::lookup_type_cache(typid, typcache::TYPECACHE_HASH_OPFAMILY)?;
    let (opf, intype) = (h.hash_opf(), h.hash_opintype());
    if opf == 0 || eq == 0 || lsyscache::get_opfamily_member(opf, intype, intype, lsyscache::HTEqualStrategyNumber)? != eq {
        return Ok(false);
    }
    match lsyscache::get_opfamily_proc(opf, intype, intype, HASHSTANDARD_PROC)? {
        0 => Ok(false),
        F_HASH_ARRAY => Ok(t.typelem != 0 && hash_agrees(t.typelem, depth + 1)?),
        F_HASH_RECORD => {
            for att in 1.. {
                let Some(a) = syscache_seams::lookup_pg_attribute_shape::call(t.typrelid, att)? else { break };
                if !a.attisdropped && !hash_agrees(a.atttypid, depth + 1)? {
                    return Ok(false);
                }
            }
            Ok(t.typrelid != 0)
        }
        F_HASH_RANGE => match syscache_seams::lookup_pg_range_shape::call(typid)? {
            Some(r) => Ok(super::super::subtype_equality(r.rngsubtype, r.rngsubopc)? && hash_agrees(r.rngsubtype, depth + 1)?),
            None => Ok(false),
        },
        F_HASH_MULTIRANGE => match syscache_seams::lookup_pg_range_by_multirange::call(typid)? {
            Some(rng) => hash_agrees(rng, depth + 1),
            None => Ok(false),
        },
        _ => Ok(true),
    }
}

/// The comparison road (a key type with no hash proc: PostgreSQL only sorts it, a Sort by its btree FUNCTION 1 then a
/// Group / Unique by its `=`): classes under that comparison are PostgreSQL's groups when the comparison and the `=` are
/// one btree family's (its BTORDER_PROC and its BTEqualStrategyNumber member, the btree opclass contract) at every level
/// the comparison reads: the type's own (`own_eq`: the `=` the type cache derives, read from the catalog; an `=` taken
/// from the hash family because the btree family has none for the type does not agree), an array's element (btarraycmp
/// and array_eq read the element's), a record's columns (btrecordcmp and record_eq read each column's). A range or a
/// multirange orders and equates its bounds by its subtype opclass's one FUNCTION 1 (range_cmp and range_eq both call
/// range_cmp_bounds), so it agrees. A domain is its base. A type nested deeper than 16 levels, or one the catalog does
/// not know, does not agree.
fn cmp_agrees(typid: Oid, depth: u32) -> R<bool> {
    if depth > 16 {
        return Ok(false);
    }
    let Some(t) = syscache_seams::lookup_pg_type_typcache_shape::call(typid)? else { return Ok(false) };
    if t.typtype as u8 == b'd' {
        let Some(b) = syscache_seams::pg_type_base_shape::call(typid)? else { return Ok(false) };
        return cmp_agrees(b.typbasetype, depth + 1);
    }
    let (opf, intype, eq) = super::super::own_eq(typid)?;
    if opf == 0 || eq == 0 || lsyscache::get_opfamily_member(opf, intype, intype, lsyscache::BTEqualStrategyNumber)? != eq {
        return Ok(false);
    }
    match lsyscache::get_opfamily_proc(opf, intype, intype, super::super::BTORDER_PROC)? {
        0 => Ok(false),
        F_BTARRAYCMP => Ok(t.typelem != 0 && cmp_agrees(t.typelem, depth + 1)?),
        F_BTRECORDCMP => {
            for att in 1.. {
                let Some(a) = syscache_seams::lookup_pg_attribute_shape::call(t.typrelid, att)? else { break };
                if !a.attisdropped && !cmp_agrees(a.atttypid, depth + 1)? {
                    return Ok(false);
                }
            }
            Ok(t.typrelid != 0)
        }
        _ => Ok(true),
    }
}

fn intern(k: UserKey) -> u64 {
    let id = id_of(&k);
    let mut t = SPECS.write().unwrap_or_else(|e| e.into_inner());
    if !t.iter().any(|e| e.0 == id) {
        t.push((id, k)); // alloc: Cold
    }
    id
}

/// The user key of id `id` (any thread, no catalog); an error when no lowering in this process interned it.
pub fn spec(id: u64) -> R<UserKey> {
    let t = SPECS.read().unwrap_or_else(|e| e.into_inner());
    t.iter().find(|e| e.0 == id).map(|e| e.1).ok_or_else(|| Box::new(crate::PgError::error(format!("gravity: rep.hosted.hash: user key {id:#x} was not built in this process")))) // alloc: Cold
}

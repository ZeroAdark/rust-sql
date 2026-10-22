//! The key decisions of lowerv4's rep arms, answered in the key-semantics home: which canonical form or user key a key
//! takes, and where its support calls run. The arms keep the IR they emit and their refusals (rep.hosted.hash `image` /
//! `record_tf` / `user_words`, rep.word.hash `user_spec` / `user_class`, rep.keylane `native_cmp`).
use types_core::Oid;

use super::super::R;

/// How a hashed key keys (rep.hosted.hash `image`): by the Fp128 of an interned canonical form, or by its class under a
/// user key (`m6_user_fp` over one representative per class, through fmgr).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashKey {
    Image(u64),
    User(u64),
}

/// rep.hosted.hash's key for a key of type `typid` with facts' form `rec` (an anonymous record's columns' or a bytea
/// payload's, `record_key_form`) and user key `user` (a hashed group's under a user `=`, a recursive UNION's record key):
/// `rec` when set, else `user` when set, else the type's own canonical form (typesem `form`). A type PostgreSQL compares
/// by user code or a support function no byte image keeps keys by its class under its own `=` (`user_key`); with no
/// user key either, `form`'s reason. The outer error is a catalog read `form` could not make.
pub fn hash_key(typid: Oid, rec: u64, user: u64) -> R<Result<HashKey, &'static str>> {
    if rec != 0 {
        return Ok(Ok(HashKey::Image(rec)));
    }
    if user != 0 {
        return Ok(Ok(HashKey::User(user)));
    }
    Ok(match super::super::form(typid)? {
        Ok((id, _)) => Ok(HashKey::Image(id)),
        Err(why) => match super::user::user_key(typid) {
            Ok(Ok(spec)) => Ok(HashKey::User(spec)),
            _ => Err(why),
        },
    })
}

/// D1604 / D1889 (rep.hosted.user_op `tf`): the user key of a group key of type `typid` whose PostgreSQL node groups it on
/// road `road` (`Hashed` / `Sorted`, `user::user_key_for`), when the type's canonical form refuses it on a user road and
/// the strategy-free user key (`hash_key`'s) refuses too: `Ok(None)` for every other key, which keys as before.
pub fn group_user_key(typid: Oid, road: super::user::Road) -> R<Option<u64>> {
    if road == super::user::Road::Either {
        return Ok(None);
    }
    match super::super::form(typid)? {
        Err(why) if super::user::support_keyed(why) => {}
        _ => return Ok(None),
    }
    if super::user::user_key(typid)?.is_ok() {
        return Ok(None);
    }
    Ok(super::user::user_key_for(typid, road)?.ok())
}

/// rep.hosted.hash `record_tf`'s form: a key under a user `=` over byteaeq (`bytes_eq`, D2163) keys by bytea's form; an
/// anonymous record over the plan-known columns `cols` by their canonical form (typesem `record_form`); any other key
/// keeps its facts (`Ok(None)`). `Err(Some(why))`: a column has no form; `Err(None)`: a catalog read failed (bytea's form
/// answers only that way).
pub fn record_key_form(bytes_eq: bool, cols: Option<&[(Oid, Oid)]>) -> Result<Option<u64>, Option<&'static str>> {
    if bytes_eq {
        return match super::super::form(types_core::catalog::BYTEAOID) {
            Ok(Ok((rec, _))) => Ok(Some(rec)),
            _ => Err(None),
        };
    }
    let Some(cols) = cols else { return Ok(None) };
    match super::super::record_form(cols) {
        Ok(Ok((rec, _))) => Ok(Some(rec)),
        Ok(Err(why)) => Err(Some(why)),
        Err(_) => Err(None),
    }
}

/// D2723 (rep.word.hash `user_spec`): the user key of a by-value key type the type table does not list (`listed`: decompose
/// typefacts has its row) whose `=` is user code (typesem `form` refuses it on a user road), `Ok(None)` for any other
/// (its word is its image). `Err(why)`: such a type with no usable user key, by `form`'s reason (PostgreSQL compares it by
/// user code, so its word is no image).
pub fn word_user_key(typid: Oid, byval: bool, listed: bool) -> Result<Option<u64>, &'static str> {
    if !byval || listed {
        return Ok(None);
    }
    let why = match super::super::form(typid) {
        Ok(Err(why)) if super::user::support_keyed(why) => why,
        _ => return Ok(None),
    };
    match super::user::user_key(typid) {
        Ok(Ok(spec)) => Ok(Some(spec)),
        _ => Err(why),
    }
}

/// The site of a user key's support calls (`m6_user_fp`, `m6_user_class`: the key's procs through fmgr, user code that
/// reaches SPI): the leader. Marks the pipeline being made or emitted serial (`resolved::leader_only`).
pub fn user_key_site() {
    crate::resolved::leader_only();
}

/// rep.keylane `native_cmp`'s site: the id of comparator `proc` (of language `prolang`) for a sort key of type `typid`,
/// bound once at lowering so a pool worker calls a copy with no catalog access, or the reason it cannot be
/// (`resolved::native_key`: a SQL / PL comparator sends the pipeline to the leader, one no worker may call too).
pub fn sort_cmp_site(prolang: Option<Oid>, proc: Oid, typid: Oid) -> Result<u32, &'static str> {
    crate::resolved::native_key(prolang, proc, typid)
}

//! core-face's effect tests, in the effects home (lint rule R1; moved from hir.rs and props/ops.rs): a set-returning
//! call's pool safety, and a SQL / PL body by language. User-code reach by OID is decompose's `effects::user_code`.
use gravity_decompose::catalog::Facts;
use gravity_decompose::effects::{self, Parallel};

/// `pg_language` oids of the languages whose functions run no nested statement (V4R-233).
const LANG_INTERNAL: types_core::Oid = 12;
const LANG_C: types_core::Oid = 13;

/// Whether set-returning function `f` may run in the parallel pool: a call to it is PARALLEL SAFE on a pool
/// (`FuncProps::pool`: a builtin's `proparallel`) and does not act (`effects::of_call`: VOLATILE, or a function the catalog
/// cannot answer) (hir.rs `srf_serial`'s pool-safe series).
pub(crate) fn pool_safe(facts: &dyn Facts, f: u32) -> bool {
    facts.func(f).is_some_and(|p| p.pool(f) == Parallel::Safe) && !effects::of_call(facts, f).acts
}

/// V4R-233 (M5 Decision 23): whether function `f` has a SQL / PL body (a nested statement through SPI) by its language; a
/// language the catalog cannot give (the rig's user functions) by the OID (props/ops.rs, kind srf.pl).
pub(crate) fn pl_body(catalog: &dyn gravity_expr::Catalog<'_>, f: u32) -> bool {
    match catalog.func(f).ok().and_then(|x| x.prolang) {
        Some(lang) => lang != LANG_INTERNAL && lang != LANG_C,
        None => effects::user_code(f),
    }
}

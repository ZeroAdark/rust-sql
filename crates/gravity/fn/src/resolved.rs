//! M6-HOSTED (main 09-28): a comparator resolved once, at lowering, for the threads that call it. A hosted sort
//! (rep.hosted.order's run tail, SK_CMP's group sort) compares in pool worker claims, and a worker must not reach the
//! catalog: a function with no builtin row of its own oid (a LANGUAGE internal alias, a C function) resolves through
//! pg_proc. `resolve` (the backend, at lowering) runs `fmgr_info` and interns the resolved `FmgrInfo` by its content; the
//! program carries the id, and `FmgrCall::bind_resolved` (any thread) binds a copy of it (C `fmgr_info_copy`).
//!
//! An id names one resolution for the process's life: a replaced function resolves to another body, so to another id;
//! the table only grows, by one entry per distinct resolved comparator.
use pgsync::RwLock;

use fmgr::{FmgrInfo, FnKind};
use types_core::Oid;
use types_error::PgResult;

use crate::effects::user_language;

/// A resolved comparator; only `resolve` writes one, with no `fn_extra` and no calling node, and a copy is all a
/// thread reads (`info`).
struct Entry(FmgrInfo);

// SAFETY: an interned `FmgrInfo` holds no `fn_extra` and no `fn_expr` (both `None`, asserted in `resolve`): its fields are
// plain values and a function pointer; it is never called in place, only copied.
unsafe impl Send for Entry {}
unsafe impl Sync for Entry {}

impl Entry {
    fn same(&self, f: &FmgrInfo) -> bool {
        key(&self.0) == key(f)
    }
}

/// A resolution's content: every field a call reads, and the body's address as its offset from this module's code (the
/// same in every process of a build; ASLR moves the base only). Two resolutions with equal keys are one comparator.
fn key(f: &FmgrInfo) -> (Oid, i64, u8, Oid, bool, i16, bool, u8) {
    let at = (f.fn_addr() as *const () as usize as i64).wrapping_sub(resolve as *const () as usize as i64);
    (f.fn_oid, at, f.fn_kind as u8, f.fn_body, f.fn_strict, f.fn_nargs, f.fn_retset, f.fn_stats)
}

/// A resolution's id: the SipHash (fixed keys) of its catalog identity (oid, kind, body, strictness, arity: never the
/// code address, which moves with every build, so a pinned program's words stay put), folded to 32 bits and never 0.
/// When another content already holds the id (a replaced function that resolves to another body), the id moves to
/// the next free one, so an id never names two comparators.
fn id_of(f: &FmgrInfo, t: &[(u32, Entry)]) -> u32 {
    use core::hash::{Hash, Hasher};
    #[allow(deprecated)]
    let mut h = core::hash::SipHasher::new_with_keys(0x6d36_686f_7374_6564, 0x7265_736f_6c76_6564);
    (f.fn_oid, f.fn_kind as u8, f.fn_body, f.fn_strict, f.fn_nargs, f.fn_retset, f.fn_stats).hash(&mut h);
    let x = h.finish();
    let mut id = (x ^ x >> 32) as u32;
    while id == 0 || t.iter().any(|(i, _)| *i == id) {
        id = id.wrapping_add(1);
    }
    id
}

static RESOLVED: RwLock<Vec<(u32, Entry)>> = RwLock::new(Vec::new()); // alloc: Cold

/// `proc` resolved (the catalog read here, at lowering), interned: its id (never 0), or the Shape it refuses by. A
/// SECURITY DEFINER comparator or one with SET options (D3375 N8) resolves too: its call goes through
/// `fmgr_security_definer`, which switches the user and the GUCs around each call as PostgreSQL's does; its kind is no
/// builtin, so `pool_safe` is false and its pipeline runs on the leader, which owns both (`native_key`).
pub fn resolve(proc: Oid) -> PgResult<Result<u32, &'static str>> {
    let f = fmgr_core::fmgr_info(proc)?;
    debug_assert!(f.fn_extra.is_none() && f.fn_expr.is_none(), "gravity_fn: a fresh resolution with a memo");
    if let Some((id, _)) = RESOLVED.read().unwrap_or_else(|p| p.into_inner()).iter().find(|(_, e)| e.same(&f)) {
        return Ok(Ok(*id));
    }
    let mut t = RESOLVED.write().unwrap_or_else(|p| p.into_inner());
    if let Some((id, _)) = t.iter().find(|(_, e)| e.same(&f)) {
        return Ok(Ok(*id));
    }
    let id = id_of(&f, &t);
    t.push((id, Entry(f))); // alloc: Cold
    Ok(Ok(id))
}

/// The Shape of a sort key whose comparator's language the catalog cannot give (`native`: a comparator in a language
/// other than C or internal is served on the leader since spicmp; this name is kept for the unknown language only).
pub const USER_CMP: &str = "a sort key ordered by a comparator that is not a C or internal function (it runs SQL through SPI, which a pool worker cannot)";

/// The Shape of a sort key whose comparator does not resolve at lowering.
pub const UNRESOLVED_CMP: &str = "a sort key ordered by a comparator the function manager cannot resolve";

/// A comparator checked and resolved at lowering (rep.hosted.order's run tail, SK_CMP's group sort): its
/// `pg_proc.prolang` (`prolang`, the caller's catalog read) is one the catalog gives (a language it cannot give refuses
/// USER_CMP), then `resolve`. Its id, or the Shape. A comparator in a language other than internal or C (SQL, plpgsql,
/// plperl, plv8, ..) runs a nested statement through SPI with a snapshot and a portal a pool worker has none of (uc1,
/// 09-28): `crate::effects::user_language` names it, and `native_key` sends its pipeline to the leader (spicmp), where the backend calls
/// it through fmgr as PostgreSQL's tuplesort does (the user's function runs at each compare that reaches it).
pub fn native(prolang: Option<Oid>, proc: Oid) -> Result<u32, &'static str> {
    if prolang.is_none() {
        return Err(USER_CMP);
    }
    resolve(proc).unwrap_or(Err(UNRESOLVED_CMP))
}

/// Whether a pool worker may call comparator `proc` over key type `typid` (main 09-28, default serial): only a builtin
/// by oid (no pg_proc row read to resolve it) over a scalar type whose comparator reads no type cache, no catalog and no
/// `fn_extra` state (numeric_cmp, network_cmp, bitcmp, varbitcmp: wp1 09-28, an array comparator's element state comes
/// from the thread-local typcache, which a pool worker fills through a syscache it has none of). Anything else (arrays,
/// ranges, multiranges, composites, enums, domains, user types, an alias, a C function) is `false`: its sort runs on
/// the leader (`pipeline_scope`).
pub fn pool_safe(typid: Oid, proc: Oid) -> bool {
    const NUMERIC: Oid = 1700;
    const INET: Oid = 869;
    const CIDR: Oid = 650;
    const BIT: Oid = 1560;
    const VARBIT: Oid = 1562;
    matches!((typid, proc), (NUMERIC, 1769) | (INET | CIDR, 926) | (BIT, 1596) | (VARBIT, 1672)) && fmgr_core::fmgr_info(proc).is_ok_and(|f| f.fn_kind == FnKind::Builtin && f.fn_body == proc)
}

leader_tls::leader_tls! {
    thread_local! {
        /// Set by `native` when a comparator the lowering binds is not `pool_safe`: the pipeline being made runs on the
        /// leader (lowerv4 frame/mechs.rs `resolve` and frame/program.rs `functions` scope it per pipeline: `pipeline_scope`).
        /// Lowering is one thread's.
        static SERIAL: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    }
}

/// Runs `f` (one pipeline's makers) with the flag clear, and reports whether a comparator it bound must run on the leader.
/// The flag outside is restored after, so nothing leaks between pipelines or statements (a scope nests, a panic out of
/// `f` leaves it clear for the next lowering: `f` runs under a guard).
pub fn pipeline_scope<R>(f: impl FnOnce() -> R) -> (R, bool) {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            SERIAL.with(|c| c.set(self.0));
        }
    }
    let outer = Restore(SERIAL.with(|c| c.replace(false)));
    let r = f();
    let serial = SERIAL.with(|c| c.get());
    drop(outer);
    (r, serial)
}

/// USEROP (typesem `user_key`): the pipeline being made calls a key's support functions through fmgr, user code that
/// reaches SPI, so it runs on the leader (`pipeline_scope` reports it, as for a comparator `native_key` binds).
pub fn leader_only() {
    SERIAL.with(|c| c.set(true));
}

/// `native` for a sort key of type `typid`: the id, and the pipeline goes serial unless `pool_safe`.
/// A SQL / PL comparator (`crate::effects::user_language`, spicmp) always runs on the leader. A by-value key under its type's own user
/// comparator serves too: its group keys by its class representative's word (semantics' user key, D2723).
pub fn native_key(prolang: Option<Oid>, proc: Oid, typid: Oid) -> Result<u32, &'static str> {
    let user = user_language(prolang) && prolang.is_some();
    let id = native(prolang, proc)?;
    if user || !pool_safe(typid, proc) {
        leader_only();
    }
    Ok(id)
}

/// A copy of resolution `id` (C `fmgr_info_copy`: no `fn_extra`), with no catalog access; `None` for an id no
/// `resolve` gave.
pub fn info(id: u32) -> Option<FmgrInfo> {
    let t = RESOLVED.read().unwrap_or_else(|p| p.into_inner());
    t.iter().find(|(i, _)| *i == id).map(|(_, e)| e.0.clone())
}

/// The kind of resolution `id` (how fmgr calls it); `None` for an id no `resolve` gave. The effects home reads it
/// (`crate::effects::user_call`).
pub(crate) fn kind_of(id: u32) -> Option<FnKind> {
    let t = RESOLVED.read().unwrap_or_else(|p| p.into_inner());
    t.iter().find(|(i, _)| *i == id).map(|(_, e)| e.0.fn_kind)
}

/// The table's entries (the growth test).
pub fn len() -> usize {
    RESOLVED.read().unwrap_or_else(|p| p.into_inner()).len()
}

#[cfg(test)]
mod tests {
    use core::cmp::Ordering;

    /// btint4cmp (351): one id per resolution, the same on a second resolve; a copy binds and compares on another thread
    /// with no catalog. OID-17 (overfit, keep): 351 is this test's fixture comparator and the `info(id ^ 1)` line its
    /// oracle; does not apply to production code, where no path compares a resolved `fn_oid` with a literal.
    #[test]
    fn a_resolved_comparator_binds_on_any_thread() {
        let id = super::resolve(351).unwrap().unwrap();
        assert_ne!(id, 0);
        assert_eq!(super::resolve(351).unwrap().unwrap(), id);
        assert_eq!(super::info(id).unwrap().fn_oid, 351);
        assert!(super::info(id ^ 1).is_none_or(|f| f.fn_oid != 351));
        let other = std::thread::spawn(move || {
            let mut f = crate::FmgrCall::bind_resolved(id, 2, 0).unwrap();
            let cx = mcx::MemoryContext::new("resolved test");
            let r = f.call(cx.mcx(), &[datum::Datum::from_i32(3), datum::Datum::from_i32(7)], &[false, false]).unwrap();
            r.value.as_i32().cmp(&0)
        });
        assert_eq!(other.join().unwrap(), Ordering::Less);
        assert!(super::len() >= 1);
    }

    /// The classes: a scalar builtin comparator over its own type stays parallel (numeric, inet / cidr, bit, varbit);
    /// arrays, ranges, multiranges, records, enums, a domain's or user type's oid, and a comparator over another type
    /// run serial.
    #[test]
    fn only_scalar_builtin_comparators_are_pool_safe() {
        for (t, p) in [(1700, 1769), (869, 926), (650, 926), (1560, 1596), (1562, 1672)] {
            assert!(super::pool_safe(t, p), "scalar {t} / {p}");
        }
        // int4[] / text[] (btarraycmp), int4range (range_cmp), int4multirange (multirange_cmp), record (btrecordcmp),
        // an enum type oid over enum_cmp, a domain / user type oid over numeric_cmp, numeric_cmp over inet
        for (t, p) in [(1007, 382), (1009, 382), (3904, 3870), (4451, 4273), (2249, 2987), (16384, 3514), (16390, 1769), (869, 1769)] {
            assert!(!super::pool_safe(t, p), "serial {t} / {p}");
        }
        let (id, serial) = super::pipeline_scope(|| super::native_key(Some(12), 1769, 1700));
        assert_eq!((id, serial), (super::native(Some(12), 1769), false), "numeric stays parallel");
        let (_, serial) = super::pipeline_scope(|| super::native_key(Some(12), 382, 1007));
        assert!(serial, "an array key's comparator runs serial");
    }

    /// spicmp: a comparator whose language is SQL (14) or a PL resolves and sends its pipeline to the leader, even over a
    /// type whose builtin comparator would stay parallel (numeric_cmp's oid stands in for the user function: the unit
    /// test has no catalog to resolve one); a language the catalog cannot give still refuses USER_CMP. OID-17-style
    /// fixture oids (1769 / 1700): test-only, no production path compares a literal.
    #[test]
    fn a_sql_or_pl_comparator_runs_on_the_leader() {
        for lang in [14, 13000, 16384] {
            let (id, serial) = super::pipeline_scope(|| super::native_key(Some(lang), 1769, 1700));
            assert_eq!((id, serial), (super::native(Some(12), 1769), true), "language {lang}");
        }
        assert_eq!(super::native(None, 1769), Err(super::USER_CMP));
        let (_, serial) = super::pipeline_scope(super::leader_only);
        assert!(serial, "leader_only marks the pipeline");
        let (_, after) = super::pipeline_scope(|| ());
        assert!(!after, "and only that one");
        assert!(super::user_language(Some(14)) && super::user_language(None) && !super::user_language(Some(12)) && !super::user_language(Some(13)));
    }

    /// Two pipelines of one graph, the first binding an array comparator (serial), the second a numeric one (parallel):
    /// the second is not serial. The flag does not leak out of a scope either (the next statement), nor out of a scope
    /// its maker panicked in (a refusal).
    #[test]
    fn the_serial_flag_is_one_pipelines() {
        let (_, first) = super::pipeline_scope(|| super::native_key(Some(12), 382, 1007).unwrap());
        let (_, second) = super::pipeline_scope(|| super::native_key(Some(12), 1769, 1700).unwrap());
        assert_eq!((first, second), (true, false));
        let (_, none) = super::pipeline_scope(|| ());
        assert!(!none, "a pipeline that binds no comparator is not serial");
        let _ = std::panic::catch_unwind(|| super::pipeline_scope(|| {
            super::native_key(Some(12), 382, 1007).unwrap();
            panic!("a maker refuses");
        }));
        let (_, after) = super::pipeline_scope(|| super::native_key(Some(12), 926, 869).unwrap());
        assert!(!after, "a refused pipeline's flag does not reach the next lowering's");
        let (_, nested) = super::pipeline_scope(|| {
            super::native_key(Some(12), 3870, 3904).unwrap();
            let (_, inner) = super::pipeline_scope(|| ());
            assert!(!inner);
        });
        assert!(nested, "an inner scope keeps the outer pipeline's flag");
    }
}

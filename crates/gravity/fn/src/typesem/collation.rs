//! Collation facts (docs/gravity/expr.md §3 case 11): provider, determinism, C-ness and the
//! provider's version for a collation OID, and per function the predicate "the dictionary's code
//! order (byte order of the sorted entries) is this function's collation order", which P4 and
//! the kernels consult before using code order for anything.

use adt_datetime::tz::{self, PgTzRef};
use adt_datetime::ClearTimeZoneAbbrevCache;
use pg_locale::PgLocale;
use types_core::Oid;
use types_error::{PgError, PgResult};

pub use pg_locale::{COLLPROVIDER_BUILTIN, COLLPROVIDER_ICU, COLLPROVIDER_LIBC};

pub const DEFAULT_COLLATION_OID: Oid = 100;
pub const C_COLLATION_OID: Oid = 950;
pub const POSIX_COLLATION_OID: Oid = 951;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Facts {
    pub oid: Oid,
    pub provider: u8,
    pub deterministic: bool,
    pub collate_is_c: bool,
    pub ctype_is_c: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    Order,
    Equality,
    None,
}

impl Facts {
    pub const C: Facts = Facts { oid: C_COLLATION_OID, provider: COLLPROVIDER_LIBC, deterministic: true, collate_is_c: true, ctype_is_c: true };

    pub fn of(collid: Oid) -> PgResult<Facts> {
        if collid == C_COLLATION_OID || collid == POSIX_COLLATION_OID {
            return Ok(Facts { oid: collid, ..Facts::C });
        }
        let l = pg_locale::pg_newlocale_from_collation(collid)?;
        Ok(Facts { oid: collid, provider: l.provider, deterministic: l.deterministic, collate_is_c: l.collate_is_c, ctype_is_c: l.ctype_is_c })
    }

    pub fn version(&self) -> PgResult<Option<String>> {
        if self.oid == C_COLLATION_OID || self.oid == POSIX_COLLATION_OID {
            return Ok(None);
        }
        let cx = crate::session::root_context("gravity_fn collation version"); // alloc: Cold
        let Some(row) = syscache_seams::lookup_pg_collation_locale_row::call(cx.mcx(), self.oid)? else {
            return Ok(None);
        };
        let locale = if row.collprovider == COLLPROVIDER_LIBC { row.collcollate.as_ref() } else { row.colllocale.as_ref() };
        match locale {
            Some(l) => pg_locale::get_collation_actual_version(row.collprovider, l.as_str()),
            None => Ok(None),
        }
    }

    #[inline]
    pub fn order_is_bytes(&self) -> bool {
        self.collate_is_c
    }

    #[inline]
    pub fn equality_is_bytes(&self) -> bool {
        self.deterministic
    }

    pub fn code_order_is_collation_order(&self, proc_oid: Oid) -> bool {
        match need_of(proc_oid) {
            Need::Order => self.order_is_bytes(),
            Need::Equality => self.equality_is_bytes(),
            Need::None => true,
        }
    }
}

pub fn need_of(proc_oid: Oid) -> Need {
    let Some(row) = crate::row_of(proc_oid) else { return Need::Order };
    need_of_name(row.name)
}

pub fn need_of_name(name: &str) -> Need {
    let base = name.strip_suffix("_pattern").unwrap_or(name);
    if base.ends_with("_pattern_lt") || base.ends_with("_pattern_le") || base.ends_with("_pattern_gt") || base.ends_with("_pattern_ge") || base.ends_with("_pattern_cmp") {
        return Need::None;
    }
    if base.ends_with("lt") || base.ends_with("le") || base.ends_with("gt") || base.ends_with("ge") || base.ends_with("cmp") || base.contains("larger") || base.contains("smaller") || base.ends_with("min") || base.ends_with("max") || base.contains("sort") {
        return Need::Order;
    }
    if base.ends_with("eq") || base.ends_with("ne") || base.starts_with("hash") || base.contains("like") || base.contains("regex") || base.contains("starts_with") || base.contains("pos") || base.contains("strpos") || base.contains("replace") {
        return Need::Equality;
    }
    Need::None
}

/// D1236 (MERGE R3, V4R-487): `pattern` compiles as a regular expression under `collation` with `~`'s flags (`icase`:
/// `~*`'s), checked once at plan time: the row's own call over an empty subject (the collation check, then the RE2 tier or
/// Spencer, as `regex_engine` dispatches it) and Spencer's compile under the row path's flags (the planner's
/// `regexp_fixed_prefix`), which a row whose subject RE2 cannot take falls back to. An invalid pattern or a
/// nondeterministic collation is the Err the first row would raise: false. So is a thread without the database default
/// locale or the catalog (no backend: a lowering or front-walk test, whose facts answer false), as `prefix_enabled`.
pub fn regex_compiles(pattern: &[u8], icase: bool, collation: Oid) -> bool {
    if pg_locale::default_locale().is_none() || !syscache_seams::lookup_pg_collation_locale_row::is_installed() {
        return false;
    }
    let cx = mcx::MemoryContext::new_bump("gravity regex_compiles"); // alloc: Cold
    let row = if icase { adt_regexp::texticregexeq(cx.mcx(), b"", pattern, collation) } else { adt_regexp::textregexeq(cx.mcx(), b"", pattern, collation) };
    row.is_ok() && adt_regexp::regexp_fixed_prefix(cx.mcx(), pattern, icase, collation).is_ok()
}

/// A-16b: the regex engine's case map under `collid` (regc_pg_locale.c `pg_wc_toupper` / `pg_wc_tolower` as regex_core's
/// `pg_set_regex_collation` picks the strategy) takes every ASCII letter to its ASCII twin, so `REG_ICASE` over an ASCII
/// pattern is ASCII case folding: a C ctype; ICU (`u_toupper`, the root's simple mapping) and the builtin provider
/// (Unicode's simple mapping); the database's libc default (ASCII forced below 128); another libc locale when its
/// `towupper` / `towlower` agree on all 52 letters (tr_TR's `i` -> `İ` does not). A nondeterministic collation (the
/// engine refuses it), an error, or a thread without the catalog (as `regex_compiles`) answers no.
pub fn regex_ascii_cases(collid: Oid) -> bool {
    if collid == C_COLLATION_OID || collid == POSIX_COLLATION_OID {
        return true;
    }
    if pg_locale::default_locale().is_none() || !syscache_seams::lookup_pg_collation_locale_row::is_installed() {
        return false;
    }
    let Ok(l) = pg_locale::pg_newlocale_from_collation(collid) else { return false };
    if !l.deterministic {
        return false;
    }
    if l.ctype_is_c || l.provider != COLLPROVIDER_LIBC || l.is_default {
        return true;
    }
    (b'a'..=b'z').all(|c| {
        let (lc, uc) = (c as u32, c.to_ascii_uppercase() as u32);
        l.wc_toupper_wide(lc) == uc && l.wc_tolower_wide(lc) == lc && l.wc_toupper_wide(uc) == uc && l.wc_tolower_wide(uc) == lc
    })
}

/// The backend's database encoding (captured at admission for a C-tier body that runs on a pool worker).
pub fn database_encoding() -> i32 {
    mbutils::GetDatabaseEncoding() as i32
}

/// Arm this thread with the statement's database encoding: a pool worker boots as SQL_ASCII, and the
/// regexp dispatch classifies a pattern under the thread's encoding (RE2 only under UTF-8).
pub fn arm_database_encoding(encoding: i32) -> PgResult<()> {
    if mbutils::GetDatabaseEncoding() as i32 != encoding {
        mbutils::SetDatabaseEncoding(encoding as _)?;
    }
    Ok(())
}

/// The backend state a C-tier body reads that a pool worker does not inherit: the database
/// encoding (the regexp dispatch, multibyte lengths), the database default locale (the DEFAULT
/// collation's collate/ctype), every other collation the leader resolved (a worker has no
/// catcache: D316) and the session timezone (timestamptz formatting and parts).
/// Captured on the leader when a section or job is submitted; each worker applies it per claim —
/// three compares against the thread's live values, a store only where one differs, nothing per row.
#[derive(Clone)]
pub struct StatementArm {
    encoding: i32,
    default_locale: Option<&'static PgLocale>,
    collations: &'static [(Oid, &'static PgLocale)],
    session_timezone: Option<PgTzRef>,
    /// C7 (c): the database ctype (tsearch's `database_ctype_is_c`, `with_backend_ctype`'s LC_CTYPE).
    ctype: pg_locale::DatabaseCtype,
}

/// A claim's collation arming (`StatementArm::arm`): dropped at the claim's end, on an unwind too, it disarms the thread,
/// so no later serve on the thread reads a slice of a statement (or a session) that has ended (D316). The drop is a plain
/// thread-local store (`pg_locale::arm_collations`): no allocation and no panic, since it runs inside the pool's
/// `catch_unwind` during an unwind, where a panic would abort the server.
/// D1367: a claim that moved the thread's database default locale puts the thread's own back (`prev`, with its database
/// id through `arm_default_locale`), so a retained pool thread never carries a leader's default into a later parallel
/// query task of another database (`init_database_collation` keeps a locale it finds armed).
#[must_use = "the collations stay armed only while the guard lives"]
pub struct Armed {
    prev: Option<Option<&'static PgLocale>>,
    /// C7 (c): the thread's own database ctype, when the claim armed its leader's.
    ctype: Option<pg_locale::DatabaseCtype>,
}

impl Drop for Armed {
    fn drop(&mut self) {
        pg_locale::arm_collations(&[]);
        let _ = CLAIM.try_with(|c| c.set(None)); // V4R-452: a plain store, as above
        // V4R-453 (D1409): a hosted aggregate's catalog arm (armed by the arena on the claim's first call) ends with the
        // claim: the rows forgotten, typcache's per-claim entries dropped (no allocation, no panic)
        syscache_seams::arm_type_rows(&[]);
        typcache::armed::disarm();
        if let Some(prev) = self.prev {
            pg_locale::arm_default_locale(prev);
        }
        if let Some(own) = self.ctype.take() {
            pg_locale::restore_database_ctype(own);
        }
    }
}

std::thread_local! {
    /// V4R-452 (D1409a): the collations a claim armed (`arm`), `None` off a claim: `check_worker_collation`'s truth.
    static CLAIM: core::cell::Cell<Option<&'static [(Oid, &'static PgLocale)]>> = const { core::cell::Cell::new(None) };
    /// B2 (workersess): the claim generation a claim armed (session.rs `arm_claim_gen`), 0 off a claim. Kept in this block
    /// with the claim's other state.
    pub(crate) static CLAIM_GEN: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
    /// B2: the statement catalog arm a leader holds for a section (pool_catalog.rs `arm_leader_statement`).
    pub(crate) static LEADER_CATALOG: core::cell::RefCell<Option<std::sync::Arc<crate::pool_catalog::StatementCatalog>>> = const { core::cell::RefCell::new(None) };
}

/// Whether this thread runs a pool claim (V4R-453: a hosted aggregate arms its catalog facts only there; the leader has
/// its catcache).
#[inline]
pub fn on_claim() -> bool {
    CLAIM.try_with(|c| c.get().is_some()).unwrap_or(false)
}

/// Whether a collation reaches a pool worker with nothing resolved for it: none (0), the database default (a claim arms
/// the default locale) or C / POSIX (static locales).
pub fn armed_by_default(collid: Oid) -> bool {
    matches!(collid, 0 | DEFAULT_COLLATION_OID | C_COLLATION_OID | POSIX_COLLATION_OID)
}

#[cold]
fn unresolved(collid: Oid) -> Box<PgError> {
    let m = format!("gravity: collation {collid} was not resolved on the leader for a pool worker (D1409)"); // alloc: Cold
    Box::new(PgError::error(m)) // alloc: Cold
}

/// V4R-452 (D1409a): under a claim, a collation neither armed by default nor in the claim's armed slice is a named ERROR,
/// where the worker would otherwise look its locale up in a catcache it does not have; off a claim (the leader), Ok.
pub fn check_worker_collation(collid: Oid) -> PgResult<()> {
    match CLAIM.with(core::cell::Cell::get) {
        Some(armed) if !armed_by_default(collid) && !armed.iter().any(|&(o, _)| o == collid) => Err(unresolved(collid)),
        _ => Ok(()),
    }
}

impl StatementArm {
    /// The leader's stamp. Its resolved collations are the ones its statement compiled (gravity_expr `bind_typed` resolves
    /// a call's collation on the leader); a snapshot that fails to allocate arms none, and a worker then errors as before.
    pub fn capture() -> StatementArm {
        // D1813: POSIX is armed by default (`armed_by_default`), but pg_locale builds it from the catalog like any libc
        // collation (only C is static), so the leader resolves it here and every claim carries it; a leader that never
        // computed with POSIX (`Facts::of` answers it without pg_locale) left a worker's initcap / `<` to its catcache.
        // A failed resolve (no catalog seam: a crate test) arms what the leader has, as before. The resolve runs with
        // interrupts held (D376): its catalog read reaches CHECK_FOR_INTERRUPTS, and a cancel or statement_timeout
        // raised there into this `let _ =` was consumed and lost (the statement ran on and answered, with the scan's
        // buffer pin and snapshot left to the resource owner). Held, it stays pending for the run's next check.
        if syscache_seams::lookup_pg_collation_locale_row::is_installed() {
            let _held = init_small::globals::hold_interrupts();
            let _ = pg_locale::pg_newlocale_from_collation(POSIX_COLLATION_OID);
        }
        let collations = pg_locale::resolved_collations().unwrap_or(&[]);
        StatementArm { encoding: database_encoding(), default_locale: pg_locale::default_locale(), collations, session_timezone: tz::session_timezone(), ctype: pg_locale::capture_database_ctype() }
    }

    fn key(&self) -> (i32, usize, usize) {
        (
            self.encoding,
            self.default_locale.map_or(0, |l| std::ptr::from_ref(l) as usize),
            self.session_timezone.as_ref().map_or(0, |z| PgTzRef::as_ptr(z) as usize),
        )
    }

    /// Arm this thread with the stamp, each piece compared with the thread's live value, not a remembered stamp (D351): a
    /// parallel query task on the same standing thread resets its GUC store and binds its own session's, whose assign
    /// hooks move the zone (and the encoding and default locale) under an unchanged stamp. The collations are `arm`'s.
    pub fn apply(&self) -> PgResult<()> {
        arm_database_encoding(self.encoding)?;
        if pg_locale::default_locale().map(std::ptr::from_ref) != self.default_locale.map(std::ptr::from_ref) {
            pg_locale::arm_default_locale(self.default_locale);
        }
        // Compared under a borrow (no handle per claim); the set runs after the borrow ends.
        let live = tz::with_session_timezone(|z| z.map(std::ptr::from_ref));
        if live != self.session_timezone.as_deref().map(std::ptr::from_ref) {
            tz::set_session_timezone(self.session_timezone.clone());
            // PG 18's assign_timezone clears it too: an abbreviation the session zone itself uses (IST under
            // Asia/Kolkata) answers from the zone before the table, so an entry cached under the old zone is wrong (D3112).
            ClearTimeZoneAbbrevCache();
        }
        Ok(())
    }

    /// A claim's arming: `apply`, and the leader's resolved collations until the returned guard drops (a pool worker has
    /// no catcache to build a collation's locale from: D316). One store per claim, nothing per row.
    pub fn arm(&self) -> PgResult<Armed> {
        let prev = pg_locale::default_locale();
        let moved = prev.map(std::ptr::from_ref) != self.default_locale.map(std::ptr::from_ref);
        // C7 (c): the database ctype first, so a failed apply's guard puts it back
        let mut armed = Armed { prev: None, ctype: pg_locale::arm_database_ctype(&self.ctype)? };
        self.apply()?;
        pg_locale::arm_collations(self.collations);
        CLAIM.with(|c| c.set(Some(self.collations)));
        armed.prev = moved.then_some(prev);
        Ok(armed)
    }

    pub fn encoding(&self) -> i32 {
        self.encoding
    }
}

impl PartialEq for StatementArm {
    fn eq(&self, other: &StatementArm) -> bool {
        self.key() == other.key()
    }
}

impl Eq for StatementArm {}

impl std::fmt::Debug for StatementArm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (enc, loc, tz) = self.key();
        write!(f, "StatementArm(encoding {enc}, locale {loc:#x}, {} collations, timezone {tz:#x})", self.collations.len())
    }
}

/// The provider's sort key of `src` under `collid` into `dest` (`pg_strnxfrm`): `None` when the provider has no trusted sort key (libc: `TRUST_STRXFRM` is off), else the key's length — past `dest.len()` the key was cut short and the caller retries with a larger buffer.
/// Under `guarded` (D314): a kernel's `extern "C"` entry calls it.
pub fn sort_key(collid: Oid, src: &[u8], dest: &mut [u8]) -> PgResult<Option<usize>> {
    crate::guarded(|| {
        let l = pg_locale::pg_newlocale_from_collation(collid)?;
        if !l.pg_strxfrm_enabled() {
            return Ok(None);
        }
        Ok(Some(l.pg_strnxfrm(dest, src)?))
    })
}

/// Whether `collid`'s provider gives a trusted sort key (`sort_key` answers `Some`): the locale built here, on the caller's
/// thread (the leader's, so a pool worker's claim carries it).
pub fn strxfrm_trusted(collid: Oid) -> PgResult<bool> {
    crate::guarded(|| Ok(pg_locale::pg_newlocale_from_collation(collid)?.pg_strxfrm_enabled()))
}

/// D1581 (G4 L1', V4R-517): `compare` for a sort's compare of two payloads (kernels rowsort.rs `var_cmp`), `varstr_cmp`'s
/// result: "C" / "POSIX" compare the bytes; any other collation goes straight to the collation seam (`varstr_cmp_locale`:
/// the locale's resolve, its equality probe, the locale's order, a deterministic collation's bytes tie-break), without
/// `varstr_cmp`'s own equality probe in front of the seam's: PostgreSQL's varstrfastcmp_locale probes once. The seam stays
/// the one compare (D1293 injects its error there); no collation (0) is `varstr_cmp`'s error. Under `guarded` (D314).
pub fn sort_compare(a: &[u8], b: &[u8], collid: Oid) -> PgResult<i32> {
    if collid == C_COLLATION_OID || collid == POSIX_COLLATION_OID {
        return Ok(varlena::varstrfastcmp_c(a, b));
    }
    crate::guarded(|| {
        varlena::check_collation_set(collid)?;
        pg_locale_seams::varstr_cmp_locale::call(collid, a, b)
    })
}

/// `varstr_cmp` of two byte strings under `collid`, under `guarded` (D314): a kernel's `extern "C"` entry calls it.
pub fn compare(a: &[u8], b: &[u8], collid: Oid) -> PgResult<i32> {
    crate::guarded(|| varlena::varstr_cmp(a, b, collid))
}

/// D1582 (G11): whether `collid`'s provider gives a sort-key prefix (`pg_strnxfrm_prefix_enabled`: ICU), the prefix
/// PostgreSQL abbreviates a non-C sort key to (varlena.c `varstr_abbrev_convert`). Asked at lower time on the backend,
/// where decompose's facts (hosts `coll`, `Facts::of`) built the same locale; a collation whose locale cannot be built
/// answers false (its comparator raises the error at run time, as before), and so does a thread without the database
/// default locale or the catalog (no backend: a lowering test, whose facts answer `KeyColl::of`).
pub fn prefix_enabled(collid: Oid) -> bool {
    if matches!(collid, 0 | C_COLLATION_OID | POSIX_COLLATION_OID) || pg_locale::default_locale().is_none() || !syscache_seams::lookup_pg_collation_locale_row::is_installed() {
        return false;
    }
    // interrupts held (D376): a cancel the catalog read meets stays pending instead of being consumed into `false`
    let _held = init_small::globals::hold_interrupts();
    pg_locale::pg_newlocale_from_collation(collid).is_ok_and(|l| l.pg_strnxfrm_prefix_enabled())
}

/// D1582 (G11): the first 8 bytes of the provider's sort key of `src` under `collid` (`pg_strnxfrm_prefix`, ICU's
/// `ucol_nextSortKeyPart`), big-endian and zero-padded as PostgreSQL's abbreviated key (varlena.c `varstr_abbrev_convert`):
/// two strings whose words differ order as the words do (unsigned), equal words are the full comparator's to order.
/// `None` when the provider has no prefix (the caller then gives every value one word). Under `guarded` (D314): a kernel's
/// `extern "C"` entry calls it.
pub fn sort_key_prefix(collid: Oid, src: &[u8]) -> PgResult<Option<u64>> {
    crate::guarded(|| {
        let l = pg_locale::pg_newlocale_from_collation(collid)?;
        if !l.pg_strnxfrm_prefix_enabled() {
            return Ok(None);
        }
        let mut w = [0u8; 8];
        let n = l.pg_strnxfrm_prefix(&mut w, src)?.min(8);
        w[n..].fill(0);
        Ok(Some(u64::from_be_bytes(w)))
    })
}

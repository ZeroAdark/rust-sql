use types_core::Oid;
use types_error::PgResult;

seam_core::seam!(
    pub fn varstr_cmp_locale(collid: Oid, arg1: &[u8], arg2: &[u8]) -> PgResult<i32>
);

seam_core::seam!(
    pub fn collation_is_deterministic(collid: Oid) -> PgResult<bool>
);

seam_core::seam!(
    pub fn collation_collate_is_c(collid: Oid) -> PgResult<bool>
);

seam_core::seam!(
    pub fn collation_ctype_is_c(collid: Oid) -> PgResult<bool>
);

seam_core::seam!(
    // category is the POSIX LC_* value; Ok(None) is C's NULL return (setlocale failure).
    pub fn pg_perm_setlocale<'mcx>(
        mcx: mcx::Mcx<'mcx>,
        category: i32,
        locale: &str,
    ) -> PgResult<Option<mcx::PgString<'mcx>>>
);

seam_core::seam!(
    pub fn set_database_ctype_is_c(v: bool)
);

seam_core::seam!(
    pub fn init_database_collation() -> PgResult<()>
);

seam_core::seam!(
    // Ok(None) = deterministic collation, the caller hashes the raw bytes. seed None = 32-bit hash zero-extended.
    pub fn varstr_nondeterministic_hash(
        collid: Oid,
        data: &[u8],
        seed: Option<u64>,
    ) -> PgResult<Option<u64>>
);

seam_core::seam!(
    // Ok(None) is C's NULL (no version for this provider).
    pub fn get_collation_actual_version<'mcx>(
        mcx: mcx::Mcx<'mcx>,
        collprovider: u8,
        locale: &str,
    ) -> PgResult<Option<mcx::PgString<'mcx>>>
);

seam_core::seam!(
    // None = libicu not loadable, matching a C build without --with-icu.
    pub fn icu_unicode_version() -> Option<&'static str>
);


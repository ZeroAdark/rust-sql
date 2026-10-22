use ::mcx::Mcx;
use ::types_core::{Oid, PgWChar};
use ::types_error::PgResult;
use ::regex::{RegMatch, RegcompResult, RegexCompiled, RegexecResult, RegprefixResult};

seam_core::seam!(
    pub fn pg_set_regex_collation<'mcx>(
        mcx: Mcx<'mcx>,
        collation: Oid,
    ) -> PgResult<()>
);

seam_core::seam!(
    pub fn pg_regcomp(
        pattern: &[PgWChar],
        cflags: i32,
        collation: Oid,
    ) -> PgResult<RegcompResult>
);

seam_core::seam!(
    pub fn pg_regexec(
        re: &RegexCompiled,
        data: &[PgWChar],
        search_start: i32,
        pmatch: &mut [RegMatch],
    ) -> PgResult<RegexecResult>
);

seam_core::seam!(
    pub fn pg_regprefix<'mcx>(
        mcx: Mcx<'mcx>,
        re: &RegexCompiled,
    ) -> PgResult<RegprefixResult<'mcx>>
);

seam_core::seam!(
    /// In C, `regexp.c` evicts a cache entry with
    /// `MemoryContextDelete(cre_context)`, which frees the palloc'd engine
    /// state; here the carrier is taken by value and its `Rc` dropped, freeing
    /// the engine `regex_t` when the last reference goes away.
    pub fn pg_regfree(re: RegexCompiled)
);

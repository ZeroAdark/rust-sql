#![allow(non_snake_case)]

use ::mcx::{Mcx, PgVec};
use ::types_core::Oid;
use ::types_error::PgResult;
use ::regex::{RegMatch, RegexCompiled};

seam_core::seam!(
    pub fn RE_compile_and_cache<'mcx>(
        mcx: Mcx<'mcx>,
        pattern: &[u8],
        cflags: i32,
        collation: Oid,
    ) -> PgResult<RegexCompiled>
);

seam_core::seam!(
    pub fn RE_compile_and_execute<'mcx>(
        mcx: Mcx<'mcx>,
        pattern: &[u8],
        dat: &[u8],
        cflags: i32,
        collation: Oid,
        pmatch: &mut [RegMatch],
    ) -> PgResult<bool>
);

seam_core::seam!(
    pub fn regexp_fixed_prefix<'mcx>(
        mcx: Mcx<'mcx>,
        text_re: &[u8],
        case_insensitive: bool,
        collation: Oid,
    ) -> PgResult<Option<(PgVec<'mcx, u8>, bool)>>
);

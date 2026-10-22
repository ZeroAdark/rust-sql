//! Keys are header-ful `text` varlena images (flag byte 'K'/'V'/'N' + bytes,
//! hstore_gin.c makeitem); check values are GinTernaryValue (i8: 0 false, 1
//! true, 2 maybe).

use types_error::PgResult;

seam_core::seam!(
    pub fn hstore_extract_value(hs_image: &[u8]) -> PgResult<Vec<Vec<u8>>>
);

seam_core::seam!(
    pub fn hstore_extract_query(query_image: &[u8], strategy: u16) -> PgResult<(Vec<Vec<u8>>, i32)>
);

seam_core::seam!(
    pub fn hstore_consistent(check: &[i8], strategy: u16, nkeys: usize) -> PgResult<(bool, bool)>
);

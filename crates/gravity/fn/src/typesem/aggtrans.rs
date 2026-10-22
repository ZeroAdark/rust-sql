//! The type-cache reads hosts ops/agg/trans makes at run time for a parallel aggregate arena's catalog arm
//! (hosted_catalog.rs): the comparison facts an aggregate's support functions look up in the type cache on their first
//! call, resolved on the leader at arena build (lookup_type_cache, as those calls resolve them in ExecInitAgg's process),
//! so a pool claim reads them in place of a catalog. Run-time reads through typcache's own cache: nothing is cached or
//! recorded here.
use types_core::Oid;
use types_error::PgResult;

const TYPTYPE_RANGE: i8 = b'r' as i8;
const TYPTYPE_MULTIRANGE: i8 = b'm' as i8;

/// Type `t`'s type-cache image (`typtype` is its pg_type row's), with typcache's own closure (an array's element, a
/// range's subtype, a multirange's range), appended to `images`: its equality and ordering operators and its comparison
/// procedure with their FmgrInfos (array_cmp's element compare, an enum's order, the extremes' comparisons), and a range's
/// or multirange's own info (its subtype's compare).
pub fn compare_closure(t: Oid, typtype: i8, images: &mut Vec<typcache::armed::TypcacheImage>) -> PgResult<()> {
    let more = match typtype {
        TYPTYPE_RANGE => typcache::TYPECACHE_RANGE_INFO,
        TYPTYPE_MULTIRANGE => typcache::TYPECACHE_MULTIRANGE_INFO,
        _ => 0,
    };
    let flags = typcache::TYPECACHE_EQ_OPR | typcache::TYPECACHE_LT_OPR | typcache::TYPECACHE_GT_OPR | typcache::TYPECACHE_CMP_PROC | typcache::TYPECACHE_EQ_OPR_FINFO | typcache::TYPECACHE_CMP_PROC_FINFO | more;
    typcache::armed::image_closure(t, flags, images)
}

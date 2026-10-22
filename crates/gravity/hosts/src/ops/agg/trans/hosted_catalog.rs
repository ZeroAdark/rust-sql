//! The hosted aggregate bracket's catalog arm (a `#[path]` child of hosts v4/m6_agg.rs, split from m6_agg_bind.rs, V4R-453
//! D1409b): the catalog facts a parallel arena's pool claims read in place of a catcache, resolved on the leader at arena
//! build, the audited transition / combine functions that gate it, and its arming on a claim. Created by M6-REPS.
use types_core::Oid;
use types_error::PgResult;

mod raw;

use crate::ops::agg::trans::hosted::Spec;

/// V4R-453 (D1409, D1407's proper fix): the catalog facts a parallel arena's pool claims read in place of a catcache (a
/// pool thread has none): each type's pg_type row (lsyscache's readers) and typcache entry (array_cmp's element compare,
/// an enum's order, a range's subtype), resolved on the leader at arena build as ExecInitAgg resolves its own. The arena
/// owns them; its entries arm them on a claim's first call, and the claim's end disarms them (gravity_fn Armed).
pub(crate) struct CatalogArm {
    pub(crate) rows: Box<[syscache_seams::ArmedTypeRow]>, // alloc: Exec
    pub(crate) images: Box<[typcache::armed::TypcacheImage]>, // alloc: Exec
}

/// V4R-453 (D1409): the transition and combine functions a parallel arena may run on a pool worker: every transfn and
/// combinefn of PG 18's parallel-capable aggregates (a combinefn in pg_aggregate.dat, not ordered-set), 101 functions, each
/// read for what it looks up on first call and per state. Catalog-free: the int / float / numeric / interval / money /
/// date-time / bool / bit / oid / tid / xid8 / pg_lsn / inet / bytea arithmetic and extremes, count, string_agg, any_value.
/// Covered by the catalog arm: array_agg / array_agg_array (the element's and the array's pg_type rows), array_larger /
/// smaller (the element's typcache compare), enum_larger / smaller (fn_extra's enum type, set here, and the enum's typcache
/// order), range / multirange_intersect_agg (pg_range rows, the range's and its subtype's typcache entries), text / bpchar
/// extremes (the input collation, V4R-452). record_larger / smaller are listed but a record or row type runs serial.
/// Any other function (a user aggregate over another builtin, a PL or C function) runs its arena serial.
pub(crate) const AUDITED: &[Oid] = &[
    6292, 6296, 4051, 6293, 2333, 515, 516, 1673, 1674, 1675, 2515, 2516, 1063, 1064, 6393, 6394, 3543, 894, 898,
    899, 1138, 1139, 3525, 3524, 208, 209, 204, 211, 222, 276, 2806, 3342, 223, 218, 224, 1834, 1962, 1840, 1892,
    770, 1893, 771, 1894, 1835, 1963, 3324, 1841, 1898, 768, 1899, 769, 1900, 1836, 2746, 2785, 1904, 1219, 2804,
    2805, 1236, 1905, 463, 1237, 1906, 1843, 3325, 1198, 1197, 4388, 3562, 3563, 1833, 2858, 3337, 3341, 1767,
    3338, 1766, 1965, 1966, 4187, 4188, 4401, 6375, 6376, 6299, 3535, 458, 459, 2795, 2796, 1377, 1378, 2036, 2035,
    1196, 1195, 1379, 1380, 5097, 5098,
];

const RECORDOID: Oid = 2249;
const RECORDARRAYOID: Oid = 2287;
const TYPTYPE_COMPOSITE: i8 = b'c' as i8;
const TYPTYPE_DOMAIN: i8 = b'd' as i8;
const TYPTYPE_PSEUDO: i8 = b'p' as i8;

/// The parallel arena's catalog arm over `specs`, or None where a worker could still need the catalog (the arena runs
/// serial, D1409b): a composite / record or domain type in an aggregate's closure (a row type's
/// tuple descriptor is a relcache open; a domain's constraints a catalog crawl), or a transfn / combinefn, or a compare /
/// equality function the images resolve, that is not an fmgr builtin (resolving it reads pg_proc). The closure: each
/// aggregate's argument types and transition type (pseudo-types carry no facts): every type's row; a typcache image for
/// the types whose functions read one (an array: its element's compare; an enum: its order; a range / multirange: its
/// subtype), with typcache's own closure (an array's element, a range's subtype, a multirange's range).
pub(crate) fn catalog(specs: &[Spec]) -> PgResult<Option<CatalogArm>> {
    let builtin = |f: Oid| f == 0 || fmgr_core::fmgr_isbuiltin(f).is_some();
    let (mut rows, mut images) = (Vec::new(), Vec::new()); // alloc: Exec
    // bounded("other: the Agg node's aggregate calls, one catalog capture each at bind: the query text bounds them, as PG's ExecInitAgg walks the same aggregates with no poll")
    for s in specs {
        if !AUDITED.contains(&s.shape.aggtransfn) || !AUDITED.contains(&s.shape.aggcombinefn) {
            return Ok(None);
        }
        // bounded("small-fixed: one type row per argument type of an aggregate and its transition type, at most FUNC_MAX_ARGS (100) + 1")
        for t in s.a.aggargtypes.iter().chain([s.a.aggtranstype]) {
            if t == RECORDOID || t == RECORDARRAYOID {
                return Ok(None); // an anonymous record: its descriptor is the typcache's record registry, not armed
            }
            let Some(row) = syscache_seams::capture_type_row(t)? else { return Ok(None) };
            // every other type: its row and its typcache entry (with typcache's closure: an array's element, a range's
            // subtype, a multirange's range), whatever its functions read
            match row.typtype {
                TYPTYPE_PSEUDO => continue,
                TYPTYPE_COMPOSITE | TYPTYPE_DOMAIN => return Ok(None),
                _ => gravity_fn::typesem::aggtrans::compare_closure(t, row.typtype, &mut images)?,
            }
            push_row(&mut rows, row);
        }
    }
    // bounded("other: one typcache image per type in the closure of the aggregates' argument and transition types (an array's element, a range's subtype), captured once at bind: the query text bounds them, as PG's ExecInitAgg resolves the same types with no poll")
    for i in &images {
        let Some(row) = syscache_seams::capture_type_row(i.type_id())? else { return Ok(None) };
        if matches!(row.typtype, TYPTYPE_COMPOSITE | TYPTYPE_DOMAIN) || !i.procs().all(builtin) {
            return Ok(None);
        }
        if !gravity_fn::typesem::collation::armed_by_default(i.rng_collation()) {
            // a range's subtype compare runs under its collation: resolved here like an input collation (V4R-452)
            gravity_fn::typesem::collation::Facts::of(i.rng_collation())?;
        }
        push_row(&mut rows, row);
    }
    Ok(Some(CatalogArm { rows: rows.into_boxed_slice(), images: images.into_boxed_slice() }))
}

fn push_row(rows: &mut Vec<syscache_seams::ArmedTypeRow>, row: syscache_seams::ArmedTypeRow) {
    if !rows.iter().any(|r| r.oid == row.oid) {
        rows.push(row); // alloc: Exec
    }
}

impl CatalogArm {
    /// V4R-453 (D1409): on a pool claim, this thread armed with the arena's catalog facts (each call: one thread-local
    /// read when armed; the claim guard's drop disarms, gravity_fn Armed). The leader keeps its own catcache.
    #[inline]
    pub(crate) fn arm(&self) {
        let c = self;
        // the arena owns the slices and outlives every claim of its statement (each claim's guard disarms first)
        let images: &'static [typcache::armed::TypcacheImage] = raw::statement_slice(&*c.images);
        if typcache::armed::armed_with(images) || !gravity_fn::typesem::collation::on_claim() {
            return;
        }
        let rows: &'static [syscache_seams::ArmedTypeRow] = raw::statement_slice(&*c.rows);
        syscache_seams::arm_type_rows(rows);
        typcache::armed::arm(images);
    }
}

#[cfg(test)]
mod audited_tests {
    use super::AUDITED;

    /// Each audited function, its oid, and the m6_d1409b sweep expression that runs it on the pool workers at W8 (where a
    /// worker has no catcache: a catalog read the arena did not capture is the named D1409 ERROR, a WRONG row).
    const SWEPT: &[(&str, u32, &str)] = &[
        ("any_value_transfn", 6292, "any_value(c)"),
        ("array_agg_array_combine", 6296, "array_agg(arr)"),
        ("array_agg_array_transfn", 4051, "array_agg(arr)"),
        ("array_agg_combine", 6293, "array_agg(i4)"),
        ("array_agg_transfn", 2333, "array_agg(i4)"),
        ("array_larger", 515, "max(arr)"),
        ("array_smaller", 516, "min(arr)"),
        ("bitand", 1673, "bit_and(bits)"),
        ("bitor", 1674, "bit_or(bits)"),
        ("bitxor", 1675, "bit_xor(bits)"),
        ("booland_statefunc", 2515, "bool_and(b)"),
        ("boolor_statefunc", 2516, "bool_or(b)"),
        ("bpchar_larger", 1063, "max(bp)"),
        ("bpchar_smaller", 1064, "min(bp)"),
        ("bytea_larger", 6393, "max(by)"),
        ("bytea_smaller", 6394, "min(by)"),
        ("bytea_string_agg_transfn", 3543, "string_agg(by,"),
        ("cash_pl", 894, "sum(m)"),
        ("cashlarger", 898, "max(m)"),
        ("cashsmaller", 899, "min(m)"),
        ("date_larger", 1138, "max(d)"),
        ("date_smaller", 1139, "min(d)"),
        ("enum_larger", 3525, "max(e)"),
        ("enum_smaller", 3524, "min(e)"),
        ("float4_accum", 208, "avg(f4)"),
        ("float4larger", 209, "max(f4)"),
        ("float4pl", 204, "sum(f4)"),
        ("float4smaller", 211, "min(f4)"),
        ("float8_accum", 222, "avg(f8)"),
        ("float8_combine", 276, "avg(f8)"),
        ("float8_regr_accum", 2806, "regr_sxx(f8, f4)"),
        ("float8_regr_combine", 3342, "regr_sxx(f8, f4)"),
        ("float8larger", 223, "max(f8)"),
        ("float8pl", 218, "sum(f8)"),
        ("float8smaller", 224, "min(f8)"),
        ("int2_accum", 1834, "var_pop(i2)"),
        ("int2_avg_accum", 1962, "avg(i2)"),
        ("int2_sum", 1840, "sum(i2)"),
        ("int2and", 1892, "bit_and(i2)"),
        ("int2larger", 770, "max(i2)"),
        ("int2or", 1893, "bit_or(i2)"),
        ("int2smaller", 771, "min(i2)"),
        ("int2xor", 1894, "bit_xor(i2)"),
        ("int4_accum", 1835, "var_samp(i4)"),
        ("int4_avg_accum", 1963, "avg(i4)"),
        ("int4_avg_combine", 3324, "avg(i4)"),
        ("int4_sum", 1841, "sum(i4)"),
        ("int4and", 1898, "bit_and(i4)"),
        ("int4larger", 768, "max(i4)"),
        ("int4or", 1899, "bit_or(i4)"),
        ("int4smaller", 769, "min(i4)"),
        ("int4xor", 1900, "bit_xor(i4)"),
        ("int8_accum", 1836, "variance(i8)"),
        ("int8_avg_accum", 2746, "avg(i8)"),
        ("int8_avg_combine", 2785, "avg(i8)"),
        ("int8and", 1904, "bit_and(i8)"),
        ("int8inc", 1219, "count(*)"),
        ("int8inc_any", 2804, "count(k)"),
        ("int8inc_float8_float8", 2805, "regr_count(f8, f4)"),
        ("int8larger", 1236, "max(i8)"),
        ("int8or", 1905, "bit_or(i8)"),
        ("int8pl", 463, "sum(i4)"),
        ("int8smaller", 1237, "min(i8)"),
        ("int8xor", 1906, "bit_xor(i8)"),
        ("interval_avg_accum", 1843, "avg(iv)"),
        ("interval_avg_combine", 3325, "avg(iv)"),
        ("interval_larger", 1198, "max(iv)"),
        ("interval_smaller", 1197, "min(iv)"),
        ("multirange_intersect_agg_transfn", 4388, "range_intersect_agg(mr)"),
        ("network_larger", 3562, "max(ip)"),
        ("network_smaller", 3563, "min(ip)"),
        ("numeric_accum", 1833, "stddev_pop(n)"),
        ("numeric_avg_accum", 2858, "avg(n)"),
        ("numeric_avg_combine", 3337, "avg(n)"),
        ("numeric_combine", 3341, "variance(i8)"),
        ("numeric_larger", 1767, "max(n)"),
        ("numeric_poly_combine", 3338, "var_samp(i4)"),
        ("numeric_smaller", 1766, "min(n)"),
        ("oidlarger", 1965, "max(o)"),
        ("oidsmaller", 1966, "min(o)"),
        ("pg_lsn_larger", 4187, "max(lsn)"),
        ("pg_lsn_smaller", 4188, "min(lsn)"),
        ("range_intersect_agg_transfn", 4401, "range_intersect_agg(r)"),
        ("record_larger", 6375, "max(ROW("),
        ("record_smaller", 6376, "min(ROW("),
        ("string_agg_combine", 6299, "string_agg(t,"),
        ("string_agg_transfn", 3535, "string_agg(t,"),
        ("text_larger", 458, "max(t)"),
        ("text_smaller", 459, "min(t)"),
        ("tidlarger", 2795, "max(ctid)"),
        ("tidsmaller", 2796, "min(ctid)"),
        ("time_larger", 1377, "max(tm)"),
        ("time_smaller", 1378, "min(tm)"),
        ("timestamp_larger", 2036, "max(ts)"),
        ("timestamp_smaller", 2035, "min(ts)"),
        ("timestamptz_larger", 1196, "max(tstz)"),
        ("timestamptz_smaller", 1195, "min(tstz)"),
        ("timetz_larger", 1379, "max(tz)"),
        ("timetz_smaller", 1380, "min(tz)"),
        ("xid8_larger", 5097, "max(x8)"),
        ("xid8_smaller", 5098, "min(x8)"),
    ];

    fn root() -> std::path::PathBuf {
        std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it for a test")).join("../../..")
    }

    /// V4R-453 (main's condition 2): AUDITED is exactly PG 18's transition and combine functions of the aggregates with a
    /// combinefn (not ordered-set), by name and oid from pg_aggregate.dat / pg_proc.dat; each one is run by the sweep e2e.
    /// A function added to the list without its reading, or a PG catalog that moves, fails here.
    #[test]
    fn audited_functions_are_pg_and_swept() {
        let cat = root().join("crates/postgres-18.6-reference/src/include/catalog");
        let agg = std::fs::read_to_string(cat.join("pg_aggregate.dat")).unwrap();
        let procs = std::fs::read_to_string(cat.join("pg_proc.dat")).unwrap();
        let field = |row: &str, k: &str| row.split(&format!("{k} => '")).nth(1).and_then(|r| r.split('\'').next()).map(str::to_owned); // alloc: Cold
        let mut want = std::collections::BTreeSet::new();
        for row in agg.split('{').skip(1) {
            let (Some(t), Some(c)) = (field(row, "aggtransfn"), field(row, "aggcombinefn")) else { continue };
            if c != "-" && field(row, "aggkind").is_none() {
                want.insert(t);
                want.insert(c);
            }
        }
        let mut oid_of = std::collections::BTreeMap::new();
        for row in procs.split('{').skip(1) {
            if let (Some(n), Some(o)) = (field(row, "proname"), field(row, "oid")) {
                oid_of.entry(n).or_insert(o.parse::<u32>().unwrap());
            }
        }
        let got: std::collections::BTreeSet<String> = SWEPT.iter().map(|&(n, ..)| n.to_owned()).collect();
        assert_eq!(got, want, "the audited names are PG's combinefn aggregates' functions");
        for &(n, o, _) in SWEPT {
            assert_eq!(oid_of.get(n), Some(&o), "{n}'s oid");
            assert!(AUDITED.contains(&o), "{n} ({o}) in AUDITED");
        }
        assert_eq!(AUDITED.len(), SWEPT.len(), "AUDITED has no function the sweep does not run");
        // The sweep e2e lives in the internal tree (internal/e2e, beside public/); a public-only checkout has
        // no e2e to cross-check, and the name/oid checks above already ran.
        let sweep_path = root().join("../internal/e2e/gravity/v4/m6_d1409b_worker_catalog_sweep.sql");
        if !sweep_path.exists() {
            eprintln!("SKIP the W8 cross-check: no internal tree at {}", sweep_path.display());
            return;
        }
        let sweep = std::fs::read_to_string(&sweep_path).unwrap();
        let w8 = sweep.split("==== W8").nth(1).expect("the sweep's W8 section");
        for &(n, _, expr) in SWEPT {
            assert!(w8.contains(expr), "{n}: the sweep's W8 section runs `{expr}`");
        }
    }
}

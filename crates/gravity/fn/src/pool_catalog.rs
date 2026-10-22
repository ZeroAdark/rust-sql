//! B2 (workersess): the catalog arm of a statement's hosted scalar sites on the pool threads. A pool thread has no
//! catcache (D1409); a builtin whose body reads a type's typcache entry (an array's element compare, a range's subtype,
//! an enum's order) runs on a claim only when the leader resolved every fact it reaches. The precedent is the parallel
//! hosted aggregate's arm (hosts ops/agg/trans/hosted_catalog.rs, V4R-453): the same images (typcache armed.rs) and
//! pg_type rows (syscache_seams), resolved on the leader, armed per claim. Here they are the STATEMENT's: lowerv4 records
//! the types its admitted sites reach (`closure_types`), exec builds them once per run on the leader
//! (`arm_leader_statement`), each section's carry clones them (`leader_statement`) and each claim arms them
//! (`StatementCatalog::arm`) beside the arena slot, so the two never evict each other.
//!
//! Admission makes a miss impossible by construction: a site is admitted (`pool_catalog`) only for an audited body
//! (`AUDITED`) over types whose whole closure images here (no composite, record or domain; every resolved procedure an
//! fmgr builtin). A miss anyway is counted (catcache_seams `POOL_CATALOG_MISSES`, exec's report line) and, under the
//! claim's strict flag, a debug-build assert.

use std::sync::Arc;

mod raw;

use mcx::{GrowIn, Mcx, PgVec};
use types_core::Oid;
use types_error::PgResult;

/// The bodies admitted on the arm: each reads only typcache entries (`image_closure`'s facts with `FLAGS`) and pg_type
/// rows of its argument and result types and their closure (an array's element, a range's subtype, a multirange's
/// range), and calls only the procedures those entries resolve. Output / send bodies (an element's output procedure:
/// pg_type's typoutput, not imaged) and the sorts stay serial.
pub const AUDITED: &[&str] = &[
    // arrays: the element's equality / ordering / hash (array_cmp, array_eq, hash_array's typcache entry in fn_extra)
    "array_eq", "array_ne", "array_lt", "array_gt", "array_le", "array_ge", "btarraycmp", "array_larger", "array_smaller",
    "hash_array", "hash_array_extended", "arrayoverlap", "arraycontains", "arraycontained", "width_bucket_array",
    // enums: the order (enum_cmp_internal's typcache enum data)
    "enum_lt", "enum_gt", "enum_le", "enum_ge", "enum_cmp",
    // ranges: the range's typcache entry (subtype compare, canonical, subdiff)
    "range_eq", "range_ne", "range_lt", "range_le", "range_ge", "range_gt", "range_cmp", "range_overlaps", "range_contains_elem",
    "range_contains", "elem_contained_by_range", "range_contained_by", "range_adjacent", "range_before", "range_after",
    "range_overleft", "range_overright", "range_union", "range_intersect", "range_minus", "range_merge", "hash_range",
    "hash_range_extended",
    // multiranges: the multirange's entry and its range's
    "multirange_eq", "multirange_ne", "multirange_lt", "multirange_le", "multirange_ge", "multirange_gt", "multirange_cmp",
    "multirange_lower_inc", "multirange_upper_inc", "multirange_lower_inf", "multirange_upper_inf", "multirange_contains_elem",
    "multirange_contains_range", "multirange_contains_multirange", "elem_contained_by_multirange", "range_contained_by_multirange",
    "multirange_contained_by_range", "multirange_contained_by_multirange", "range_overlaps_multirange", "multirange_overlaps_range",
    "multirange_overlaps_multirange", "range_adjacent_multirange", "multirange_adjacent_range", "multirange_adjacent_multirange",
    "range_before_multirange", "multirange_before_range", "multirange_before_multirange", "range_after_multirange",
    "multirange_after_range", "multirange_after_multirange", "range_overleft_multirange", "multirange_overleft_range",
    "multirange_overleft_multirange", "range_overright_multirange", "multirange_overright_range", "multirange_overright_multirange",
    "multirange_union", "multirange_intersect", "multirange_minus", "range_merge_from_multirange", "hash_multirange",
    "hash_multirange_extended", "multirange_constructor0", "multirange_constructor1", "multirange_constructor2",
    // the range constructors (tier C for their null semantics, which live in the body): the range's typcache entry and
    // its canonical function
    "range_constructor2", "range_constructor3",
    // encoding conversion (the second B2 entry): the pair the site's constant encoding name(s) and the database encoding
    // reach, resolved on the leader (`CONVERT`, mbutils `armed_conversion`), besides its text / bytea / name arguments
    "pg_convert_to", "pg_convert_from", "pg_convert",
];

/// The audited conversion bodies: admitted only when each encoding-name argument is a constant (`Reach::convs`).
pub const CONVERT: &[&str] = &["pg_convert_to", "pg_convert_from", "pg_convert"];

/// Stands for the database encoding in a `Reach::convs` pair (`convert_to`'s source, `convert_from`'s destination),
/// resolved when the run's catalog is built: a cached program outlives one database's encoding.
pub const DATABASE_ENCODING: i32 = -100;

/// What one admitted catalog site reaches: its argument and result types (a function has at most `SITE_TYPES - 1`
/// arguments) and, for a conversion body, its encoding pair(s). On the stack: it is a check and a copy source, never kept.
pub struct SiteReach {
    types: [Oid; SITE_TYPES],
    nt: usize,
    convs: [(i32, i32); 2],
    nc: usize,
}

const SITE_TYPES: usize = 129;

impl SiteReach {
    /// The reach of types `ts`; None when there are more than a function can have (the site stays serial).
    pub fn of_types(ts: impl IntoIterator<Item = Oid>) -> Option<SiteReach> {
        let mut r = SiteReach { types: [0; SITE_TYPES], nt: 0, convs: [(0, 0); 2], nc: 0 };
        for t in ts {
            *r.types.get_mut(r.nt)? = t;
            r.nt += 1;
        }
        Some(r)
    }

    /// Add a conversion pair (at most two per site).
    pub fn push_conv(&mut self, p: (i32, i32)) {
        if let Some(c) = self.convs.get_mut(self.nc) {
            *c = p;
            self.nc += 1;
        }
    }

    pub fn types(&self) -> &[Oid] {
        &self.types[..self.nt]
    }

    pub fn convs(&self) -> &[(i32, i32)] {
        &self.convs[..self.nc]
    }
}

/// What a statement's admitted catalog sites reach: the types to image and the encoding pairs to resolve. Its lists live
/// in the context it is made in: the planning context while the lowering builds it, then the owner's (or the template's)
/// product root (`clone_in`).
#[derive(Debug)]
pub struct Reach<'m> {
    pub types: PgVec<'m, Oid>,
    pub convs: PgVec<'m, (i32, i32)>,
}

impl<'m> Reach<'m> {
    pub fn new_in(m: Mcx<'m>) -> Reach<'m> {
        Reach { types: PgVec::new_in(m), convs: PgVec::new_in(m) }
    }

    pub fn is_empty(&self) -> bool {
        self.types.is_empty() && self.convs.is_empty()
    }

    /// Add `site`'s types and pairs not already here.
    pub fn add(&mut self, site: &SiteReach) {
        for &t in site.types() {
            if !self.types.contains(&t) {
                self.types.push_in(t);
            }
        }
        for &p in site.convs() {
            if !self.convs.contains(&p) {
                self.convs.push_in(p);
            }
        }
    }

    /// The same reach, its lists in `m` (the owner's or the template's root).
    pub fn clone_in<'b>(&self, m: Mcx<'b>) -> Reach<'b> {
        let mut r = Reach { types: mcx::reserved_in(self.types.len(), m), convs: mcx::reserved_in(self.convs.len(), m) };
        r.types.extend_from_slice_in(&self.types);
        r.convs.extend_from_slice_in(&self.convs);
        r
    }
}

/// Whether `oid` is an audited conversion body (its encoding names must be constants).
pub fn converts(oid: Oid) -> bool {
    crate::row_of(oid).is_some_and(|r| CONVERT.contains(&r.name))
}

/// The encoding of a constant `name` argument (its image at `addr`): pg_char_to_encoding, -1 for an unknown name (the
/// body raises before it converts).
pub fn name_encoding(addr: usize) -> i32 {
    mbutils::pg_char_to_encoding_bytes(raw::name_bytes(addr))
}

/// The pair a conversion site converts, from its constant encoding names' encodings (`DATABASE_ENCODING` for the side the
/// body takes from the database), or None for an unknown name (the body raises first: nothing to resolve).
pub fn convert_pair(funcid: Oid, encs: &[i32]) -> Option<(i32, i32)> {
    let name = crate::row_of(funcid)?.name;
    let (src, dest) = match (name, encs) {
        ("pg_convert_to", [d]) => (DATABASE_ENCODING, *d),
        ("pg_convert_from", [s]) => (*s, DATABASE_ENCODING),
        ("pg_convert", [s, d]) => (*s, *d),
        _ => return None,
    };
    (src != -1 && dest != -1).then_some((src, dest))
}

/// The facts an image carries: everything the audited bodies ask lookup_type_cache for (a request beyond an image's
/// ready bits is not armed: the catalog path, the counted miss).
pub const FLAGS: i32 = typcache::TYPECACHE_EQ_OPR
    | typcache::TYPECACHE_LT_OPR
    | typcache::TYPECACHE_GT_OPR
    | typcache::TYPECACHE_CMP_PROC
    | typcache::TYPECACHE_HASH_PROC
    | typcache::TYPECACHE_EQ_OPR_FINFO
    | typcache::TYPECACHE_CMP_PROC_FINFO
    | typcache::TYPECACHE_HASH_PROC_FINFO
    | typcache::TYPECACHE_HASH_EXTENDED_PROC
    | typcache::TYPECACHE_HASH_EXTENDED_PROC_FINFO;

const RECORDOID: Oid = 2249;
const RECORDARRAYOID: Oid = 2287;
const TYPTYPE_COMPOSITE: i8 = b'c' as i8;
const TYPTYPE_DOMAIN: i8 = b'd' as i8;
const TYPTYPE_PSEUDO: i8 = b'p' as i8;
const TYPTYPE_RANGE: i8 = b'r' as i8;
const TYPTYPE_MULTIRANGE: i8 = b'm' as i8;

/// Whether `oid` is an audited body (by its tier row's name: every overload of the name reads alike).
pub fn audited(oid: Oid) -> bool {
    crate::row_of(oid).is_some_and(|r| AUDITED.contains(&r.name))
}

/// The statement's catalog arm: the pg_type rows and typcache images of every type its admitted sites reach.
pub struct StatementCatalog {
    rows: Box<[syscache_seams::ArmedTypeRow]>, // alloc: Exec
    images: Box<[typcache::armed::TypcacheImage]>, // alloc: Exec
    conversions: Box<[mbutils::ArmedConversion]>, // alloc: Exec
}

impl StatementCatalog {
    /// The arm over `types` (a site's argument and result types), or None where a worker
    /// could still need the catalog: a record, composite or domain anywhere in the closure (a row type's descriptor is a
    /// relcache open, a domain's constraints a catalog crawl), or a resolved procedure that is not an fmgr builtin (its
    /// resolution would read pg_proc). On the leader (its catcache).
    /// And for each of `reach.convs`, the conversion it runs (its procedure an fmgr builtin or in-core C function: a
    /// claim calls its entry point directly), or None when it would resolve through pg_proc.
    pub fn build(reach: &Reach<'_>) -> PgResult<Option<StatementCatalog>> {
        Self::build_of(&reach.types, &reach.convs)
    }

    /// `build` over one site's reach (`SiteReach`): whether its closure can be imaged at all.
    pub fn build_site(site: &SiteReach) -> PgResult<Option<StatementCatalog>> {
        Self::build_of(site.types(), site.convs())
    }

    fn build_of(types: &[Oid], convs: &[(i32, i32)]) -> PgResult<Option<StatementCatalog>> {
        if !syscache_seams::type_rows_capturable() {
            return Ok(None); // no backend (a lowering test): nothing to image, the site stays serial
        }
        let builtin = |f: Oid| f == 0 || fmgr_core::fmgr_isbuiltin(f).is_some();
        let (mut rows, mut images) = (Vec::new(), Vec::new()); // alloc: Exec
        for &t in types {
            if t == 0 {
                continue;
            }
            if t == RECORDOID || t == RECORDARRAYOID {
                return Ok(None);
            }
            let Some(row) = syscache_seams::capture_type_row(t)? else { return Ok(None) };
            match row.typtype {
                // a polymorphic declared type (a RowCompareExpr binds its btree compare proc over the operator's
                // declared inputs: enum_cmp(anyenum, anyenum)) names no closure to image: the body would find the
                // real type through the catalog (enum_cmp_internal's ENUMOID lookup), so the site stays serial
                TYPTYPE_PSEUDO => return Ok(None),
                TYPTYPE_COMPOSITE | TYPTYPE_DOMAIN => return Ok(None),
                tt => {
                    let more = match tt {
                        TYPTYPE_RANGE => typcache::TYPECACHE_RANGE_INFO,
                        TYPTYPE_MULTIRANGE => typcache::TYPECACHE_MULTIRANGE_INFO,
                        _ => 0,
                    };
                    typcache::armed::image_closure(t, FLAGS | more, &mut images)?;
                }
            }
            push_row(&mut rows, row);
        }
        for i in &images {
            let Some(row) = syscache_seams::capture_type_row(i.type_id())? else { return Ok(None) };
            if matches!(row.typtype, TYPTYPE_COMPOSITE | TYPTYPE_DOMAIN) || i.type_id() == RECORDOID || !i.procs().all(builtin) {
                return Ok(None);
            }
            if !crate::typesem::collation::armed_by_default(i.rng_collation()) {
                // a range's subtype compare runs under its collation: resolved here, so the claim's StatementArm has it
                crate::typesem::collation::Facts::of(i.rng_collation())?;
            }
            push_row(&mut rows, row);
        }
        let mut conversions = Vec::new(); // alloc: Exec
        for &(s, d) in convs {
            let db = mbutils::GetDatabaseEncoding() as i32;
            let (s, d) = (if s == DATABASE_ENCODING { db } else { s }, if d == DATABASE_ENCODING { db } else { d });
            let Some(a) = mbutils::armed_conversion(s as _, d as _)? else { continue };
            if a.fn_addr.is_some() && !fmgr_core::fmgr_is_native(a.proc) {
                return Ok(None);
            }
            if !conversions.iter().any(|c: &mbutils::ArmedConversion| c.src == a.src && c.dest == a.dest) {
                conversions.push(a); // alloc: Exec
            }
        }
        Ok(Some(StatementCatalog { rows: rows.into_boxed_slice(), images: images.into_boxed_slice(), conversions: conversions.into_boxed_slice() }))
    }

    /// The types imaged (tests, the report).
    pub fn types(&self) -> impl Iterator<Item = Oid> + '_ {
        self.images.iter().map(|i| i.type_id())
    }

    /// Arm this pool thread's claim with the statement's facts until the guard drops (the carry holds `self` alive for
    /// the claim, `arm`'s `&'static` borrow never outlives it).
    pub fn arm(self: &Arc<Self>) -> CatalogArmed {
        // the guard holds an Arc of `self`; the slots are disarmed (emptied) in its drop, before that Arc goes
        let images: &'static [typcache::armed::TypcacheImage] = raw::claim_slice(&self.images[..]);
        let rows: &'static [syscache_seams::ArmedTypeRow] = raw::claim_slice(&self.rows[..]);
        syscache_seams::arm_statement_type_rows(rows);
        typcache::armed::arm_statement(images);
        let conversions = mbutils::arm_statement_conversions(Some(raw::claim_slice(&self.conversions[..])));
        let strict = catcache::set_pool_strict(true);
        CatalogArmed { _keep: Arc::clone(self), strict, conversions }
    }
}

impl core::fmt::Debug for StatementCatalog {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StatementCatalog").field("rows", &self.rows.len()).field("images", &self.images.len()).field("conversions", &self.conversions).finish()
    }
}

fn push_row(rows: &mut Vec<syscache_seams::ArmedTypeRow>, row: syscache_seams::ArmedTypeRow) {
    if !rows.iter().any(|r| r.oid == row.oid) {
        rows.push(row); // alloc: Exec
    }
}

/// A claim's statement catalog arm: its drop (the claim's end, an unwind too) empties both slots before the facts can go,
/// and puts the strict flag back. Plain thread-local stores and a map clear: no panic.
#[must_use = "the statement's catalog stays armed only while the guard lives"]
pub struct CatalogArmed {
    _keep: Arc<StatementCatalog>,
    strict: bool,
    conversions: Option<&'static [mbutils::ArmedConversion]>,
}

impl Drop for CatalogArmed {
    fn drop(&mut self) {
        catcache::set_pool_strict(self.strict);
        mbutils::arm_statement_conversions(self.conversions);
        typcache::armed::disarm_statement();
        syscache_seams::arm_statement_type_rows(&[]);
    }
}

/// The leader's current statement arm, for a section's carry (None: the statement has no admitted catalog site).
pub fn leader_statement() -> Option<Arc<StatementCatalog>> {
    crate::typesem::collation::LEADER_CATALOG.with(|l| l.borrow().clone())
}

/// For a run on the leader: `cat` is the statement's arm until the guard drops (a nested run's guard puts the outer
/// statement's back).
pub fn arm_leader_statement(cat: Option<Arc<StatementCatalog>>) -> LeaderArmed {
    LeaderArmed { prev: crate::typesem::collation::LEADER_CATALOG.with(|l| l.replace(cat)) }
}

#[must_use = "the statement's arm stays the leader's only while the guard lives"]
pub struct LeaderArmed {
    prev: Option<Arc<StatementCatalog>>,
}

impl Drop for LeaderArmed {
    fn drop(&mut self) {
        let prev = self.prev.take();
        let _ = crate::typesem::collation::LEADER_CATALOG.try_with(|l| l.replace(prev));
    }
}

#[cfg(test)]
mod tests {
    use super::{convert_pair, DATABASE_ENCODING, AUDITED, CONVERT};

    /// A conversion site's pair: the database encoding on the side its body takes it; an unknown name (the body raises
    /// first) or a wrong arity resolves nothing; every conversion body is audited.
    #[test]
    fn conversion_pairs() {
        assert_eq!(convert_pair(1717, &[8]), Some((DATABASE_ENCODING, 8)), "convert_to(text, 'LATIN1')");
        assert_eq!(convert_pair(1714, &[8]), Some((8, DATABASE_ENCODING)), "convert_from(bytea, 'LATIN1')");
        assert_eq!(convert_pair(1717, &[-1]), None, "an unknown name");
        assert_eq!(convert_pair(1717, &[8, 6]), None, "convert_to takes one name");
        assert_eq!(convert_pair(1039, &[8]), None, "not a conversion body");
        assert!(CONVERT.iter().all(|n| AUDITED.contains(n)));
    }

    /// Every audited name is a builtin's (a tier row), function-manager bound (a shim never needs the arm), and no name
    /// is listed twice.
    #[test]
    fn audited_names_are_fmgr_builtins() {
        for n in AUDITED {
            let rows: Vec<_> = crate::ROWS.iter().filter(|r| r.name == *n).collect();
            assert!(!rows.is_empty(), "{n}: no tier row");
            assert!(rows.iter().all(|r| r.status == crate::Status::Fmgr), "{n}: not function-manager bound");
            assert_eq!(AUDITED.iter().filter(|m| *m == n).count(), 1, "{n} listed twice");
        }
    }
}

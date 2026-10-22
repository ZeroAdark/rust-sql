//! `gravity_decompose::Facts` over the relcache (decompose.md §7's need; hosts.md "The relcache facts"): the relation's access method and attributes from `table_open`, the index's keys, options and
//! expressions from `index_open`; a relation opened with no lock (the plan holds it), an index under AccessShareLock taken only if granted at once and released after its facts are read (D582: an INSERT target's indexes were never opened by the planner, and PostgreSQL locks them only at the first inserted row) in the caller's memory
//! context. Every fact the capture script's fixture header carries is answered here — the two are held equal by `scripts/gravity-facts-check.sh` (`facts::render` on both).

use gravity_decompose::catalog::{expr_fp, nth_index_expr, AttLayout, IndexExt, IndexOpc, KeyBind, LeafFact, RelExt};
use gravity_decompose::effects::Parallel;
use gravity_decompose::ops::modifytable::trig_facts::TrigExt;
use gravity_decompose::catalog::typefacts::KeyColl;
use gravity_decompose::{Am, AttFact, ExprFp, Facts, IndexAm, IndexFact, InsteadOf, RelFact};
use datum::Datum;
use gravity_program::limits;
use mcx::{Mcx, TryCollectOptionIn};
use tableam_vocab::TableAm;
use types_core::catalog::{FirstNormalObjectId, ANYARRAYOID, RECORDOID};
use types_core::Oid;
use types_error::PgResult;
use types_storage::lock::{AccessShareLock, NoLock};
use types_tuple::ATTNULLABLE_VALID;

use crate::ops::scan::pgrc::granule_source::PartList;

pub struct RelcacheFacts<'m> {
    pub mcx: Mcx<'m>,
    /// D4854: the resident parts of each pgrc relation this lowering read ([`Self::resident`]).
    pinned: std::cell::RefCell<[Option<(Oid, std::sync::Arc<PartList>)>; PINNED]>,
}

/// D4854: the pgrc relations one lowering pins; a lowering that reads more reads the rest unpinned.
const PINNED: usize = 16;

impl Facts for RelcacheFacts<'_> {
    fn rel(&self, relid: u32) -> Option<RelFact> {
        let r = table::table_open(self.mcx, relid as Oid, NoLock).ok()?;
        let am = match TableAm::of(&r) {
            Some(TableAm::Heap) => Am::Heap,
            Some(TableAm::Pgrcolumnar) => Am::Pgrcolumnar,
            _ => Am::Other,
        };
        // D4469 road 2: no relation's order is a fact of its data (each pgrc part is sorted only inside itself); a scan
        // the plan relies on for its order is marked and merges its parts (decompose reads `SeqScan.pgrc_merge`)
        let mut f = RelFact::plain(relid, am, r.rd_rel.relkind as u8, r.rd_att.natts as i16);
        // PGRCSERVE G4: the twin lever sends a user heap table's scans down the rows road (ops/scan/am_rows.rs); the
        // catalogs keep their index roads. G5: so does the admission's road answer for a table of another access method
        let lever = am == Am::Heap && r.rd_id >= FirstNormalObjectId && matches!(r.rd_rel.relkind, types_rel::RELKIND_RELATION | types_rel::RELKIND_MATVIEW) && crate::ops::scan::am_rows::forced();
        if lever || am != Am::Heap && crate::ops::scan::am_rows::scoped_rows(relid) {
            f = f.on_rows_road();
        }
        let _ = r.close(NoLock);
        Some(f)
    }

    fn att(&self, relid: u32, attno: i16) -> Option<AttFact> {
        // a system column (ctid, xmin, cmin, xmax, cmax, tableoid): PG's fixed definition, the same for every relation (D2366)
        if attno < 0 {
            let a = catalog_heap::SystemAttributeDefinition(attno).ok()?;
            return Some(AttFact::plain(a.atttypid, a.atttypmod, a.attlen, a.attbyval, a.attnotnull));
        }
        let r = table::table_open(self.mcx, relid as Oid, NoLock).ok()?;
        // a dropped column has no facts (the corpus capture's contract: a write nulls its cell). D2838: never null only
        // under a VALID not-null constraint: PG 18 sets attnotnull for a NOT VALID one too, over rows that may hold NULLs;
        // the relcache's attnullability (from pg_constraint) tells them apart, as the planner's plancat.rs reads it
        let i = attno as usize - 1;
        let a = r.rd_att.attrs.get(i).filter(|a| !a.attisdropped).map(|a| (a.atttypid, a.atttypmod, a.attlen, a.attbyval, r.rd_att.compact_attr(i).attnullability == ATTNULLABLE_VALID));
        let _ = r.close(NoLock);
        a.and_then(|(t, m, len, byval, notnull)| based(AttFact::plain(t, m, len, byval, notnull)))
    }

    fn dropped_layout(&self, relid: u32, attno: i16) -> Option<AttLayout> {
        if attno < 1 {
            return None;
        }
        let r = table::table_open(self.mcx, relid as Oid, NoLock).ok()?;
        let a = r.rd_att.attrs.get(attno as usize - 1).filter(|a| a.attisdropped).and_then(|a| {
            let align = match a.attalign as u8 {
                b'c' => 1,
                b's' => 2,
                b'i' => 4,
                b'd' => 8,
                _ => return None,
            };
            Some(AttLayout { typlen: a.attlen, byval: a.attbyval, align })
        });
        let _ = r.close(NoLock);
        a
    }

    fn type_row(&self, typid: u32) -> Option<gravity_decompose::TypeRow> {
        let (typlen, byval, typtype, base) = gravity_fn::typesem::keysem::catalog::type_row(typid as Oid)?;
        Some(gravity_decompose::TypeRow { typlen, byval, typtype, base: base as u32 })
    }

    /// D3796: C `DomainConstraintRef`'s list (typcache load_domaintype_info: the NOT NULL and the planned CHECKs of every
    /// domain in the chain), held while `f` reads it (its Rc keeps the constraints' context when an invalidation the
    /// reader's catalog reads take rebuilds the entry). Each domain of the chain is read first: an ALTER DOMAIN sends a
    /// pg_type invalidation of the domain it alters, which a recording of these reads holds (C3, gravity_deps). False for
    /// a type that is no domain or a failed catalog read.
    fn domain_checks(&self, typid: u32, f: &mut dyn for<'n> FnMut(Option<types_nodes::Node<'n>>)) -> bool {
        let mut t = typid as Oid;
        loop {
            match syscache_seams::pg_type_domain_shape::call(t) {
                Ok(Some(s)) if s.typtype == lsyscache::TYPTYPE_DOMAIN => t = s.typbasetype,
                Ok(Some(_)) if t != typid as Oid => break,
                _ => return false,
            }
        }
        let Ok(r) = typcache::DomainConstraintRef::init(typid as Oid) else { return false };
        for c in r.constraints() {
            match (c.constrainttype, c.check_expr) {
                (typcache::DomConstraintType::NotNull, _) => f(None),
                (typcache::DomConstraintType::Check, Some(e)) => f(Some(e)),
                (typcache::DomConstraintType::Check, None) => return false,
            }
        }
        true
    }

    /// C `get_range_subtype` (InvalidOid for a type that is no range: none).
    fn range_subtype(&self, typid: u32) -> Option<u32> {
        lsyscache::get_range_subtype(typid as Oid).ok().filter(|&t| t != 0).map(|t| t as u32)
    }

    /// D693: C `lookup_rowtype_tupdesc_domain`'s descriptor (a domain read through to its composite base), copied into
    /// the caller's context; a type that is no composite answers none (the typcache's error is the refusal's cause).
    fn row_att(&self, typid: u32, attno: i16) -> Option<AttFact> {
        let base = lsyscache::typ::getBaseType(typid as Oid).ok()?;
        let d = typcache::lookup_rowtype_tupdesc_copy(self.mcx, base, -1).ok()?;
        let a = d.attrs.get((attno as usize).checked_sub(1)?).filter(|a| attno as i32 <= d.natts && !a.attisdropped)?;
        // D2838: a composite value's field may be NULL whatever its relation's NOT NULL constraints (PG checks them only
        // on the relation's own rows), so the row type's attribute is never "never null"
        based(AttFact::plain(a.atttypid, a.atttypmod, a.attlen, a.attbyval, false))
    }

    /// AGC-20: `row_att`'s descriptor, read once: every live column in attribute order (rowtypes.c's record_eq /
    /// btrecordcmp / hash_record skip a dropped one); false when the type is no composite (the typcache's error).
    fn row_cols(&self, typid: u32, f: &mut dyn FnMut(AttFact)) -> bool {
        let Some(d) = lsyscache::typ::getBaseType(typid as Oid).ok().and_then(|b| typcache::lookup_rowtype_tupdesc_copy(self.mcx, b, -1).ok()) else {
            return false;
        };
        for a in d.attrs.iter().take(d.natts.max(0) as usize).filter(|a| !a.attisdropped) {
            match based(AttFact::plain(a.atttypid, a.atttypmod, a.attlen, a.attbyval, false)) {
                Some(x) => f(x),
                None => return false,
            }
        }
        true
    }

    /// D1901 (V4R-508): the key type's canonical form as this session's catalog shows it (gravity_fn canon.rs).
    fn key_holds_multirange(&self, typid: u32) -> bool {
        gravity_fn::typesem::holds_multirange(typid as Oid)
    }

    /// D2163: typesem's `eq_form` of the operator over the key type (a failed catalog read answers `Other`: the refusal).
    fn key_eq(&self, opno: u32, typid: u32) -> gravity_decompose::catalog::KeyEq {
        use gravity_fn::typesem::EqForm;
        let form = gravity_fn::typesem::eq_form(self.mcx, opno as Oid, typid as Oid);
        let is = |f: EqForm| matches!(&form, Ok(x) if *x == f);
        gravity_decompose::catalog::keysem::image::key_eq_from(is(EqForm::Own), is(EqForm::Bytes), is(EqForm::User))
    }

    /// hj-0310: typesem's `op_user_key` of the operator over the key type (a failed catalog read answers false: the refusal).
    fn op_key_hash(&self, opno: u32, typid: u32, coll: u32) -> bool {
        matches!(gravity_fn::typesem::keysem::user::op_user_key(&gravity_fn::typesem::KeyReq::hash(opno as Oid, coll as Oid), typid as Oid), Ok(Ok(_)))
    }

    /// KeySem slice 4: the operator's image-equality sides, from the key home (`keysem::image::eq`).
    fn op_image_eq(&self, opno: u32) -> Option<(u32, u32)> {
        gravity_fn::typesem::keysem::image::eq(opno)
    }

    /// An anonymous record constant's row type: the record cache's descriptor of its blessed typmod.
    fn record_cols(&self, typmod: i32) -> Option<Vec<(u32, u32)>> {
        if typmod < 0 {
            return None;
        }
        let d = typcache::lookup_rowtype_tupdesc_copy(self.mcx, RECORDOID, typmod).ok()?;
        let cols = d.attrs.iter().take(d.natts.max(0) as usize);
        cols.map(|a| (!a.attisdropped).then_some((a.atttypid, a.attcollation))).collect()
    }

    /// D1073: C `get_expr_result_type`'s descriptor of a FROM call, as `ExecInitFunctionScan` takes it.
    fn call_col(&self, call: types_nodes::Node<'_>, attno: i16) -> Option<(u32, i32, u32)> {
        let d = funcapi::get_expr_result_type(self.mcx, Some(call)).ok()?.result_tuple_desc?;
        let a = d.attrs.get((attno as usize).checked_sub(1)?).filter(|a| attno as i32 <= d.natts && !a.attisdropped)?;
        Some((a.atttypid, a.atttypmod, a.attcollation))
    }

    /// D3374: `call_col`'s descriptor, its attribute `attno` a dropped one.
    fn call_col_dropped(&self, call: types_nodes::Node<'_>, attno: i16) -> bool {
        let Some(d) = funcapi::get_expr_result_type(self.mcx, Some(call)).ok().and_then(|r| r.result_tuple_desc) else { return false };
        (attno as usize).checked_sub(1).and_then(|i| d.attrs.get(i)).is_some_and(|a| attno as i32 <= d.natts && a.attisdropped)
    }

    fn att_range(&self, relid: u32, attno: i16) -> Option<(i64, i64)> {
        let r = table::table_open(self.mcx, relid as Oid, NoLock).ok()?;
        let x = if matches!(TableAm::of(&r), Some(TableAm::Pgrcolumnar)) { crate::ops::scan::pgrc::granule_source::zone_extrema(&r, attno as u32).ok().flatten() } else { None };
        let _ = r.close(NoLock);
        x
    }

    /// The zone extrema on pgrc; on heap the `pg_statistic` bounds (M3-L6J) — an estimate: rows written since the last ANALYZE may lie outside it, and only the packed front's guarded index reads it.
    fn att_estimate(&self, relid: u32, attno: i16) -> Option<(i64, i64)> {
        let r = table::table_open(self.mcx, relid as Oid, NoLock).ok()?;
        let x = match TableAm::of(&r) {
            Some(TableAm::Pgrcolumnar) => crate::ops::scan::pgrc::granule_source::zone_extrema(&r, attno as u32).ok().flatten(),
            Some(TableAm::Heap) => stats_bounds(self.mcx, relid as Oid, attno),
            _ => None,
        };
        let _ = r.close(NoLock);
        x
    }

    /// M6 D2: pgrc's resident parts' election of a jsonb path (`granule_source::shred_election`); none on heap.
    fn shred_election(&self, relid: u32, attno: i16, keys: &[&str]) -> Option<(u8, i32)> {
        let r = table::table_open(self.mcx, relid as Oid, NoLock).ok()?;
        let path = crate::ops::scan::pgrc::granule_source::shred_path(keys);
        let pgrc = matches!(TableAm::of(&r), Some(TableAm::Pgrcolumnar)) && attno > 0 && layout_read(relid);
        let x = if pgrc { self.resident(&r).and_then(|l| crate::ops::scan::pgrc::granule_source::shred_election(&l, attno as u32, &path)).ok().flatten() } else { None };
        let _ = r.close(NoLock);
        // D4854's witness (regress/isolation-overlay/specs/pgrc-d4854-election-pin.spec): a publish committed after one of a
        // lowering's election reads, before the next
        if pgrc {
            injection_point::injection_point("gravity-shred-election-read").ok()?;
        }
        x.map(|(k, s)| (crate::ops::scan::pgrc::granule_source::ShredLane::code_of(k), s))
    }

    /// SRC-30: pgrc's resident parts' dictionary seal of a jsonb text path (`granule_source::shred_dict`); false on heap.
    fn shred_dict(&self, relid: u32, attno: i16, keys: &[&str]) -> bool {
        let Ok(r) = table::table_open(self.mcx, relid as Oid, NoLock) else { return false };
        let path = crate::ops::scan::pgrc::granule_source::shred_path(keys);
        let x = matches!(TableAm::of(&r), Some(TableAm::Pgrcolumnar)) && attno > 0 && layout_read(relid) && self.resident(&r).and_then(|l| crate::ops::scan::pgrc::granule_source::shred_dict(&l, attno as u32, &path)).unwrap_or(false);
        let _ = r.close(NoLock);
        x
    }

    /// jsonops (3): `granule_source::shred_object_roots`; false on heap.
    fn shred_object_roots(&self, relid: u32, attno: i16, keys: &[&str]) -> bool {
        let Ok(r) = table::table_open(self.mcx, relid as Oid, NoLock) else { return false };
        let path = crate::ops::scan::pgrc::granule_source::shred_path(keys);
        let x = matches!(TableAm::of(&r), Some(TableAm::Pgrcolumnar)) && attno > 0 && layout_read(relid) && self.resident(&r).and_then(|l| crate::ops::scan::pgrc::granule_source::shred_object_roots(&l, attno as u32, &path)).unwrap_or(false);
        let _ = r.close(NoLock);
        x
    }

    /// sortpath: pgrc's declared `sorted_by` key holds jsonb path `keys` of column `attno`; false on heap.
    fn shred_sorted(&self, relid: u32, attno: i16, keys: &[&str]) -> bool {
        let Ok(r) = table::table_open(self.mcx, relid as Oid, NoLock) else { return false };
        let x = matches!(TableAm::of(&r), Some(TableAm::Pgrcolumnar))
            && attno > 0
            && pgrc_am::reloptions::sorted_by_keys(&r).is_some_and(|ks| ks.iter().any(|(a, p)| *a == attno as u32 && p.len() == keys.len() && p.iter().zip(keys).all(|(x, k)| x.as_slice() == k.as_bytes())));
        let _ = r.close(NoLock);
        x
    }

    /// pgrc's resident parts' rows (no snapshot: the latest generation); none on heap.
    fn rel_rows(&self, relid: u32) -> Option<u64> {
        let r = table::table_open(self.mcx, relid as Oid, NoLock).ok()?;
        let x = if matches!(TableAm::of(&r), Some(TableAm::Pgrcolumnar)) { self.resident(&r).ok().map(|l| l.rows()) } else { None };
        let _ = r.close(NoLock);
        x
    }

    /// PS1-21 / PS1-26 step 1b: a heap relation's rows bounded from above by its main fork's current pages (read now, as
    /// a scan would) x MaxHeapTuplesPerPage; none on any other AM. No statistic is read, so none can make it stale.
    fn rel_rows_bound(&self, relid: u32) -> Option<u64> {
        let r = table::table_open(self.mcx, relid as Oid, NoLock).ok()?;
        let x = matches!(TableAm::of(&r), Some(TableAm::Heap))
            .then(|| bufmgr_seams::relation_get_number_of_blocks_in_fork::call(&r, types_core::ForkNumber::MAIN_FORKNUM).ok())
            .flatten()
            .map(|pages| u64::from(pages).saturating_mul(types_storage::bufpage::MaxHeapTuplesPerPage as u64));
        let _ = r.close(NoLock);
        x
    }

    /// pgrc's footer: the column's dictionary entries over the committed parts when every part codes it by dictionary
    /// (hosts ops/agg `entries_bound`'s count); a column stored verbatim, whose code lane ranks each part's distinct
    /// values (granule_source `Dict::Ranked`), its distinct values: the footer's NDV, else ANALYZE's `stadistinct`
    /// (whole-table estimates; several parts can hold one value each, so they lean low and keep the table). None on heap,
    /// before a publish, for any other codec or without either count.
    fn att_entries(&self, relid: u32, attno: i16) -> Option<u64> {
        use pgrc_am::footer::{CODEC_DICT, CODEC_FIXED, CODEC_TEXT};
        let a = usize::try_from(attno).ok()?.checked_sub(1)?;
        let r = table::table_open(self.mcx, relid as Oid, NoLock).ok()?;
        let x = match TableAm::of(&r) {
            Some(TableAm::Pgrcolumnar) => match pgrc_am::footer::footer_codecs(&r).ok().flatten().map(|c| (c.codec.get(a).copied(), c.ncodes.get(a).copied())) {
                Some((Some(CODEC_DICT), Some(n))) if n > 0 => Some(n),
                Some((Some(CODEC_TEXT | CODEC_FIXED), _)) => pgrc_am::footer::footer_ndv(&r).ok().flatten().and_then(|v| v.get(a).copied()).filter(|&n| n > 0).or_else(|| {
                    let rows = self.resident(&r).ok().map(|l| l.rows())?;
                    stats_distinct(self.mcx, relid as Oid, attno, rows)
                }),
                _ => None,
            },
            _ => None,
        };
        let _ = r.close(NoLock);
        x
    }

    /// D3038: pgrc's footer: column `attno` is coded by dictionary in every committed part (codec `CODEC_DICT`), or in
    /// the parts that hold nearly all its rows (the qual-class vote `QUAL_CLASS_DICT`, 90 % of the rows: the codec and
    /// `ncodes` serve a mixed column as verbatim); false otherwise, before a publish, and on any other AM.
    fn att_dict(&self, relid: u32, attno: i16) -> bool {
        use pgrc_am::footer::{footer_codecs, footer_qual_classes, CODEC_DICT, QUAL_CLASS_DICT};
        let Some(a) = usize::try_from(attno).ok().and_then(|a| a.checked_sub(1)) else { return false };
        let Ok(r) = table::table_open(self.mcx, relid as Oid, NoLock) else { return false };
        let x = matches!(TableAm::of(&r), Some(TableAm::Pgrcolumnar))
            && (footer_codecs(&r).ok().flatten().is_some_and(|c| c.codec.get(a).copied() == Some(CODEC_DICT))
                || footer_qual_classes(&r).ok().flatten().is_some_and(|q| q.get(a).is_some_and(|&b| b & QUAL_CLASS_DICT != 0)));
        let _ = r.close(NoLock);
        x
    }

    /// pgrc's footer width of the column (the planner's convention), else ANALYZE's `stawidth`; none on any other AM.
    fn att_width(&self, relid: u32, attno: i16) -> Option<u32> {
        let a = usize::try_from(attno).ok()?.checked_sub(1)?;
        let r = table::table_open(self.mcx, relid as Oid, NoLock).ok()?;
        let x = match TableAm::of(&r) {
            Some(TableAm::Pgrcolumnar) => pgrc_am::footer::footer_avg_widths(&r).ok().flatten().and_then(|w| w.get(a).copied()).filter(|&w| w > 0).map(|w| w as u32).or_else(|| stats_width(self.mcx, relid as Oid, attno)),
            _ => None,
        };
        let _ = r.close(NoLock);
        x
    }

    /// pgrc's footer width less its 4-byte header (pgrc_write facts' `VARLENA_HEADER_BYTES`), else ANALYZE's `stawidth`
    /// less a short value's 1-byte header (analyze.c's `VARSIZE_ANY` of the stored datum); any other table's `stawidth` so.
    fn att_value_bytes(&self, relid: u32, attno: i16) -> Option<u32> {
        let a = usize::try_from(attno).ok()?.checked_sub(1)?;
        let r = table::table_open(self.mcx, relid as Oid, NoLock).ok()?;
        let footer = match TableAm::of(&r) {
            Some(TableAm::Pgrcolumnar) => pgrc_am::footer::footer_avg_widths(&r).ok().flatten().and_then(|w| w.get(a).copied()).filter(|&w| w > 0),
            _ => None,
        };
        let _ = r.close(NoLock);
        match footer {
            Some(w) => (w as u32).checked_sub(pgrc_write::facts::VARLENA_HEADER_BYTES as u32),
            None => stats_width(self.mcx, relid as Oid, attno)?.checked_sub(1),
        }
    }

    /// ANALYZE's `stadistinct` as the planner reads it, with the rows it scales by: pgrc's resident rows, heap's `reltuples`
    /// (none before the first ANALYZE or VACUUM counts them). None on any other AM.
    fn att_ndistinct(&self, relid: u32, attno: i16) -> Option<(u64, u64)> {
        let rows = self.rel_rows(relid)?;
        Some((stats_distinct(self.mcx, relid as Oid, attno, rows)?, rows))
    }

    /// D4497: a default-off decompose road, on when this session's `pgrust.gravity_road_off` names it `+<name>` and does
    /// not name it off plainly (residency.rs, as `scan_mergefree` reads its road).
    fn road_on(&self, name: &str) -> bool {
        use crate::ops::scan::pgrc::residency::{road_named_off, road_named_on};
        road_named_on(name) && !road_named_off(name)
    }

    /// D3784: the largest value's share (NULL a value) of `rows` rows read from column `attno`, from its ANALYZE
    /// statistics and the relation's rows (`stats_top_share`).
    fn att_top_share(&self, relid: u32, attno: i16, rows: u64) -> Option<f64> {
        let table = self.rel_rows(relid)?;
        stats_top_share(self.mcx, relid as Oid, attno, table, rows)
    }

    /// The server's widest parallel width (`inputs::parallel_width`).
    fn width(&self) -> Option<u16> {
        crate::inputs::parallel_width()
    }

    /// D3047: whether this session runs every statement on the leader (`inputs::serial_session`).
    fn serial(&self) -> Option<bool> {
        crate::inputs::serial_session()
    }

    /// D4568: road `scan.mergefree`, on by default (D4585, lowerv4 road_off.rs `DEFAULT_OFF` does not hold it): on unless
    /// this session's `pgrust.gravity_road_off` names it off plainly (residency.rs).
    fn scan_mergefree(&self) -> bool {
        !crate::ops::scan::pgrc::residency::road_named_off("scan.mergefree")
    }

    /// D4585: road `scan.mergefree.build`, default off: on when this session's `pgrust.gravity_road_off` names it
    /// `+scan.mergefree.build` and does not name it off plainly (lowerv4 road_off.rs `DEFAULT_OFF`).
    fn scan_mergefree_build(&self) -> bool {
        use crate::ops::scan::pgrc::residency::{road_named_off, road_named_on};
        road_named_on("scan.mergefree.build") && !road_named_off("scan.mergefree.build")
    }

    /// D4565 / D4567: an on-by-default decompose road (`gravity_decompose::ROADS_DEFAULT_ON`): on unless this session's
    /// `pgrust.gravity_road_off` names it off plainly (residency.rs, as `scan_mergefree` reads its road).
    fn default_on_road_on(&self, road: &str) -> bool {
        !crate::ops::scan::pgrc::residency::road_named_off(road)
    }

    fn index(&self, indexid: u32) -> Option<IndexFact> {
        let (r, _held) = self.open_index(indexid)?;
        let f = r.rd_index.as_ref().and_then(|i| {
            // the capture's `keys=` is the whole indkey (INCLUDE columns after the key columns), `opts=` the key columns' indoption; INDEX_MAX_KEYS bounds both (limits::KEYS)
            let natts = i.indkey.len();
            if natts > limits::KEYS {
                return None;
            }
            let mut keys = [0i16; limits::KEYS];
            let mut opts = [0u8; limits::KEYS];
            for (k, a) in i.indkey.iter().enumerate() {
                keys[k] = *a;
            }
            for (k, o) in r.rd_indoption.iter().take(natts).enumerate() {
                opts[k] = *o as u8;
            }
            Some(IndexFact { indexid, relid: i.indrelid, keys, opts, nkeys: natts as u8, unique: i.indisunique, immediate: i.indimmediate, btree: am_of(r.rd_rel.relam) == IndexAm::Btree, partial: i.has_indpred })
        });
        let _ = r.close(NoLock);
        f
    }

    fn index_am(&self, indexid: u32) -> IndexAm {
        let Some((r, _held)) = self.open_index(indexid) else { return IndexAm::Other };
        let am = am_of(r.rd_rel.relam);
        let _ = r.close(NoLock);
        am
    }

    fn index_insertable(&self, indexid: u32) -> bool {
        let Some((r, _held)) = self.open_index(indexid) else { return false };
        let ok = crate::ops::modifytable::write_index_am::insertable(types_relscan::IndexAmKind::from_relam(r.rd_rel.relam));
        let _ = r.close(NoLock);
        ok
    }

    fn indexes_of(&self, relid: u32, f: &mut dyn FnMut(IndexFact)) {
        let Ok(list) = relcache_seams::relation_get_index_list::call(self.mcx, relid as Oid) else { return };
        for &oid in list.iter() {
            if let Some(i) = self.index(oid) {
                f(i);
            }
        }
    }

    /// D582: every index held for the whole list (each lock only if free now, released after), or none read.
    fn indexes_all(&self, relid: u32, f: &mut dyn FnMut(IndexFact)) -> bool {
        let Ok(list) = relcache_seams::relation_get_index_list::call(self.mcx, relid as Oid) else { return false };
        // memsig E-pub t3: both lists in the facts' context (the planning statement's); the guards still drop after the facts,
        // and a `None` drops what was taken, in order, as `collect::<Option<Vec<_>>>` does
        let Some(_held) = list.iter().map(|&oid| Held::take(oid)).try_collect_in(self.mcx) else { return false };
        let Some(facts) = list.iter().map(|&oid| self.index(oid)).try_collect_in(self.mcx) else { return false };
        facts.into_iter().for_each(|i| f(i));
        true
    }

    fn index_expr(&self, indexid: u32, col: u8) -> Option<ExprFp> {
        let idx = self.index(indexid)?;
        let (r, _held) = self.open_index(indexid)?;
        // the stored pg_node_tree, as the capture writes it: the same text on both sides, the same reader
        let fp = r.rd_index.as_ref().and_then(|i| i.indexprs_src.as_ref()).and_then(|src| {
            let list = readfuncs::stringToNodeNullable(self.mcx, src.as_str()).ok()??;
            let list = list.as_list()?;
            let e = nth_index_expr(&idx, list, col)?;
            expr_fp(e, 1)
        });
        let _ = r.close(NoLock);
        fp
    }

    /// D2313: a btree key column's family against its declared and stored types' default families (typcache `btree_opf`); a
    /// failed default read keeps the family (another family's order is never taken for the default's).
    fn index_family(&self, indexid: u32, col: u8) -> u32 {
        let Some((r, _held)) = self.open_index(indexid) else { return 0 };
        let k = col as usize;
        let n = r.rd_index.as_ref().map_or(0, |i| i.indnkeyatts.max(0) as usize);
        let fam = if k < n && am_of(r.rd_rel.relam) == IndexAm::Btree && r.rd_opfamily.len() > k && r.rd_opcintype.len() > k {
            gravity_fn::typesem::keysem::catalog::index_family(r.rd_opfamily[k], r.rd_opcintype[k], r.rd_att.attrs.get(k).map(|a| a.atttypid))
        } else {
            0
        };
        let _ = r.close(NoLock);
        fam
    }

    /// D3752: a key column's collation from the index's relcache entry (`rd_indcollation`).
    fn index_coll(&self, indexid: u32, col: u8) -> Option<u32> {
        let (r, _held) = self.open_index(indexid)?;
        let n = r.rd_index.as_ref().map_or(0, |i| i.indnkeyatts.max(0) as usize);
        let c = r.rd_indcollation.get(col as usize).filter(|_| (col as usize) < n).copied();
        let _ = r.close(NoLock);
        c
    }

    /// D3752: `pg_sequence.seqcycle` through the syscache (`SEQRELID`); a failed read is none.
    fn seq_cycles(&self, seqid: u32) -> Option<bool> {
        if !syscache_seams::lookup_pg_sequence_form::is_installed() {
            return None;
        }
        syscache_seams::lookup_pg_sequence_form::call(seqid).ok().flatten().map(|f| f.seqcycle)
    }

    /// KeySem slice 3: a sort operator's order, one read for every order fact (`op_dir`, `op_family_dir`, `op_order`,
    /// `op_family`, `no_order_proc` read it): the key-semantics home's `keysem::sort_order` (`get_ordering_op_properties`,
    /// decision 49; the type cache's default btree family, D1346; the family's comparator and `FinishSortSupportFunction`'s
    /// raise test). A failed read answers none (the key refuses by its consumer's name, as an unknown operator does).
    fn key(&self, req: gravity_decompose::catalog::KeyReq) -> Option<gravity_decompose::catalog::KeyOrder> {
        let o = gravity_fn::typesem::keysem::sort_order(req.op as Oid).ok().flatten()?;
        Some(gravity_decompose::catalog::KeyOrder { desc: o.desc, family: o.family as u32, opcintype: o.opcintype as u32, default_family: o.default_family, cmp: o.cmp as u32, no_proc: o.no_proc })
    }

    /// OID-07: a btree equality of any family (`get_mergejoin_opfamilies`: strategy 3 in some btree opfamily).
    fn op_eq(&self, opno: u32) -> bool {
        gravity_fn::typesem::keysem::catalog::op_eq(self.mcx, opno as Oid)
    }

    /// DC-14: the pattern compiles as the first row's `~` / `~*` would compile it (D1236's check: the collation's, then
    /// the RE2 tier's or Spencer's), so the constant-pattern match raises on no row.
    fn regex_compiles(&self, pattern: &[u8], icase: bool, collation: u32) -> bool {
        gravity_fn::typesem::collation::regex_compiles(pattern, icase, collation as Oid)
    }

    /// Record-image order: the type's default btree family (the typcache's `btree_opf`; none when it has none).
    fn default_family(&self, typid: u32) -> Option<u32> {
        gravity_fn::typesem::keysem::catalog::default_family(typid as Oid).map(|f| f as u32)
    }

    /// KeySem (LANGUAGE internal; equivclass#53): `keysem::body`, the built-in a call of the function runs (a catalog read
    /// that fails answers none: the call stays the user function it is).
    fn builtin_body(&self, funcid: u32) -> Option<u32> {
        gravity_fn::typesem::keysem::body(self.mcx, funcid as Oid).ok().flatten().map(|b| b as u32)
    }

    /// D3082: `keysem::sql_inlined`, a user `LANGUAGE sql` function's body as the planner's `inline_function` would
    /// substitute it, judged (a failed read or inline answers false).
    fn sql_inlined(&self, funcid: u32, judge: &mut dyn FnMut(types_nodes::Node<'_>) -> bool) -> bool {
        gravity_fn::typesem::keysem::sql_inlined(self.mcx, funcid as Oid, judge).unwrap_or(false)
    }

    /// D3713: `keysem::sql_fmgr_body`, the body fmgr_sql runs past the SECURITY DEFINER / SET clause gates, judged.
    fn sql_fmgr_body(&self, funcid: u32, judge: &mut dyn FnMut(types_nodes::Node<'_>) -> bool) -> bool {
        gravity_fn::typesem::keysem::sql_fmgr_body(self.mcx, funcid as Oid, judge).unwrap_or(false)
    }

    /// KeySem: `keysem::op_calls`, the support functions a node naming the operator alone runs (none on a failed read).
    fn op_calls(&self, opno: u32) -> Option<gravity_decompose::catalog::OpCalls> {
        let c = gravity_fn::typesem::keysem::op_calls(opno as Oid).ok().flatten()?;
        let pair = |p: [Oid; 2]| [p[0] as u32, p[1] as u32];
        Some(gravity_decompose::catalog::OpCalls { code: c.code as u32, order: c.order.map(pair), hash: c.hash.map(pair) })
    }

    /// D247: the collation as `pg_locale` builds it (`pg_newlocale_from_collation`, the database default's from
    /// `pg_database`): bytes order values iff `collate_is_c`, equal bytes are equal values iff deterministic. No collation
    /// (0) is byte order; a collation the catalog cannot build answers what the oid alone says.
    fn coll(&self, oid: u32) -> KeyColl {
        key_coll(oid)
    }

    fn database_utf8(&self) -> bool {
        gravity_fn::typesem::collation::database_encoding() == 6 // PG_UTF8
    }

    /// D2063: key column `k` passes when the index AM's handler is a builtin one, the column's opcintype and stored type
    /// are initdb types, the opcintype is neither anyarray nor record (their support procs call the element's or field's
    /// DEFAULT opclass through the typcache, which a user may create), and its family is `family_initdb`'s. A lock not
    /// free now or a catalog read that fails answers `UNKNOWN`.
    fn index_opc(&self, indexid: u32) -> IndexOpc {
        if !(syscache_seams::lookup_pg_amproc_rows::is_installed() && syscache_seams::lookup_pg_amop_rows::is_installed()) {
            return IndexOpc::UNKNOWN;
        }
        let Some((r, _held)) = self.open_index(indexid) else { return IndexOpc::UNKNOWN };
        let n = r.rd_index.as_ref().map_or(0, |i| i.indnkeyatts.max(0) as usize);
        let ok = n > 0 && n <= limits::KEYS && am_of(r.rd_rel.relam) != IndexAm::Other && r.rd_opfamily.len() >= n && r.rd_opcintype.len() >= n && r.rd_att.attrs.len() >= n;
        let o = ok.then(|| {
            let mut builtin = 0u32;
            for k in 0..n {
                let t = r.rd_opcintype[k];
                let typed = initdb(t) && t != ANYARRAYOID && t != RECORDOID && initdb(r.rd_att.attrs[k].atttypid);
                if typed && family_initdb(self.mcx, r.rd_opfamily[k])? {
                    builtin |= 1 << k;
                }
            }
            Some(IndexOpc { builtin, nkeys: n as u8, known: true })
        });
        let _ = r.close(NoLock);
        o.flatten().unwrap_or(IndexOpc::UNKNOWN)
    }

    /// D1's index facts (decompose.md §2.12): the constraint forms and the key-column count from `rd_index`.
    fn index_ext(&self, indexid: u32) -> IndexExt {
        let Some((r, _held)) = self.open_index(indexid) else { return IndexExt::UNKNOWN };
        let e = r.rd_index.as_ref().map_or(IndexExt::UNKNOWN, |i| IndexExt { deferrable: !i.indimmediate, valid: i.indisvalid, ready: i.indisready, exclusion: i.indisexclusion, primary: i.indisprimary, nkeyatts: i.indnkeyatts as u8, known: true });
        let _ = r.close(NoLock);
        e
    }

    /// AGB-28: one open of the index; each key column's family (`rd_opfamily`, typesem's strategy read) and collation
    /// (`rd_indcollation`) against its binds, a nullable bind's operator strict (`op_strict`). A failed read is false.
    fn unique_bound(&self, indexid: u32, binds: &[KeyBind], partial_ok: bool) -> bool {
        let Some((r, _held)) = self.open_index(indexid) else { return false };
        let bound = |k: usize, b: &KeyBind| {
            b.col as usize == k
                && r.rd_opfamily.get(k).is_some_and(|&fam| gravity_fn::typesem::btree_equality_member(b.opno as Oid, fam))
                && r.rd_indcollation.get(k).is_some_and(|&c| c == 0 || c == b.collid as Oid)
                && (!b.nullable || lsyscache::op_strict(b.opno as Oid).unwrap_or(false))
        };
        let ok = r.rd_index.as_ref().is_some_and(|i| {
            let n = i.indnkeyatts.max(0) as usize;
            i.indisunique && i.indimmediate && i.indisvalid && (partial_ok || !i.has_indpred) && n > 0 && (0..n).all(|k| binds.iter().any(|b| bound(k, b)))
        });
        let _ = r.close(NoLock);
        ok
    }

    fn rel_ext(&self, relid: u32) -> RelExt {
        let Ok(r) = table::table_open(self.mcx, relid as Oid, NoLock) else { return RelExt::UNKNOWN };
        let e = RelExt { has_triggers: r.rd_hastriggers, has_rules: r.rd_hasrules, has_subclass: r.rd_rel.relhassubclass, is_partition: r.rd_rel.relispartition, known: true, temp: r.rd_rel.relpersistence as u8 == b't' };
        let _ = r.close(NoLock);
        e
    }

    /// The INSTEAD OF row triggers from the relcache's trigger descriptor (`RelationGetTriggerDesc`, built on
    /// demand while the relation is open; a relation without triggers has none), as `CheckValidResultRel` reads them.
    fn instead_of(&self, relid: u32) -> InsteadOf {
        let Ok(r) = table::table_open(self.mcx, relid as Oid, NoLock) else { return InsteadOf::UNKNOWN };
        let t = if r.rd_hastriggers { relcache_seams::relation_get_trigger_desc::call(relid as Oid).ok().flatten() } else { None };
        let f = t.map_or(InsteadOf::NONE, |d| InsteadOf { insert: d.trig_insert_instead_row, update: d.trig_update_instead_row, delete: d.trig_delete_instead_row, known: true });
        let _ = r.close(NoLock);
        f
    }

    fn trig_ext(&self, relid: u32) -> gravity_decompose::ops::modifytable::trig_facts::TrigExt {
        crate::ops::modifytable::trigger::trig_ext_of(relid as Oid)
    }

    /// The routed INSERT's leaves (`route_leaves.rs`): the partition tree walked as tuple routing walks it
    /// (`pg_inherits` in OID order, detach-pending partitions left out, sub-partitioned levels descended). PostgreSQL
    /// locks a leaf only when a row first routes to it, so admission never waits on one and never keeps one (D573: a
    /// wait here, behind a concurrent ATTACH's lock on the default partition, let the fallthrough build its routing
    /// after the ATTACH and skip the partition check PostgreSQL raises): each partition and each leaf index is locked
    /// AccessShareLock only if that is granted at once, read, and released. V4R-303: a lock not free now (a concurrent
    /// ATTACH's AccessExclusiveLock on the default partition) reads the entry with NoLock instead of answering the leaves
    /// unknown: the relcache builds an entry from the catalog snapshot, so it is the last committed state (the holder's
    /// uncommitted DDL is invisible; never half-applied), at worst stale by invalidations this backend has not taken yet,
    /// and the partition descriptor routing uses at bind is read the same way. Taking the leaf's RowExclusiveLock at its
    /// first row (the wait, as PostgreSQL's) processes those invalidations, and the host re-admits the leaf then from
    /// its current entry, as ExecInitPartitionInfo does (D833: never compared with the admission); ExecFindPartition's
    /// default arm rechecks the default's changed constraint (PostgreSQL's 23514). A partition dropped since the listing
    /// still answers `known = false`.
    fn partition_leaves(&self, root: u32, f: &mut dyn FnMut(LeafFact)) -> bool {
        let Ok(r) = table::table_open(self.mcx, root as Oid, NoLock) else { return false };
        let ok = self.leaves_under(&r, &r, f);
        let _ = r.close(NoLock);
        ok
    }

    /// D3585: both relations' descriptors as the relcache holds them (the plan locked both), compared attribute by
    /// attribute (`Lead::twin`).
    /// D3615 (a): `a`, a run's first relation (decompose asks it against each later member), is read once while what that
    /// read stands (`LEAD`, C3: its `Held`, replayed into a recording at each hit): an Append over N twins opened it N
    /// times, half of a fresh session's shared hit at 4000 partitions.
    fn heap_twin(&self, a: u32, b: u32) -> bool {
        let hit = LEAD.with(|c| {
            c.borrow().as_ref().is_some_and(|l| {
                let hit = l.relid == a && l.held.holds();
                if hit && gravity_deps::checking() {
                    l.held.check("hosts facts heap_twin (LEAD)", || lead_facts(self.mcx, a).is_some_and(|f| f.0 == l.plain && f.1 == l.natts && f.2 == l.relam && f.3 == l.atts));
                }
                hit
            })
        });
        if !hit {
            let (lead, held) = gravity_deps::Held::fill(|| lead_facts(self.mcx, a));
            let Some((plain, natts, relam, atts)) = lead else { return false };
            LEAD.with(|c| *c.borrow_mut() = Some(Lead { relid: a, held, plain, natts, relam, atts }));
        }
        let Ok(y) = table::table_open(self.mcx, b as Oid, NoLock) else { return false };
        let twin = LEAD.with(|c| c.borrow().as_ref().is_some_and(|l| l.twin(&y)));
        let _ = y.close(NoLock);
        twin
    }

    /// D3681 (`Facts::write_twin`): `heap_twin`, then both relations' write facts alike (`WriteLead::twin`); `a`'s read once
    /// while what that read stands (`WLEAD`, as `heap_twin`'s lead: its index shapes are recorded with it).
    fn write_twin(&self, a: u32, b: u32) -> bool {
        if !self.heap_twin(a, b) {
            return false;
        }
        let hit = WLEAD.with(|c| {
            c.borrow().as_ref().is_some_and(|l| {
                let hit = l.relid == a && l.held.holds();
                if hit && gravity_deps::checking() {
                    l.held.check("hosts facts write_twin (WLEAD)", || self.wlead_facts(a).is_some_and(|f| f.0 == l.form && f.1 == l.indexes));
                }
                hit
            })
        });
        if !hit {
            let (lead, held) = gravity_deps::Held::fill(|| self.wlead_facts(a));
            let Some((form, indexes)) = lead else { return false };
            WLEAD.with(|c| *c.borrow_mut() = Some(WriteLead { relid: a, held, form, indexes }));
        }
        let Ok(y) = table::table_open(self.mcx, b as Oid, NoLock) else { return false };
        let form = write_form(&y);
        let _ = y.close(NoLock);
        let Some(form) = form else { return false };
        if !WLEAD.with(|c| c.borrow().as_ref().is_some_and(|l| l.indexes.is_some() && l.form.as_ref() == Some(&form))) {
            return false;
        }
        let Some(indexes) = self.index_shapes(b) else { return false };
        WLEAD.with(|c| c.borrow().as_ref().is_some_and(|l| l.indexes.as_ref() == Some(&indexes)))
    }

    /// `pg_attribute.attgenerated` from the tuple descriptor (0 = none; a dropped column has none).
    fn att_generated(&self, relid: u32, attno: i16) -> u8 {
        let Ok(r) = table::table_open(self.mcx, relid as Oid, NoLock) else { return 0 };
        let g = r.rd_att.attrs.get(attno as usize - 1).filter(|a| !a.attisdropped).map_or(0, |a| a.attgenerated as u8);
        let _ = r.close(NoLock);
        g
    }

    /// D1071: the type's pg_type input / output functions (`getTypeInputInfo` / `getTypeOutputInfo`, a user type's and a
    /// domain's included); a builtin's from the builtin table first. None when the catalog has no usable row (a shell type,
    /// a type without I/O functions), which every fact reads as volatile.
    fn type_io(&self, typid: u32) -> Option<(u32, u32)> {
        if let Some(io) = gravity_decompose::effects::builtin_type_io(typid) {
            return Some(io);
        }
        let (input, _) = lsyscache::typ::getTypeInputInfo(typid as Oid).ok()?;
        let (output, _) = lsyscache::typ::getTypeOutputInfo(typid as Oid).ok()?;
        Some((input as u32, output as u32))
    }

    /// D1214 (V4R-393): the relation's PRIMARY KEY columns from pg_constraint (contype 'p', conkey), as
    /// `check_functional_grouping` reads them (`get_primary_key_attnos`, deferrable_ok false: a deferrable key is none).
    fn primary_key(&self, relid: u32) -> Option<([i16; limits::KEYS], u8)> {
        if !syscache_seams::pg_constraint_primary_key_attnos::is_installed() {
            return None;
        }
        let (attnos, _) = syscache_seams::pg_constraint_primary_key_attnos::call(self.mcx, relid as Oid, false).ok().flatten()?;
        if attnos.is_empty() || attnos.len() > limits::KEYS || attnos.iter().any(|&a| a <= 0) {
            return None;
        }
        let mut keys = [0i16; limits::KEYS];
        keys[..attnos.len()].copy_from_slice(&attnos);
        Some((keys, attnos.len() as u8))
    }

    /// A user aggregate's pg_aggregate support functions (`SearchSysCache1(AGGFNOID)`); none without a row (or without the
    /// syscache), which a user aggregate's call reads as VOLATILE.
    fn agg_support(&self, aggfnoid: u32) -> Option<gravity_decompose::catalog::AggSupport> {
        if !syscache_seams::lookup_pg_aggregate_shape::is_installed() {
            return None;
        }
        let a = syscache_seams::lookup_pg_aggregate_shape::call(aggfnoid as Oid).ok().flatten()?;
        Some(gravity_decompose::catalog::AggSupport {
            trans: a.aggtransfn as u32,
            final_: a.aggfinalfn as u32,
            combine: a.aggcombinefn as u32,
            serial: a.aggserialfn as u32,
            deserial: a.aggdeserialfn as u32,
            mtrans: a.aggmtransfn as u32,
            minv: a.aggminvtransfn as u32,
            mfinal: a.aggmfinalfn as u32,
        })
    }

    /// D1061: the function's `pg_proc` row (a user function's too, ALTER FUNCTION's changes included); none when the
    /// catalog has no row, which every fact reads as volatile.
    fn func(&self, funcid: u32) -> Option<gravity_decompose::effects::FuncProps> {
        if !syscache_seams::lookup_pg_proc_shape::is_installed() {
            return gravity_decompose::effects::builtin_func(funcid);
        }
        // D3615 (a): the last few functions asked, each while what its read stands (C3: its `Held`, replayed into a
        // recording at each hit): an Append over N partitions asks each arm's qual's function again (4000 syscache lookups
        // of int4gt in a fresh session's shared hit)
        let hit = FUNCS.with(|c| {
            let v = c.borrow();
            let (_, f, held) = v.iter().find(|e| e.0 == funcid && e.2.as_ref().is_some_and(gravity_deps::Held::holds))?;
            if let Some(held) = held.as_ref().filter(|_| gravity_deps::checking()) {
                held.check("hosts facts func (FUNCS)", || func_props(funcid) == *f);
            }
            Some(*f)
        });
        if let Some(f) = hit {
            return f;
        }
        let (f, held) = gravity_deps::Held::fill(|| func_props(funcid));
        FUNCS.with(|c| {
            let mut v = c.borrow_mut();
            v.rotate_right(1);
            v[0] = (funcid, f, Some(held));
        });
        f
    }

    /// D1018 (create_function_sql#92): the function's pg_proc row as text, read now (language first: only a LANGUAGE sql
    /// row reads its namespace, name and body), handed to `f`.
    fn proc_text(&self, funcid: u32, f: &mut dyn FnMut(&[u8], &[u8], u32, &[u8]) -> bool) -> bool {
        if !(syscache_seams::lookup_pg_proc_shape::is_installed() && syscache_seams::pg_namespace_nspname::is_installed() && syscache_seams::pg_proc_proname::is_installed() && syscache_seams::lookup_pg_proc_prosrc::is_installed()) {
            return false;
        }
        let Some(p) = syscache_seams::lookup_pg_proc_shape::call(funcid as Oid).ok().flatten().filter(|p| p.prolang as u32 == types_core::quiet::SQL_LANGUAGE) else { return false };
        let nsp = syscache_seams::pg_namespace_nspname::call(p.pronamespace).ok().flatten();
        let name = syscache_seams::pg_proc_proname::call(funcid as Oid).ok().flatten();
        let src = syscache_seams::lookup_pg_proc_prosrc::call(self.mcx, funcid as Oid).ok().flatten();
        matches!((nsp, name, src), (Some(n), Some(m), Some(b)) if f(n.name_str(), m.name_str(), p.prolang as u32, b.as_bytes()))
    }

    /// D2036: the hash functions PostgreSQL's hash join calls on each input of `opno` (typesem's `hash_procs`: the procs
    /// `get_op_hash_functions` resolves, as ExecInitHashJoin does); none without hash support, under an indeterminate
    /// collation, or without a readable catalog.
    fn op_hash_functions(&self, opno: u32, coll: u32) -> Option<(u32, u32)> {
        let (l, r) = gravity_fn::typesem::hash_procs(self.mcx, opno as Oid, coll as Oid).ok()?.ok()?;
        Some((l as u32, r as u32))
    }
}


/// A plain permanent heap table (`Lead::twin`'s relations).
fn plain_heap(r: &types_rel::Relation<'_>) -> bool {
    r.rd_rel.relkind as u8 == b'r' && r.rd_rel.relpersistence as u8 == types_core::catalog::RELPERSISTENCE_PERMANENT && TableAm::of(r) == Some(TableAm::Heap)
}

/// Attribute `i`'s facts `Lead::twin` compares: whether it has a missing value, and (dropped, type, typmod, length, by
/// value, alignment, storage, collation, array dimensions, not null, generated, nullability).
#[derive(Clone, Copy, PartialEq)]
struct AttTwin {
    missing: bool,
    layout: (bool, Oid, i32, i16, bool, i8, i8, Oid, i16, bool, i8, i8),
}

fn att_layout(r: &types_rel::Relation<'_>, i: usize) -> AttTwin {
    let p = &r.rd_att.attrs[i];
    AttTwin { missing: p.atthasmissing, layout: (p.attisdropped, p.atttypid, p.atttypmod, p.attlen, p.attbyval, p.attalign, p.attstorage, p.attcollation, p.attndims, p.attnotnull, p.attgenerated, r.rd_att.compact_attr(i).attnullability) }
}

/// `Lead`'s facts of relation `a`, read from its relcache entry (plain heap, natts, relam, every attribute's layout).
fn lead_facts(mcx: Mcx<'_>, a: u32) -> Option<(bool, i32, Oid, Vec<AttTwin>)> {
    let x = table::table_open(mcx, a as Oid, NoLock).ok()?;
    let facts = (plain_heap(&x), x.rd_att.natts, x.rd_rel.relam, (0..x.rd_att.natts.max(0) as usize).map(|i| att_layout(&x, i)).collect()); // alloc: Program
    let _ = x.close(NoLock);
    Some(facts)
}

/// D3615 (a): a run's first relation's `Lead::twin` facts, read once while that read stands (`Facts::heap_twin`).
struct Lead {
    relid: u32,
    held: gravity_deps::Held,
    plain: bool,
    natts: i32,
    relam: Oid,
    atts: Vec<AttTwin>,
}

impl Lead {
    /// D3585 (`Facts::heap_twin`): the lead and `y` are plain permanent heap tables of one physical layout: every attribute's
    /// type, typmod, length, by-value flag, alignment, storage, collation, array dimensions, nullability and generation the
    /// same, a dropped one where the other's is dropped, and no attribute with a missing value (a heap range recipe deforms
    /// by one descriptor and fills one set of missing values; a lowering reads one relation's attribute facts).
    fn twin(&self, y: &types_rel::Relation<'_>) -> bool {
        self.plain && plain_heap(y) && self.natts == y.rd_att.natts && self.relam == y.rd_rel.relam && self.atts.iter().enumerate().all(|(i, p)| {
            let q = att_layout(y, i);
            !p.missing && !q.missing && p.layout == q.layout
        })
    }
}

/// D3681: a relation's write facts `write_twin` compares: row security, CHECK constraints (name, expression, enforcement,
/// validity, inheritance; `relchecks`) and column defaults (attno, expression), both lists in the relcache's order.
#[derive(PartialEq)]
struct WriteForm {
    rowsecurity: bool,
    relchecks: i16,
    checks: Vec<(Option<String>, Option<String>, bool, bool, bool)>,
    defaults: Vec<(i16, Option<String>)>,
}

/// `WriteForm` of `r`; `None` for a relation with triggers or rules (each fires or rewrites per relation: never a set's).
fn write_form(r: &types_rel::Relation<'_>) -> Option<WriteForm> {
    if r.rd_hastriggers || r.rd_hasrules {
        return None;
    }
    let text = |s: &Option<mcx::PgString<'_>>| s.as_ref().map(|s| s.as_str().to_owned()); // alloc: Program
    let (relchecks, checks, defaults) = match r.rd_att.constr.as_ref() {
        None => (0, Vec::new(), Vec::new()), // alloc: Program
        Some(c) => (
            c.relchecks,
            c.check.iter().map(|k| (text(&k.ccname), text(&k.ccbin), k.ccenforced, k.ccvalid, k.ccnoinherit)).collect(), // alloc: Program
            c.defval.iter().map(|d| (d.adnum, text(&d.adbin))).collect(), // alloc: Program
        ),
    };
    Some(WriteForm { rowsecurity: r.rd_rel.relrowsecurity, relchecks, checks, defaults })
}

/// D3681: one index as `write_twin` compares it: access method; (natts, key natts, unique, nulls not distinct, primary,
/// exclusion, immediate, valid, ready); columns; operator families; collations; options; expressions and predicate text.
#[derive(PartialEq)]
struct IndexShape {
    am: Oid,
    form: (i16, i16, bool, bool, bool, bool, bool, bool, bool),
    keys: Vec<i16>,
    families: Vec<Oid>,
    collations: Vec<Oid>,
    options: Vec<i16>,
    exprs: Option<String>,
    pred: Option<String>,
}

/// D3681: a write run's first relation's `write_twin` facts, read once while that read stands (`Facts::write_twin`).
struct WriteLead {
    relid: u32,
    held: gravity_deps::Held,
    form: Option<WriteForm>,
    indexes: Option<Vec<IndexShape>>,
}

leader_tls::leader_tls! {
    thread_local! {
        /// The last `heap_twin` lead (D3615 (a)).
        static LEAD: std::cell::RefCell<Option<Lead>> = const { std::cell::RefCell::new(None) };
        /// The last `write_twin` lead (D3681).
        static WLEAD: std::cell::RefCell<Option<WriteLead>> = const { std::cell::RefCell::new(None) };
    }
}

thread_local! {
    /// D3615 (a): `RelcacheFacts::func`'s last answers, newest first: (function, answer, what its read held); function 0 = none.
    #[allow(clippy::type_complexity)]
    static FUNCS: std::cell::RefCell<[(u32, Option<gravity_decompose::effects::FuncProps>, Option<gravity_deps::Held>); 4]> = const { std::cell::RefCell::new([const { (0, None, None) }; 4]) };
}

/// `Facts::func` read from pg_proc.
fn func_props(funcid: u32) -> Option<gravity_decompose::effects::FuncProps> {
    use gravity_decompose::effects::{FuncProps, Volatility};
    let p = syscache_seams::lookup_pg_proc_shape::call(funcid as Oid).ok().flatten()?;
    let volatility = match p.provolatile as u8 {
        b'i' => Volatility::Immutable,
        b's' => Volatility::Stable,
        _ => Volatility::Volatile,
    };
    // D1035: proparallel as stored; effects (`FuncProps::pool`) takes a user function's call as Unsafe
    let parallel = parallel_of(p.proparallel);
    Some(FuncProps { volatility, strict: p.proisstrict, leakproof: p.proleakproof, parallel })
}

/// D2700: `f` with its base type and typmod (C `getBaseTypeAndTypmod`): a domain's base and the domain's typmod, so the
/// text and packed-numeric keys read what the column stores; a failed catalog read keeps the column's own (the slower
/// path, never a wrong key).
fn based(mut f: AttFact) -> Option<AttFact> {
    let mut m = f.typmod;
    if let Ok(b) = lsyscache::typ::getBaseTypeAndTypmod(f.typid as Oid, &mut m) {
        (f.base, f.base_typmod) = (b as u32, m);
    }
    Some(f)
}

fn initdb(oid: Oid) -> bool {
    oid < FirstNormalObjectId
}

/// D2063: operator family `fam` is an initdb one, and so is every member a scan keyed by initdb operators over initdb types
/// can reach: each pg_amproc row over two initdb types has an initdb proc, and one such row exists; each pg_amop row over
/// two initdb types has an initdb operator and sort family, and an initdb operator is a member over initdb types only. A
/// member over a user type (regress equivclass.sql's int8alias rows in integer_ops) is reached only by a key of that type,
/// through a user operator. Read as the AM validators read them (SearchSysCacheList1 AMPROCNUM / AMOPSTRATEGY); `None`
/// when a read fails.
fn family_initdb(mcx: Mcx<'_>, fam: Oid) -> Option<bool> {
    if fam == 0 || !initdb(fam) {
        return Some(false);
    }
    let reach = |l: Oid, r: Oid| initdb(l) && initdb(r);
    let (procs, _) = syscache_seams::lookup_pg_amproc_rows::call(mcx, fam).ok()?;
    let (ops, _) = syscache_seams::lookup_pg_amop_rows::call(mcx, fam).ok()?;
    let procs_ok = procs.iter().any(|p| reach(p.amproclefttype, p.amprocrighttype)) && procs.iter().all(|p| !reach(p.amproclefttype, p.amprocrighttype) || initdb(p.amproc));
    let ops_ok = ops.iter().all(|o| if reach(o.amoplefttype, o.amoprighttype) { initdb(o.amopopr) && initdb(o.amopsortfamily) } else { !initdb(o.amopopr) });
    Some(procs_ok && ops_ok)
}

/// An index AM by its handler's routine, never its oid (D429): `CREATE ACCESS METHOD gist2 TYPE INDEX HANDLER gisthandler`
/// is gist. The relcache resolved the handler when the index opened, so this cannot fail; an extension's handler is Other.
fn am_of(relam: Oid) -> IndexAm {
    use types_relscan::IndexAmKind as K;
    match K::from_relam(relam) {
        K::Btree => IndexAm::Btree,
        K::Hash => IndexAm::Hash,
        K::Gist => IndexAm::Gist,
        K::Spgist => IndexAm::Spgist,
        K::Gin => IndexAm::Gin,
        K::Brin => IndexAm::Brin,
        _ => IndexAm::Other,
    }
}

/// `pg_statistic.stakind` values (pg_statistic.h): the most-common values and the histogram bounds.
const STAT_MCV: i16 = 1;
const STAT_HISTOGRAM: i16 = 2;

/// The extrema of the column's MCV list and histogram over an int-word type (bool, int2, int4, int8: the key words the front indexes); none without statistics or over another type.
fn stats_bounds(mcx: Mcx<'_>, relid: Oid, attno: i16) -> Option<(i64, i64)> {
    let b = syscache_seams::lookup_pg_statistic_bundle::call(mcx, relid, attno, false).ok().flatten()?;
    let mut r: Option<(i64, i64)> = None;
    for s in b.slots.iter().filter(|s| matches!(s.kind, STAT_MCV | STAT_HISTOGRAM)) {
        let word: fn(Datum) -> i64 = match s.valuetype().ok()? {
            16 => |d| d.as_bool() as i64,
            21 => |d| d.as_i16() as i64,
            23 => |d| d.as_i32() as i64,
            20 => |d| d.as_i64(),
            _ => return None,
        };
        for &v in s.values().ok()? {
            let v = word(v);
            r = Some(r.map_or((v, v), |(lo, hi)| (lo.min(v), hi.max(v))));
        }
    }
    r
}

/// ANALYZE's distinct values of `attno` over `rows` rows (`stadistinct`: a count, or minus a fraction of the rows); none
/// unanalyzed or unknown (0).
/// ANALYZE's average width of column `attno` (`stawidth`), none without statistics.
fn stats_width(mcx: Mcx<'_>, relid: Oid, attno: i16) -> Option<u32> {
    if !syscache_seams::lookup_pg_statistic_bundle::is_installed() {
        return None;
    }
    let w = syscache_seams::lookup_pg_statistic_bundle::call(mcx, relid, attno, false).ok().flatten()?.stawidth;
    u32::try_from(w).ok().filter(|&w| w > 0)
}

fn stats_distinct(mcx: Mcx<'_>, relid: Oid, attno: i16, rows: u64) -> Option<u64> {
    if !syscache_seams::lookup_pg_statistic_bundle::is_installed() {
        return None;
    }
    let d = syscache_seams::lookup_pg_statistic_bundle::call(mcx, relid, attno, false).ok().flatten()?.stadistinct as f64;
    match d {
        d if d > 0.0 => Some(d as u64),
        d if d < 0.0 => Some((-d * rows as f64) as u64),
        _ => None,
    }
}

/// pg_statistic.h `STATISTIC_KIND_MCV`: the slot of the most common values and their frequencies.
const STATISTIC_KIND_MCV: i16 = 1;

/// Column `attno`'s largest value share of `rows` rows read from its relation of `table` rows (`att_top_share`; 0 rows:
/// all of them), NULL a value as a group key reads it. ANALYZE's statistics give each value's share of the relation: the
/// MCV slot's frequencies (most common first), the null fraction, and the mean of the values past the list,
/// `(1 - nullfrac - sum) / (ndistinct - listed)`. Rows a filter left are read two ways, and the smaller share is the
/// estimate: a filter independent of the column keeps each value's share (the largest share), and a filter on the column
/// keeps or drops a value whole, so a value with more rows than were read was dropped (the planner's estimate already took
/// it out: `c <> ''` over its most common value) and the largest value that fits holds `f table / rows` of them. None
/// without statistics, or with no MCV list and no distinct count.
fn stats_top_share(mcx: Mcx<'_>, relid: Oid, attno: i16, table: u64, rows: u64) -> Option<f64> {
    if !syscache_seams::lookup_pg_statistic_bundle::is_installed() {
        return None;
    }
    let b = syscache_seams::lookup_pg_statistic_bundle::call(mcx, relid, attno, false).ok().flatten()?;
    let mcv = match b.slots.iter().find(|s| s.kind == STATISTIC_KIND_MCV) {
        Some(s) => s.numbers().ok()?,
        None => &[],
    };
    top_share(b.stanullfrac, mcv, b.stadistinct, table, rows)
}

/// `stats_top_share` over the statistics' numbers: `stanullfrac`, the MCV frequencies, `stadistinct`.
fn top_share(nullfrac: f32, mcv: &[f32], stadistinct: f32, table: u64, rows: u64) -> Option<f64> {
    let (t, nulls) = (table as f64, (nullfrac as f64).clamp(0.0, 1.0));
    let distinct = match stadistinct as f64 {
        d if d > 0.0 => d,
        d if d < 0.0 => -d * t,
        _ => 0.0,
    };
    if mcv.is_empty() && distinct == 0.0 {
        return None;
    }
    let listed: f64 = mcv.iter().map(|&f| f as f64).sum();
    let rest = if distinct > mcv.len() as f64 { (1.0 - nulls - listed).max(0.0) / (distinct - mcv.len() as f64) } else { 0.0 };
    let shares = || core::iter::once(nulls).chain(mcv.iter().map(|&f| f as f64)).chain(core::iter::once(rest));
    let independent = shares().fold(0.0, f64::max);
    let read = if rows == 0 { t } else { rows as f64 };
    let whole = shares().filter(|&f| f * t <= read).fold(0.0, f64::max) * t / read;
    Some(independent.min(whole).clamp(0.0, 1.0))
}

/// A collation's key facts from `pg_locale` (`Facts::coll`'s server answer, D247): also exec front.rs's, which keys a
/// constant-pattern match's class (DC-14, decompose effects `pattern_good`) with the same collation answer the lowering reads.
pub fn key_coll(oid: u32) -> KeyColl {
    if oid == 0 {
        return KeyColl::of(0);
    }
    match gravity_fn::typesem::collation::Facts::of(oid as Oid) {
        Ok(f) => KeyColl { oid, bytes_eq: f.equality_is_bytes(), bytes_order: f.order_is_bytes() },
        Err(_) => KeyColl::of(oid),
    }
}

/// `pg_proc.proparallel` as stored, decoded (pg_proc.h `PROPARALLEL_SAFE` 's', `PROPARALLEL_RESTRICTED` 'r', anything
/// else unsafe).
fn parallel_of(proparallel: i8) -> Parallel {
    match proparallel as u8 {
        b's' => Parallel::Safe,
        b'r' => Parallel::Restricted,
        _ => Parallel::Unsafe,
    }
}

/// `funcid`'s `proparallel` (`SearchSysCache1(PROCOID)`), none without a row; the syscache's error passes through. A
/// hosted aggregate's arena runs partial states per worker only for a PARALLEL SAFE aggregate (with a combinefn), as
/// PostgreSQL's partial aggregation does.
pub(crate) fn proc_parallel(funcid: Oid) -> PgResult<Option<Parallel>> {
    Ok(syscache_seams::lookup_pg_proc_shape::call(funcid)?.map(|p| parallel_of(p.proparallel)))
}

/// D4804: a read of a pgrc relation's jsonb path elections (`shred_election`, `shred_dict`, `shred_object_roots`) holds
/// its storage: a publish that moves what they answer sends a storage-scoped relcache message (pgrc_am `catalog_publish`,
/// `Generation::election_digest`). The estimate facts hold none: the front key's estimate guards judge them. True, for
/// the readers' `&&` chains.
#[inline]
fn layout_read(relid: u32) -> bool {
    gravity_deps::note_stats(relid as Oid);
    true
}

impl<'m> RelcacheFacts<'m> {
    pub fn new(mcx: Mcx<'m>) -> Self {
        RelcacheFacts { mcx, pinned: std::cell::RefCell::new([const { None }; PINNED]) }
    }

    /// D4854: pgrc relation `r`'s resident parts as this lowering first read them (no snapshot: the latest generation then),
    /// so its layout facts (the path elections, the resident rows) answer from one generation however many times a step
    /// asks: a publish committed between two reads moves none of them. The storage-scoped relcache message it may send
    /// (D4804) leaves the program built from them.
    fn resident(&self, r: &types_rel::Relation<'_>) -> PgResult<std::sync::Arc<PartList>> {
        let mut pinned = self.pinned.borrow_mut();
        if let Some((_, l)) = pinned.iter().flatten().find(|(id, _)| *id == r.rd_id) {
            return Ok(l.clone());
        }
        let l = crate::ops::scan::pgrc::residency::resident_parts(r, None)?;
        if let Some(free) = pinned.iter_mut().find(|p| p.is_none()) {
            *free = Some((r.rd_id, l.clone()));
        }
        Ok(l)
    }

    /// The rows the planner's statistics scale by: pgrc's resident rows, heap's `reltuples` (none before the first
    /// ANALYZE or VACUUM counts them); none on any other AM.
    fn rel_rows(&self, relid: u32) -> Option<u64> {
        let r = table::table_open(self.mcx, relid as Oid, NoLock).ok()?;
        // C3 S5 (D3889): the relation's size (a TRUNCATE's storage-scoped message moves it, not the entry)
        gravity_deps::note_stats(relid as Oid);
        let rows = match TableAm::of(&r) {
            Some(TableAm::Pgrcolumnar) => self.resident(&r).ok().map(|l| l.rows()),
            Some(TableAm::Heap) => Some(r.rd_rel.reltuples).filter(|t| t.is_finite() && *t > 0.0).map(|t| t as u64),
            _ => None,
        };
        let _ = r.close(NoLock);
        rows.filter(|&n| n > 0)
    }

    /// Index `indexid` opened for its facts under AccessShareLock taken only if granted at once, and released when the
    /// returned guard drops (D582): PostgreSQL opens an INSERT target's indexes at its first inserted row
    /// (`ExecOpenIndices`, RowExclusiveLock) and a scanned or UPDATE / DELETE target's at planning, whose lock this
    /// backend then already holds (the conditional lock is granted and its release leaves the planner's); so admission
    /// never waits on an index lock PostgreSQL would take later, nor keeps one it would not hold. `None` when the lock is
    /// not free now (the facts unknown: a write refuses and falls through, where the executor takes its own lock).
    fn open_index(&self, indexid: u32) -> Option<(types_rel::Relation<'m>, Held)> {
        let held = Held::take(indexid as Oid)?;
        let r = indexam::index_open(self.mcx, indexid as Oid, NoLock).ok()?;
        Some((r, held))
    }

    /// `WriteLead`'s facts of relation `a`: its write form and its index shapes (`write_twin`).
    fn wlead_facts(&self, a: u32) -> Option<(Option<WriteForm>, Option<Vec<IndexShape>>)> {
        let x = table::table_open(self.mcx, a as Oid, NoLock).ok()?;
        let form = write_form(&x);
        let _ = x.close(NoLock);
        Some((form, self.index_shapes(a)))
    }

    /// D3681: relation `relid`'s indexes in index-list order as `write_twin` compares them (`IndexShape`); `None` when an
    /// index is locked by another session now or unread (`open_index`).
    fn index_shapes(&self, relid: u32) -> Option<Vec<IndexShape>> {
        let list = relcache_seams::relation_get_index_list::call(self.mcx, relid as Oid).ok()?;
        list.iter()
            .map(|&oid| {
                let (r, _held) = self.open_index(oid)?;
                let s = r.rd_index.as_ref().map(|i| IndexShape {
                    am: r.rd_rel.relam,
                    form: (i.indnatts, i.indnkeyatts, i.indisunique, i.indnullsnotdistinct, i.indisprimary, i.indisexclusion, i.indimmediate, i.indisvalid, i.indisready),
                    keys: i.indkey.iter().copied().collect(), // alloc: Program
                    families: r.rd_opfamily.iter().copied().collect(), // alloc: Program
                    collations: r.rd_indcollation.iter().copied().collect(), // alloc: Program
                    options: r.rd_indoption.iter().copied().collect(), // alloc: Program
                    exprs: i.indexprs_src.as_ref().map(|s| s.as_str().to_owned()), // alloc: Program
                    pred: i.indpred_src.as_ref().map(|s| s.as_str().to_owned()), // alloc: Program
                });
                let _ = r.close(NoLock);
                s
            })
            .collect() // alloc: Program
    }

    fn leaves_under(&self, root: &types_rel::Relation<'_>, parent: &types_rel::Relation<'_>, f: &mut dyn FnMut(LeafFact)) -> bool {
        stack_depth_core::check_stack_depth_or_panic();
        let Ok(children) = pg_inherits::find_inheritance_children_extended(self.mcx, parent.rd_id, true, NoLock, None, None) else { return false };
        for &oid in children.iter() {
            let ok = held_briefly(oid, |held| {
                // a partition dropped since the listing fails its open: the leaves unknown
                let Some(c) = open_held(held, oid, || table::table_open(self.mcx, oid, NoLock)) else { return false };
                let ok = if c.rd_rel.relkind == types_rel::RELKIND_PARTITIONED_TABLE { self.leaves_under(root, &c, f) } else { self.leaf_fact(root, &c).map(|l| f(l)).is_some() };
                let _ = c.close(NoLock);
                ok
            });
            if !ok {
                return false;
            }
        }
        true
    }

    /// A leaf's facts (an index locked now is read with NoLock, `held_briefly`).
    fn leaf_fact(&self, root: &types_rel::Relation<'_>, leaf: &types_rel::Relation<'_>) -> Option<LeafFact> {
        let natts = leaf.rd_att.natts as i16;
        let same_layout = leaf.rd_att.natts == root.rd_att.natts && root.rd_att.attrs.iter().zip(leaf.rd_att.attrs.iter()).all(|(a, b)| a.attisdropped == b.attisdropped && (a.attisdropped || (a.attname == b.attname && a.atttypid == b.atttypid && a.atttypmod == b.atttypmod)));
        let has_generated = leaf.rd_att.constr.as_ref().is_some_and(|c| c.has_generated_stored || c.has_generated_virtual);
        let (mut nindexes, mut indexes_plain) = (0u16, true);
        match relcache_seams::relation_get_index_list::call(self.mcx, leaf.rd_id) {
            Ok(list) => {
                nindexes = list.len().min(u16::MAX as usize) as u16;
                for &oid in list.iter() {
                    held_briefly(oid, |held| {
                        match open_held(held, oid, || indexam::index_open(self.mcx, oid, NoLock)) {
                            Some(i) => {
                                indexes_plain &= crate::ops::modifytable::index_maint::unmaintainable_routed(&i).is_none();
                                let _ = i.close(NoLock);
                            }
                            None => indexes_plain = false,
                        }
                        true
                    });
                }
            }
            Err(_) => indexes_plain = false,
        }
        Some(LeafFact { relid: leaf.rd_id, natts, heap: leaf.rd_rel.relkind == types_rel::RELKIND_RELATION && TableAm::of(leaf) == Some(TableAm::Heap), foreign: leaf.rd_rel.relkind == types_rel::RELKIND_FOREIGN_TABLE, pgrc: leaf.rd_rel.relkind == types_rel::RELKIND_RELATION && TableAm::of(leaf) == Some(TableAm::Pgrcolumnar), same_layout, has_triggers: leaf.rd_hastriggers, trig: if leaf.rd_hastriggers { crate::ops::modifytable::trigger::trig_ext_of(leaf.rd_id) } else { TrigExt::NONE }, has_rules: leaf.rd_hasrules, has_generated, nindexes, indexes_plain })
    }
}

/// `f` under AccessShareLock on `oid` when that lock is granted at once (`ConditionalLockRelationOid`), released after:
/// admission reads a partition's relcache entry under a lock but never waits for one, nor keeps it (D573). V4R-303: when
/// the lock is not free now, `f` reads with no lock (partition_leaves: the committed state, revalidated at the first row);
/// `f` is told which (`open_held`).
fn held_briefly(oid: Oid, f: impl FnOnce(bool) -> bool) -> bool {
    let held = Held::take(oid);
    f(held.is_some())
}

/// Relation `oid` for its facts: `open` (a NoLock open) under the lock `held_briefly` holds, else (V4R-303, the lock busy)
/// its relcache entry as built from the catalog snapshot, taken without `relation_open`, which asserts that a NoLock
/// opener holds a lock (PostgreSQL's relation_open Assert, D2951: this backend holds none here): RelationIdGetRelation,
/// as relation_open reads the entry after its lock. Its handle closes nothing. `None`: the relation is gone.
fn open_held<'m>(held: bool, oid: Oid, open: impl FnOnce() -> PgResult<types_rel::Relation<'m>>) -> Option<types_rel::Relation<'m>> {
    if held {
        return open().ok();
    }
    let data = relcache_seams::relation_id_get_relation::call(oid).ok().flatten()?;
    Some(types_rel::Relation::open_rc(data, None))
}

/// AccessShareLock on a relation taken only if granted at once, released on drop (D582, `open_index`).
struct Held(Oid);

impl Held {
    fn take(oid: Oid) -> Option<Held> {
        matches!(lmgr_seams::conditional_lock_relation_oid::call(oid, AccessShareLock), Ok(true)).then(|| Held(oid))
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let _ = lmgr_seams::unlock_relation_oid::call(self.0, AccessShareLock);
    }
}

/// D3402: keysem's alias verdict (`gravity_fn::typesem::keysem::call::set_alias_verdict`): SQL comparator `cmp`'s statement
/// calling built-in `b` over two non-NULL values answers with `b`'s result alone, by this home's reading of the catalog:
/// `b` raises on no input (`effects::quiet_call`), and `cmp` is IMMUTABLE or STABLE (a VOLATILE function's statement
/// advances the command counter). Read in a scratch context of its own.
pub fn alias_verdict(cmp: u32, b: u32) -> bool {
    let cx = mcx::MemoryContext::new_bump("gravity_hosts alias verdict");
    let f = RelcacheFacts::new(cx.mcx());
    let stable = f.func(cmp).is_some_and(|p| p.volatility <= gravity_decompose::effects::Volatility::Stable);
    stable && gravity_decompose::effects::quiet_call(&f, b)
}

#[cfg(test)]
mod tests {
    use super::top_share;

    #[test]
    fn a_dropped_most_common_value_is_not_the_largest_group() {
        // A 100 M-row table whose `model` text is mostly '': `model <> ''` leaves 5.40 M rows, where one `kind` value
        // is 79 % of them (the most common kind went with ''), and `phrase <> ''` leaves 13.0 M, no phrase over 0.7 %
        let t = 100_000_000;
        let kind = top_share(0.0, &[0.93, 0.0425, 0.0165, 0.003], 40.0, t, 5_400_000).unwrap();
        let model = top_share(0.0, &[0.946, 0.0495, 0.0023], 170.0, t, 5_400_000).unwrap();
        assert!((kind - 0.787).abs() < 0.001 && (model - 0.917).abs() < 0.001, "{kind} {model}");
        let phrase = top_share(0.0, &[0.87, 0.0009, 0.000_37, 0.000_27], 5_800_000.0, t, 13_000_000).unwrap();
        assert!(phrase < 0.01, "{phrase}");
        // unfiltered: the most common value's share; an independent filter keeps it (the smaller reading)
        assert!((top_share(0.0, &[0.183, 0.066], 9195.0, t, 0).unwrap() - 0.183).abs() < 1e-6);
        assert!((top_share(0.0, &[0.183, 0.066], 9195.0, t, t).unwrap() - 0.183).abs() < 1e-6);
        assert!((top_share(0.0, &[0.3, 0.1], 100.0, 1000, 200).unwrap() - 0.3).abs() < 1e-6);
        // NULL is a group: a column mostly NULL
        assert!((top_share(0.9, &[0.05], 10.0, 1000, 0).unwrap() - 0.9).abs() < 1e-6);
        // no MCV list: a value's mean share; a distinct count that scales with the rows: near nothing
        assert!((top_share(0.0, &[], 50.0, 1000, 0).unwrap() - 0.02).abs() < 1e-9);
        assert!(top_share(0.0, &[], -0.5, 1000, 0).unwrap() < 0.01);
        assert!(top_share(0.0, &[], 0.0, 1000, 0).is_none());
    }
}

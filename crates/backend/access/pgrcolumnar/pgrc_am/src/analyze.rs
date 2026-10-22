//! M5a: the ENGINE ANALYZE FOLD read leg (ST-2 made LIVE — the M3
//! remainder + the S5 sketch-leg residue). `ANALYZE` on a pgrcolumnar
//! table folds the seal-built facts — part-grain footer `StatsRecord`s,
//! the Stats-sidecar distribution sketches (ST-1/OD-2), the footer HLL
//! NDV union, and the FT-6/SortKey cluster witness — into
//! `pg_statistic`-shaped per-column output via the ONE fold implementation
//! (`pgrc_meta::fold::fold_pg_statistic`). **No sampling scan**: the v2
//! comparator pays a whole-table sampling pass (46.9s at 10m); the fold
//! reads footers + sidecars only (ms class).
//!
//! ## The refusal seam (the fail-open law, ES-1.3 applied to ANALYZE)
//!
//! The fold either serves the WHOLE relation or declines TYPED — and a
//! decline is never an error and never wrong stats: the caller falls open
//! to the pre-existing sampling path (`pgrc_acquire_sample_rows`), the
//! conservative election. Decline causes are a closed vocabulary
//! ([`FoldDecline`]), censused by the caller. On BANKED data the decline
//! path is unreachable by construction (stats are a seal byproduct; banks
//! ship stats-built — the ST-2 law the M3 estimate-probe cells witnessed,
//! with a born-RED stats-stripped seed).
//!
//! Notable causes:
//! - **stand-in vintage** ([`FoldDecline::StatsWitnessAbsent`]): a part
//!   record without the `STATSF_COMPUTED` witness carries exact `nonnull`
//!   with UNCOMPUTED zero aggregates — folding it would deflate widths
//!   silently (the S4-review C2 lesson applied to ANALYZE).
//! - **deletion-bearing** ([`FoldDecline::DeletionBearing`]): seal-time
//!   counts ignore delete vectors; folding them would serve resurrected
//!   rows to the planner (the C4/DM-2 lesson applied to stats).
//!
//! ## Rendering discipline
//!
//! Output values are CANONICAL BYTES (spec §18.1). The AM stays
//! render-agnostic: [`ColFold`] carries the column's `StorageClass` +
//! `TypeSemantics` + `CollationClass`, and the consumer (`commands/
//! analyze`) admits value lists per the byte-faithfulness gates it owns
//! (byte-eq == value-eq for MCVs; recoverable order for histograms).

// Re-exported for the AM facade (`tableam`): the consumer's render gates.
pub use pgrc_format::class::{CollationClass, StorageClass, TypeSemantics};
pub use pgrc_format::sidecar::STATS_MCV_K as PGRC_STATS_MCV_K;
use pgrc_format::bankstats as plane;
use pgrc_format::dirlayout::part_file_name;
use pgrc_format::ident::schema_fingerprint;
use pgrc_format::manifest::Manifest;
use pgrc_format::meta::{StatsRecord, STATSF_COMPUTED};
use pgrc_format::part::SectionKind;
use pgrc_format::sidecar::{ColDistribution, SidecarKind};
use pgrc_meta::fold::{fold_pg_statistic_for, PartColInput};
pub use pgrc_meta::fold::PgStatColumn;
use pgrc_read::{PartExpect, TableExpect, VfsPartIo};
use memledger::charge::Owner;
use memledger::{ChargedVec, Kind};
use types_error::{PgError, PgResult};
use types_rel::Relation;

use crate::probe::SnapshotCommitCheck;

/// The typed decline vocabulary (append-only; `cause()` is the census
/// currency). A decline means "sample instead", never "error" and never
/// "serve anyway".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoldDecline {
    /// Never-published table: the sampling path scans nothing at ~0 cost
    /// and writes the never-analyzed convention — no fold to serve.
    NoCommittedPublish,
    /// A part referenced by the manifest carries a delete vector; seal
    /// counts ignore Dv (the C4/DM-2 stats lesson).
    DeletionBearing { part_no: u32 },
    /// The Stats sidecar companion is absent for a part (stats-stripped
    /// twin, or a wholly sketch-free part — all-opaque profiles).
    SidecarAbsent { part_no: u32 },
    /// The sidecar failed envelope validation (refuse-and-rebuild class).
    SidecarStale { part_no: u32, why: &'static str },
    /// A column's Stats section is absent in a part.
    StatsSectionAbsent { part_no: u32, attno: u32 },
    /// A part record without the `STATSF_COMPUTED` witness (stand-in
    /// vintage — exact nonnull, zeroed aggregates).
    StatsWitnessAbsent { part_no: u32, attno: u32 },
    /// The NDV supply declined (footer fold unavailable).
    NdvSupplyAbsent,
    /// Rows in the front's segments, which the part footers do not count (F1).
    FrontResidue,
    /// FASTADD: parts written before an ADD/DROP COLUMN lack a column the rows read as its missing value (the
    /// generation lists them under their older schema); the sampled rows carry it.
    SchemaLagged,
}

impl FoldDecline {
    /// Stable census token.
    pub fn cause(&self) -> &'static str {
        match self {
            FoldDecline::NoCommittedPublish => "no-committed-publish",
            FoldDecline::DeletionBearing { .. } => "deletion-bearing",
            FoldDecline::SidecarAbsent { .. } => "sidecar-absent",
            FoldDecline::SidecarStale { .. } => "sidecar-stale",
            FoldDecline::StatsSectionAbsent { .. } => "stats-section-absent",
            FoldDecline::StatsWitnessAbsent { .. } => "stats-witness-absent",
            FoldDecline::NdvSupplyAbsent => "ndv-supply-absent",
            FoldDecline::FrontResidue => "front-residue",
            FoldDecline::SchemaLagged => "schema-lagged",
        }
    }
}

/// One folded column, with the render-gate facts the consumer needs.
#[derive(Debug, Clone)]
pub struct ColFold {
    pub attno: u32,
    pub stat: PgStatColumn,
    pub class: StorageClass,
    pub semantics: TypeSemantics,
    pub collation_class: CollationClass,
    /// Σ nonnull across parts (exact; the correlation/value-count gate).
    pub nonnull: u64,
}

/// The whole-relation fold.
#[derive(Debug, Clone)]
pub struct RelFold {
    /// Exact committed row count (footer facts — `reltuples` truth).
    pub total_rows: u64,
    /// Every live column, attno-ascending.
    pub cols: Vec<ColFold>,
    /// Where the per-part Stats records came from: `"answers"` (the
    /// catalog generation's part-grain answers, in memory, P-1), `"plane"`
    /// (the bank-grain stats plane, one pread per column) or `"sections"`
    /// (one meta-section pread per part per column). Census only; the
    /// records are the same bytes either way.
    pub source: &'static str,
}

/// `PGRUST_PGRC_ANALYZE_PLANE=0|off`: never consult the stats plane (the
/// byte-identity A/B arm — the sections path is the authoritative one).
fn plane_enabled() -> bool {
    static F: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *F.get_or_init(|| {
        !matches!(
            std::env::var("PGRUST_PGRC_ANALYZE_PLANE").as_deref(),
            Ok("0") | Ok("off")
        )
    })
}

/// The per-part Stats records for every live column, read from the
/// bank-grain stats plane (`bankstats-<gen>.pgrcbs`, `pgrc_format::
/// bankstats`): the plane holds every part's UNWRAPPED Stats body per
/// column CONTIGUOUSLY, so the whole supply is one pread per column
/// instead of one small pread per part per column. On a cold page cache
/// the section path is IOPS-bound (parts × columns ≈ 53k 5-KB reads at
/// 100m — 20–27 s on gp2, measured 2026-09-15); the plane path is
/// throughput-bound (≈ 1.7 % of the bank, sequential).
///
/// Derived-artifact law (bankplane.rs): the plane is never the source of
/// truth. It is bound to this exact manifest by gen + schema fingerprint +
/// the full per-part identity vector; ANY mismatch, short read, crc or
/// shape refusal, or record-geometry surprise returns `None` and the caller
/// reads the sections — never wrong, only slower. Result is col-major
/// `[col_ix][part_ix]`; `None` inside means the plane carries no body for
/// that (part, column) and the caller consults the section for it. Also compaction's part-size estimate (K1 L7).
///
/// Each column's records sit in a `ChargedVec` on `owner` (the ANALYZE
/// statement, wave 3 cvformat): reserved once at the part count, so the
/// whole supply is charged when it is allocated. A refused reservation is
/// a skipped accelerator (the sections are read instead).
pub fn plane_records(
    dir: &str,
    manifest: &Manifest,
    fp: u64,
    attnos: &[u32],
    owner: Owner,
) -> Option<ChargedVec<ChargedVec<Option<StatsRecord>>>> {
    if !plane_enabled() {
        return None;
    }
    let path = format!("{dir}/{}", plane::bankstats_file_name(manifest.header.gen)); // alloc: Cold
    let cpath = std::ffi::CString::new(path).ok()?;
    let io = VfsPartIo::open(&cpath).ok()?;
    let file_len = pgrc_read::PartIo::len(&io);
    let mut head = [0u8; plane::BANKSTATS_HEADER_LEN];
    pgrc_read::PartIo::pread_exact(&io, 0, &mut head, "bankstats header").ok()?;
    // Header word 5 (after magic, version, gen, fingerprint, counts).
    let meta_len = u64::from_le_bytes(head[32..40].try_into().ok()?);
    if meta_len < plane::BANKSTATS_HEADER_LEN as u64 || meta_len > file_len {
        return None;
    }
    let mut meta = crate::alloc::try_filled(meta_len as usize, 0u8, "analyze").ok().or_else(|| crate::accel_skipped(pgrc_format::Accel::BankPlane, meta_len as usize))?; // alloc: Cold
    pgrc_read::PartIo::pread_exact(&io, 0, &mut meta, "bankstats meta").ok()?;
    let (header, idents, cols) = plane::decode_meta(&meta).ok()?;
    let bound = header.gen == manifest.header.gen
        && header.schema_fingerprint == fp
        && header.schema_fingerprint == manifest.header.schema_fingerprint
        && header.part_count as usize == manifest.parts.len()
        && idents.len() == manifest.parts.len()
        && idents.iter().zip(manifest.parts.iter()).all(|(a, b)| {
            a.part_no == b.part_no
                && a.granule_count == b.granule_count
                && a.band_count == b.band_count
                && a.rows == b.rows
                && a.footer_off == b.footer_off
        });
    if !bound {
        return None;
    }
    let mut out: ChargedVec<ChargedVec<Option<StatsRecord>>> = ChargedVec::new(Some(owner), Kind::Region);
    if out.try_reserve(attnos.len()).is_err() { // alloc: Exec charged owner
        return crate::accel_skipped(pgrc_format::Accel::BankPlane, attnos.len().saturating_mul(core::mem::size_of::<ChargedVec<Option<StatsRecord>>>()));
    }
    for &attno in attnos {
        let e = cols.iter().find(|c| c.attno == attno)?;
        let end = e.off.checked_add(e.len)?;
        if end > file_len {
            return None;
        }
        let len = e.len as usize;
        let mut buf: ChargedVec<u8> = ChargedVec::new(Some(owner), Kind::Region);
        if buf.try_reserve(len).is_err() { // alloc: Exec charged owner
            return crate::accel_skipped(pgrc_format::Accel::BankPlane, len);
        }
        buf.resize(len, 0).ok()?; // alloc: Exec charged owner (reserved above: no growth)
        pgrc_read::PartIo::pread_exact(&io, e.off, &mut buf, "bankstats column").ok()?;
        let view = plane::ColPayloadRef::new(e, header.part_count, &buf).ok()?;
        let mut col: ChargedVec<Option<StatsRecord>> = ChargedVec::new(Some(owner), Kind::Region);
        let n = manifest.parts.len();
        if col.try_reserve(n).is_err() { // alloc: Exec charged owner
            return crate::accel_skipped(pgrc_format::Accel::BankPlane, n.saturating_mul(core::mem::size_of::<Option<StatsRecord>>()));
        }
        for (pi, rec) in manifest.parts.iter().enumerate() {
            let r = match view.stats_body(pi) {
                None => None,
                Some(body) => Some(
                    pgrc_write::facts::part_stats_record(
                        body,
                        rec.granule_count,
                        rec.band_count,
                        attno,
                    )
                    .ok()?,
                ),
            };
            col.push(r).ok()?; // alloc: Exec charged owner (reserved above: no growth)
        }
        out.push(col).ok()?; // alloc: Exec charged owner (reserved above: no growth)
    }
    Some(out)
}

/// K1 L7 (R15 merge): [`plane_records`] for a caller with no owner of its own to name (pgrc_front's compaction
/// part-size estimate): the records are charged to the running thread's charge context.
pub fn plane_records_here(dir: &str, manifest: &Manifest, fp: u64, attnos: &[u32]) -> Option<ChargedVec<ChargedVec<Option<StatsRecord>>>> {
    plane_records(dir, manifest, fp, attnos, memledger::charge::context().0)
}

/// P-1 L2: the part-grain answers of the catalog's generation that is `manifest` (the same number, publisher and part
/// records): every part's Stats record in memory. `None` when the catalog holds no such generation, or its answers
/// do not cover the manifest's parts.
pub(crate) fn catalog_answers(slot: crate::catalog::RelSlot, manifest: &Manifest) -> Option<std::sync::Arc<crate::answers::Answers>> {
    let (gens, _) = crate::catalog::generations(slot)?;
    let h = manifest.header;
    let g = gens.into_iter().find(|g| g.gen == h.gen && g.publisher_fxid == h.publisher_fxid && !g.composed())?;
    if g.manifest.parts != manifest.parts {
        return None;
    }
    g.answers().filter(|a| a.parts() == manifest.parts.len())
}

/// An empty per-part slot column on `owner`, reserved at `n` parts (ERROR
/// 53200 when refused).
fn part_slots<T>(owner: Owner, n: usize) -> PgResult<ChargedVec<T>> {
    let mut v = ChargedVec::new(Some(owner), Kind::Region);
    v.try_reserve(n).map_err(|_| { // alloc: Node charged owner
        crate::format_error(pgrc_format::FormatError::OutOfMemory { bytes: n.saturating_mul(core::mem::size_of::<T>()) })
    })?;
    Ok(v)
}

/// One part's slot pushed (reserved at the part count, so it never grows;
/// a refusal would be ERROR 53200).
fn slot_push<T>(v: &mut ChargedVec<T>, x: T) -> PgResult<()> {
    v.push(x).map_err(|_| crate::format_error(pgrc_format::FormatError::OutOfMemory { bytes: core::mem::size_of::<T>() })) // alloc: Node charged owner (reserved: no growth)
}

/// The heap a decoded Stats payload holds, by capacity.
fn payload_heap(dist: &Vec<(u32, u32, ColDistribution)>) -> u64 {
    let slots = dist.capacity().saturating_mul(core::mem::size_of::<(u32, u32, ColDistribution)>()) as u64;
    slots + dist.iter().map(|(_, _, d)| d.heap_bytes()).sum::<u64>()
}

/// Fold outcome: served or typed-declined (the fail-open seam).
#[derive(Debug, Clone)]
pub enum AnalyzeFold {
    Folded(RelFold),
    Declined(FoldDecline),
}

/// Attempt the fold. Errors are real IO/corruption refusals (typed, same
/// classes the scan path raises); "cannot serve" is [`AnalyzeFold::
/// Declined`], never an error.
pub fn analyze_fold(rel: &Relation<'_>) -> PgResult<AnalyzeFold> {
    crate::inval::ensure_inval_registered()?;
    let locator = rel.rd_locator.get();
    let relfilenumber = locator.relNumber as u64;
    let schemas = crate::schema::col_schemas(rel)?;
    let fp = schema_fingerprint(&schemas);
    let dir = crate::dirpath::table_dir_path(locator, rel.rd_backend);

    let check = SnapshotCommitCheck::new(None);
    // the schema fingerprint is checked against the effective generation below (D2189: a column dropped
    // with no ALTER publish leaves the generation under its stamp; the fold declines it to the sampling path, whose row
    // scan reads it through the schema book)
    let expect = TableExpect {
        relfilenumber: Some(relfilenumber),
        schema_fingerprint: None,
    };
    let eff = crate::scan::resolve_for_scan(&dir, &check, &expect)?;
    check.take_error()?;
    let Some(eff) = eff else {
        return Ok(AnalyzeFold::Declined(FoldDecline::NoCommittedPublish));
    };
    if eff.manifest.header.flags & pgrc_format::manifest::MANIFESTF_SCHEMAS != 0 || eff.manifest.header.schema_fingerprint != fp {
        return Ok(AnalyzeFold::Declined(FoldDecline::SchemaLagged));
    }

    // NDV supply: the footer HLL union, served through the process-grain
    // fact cache (nanoseconds when any session of this process folded
    // this generation).
    let Some(ndv) = crate::footer::footer_ndv(rel)? else {
        return Ok(AnalyzeFold::Declined(FoldDecline::NdvSupplyAbsent));
    };

    let attnos: Vec<u32> = schemas.iter().map(|s| s.attno).collect();
    let ncols = attnos.len();
    let nparts = eff.manifest.parts.len();
    // Wave 3 (cvformat): what the fold keeps across parts sits on the
    // ANALYZE statement (the leader's charge context), by construction.
    // The per-column record and sketch slots are ChargedVecs reserved once
    // at the part count. The sketches' value bytes (decoded per part and
    // moved in, never cloned) are held by `kept`, fitted on this thread at
    // each part: up to the whole decoded payload while it lives, then to
    // the moved sketches once the rest is dropped. The fold runs on this
    // one thread, so each fit commits what this thread allocated.
    let owner = memledger::charge::context().0;
    // Per column, per part: (part record, part rows, sketch).
    let mut records: Vec<ChargedVec<StatsRecord>> = crate::alloc::try_vec(ncols, "analyze")?;
    let mut sketches: Vec<ChargedVec<Option<ColDistribution>>> = crate::alloc::try_vec(ncols, "analyze")?;
    for _ in 0..ncols {
        records.push(part_slots(owner, nparts)?);
        sketches.push(part_slots(owner, nparts)?);
    }
    let mut kept = pgrc_meta::kept::Kept::new(Some((owner, Kind::Region)));
    let mut held: u64 = 0;
    let mut part_rows: Vec<u64> = Vec::new();
    // FT-6 cluster witness: Some(attno) while every part's SortKey section
    // leads with the same column; None once falsified.
    let mut cluster_lead: Option<u32> = None;
    let mut first_part = true;

    // The supply, in order (P-1 L2): the catalog generation's part-grain answers (every part's record in memory, no
    // read); else the plane; `None` from both = every record comes from its section.
    let answers = catalog_answers((locator.spcOid, locator.dbOid, relfilenumber), &eff.manifest);
    let plane_recs = if answers.is_some() { None } else { plane_records(&dir, &eff.manifest, fp, &attnos, owner) };
    let source: &'static str = match (&answers, &plane_recs) {
        (Some(_), _) => "answers",
        (None, Some(_)) => "plane",
        (None, None) => "sections",
    };

    let reg = crate::inval::registry();
    let mut total_rows: u64 = 0;
    for (part_ix, rec) in eff.manifest.parts.iter().enumerate() {
        if rec.dv_gen != 0 {
            return Ok(AnalyzeFold::Declined(FoldDecline::DeletionBearing {
                part_no: rec.part_no,
            }));
        }
        total_rows += rec.rows;
        part_rows.push(rec.rows);

        // The Stats sidecar companion (ST-1): one file per part, gen 1,
        // key fingerprint = the schema fingerprint (the writer's
        // publish_stats_sidecars contract).
        let dir_io = pgrc_read::io::VfsTableDir::new(dir.clone());
        let consult = pgrc_read::sidecar::read_sidecar(
            &dir_io,
            rec.part_no,
            SidecarKind::Stats,
            1,
            None,
            Some(fp),
        )
        .map_err(crate::read_error)?;
        let payload = match consult {
            pgrc_read::sidecar::SidecarConsult::Hit { payload, .. } => payload,
            pgrc_read::sidecar::SidecarConsult::Absent => {
                return Ok(AnalyzeFold::Declined(FoldDecline::SidecarAbsent {
                    part_no: rec.part_no,
                }))
            }
            pgrc_read::sidecar::SidecarConsult::Stale(why) => {
                return Ok(AnalyzeFold::Declined(FoldDecline::SidecarStale {
                    part_no: rec.part_no,
                    why,
                }))
            }
        };
        let mut dist = pgrc_format::sidecar::decode_stats_payload(&payload).map_err(|e| {
            Box::new(PgError::error(format!( // alloc: Cold
                "pgrcolumnar: Stats sidecar payload for part {}: {e:?}",
                rec.part_no
            ))
            .with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED))
        })?;
        kept.fit(held + payload_heap(&dist));

        // Pin the part; read every column's part-grain record + SortKey.
        let pexpect = PartExpect::from_manifest(rec, fp, relfilenumber);
        let path = format!("{dir}/{}", part_file_name(rec.part_no));
        let cpath = std::ffi::CString::new(path.clone()).map_err(|_| {
            Box::new(PgError::error(format!( // alloc: Cold
                "pgrcolumnar: part path contains NUL: {path}"
            )))
        })?;
        let pin = reg
            .open_pinned(&pexpect, || {
                Ok(Box::new(VfsPartIo::open(&cpath)?) as Box<dyn pgrc_read::PartIo>)
            })
            .map_err(crate::read_error)?;
        let part = pin.part();
        let ident = part.ident();
        crate::inval::record_part_key(rel.rd_id, (ident.dev, ident.ino, ident.len));
        let granules = part.footer().granule_count;
        let bands = part.footer().band_count;
        // The plane's records were sliced with the manifest's geometry; the
        // opened footer is the authority — a disagreement (structurally
        // unreachable past the open's expectation checks) reads the sections.
        let plane_geometry_ok = granules == rec.granule_count && bands == rec.band_count;

        for (i, &attno) in attnos.iter().enumerate() {
            // the answers hold the body's last record, the one the plane and the section slice (spec §8.1)
            let answered = answers.as_ref().and_then(|a| a.part_record(attno, part_ix)).and_then(|b| StatsRecord::decode(&mut pgrc_format::wire::Cur::new(b)).ok());
            let planed = if plane_geometry_ok {
                answered.or_else(|| plane_recs.as_ref().and_then(|p| p[i][part_ix]))
            } else {
                None
            };
            let record = match planed {
                Some(r) => r,
                None => {
                    let Some(idx) = part.find_section(SectionKind::Stats, attno, 0) else {
                        return Ok(AnalyzeFold::Declined(FoldDecline::StatsSectionAbsent {
                            part_no: rec.part_no,
                            attno,
                        }));
                    };
                    let body = crate::footer::meta_section_body(part, idx)?;
                    crate::footer::part_stats_record(&body, granules, bands, attno)?
                }
            };
            if record.flags & STATSF_COMPUTED == 0 {
                return Ok(AnalyzeFold::Declined(FoldDecline::StatsWitnessAbsent {
                    part_no: rec.part_no,
                    attno,
                }));
            }
            slot_push(&mut records[i], record)?;
            // The top-level column's sketch is the path_ord == 0 entry;
            // absence is honest (empty value lists), never a decline. It
            // moves out of the payload (attnos are distinct: taken once).
            let sketch = dist
                .iter_mut()
                .find(|(a, path_ord, _)| *a == attno && *path_ord == 0)
                .map(|(_, _, d)| core::mem::take(d));
            held += sketch.as_ref().map_or(0, ColDistribution::heap_bytes);
            slot_push(&mut sketches[i], sketch)?;
        }
        // The rest of the payload (other paths, emptied entries) goes; the
        // charge follows down to the sketches kept.
        drop(dist);
        kept.fit(held);

        // FT-6: the per-part clustered witness (SortKey section, part
        // grain — attno 0). nkeys == 0 = undeclared (OD-8).
        // sortpath (trunk 11a92c3cccf): a path key orders a value of the column, not the column
        let lead = crate::footer::part_sort_key(part)?.keys.first().filter(|k| k.pad == 0).map(|k| k.attno);
        if first_part {
            cluster_lead = lead;
            first_part = false;
        } else if cluster_lead != lead {
            cluster_lead = None;
        }
    }

    if part_rows.is_empty() {
        // A committed but part-less generation: zero rows; the sampling
        // path writes the same truth at the same cost.
        return Ok(AnalyzeFold::Declined(FoldDecline::NoCommittedPublish));
    }

    let mut cols = crate::alloc::try_vec(ncols, "analyze")?;
    for (i, s) in schemas.iter().enumerate() {
        let mut inputs: ChargedVec<PartColInput<'_>> = part_slots(owner, nparts)?;
        let per_part = records[i].iter().zip(part_rows.iter()).zip(sketches[i].iter());
        let per_part = per_part.map(|((record, &rows), sketch)| PartColInput { part_record: record, rows, sketch: sketch.as_ref() });
        inputs.with_vec(|v| v.extend(per_part)); // alloc: Exec charged owner (reserved at the part count: no growth)
        let ndv_est = ndv.get(s.attno as usize - 1).copied().unwrap_or(0) as f64;
        let cluster_declared = cluster_lead == Some(s.attno);
        // FOLDSTATS: the column's value order, which its sketches' bounds follow and the merge keeps
        let order = pgrc_meta::profile::MetaProfile::derive(s.class, s.collation_class, s.semantics)
            .map_or(pgrc_meta::fold::HistOrder::Bytes, |p| pgrc_meta::fold::HistOrder::of(&p));
        let Some(stat) = fold_pg_statistic_for(&inputs, ndv_est, cluster_declared, order, Some((owner, Kind::Region))) else {
            // Empty relation (total_rows == 0 handled above per part);
            // structurally unreachable here, but decline honestly.
            return Ok(AnalyzeFold::Declined(FoldDecline::NoCommittedPublish));
        };
        let nonnull: u64 = records[i].iter().map(|r| u64::from(r.nonnull)).sum();
        cols.push(ColFold {
            attno: s.attno,
            stat,
            class: s.class,
            semantics: s.semantics,
            collation_class: s.collation_class,
            nonnull,
        });
    }

    // Fold-vintage witness for the S-1 pg_statistic read leg's honest
    // source stamping (this process wrote fold-derived rows for this
    // relation — the consumer records it AFTER the pg_statistic write).
    Ok(AnalyzeFold::Folded(RelFold { total_rows, cols, source }))
}

/// Record that fold-derived pg_statistic rows were WRITTEN for `rel`
/// (called by the ANALYZE consumer after `update_attstats` commits its
/// work into the command). Process-lifetime witness only.
pub fn record_fold_vintage(rel: &Relation<'_>) {
    let locator = rel.rd_locator.get();
    crate::catalog::record_fold_vintage((
        locator.spcOid,
        locator.dbOid,
        locator.relNumber as u64,
    ));
}

/// True when THIS process wrote `rel`'s pg_statistic rows via the fold
/// (SketchFold-class provenance). False after restart or on
/// sampled-fallback relations — the conservative AnalyzeSample label.
pub fn is_fold_vintage(rel: &Relation<'_>) -> bool {
    let locator = rel.rd_locator.get();
    crate::catalog::is_fold_vintage((
        locator.spcOid,
        locator.dbOid,
        locator.relNumber as u64,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decline vocabulary is pinned (append-only census tokens — a
    /// rename breaks every consumer's census diff).
    #[test]
    fn decline_cause_vocabulary_is_pinned() {
        let causes: Vec<&'static str> = vec![
            FoldDecline::NoCommittedPublish.cause(),
            FoldDecline::DeletionBearing { part_no: 1 }.cause(),
            FoldDecline::SidecarAbsent { part_no: 1 }.cause(),
            FoldDecline::SidecarStale { part_no: 1, why: "x" }.cause(),
            FoldDecline::StatsSectionAbsent { part_no: 1, attno: 1 }.cause(),
            FoldDecline::StatsWitnessAbsent { part_no: 1, attno: 1 }.cause(),
            FoldDecline::NdvSupplyAbsent.cause(),
            FoldDecline::FrontResidue.cause(),
        ];
        assert_eq!(
            causes,
            vec![
                "no-committed-publish",
                "deletion-bearing",
                "sidecar-absent",
                "sidecar-stale",
                "stats-section-absent",
                "stats-witness-absent",
                "ndv-supply-absent",
                "front-residue",
            ]
        );
    }
}

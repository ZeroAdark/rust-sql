//! Table-grain footer facts for the planner and ANALYZE (WW-2, census S02 /
//! register U-22): the pgrcolumnar sibling of the old AM's
//! `pgrcolumnar::footer_ndv` / `footer_rows` surface (`tableam` re-exports
//! both AMs' faces side by side).
//!
//! ## What is served
//!
//! - [`footer_size`] — `(total rows, total on-disk part bytes)` straight
//!   from the effective manifest's part records (no part opens). Consumed
//!   by `tableam::table_relation_estimate_size`: without it every
//!   pgrcolumnar plan sized the relation from the deliberately-EMPTY main
//!   fork (0 blocks ⇒ plan-time tuples 0), which also starved every
//!   NDV consumer downstream (`add_unique_group_var` clamps to
//!   `rel->tuples`, and a negative `stadistinct` multiplies by it).
//! - [`footer_ndv`] — per-column whole-table NDV, the max-per-register
//!   union of every part's `NdvRegisters` section (spec §8.4 dense HLL —
//!   "store the mergeable form"; per-part `ndv_est` scalars cannot be
//!   combined, a sum double-counts values shared across parts). Indexed by
//!   `attnum - 1` (attnos are dense 1-based — `schema::col_schemas`
//!   refuses dropped columns); `0` = unknown for that column (section
//!   absent in some part: Opaque semantics, or a profile without NDV).
//!   Consumed by ANALYZE's footer-NDV override (sampled Duj1 underestimates
//!   heavy-tailed text NDV 100-1500x; the ingest-time whole-stream sketch
//!   wins) and by plancat's no-pg_statistic group-key estimation.
//! - [`footer_col_bytes`] — per-column ON-DISK data bytes, Σ over every
//!   part of the column's data-section lengths (`StreamDir`/`Stream`/
//!   `PathTable` entries in the section table — dict payload and overflow
//!   streams are `Stream` sections under the column's attno, so a
//!   part-global dict's bytes charge to the column that owns it). EXACT:
//!   the section table is the writer's own byte accounting, and the sum
//!   costs zero extra IO (the table is resident on every open). Metadata
//!   sections (Stats/Psma/Bloom/NdvRegisters/SortKey) are excluded: they
//!   are uniform, small, and consulted for pruning regardless of the plan's
//!   column need-set. Consumed by plancat for column-fraction seqscan disk
//!   costing (the pgrc arm of `costsize::pgrcolumnar_scan_col_fraction`).
//! - [`footer_avg_widths`] — per-column planner-convention average datum
//!   width (`0` = unknown): round(Σ `byte_len_sum` / Σ `nonnull`) +
//!   VARHDRSZ, folded over every part's PART-grain `StatsRecord`.
//!   DERIVED-EXACT: both sums are ingest-time whole-stream counts (never
//!   sampled), guarded by the `STATSF_COMPUTED` witness (#598) in every
//!   part; the +4 mirrors `stawidth`'s `VARSIZE_ANY` convention for
//!   4-byte-header varlena (short-header slack accepted — cost-model
//!   grade). Only LenStats columns ever answer (fixed-width columns read 0
//!   and the consumer's `get_typavgwidth` fallback is already exact for
//!   them). Consumed by plancat's pgrc `attr_widths` pre-fill (the
//!   `set_rel_width` cache), replacing the 32-byte type-default guess on
//!   never-/sampled-ANALYZEd wide text columns.
//! - [`footer_codecs`] / [`footer_zones`] — the planner's scan cost model
//!   (`costsize::pgrc_cost`): per-column codec kind
//!   (`CODEC_*`, from the Values stream's election and the storage class,
//!   AND-folded like the qual classes: dict only when every part elected
//!   `DictCodes`), Σ per-part dictionary entries (`ncodes`, the DictPayload
//!   stream's value count), the granule count, and per (column, granule)
//!   `[min_key, max_key]` for every column whose zone key is a byval word
//!   (`KeyDerivation::{SignedWord, UnsignedWordSmall, Bool}`), read from
//!   the granule records of the Stats section in granule order across the
//!   parts. An all-null granule is the empty range `(i64::MAX, i64::MIN)`;
//!   a granule without an `Exact` key is the full range; a part whose part
//!   record carries no `Exact` key drops the column (no zones = no pruning
//!   credit, never optimistic). The ranges are shared (`Arc`) so the
//!   per-ask clone of the fold stays cheap.
//!
//! ## Visibility
//!
//! `SnapshotCommitCheck::new(None)` — committed-or-own-transaction, exactly
//! the visibility the AM's ANALYZE scan face runs under
//! (`table_beginscan_analyze` carries no snapshot). Plan-time consumers get
//! advisory statistics, not query answers, so snapshot-exactness is not
//! owed (the old AM's session part cache makes the same call).
//!
//! ## Cost / caching
//!
//! The planner asks on EVERY plan of a pgrcolumnar rel (the old-AM lesson:
//! the uncached footer re-read was ~1.3ms of a ~1.9ms plan constant —
//! fixed-overhead audit 2026-07-14). Every ask re-resolves the effective
//! manifest (two small file reads — staleness is structurally impossible),
//! and the folded facts are served from three tiers, each keyed by the
//! generation: the session TLS block, the PROCESS-GRAIN [`crate::factcache`]
//! (`(spc, db, relfilenumber)` × `(gen, publisher_fxid, schema fingerprint)`),
//! and — since gravity 1c pass 3, the user's ruling 2026-09-13 — the
//! generation's own `facts-<gen>.pgrcf` beside its manifest
//! (`pgrc_format::facts`, written by the publisher, `pgrc_write::facts`):
//! one file read and decode per process per generation, no part opened.
//! A generation without a file (a bank published before the file existed)
//! is walked for THAT plan — the same fold over the parts' sections, one
//! coalesced read per column cluster — and never written at plan time: the
//! startup prime (`gravity_hosts::prime`) and ANALYZE walk once and write
//! it ([`backfill_facts`]). One asker per key folds or reads
//! (`factcache::acquire`); the others wait for it. Entries for
//! dropped/truncated tables are dropped by the relcache-invalidation face;
//! a stale GENERATION can never be served because the resolve precedes
//! every cache read and every tier is content-addressed by the resolved key.
//!
//! ## Error discipline
//!
//! Decode refusals are typed data-corruption errors, never silent zeros
//! (the `Hll::decode_section` contract). A register-length mismatch
//! ACROSS parts refuses too: `Hll::merge` only debug-asserts equal
//! lengths, so the guard lives here, before the merge.

use std::sync::Arc;

use pgrc_format::class::ColSchema;
use pgrc_format::facts::{ColFacts, FactsImage};
use pgrc_format::ident::schema_fingerprint;
use pgrc_format::meta::StatsRecord;
use pgrc_format::part::SectionKind;
use pgrc_format::sortkey::{SortKeyEntry, SortKeyRecord};
use pgrc_format::manifest::Manifest;
use pgrc_format::zonemap::{ZoneMap, ZoneSummary};
use pgrc_read::OpenPart;
use pgrc_meta::key::KeyDerivation;
use pgrc_meta::profile::MetaProfile;
use pgrc_write::facts::{class_codec, serve, zone_rule_without_catalog, FactsAcc, ServedCol};
use types_error::{PgError, PgResult};
use types_rel::Relation;

use crate::probe::SnapshotCommitCheck;

pub use pgrc_write::facts::{
    CODEC_DICT, CODEC_FIXED, CODEC_TEXT, CODEC_UNKNOWN, CODEC_WORD, QUAL_CLASS_BLOOM, QUAL_CLASS_DICT, QUAL_CLASS_ZONE,
};

/// Per-column cost facts for the planner (attnum-1 indexed), folded from
/// every part in the effective manifest and cached with the NDV union
/// (same part-open pass, same generation key).
#[derive(Debug, Clone)]
pub struct ColCostFacts {
    /// Σ on-disk data-section bytes (StreamDir/Stream/PathTable) per
    /// column — exact, from the section tables alone.
    pub disk_bytes: Vec<u64>,
    /// Planner-convention average datum width (payload avg + VARHDRSZ);
    /// 0 = unknown (Stats section or `STATSF_COMPUTED` witness absent in
    /// any part, non-LenStats column, or empty sums).
    pub avg_width: Vec<i32>,
    /// gravity §3 #7: per-column qual evaluation class bits
    /// ([`QUAL_CLASS_ZONE`] | [`QUAL_CLASS_BLOOM`] | [`QUAL_CLASS_DICT`]),
    /// each bit set only when EVERY part carries the witness.
    pub qual_class: Vec<u8>,
    /// gravity §3 #6: max character length over the parts (`char_len_max`
    /// of the part-grain Stats record); 0 = unknown.
    pub max_len: Vec<i32>,
    /// NEW-stats-2: Σ non-null values over the parts' part-grain Stats
    /// records; `None` = unknown (poisoned, or a part lacks the column). A
    /// cost input only (it may lag the snapshot).
    pub nonnull: Vec<Option<u64>>,
    /// The scan cost model's decode-cost class of the Values stream
    /// (`CODEC_*`); `CODEC_DICT` only when every part elected `DictCodes`.
    pub codec: Vec<u8>,
    /// The scan cost model's Σ per-part dictionary entries (0 = none).
    pub ncodes: Vec<u64>,
}

/// One zone column's `[min_key, max_key]` per granule, granule order across
/// the parts (an all-null granule is `(i64::MAX, i64::MIN)`): the compact
/// map the image carries (`pgrc_format::zonemap`), the planner's summary
/// of it (`ZoneSummary`: extrema and the rank grid the census answers
/// from, derived here — at catalog load or at the rig's publish — so no
/// plan walks granules), and the map's expansion, made once per column per
/// process on first use (the zone-walk paths and the JSON dump).
#[derive(Debug)]
pub struct ZoneColumn {
    map: ZoneMap,
    summary: ZoneSummary,
    expanded: Arc<[(i64, i64)]>,
}

impl ZoneColumn {
    pub fn map(&self) -> &ZoneMap {
        &self.map
    }

    pub fn summary(&self) -> &ZoneSummary {
        &self.summary
    }

    /// The ranges, expanded on first use.
    pub fn ranges(&self) -> &Arc<[(i64, i64)]> {
        &self.expanded
    }
}

#[derive(Debug, Clone)]
pub struct ZoneFacts {
    pub attno: u32,
    pub col: Arc<ZoneColumn>,
}

impl ZoneFacts {
    pub fn new(attno: u32, map: ZoneMap) -> PgResult<ZoneFacts> {
        let ranges = map.decode().map_err(crate::format_error)?;
        let summary = ZoneSummary::build(&ranges); // alloc: Cold charged its keeper (heap_bytes: the generation's held_bytes, else the resident part list's zones charge)
        Ok(ZoneFacts { attno, col: Arc::new(ZoneColumn { map, summary, expanded: Arc::from(ranges) }) }) // alloc: Cold charged its keeper (heap_bytes, as above)
    }

    pub fn ranges(&self) -> Arc<[(i64, i64)]> {
        Arc::clone(self.col.ranges())
    }

    /// D4537 4: the column's own bytes (its summary and expanded ranges; the map's are its facts image's).
    pub fn heap_bytes(&self) -> usize {
        const ARC: usize = 2 * core::mem::size_of::<usize>();
        let s = self.col.summary();
        let lists = s.min_lt.capacity() + s.min_le.capacity() + s.max_lt.capacity() + s.max_le.capacity() + s.eq_whole.capacity();
        2 * ARC + core::mem::size_of::<ZoneColumn>() + 8 * s.keys.capacity() + 4 * lists + 16 * self.col.ranges().len()
    }
}

/// The codec-side facts one seam serves together (`footer_codecs`).
#[derive(Debug, Clone)]
pub struct CodecFacts {
    pub granules: u64,
    pub codec: Vec<u8>,
    pub ncodes: Vec<u64>,
}

pub const PARTS_UNKNOWN: u64 = u64::MAX;

/// The session-cached fold for one relfilenumber (lives in [`crate::session`]'s single census-pinned TLS block): the
/// facts and the catalog generation they were folded from, held by identity. GENID / D1128: a generation NUMBER
/// named other content after a same-transaction TRUNCATE (and after a rolled-back publish) and served the old
/// generation's zone columns; a number no longer repeats in a process, and the `Weak` (which keeps the allocation, so
/// no later generation gets its address) makes a hit need the very generation.
pub struct SessionFacts {
    of: std::sync::Weak<crate::catalog::Generation>,
    pub facts: FooterFacts,
}

impl SessionFacts {
    /// D4537 T18c: the heap bytes the facts hold (the per-column lists; the zone columns are their keeper's own charges).
    pub(crate) fn heap_bytes(&self) -> usize {
        let f = &self.facts;
        let cols = f.cols.as_ref().map_or(0, |c| {
            c.disk_bytes.capacity() * size_of::<u64>()
                + c.avg_width.capacity() * size_of::<i32>()
                + c.qual_class.capacity()
                + c.max_len.capacity() * size_of::<i32>()
                + c.nonnull.capacity() * size_of::<Option<u64>>()
                + c.codec.capacity()
                + c.ncodes.capacity() * size_of::<u64>()
        });
        f.ndv.as_ref().map_or(0, |n| n.capacity() * size_of::<u64>()) + cols + f.zones.capacity() * size_of::<ZoneFacts>()
    }

    /// Facts of no generation (tests).
    #[cfg(test)]
    pub(crate) fn detached(facts: FooterFacts) -> SessionFacts {
        SessionFacts { of: std::sync::Weak::new(), facts }
    }

    fn of(gen: &Arc<crate::catalog::Generation>, facts: FooterFacts) -> SessionFacts {
        SessionFacts { of: Arc::downgrade(gen), facts }
    }

    /// The facts when they were folded from `gen` itself.
    fn for_gen(&self, gen: &Arc<crate::catalog::Generation>) -> Option<&FooterFacts> {
        (std::sync::Weak::as_ptr(&self.of) == Arc::as_ptr(gen) && self.facts.gen == gen.gen).then_some(&self.facts)
    }
}

#[derive(Debug, Clone)]
pub struct FooterFacts {
    /// The manifest generation the facts were folded from.
    pub gen: u64,
    /// Σ part rows.
    pub rows: u64,
    /// Σ part file bytes (on-disk size of the sealed parts).
    pub bytes: u64,
    /// Σ part granules.
    pub granules: u64,
    /// Committed parts in the manifest. `sorted_by` is enforced per part,
    /// so a plan rig's fold (no parts on disk) claims the declared order
    /// only when `parts <= 1`; `PARTS_UNKNOWN` (a published fold that did
    /// not say) claims nothing. Real parts answer from their SortKey
    /// sections ([`committed_order`]).
    pub parts: u64,
    /// Per-column NDV (attnum-1 indexed; 0 = unknown). `None` = the deep
    /// fold has not run for this generation (size-only asks never open
    /// parts). Invariant: `ndv.is_some() == cols.is_some()` — the deep
    /// fold computes both in one part-open pass.
    pub ndv: Option<Vec<u64>>,
    /// Per-column cost facts; `None` exactly when `ndv` is `None`.
    pub cols: Option<ColCostFacts>,
    /// Zone columns' granule ranges (deep fold only; empty when shallow or
    /// when no column carries a word key).
    pub zones: Vec<ZoneFacts>,
}

/// Total committed `(rows, on-disk bytes)`; `None` while the table has no
/// committed publish (the never-loaded posture — callers fall through to
/// their pre-existing convention).
/// Committed part count (`None` while the table has no committed publish;
/// `PARTS_UNKNOWN` for a published fold without one).
pub fn part_count(rel: &Relation<'_>) -> PgResult<Option<u64>> {
    Ok(with_facts(rel, false)?.map(|f| f.parts))
}

pub fn footer_size(rel: &Relation<'_>) -> PgResult<Option<(u64, u64)>> {
    let Some(facts) = with_facts(rel, false)? else {
        return Ok(None);
    };
    Ok(Some((facts.rows, facts.bytes)))
}

/// Per-column whole-table NDV (attnum-1 indexed; 0 = unknown); `None`
/// while the table has no committed publish.
pub fn footer_ndv(rel: &Relation<'_>) -> PgResult<Option<Vec<u64>>> {
    let Some(facts) = with_facts(rel, true)? else {
        return Ok(None);
    };
    Ok(facts.ndv)
}

/// Per-column on-disk data bytes (attnum-1 indexed) for column-fraction
/// disk costing; `None` while the table has no committed publish.
pub fn footer_col_bytes(rel: &Relation<'_>) -> PgResult<Option<Vec<u64>>> {
    let Some(facts) = with_facts(rel, true)? else {
        return Ok(None);
    };
    Ok(facts.cols.map(|c| c.disk_bytes))
}

/// Per-column qual evaluation class bits (attnum-1 indexed; `QUAL_CLASS_*`);
/// `None` while the table has no committed publish.
pub fn footer_qual_classes(rel: &Relation<'_>) -> PgResult<Option<Vec<u8>>> {
    let Some(facts) = with_facts(rel, true)? else {
        return Ok(None);
    };
    Ok(facts.cols.map(|c| c.qual_class))
}

/// Per-column max character length (attnum-1 indexed; 0 = unknown);
/// `None` while the table has no committed publish.
pub fn footer_max_len(rel: &Relation<'_>) -> PgResult<Option<Vec<i32>>> {
    let Some(facts) = with_facts(rel, true)? else {
        return Ok(None);
    };
    Ok(facts.cols.map(|c| c.max_len))
}

/// Per-column planner-convention average datum widths (attnum-1 indexed;
/// 0 = unknown); `None` while the table has no committed publish.
pub fn footer_avg_widths(rel: &Relation<'_>) -> PgResult<Option<Vec<i32>>> {
    let Some(facts) = with_facts(rel, true)? else {
        return Ok(None);
    };
    Ok(facts.cols.map(|c| c.avg_width))
}

/// NEW-stats-2: the folded row total (Σ part rows) and per column
/// (attnum-1 indexed) its non-null count, `None` where unknown; `None`
/// while the table has no committed publish. A cost input only: the
/// counts are ingest-time and may lag the snapshot, so no reader takes
/// `nonnull == rows` as a proof that a column holds no NULL.
pub fn footer_nonnull(rel: &Relation<'_>) -> PgResult<Option<(u64, Vec<Option<u64>>)>> {
    let Some(facts) = with_facts(rel, true)? else {
        return Ok(None);
    };
    let rows = facts.rows;
    Ok(facts.cols.map(|c| (rows, c.nonnull)))
}

/// Granule count plus per-column codec kind and dictionary size (attnum-1
/// indexed); `None` while the table has no committed publish.
pub fn footer_codecs(rel: &Relation<'_>) -> PgResult<Option<CodecFacts>> {
    let Some(facts) = with_facts(rel, true)? else {
        return Ok(None);
    };
    Ok(facts.cols.map(|c| CodecFacts { granules: facts.granules, codec: c.codec, ncodes: c.ncodes }))
}

/// One column's zone facts — the planner's summary and, on demand, the
/// per-granule ranges; `None` while the table has no committed publish or
/// when the column carries no word zone key.
pub fn footer_zone_column(rel: &Relation<'_>, attno: u32) -> PgResult<Option<Arc<ZoneColumn>>> {
    let Some(facts) = with_facts(rel, true)? else {
        return Ok(None);
    };
    Ok(facts.zones.iter().find(|z| z.attno == attno).map(|z| Arc::clone(&z.col)))
}

/// What the committed parts attest about their order: the planner's `sorted_by` witness
/// (`reloptions::planner_sorted_attnos`).
#[derive(Debug, Clone)]
pub enum CommittedOrder {
    /// No committed part: the relation's committed rows are in every order.
    NoParts,
    /// Exactly one part: its SortKey section's entries (empty = an unordered part — a front
    /// segment's seal keeps heap order and writes `nkeys = 0`).
    OnePart(Arc<[SortKeyEntry]>),
    /// Two or more parts: the order is per part, nothing is promised across them.
    Many,
}

/// The committed parts' order witness. One part's SortKey section is read once per process and
/// kept with its generation (`catalog::Generation::sort_key`); a relation without generations
/// answers from the plan rig's published part count, as `part_count` does.
pub fn committed_order(rel: &Relation<'_>) -> PgResult<CommittedOrder> {
    let r = resolve(rel)?;
    let gen = match r.found {
        crate::catalog::Lookup::Gen(g) => g,
        crate::catalog::Lookup::Empty => {
            let parts = crate::catalog::override_facts(r.slot).map_or(0, |f| f.parts);
            return Ok(if parts > 1 { CommittedOrder::Many } else { CommittedOrder::NoParts });
        }
        crate::catalog::Lookup::Unknown => return Err(crate::catalog::not_loaded(rel, &r.dir)),
    };
    order_of(gen.parts.len(), || part_count(rel), || gen.sort_key(|part| Ok(part_sort_key(part)?.keys)))
}

/// [`committed_order`] over a visible generation of `nparts` parts. A generation without parts (F1's CREATE publishes
/// one) answers from `published`, the part count [`part_count`] serves, as the claim did before the SortKey witness:
/// the plan rig's published fold (`publish_facts`) for its empty template, else none. `one` (the part's SortKey
/// entries) is asked for one part only; two or more parts ask neither.
fn order_of(
    nparts: usize,
    published: impl FnOnce() -> PgResult<Option<u64>>,
    one: impl FnOnce() -> PgResult<Arc<[SortKeyEntry]>>,
) -> PgResult<CommittedOrder> {
    Ok(match nparts {
        0 => match published()?.unwrap_or(0) {
            p if p > 1 => CommittedOrder::Many,
            _ => CommittedOrder::NoParts,
        },
        1 => CommittedOrder::OnePart(one()?),
        _ => CommittedOrder::Many,
    })
}

/// One part's SortKey record (part grain, attno 0); an absent section reads as `nkeys = 0`
/// (OD-8: legal, unlicensed).
pub(crate) fn part_sort_key(part: &OpenPart) -> PgResult<SortKeyRecord> {
    let Some(idx) = part.find_section(SectionKind::SortKey, 0, 0) else { return Ok(SortKeyRecord::default()) };
    let body = meta_section_body(part, idx)?;
    SortKeyRecord::decode(&body).map_err(|e| {
        Box::new(PgError::error(format!("pgrcolumnar: SortKey record: {e:?}")).with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED))
    })
}

/// Every zone column the footer carries (one facts read; the ranges stay unexpanded until asked).
pub fn footer_zone_columns(rel: &Relation<'_>) -> PgResult<Vec<ZoneFacts>> {
    Ok(with_facts(rel, true)?.map(|f| f.zones).unwrap_or_default())
}

/// One ask's resolved identity: the catalog slot, the schema fingerprint, the directory, the
/// column schemas and the visible generation — from the process catalog, no file touched.
struct Resolved {
    slot: crate::catalog::RelSlot,
    dir: String,
    schemas: Vec<ColSchema>,
    found: crate::catalog::Lookup,
}

fn resolve(rel: &Relation<'_>) -> PgResult<Resolved> {
    crate::inval::ensure_inval_registered()?;
    let locator = rel.rd_locator.get();
    let schemas = crate::schema::col_schemas(rel)?;
    let fp = schema_fingerprint(&schemas);
    let dir = crate::dirpath::table_dir_path(locator, rel.rd_backend);
    let slot: crate::catalog::RelSlot = (locator.spcOid, locator.dbOid, locator.relNumber as u64);
    crate::catalog::record_relid(rel.rd_id, slot);
    // committed-or-own-transaction, the visibility the AM's ANALYZE scan face runs under
    let check = SnapshotCommitCheck::new(None);
    let found = crate::catalog::resolve(rel, slot, &dir, &check)?;
    check.take_error()?;
    if let crate::catalog::Lookup::Gen(g) = &found {
        // FASTADD: the generation's schema is the reader's, or an older one the book describes (its facts are by
        // attno; a column its parts lack serves unknown). D2189: a column dropped with no ALTER publish
        // leaves a stamp no generation describes; the book learns it first, or the read fails closed
        if g.schema_fp != fp {
            crate::schema::book_dropped_stamp(rel, g.schema_fp)?;
        }
        crate::fastadd::presence(&schemas, fp, g.schema_fp)?;
    }
    Ok(Resolved { slot, dir, schemas, found })
}

/// The plan rig's external publish (`planner_facts`): install `facts` as the relation's fold —
/// on its visible generation when it has one, as the override of a relation without parts.
pub(crate) fn publish_facts(rel: &Relation<'_>, mut facts: FooterFacts) -> PgResult<()> {
    let r = resolve(rel)?;
    match &r.found {
        crate::catalog::Lookup::Gen(g) => {
            facts.gen = g.gen;
            g.install_facts(Arc::new(facts.clone())); // alloc: Cold charged the generation's charge (catalog Generation::held_bytes)
            crate::session::with_footer_facts(|m| {
                m.insert(r.slot.2, SessionFacts::of(g, facts));
            });
        }
        crate::catalog::Lookup::Empty => {
            facts.gen = 0;
            crate::catalog::set_override_facts(r.slot, Arc::new(facts)); // alloc: Cold charged the entry's charge (catalog RelEntry::held_bytes)
            // a relation without a generation serves the override, never the session's fold
            crate::session::with_footer_facts(|m| m.remove(&r.slot.2));
        }
        crate::catalog::Lookup::Unknown => return Err(crate::catalog::not_loaded(rel, &r.dir)),
    }
    Ok(())
}

/// The deep fold, for `planner_facts`' dump face.
pub(crate) fn deep_facts(rel: &Relation<'_>) -> PgResult<Option<FooterFacts>> {
    with_facts(rel, true)
}

/// The facts of the relation's visible generation from the process catalog: shallow asks take
/// the manifest sums; deep asks the generation's facts (served once per process from its image
/// under this catalog's zone rule). Nothing here opens, lists or reads a file.
fn with_facts(rel: &Relation<'_>, deep: bool) -> PgResult<Option<FooterFacts>> {
    let r = resolve(rel)?;
    let gen = match r.found {
        crate::catalog::Lookup::Gen(g) => g,
        crate::catalog::Lookup::Empty => return Ok(crate::catalog::override_facts(r.slot).map(|f| (*f).clone())),
        crate::catalog::Lookup::Unknown => return Err(crate::catalog::not_loaded(rel, &r.dir)),
    };
    let relfilenumber = r.slot.2;
    let cached = crate::session::with_footer_facts(|m| m.get(&relfilenumber).and_then(|f| f.for_gen(&gen)).cloned());
    if let Some(f) = &cached {
        if !deep || f.ndv.is_some() {
            return Ok(cached);
        }
    }
    let sums = ManifestSums::of(&gen.manifest);
    if !deep {
        let (ndv, cols, zones) = match cached {
            Some(f) => (f.ndv, f.cols, f.zones),
            None => (None, None, Vec::new()),
        };
        let facts = FooterFacts { gen: gen.gen, rows: sums.rows, bytes: sums.bytes, granules: sums.granules, parts: sums.parts, ndv, cols, zones };
        crate::session::with_footer_facts(|m| {
            m.insert(relfilenumber, SessionFacts::of(&gen, facts.clone()));
        });
        return Ok(Some(facts));
    }
    let schemas = &r.schemas;
    // Dense by attnum: a dropped column's gap serves zeros.
    let attnos: Vec<u32> = (1..=schemas.last().map_or(0, |s| s.attno)).collect();
    let class_of = |attno: u32| schemas.iter().find(|s| s.attno == attno).map_or(u8::MAX, |s| s.class.id());
    let zone_rule = |c: &ColFacts| schemas.iter().find(|s| s.attno == c.attno).is_some_and(word_zone_column);
    let Some(facts) = gen.facts(|image| served_to_footer(image, &sums, &attnos, &class_of, &zone_rule))? else {
        return Err(Box::new(PgError::error(format!( // alloc: Cold
            "pgrcolumnar: the planner facts of {} generation {} were not loaded at server start",
            r.dir, gen.gen
        ))));
    };
    let facts = (*facts).clone();
    crate::session::with_footer_facts(|m| {
        m.insert(relfilenumber, SessionFacts::of(&gen, facts.clone()));
    });
    Ok(Some(facts))
}

/// The table-grain sums straight from the manifest's part records.
struct ManifestSums {
    rows: u64,
    bytes: u64,
    granules: u64,
    parts: u64,
}

impl ManifestSums {
    fn of(m: &Manifest) -> ManifestSums {
        let mut s = ManifestSums { rows: 0, bytes: 0, granules: 0, parts: m.parts.len() as u64 };
        for rec in &m.parts {
            s.rows += rec.rows;
            s.bytes += rec.file_len;
            s.granules += u64::from(rec.granule_count);
        }
        s
    }
}

pub fn serve_generation_without_catalog(gen: &crate::catalog::Generation) -> PgResult<Option<FooterFacts>> {
    let Some(image) = gen.image() else { return Ok(None) };
    if image.cols.iter().any(|c| c.flags & pgrc_format::facts::COLF_ZONES != 0 && zone_rule_without_catalog(c).is_none()) {
        return Ok(None);
    }
    let sums = ManifestSums::of(&gen.manifest);
    let attnos: Vec<u32> = (1..=image.cols.iter().map(|c| c.attno).max().unwrap_or(0)).collect(); // alloc: Cold
    served_to_footer(&image, &sums, &attnos, &|_| u8::MAX, &|c| zone_rule_without_catalog(c).unwrap_or(false)).map(Some)
}

/// The fold over a catalog generation's open parts (the startup load's backfill of a
/// generation without its facts file): the same fold the publisher runs, in manifest order.
pub fn fold_generation(gen: &crate::catalog::Generation) -> PgResult<FactsImage> {
    let mut acc = FactsAcc::new();
    acc.fold_open_parts(gen.parts.iter().zip(&gen.manifest.parts).map(|(p, rec)| (&*p.part, rec.file_len))).map_err(crate::read_error)?;
    acc.into_image(gen.gen, gen.publisher_fxid, gen.schema_fp).map_err(crate::format_error)
}

/// A column whose zone key is the datum word itself (the planner lowers a
/// constant to it unaided): `SignedWord`, `UnsignedWordSmall`, `Bool`.
fn word_zone_column(s: &ColSchema) -> bool {
    MetaProfile::derive(s.class, s.collation_class, s.semantics)
        .is_ok_and(|p| matches!(p.key, KeyDerivation::SignedWord | KeyDerivation::UnsignedWordSmall | KeyDerivation::Bool))
}

/// The served image as the planner's shape: `attnos` are the columns to
/// serve (attnum-1 indexed vectors), `class_of` the storage class of a
/// column the image does not carry (its codec defaults to the class's),
/// `zone_rule` which columns' zone ranges are served.
fn served_to_footer(
    image: &FactsImage,
    sums: &ManifestSums,
    attnos: &[u32],
    class_of: &dyn Fn(u32) -> u8,
    zone_rule: &dyn Fn(&ColFacts) -> bool,
) -> PgResult<FooterFacts> {
    let served: Vec<ServedCol> = serve(image, zone_rule).map_err(|e| {
        Box::new(PgError::error(format!("pgrcolumnar: facts image: {e}")).with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED)) // alloc: Cold
    })?;
    let n = attnos.len();
    let mut ndv = crate::alloc::try_vec(n, "footer")?;
    let mut cols = ColCostFacts {
        disk_bytes: crate::alloc::try_vec(n, "footer")?,
        avg_width: crate::alloc::try_vec(n, "footer")?,
        qual_class: crate::alloc::try_vec(n, "footer")?,
        max_len: crate::alloc::try_vec(n, "footer")?,
        nonnull: crate::alloc::try_vec(n, "footer")?,
        codec: crate::alloc::try_vec(n, "footer")?,
        ncodes: crate::alloc::try_vec(n, "footer")?,
    };
    let mut zones = Vec::new();
    for &attno in attnos {
        match served.binary_search_by_key(&attno, |c| c.attno).ok().map(|i| &served[i]) {
            Some(c) => {
                ndv.push(c.ndv);
                cols.disk_bytes.push(c.disk_bytes);
                cols.avg_width.push(c.avg_width);
                cols.qual_class.push(c.qual_class);
                cols.max_len.push(c.max_len);
                cols.nonnull.push(c.nonnull);
                cols.codec.push(c.codec);
                cols.ncodes.push(c.ncodes);
                if let Some(z) = &c.zones {
                    zones.push(ZoneFacts::new(attno, z.clone())?);
                }
            }
            None => {
                ndv.push(0);
                cols.disk_bytes.push(0);
                cols.avg_width.push(0);
                cols.qual_class.push(0);
                cols.max_len.push(0);
                cols.nonnull.push(None);
                cols.codec.push(class_codec(class_of(attno)));
                cols.ncodes.push(0);
            }
        }
    }
    Ok(FooterFacts { gen: image.header.gen, rows: sums.rows, bytes: sums.bytes, granules: sums.granules, parts: sums.parts, ndv: Some(ndv), cols: Some(cols), zones })
}

/// Slice + decode the ONE part-grain record at the tail of a Stats section
/// body (ANALYZE's per-part read; the fold's own copy is
/// `pgrc_write::facts::part_stats_record`).
pub(crate) fn part_stats_record(body: &[u8], granules: u32, bands: u32, attno: u32) -> PgResult<StatsRecord> {
    pgrc_write::facts::part_stats_record(body, granules, bands, attno).map_err(|e| {
        Box::<PgError>::new( // alloc: Cold
            PgError::error(format!("pgrcolumnar: Stats section for column {attno}: {e}")) // alloc: Cold
                .with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED),
        )
    })
}

/// Fetch one META-PLANE section body, unwrapping the SB-6 zstd meta
/// envelope when `SECTIONF_META_ZSTD` marks the stored form (ANALYZE's
/// per-part reads).
pub(crate) fn meta_section_body(part: &OpenPart, idx: usize) -> PgResult<Vec<u8>> {
    let flags = part.sections()[idx].flags;
    let raw = part.section_bytes_uncached(idx).map_err(crate::read_error)?;
    if flags & pgrc_format::part::SECTIONF_META_ZSTD != 0 {
        pgrc_codec::wrapper::meta_unwrap_body(raw.bytes())
            .map_err(|e| Box::new(PgError::error(format!("pgrcolumnar meta section unwrap: {e}")))) // alloc: Cold
    } else {
        Ok(raw.bytes().to_vec())
    }
}

#[cfg(test)]
mod order_tests {
    use std::sync::Arc;

    use pgrc_format::sortkey::SortKeyEntry;
    use types_error::PgResult;

    use super::{order_of, CommittedOrder, PARTS_UNKNOWN};

    fn never<T>() -> PgResult<T> {
        panic!("not asked for this part count")
    }

    /// A generation without parts (F1's CREATE publishes one; the plan rig's empty template is one) answers from the
    /// published part count: more than one part promises nothing across them, as a relation without generations does.
    #[test]
    fn a_generation_without_parts_answers_from_the_published_count() {
        let zero = |published: Option<u64>| order_of(0, || Ok(published), never).expect("order");
        assert!(matches!(zero(None), CommittedOrder::NoParts));
        assert!(matches!(zero(Some(0)), CommittedOrder::NoParts), "a served fold of no parts");
        assert!(matches!(zero(Some(1)), CommittedOrder::NoParts));
        assert!(matches!(zero(Some(2)), CommittedOrder::Many), "the rig's 100 M template: many parts");
        assert!(matches!(zero(Some(PARTS_UNKNOWN)), CommittedOrder::Many));
    }

    /// One part or more: the witness is the parts themselves, the published count is never asked.
    #[test]
    fn parts_answer_without_the_published_count() {
        let keys: Arc<[SortKeyEntry]> = Arc::from(Vec::new());
        let one = order_of(1, never, || Ok(Arc::clone(&keys))).expect("order");
        assert!(matches!(one, CommittedOrder::OnePart(k) if Arc::ptr_eq(&k, &keys)));
        assert!(matches!(order_of(2, never, never).expect("order"), CommittedOrder::Many));
        assert!(matches!(order_of(17, never, never).expect("order"), CommittedOrder::Many));
    }
}

#[cfg(test)]
mod genid_tests {
    use super::*;
    use pgrc_format::manifest::{Manifest, ManifestHeader};

    fn generation(gen: u64) -> Arc<crate::catalog::Generation> {
        let header = ManifestHeader { gen, prev_gen: 0, publisher_fxid: 10, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: 0, next_part_no: 0, flags: 0, reserved: 0 };
        Arc::new(crate::catalog::Generation::detached(Manifest::new(header, Vec::new())).expect("generation"))
    }

    /// GENID / D1128: a session's facts serve only the generation they were folded from, never another content under
    /// its number (a same-transaction TRUNCATE's, a rolled-back publish's successor's).
    #[test]
    fn session_facts_serve_only_their_own_generation() {
        let g = generation(2);
        let facts = FooterFacts { gen: 2, rows: 7, bytes: 0, granules: 0, parts: 1, ndv: None, cols: None, zones: Vec::new() };
        let s = SessionFacts::of(&g, facts);
        assert_eq!(s.for_gen(&g).map(|f| f.rows), Some(7));
        assert!(s.for_gen(&generation(2)).is_none(), "the same number, another generation");
        drop(g);
        assert!(s.for_gen(&generation(2)).is_none(), "the Weak keeps the allocation: no later generation takes its address");
    }
}

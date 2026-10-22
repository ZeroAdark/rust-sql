//! COPY routing into the frozen M3-D writer face (`TableWriter::append_row`
//! → `finish` → `publish`), with the per-(fxid,cid) `WriterRegistry`
//! discipline and the O-7 lazy directory creation.
//!
//! Reachability (O-M3-1a): `table_tuple_insert` / `table_multi_insert` on a
//! pgrcolumnar relation are reachable from COPY FROM and the bulk
//! receivers (CTAS / matview refresh) ONLY — trickle DML refuses typed at
//! the ModifyTable gate before any AM call. All of them end in
//! `table_finish_bulk_insert` → [`finish_bulk`], the publish point.
//!
//! ## Election posture (the M3-A2 full-registry wiring, M3-J)
//!
//! `SealEnv.sources` carries the full-registry [`CodecCandidates`] source
//! and `SealEnv.resolver` is [`CodecResolver`] (PR #474's contract): parts
//! written through COPY carry real BYTE_FOR/ALP/ALP_RD/DELTA_FOR/
//! DICT_CODES/PACKED_NUMERIC/LZ4 elections and seal-verify resolves the
//! SAME vtables a conformant reader will. Per-column postures are derived
//! from catalog facts in [`codec_candidates`] (numeric unlocks the
//! PACKED_NUMERIC arm; dict election only where the schema-derived
//! collation class is `C`, so code-eq == value-eq — the byte-semantics
//! lattice `DICT_EXEC` additionally requires the seal-time zero-null
//! proof). The candidates are cached per relfilenumber in the session
//! block (a pure function of the relation schema; purged with the writers
//! at transaction end).
//!
//! ## Durability
//!
//! A publish is WAL-first through the writer's sink (delta v2 design §7), `sink_for(rel)`: the commit record's
//! flush is the #253 fence. An unlogged table logs nothing and registers its files for the checkpoint; a temp
//! table, or one created under `wal_level=minimal` in this transaction, gets `NoWal` (the commit fsyncs the latter).

use std::cell::Cell;
use std::sync::Arc;

use pgrc_write::dict::TextSemantics;
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, DictPolicy};
use pgrc_write::ingest::{NoExternalDetoast, RawDatum};
use pgrc_write::par::{NoDetoastProvider, ParProviders, RealVfsProvider, RowChunk, WalProvider};
use pgrc_write::seal::{CodecResolver, ColumnPool};
use pgrc_write::shred_jsonb::JsonbShredSource;
use pgrc_write::structural::StructuralPolicy;
use pgrc_write::walsink::{NoWal, WalSink};
use pgrc_write::writer::{PartCutPolicy, SealEnv, SealPool, SubxactEvidence, TableWriter, TxnStamp};
use pgrc_write::wvfs::RealVfs;
use pgrc_format::manifest::{SegState, SegmentRecord};
use pgrc_format::relopt::ShredOptions;
use types_error::{PgError, PgResult};
use types_rel::Relation;
use types_tuple::varatt::{self, varsize_any};

use crate::probe::ClogTxnProbe;
use pgrc_write::publish::{BaseStats, PublishOutcome, TxnProbe, TxnVerdict};
use crate::{session, write_error};

/// The dict NDV cap: 2^24, the widest packed code width, so on real data (a
/// part holds ~200 k rows) it never fires and the election is decided by the
/// existing payload/size test alone (ruling 2026-09-14). The previous 100 k
/// was a placeholder "LowCardinality-cliff anchor" marked re-measure; the
/// reseal of URL part 11 at NDV 130,305 measured the dictionary form 10.2 %
/// smaller, i.e. the cliff was the cap, not the data. Nothing sizes from this
/// constant (reader-side sizes come from the part's own streams).
const DICT_NDV_CAP: u64 = 1 << 24;
const DICT_NDV_CAP_GUC: &str = "pgrust.pgrc_dict_ndv_cap";

/// The cap in force: `pgrust.pgrc_dict_ndv_cap` (a placeholder custom option; `SET` /
/// `postgresql.conf` / `-c`), else `PGRUST_PGRC_DICT_NDV_CAP`, else [`DICT_NDV_CAP`]. Read at
/// every posture build (the postures are cached per relfilenumber in the session block).
pub(crate) fn dict_ndv_cap() -> u64 {
    let from_guc = if guc_seams::get_config_option_missing_ok::is_installed() {
        guc_seams::get_config_option_missing_ok::call(DICT_NDV_CAP_GUC).ok().flatten().and_then(|v| v.trim().parse::<u64>().ok())
    } else {
        None
    };
    from_guc.or_else(|| std::env::var("PGRUST_PGRC_DICT_NDV_CAP").ok().and_then(|v| v.trim().parse::<u64>().ok())).unwrap_or(DICT_NDV_CAP)
}

/// Full-registry election postures from catalog facts (pure function of
/// the relation schema; cached per relfilenumber in the session block).
///
/// - `cold: true` is the STORAGE posture, every column (CMP-B election
///   widening, ruled 2026-08-10 — `docs/design/pgrc-compression.md` §3
///   CMP-B row): pgrc sealed parts ARE the frozen storage tier, so the
///   cold/size arm (DELTA_FOR — the frozen-matrix "cold/size election
///   only" encoding) competes on EVERY int-family stream and wins exactly
///   where its exact bytes clear the standing ≥10% law (elections change
///   layout, never answers; scan cost is priced by the arc's P1 bar).
/// - `numeric`: exactly the numeric-typed columns (unlocks PACKED_NUMERIC).
/// - `dict`: text-family varlena columns under a deterministic collation
///   (class `C` or `OtherDeterministic`): equal bytes are equal values under
///   every deterministic collation, so code-eq == value-eq holds. Code ORDER
///   is byte order, which is value order only under `C`; readers gate every
///   order use on the column's collation. `exec_ok` rides the same proof;
///   seal additionally gates `DICT_EXEC` on the zero-null witness.
/// - `fused` stays false: the M3 scan consumes flat decode and the S4
///   verdict (FFOR wins fused, loses flat 1.9–2.7×) STANDS until the
///   CMP-B oracle census overturns it with evidence — the census reports
///   FFOR's marginal size attribution for exactly that adjudication.
///   (The wrapper offer no longer rides any posture flag: O-CMP-3(a),
///   landed by CMP-A, offers it on every election.)
pub(crate) fn dict_eligible(
    text_family: bool,
    class: pgrc_format::class::StorageClass,
    coll: pgrc_format::class::CollationClass,
) -> bool {
    text_family
        && matches!(class, pgrc_format::class::StorageClass::VarlenaVerbatim)
        && coll != pgrc_format::class::CollationClass::Nondeterministic
}

pub(crate) fn codec_candidates(rel: &Relation<'_>) -> PgResult<CodecCandidates> {
    use types_core::catalog::{BPCHAROID, BYTEAOID, NUMERICOID, TEXTOID, VARCHAROID};
    let schemas = crate::schema::col_schemas(rel)?;
    let atts = &rel.rd_att.attrs;
    /// The bank/storage posture: cold size arms offered by default.
    const STORAGE_POSTURE: ColumnPosture = ColumnPosture {
        fused: false,
        cold: true,
        numeric: false,
        dict: None,
    };
    let mut cands = CodecCandidates::new(STORAGE_POSTURE);
    for s in &schemas {
        let att = &atts[(s.attno - 1) as usize];
        let mut p = STORAGE_POSTURE;
        if att.atttypid == NUMERICOID {
            p.numeric = true;
        }
        let text_family = matches!(att.atttypid, TEXTOID | VARCHAROID | BPCHAROID | BYTEAOID);
        // [sqe-bpchar] code-eq == value-eq holds for bpchar ONLY under the
        // uniform-padding witness (atttypmod >= 5 pads every stored image
        // to the declared char width). BARE bpchar stores unpadded: 'ab'
        // and 'ab ' are byte-distinct dict codes that compare EQUAL under
        // bpchareq — asserting DICT_EXEC there would be the wrong-answer
        // class the §7 lattice exists to prevent. (Latent before the
        // pad-aware lane: the seam refused every bpchar shape, so the
        // flag was never consumed; pinned now that group-on-codes rides.)
        let bpchar_unpadded = att.atttypid == BPCHAROID && att.atttypmod < 5;
        if dict_eligible(text_family, s.class, s.collation_class) {
            // TEXT/VARCHAR under a UTF-8 database CLAIM Utf8Chars — the
            // claim is catalog-founded (PG's input functions verify server
            // encoding on every value), but the election VERIFIES it per
            // part before any char-length fact rides it
            // (`resolve_text_policy`, elect.rs) — verified, never trusted. The encoding gate is REQUIRED, not
            // belt-and-braces: in a non-UTF8 database (e.g. LATIN1) the
            // bytes can be accidentally UTF-8-valid while the true char
            // semantics are the server encoding's — lead-byte counts
            // would be wrong facts. bytea keeps byte semantics by type;
            // bpchar stays BytesOnly (space-padded images — its char
            // facts are not the length() surface, spec §7 lattice).
            // pgrc format 2.2: the char-length fact is a REAL UTF-8 count on every
            // text/varchar column of a UTF-8 database (the packed DictIndex
            // stores `byte_len − char_len`); the claim is still verified
            // per part.
            let utf8_text = matches!(att.atttypid, TEXTOID | VARCHAROID)
                && mbutils::GetDatabaseEncoding() == wchar::PG_UTF8;
            p.dict = Some(DictPolicy {
                ndv_cap: dict_ndv_cap(),
                // Bare bpchar keeps storage-grade dedup only (see above).
                exec_ok: !bpchar_unpadded,
                sem: if utf8_text {
                    TextSemantics::Utf8Chars
                } else {
                    TextSemantics::BytesOnly
                },
            });
        }
        if p != STORAGE_POSTURE {
            cands = cands.with_column(s.attno, 0, p);
        }
        // [json-rung1] jsonb DERIVED shred lanes (path_ord ≥ 1): offer
        // the dict arm to the TEXT lanes. A text lane's values are the
        // verbatim jsonb string bytes under the lane lattice's C-class
        // byte law (shredlane::col_schema: TextCollated / CollationClass
        // ::C) — code-eq IS value-eq, so `exec_ok` holds; the seal still
        // gates DICT_EXEC on the zero-null proof. BytesOnly claims no
        // char facts. Byval lanes (NumericFs mantissa words) never see
        // the dict arm (the election is class-gated to VarlenaVerbatim);
        // the image column itself keeps dict: None (jsonb-as-value:
        // code-eq ≠ value-eq, the spec §7 lattice).
        if att.atttypid == types_core::catalog::JSONBOID
            && matches!(s.class, pgrc_format::class::StorageClass::VarlenaVerbatim)
        {
            let mut lp = STORAGE_POSTURE;
            lp.dict = Some(DictPolicy {
                ndv_cap: dict_ndv_cap(),
                exec_ok: true,
                sem: TextSemantics::BytesOnly,
            });
            cands = cands.with_lane_default(s.attno, lp);
        }
    }
    Ok(cands)
}

/// Run `f` with the cached (or freshly derived) candidates for `rel`.
fn with_rel_candidates<R>(
    rel: &Relation<'_>,
    relfilenumber: u64,
    f: impl FnOnce(&CodecCandidates) -> R,
) -> PgResult<R> {
    let have = session::with_candidates(|m| m.contains_key(&relfilenumber));
    if !have {
        let built = codec_candidates(rel)?;
        session::with_candidates(|m| m.insert(relfilenumber, built));
    }
    Ok(session::with_candidates(|m| {
        f(m.get(&relfilenumber).expect("just inserted"))
    }))
}

fn current_stamp() -> PgResult<TxnStamp> {
    // The CURRENT (sub)transaction's full xid, not the top's: a publish
    // from inside a savepoint must become invisible when that savepoint
    // rolls back even though the top transaction commits — clog carries the
    // subabort verdict only on the SUBXACT xid (the DDL battery's
    // savepoint leg witnessed the top-fxid form leaking rows).
    let fxid = xact::GetCurrentFullTransactionId()?;
    let cid = xact_seams::get_current_command_id::call(false)?;
    Ok(TxnStamp {
        fxid: fxid.value,
        cid,
    })
}

fn subxact_evidence(rel: &Relation<'_>) -> SubxactEvidence {
    let cur = xact_seams::get_current_sub_transaction_id::call();
    SubxactEvidence {
        cur_subxact_valid: cur != 0,
        rel_created_in_cur_subxact: rel.rd_createSubid.get() == cur && cur != 0,
        new_relfilelocator_in_cur_subxact: rel.rd_newRelfilelocatorSubid.get() == cur && cur != 0,
    }
}

fn open_writer(rel: &Relation<'_>, stamp: TxnStamp) -> PgResult<TableWriter> {
    let locator = rel.rd_locator.get();
    let dir = crate::dirpath::table_dir_path(locator, rel.rd_backend);
    if !crate::dirpath::dir_exists(&dir)? {
        crate::refuse_v1_format(rel)?;
        // P304: the directory is made with the relfilenumber, so a write that finds none meets damage: C's 58P01 for a
        // relation file that is gone, as the read (`catalog::resolve`)
        return Err(Box::new( // alloc: Cold
            types_error::PgError::error(format!("could not open directory \"{dir}\": No such file or directory")) // alloc: Cold
                .with_sqlstate(types_error::ERRCODE_UNDEFINED_FILE),
        ));
    }
    let schemas = crate::schema::col_schemas(rel)?;
    let mut w = TableWriter::open(
        dir,
        schemas,
        locator.spcOid,
        locator.dbOid,
        locator.relNumber as u64,
        stamp,
        &subxact_evidence(rel),
        PartCutPolicy::default(),
    )
    .map_err(write_error)?
    .with_sink(pgrc_wal_seams::sink_for::call(rel));
    // The declared `sorted_by` key: the seal sorts every part by it
    // (pgrc_write::seal's cluster sort) and the parallel path inherits it
    // through par_facts, so serial and parallel COPY seal the same order.
    let key = crate::reloptions::cluster_key(rel);
    // IXBULKSORT: the sort moves the rows off the IDs they were handed, which their index entries need ([`place`])
    if rel.rd_rel.relhasindex && !key.is_empty() {
        w.keep_placements();
    }
    w.set_structural(StructuralPolicy::new().with_cluster_key(key));
    w.set_owner(load_owner());
    Ok(w)
}

/// CONVERT W3-2b (main Q1 GO 2026-10-14): the loading statement's memory owner, read ONCE per load on the leader where
/// its writer opens ([`open_writer`]) and passed down by value from there: the writer's columns, the parallel
/// session's chunks and parts, the seal's buffers. COPY runs as a utility portal (no PortalStart owner), so this is the
/// statement the backend's main loop began (`memledger::statement_begin`). Anything else (no statement running: a
/// session's or a holder's context, the compaction worker) charges nothing: the bytes stay on the account as before.
pub fn load_owner() -> Option<memledger::Owner> {
    match memledger::charge::context().0 {
        o @ (memledger::Owner::Statement { .. } | memledger::Owner::Portal { .. }) => Some(o),
        _ => None,
    }
}

/// IX0 L3: a bulk row's ID. IXBULKSORT: `handed` = the ID of the row's arrival position, which its part's seal moves
/// (a `sorted_by` table with indexes): the row's own is [`place`]d once its part is cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowId {
    pub id: u64,
    pub handed: bool,
}

/// IXBULKSORT: a cut part of a bulk writer that keeps placements (`pgrc_write::writer::Placed`), held until each of its
/// rows was [`place`]d or its writer's (sub)transaction is gone; the transaction end drops the rest
/// (`session::purge_writers`).
pub struct Placement {
    relfilenumber: u64,
    fxid: u64,
    part: pgrc_write::writer::Placed,
    placed: u64,
}

impl Placement {
    /// D4537: the placement's heap bytes (its part's ID ranges and permutation).
    pub(crate) fn heap_bytes(&self) -> usize {
        self.part.heap_bytes()
    }
}

fn hold_placements(relfilenumber: u64, fxid: u64, parts: Vec<pgrc_write::writer::Placed>) {
    if !parts.is_empty() {
        session::with_placed(|p| p.extend(parts.into_iter().map(|part| Placement { relfilenumber, fxid, part, placed: 0 })));
    }
}

/// IXBULKSORT: a cut part of `rel` holds rows not yet [`place`]d (a part of an aborted subtransaction's writer is
/// dropped here: its rows were discarded with it).
pub fn placements_waiting(rel: &Relation<'_>) -> bool {
    let relfilenumber = rel.rd_locator.get().relNumber as u64;
    session::with_placed(|p| {
        p.retain(|e| live(TxnStamp { fxid: e.fxid, cid: 0 }));
        p.iter().any(|e| e.relfilenumber == relfilenumber)
    })
}

/// IXBULKSORT: the ID of `rel`'s bulk row handed `handed` ([`RowId`]), once its part is cut; `None` while the row's part
/// is buffered. Each row is placed once: a part whose rows all were is forgotten.
pub fn place(rel: &Relation<'_>, handed: u64) -> Option<u64> {
    let relfilenumber = rel.rd_locator.get().relNumber as u64;
    session::with_placed(|p| {
        let (i, id) = p.iter().enumerate().filter(|(_, e)| e.relfilenumber == relfilenumber).find_map(|(i, e)| e.part.place(handed).map(|id| (i, id)))?;
        p[i].placed += 1;
        if p[i].placed >= p[i].part.rows() {
            p.remove(i);
        }
        Some(id)
    })
}

/// Validate a borrowed inline varlena image against the bytes that are
/// actually backed at its location. `avail` is the remaining bytes of the
/// materialized tuple from the datum's pointer onward — an INDEPENDENT
/// readable-byte witness (derived from the tuple's `t_len`, never from the
/// varlena header itself), so a crafted 4B header claiming ~1 GiB of process
/// memory is refused here instead of sizing an out-of-bounds read. Returns
/// the validated total length (header included). External (1B_E) pointers
/// must be screened by the caller before this is reached.
fn checked_varlena_len(avail: &[u8]) -> PgResult<usize> {
    let b0 = *avail
        .first()
        .ok_or_else(|| crate::corrupt("empty varlena image in tuple"))?;
    // Header footprint: a 1B short header (low bit set) vs a 4B long header.
    // `varsize_any` reads only these header bytes for the non-external forms,
    // so guarding it here keeps even the header read in bounds.
    let hdr = if b0 & 0x01 == 0x01 {
        varatt::VARHDRSZ_SHORT
    } else {
        varatt::VARHDRSZ
    };
    if avail.len() < hdr {
        return Err(crate::corrupt(
            "varlena header truncated within its tuple image",
        ));
    }
    // SAFETY: `avail` is backed for at least `hdr` bytes (checked above), and
    // the caller has screened the external (1B_E) form, so `varsize_any`
    // reads only in-bounds header bytes of a 1B/4B inline form.
    let len = unsafe { varsize_any(avail.as_ptr()) };
    if len < hdr || len > avail.len() {
        // The header claims more (or fewer) bytes than the tuple backs: a
        // crafted/corrupt on-page header. Refuse before forming the slice.
        return Err(crate::corrupt(
            "varlena length exceeds the bytes backed by its tuple image",
        ));
    }
    Ok(len)
}

/// The readable window of a by-reference datum within the materialized tuple
/// image: the bytes from `p` to the end of the image. Returns a typed
/// corruption error when the datum points outside the image (a corrupt or
/// aliased offset), or `None` when no image witness was supplied.
fn datum_window<'i>(p: *const u8, tuple_image: Option<&'i [u8]>) -> PgResult<Option<&'i [u8]>> {
    let Some(img) = tuple_image else {
        return Ok(None);
    };
    let base = img.as_ptr() as usize;
    let end = base + img.len();
    let pa = p as usize;
    if pa < base || pa >= end {
        return Err(crate::corrupt(
            "by-reference datum points outside its tuple image",
        ));
    }
    Ok(Some(&img[pa - base..]))
}

/// Build one writer-face row image from a deformed slot row. By-reference
/// datums are passed as raw images; varlena images may arrive in any inline
/// toast form (the writer normalizes; external pointers refuse typed —
/// unreachable from COPY, whose input datums are always inline).
///
/// `tuple_image`, when supplied, is the flat backing bytes of the caller's
/// MATERIALIZED physical tuple (see [`exectuples::slot_materialized_image`]):
/// after materialize + deform every by-reference datum points INTO this
/// image, so each datum's header-claimed size is bounded against the bytes
/// actually backed there. This is the independent witness the OOB/info-leak
/// class requires — the datum's own on-tuple varlena header is UNTRUSTED for
/// sizing. `None` means the caller could not supply a single flat image (a
/// Virtual slot, whose materialize pass owns its own bounding — see the
/// executor materialize lane); the legacy header-sized behavior is preserved
/// there.
///
/// SAFETY of the pointer reads: the slot is materialized/deformed by the
/// caller; byref datums point at live images for the duration of the call
/// (the writer copies what it keeps).
///
/// Public since M4-S3b: the engine parallel-COPY driver (copy_cmd ->
/// pgrc_ingest_par) builds its capture rows through EXACTLY this
/// conversion — one row image law for the serial and parallel arms.
pub fn raw_row<'a>(
    rel: &Relation<'_>,
    values: &'a [datum::Datum],
    isnull: &'a [bool],
    tuple_image: Option<&[u8]>,
) -> PgResult<Vec<RawDatum<'a>>> {
    let atts = &rel.rd_att.attrs;
    if values.len() < atts.len() || isnull.len() < atts.len() {
        return Err(Box::new(PgError::error( // alloc: Cold
            "pgrcolumnar: slot narrower than relation descriptor".to_string(),
        )));
    }
    let mut out = Vec::with_capacity(atts.len());
    // A dropped attribute has no column (`col_schemas`).
    for (i, att) in atts.iter().enumerate().filter(|(_, a)| !a.attisdropped) {
        out.push(if isnull[i] { RawDatum::Null } else { raw_att(att, &values[i], tuple_image)? });
    }
    Ok(out)
}

/// One non-NULL attribute of [`raw_row`]: `att`'s datum `value`, bounded against `tuple_image` as there. Also the
/// column feed of a compaction (K1-MEM), whose decoded datums are never external.
pub(crate) fn raw_att<'a>(
    att: &types_tuple::tupdesc::FormData_pg_attribute,
    value: &'a datum::Datum,
    tuple_image: Option<&[u8]>,
) -> PgResult<RawDatum<'a>> {
    if att.attbyval {
        Ok(RawDatum::Word(value.as_u64()))
    } else if att.attlen > 0 {
        let p = value.as_u64() as *const u8;
        let n = att.attlen as usize;
        // Fixed-length byref: the catalog attlen must fit the tuple image.
        if let Some(avail) = datum_window(p, tuple_image)? {
            if avail.len() < n {
                return Err(crate::corrupt(
                    "fixed-length datum overruns its tuple image",
                ));
            }
        }
        // SAFETY: fixed-length byref datum — attlen readable bytes,
        // bounded above against the tuple image when one was supplied.
        let img = unsafe { core::slice::from_raw_parts(p, n) };
        Ok(RawDatum::Bytes(img))
    } else if att.attlen == -1 {
        let p = value.as_u64() as *const u8;
        let avail = datum_window(p, tuple_image)?;
        // Screen the external (1B_E) form first: COPY never produces one,
        // and its header sizing (`varsize_external`) is a different tag.
        let b0 = match avail {
            Some(w) => w[0], // `window` guarantees >= 1 backed byte
            // SAFETY: no independent witness — legacy header-readable
            // trust for the Virtual-slot path (see the doc comment).
            None => unsafe { *p },
        };
        if b0 == 0x01 {
            // 1B_E external/indirect pointer: COPY never produces one;
            // refuse typed rather than size an unknown tag.
            return Err(crate::unsupported(
                "ingesting externally-toasted datums (detoast capability arrives with the DML sink)",
            ));
        }
        let len = match avail {
            // Validated against the tuple's own byte extent (t_len), the
            // independent witness that breaks the writer's circular
            // header-derived bound.
            Some(w) => checked_varlena_len(w)?,
            // SAFETY: legacy path — header-sized inline image length.
            None => unsafe { varsize_any(p) },
        };
        // SAFETY: inline varlena image — `len` readable bytes, proven
        // in-bounds by `checked_varlena_len` when a witness was supplied.
        let img = unsafe { core::slice::from_raw_parts(p, len) };
        Ok(RawDatum::Bytes(img))
    } else {
        Err(crate::unsupported("cstring-typlen columns"))
    }
}

/// The TY-3 shred arm's registration (JSON-routing landing, the OD-4
/// two-arm election's write wiring): register every jsonb column of the
/// relation on the production [`JsonbShredSource`] — catalog knowledge
/// (`atttypid`) the writer cannot derive from `ColSchema`, exactly the
/// `DictPolicy` posture. A pure function of the relation schema, so the
/// serial seals, the statement finish, and the parallel providers can
/// never disagree. Non-jsonb tables register nothing (the source leaves
/// unregistered columns alone — the NoShred-equivalent degenerate).
/// Budgets ride `ShredOptions::default()` until the reloption plumbing
/// lands at this seam (`pgrc_shred_max_paths`/`pgrc_shred_paths` —
/// spec §17 vocabulary, noted in the routing doc).
fn shred_source_for(rel: &Relation<'_>) -> PgResult<JsonbShredSource> {
    let mut s = JsonbShredSource::new();
    let jsonb = crate::schema::jsonb_attnos(rel);
    for &a in &jsonb {
        s = s.with_column(a);
    }
    // DESIGN R3 (sortpath; trunk 8d42eadfb27 + 2a923763e45, ported): each `pgrc_shred_paths` declaration on every jsonb
    // column at its kind, then every `sorted_by` path key no declaration names, at the lane its values elect (so its
    // zones hold in every part; a numeric key forced to Text forfeited its lane in every part and q3-q5 read time_us
    // from the image)
    let typed = rel.rd_options.as_ref().and_then(|o| o.pgrcolumnar()).map(|o| o.shred_path_decls()).unwrap_or_default();
    for d in typed {
        use types_rel::reloptions::ShredPathKind;
        // jsonb_shred::Lane's bytes: Text 0, Uuid16 1, NumericFs 2, Bool 3
        let lane = match d.kind {
            ShredPathKind::Text => 0,
            ShredPathKind::Uuid => 1,
            ShredPathKind::Numeric => 2,
            ShredPathKind::Bool => 3,
        };
        for &a in &jsonb {
            let mut forced = crate::alloc::try_vec(d.keys.len(), "ingest")?;
            for k in d.keys.iter() {
                forced.push(crate::alloc::try_to_vec(k.as_bytes(), "ingest")?);
            }
            s = s.with_forced(a, forced, lane);
        }
    }
    for (attno, path) in crate::reloptions::sorted_by_keys(rel).unwrap_or_default() {
        if !path.is_empty() {
            s = s.with_forced_auto(attno, path);
        }
    }
    Ok(s)
}

/// Ingest one deformed row (the `table_tuple_insert` / `table_multi_insert`
/// arm body). The caller has already deformed the slot
/// (`slot_getallattrs`) and supplies `tuple_image` — the flat backing bytes
/// of the materialized physical tuple — as the independent readable-byte
/// witness for `raw_row`'s by-reference bound (`None` when the slot has no
/// single flat image).
///
/// `cid` is the statement's command id: the writer's stamp must not follow the command counter, which the statement's
/// own triggers and segment activations advance between its rows (COPYLOSS).
///
/// IX0 L3: the row's ID (`None` for a writer whose parts get theirs at the publish).
pub fn ingest_row(
    rel: &Relation<'_>,
    cid: types_core::CommandId,
    values: &[datum::Datum],
    isnull: &[bool],
    tuple_image: Option<&[u8]>,
) -> PgResult<Option<RowId>> {
    session::ensure_session_hooks();
    crate::inval::ensure_inval_registered()?;
    let stamp = TxnStamp { fxid: xact::GetCurrentFullTransactionId()?.value, cid };
    let relfilenumber = rel.rd_locator.get().relNumber as u64;
    let row = raw_row(rel, values, isnull, tuple_image)?;
    let mut vfs = RealVfs;
    let resolver = CodecResolver;
    let mut shred = shred_source_for(rel)?;
    let shred_opts = ShredOptions::default();

    let mut open_err: Option<Box<PgError>> = None;
    let mut outer: Option<TxnStamp> = None;
    let mut id: Option<RowId> = None;
    let mut released = Vec::new();
    let mut placed = Vec::new();
    let res = with_rel_candidates(rel, relfilenumber, |cands| {
        let sources: [&dyn CandidateSource; 1] = [cands];
        session::with_writers(|w| {
            if let Some(open) = w.stamp_of(relfilenumber).filter(|&open| open != stamp && live(open)) {
                outer = Some(open);
                return Err(pgrc_write::WriteError::Contract { detail: "outer statement's writer (published first)" });
            }
            let writer = match w.get_or_open(&mut vfs, relfilenumber, stamp, || {
                open_writer(rel, stamp).map_err(|e| {
                    open_err = Some(e);
                    pgrc_write::WriteError::Contract {
                        detail: "writer open failed (see raised error)",
                    }
                })
            }) {
                Ok(w) => w,
                Err(e) => return Err(e),
            };
            // IX0 L3 (DV-3), IDBURN: a row past the IDs held reserves the writer's next grant
            if let Some(n) = writer.needs_ids() {
                // IX1 L3: in flight until the transaction ends (an index cleanup leaves the entries of its homeless IDs)
                match crate::rowid_alloc::reserve_in_flight(rel, n) {
                    Ok(r) => writer.grant_ids(r.lo, r.hi)?,
                    Err(e) => {
                        open_err = Some(e);
                        return Err(pgrc_write::WriteError::Contract { detail: "row-ID reservation failed (see raised error)" });
                    }
                }
            }
            id = writer.next_id().map(|id| RowId { id, handed: writer.keeps_placements() });
            let mut env = SealEnv {
                vfs: &mut vfs,
                sources: &sources,
                resolver: &resolver,
                shred: &mut shred,
                shred_opts: &shred_opts,
            };
            let out = writer.append_row(&row, &mut NoExternalDetoast, &mut env);
            released = writer.take_released();
            placed = writer.take_placements();
            out
        })
    })?;
    crate::rowid_alloc::give_back(rel, &released);
    hold_placements(relfilenumber, stamp.fxid, placed);
    if let Some(e) = open_err {
        return Err(e);
    }
    if let Some(open) = outer {
        // a statement nested in another statement's load of this table (a trigger's, a function's): the outer
        // statement's rows so far are published now, as its own finish would, and it opens a new writer after
        publish_taken(rel, relfilenumber, open)?;
        return ingest_row(rel, cid, values, isnull, tuple_image);
    }
    res.map_err(write_error)?;
    Ok(id)
}

/// PUBLISH-WAIT: one publish of `rel` under its publish lock, `publish` given a clog probe and the head generation as
/// the process catalog holds it (P-1: its manifest and stats runs, so the publisher reads no manifest, run or footer
/// while `CURRENT` still names it; [`crate::catalog::BaseHold`]). A foreign in-progress generation at the chain head
/// (`PublishBusy`, returned before anything is staged) is waited out as heap waits out a conflicting xmax, with the
/// lock dropped first (the holder's next publish takes it, an edge the deadlock detector cannot see); then the whole
/// publish is redone from a fresh manifest read, so commit order is chain order (an aborted holder's generation is
/// skipped and its numbers reused). A background publisher defers instead ([`is_deferral`]).
///
/// SEAL-OFFHEAD: `stack` = a maintenance publish (rotation, seal, a freed segment, pgrc-plans/SEAL-OFFHEAD.md) or,
/// CONCLOAD, a bulk load: it chains on a foreign in-progress head whose run only added parts, those parts tagged with
/// their creator (`EXT_UNDER`), instead of waiting or deferring.
///
/// CONCLOAD: `record` (the catalog's record of the publish, [`catalog_publish`]) runs under the same lock, so the
/// catalog holds generations in the order they were published: a publish recorded after a later one would drop it as
/// a rolled-back generation above its base (GENID `drop_dead`).
fn publish_serialized<R, T>(
    rel: &Relation<'_>,
    stack: bool,
    publish: &mut dyn FnMut(&ClogTxnProbe, BaseStats<'_>) -> pgrc_write::WriteResult<R>,
    record: &mut dyn FnMut(R) -> PgResult<T>,
) -> PgResult<T> {
    let l = rel.rd_locator.get();
    // a relation without storage (relfilenumber 0: a partitioned table, whose relam only names its partitions'
    // default) has no directory to publish into: refused by name before anything is read (pgrc_write's publishes
    // refuse it too, `pgrc_write::publish::NO_STORAGE`)
    if l.relNumber == types_core::InvalidRelFileNumber {
        return Err(Box::new(
            PgError::error(format!("pgrc write: refused a publish on relation \"{}\" (relkind '{}'): it has no storage", rel.name(), rel.rd_rel.relkind as char))
                .with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR),
        ));
    }
    let slot: crate::catalog::RelSlot = (l.spcOid, l.dbOid, l.relNumber as u64);
    // D2189: a publish of the live schema on a head under a dropped-column stamp no generation describes is
    // refused ("manifest base identity mismatch"), so the stamp's schema generation goes first (not for a schema
    // publish itself: it describes its own)
    if !SCHEMA_PUBLISH.with(Cell::get) {
        catch_up_dropped_stamp(rel, slot)?;
    }
    let lock = crate::inval::publish_lock(slot.2);
    loop {
        let guard = pgsync::lock(&lock);
        let probe = if stack { ClogTxnProbe::stacking() } else { ClogTxnProbe::new() };
        let base = crate::catalog::generations(slot).and_then(|(gens, head)| gens.into_iter().find(|g| g.gen == head)).map(|g| g.base_hold());
        match publish(&probe, base.as_ref().map_or(BaseStats::None, |b| b.stats())) {
            Err(pgrc_write::WriteError::PublishBusy { holder_fxid }) => {
                drop(guard);
                probe.take_error()?;
                wait_for_publisher(rel, holder_fxid)?;
            }
            out => {
                let out = out.map_err(write_error)?;
                probe.take_error()?;
                return record(out);
            }
        }
    }
}

const DEFERRED: &str = "publish deferred: another transaction's publish is in progress on";

/// A background publisher: pgrc's maintenance workers (`PROC_IS_DB_MAINT_WORKER`) and autovacuum. It never waits
/// for a publish (its maintenance lock and segment locks would join the waits-for graph with user sessions): it
/// defers, and its table is due again.
fn background() -> bool {
    use types_storage::storage::{PROC_IS_AUTOVACUUM, PROC_IS_DB_MAINT_WORKER};
    let flags = |p| lmgr_proc::GetPGProcByNumber(p).statusFlags.load(std::sync::atomic::Ordering::Relaxed);
    lmgr_proc::MyProc().is_some_and(|p| flags(p) & (PROC_IS_AUTOVACUUM | PROC_IS_DB_MAINT_WORKER) != 0)
}

/// TOMB: [`background`] for pgrc_front's compile (a background compile never holds off a writer).
pub fn is_background() -> bool {
    background()
}

/// AUTOSTATS: an autovacuum worker (`PROC_IS_AUTOVACUUM`; its VACUUM of a pgrcolumnar table does not flush).
pub fn is_autovacuum() -> bool {
    use types_storage::storage::PROC_IS_AUTOVACUUM;
    lmgr_proc::MyProc().is_some_and(|p| lmgr_proc::GetPGProcByNumber(p).statusFlags.load(std::sync::atomic::Ordering::Relaxed) & PROC_IS_AUTOVACUUM != 0)
}

/// A background publisher's deferral (55P03): the worker re-queues the table without a WARNING.
pub fn is_deferral(e: &PgError) -> bool {
    e.sqlstate() == types_error::ERRCODE_LOCK_NOT_AVAILABLE && e.message().contains(DEFERRED)
}

/// Wait for the holder of `rel`'s head (the xid lock wait follows its subtransactions to the top, and a prepared
/// holder to COMMIT / ROLLBACK PREPARED): interrupts, `lock_timeout`, `statement_timeout` and the deadlock check
/// apply, and `pg_locks` shows a `transactionid` wait. A background publisher defers instead.
fn wait_for_publisher(rel: &Relation<'_>, holder_fxid: u64) -> PgResult<()> {
    if background() {
        return Err(deferral(rel));
    }
    let xid = crate::probe::holder_xid(holder_fxid)?;
    if xact_seams::transaction_id_is_current_transaction_id::call(xid) {
        return Err(crate::corrupt(&format!("publish of \"{}\" would wait for its own transaction {xid}", rel.name())));
    }
    lmgr::XactLockTableWait(xid, None, None, types_storage::lock::XLTW_Oper::None).map_err(|mut e| {
        e.add_context_line(format!("while publishing to relation \"{}\"", rel.name()));
        e
    })
}

/// Another transaction's publish heads `rel`'s chain now (the process catalog's head): a background publisher defers
/// before it feeds or reads ([`defer_if_busy`]; compaction asks [`compaction_blocked`]).
pub fn publish_in_progress(rel: &Relation<'_>) -> PgResult<bool> {
    let l = rel.rd_locator.get();
    let Some((gens, head)) = crate::catalog::generations((l.spcOid, l.dbOid, l.relNumber as u64)) else { return Ok(false) };
    let probe = ClogTxnProbe::new();
    // SEAL-OFFHEAD: a creator still in progress under a stacked head holds it as the head it was
    let foreign = |x: u64| probe.verdict(x) == TxnVerdict::InProgress && !probe.is_own(x, 0);
    let busy = gens.iter().find(|g| g.gen == head).is_some_and(|g| foreign(g.publisher_fxid) || g.under().iter().any(|e| foreign(e.fxid)));
    probe.take_error()?;
    Ok(busy)
}

/// COMPACTP item 2 (b): a compaction's publish would wait or defer on `rel`'s chain (K1 choice 8: checked before the
/// job decodes anything). A compaction stacks (`compact_publish`), so a foreign head in progress blocks it only when
/// the run under it changed more than its parts (the publish's own test, `pgrc_write::publish::stacks_on`, over the
/// catalog's manifests): a bulk load's or a seal's head does not; a rotation's or a compile's does.
pub fn compaction_blocked(rel: &Relation<'_>) -> PgResult<bool> {
    let l = rel.rd_locator.get();
    let Some((gens, head)) = crate::catalog::generations((l.spcOid, l.dbOid, l.relNumber as u64)) else { return Ok(false) };
    let probe = ClogTxnProbe::stacking();
    let mut read = |g: u64| gens.iter().find(|x| x.gen == g).map(|x| (*x.manifest).clone());
    let ok = pgrc_write::publish::stacks_on(head, &mut read, &probe, 0).map_err(write_error);
    probe.take_error()?;
    Ok(!ok?)
}

/// SEAL-OFFHEAD: `rel`'s head generation carries `EXT_UNDER` tags whose creator has ended (committed, or rolled
/// back: readers hide its parts through a composed view until a publish drops them). The front worker's settle
/// publishes then.
pub fn unsettled(rel: &Relation<'_>) -> PgResult<bool> {
    let l = rel.rd_locator.get();
    let Some((gens, head)) = crate::catalog::generations((l.spcOid, l.dbOid, l.relNumber as u64)) else { return Ok(false) };
    let probe = ClogTxnProbe::new();
    let due = gens.iter().find(|g| g.gen == head).is_some_and(|g| g.under().iter().any(|e| probe.verdict(e.fxid) != TxnVerdict::InProgress));
    probe.take_error()?;
    Ok(due)
}

/// A background publisher's pre-check before a seal feeds or a compile reads: deferred now rather than at its publish,
/// so the deferral wastes no work.
pub fn defer_if_busy(rel: &Relation<'_>) -> PgResult<()> {
    match background() && publish_in_progress(rel)? {
        true => Err(deferral(rel)),
        false => Ok(()),
    }
}

fn deferral(rel: &Relation<'_>) -> Box<PgError> {
    Box::new(PgError::error(format!("pgrcolumnar: {DEFERRED} \"{}\"", rel.name())).with_sqlstate(types_error::ERRCODE_LOCK_NOT_AVAILABLE))
}

/// COMPACTP item 4: this transaction holds a bulk writer of `rel` with rows [`finish_bulk`] publishes (the L0 cap
/// waits before that publish).
pub fn bulk_open(rel: &Relation<'_>) -> bool {
    let cur = xact::GetCurrentFullTransactionIdIfAny();
    let relfilenumber = rel.rd_locator.get().relNumber as u64;
    cur.is_valid() && session::with_writers(|w| w.stamp_of(relfilenumber)).is_some_and(|open| open.fxid == cur.value)
}

/// Statement-end publish (the `table_finish_bulk_insert` arm body): seal
/// the remainder, publish ONE manifest generation under the per-table
/// publish lock, witness the outcome. No-op when this statement buffered
/// nothing (heap relations pass through long before reaching here). ->
/// the rows published (AUTOSTATS: the caller counts them as the parent's
/// inserts, as main's 024123e763b did here).
///
/// The writer is this statement's when its (sub)transaction is the current one, whatever command id it carries. A
/// live outer (sub)transaction's writer is left to its own statement; an aborted one's is dropped.
pub fn finish_bulk(rel: &Relation<'_>) -> PgResult<u64> {
    let cur = xact::GetCurrentFullTransactionIdIfAny();
    let relfilenumber = rel.rd_locator.get().relNumber as u64;
    let Some(open) = session::with_writers(|w| w.stamp_of(relfilenumber)) else {
        return Ok(0);
    };
    if !cur.is_valid() || (open.fxid != cur.value && live(open)) {
        return Ok(0);
    }
    publish_taken(rel, relfilenumber, TxnStamp { fxid: cur.value, cid: open.cid })
}

/// The registered writer of `relfilenumber` published, if its stamp is `stamp` (else aborted as stale).
fn publish_taken(rel: &Relation<'_>, relfilenumber: u64, stamp: TxnStamp) -> PgResult<u64> {
    let mut vfs = RealVfs;
    let taken = session::with_writers(|w| w.take_for_publish(&mut vfs, relfilenumber, stamp))
        .map_err(write_error)?;
    let Some(mut writer) = taken else {
        return Ok(0);
    };
    let out = finish_and_publish(rel, &mut writer, &mut vfs, relfilenumber);
    if out.is_err() {
        // The writer left the registry, so the end-of-transaction purge cannot reach it: a failed publish after the
        // seal (its wait for another transaction's publish cancelled, timed out or a deadlock victim) unlinks its temp
        // parts here, under a `RECLAIM`.
        if let Err(e) = writer.abort(&mut vfs) {
            let _ = elog::elog(types_error::WARNING, format!("pgrc: temp parts of \"{}\" not reclaimed: {e:?}", rel.name()));
        }
    }
    out
}

/// IX0 L3: every part without IDs (the parallel lane's deposits) reserves `align_up(rows)` before the publish, in
/// part order; a part granted its IDs at its first row keeps them.
fn give_ids(rel: &Relation<'_>, parts: &mut [pgrc_write::seal::SealedPart]) -> PgResult<()> {
    for p in parts.iter_mut().filter(|p| p.ids.is_empty() && p.rows > 0) {
        let r = crate::rowid_alloc::reserve_in_flight(rel, p.rows)?;
        p.ids = vec![(r.lo, r.hi)];
    }
    Ok(())
}

/// A registered writer whose (sub)transaction is still in progress in this backend: its rows are not stale.
fn live(stamp: TxnStamp) -> bool {
    xact::TransactionIdIsCurrentTransactionId(types_core::xact::FullTransactionId::from_u64(stamp.fxid).xid())
}

fn finish_and_publish(rel: &Relation<'_>, writer: &mut TableWriter, vfs: &mut RealVfs, relfilenumber: u64) -> PgResult<u64> {
    let resolver = CodecResolver;
    let mut shred = shred_source_for(rel)?;
    let shred_opts = ShredOptions::default();
    let finished = with_rel_candidates(rel, relfilenumber, |cands| {
        let sources: [&dyn CandidateSource; 1] = [cands];
        let mut env = SealEnv {
            vfs: &mut *vfs,
            sources: &sources,
            resolver: &resolver,
            shred: &mut shred,
            shred_opts: &shred_opts,
        };
        writer.finish(&mut env)
    });
    crate::rowid_alloc::give_back(rel, &writer.take_released());
    hold_placements(relfilenumber, writer.stamp().fxid, writer.take_placements());
    finished?.map_err(write_error)?;
    if writer.sealed_parts().is_empty() {
        return Ok(0);
    }
    give_ids(rel, writer.sealed_parts_mut())?;
    let rows = writer.sealed_parts().iter().map(|p| p.rows).sum();
    let locator = rel.rd_locator.get();
    let slot: crate::catalog::RelSlot = (locator.spcOid, locator.dbOid, relfilenumber);
    let t0 = pg_clock::MonoStamp::now();
    // the catalog's record of this generation (its parts open, its stats as the publisher planned them) before this
    // transaction's commit makes its publisher visible
    let dir = crate::dirpath::table_dir_path(locator, rel.rd_backend);
    // CONCLOAD: a bulk load stacks on another load in progress instead of waiting for it (PostgreSQL's COPYs into one
    // heap never wait for each other)
    publish_serialized(rel, true, &mut |probe, base| writer.publish_with_gen(&mut *vfs, probe, base), &mut |outcome| catalog_publish(rel, slot, &dir, outcome, t0))?;
    // CONCLOAD test hook (the pgrc-catalog-order isolation spec): published and recorded, the lock released
    injection_point::injection_point("pgrc-bulk-published")?;
    Ok(rows)
}

/// D2189 (trunk 45e21f82010, ported onto FASTADD): a column dropped with no ALTER TABLE publish (a
/// dependency's cascade, an inheritance or partition child, a temp type's cleanup) leaves the head generation under a
/// stamp the relation's live schema no longer has, so every publish of the live schema was refused ("manifest base
/// identity mismatch": a base of another schema must be described): a bulk append, a front seal, a tombstone compile,
/// a compaction, VACUUM FULL. Trunk stamped the bulk writer with the old schema; on the line [`publish_serialized`]
/// first publishes FASTADD's metadata-only schema generation describing the stamp ([`publish_schema`], rebuilt by
/// [`crate::schema::book_dropped_stamp`]) in the publisher's transaction, so the base's parts keep reading under it
/// and the new ones are written under the live schema. Only a relation with a dropped attribute takes a step; a stamp
/// no candidate rebuilds (or past the candidate cap) is the rebuild's fail-closed refusal, before anything is published.
fn catch_up_dropped_stamp(rel: &Relation<'_>, slot: crate::catalog::RelSlot) -> PgResult<()> {
    if !rel.rd_att.attrs.iter().any(|a| a.attisdropped) {
        return Ok(());
    }
    let Some(head_fp) = crate::catalog::generations(slot).and_then(|(gens, head)| gens.into_iter().find(|g| g.gen == head)).map(|g| g.schema_fp) else {
        return Ok(());
    };
    let live = pgrc_format::ident::schema_fingerprint(&crate::schema::col_schemas(rel)?);
    if head_fp == live {
        return Ok(());
    }
    crate::schema::book_dropped_stamp(rel, head_fp)?;
    let Some(old) = crate::fastadd::schema_of(head_fp) else { return Ok(()) };
    publish_schema(rel, ::mcx::stdvec::try_to_vec(&old[..], "pgrc schema publish")?)?;
    Ok(())
}

/// The catalog's record of a publish (P-1: nothing read or written), the `sorted_by` claim check
/// ([`crate::reloptions::inval_on_claim_change`], D1159), and the DEBUG1 publish line: the part count,
/// the stats runs written and their bytes, the microseconds in the publisher's stats, in the catalog and in the whole
/// publish (from `started`, before the publish lock: base, plan, stage, log, apply, catalog), and whether the base was
/// the catalog's (the P-1 bench reads the line).
pub(crate) fn catalog_publish(rel: &Relation<'_>, slot: crate::catalog::RelSlot, dir: &str, outcome: PublishOutcome, started: pg_clock::MonoStamp) -> PgResult<Arc<crate::catalog::Generation>> {
    let t0 = pg_clock::MonoStamp::now();
    let PublishOutcome { gen, kind, part_nos, manifest, pointer, stats, runs_written, run_bytes, stats_us, base_given, legacy_parts, stacked_over, dropped, .. } = outcome;
    let kind_name = pgrc_write::publish::counters::KINDS.iter().find(|k| k.0 == kind).map_or("?", |k| k.1);
    if !stacked_over.is_empty() {
        let _ = elog::elog(types_error::DEBUG1, format!("pgrcolumnar: stacked {kind_name} of \"{}\" gen {gen} over in-progress {stacked_over:?}", rel.name())); // alloc: Cold
    }
    if !dropped.is_empty() {
        let _ = elog::elog(types_error::DEBUG1, format!("pgrcolumnar: settled \"{}\" gen {gen} ({kind_name}): dropped parts {dropped:?} of rolled-back writers", rel.name())); // alloc: Cold
    }
    let parts = manifest.parts.len();
    if legacy_parts > 0 {
        let _ = elog::elog(types_error::LOG, format!("pgrcolumnar: publish of \"{}\" gen {gen} wrote the legacy stats run of its base's {legacy_parts} parts (a generation written before P-1, stats from its sections file or its parts)", rel.name())); // alloc: Cold
    }
    let prev_gen = manifest.header.prev_gen;
    let g = crate::catalog::publish(slot, dir, manifest, stats, &crate::footer::fold_generation)?;
    g.set_pointer(pointer);
    // D339 (trunk 0a14b58f683, ported): a generation this transaction published records its command id, marked used, so
    // a snapshot taken before it (a cursor declared earlier in this transaction) does not see it and the next command's
    // snapshot does (`SnapshotCommitCheck::sees_command`). A maintenance publish of another transaction, or one with no
    // xid, records nothing.
    if xact_seams::transaction_id_is_current_transaction_id::call(types_core::xact::FullTransactionId::from_u64(g.publisher_fxid).xid()) {
        let cid = xact_seams::get_current_command_id::call(true)?;
        session::record_publish_cid(slot, g.gen, cid);
    }
    // COMPACTP: the L0 ledger's entry, merged from the base's under the publish lock (the trigger and the cap read it)
    let _ = crate::l0::of(slot, &g);
    // D1159: a publish that moves the planner's `sorted_by` claim invalidates the relation's cached plans. CONCLOAD: a
    // stacked bulk load always does (COMPACTP: and a stacked compaction). At its commit the readers that do not see a
    // creator under it move from the generation below that creator to this one without its parts, which neither whole
    // generation's claim measures
    let stacked = matches!(kind, pgrc_format::wal::PublishKind::Bulk | pgrc_format::wal::PublishKind::Compact) && !stacked_over.is_empty();
    crate::reloptions::inval_on_claim_change(rel, slot, prev_gen, &g, stacked)?;
    // D4804: a publish that moves what the lowering-time election reads answer (Gravity's `shred_election` /
    // `shred_dict` / `shred_object_roots`, whose readers hold the relation's `Dep::Stats`) sends a storage-scoped relcache
    // message, when the publish becomes visible (commit; locally at the command's end, undone with a rolled-back
    // savepoint), which leaves the entry's program generation. Any kind can install or retire a part (a rolled-back
    // writer's settles under any); a stacked publish's base is not settled, so it always sends
    let elections = if stacked_over.is_empty() { elections_moved(slot, prev_gen, &g) } else { Elections::Send("stacked, storage message") };
    if let Elections::Send(_) = elections {
        inval::with_relcache_scope(inval::REL_SCOPE_STORAGE, || inval::invalidate::CacheInvalidateRelcache(rel))?;
    }
    if !matches!(elections, Elections::SameParts) {
        let what = match elections { Elections::Send(why) => why, _ => "unchanged, no message" };
        let _ = elog::elog(types_error::DEBUG1, format!("pgrc elections: rel={} gen={gen} base={prev_gen} {kind_name}: {what}", rel.name())); // alloc: Cold
    }
    let end = pg_clock::MonoStamp::now();
    let publish_us = end.since_ns(started) / 1000;
    crate::scanstats::published(slot.1, rel.rd_id, kind, publish_us, end.as_ns());
    let _ = elog::elog(
        types_error::DEBUG1,
        format!(
            "pgrc publish: rel={} gen={gen} kind={} parts={} table_parts={parts} runs_written={runs_written} run_bytes={run_bytes} stats_us={stats_us} catalog_us={} publish_us={publish_us} base={}",
            rel.name(),
            pgrc_write::publish::counters::KINDS.iter().find(|k| k.0 == kind).map_or("?", |k| k.1),
            part_nos.len(),
            end.since_ns(t0) / 1000,
            if base_given { "catalog" } else { "directory" },
        ),
    );
    Ok(g)
}

/// D4804: how a publish's election reads compare with its base's (`elections_moved`).
#[derive(Clone, Copy)]
enum Elections {
    /// The same open parts: no part installed or retired.
    SameParts,
    /// Other parts, equal election digests.
    Unchanged,
    /// The storage message goes, and why.
    Send(&'static str),
}

/// D4804: generation `g`'s election reads against its base `prev_gen`'s: a moved digest sends, and so does one that
/// cannot be told (a first publish, a base the catalog no longer holds, a digest that did not read).
fn elections_moved(slot: crate::catalog::RelSlot, prev_gen: u64, g: &crate::catalog::Generation) -> Elections {
    let base = (prev_gen != 0).then(|| crate::catalog::generations(slot).and_then(|(gens, _)| gens.into_iter().find(|x| x.gen == prev_gen))).flatten();
    let Some(base) = base else { return Elections::Send("untold, storage message") };
    if crate::catalog::same_parts(&base, g) {
        return Elections::SameParts;
    }
    match (base.election_digest(), g.election_digest()) {
        (Some(a), Some(b)) if a == b => Elections::Unchanged,
        (Some(_), Some(_)) => Elections::Send("moved, storage message"),
        _ => Elections::Send("untold, storage message"),
    }
}

/// A publish of the extension block only (F1 rotation, design §4.5), in this transaction under the table's publish
/// lock: `edit` gets the base generation and the plan; the catalog records the generation like any publish.
pub fn publish_ext(rel: &Relation<'_>, edit: pgrc_write::publish::Edit<'_>) -> PgResult<Arc<crate::catalog::Generation>> {
    publish_ext_as(rel, false, edit)
}

/// FASTADD (pgrc-plans/FASTADD.done): the metadata-only ADD/DROP COLUMN's publish, in the ALTER's transaction under
/// its AccessExclusiveLock: a generation of the relation's new schema (the header's fingerprint) whose parts keep the
/// schemas they were written with, `old` (the table's before the ALTER) described. No part is read or written; the
/// generation is effective iff the ALTER commits.
pub fn publish_schema(rel: &Relation<'_>, old: Vec<pgrc_format::class::ColSchema>) -> PgResult<Arc<crate::catalog::Generation>> {
    crate::fastadd::note(&old);
    let mut edit = |_: Option<&pgrc_format::manifest::Manifest>, b: &mut pgrc_write::plan::PlanBuilder<'_>| -> pgrc_write::WriteResult<()> {
        b.describe(old.clone());
        Ok(())
    };
    in_schema_publish(|| publish_ext(rel, &mut edit))
}

thread_local! {
    /// D2189: this backend is in [`publish_schema`], whose publish describes its own base (no catch-up).
    static SCHEMA_PUBLISH: Cell<bool> = const { Cell::new(false) };
}

/// D4710: `f` runs with [`SCHEMA_PUBLISH`] set; the flag is put back on any exit, an unwind included.
fn in_schema_publish<T>(f: impl FnOnce() -> T) -> T {
    let _in = SchemaPublish::enter();
    f()
}

struct SchemaPublish(bool);

impl SchemaPublish {
    fn enter() -> SchemaPublish {
        SchemaPublish(SCHEMA_PUBLISH.with(|f| f.replace(true)))
    }
}

impl Drop for SchemaPublish {
    fn drop(&mut self) {
        SCHEMA_PUBLISH.with(|f| f.set(self.0));
    }
}

/// SEAL-OFFHEAD: [`publish_ext`] of a maintenance step (a rotation, a freed segment, a settle): it chains on another
/// transaction's in-progress head instead of waiting ([`publish_serialized`]'s `stack`).
pub fn publish_ext_maint(rel: &Relation<'_>, edit: pgrc_write::publish::Edit<'_>) -> PgResult<Arc<crate::catalog::Generation>> {
    publish_ext_as(rel, true, edit)
}

fn publish_ext_as(rel: &Relation<'_>, stack: bool, edit: pgrc_write::publish::Edit<'_>) -> PgResult<Arc<crate::catalog::Generation>> {
    let stamp = current_stamp()?;
    let locator = rel.rd_locator.get();
    let dir = crate::dirpath::table_dir_path(locator, rel.rd_backend);
    let relfilenumber = locator.relNumber as u64;
    let schema_fingerprint = pgrc_format::ident::schema_fingerprint(&crate::schema::col_schemas(rel)?);
    let spec = pgrc_write::seal::PartSpec { spc: locator.spcOid, db: locator.dbOid, relfilenumber, schema_fingerprint };
    let slot: crate::catalog::RelSlot = (locator.spcOid, locator.dbOid, relfilenumber);
    let mut sink = pgrc_wal_seams::sink_for::call(rel);
    let t0 = pg_clock::MonoStamp::now();
    publish_serialized(rel, stack, &mut |probe, base| pgrc_write::publish::publish_ext(&mut RealVfs, &mut *sink, &dir, &spec, stamp.fxid, probe, base, &mut *edit), &mut |outcome| catalog_publish(rel, slot, &dir, outcome, t0))
}

/// A compile (F2 L7) in this transaction under the table's publish lock: each `(part_no, rows)` folds its
/// `(granule, row)` deletions into the part's DV at the generation this publishes. `None` when no part gains a row
/// (nothing is published); the caller heap-deletes the folded tombstones in the same transaction either way.
pub fn publish_compile(rel: &Relation<'_>, marks: &[(u32, &[(u32, u16)])]) -> PgResult<Option<Arc<crate::catalog::Generation>>> {
    let stamp = current_stamp()?;
    let locator = rel.rd_locator.get();
    let dir = crate::dirpath::table_dir_path(locator, rel.rd_backend);
    let relfilenumber = locator.relNumber as u64;
    let schema_fingerprint = pgrc_format::ident::schema_fingerprint(&crate::schema::col_schemas(rel)?);
    let spec = pgrc_write::seal::PartSpec { spc: locator.spcOid, db: locator.dbOid, relfilenumber, schema_fingerprint };
    let slot: crate::catalog::RelSlot = (locator.spcOid, locator.dbOid, relfilenumber);
    let mut sink = pgrc_wal_seams::sink_for::call(rel);
    let t0 = pg_clock::MonoStamp::now();
    publish_serialized(rel, false, &mut |probe, base| pgrc_write::publish::publish_compile(&mut RealVfs, &mut *sink, &dir, &spec, marks, stamp.fxid, probe, base), &mut |outcome| {
        outcome.map(|o| catalog_publish(rel, slot, &dir, o, t0)).transpose()
    })
}

/// TOMB: [`publish_compile`] with `edit` in the same plan (the tombstone heaps' state beside the DVs of the heap it
/// folds). `None` when no part gains a row: nothing is published.
pub fn publish_compile_ext(rel: &Relation<'_>, marks: &[(u32, &[(u32, u16)])], edit: pgrc_write::publish::Edit<'_>) -> PgResult<Option<Arc<crate::catalog::Generation>>> {
    let stamp = current_stamp()?;
    let locator = rel.rd_locator.get();
    let dir = crate::dirpath::table_dir_path(locator, rel.rd_backend);
    let relfilenumber = locator.relNumber as u64;
    let schema_fingerprint = pgrc_format::ident::schema_fingerprint(&crate::schema::col_schemas(rel)?);
    let spec = pgrc_write::seal::PartSpec { spc: locator.spcOid, db: locator.dbOid, relfilenumber, schema_fingerprint };
    let slot: crate::catalog::RelSlot = (locator.spcOid, locator.dbOid, relfilenumber);
    let mut sink = pgrc_wal_seams::sink_for::call(rel);
    let t0 = pg_clock::MonoStamp::now();
    publish_serialized(rel, false, &mut |probe, base| pgrc_write::publish::publish_compile_ext(&mut RealVfs, &mut *sink, &dir, &spec, marks, stamp.fxid, probe, base, &mut *edit), &mut |outcome| {
        outcome.map(|o| catalog_publish(rel, slot, &dir, o, t0)).transpose()
    })
}

/// A segment's seal (F1, design §4.5): rows in the caller's order into one unsorted part, published under the
/// segment's reserved `part_no`. Outside the statement writers, so a seal never shares one with a bulk insert.
/// Rows stage in chunks the writer appends column by column (on the pool, when there is one: F1 L6).
pub struct SealWriter {
    w: TableWriter,
    chunk: RowChunk,
}

/// Rows per staged chunk: eight default granules (a compaction's column feed stages the same chunks, K1-MEM).
pub(crate) const SEAL_CHUNK_ROWS: u32 = 65_536;

fn seal_env<R>(rel: &Relation<'_>, f: &mut dyn FnMut(&mut SealEnv<'_>) -> pgrc_write::WriteResult<R>) -> PgResult<R> {
    let (mut vfs, resolver, mut shred, shred_opts) = (RealVfs, CodecResolver, shred_source_for(rel)?, ShredOptions::default());
    with_rel_candidates(rel, rel.rd_locator.get().relNumber as u64, |cands| {
        let sources: [&dyn CandidateSource; 1] = [cands];
        f(&mut SealEnv { vfs: &mut vfs, sources: &sources, resolver: &resolver, shred: &mut shred, shred_opts: &shred_opts })
    })?
    .map_err(write_error)
}

/// The writers of a table's parts. At `wal_level=minimal` only the bulk `COPY` writers (serial and parallel) skip WAL
/// for a large file (fsync before `PUBLISH`); the front seal and compaction log bytes as at `replica`, so a trickle
/// table pays no per-part inline fsync (main 10-04, WALMODE-MIN: Mb measured compaction outputs paying them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterKind {
    Copy,
    Seal,
    Compact,
}

/// Whether a writer of `kind` logs every file by its bytes when WAL is not needed beyond crash recovery.
pub fn bytes_only_at_minimal(kind: WriterKind) -> bool {
    kind != WriterKind::Copy
}

impl SealWriter {
    /// A front segment's seal.
    pub fn open(rel: &Relation<'_>, pool: Option<Arc<dyn ColumnPool>>) -> PgResult<SealWriter> {
        SealWriter::open_with(rel, pool, WriterKind::Seal)
    }

    fn open_with(rel: &Relation<'_>, pool: Option<Arc<dyn ColumnPool>>, kind: WriterKind) -> PgResult<SealWriter> {
        session::ensure_session_hooks();
        crate::inval::ensure_inval_registered()?;
        let mut w = open_writer(rel, current_stamp()?)?;
        if bytes_only_at_minimal(kind) && !transam_xlog::XLogIsNeeded() {
            w.bytes_only();
        }
        w.set_structural(StructuralPolicy::new());
        w.set_policy(PartCutPolicy { max_rows: u64::MAX, max_bytes: u64::MAX, ..PartCutPolicy::default() });
        session::with_writers(|r| r.share_seq(rel.rd_locator.get().relNumber as u64, &mut w));
        if let Some(pool) = pool {
            // The same candidates `seal_env` elects with: a pure function of the schema.
            w.set_pool(SealPool { pool, sources: vec![Arc::new(codec_candidates(rel)?)], resolver: Arc::new(CodecResolver) });
        }
        let ncols = crate::schema::col_schemas(rel)?.len();
        Ok(SealWriter { w, chunk: RowChunk::new(ncols, SEAL_CHUNK_ROWS) })
    }

    /// One row; external values detoasted by the caller.
    pub fn append(&mut self, rel: &Relation<'_>, values: &[datum::Datum], isnull: &[bool]) -> PgResult<()> {
        self.chunk.push_row(&raw_row(rel, values, isnull, None)?).map_err(write_error)?;
        if self.chunk.rows() >= SEAL_CHUNK_ROWS {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> PgResult<()> {
        let fresh = RowChunk::new(self.chunk.ncols(), SEAL_CHUNK_ROWS);
        let chunk = std::mem::replace(&mut self.chunk, fresh);
        self.w.append_chunk(&chunk).map_err(write_error)
    }

    /// SEALSIZE: the part's column-buffer bytes (the cut policy's measure), the staged chunk appended first.
    pub fn buffered_bytes(&mut self) -> PgResult<u64> {
        if self.chunk.rows() > 0 {
            self.flush()?;
        }
        Ok(self.w.buffered_bytes())
    }

    pub fn rows(&self) -> u64 {
        self.chunk.rows() as u64 + self.w.buffered_rows() + self.w.sealed_parts().iter().map(|s| s.rows).sum::<u64>()
    }

    /// Seal and publish in this transaction under the publish lock; `rowmap` is the part's `RowIdentMap` payload and
    /// `ids` the row IDs it homes (IX0: the segment's extents).
    /// On an error the part temp is unlinked here (the seal writer is in no registry the transaction end purges).
    pub fn publish(mut self, rel: &Relation<'_>, part_no: u32, rowmap: &[u8], ids: &[(u64, u64)], edit: pgrc_write::publish::Edit<'_>) -> PgResult<Arc<crate::catalog::Generation>> {
        let out = self.seal_and_publish(rel, part_no, rowmap, ids, edit);
        if out.is_err() {
            if let Err(e) = self.w.abort(&mut RealVfs) {
                let _ = elog::elog(types_error::WARNING, format!("pgrc: seal temp part of \"{}\" not reclaimed: {e:?}", rel.name()));
            }
        }
        out
    }

    fn seal_and_publish(&mut self, rel: &Relation<'_>, part_no: u32, rowmap: &[u8], ids: &[(u64, u64)], edit: pgrc_write::publish::Edit<'_>) -> PgResult<Arc<crate::catalog::Generation>> {
        self.flush()?;
        seal_env(rel, &mut |env| self.w.finish(env))?;
        let locator = rel.rd_locator.get();
        let slot: crate::catalog::RelSlot = (locator.spcOid, locator.dbOid, locator.relNumber as u64);
        let t0 = pg_clock::MonoStamp::now();
        let dir = crate::dirpath::table_dir_path(locator, rel.rd_backend);
        publish_serialized(rel, true, &mut |probe, base| self.w.publish_seal(&mut RealVfs, probe, base, part_no, rowmap, ids, &mut *edit), &mut |outcome| catalog_publish(rel, slot, &dir, outcome, t0))
    }

    /// The error path: unlink the part temp under a `RECLAIM`.
    pub fn abort(mut self) -> PgResult<()> {
        self.w.abort(&mut RealVfs).map_err(write_error)
    }
}

/// The newest generation every snapshot sees (design §4.7): committed, its publisher before the vacuum `horizon`.
pub fn keep_generation(rel: &Relation<'_>, horizon: u32) -> PgResult<Option<Arc<crate::catalog::Generation>>> {
    let l = rel.rd_locator.get();
    let Some((gens, _)) = crate::catalog::generations((l.spcOid, l.dbOid, l.relNumber as u64)) else { return Ok(None) };
    let probe = ClogTxnProbe::new();
    let publishers: Vec<u64> = gens.iter().map(|g| g.publisher_fxid).collect();
    let keep = keep_at(&publishers, horizon, &mut |p| probe.verdict(p) == TxnVerdict::Committed).map(|i| gens[i].clone());
    probe.take_error()?;
    Ok(keep)
}

/// FREEZEKEEP: which of `publishers` (the held generations', ascending) [`keep_generation`] keeps: the newest one
/// before `horizon` in `TransactionIdPrecedes`'s order, which puts the frozen and bootstrap publishers before every
/// normal xid (VACUUM's freeze republishes a generation every snapshot already sees under the frozen publisher), and
/// `committed`.
pub(crate) fn keep_at(publishers: &[u64], horizon: u32, committed: &mut dyn FnMut(u64) -> bool) -> Option<usize> {
    publishers.iter().rposition(|&p| types_core::xact::TransactionIdPrecedes(p as u32, horizon) && committed(p))
}

/// FREEZEKEEP: a `RECLAIM`'s conflict xid for a keep published by `publisher`: its xid, or none (0) for a frozen or
/// bootstrap one, which every snapshot sees (as [`seal_horizon`]).
pub(crate) fn reclaim_xid(publisher: u64) -> u32 {
    match pgrc_write::publish::clog_bound(publisher) {
        true => publisher as u32,
        false => 0,
    }
}

/// A freed segment's conflict horizon (RECLAIMRACE): the oldest committed generation held, up to `keep`, that lists
/// every one of `freed` Sealed, as (its publisher's xid, its number). A snapshot that sees that publisher reads none of
/// their heaps (a scan reads only Active and Sealing records), so only older snapshots conflict, and the generations
/// below it are past every snapshot that survives the conflict. When the seal's own generation already left the
/// catalog, the oldest one held stands in: a later publisher, so more readers conflict than need to (the safe side).
/// A frozen publisher is seen by every snapshot: no conflict (xid 0).
pub fn seal_horizon(rel: &Relation<'_>, keep: &crate::catalog::Generation, freed: &[SegmentRecord]) -> PgResult<(u32, u64)> {
    let l = rel.rd_locator.get();
    let held = crate::catalog::generations((l.spcOid, l.dbOid, l.relNumber as u64)).map_or_else(Vec::new, |(g, _)| g);
    let held: Vec<(u64, u64, &[SegmentRecord])> = held.iter().map(|g| (g.gen, g.publisher_fxid, &g.segments[..])).collect();
    let probe = ClogTxnProbe::new();
    let found = oldest_sealed(&held, keep.gen, freed, &|p| probe.verdict(p) == TxnVerdict::Committed);
    probe.take_error()?;
    let (publisher, gen) = found.unwrap_or((keep.publisher_fxid, keep.gen));
    Ok((if publisher == pgrc_write::publish::FROZEN_PUBLISHER_FXID { 0 } else { publisher as u32 }, gen))
}

/// The first of `held` (`(gen, publisher, segments)`, ascending) up to `keep_gen` with a committed publisher that lists
/// every one of `freed` Sealed, as (publisher, gen).
fn oldest_sealed(held: &[(u64, u64, &[SegmentRecord])], keep_gen: u64, freed: &[SegmentRecord], committed: &dyn Fn(u64) -> bool) -> Option<(u64, u64)> {
    let sealed = |recs: &[SegmentRecord]| freed.iter().all(|f| recs.iter().any(|r| r.state == SegState::Sealed && r.slot == f.slot && r.part_no == f.part_no && r.relid == f.relid));
    held.iter().find(|g| g.0 <= keep_gen && sealed(g.2) && committed(g.1)).map(|g| (g.1, g.0))
}

/// The conflict of freed segments alone (K1 L5, OC-7 a): a `RECLAIM` with no names that a hot standby resolves at `xid`
/// ([`seal_horizon`]) before it retires the generations below `keep_from`. The caller logs it before the segments
/// reset in place (not MVCC), so a standby resolves before it replays a reset; the files below keep go by
/// [`reclaim_below`], deferred.
pub fn log_segment_conflict(rel: &Relation<'_>, xid: u32, keep_from: u64) -> PgResult<()> {
    let l = rel.rd_locator.get();
    let dir = pgrc_format::wal::DirLoc { spc: l.spcOid, db: l.dbOid, relnumber: l.relNumber };
    let rec = pgrc_format::wal::ReclaimRec { dir, xid, retire_below: Some(pgrc_format::wal::Retire::Conflict(keep_from)), names: Vec::new() };
    pgrc_wal_seams::sink_for::call(rel).reclaim(&rec, &mut || Ok(())).map_err(write_error)
}

/// Unlink every generation's files below `keep` under one `RECLAIM` that retires them (on a hot standby, once its own
/// horizon passes keep's publisher: K1 L5, OC-7 a), with the DVs below `keep` that `keep` does not name (a later
/// generation names only its own or `keep`'s) and the stats runs below `keep` that `keep` does not reference (P-1); the
/// names unlinked. A freed segment's conflict is its own record ([`log_segment_conflict`]).
///
/// K1-RECLAIM: the names come from what changed, the held generations below `keep` against `keep` ([`plan_reclaim`]
/// without a listing), each checked to exist, so the cost follows the retired files and not the directory. A full
/// sweep (the directory listed, the same rules) runs when the catalog says one is due ([`crate::catalog::sweep_due`]),
/// for what the held generations cannot name.
///
/// RECLAIMDEFER (i′): the generations leave the catalog ([`crate::catalog::take_below`]) under the table's publish
/// lock, exactly the set the plan used, verified there. A reclaim that looks incremental, under the table's maintenance
/// lock (`serialized`: every caller holds it to its transaction's end, so no other reclaim runs meanwhile) and a relation
/// lock that keeps DROP, TRUNCATE and VACUUM FULL out, plans without the publish lock (the hint: the 8-client probe's
/// plans took ~53 ms there at ~3k parts); under it, `keep_now` (the horizon's keep again) and the catalog are checked
/// against the hint ([`hint_stands`]: still incremental, keep not lower, the same generations below keep by identity),
/// and a hint that fails is planned again there. An incremental reclaim ([`incremental`]) then lets go of the lock:
/// its flushes, its `RECLAIM` and the unlinks run after it (still WAL before data, inside the sink's
/// DELAY_CHKPT_START window, under the caller's locks); one that fails there leaves the next reclaim a sweep
/// ([`crate::catalog::sweep_after`]). Any other reclaim unlinks under the lock, as before. The retirement's
/// part-registry walk and its drops run last, with no lock held ([`crate::catalog::Retirement::finish`]).
pub fn reclaim_below(rel: &Relation<'_>, keep: &crate::catalog::Generation, serialized: bool, keep_now: &mut dyn FnMut() -> PgResult<Option<Arc<crate::catalog::Generation>>>) -> PgResult<usize> {
    let locator = rel.rd_locator.get();
    let dir = crate::dirpath::table_dir_path(locator, rel.rd_backend);
    let slot: crate::catalog::RelSlot = (locator.spcOid, locator.dbOid, locator.relNumber as u64);
    let lock = crate::inval::publish_lock(slot.2);
    let mut sink = pgrc_wal_seams::sink_for::call(rel);
    let rel_locked = lmgr::CheckRelationLockedByMe(rel, types_rel::AccessShareLock, true);
    let mut hint = None;
    if serialized && rel_locked && sink.logs() {
        let seen = Seen::take(slot);
        if seen.incremental() {
            hint = Some(plan_from(rel, &dir, keep, seen)?);
            // RECLAIMDEFER's hint e2e holds a reclaim here (planned, the publish lock not asked for), and forces a mismatch
            injection_point::injection_point("pgrc-reclaim-hinted")?;
            if injection_point::is_attached(HINT_STALE_POINT) {
                crate::catalog::perturb_below(slot, keep.gen)?;
            }
        }
    }
    let mut hold = crate::holds::Hold::take(&lock);
    let now = Seen::take(slot);
    let Some(keep_now) = keep_now()? else {
        // the horizon moved back past every held generation: nothing below it is safe to retire
        drop(now);
        hold.release();
        return Ok(0);
    };
    let lower = (keep_now.gen < keep.gen).then_some(keep_now);
    let keep = lower.as_deref().unwrap_or(keep);
    let mut stale = None;
    let mut p = match hint {
        Some(h) if hint_stands(&h, &now, keep.gen) => {
            crate::holds::hint(true);
            drop(now);
            h
        }
        h => {
            if h.is_some() {
                crate::holds::hint(false);
            }
            stale = h;
            plan_from(rel, &dir, keep, now)?
        }
    };
    let inc = p.facts_inc && sink.logs() && rel_locked;
    let planned: Vec<u64> = p.below.iter().map(|g| g.gen).collect();
    if p.names.is_empty() && p.oldest >= keep.gen {
        // GENID-2: dead parts whose files are gone already
        crate::catalog::forget_dead_parts(slot, &p.dead);
        crate::catalog::note_reclaim(slot, keep.gen, p.sweep, &mut p.retired.iter().copied());
        hold.release();
        return Ok(0);
    }
    hold.planned(inc);
    let spec = pgrc_write::seal::PartSpec { spc: locator.spcOid, db: locator.dbOid, relfilenumber: slot.2, schema_fingerprint: keep.schema_fp };
    let retire = pgrc_format::wal::Retire::Deferred(keep.gen);
    let rec = pgrc_format::wal::ReclaimRec { dir: pgrc_write::plan::dir_loc(&spec), xid: reclaim_xid(keep.publisher_fxid), retire_below: Some(retire), names: std::mem::take(&mut p.names) };
    let n = rec.names.len();
    // K1 L4: `keep`'s commit (an asynchronous one too) is durable before anything below it is unlinked; else a crash
    // rolls the table back to a generation whose files are gone. K1 L6: the insert END pointer: at a page boundary
    // GetXLogInsertRecPtr points past the next page header, a request XLogFlush refuses ("xlog flush request ... is not
    // satisfied").
    let flush_keep = |logged: bool| match logged && n > 0 {
        true => transam_xlog::XLogFlush(transam_xlog::insert::GetXLogInsertEndRecPtr()),
        false => Ok(()),
    };
    if !inc {
        // what a rolled-back generation left (numbers a publish may once have shared), a sweep's listing, an unlogged
        // directory, a caller without the relation lock: unlinked under the publish lock
        flush_keep(sink.logs())?;
        sink.reclaim(&rec, &mut || {
            hold.unlinking(inc, n);
            pgrc_write::plan::apply_reclaim(&mut RealVfs, &dir, &rec)
        })
        .map_err(write_error)?;
        let retired = crate::catalog::take_below(slot, keep.gen);
        crate::catalog::forget_dead_parts(slot, &p.dead);
        crate::catalog::note_reclaim(slot, keep.gen, p.sweep, &mut p.retired.iter().copied());
        hold.release();
        finish_retirement(&hold, retired);
        drop((p, stale));
        return Ok(n);
    }
    let retired = crate::catalog::take_below(slot, keep.gen);
    crate::catalog::note_reclaim(slot, keep.gen, false, &mut p.retired.iter().copied());
    if retired.gens() != planned {
        // nothing changes a table's generations below its head without its publish lock but a DROP or TRUNCATE, which
        // the relation lock keeps out: a bug guard, not a path
        crate::catalog::log(true, format!("pgrcolumnar: reclaim of {dir} planned generations {planned:?} and retired {:?}; the next reclaim sweeps", retired.gens())); // alloc: Cold
        crate::catalog::sweep_after(slot, keep.gen);
    }
    hold.release();
    drop((p, stale));
    let out = (|| {
        // RECLAIMDEFER's crash e2e kills a reclaim here: out of the catalog, nothing logged or unlinked
        injection_point::injection_point("pgrc-reclaim-unlocked")?;
        flush_keep(true)?;
        sink.reclaim(&rec, &mut || {
            hold.unlinking(inc, n);
            pgrc_write::plan::apply_reclaim(&mut RealVfs, &dir, &rec)
        })
        .map_err(write_error)
    })();
    if out.is_err() {
        crate::catalog::sweep_after(slot, keep.gen);
    }
    finish_retirement(&hold, retired);
    out.map(|()| n)
}

/// RECLAIMDEFER (i′) forced-mismatch injection: attached (any action), a reclaim's hint meets a below generation the
/// catalog replaced meanwhile ([`crate::catalog::perturb_below`]) and is planned again under the lock.
pub const HINT_STALE_POINT: &str = "pgrc-reclaim-hint-stale";

/// What a reclaim plans from: the catalog's generations of the table, its dead parts and rolled-back generations'
/// files, and whether a sweep is due.
pub(crate) struct Seen {
    gens: Vec<Arc<crate::catalog::Generation>>,
    all_dead: Vec<u32>,
    dead_files: Vec<String>,
    sweep: Option<Vec<u32>>,
}

impl Seen {
    fn take(slot: crate::catalog::RelSlot) -> Seen {
        Seen {
            gens: crate::catalog::generations(slot).map_or_else(Vec::new, |(g, _)| g),
            all_dead: crate::catalog::dead_parts(slot),
            dead_files: crate::catalog::dead_files(slot),
            sweep: crate::catalog::sweep_due(slot),
        }
    }

    fn incremental(&self) -> bool {
        incremental_facts(&self.all_dead, &self.dead_files, self.sweep.is_some(), true)
    }
}

/// A reclaim's plan below `keep_gen`, and what it was made from: the generations below keep (their identity is the
/// hint's check), the dead parts it names, whether it swept.
pub(crate) struct Planned {
    keep_gen: u64,
    below: Vec<Arc<crate::catalog::Generation>>,
    names: Vec<String>,
    retired: Vec<u32>,
    dead: Vec<u32>,
    oldest: u64,
    sweep: bool,
    facts_inc: bool,
}

/// K1-RECLAIM's plan below `keep` from `seen` ([`plan_reclaim`]; a sweep lists the directory).
fn plan_from(rel: &Relation<'_>, dir: &str, keep: &crate::catalog::Generation, seen: Seen) -> PgResult<Planned> {
    use pgrc_write::wvfs::WriteVfs;
    let Seen { gens: held, all_dead, dead_files, sweep } = seen;
    let manifests: Vec<(u64, &[pgrc_format::manifest::PartRecord])> = held.iter().map(|g| (g.gen, &g.manifest.parts[..])).collect();
    // GENID-2: rolled-back generations' parts (no reader ever needs an aborted publisher's), none a held one names
    let named = |n: u32| manifests.iter().any(|g| g.1.binary_search_by_key(&n, |p| p.part_no).is_ok());
    let dead: Vec<u32> = all_dead.iter().copied().filter(|&n| !named(n)).collect();
    let sweep = sweep.map(|pending| sweep_pending(&manifests, keep.gen, pending));
    let below: Vec<&crate::catalog::Generation> = held.iter().filter(|g| g.gen < keep.gen).map(|g| &**g).collect();
    let mut vfs = RealVfs;
    let listing = match sweep.is_some() || reclaim_check() {
        true => Some(vfs.list_dir(dir).map_err(write_error)?),
        false => None,
    };
    let facts = ReclaimFacts { manifests: &manifests, below: &below, keep, dead: &dead, dead_files: &dead_files, natts: rel.rd_att.natts.max(0) as u32, runs: kept_runs(keep), run_revs: run_revs(&keep.manifest) };
    let plan = plan_reclaim(&facts, sweep.as_deref().unwrap_or(&[]), listing.as_deref().filter(|_| sweep.is_some()), &mut |n| {
        vfs.exists_path(&format!("{dir}/{n}")).unwrap_or(true)
    });
    if sweep.is_none() {
        if let Some(all) = &listing {
            check_incremental(rel, &facts, all, &plan.names);
        }
    }
    let facts_inc = incremental_facts(&all_dead, &dead_files, sweep.is_some(), true);
    let oldest = held.first().map_or(keep.gen, |g| g.gen);
    let below = held.iter().filter(|g| g.gen < keep.gen).cloned().collect();
    Ok(Planned { keep_gen: keep.gen, below, names: plan.names, retired: plan.retired, dead, oldest, sweep: sweep.is_some(), facts_inc })
}

/// K1-RECLAIM: the sweep's pending parts ([`crate::catalog::sweep_due`]) but those a held generation from `keep_gen` on
/// names (`manifests`: every held generation's parts, ascending by `gen`), which no reclaim below `keep_gen` may unlink.
/// One that only a generation below `keep_gen` names is retired by this reclaim anyway ([`retired_parts`]), so only the
/// generations from keep on are searched; all of them when keep is not held (then `retired_parts` retires nothing).
fn sweep_pending(manifests: &[(u64, &[pgrc_format::manifest::PartRecord])], keep_gen: u64, pending: Vec<u32>) -> Vec<u32> {
    let from = if manifests.iter().any(|g| g.0 == keep_gen) { keep_gen } else { 0 };
    let live: Vec<&[pgrc_format::manifest::PartRecord]> = manifests.iter().filter(|g| g.0 >= from).map(|g| g.1).collect();
    pending.into_iter().filter(|&n| !live.iter().any(|ps| ps.binary_search_by_key(&n, |p| p.part_no).is_ok())).collect()
}

/// RECLAIMDEFER (i′): under the publish lock, a hint made without it stands when the table still looks incremental
/// (`now`: no dead part, no rolled-back generation's file, no sweep due), the horizon's keep is not below the hint's
/// (`keep_gen`, the keep this reclaim retires below), and the catalog holds the very generations below keep the hint
/// planned from (the same `Arc`s, in order): a publish since only added generations above them.
pub(crate) fn hint_stands(h: &Planned, now: &Seen, keep_gen: u64) -> bool {
    let below = now.gens.iter().filter(|g| g.gen < h.keep_gen);
    now.incremental() && keep_gen == h.keep_gen && below.clone().count() == h.below.len() && below.zip(&h.below).all(|(a, b)| Arc::ptr_eq(a, b))
}

/// A reclaim's retirement finished ([`crate::catalog::Retirement::finish`]), counted when it ran under the publish lock.
fn finish_retirement(hold: &crate::holds::Hold<'_>, retired: crate::catalog::Retirement) {
    if !retired.is_empty() {
        hold.retiring();
    }
    retired.finish();
}

/// RECLAIMDEFER (i′): a reclaim whose unlinks need not hold the publish lock. Its names then come only from committed
/// generations below `keep` (their own files, and the parts and runs only they name), numbers GENID never issues again
/// in this directory: no dead part (`dead`: GENID-2's, every one, named or not) and no rolled-back generation's own
/// file (`dead_files`), which a later publish could have shared a number with before GENID; no sweep due (a listing
/// judges what no held generation names); a logged sink (its `RECLAIM` is flushed before the unlinks); and the
/// relation locked by this backend in a mode TRUNCATE's and DROP's AccessExclusiveLock conflicts with (every mode is),
/// until the reclaim returns (every caller: a worker step's or a flush's relation lock, VACUUM's session lock), so the
/// directory cannot be emptied and refilled under the unlinks.
fn incremental(rel: &Relation<'_>, dead: &[u32], dead_files: &[String], sweep: bool, logged: bool) -> bool {
    incremental_facts(dead, dead_files, sweep, logged) && lmgr::CheckRelationLockedByMe(rel, types_rel::AccessShareLock, true)
}

/// [`incremental`] but the relation lock.
pub(crate) fn incremental_facts(dead: &[u32], dead_files: &[String], sweep: bool, logged: bool) -> bool {
    dead.is_empty() && dead_files.is_empty() && !sweep && logged
}

/// K1-RECLAIM: what a reclaim below `keep` knows without listing the directory.
struct ReclaimFacts<'a> {
    /// Every held generation's `(gen, parts)`, ascending by `gen` (`keep` among them when it is in the catalog).
    manifests: &'a [(u64, &'a [pgrc_format::manifest::PartRecord])],
    /// The held generations below `keep`.
    below: &'a [&'a crate::catalog::Generation],
    keep: &'a crate::catalog::Generation,
    /// GENID-2's dead parts that no held generation names.
    dead: &'a [u32],
    /// The rolled-back generations' own files ([`crate::catalog::dead_files`]).
    dead_files: &'a [String],
    /// The relation's attribute count: a part's `fp128` sidecars are numbered by attribute (Gravity's key tables).
    natts: u32,
    /// [`kept_runs`] of `keep`.
    runs: Option<std::collections::BTreeSet<(u64, u32)>>,
    /// SORTRUNS-E0: the sorted runs of `keep` with a reverse file ([`run_revs`]).
    run_revs: std::collections::BTreeSet<u64>,
}

/// SORTRUNS-E0: the sorted runs of `m` with a reverse file (`runrev-<run_id>`), by run ID. A reverse whose run `keep`
/// does not name and that is not newer than `keep` is dead: run IDs are the publishing generation's, never reused, and
/// a generation from `keep` on carries `keep`'s runs or ones it publishes. An undecodable record keeps nothing out of
/// the doomed set (the catalog decoded `m`, so it does not happen).
fn run_revs(m: &pgrc_format::manifest::Manifest) -> std::collections::BTreeSet<u64> {
    if !pgrc_format::sortrun::has_runs(&m.ext) {
        return std::collections::BTreeSet::new();
    }
    pgrc_format::sortrun::runs_in(&m.ext).map(|r| r.into_iter().filter(|r| r.homes()).map(|r| r.run_id).collect()).unwrap_or_default()
}

/// K1-RECLAIM: a reclaim's unlinks (sorted), and the parts among them whose optional sidecars an incremental plan did
/// not name.
struct ReclaimPlan {
    names: Vec<String>,
    retired: Vec<u32>,
}

/// K1-RECLAIM: the files a reclaim below `f.keep` unlinks. `listing` (a full sweep): the directory's names, each
/// judged by the rules; `pending` (the sweep's) adds the parts earlier incremental reclaims retired. Without a
/// listing: the names the held generations below `keep` imply (their manifest, sections, bank stats, runs and DVs,
/// and the part file, row map, stats sidecar, `fp128` sidecars and DVs of each part they retire), judged by the same
/// rules, that `exists` finds. A part's other optional sidecars (predicate cache, memo, trigram: no writer names one
/// yet) wait for the sweep.
fn plan_reclaim(f: &ReclaimFacts<'_>, pending: &[u32], listing: Option<&[String]>, exists: &mut dyn FnMut(&str) -> bool) -> ReclaimPlan {
    use pgrc_format::dirlayout::{classify, manifest_file_name, part_file_name, run_file_name, sections_file_name, sidecar_file_name, FileClass};
    use pgrc_format::sidecar::SidecarKind;
    let keep = f.keep;
    let mut retired = retired_parts(f.manifests, keep.gen);
    retired.extend(f.dead.iter().copied());
    let incremental: Vec<u32> = retired.iter().copied().collect();
    if listing.is_some() {
        retired.extend(pending.iter().copied());
    }
    let keeps_dv = |pn: u32, g: u64| keep.manifest.parts.binary_search_by_key(&pn, |p| p.part_no).is_ok_and(|i| keep.manifest.parts[i].dv_gen == g);
    let doomed = |n: &str| match classify(n) {
        Some(FileClass::Part(pn) | FileClass::Sidecar(pn, _, _)) if retired.contains(&pn) => true,
        Some(FileClass::Manifest(g) | FileClass::Sections(g) | FileClass::BankStats(g)) => g < keep.gen,
        Some(FileClass::Sidecar(pn, SidecarKind::Dv, g)) => g < keep.gen && !keeps_dv(pn, g),
        // P-1: a stats run written below `keep` that `keep` does not reference (see `kept_runs`)
        Some(FileClass::Run(g, seq)) => g < keep.gen && f.runs.as_ref().is_some_and(|r| !r.contains(&(g, seq))),
        // SORTRUNS-E0: a run's reverse once `keep` no longer names its run (the run retired); never a member rewrite's
        Some(FileClass::RunRev(r)) => r <= keep.gen && !f.run_revs.contains(&r),
        _ => false,
    };
    if let Some(all) = listing {
        let mut names: Vec<String> = all.iter().filter(|n| doomed(n)).cloned().collect();
        names.sort_unstable();
        return ReclaimPlan { names, retired: Vec::new() };
    }
    let mut cand: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for &pn in &incremental {
        cand.insert(part_file_name(pn));
        cand.insert(sidecar_file_name(pn, SidecarKind::RowIdentMap, 1));
        cand.insert(sidecar_file_name(pn, SidecarKind::Stats, 1));
        for a in 1..=f.natts {
            cand.insert(sidecar_file_name(pn, SidecarKind::Fp128, a as u64));
        }
    }
    cand.extend(f.dead_files.iter().cloned());
    for g in f.below {
        cand.insert(manifest_file_name(g.gen));
        cand.insert(sections_file_name(g.gen));
        cand.insert(pgrc_format::bankstats::bankstats_file_name(g.gen));
        for p in g.manifest.parts.iter().filter(|p| p.dv_gen != 0 || p.dv_len != 0) {
            cand.insert(sidecar_file_name(p.part_no, SidecarKind::Dv, p.dv_gen));
        }
        if let Some(st) = &g.stats {
            for r in st.root.iter().chain(st.runs.iter().map(|r| &r.rref)) {
                cand.insert(run_file_name(r.gen, r.seq));
            }
        }
        for r in run_revs(&g.manifest) {
            cand.insert(pgrc_format::dirlayout::runrev_file_name(r));
        }
    }
    let names = cand.into_iter().filter(|n| doomed(n) && exists(n)).collect();
    ReclaimPlan { names, retired: incremental }
}

/// K1-RECLAIM: `PGRUST_PGRC_RECLAIM_CHECK=1` (the gates): every incremental reclaim also lists the directory and
/// warns of a file the full rules unlink that it did not name, other than an optional sidecar of a part it retired
/// (those wait for the sweep).
fn reclaim_check() -> bool {
    static CHECK: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *CHECK.get_or_init(|| std::env::var("PGRUST_PGRC_RECLAIM_CHECK").is_ok_and(|v| v == "1"))
}

fn check_incremental(rel: &Relation<'_>, f: &ReclaimFacts<'_>, all: &[String], names: &[String]) {
    use pgrc_format::dirlayout::{classify, FileClass};
    use pgrc_format::sidecar::SidecarKind;
    let full = plan_reclaim(f, &[], Some(all), &mut |_| true).names;
    let retired = retired_parts(f.manifests, f.keep.gen);
    let waits = |n: &str| match classify(n) {
        Some(FileClass::Sidecar(pn, k, g)) => !matches!(k, SidecarKind::Dv | SidecarKind::RowIdentMap | SidecarKind::Stats) && !(k == SidecarKind::Fp128 && g >= 1 && g <= f.natts as u64) && (retired.contains(&pn) || f.dead.contains(&pn)),
        _ => false,
    };
    let missed: Vec<&String> = full.iter().filter(|n| names.binary_search(n).is_err() && !waits(n)).collect();
    let extra: Vec<&String> = names.iter().filter(|n| full.binary_search(n).is_err()).collect();
    if !missed.is_empty() || !extra.is_empty() {
        let _ = elog::elog(types_error::WARNING, format!("pgrcolumnar: reclaim check of \"{}\" below gen {}: missed {missed:?} extra {extra:?}", rel.name(), f.keep.gen)); // alloc: Cold
    }
}

/// P-1: the stats runs `keep` references, `(writer generation, seq)`: its root carrier and the runs its root lists. A
/// run written below `keep` and not among them is dead: a newer generation descends from `keep` and carries only runs
/// `keep` references or ones written at or above it. `None` (keep every run) for a generation with a root whose runs
/// the catalog does not hold (its stats were refused and it was completed from its parts).
fn kept_runs(keep: &crate::catalog::Generation) -> Option<std::collections::BTreeSet<(u64, u32)>> {
    let root = pgrc_format::manifest::stats_ref(&keep.manifest).ok()?;
    let Some(stats) = &keep.stats else { return root.is_none().then(Default::default) };
    Some(stats.root.iter().chain(stats.runs.iter().map(|r| &r.rref)).map(|r| (r.gen, r.seq)).collect())
}

/// K1 L4: the parts (with every sidecar of theirs) that only generations below `keep_from` name, out of the catalog's
/// `(gen, parts)` (each generation's parts ascending by `part_no`, as a manifest holds them). Every snapshot reads
/// `keep_from` or newer, and a publish not yet in the catalog names its base's parts (a generation from `keep_from`
/// on) or new ones, so none of these is read again. Nothing without `keep_from`. K1-RECLAIM: each generation below is
/// merged against `keep_from`'s parts, and only the parts it lacks are looked up in the newer ones.
fn retired_parts<T: PartNo>(gens: &[(u64, &[T])], keep_from: u64) -> std::collections::BTreeSet<u32> {
    let mut out = std::collections::BTreeSet::new();
    let Some(&(_, keep)) = gens.iter().find(|g| g.0 == keep_from) else { return out };
    let newer = |n: u32| gens.iter().filter(|g| g.0 > keep_from).any(|g| g.1.binary_search_by_key(&n, |p| p.part_no()).is_ok());
    for (_, parts) in gens.iter().filter(|g| g.0 < keep_from) {
        let mut k = keep.iter().map(|p| p.part_no()).peekable();
        for n in parts.iter().map(|p| p.part_no()) {
            while k.next_if(|&m| m < n).is_some() {}
            if k.peek() != Some(&n) && !out.contains(&n) && !newer(n) {
                out.insert(n);
            }
        }
    }
    out
}

/// A part number in a generation's part list.
trait PartNo {
    fn part_no(&self) -> u32;
}

impl PartNo for u32 {
    fn part_no(&self) -> u32 {
        *self
    }
}

impl PartNo for pgrc_format::manifest::PartRecord {
    fn part_no(&self) -> u32 {
        self.part_no
    }
}

/// The parallel session's leader sink, made on the leader thread from the relation (`WalProvider::leader` runs
/// once, when the engine is built).
struct LeaderWal(pgsync::Mutex<Option<Box<dyn WalSink + Send>>>);

impl WalProvider for LeaderWal {
    fn leader(&self) -> Box<dyn WalSink + Send> {
        pgsync::lock(&self.0).take().unwrap_or_else(|| Box::new(NoWal))
    }
}

/// The parallel-COPY writer seam (M4X parquet-parallel lane). Opens (or
/// re-uses) the statement writer through EXACTLY the serial registry
/// discipline — same `open_writer`, same stamp, same sink —
/// builds the production [`ParProviders`] from the SAME election facts
/// serial seals use (full-registry [`CodecCandidates`] rebuilt from catalog
/// facts — a pure function of the relation schema, identical to the cached
/// copy `ingest_row` consults — [`CodecResolver`], no-detoast, and the
/// catalog-driven jsonb shred registration [`shred_source_for`] — the
/// same source serial seals consult),
/// and runs `f` with both.
///
/// The driver sits at executor altitude (copy_cmd → pgrc_ingest_par —
/// this crate is below `runtime` and can never host the rtpool binding,
/// the M3-H split-crate law) and MUST deposit through
/// [`TableWriter::deposit_sealed_parts`] (the `parallel_ingest`
/// convenience does). On success the writer is exactly where an
/// equivalent serial statement would have left it; publish stays
/// [`finish_bulk`] via `table_finish_bulk_insert`, UNCHANGED — fence,
/// witness, publish lock and all.
///
/// Reentrancy contract: `f` runs INSIDE the session writer-registry
/// borrow for the whole parallel session; it must not re-enter any
/// pgrc_am session face (the parallel engine takes everything it needs
/// as passed capabilities, so the production driver does not).
pub fn with_parallel_copy_writer<R>(
    rel: &Relation<'_>,
    f: impl FnOnce(&mut TableWriter, ParProviders) -> PgResult<R>,
) -> PgResult<R> {
    session::ensure_session_hooks();
    crate::inval::ensure_inval_registered()?;
    let stamp = current_stamp()?;
    let relfilenumber = rel.rd_locator.get().relNumber as u64;
    // Rebuilt, not cloned out of the session cache: codec_candidates is a
    // pure function of the relation schema (the cache exists only to skip
    // recomputation), so the workers' Arc and the serial cache can never
    // disagree.
    let cands = codec_candidates(rel)?;
    let providers = ParProviders {
        vfs: Arc::new(RealVfsProvider),
        detoast: Arc::new(NoDetoastProvider),
        shred: Arc::new(shred_source_for(rel)?),
        sources: vec![Arc::new(cands)],
        resolver: Arc::new(CodecResolver),
        shred_opts: ShredOptions::default(),
        // v4 delta (TY-1): no structural elections at the AM ingest seam
        // yet — the parallel seal elects exactly as serial with the
        // default (none) policy; the structural planes wire at S3b.
        structural: Default::default(),
        wal: Arc::new(LeaderWal(pgsync::Mutex::new(Some(pgrc_wal_seams::sink_for::call(rel))))),
    };
    let mut vfs = RealVfs;
    let mut open_err: Option<Box<PgError>> = None;
    let mut body_out: Option<PgResult<R>> = None;
    let reg = session::with_writers(|w| {
        let writer = match w.get_or_open(&mut vfs, relfilenumber, stamp, || {
            open_writer(rel, stamp).map_err(|e| {
                open_err = Some(e);
                pgrc_write::WriteError::Contract {
                    detail: "writer open failed (see raised error)",
                }
            })
        }) {
            Ok(w) => w,
            Err(e) => return Err(e),
        };
        body_out = Some(f(writer, providers));
        Ok(())
    });
    if let Some(e) = open_err {
        return Err(e);
    }
    reg.map_err(write_error)?;
    body_out.expect("writer body ran")
}

/// K1 (design §4.9): a compaction writes through a seal writer (cut off, on the seal pool), which for a sorted merge
/// carries the table's `sorted_by` key: its feed is presorted, so the seal only verifies and attests the order.
impl SealWriter {
    pub fn open_compact(rel: &Relation<'_>, pool: Option<Arc<dyn ColumnPool>>, sorted: bool) -> PgResult<SealWriter> {
        let mut s = SealWriter::open_with(rel, pool, WriterKind::Compact)?;
        if sorted {
            s.w.set_structural(StructuralPolicy::new().with_cluster_key(crate::reloptions::cluster_key(rel)));
        }
        Ok(s)
    }

    /// K1-MEM: seal the compaction's output column by column, never holding its columns together
    /// (`TableWriter::seal_streamed`; the bytes are the ones `append` then the publish's seal write). `fill(c, out)`
    /// appends the output's `rows` rows of column `c` (the schema's `c`-th) to `out`. `Ok(false)`: the output needs a
    /// grain finer than the default; nothing was sealed and the caller appends the rows instead.
    pub fn seal_columns(&mut self, rel: &Relation<'_>, rows: u64, fill: &mut dyn FnMut(usize, &mut pgrc_write::ingest::ColBuffer) -> PgResult<()>) -> PgResult<bool> {
        let mut failed: Option<Box<PgError>> = None;
        let out = seal_env(rel, &mut |env| {
            self.w.seal_streamed(env, rows, &mut |c, out| {
                fill(c, out).map_err(|e| {
                    failed = Some(e);
                    pgrc_write::WriteError::Refused { what: "a compaction's column feed failed" }
                })
            })
        });
        match failed {
            Some(e) => Err(e),
            None => out,
        }
    }

    /// Seal and publish the compaction in this transaction under the publish lock: the output (when a row survived)
    /// with its `RowIdentMap` `map` = `(form, body)` homing `ids` (IX0), the inputs `retire` = `(part_no, dv_gen)` retired.
    /// The generation and the output's part number; on an error the part temp is unlinked.
    pub fn publish_compact(mut self, rel: &Relation<'_>, retire: &[(u32, u64)], map: (u32, &[u8]), ids: &[(u64, u64)]) -> PgResult<(Arc<crate::catalog::Generation>, Option<u32>)> {
        let out = self.compact_publish(rel, retire, map, ids);
        if out.is_err() {
            if let Err(e) = self.w.abort(&mut RealVfs) {
                let _ = elog::elog(types_error::WARNING, format!("pgrc: compaction temp part of \"{}\" not reclaimed: {e:?}", rel.name()));
            }
        }
        out
    }

    /// SORTRUNS-E0 L2: seal the rows appended so far as one part now (a run member whose values need a grain finer
    /// than the default: the row feed, then this).
    pub fn seal_appended(&mut self, rel: &Relation<'_>) -> PgResult<()> {
        self.flush()?;
        seal_env(rel, &mut |env| self.w.finish(env))
    }

    /// SORTRUNS-E0 L2: a run job's staged file (its reverse) through the writer's WAL ([`TableWriter::write_file`]).
    pub fn write_file(&mut self, k: u32, fill: &mut dyn FnMut(&mut dyn FnMut(&[u8]) -> pgrc_write::WriteResult<()>) -> pgrc_write::WriteResult<()>) -> PgResult<String> {
        self.w.write_file(&mut RealVfs, k, fill).map_err(write_error)
    }

    /// SORTRUNS-E0 L2: publish a run merge in this transaction under the publish lock: every part sealed so far a
    /// member of `run` (its job read generation `start_gen`), in seal order, with its map `maps[k]`; the inputs `retire` retired; `staged` renamed. `carry`
    /// runs under the lock: the rows the inputs' DVs marked since the job read them, as member ordinals
    /// ([`pgrc_write::publish::publish_runs`]). The generation and the members' part numbers; on an error the temps
    /// are unlinked.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_runs(
        mut self,
        rel: &Relation<'_>,
        maps: &[Vec<u8>],
        retire: &[u32],
        run: pgrc_format::sortrun::SortRun,
        start_gen: u64,
        staged: &[(String, String)],
        carry: &mut dyn FnMut(&pgrc_format::manifest::Manifest) -> PgResult<Vec<(usize, Vec<u32>)>>,
    ) -> PgResult<(Arc<crate::catalog::Generation>, Vec<u32>)> {
        let out = self.runs_publish(rel, maps, retire, run, start_gen, staged, carry);
        if out.is_err() {
            if let Err(e) = self.w.abort(&mut RealVfs) {
                let _ = elog::elog(types_error::WARNING, format!("pgrc: run merge temps of \"{}\" not reclaimed: {e:?}", rel.name()));
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn runs_publish(
        &mut self,
        rel: &Relation<'_>,
        maps: &[Vec<u8>],
        retire: &[u32],
        run: pgrc_format::sortrun::SortRun,
        start_gen: u64,
        staged: &[(String, String)],
        carry: &mut dyn FnMut(&pgrc_format::manifest::Manifest) -> PgResult<Vec<(usize, Vec<u32>)>>,
    ) -> PgResult<(Arc<crate::catalog::Generation>, Vec<u32>)> {
        self.flush()?;
        seal_env(rel, &mut |env| self.w.finish(env))?;
        let locator = rel.rd_locator.get();
        let slot: crate::catalog::RelSlot = (locator.spcOid, locator.dbOid, locator.relNumber as u64);
        let t0 = pg_clock::MonoStamp::now();
        let dir = crate::dirpath::table_dir_path(locator, rel.rd_backend);
        // the carry's own error, kept across the writer's (which only says the carry failed)
        let mut failed: Option<Box<PgError>> = None;
        let out = publish_serialized(
            rel,
            true,
            &mut |probe, base| {
                let mut routed = |m: &pgrc_format::manifest::Manifest| {
                    carry(m).map_err(|e| {
                        failed = Some(e);
                        pgrc_write::WriteError::Refused { what: "a run merge's DV carry failed" }
                    })
                };
                self.w.publish_runs(&mut RealVfs, probe, base, maps, retire, run.clone(), start_gen, staged, &mut routed)
            },
            &mut |(outcome, nos)| catalog_publish(rel, slot, &dir, outcome, t0).map(|gen| (gen, nos)),
        );
        match (out, failed) {
            (Err(_), Some(e)) => Err(e),
            (out, _) => out,
        }
    }

    /// SORTRUNS-E0 L3b1: publish VACUUM FULL's renumber of a sorted run in this transaction under the publish lock:
    /// every part sealed so far a member of a self-homed run of `level` with its map `maps[k]` and dense ranges
    /// `ranges[k]`; the inputs `retire` retired ([`pgrc_write::publish::publish_self_run`]). The generation and the
    /// members' part numbers; on an error the temps are unlinked.
    pub fn publish_self_run(mut self, rel: &Relation<'_>, maps: &[(u32, Vec<u8>)], ranges: &[pgrc_format::manifest::IdRanges], retire: &[(u32, u64)], level: u8) -> PgResult<(Arc<crate::catalog::Generation>, Vec<u32>)> {
        let out = (|| -> PgResult<(Arc<crate::catalog::Generation>, Vec<u32>)> {
            self.flush()?;
            seal_env(rel, &mut |env| self.w.finish(env))?;
            let locator = rel.rd_locator.get();
            let slot: crate::catalog::RelSlot = (locator.spcOid, locator.dbOid, locator.relNumber as u64);
            let t0 = pg_clock::MonoStamp::now();
            let dir = crate::dirpath::table_dir_path(locator, rel.rd_backend);
            publish_serialized(rel, true, &mut |probe, base| self.w.publish_self_run(&mut RealVfs, probe, base, maps, ranges, retire, level), &mut |(outcome, nos)| {
                catalog_publish(rel, slot, &dir, outcome, t0).map(|gen| (gen, nos))
            })
        })();
        if out.is_err() {
            if let Err(e) = self.w.abort(&mut RealVfs) {
                let _ = elog::elog(types_error::WARNING, format!("pgrc: run renumber temps of \"{}\" not reclaimed: {e:?}", rel.name()));
            }
        }
        out
    }

    /// SORTRUNS-E0 L3b1: publish a run member's slot swap in this transaction under the publish lock: the one part
    /// sealed so far replaces member `from` in its slot, with its RUN map `map`; `carry` runs under the lock: the new
    /// member's ordinals whose rows `from`'s DV marked since the job read it
    /// ([`pgrc_write::publish::publish_member_swap`]). The generation and the part number installed; on an error the
    /// temps are unlinked.
    pub fn publish_member_swap(mut self, rel: &Relation<'_>, map: &[u8], from: u32, carry: &mut dyn FnMut(&pgrc_format::manifest::Manifest) -> PgResult<Vec<u32>>) -> PgResult<(Arc<crate::catalog::Generation>, u32)> {
        let out = self.swap_publish(rel, map, from, carry);
        if out.is_err() {
            if let Err(e) = self.w.abort(&mut RealVfs) {
                let _ = elog::elog(types_error::WARNING, format!("pgrc: member swap temps of \"{}\" not reclaimed: {e:?}", rel.name()));
            }
        }
        out
    }

    fn swap_publish(&mut self, rel: &Relation<'_>, map: &[u8], from: u32, carry: &mut dyn FnMut(&pgrc_format::manifest::Manifest) -> PgResult<Vec<u32>>) -> PgResult<(Arc<crate::catalog::Generation>, u32)> {
        self.flush()?;
        seal_env(rel, &mut |env| self.w.finish(env))?;
        let locator = rel.rd_locator.get();
        let slot: crate::catalog::RelSlot = (locator.spcOid, locator.dbOid, locator.relNumber as u64);
        let t0 = pg_clock::MonoStamp::now();
        let dir = crate::dirpath::table_dir_path(locator, rel.rd_backend);
        let mut failed: Option<Box<PgError>> = None;
        let out = publish_serialized(
            rel,
            true,
            &mut |probe, base| {
                let mut routed = |m: &pgrc_format::manifest::Manifest| {
                    carry(m).map_err(|e| {
                        failed = Some(e);
                        pgrc_write::WriteError::Refused { what: "a member swap's DV carry failed" }
                    })
                };
                self.w.publish_member_swap(&mut RealVfs, probe, base, map, from, &mut routed)
            },
            &mut |(outcome, no)| catalog_publish(rel, slot, &dir, outcome, t0).map(|gen| (gen, no)),
        );
        match (out, failed) {
            (Err(_), Some(e)) => Err(e),
            (out, _) => out,
        }
    }

    fn compact_publish(&mut self, rel: &Relation<'_>, retire: &[(u32, u64)], map: (u32, &[u8]), ids: &[(u64, u64)]) -> PgResult<(Arc<crate::catalog::Generation>, Option<u32>)> {
        self.flush()?;
        seal_env(rel, &mut |env| self.w.finish(env))?;
        let locator = rel.rd_locator.get();
        let slot: crate::catalog::RelSlot = (locator.spcOid, locator.dbOid, locator.relNumber as u64);
        let t0 = pg_clock::MonoStamp::now();
        let dir = crate::dirpath::table_dir_path(locator, rel.rd_backend);
        // COMPACTP item 2 (b): it stacks on a foreign head in progress whose run only added parts (a bulk load's)
        // instead of deferring to it: its inputs are committed parts, never the run's
        publish_serialized(rel, true, &mut |probe, base| self.w.publish_compact(&mut RealVfs, probe, base, retire, map, ids), &mut |(outcome, part_no)| {
            catalog_publish(rel, slot, &dir, outcome, t0).map(|gen| (gen, part_no))
        })
    }
}

#[cfg(test)]
mod tests {
    /// Text under any deterministic collation is offered dictionary codes; a nondeterministic collation (equal
    /// values of other bytes) and non-varlena storage are not.
    #[test]
    fn dict_offered_under_every_deterministic_collation() {
        use super::dict_eligible;
        use pgrc_format::class::{CollationClass, StorageClass};
        let v = StorageClass::VarlenaVerbatim;
        assert!(dict_eligible(true, v, CollationClass::C));
        assert!(dict_eligible(true, v, CollationClass::OtherDeterministic));
        assert!(!dict_eligible(true, v, CollationClass::Nondeterministic));
        assert!(!dict_eligible(false, v, CollationClass::C));
        assert!(!dict_eligible(true, StorageClass::ByvalWord { width: 8, signed: true }, CollationClass::C));
    }

    /// D4710: a publish that unwinds out of `publish_schema` leaves `SCHEMA_PUBLISH` as it found it.
    #[test]
    fn d4710_schema_publish_is_put_back_when_the_publish_unwinds() {
        use super::{in_schema_publish, SCHEMA_PUBLISH};
        let r = std::panic::catch_unwind(|| {
            in_schema_publish(|| {
                assert!(SCHEMA_PUBLISH.with(|f| f.get()));
                panic!("publish_ext raised")
            })
        });
        assert!(r.is_err());
        assert!(!SCHEMA_PUBLISH.with(|f| f.get()), "D4710: SCHEMA_PUBLISH stayed set after the publish unwound");
    }

    /// FREEZEKEEP: a generation VACUUM froze (its publisher `FROZEN_PUBLISHER_FXID`, which every snapshot sees) is a keep
    /// like any committed one before the horizon: after a restart it is the catalog's only generation, and with no keep
    /// no reclaim (nor the sweep a new catalog entry is due) ran until the next ordinary publish.
    #[test]
    fn freezekeep_a_frozen_head_is_kept() {
        use super::keep_at;
        use pgrc_write::publish::FROZEN_PUBLISHER_FXID as FROZEN;
        let all = &mut |_| true;
        assert_eq!(keep_at(&[600, 700, FROZEN], 800, all), Some(2), "the freeze over 700 heads the chain");
        assert_eq!(keep_at(&[FROZEN], 800, all), Some(0), "a restart's catalog: the frozen head alone");
        assert_eq!(keep_at(&[FROZEN, 800], 800, all), Some(0), "800 is not before the horizon 800");
        assert_eq!(keep_at(&[FROZEN, 799], 800, all), Some(1));
        assert_eq!(keep_at(&[1], 800, all), Some(0), "the bootstrap publisher");
        assert_eq!(keep_at(&[700, 750], 800, &mut |p| p != 750), Some(0), "never an uncommitted one");
        assert_eq!(keep_at(&[(1u64 << 32) | 4_000_000_000], 10, all), Some(0), "the xid order wraps");
        assert_eq!(keep_at(&[700], 0, all), None, "no horizon, no keep");
    }

    /// FREEZEKEEP: a frozen or bootstrap keep's `RECLAIM` names no conflict xid (0): every snapshot sees it.
    #[test]
    fn freezekeep_a_frozen_keeps_reclaim_has_no_conflict_xid() {
        use super::reclaim_xid;
        assert_eq!(reclaim_xid(pgrc_write::publish::FROZEN_PUBLISHER_FXID), 0);
        assert_eq!(reclaim_xid(1), 0);
        assert_eq!(reclaim_xid((1u64 << 32) | 700), 700);
    }

    /// WALMODE-MIN: at minimal only COPY skips WAL; seals and compactions keep their bytes in WAL.
    #[test]
    fn only_copy_skips_wal_at_minimal() {
        use super::{bytes_only_at_minimal, WriterKind};
        assert!(!bytes_only_at_minimal(WriterKind::Copy));
        assert!(bytes_only_at_minimal(WriterKind::Seal));
        assert!(bytes_only_at_minimal(WriterKind::Compact));
    }

    /// RECLAIMRACE: the seal's generation is the conflict horizon, not keep; a rolled-back seal or a later incarnation
    /// of the slot never is.
    #[test]
    fn a_freed_segments_horizon_is_the_oldest_committed_generation_listing_it_sealed() {
        use pgrc_format::manifest::{SegState, SegmentRecord};
        let seg = |slot: u16, part_no: u32, state: SegState| SegmentRecord { slot, part_no, relid: 500 + slot as u32, state, fxid: 7 };
        let (y, z) = (seg(1, 40, SegState::Sealed), seg(2, 41, SegState::Active));
        let active = [seg(1, 40, SegState::Active)];
        let sealing = [seg(1, 40, SegState::Sealing), z];
        let sealed = [y, z];
        let reused = [seg(1, 50, SegState::Sealed), z];
        let held: Vec<(u64, u64, &[SegmentRecord])> = vec![(3, 100, &active[..]), (4, 101, &sealing[..]), (5, 102, &sealed[..]), (6, 103, &sealed[..]), (7, 104, &sealed[..])];
        let all = |_: u64| true;
        assert_eq!(super::oldest_sealed(&held, 7, &[y], &all), Some((102, 5)), "the seal, not keep (7)");
        assert_eq!(super::oldest_sealed(&held, 7, &[y], &|p| p != 102), Some((103, 6)), "a rolled-back seal is no horizon");
        assert_eq!(super::oldest_sealed(&held[3..], 7, &[y], &all), Some((103, 6)), "the seal left the catalog: the oldest held");
        assert_eq!(super::oldest_sealed(&held, 4, &[y], &all), None, "only up to keep");
        let held2: Vec<(u64, u64, &[SegmentRecord])> = vec![(3, 100, &reused[..]), (5, 102, &sealed[..])];
        assert_eq!(super::oldest_sealed(&held2, 5, &[y], &all), Some((102, 5)), "another part of the slot is not this segment");
        let w = seg(3, 42, SegState::Sealed);
        let both = [y, w];
        let held3: Vec<(u64, u64, &[SegmentRecord])> = vec![(5, 102, &sealed[..]), (8, 105, &both[..])];
        assert_eq!(super::oldest_sealed(&held3, 8, &[y, w], &all), Some((105, 8)), "every freed segment sealed");
    }

    #[test]
    fn k1_retired_parts_are_named_only_below_keep() {
        let owned: Vec<(u64, Vec<u32>)> = vec![(3, vec![0, 1, 2]), (4, vec![0, 1, 2, 3]), (5, vec![3, 4]), (6, vec![3, 4, 5])];
        let gens: Vec<(u64, &[u32])> = owned.iter().map(|g| (g.0, &g.1[..])).collect();
        assert_eq!(super::retired_parts(&gens, 5).into_iter().collect::<Vec<_>>(), vec![0, 1, 2], "the compaction's inputs");
        assert!(super::retired_parts(&gens, 4).is_empty(), "keep still names them");
        assert!(super::retired_parts(&gens, 7).is_empty(), "keep is not in the catalog: nothing");
        assert!(super::retired_parts(&gens[..2], 3).is_empty());
        // a part keep lacks but a newer generation names stays
        let owned: Vec<(u64, Vec<u32>)> = vec![(3, vec![1, 2, 7]), (4, vec![2]), (5, vec![2, 7, 9])];
        let gens: Vec<(u64, &[u32])> = owned.iter().map(|g| (g.0, &g.1[..])).collect();
        assert_eq!(super::retired_parts(&gens, 4).into_iter().collect::<Vec<_>>(), vec![1]);
    }

    /// RECLAIMDEFER (i′): the sweep's pending parts leave out what a generation from keep on names (judged in the plan,
    /// not under `CATALOG`), keep one that only a generation below keep names (this reclaim retires it anyway), and
    /// search every held generation when keep is not held; the sweep dooms the same files as when `sweep_due` dropped
    /// every pending part any held generation names.
    #[test]
    fn a_sweeps_pending_parts_leave_out_what_keep_or_a_newer_generation_names() {
        let (g3, g4, g5) = (gen_of(3, &[(1, 0), (2, 0), (7, 0)]), gen_of(4, &[(2, 0), (8, 0)]), gen_of(5, &[(2, 0), (9, 0)]));
        let manifests: Vec<(u64, &[pgrc_format::manifest::PartRecord])> = [&g3, &g4, &g5].iter().map(|g| (g.gen, &g.manifest.parts[..])).collect();
        let pending = vec![1, 2, 6, 7, 8, 9];
        assert_eq!(super::sweep_pending(&manifests, 4, pending.clone()), vec![1, 6, 7], "keep (4) names 2 and 8, a newer one 9");
        assert_eq!(super::sweep_pending(&manifests, 6, pending.clone()), vec![6], "keep not held: every held generation is searched");
        // the old filter (sweep_due's, under CATALOG): every pending part any held generation names left out
        let held_any = |n: u32| manifests.iter().any(|g| g.1.iter().any(|p| p.part_no == n));
        let old: Vec<u32> = pending.iter().copied().filter(|&n| !held_any(n)).collect();
        assert_eq!(old, vec![6]);
        let retired = super::retired_parts(&manifests, 4);
        assert_eq!(retired.iter().copied().collect::<Vec<_>>(), vec![1, 7], "1 and 7: pending parts only a generation below keep (3) names");
        let doomed = |pend: &[u32]| (0..10).filter(|n| retired.contains(n) || pend.contains(n)).collect::<Vec<u32>>();
        assert_eq!(doomed(&super::sweep_pending(&manifests, 4, pending.clone())), doomed(&old), "the same files doomed");
        assert_eq!(doomed(&old), vec![1, 6, 7]);
        // keep not held: retired_parts retires nothing, and the full search leaves exactly the old filter's
        assert!(super::retired_parts(&manifests, 6).is_empty());
        assert_eq!(super::sweep_pending(&manifests, 6, pending), old);
    }

    /// A detached generation of `(part_no, dv_gen)` parts.
    fn gen_of(g: u64, parts: &[(u32, u64)]) -> crate::catalog::Generation {
        use pgrc_format::manifest::{Manifest, ManifestHeader, PartRecord};
        let recs: Vec<PartRecord> = parts.iter().map(|&(part_no, dv_gen)| PartRecord { rows: 3, file_len: 64, footer_off: 0, dv_gen, dv_len: 0, part_no, flags: 0, granule_count: 1, band_count: 1, dv_crc: 0, granule_rows: 0 }).collect();
        let header = ManifestHeader { gen: g, prev_gen: g - 1, publisher_fxid: 10 + g, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: recs.len() as u32, next_part_no: 16, flags: 0, reserved: 0 };
        crate::catalog::Generation::detached(Manifest::new(header, recs)).unwrap()
    }

    /// RECLAIMDEFER (i′) conditions 2 and (b): a hint made without the publish lock stands under it only on the very
    /// generations below keep it planned from (by identity), with keep not lower and the table still incremental; a
    /// publish since (a generation above them) leaves it standing.
    #[test]
    fn a_hint_stands_on_the_same_generations_below_keep_and_falls_on_any_change() {
        use std::sync::Arc;
        let g = |n: u64| Arc::new(gen_of(n, &[(n as u32, 0)]));
        let (g1, g2, g3, g4) = (g(1), g(2), g(3), g(4));
        let seen = |gens: &[&Arc<crate::catalog::Generation>]| super::Seen { gens: gens.iter().map(|g| Arc::clone(g)).collect(), all_dead: vec![], dead_files: vec![], sweep: None };
        let hint = super::Planned { keep_gen: 3, below: vec![Arc::clone(&g1), Arc::clone(&g2)], names: vec![], retired: vec![], dead: vec![], oldest: 1, sweep: false, facts_inc: true };
        assert!(super::hint_stands(&hint, &seen(&[&g1, &g2, &g3]), 3));
        assert!(super::hint_stands(&hint, &seen(&[&g1, &g2, &g3, &g4]), 3), "a publish since: a generation above");
        let twin = Arc::new(gen_of(2, &[(2, 0)]));
        assert!(!super::hint_stands(&hint, &seen(&[&g1, &twin, &g3]), 3), "a below generation replaced (its number, another object)");
        assert!(!super::hint_stands(&hint, &seen(&[&g2, &g3]), 3), "a below generation gone");
        assert!(!super::hint_stands(&hint, &seen(&[&g1, &g2, &g3]), 2), "the horizon's keep is lower under the lock");
        let mut s = seen(&[&g1, &g2, &g3]);
        s.all_dead = vec![9];
        assert!(!super::hint_stands(&hint, &s, 3), "a rolled-back generation's part since");
        let mut s = seen(&[&g1, &g2, &g3]);
        s.dead_files = vec!["manifest-7.pgrcm".into()];
        assert!(!super::hint_stands(&hint, &s, 3), "a rolled-back generation's file since");
        let mut s = seen(&[&g1, &g2, &g3]);
        s.sweep = Some(vec![]);
        assert!(!super::hint_stands(&hint, &s, 3), "a sweep due since");
    }

    /// K1-RECLAIM: an incremental reclaim names from the held generations exactly what the full rules unlink of the
    /// files they imply; the strays (another life's manifest and run, an aborted writer's DV, a dead part, the optional
    /// sidecars of the parts it retired) wait for, and are unlinked by, the full sweep.
    #[test]
    fn k1_reclaim_incremental_names_the_retired_files_and_the_sweep_takes_the_strays() {
        use pgrc_format::dirlayout::{manifest_file_name as man, part_file_name as part, run_file_name, sections_file_name, sidecar_file_name as side};
        use pgrc_format::sidecar::SidecarKind::{Dv, Fp128, Memo, RowIdentMap as Rm, Stats};
        use std::collections::BTreeSet;
        let (g3, g4, g5, g6) = (gen_of(3, &[(0, 0), (1, 3), (2, 0)]), gen_of(4, &[(0, 0), (1, 4), (2, 0), (3, 0)]), gen_of(5, &[(1, 4), (3, 0), (4, 0)]), gen_of(6, &[(1, 4), (3, 0), (4, 0), (5, 0)]));
        let manifests: Vec<(u64, &[pgrc_format::manifest::PartRecord])> = [&g3, &g4, &g5, &g6].iter().map(|g| (g.gen, &g.manifest.parts[..])).collect();
        let below = [&g3, &g4];
        let mut dir: BTreeSet<String> = BTreeSet::new();
        for pn in 0..6 {
            dir.insert(part(pn));
            dir.insert(side(pn, Rm, 1));
        }
        for g in 3..7 {
            dir.insert(man(g));
        }
        dir.extend([sections_file_name(3), side(1, Dv, 3), side(1, Dv, 4), "CURRENT".to_string()]);
        let strays = [man(2), run_file_name(2, 0), side(3, Dv, 4), part(7), side(7, Rm, 1), side(0, Memo, 2)];
        dir.extend([side(0, Fp128, 2), side(2, Stats, 1), side(4, Stats, 1)]);
        dir.extend(strays.iter().cloned());
        let facts = super::ReclaimFacts { manifests: &manifests, below: &below, keep: &g5, dead: &[7], dead_files: &[], natts: 2, runs: super::kept_runs(&g5), run_revs: super::run_revs(&g5.manifest) };
        fn exists(d: &BTreeSet<String>) -> impl FnMut(&str) -> bool + '_ {
            move |n: &str| d.contains(n)
        }
        let inc = super::plan_reclaim(&facts, &[], None, &mut exists(&dir));
        let expect: Vec<String> = [part(0), part(2), part(7), side(0, Rm, 1), side(2, Rm, 1), side(7, Rm, 1), side(0, Fp128, 2), side(2, Stats, 1), side(1, Dv, 3), man(3), man(4), sections_file_name(3)].into_iter().collect::<BTreeSet<_>>().into_iter().collect();
        assert_eq!(inc.names, expect);
        assert_eq!(inc.retired, vec![0, 2, 7]);
        // a rolled-back generation's own files (the catalog's dead files) are named without a listing, once below keep
        let dead_files = [man(2), run_file_name(2, 0), man(8)];
        let with_dead = super::ReclaimFacts { dead_files: &dead_files, ..facts };
        let inc_dead = super::plan_reclaim(&with_dead, &[], None, &mut exists(&dir));
        assert!(inc_dead.names.contains(&man(2)) && inc_dead.names.contains(&run_file_name(2, 0)) && !inc_dead.names.contains(&man(8)));
        let facts = with_dead;
        let facts = super::ReclaimFacts { dead_files: &[], ..facts };
        let all: Vec<String> = dir.iter().cloned().collect();
        let full = super::plan_reclaim(&facts, &[], Some(&all), &mut |_| true);
        let mut union: BTreeSet<String> = inc.names.iter().cloned().collect();
        union.extend(strays.iter().cloned());
        assert_eq!(full.names, union.into_iter().collect::<Vec<_>>(), "the full rules: the incremental names and every stray");
        // the directory after the incremental reclaim; the next generation's reclaim (keep 6) sweeps with the pending parts
        let after: Vec<String> = dir.iter().filter(|n| inc.names.binary_search(n).is_err()).cloned().collect();
        let manifests6 = &manifests[2..];
        let below6 = [&g5];
        let facts6 = super::ReclaimFacts { manifests: manifests6, below: &below6, keep: &g6, dead: &[], dead_files: &[], natts: 2, runs: super::kept_runs(&g6), run_revs: super::run_revs(&g6.manifest) };
        let inc6 = super::plan_reclaim(&facts6, &[], None, &mut exists(&after.iter().cloned().collect()));
        assert_eq!(inc6.names, vec![man(5)], "an incremental reclaim leaves the strays");
        let sweep = super::plan_reclaim(&facts6, &inc.retired, Some(&after), &mut |_| true);
        let mut want: Vec<String> = strays.iter().filter(|n| **n != part(7) && **n != side(7, Rm, 1)).cloned().collect();
        want.push(man(5));
        want.sort();
        assert_eq!(sweep.names, want, "the sweep takes the strays and the retired parts' optional sidecars");
    }

    /// SORTRUNS-E0 D2 lifecycle: a run's reverse is unlinked once `keep` no longer names its run (the run retired), by
    /// the incremental reclaim and the sweep alike; a run `keep` names keeps its reverse though a member of it retired
    /// below `keep` (an in-place rewrite moved the slot); a reverse above `keep` (a newer publish's) waits; a stray at
    /// or below `keep` that no generation names goes with the sweep.
    #[test]
    fn sortruns_reclaim_unlinks_a_reverse_only_once_its_run_retired() {
        use pgrc_format::dirlayout::{manifest_file_name as man, part_file_name as part, runrev_file_name as rr};
        use pgrc_format::sortrun::{with_runs, RankForm, RunRevForm, SortRun};
        use std::collections::BTreeSet;
        use std::sync::Arc;
        let run = |id: u64, members: &[u32]| SortRun { run_id: id, level: 1, rev: RunRevForm::Labels, rank: RankForm::Dense, rows: 291, rev_len: 4096, rev_crc: 1, members: members.to_vec(), ranges: vec![(291 * id, 291 * (id + 1))] };
        let with = |mut g: crate::catalog::Generation, runs: &[SortRun]| {
            let m = (*g.manifest).clone();
            let ext = with_runs(&m.ext, runs).unwrap();
            g.manifest = Arc::new(m.with_ext(ext));
            g
        };
        // g3: run 2 (parts 0, 1) and run 3 (part 2); g4 and g5 (keep): part 1 rewritten as part 3 in run 2's slot, run
        // 3 retired with part 2
        let g3 = with(gen_of(3, &[(0, 0), (1, 0), (2, 0)]), &[run(2, &[0, 1]), run(3, &[2])]);
        let g4 = with(gen_of(4, &[(0, 0), (3, 0)]), &[run(2, &[0, 3])]);
        let g5 = with(gen_of(5, &[(0, 0), (3, 0)]), &[run(2, &[0, 3])]);
        let manifests: Vec<(u64, &[pgrc_format::manifest::PartRecord])> = [&g3, &g4, &g5].iter().map(|g| (g.gen, &g.manifest.parts[..])).collect();
        let below = [&g3, &g4];
        let dir: BTreeSet<String> = [part(0), part(1), part(2), part(3), man(3), man(4), man(5), rr(1), rr(2), rr(3), rr(6)].into_iter().collect();
        let facts = super::ReclaimFacts { manifests: &manifests, below: &below, keep: &g5, dead: &[], dead_files: &[], natts: 2, runs: super::kept_runs(&g5), run_revs: super::run_revs(&g5.manifest) };
        assert_eq!(facts.run_revs.iter().copied().collect::<Vec<_>>(), vec![2]);
        let inc = super::plan_reclaim(&facts, &[], None, &mut |n| dir.contains(n));
        assert!(inc.names.contains(&rr(3)), "run 3 retired below keep: {:?}", inc.names);
        assert!(!inc.names.contains(&rr(2)), "keep names run 2");
        assert!(!inc.names.contains(&rr(6)) && !inc.names.contains(&rr(1)), "no held generation names 6 or 1");
        assert!(inc.names.contains(&part(1)) && inc.names.contains(&part(2)), "the retired members' parts go");
        let all: Vec<String> = dir.iter().cloned().collect();
        let full = super::plan_reclaim(&facts, &[], Some(&all), &mut |_| true);
        let revs: Vec<&String> = full.names.iter().filter(|n| n.starts_with("runrev-")).collect();
        assert_eq!(revs, vec![&rr(1), &rr(3)], "the sweep: the retired run's and the stray at or below keep");
    }

    use super::checked_varlena_len;
    use types_tuple::varatt;

    /// A 4B-U varlena image whose header claims `claim` total bytes, laid
    /// into a buffer whose backing is exactly `backed` bytes (the tuple
    /// witness). Native-endian, matching `varsize_4b`'s unaligned read.
    fn mk_4b(claim: usize, backed: usize) -> Vec<u8> {
        let word = varatt::set_varsize_4b_word(claim as u32);
        let mut v = vec![0u8; backed.max(varatt::VARHDRSZ)];
        v[..varatt::VARHDRSZ].copy_from_slice(&word.to_ne_bytes());
        v.truncate(backed);
        v
    }

    #[test]
    fn honest_4b_header_within_witness_is_accepted() {
        // Header claims exactly the 8 backed bytes: honored.
        let img = mk_4b(8, 8);
        assert_eq!(checked_varlena_len(&img).expect("in bounds"), 8);
    }

    #[test]
    fn overclaiming_4b_header_is_refused_not_overread() {
        // The exploit shape: a 4B-U header claiming far more than the tuple
        // backs (8 bytes). Must refuse typed, never size the OOB slice.
        let img = mk_4b(1_000_000, 8);
        let err = checked_varlena_len(&img).expect_err("over-claim must refuse");
        assert_eq!(
            err.sqlstate(),
            types_error::ERRCODE_DATA_CORRUPTED,
            "over-claiming varlena header must be a typed data-corruption error"
        );
    }

    #[test]
    fn truncated_4b_header_is_refused() {
        // Fewer bytes backed than the 4-byte header itself: refuse before
        // the header read.
        let img = vec![0u8; 2]; // even b0 => 4B form, only 2 backed
        assert!(checked_varlena_len(&img).is_err());
    }

    #[test]
    fn short_1b_header_bounds_are_enforced() {
        // 1B short header, total = 3 (incl. header), 3 bytes backed: honored.
        let ok = vec![(3u8 << 1) | 0x01, b'a', b'b'];
        assert_eq!(checked_varlena_len(&ok).expect("in bounds"), 3);
        // 1B short header claiming 100 bytes over a 3-byte witness: refused.
        let bad = vec![(100u8 << 1) | 0x01, b'a', b'b'];
        assert!(checked_varlena_len(&bad).is_err());
    }
}

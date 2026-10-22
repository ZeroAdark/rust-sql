//! [fmt-land] Bank-grain stats plane, generated AS DATA IS COPIED IN
//! (the user 2026-08-17: "doing landing the bankstats plane sgtm. We should
//! generate it as we copy data in").
//!
//! Layout/validation law: `pgrc_format::bankstats` (the fmt-layout lane's
//! format module). This module is the WRITE-PATH producer: every
//! `publish_parts` generation folds the freshly sealed parts' captured
//! slices (`SealedPart::plane_slices` — a SEAL byproduct, never a file
//! read-back) together with the prior generation's plane into
//! `bankstats-<gen>.pgrcbs`, so freshly written banks carry the plane
//! from birth — no offline `--bankstats-build` pass required (the bench
//! builder stays as the BACKFILL tool for pre-existing banks).
//!
//! Discipline (the sidecar.rs doctrine, applied verbatim):
//! - DERIVED artifact: a pure function of the sealed parts, rebuildable at
//!   any time; never the source of truth. The reader's validity witness
//!   (gen + schema fingerprint + full per-part identity vector) binds it
//!   to one exact part-set — a compacted/appended/foreign part-set fails
//!   the witness and the reader falls back to per-part sections.
//! - BEST-EFFORT: any failure here (missing prior plane, IO error,
//!   arity skew) SKIPS the plane and never fails the publish — an absent
//!   plane is a slower read, never a wrong one.
//! - CRASH story: the image is logged into `<final>.tmp` and renamed by the
//!   generation's publish record, so the final name appears with its
//!   manifest or not at all; an orphan `.tmp` is end-of-recovery cleanup's.
//!   A torn/forged final image fails the meta crc / column crc / identity
//!   witness and is refused typed (`pgrc_format::bankstats::decode_meta` +
//!   `ColPayloadRef::new`).
//! - FIRST PUBLISH ONLY (P-1, OC-2 a): the plane is written by a publish
//!   over an empty base (a bulk load's first COPY), which is O(new); any
//!   later publish drops it (the carry-forward, O(parts) per publish, is
//!   gone). The generation-grain stats live in the stats runs (`stats.rs`).
//! - GC: old-generation planes persist until the compaction/GC rung,
//!   exactly like `.pgrcs` companions (sidecar.rs posture). When the M5
//!   compaction rung rewrites a part-set, its own publish writes the new
//!   generation's plane and the old ones are dead bytes with a failed
//!   witness, never misread.
//!
//! Kill switch: `PGRUST_PGRC_BANKSTATS_SEAL=0|off` skips generation (the
//! A/B ingest-cost arm; default ON — the plane is a separate sidecar file,
//! part/manifest bytes are IDENTICAL either way, so blessed bank dirshas
//! over format files are unaffected; ledger row in lint-determinism.allow).

use crate::seal::SealedPart;
use crate::wvfs::WriteVfs;
use pgrc_format::bankstats as fb;
use pgrc_format::manifest::Manifest;

/// Kill switch (default ON). Read once, `pgsync::OnceLock`-cached.
pub fn seal_plane_enabled() -> bool {
    static F: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *F.get_or_init(|| {
        !matches!(
            std::env::var("PGRUST_PGRC_BANKSTATS_SEAL").as_deref(),
            Ok("0") | Ok("off")
        )
    })
}

/// The publish outcome witness for the plane write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaneOutcome {
    /// Written + renamed; carries the final file name and image bytes.
    Written { file_name: String, bytes: u64 },
    /// Kill switch off.
    Disabled,
    /// Base parts exist but no valid prior plane to carry forward —
    /// the backfill tool owns this bank's first plane. (Not produced since P-1: a base with parts is `Skipped`.)
    NoPriorPlane,
    /// Structural skip (arity/column-set skew, IO failure). The publish
    /// itself succeeded; the plane is simply absent for this generation.
    Skipped { reason: &'static str },
}

fn ident_of(p: &pgrc_format::manifest::PartRecord) -> fb::PartIdent {
    fb::PartIdent {
        part_no: p.part_no,
        granule_count: p.granule_count,
        band_count: p.band_count,
        rows: p.rows,
        footer_off: p.footer_off,
    }
}

/// A Stats body's part-grain record: its last `STATS_RECORD_LEN` bytes (empty when none).
pub fn part_record(body: &[u8]) -> &[u8] {
    let n = body.len() / pgrc_format::meta::STATS_RECORD_LEN;
    if n == 0 { &[] } else { &body[(n - 1) * pgrc_format::meta::STATS_RECORD_LEN..n * pgrc_format::meta::STATS_RECORD_LEN] }
}

/// Stage `bankstats-<gen>.pgrcbs` for the manifest being published: the image is written to its `.tmp` name
/// through `vfs` (the publisher's logged vfs), and the rename into place rides the publish record.
/// Only over an empty base (OC-2 a, P-1): `manifest.parts` = the `sealed` parts in order. Best-effort by doctrine: a
/// skipped plane is `(outcome, None)`, never an error.
pub fn stage_plane(
    vfs: &mut dyn WriteVfs,
    dir: &str,
    base: Option<&Manifest>,
    manifest: &Manifest,
    sealed: &[SealedPart],
) -> (PlaneOutcome, Option<(String, String)>) {
    let skip = |o: PlaneOutcome| (o, None);
    if !seal_plane_enabled() {
        return skip(PlaneOutcome::Disabled);
    }
    // OC-2 (a), P-1: the plane is written only over an empty base, which is O(new); any later publish drops it.
    if base.is_some_and(|b| !b.parts.is_empty()) {
        return skip(PlaneOutcome::Skipped { reason: "base has parts" });
    }
    // Canonical column set: the first sealed part's captured attno list
    // (attno-sorted at seal); every sealed part must agree.
    let Some(first) = sealed.first() else {
        return skip(PlaneOutcome::Skipped {
            reason: "no sealed parts",
        });
    };
    let attnos: Vec<u32> = first.plane_slices.iter().map(|s| s.attno).collect();
    if attnos.is_empty() {
        return skip(PlaneOutcome::Skipped {
            reason: "no plane slices captured",
        });
    }
    for s in sealed {
        if s.plane_slices.len() != attnos.len()
            || s.plane_slices
                .iter()
                .zip(attnos.iter())
                .any(|(sl, a)| sl.attno != *a)
        {
            return skip(PlaneOutcome::Skipped {
                reason: "sealed-part column-set skew",
            });
        }
    }
    // Assemble ColInputs from the sealed slices (borrowed from the seal captures).
    let mut cols: Vec<fb::ColInput<'_>> = Vec::with_capacity(attnos.len());
    for (ci, attno) in attnos.iter().enumerate() {
        let mut bodies: Vec<&[u8]> = Vec::with_capacity(manifest.parts.len());
        let mut digests: Vec<Option<[u8; fb::BANKSTATS_DIGEST_LEN]>> =
            Vec::with_capacity(manifest.parts.len());
        for s in sealed {
            let sl = &s.plane_slices[ci];
            bodies.push(&sl.stats_body);
            digests.push(sl.digest);
        }
        cols.push(fb::ColInput {
            attno: *attno,
            stats_bodies: bodies,
            digests,
        });
    }
    let idents: Vec<fb::PartIdent> = manifest.parts.iter().map(ident_of).collect();
    let Ok(image) = fb::build_image(
        manifest.header.gen,
        manifest.header.schema_fingerprint,
        &idents,
        &cols,
    ) else {
        return skip(PlaneOutcome::Skipped {
            reason: "build_image refused",
        });
    };
    let final_name = fb::bankstats_file_name(manifest.header.gen);
    let tmp_name = format!("{final_name}.tmp");
    let write = (|| -> crate::WriteResult<()> {
        let fd = vfs.create_rw(&format!("{dir}/{tmp_name}"))?;
        vfs.pwrite_at(&fd, 0, &image)?;
        vfs.close_file(fd)
    })();
    match write {
        Ok(()) => (
            PlaneOutcome::Written { file_name: final_name.clone(), bytes: image.len() as u64 },
            Some((tmp_name, final_name)),
        ),
        Err(_) => skip(PlaneOutcome::Skipped { reason: "plane write io" }),
    }
}

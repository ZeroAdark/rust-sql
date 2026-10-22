//! The publish plan and the file operations the primary and redo share (design §7.4, §7.6): one `apply_*` per record.

use pgrc_format::dirlayout::{manifest_file_name, part_file_name, CURRENT_FILE_NAME, CURRENT_TMP_FILE_NAME};
use pgrc_format::class::ColSchema;
use pgrc_format::ident::schema_fingerprint;
use pgrc_format::manifest::{entries_of, has_tag, schemas, with_schemas, CommitPointer, Manifest, ManifestHeader, PartRecord, SchemaEntry, EXT_SCHEMAS, MANIFESTF_SCHEMAS, MANIFEST_MAGIC};
use pgrc_format::part::{FooterFixed, PartHeader, FOOTER_FIXED_LEN, PART_HEADER_LEN};
use pgrc_format::sections::{decode_front, read_front_bytes};
use pgrc_format::spool::MaskedCrc;
use pgrc_format::wal::{DirLoc, FileRefRec, InstallRec, PartInstall, PublishKind, PublishRec, ReclaimRec};
use pgrc_format::wire::crc32c;
use pgrc_format::{FormatError, FORMAT_VERSION};

use crate::seal::{PartSpec, SealedPart};
use crate::wvfs::WriteVfs;
use crate::{WriteError, WriteResult};

/// Primary: every named source must exist. Redo: a missing one was already applied, or a later record removed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyMode {
    Primary,
    Redo,
}

pub fn dir_loc(spec: &PartSpec) -> DirLoc {
    DirLoc { spc: spec.spc, db: spec.db, relnumber: spec.relfilenumber as u32 }
}

fn path(dir: &str, name: &str) -> String {
    format!("{dir}/{name}")
}

fn is_enoent(e: &WriteError) -> bool {
    matches!(e, WriteError::Io { errno, .. } if *errno == libc::ENOENT)
}

/// Generation `gen`'s manifest, or `None` when its file is missing or does not decode.
pub fn read_manifest(vfs: &mut dyn WriteVfs, dir: &str, gen: u64) -> Option<Manifest> {
    let p = path(dir, &manifest_file_name(gen));
    if !matches!(vfs.exists_path(&p), Ok(true)) {
        return None;
    }
    let (bytes, _) = read_front_bytes(&mut |len| vfs.read_prefix(&p, len)).ok()?;
    decode_front(&bytes).ok().map(|(m, _)| m)
}

/// A publish ready to log: the record and the manifest it produces.
#[derive(Debug, Clone)]
pub struct PublishPlan {
    pub rec: PublishRec,
    pub manifest: Manifest,
    /// `manifest`'s file image (encoded once for the record's crc; the publisher writes it: [`apply_planned`]).
    bytes: Vec<u8>,
}

impl PublishPlan {
    pub fn build<'b>(base: Option<&'b Manifest>, spec: &PartSpec, fxid: u64, kind: PublishKind) -> PlanBuilder<'b> {
        PlanBuilder {
            base,
            spec: *spec,
            fxid,
            kind,
            next_part_no: base.map_or(0, |b| b.header.next_part_no),
            used_gen: 0,
            upsert: Vec::new(),
            retire: Vec::new(),
            ext: base.map(|b| b.ext.clone()).unwrap_or_default(),
            installs: Vec::new(),
            renames: Vec::new(),
            described: Vec::new(),
            ranges: Vec::new(),
            repoint: Vec::new(),
        }
    }

    /// Renames do not change the manifest, so files staged after the plan was built can still join it.
    pub fn rename(&mut self, from: String, to: String) {
        self.rec.renames.push((from, to));
    }

    pub fn part_nos(&self) -> Vec<u32> {
        self.rec.installs.iter().map(|i| i.part_no).collect()
    }

    /// Replace the extension block after `finish` (the stats root, P-1: its run bytes depend on the part set the
    /// plan fixed). The manifest, the record's header and its crc follow.
    pub fn set_ext(&mut self, ext: Vec<u8>) {
        let header = self.manifest.header;
        self.manifest = std::mem::replace(&mut self.manifest, Manifest::new(header, Vec::new())).with_ext(ext.clone());
        self.rec.header = self.manifest.header;
        self.rec.ext = ext;
        self.bytes = self.manifest.encode();
        self.rec.manifest_crc = trailing_crc(&self.bytes);
    }

    /// The record, the manifest and its file image.
    pub fn into_parts(self) -> (PublishRec, Manifest, Vec<u8>) {
        (self.rec, self.manifest, self.bytes)
    }
}

pub struct PlanBuilder<'b> {
    base: Option<&'b Manifest>,
    spec: PartSpec,
    fxid: u64,
    kind: PublishKind,
    next_part_no: u32,
    /// GENID: the highest generation number the directory already used ([`PlanBuilder::number_above`]).
    used_gen: u64,
    upsert: Vec<PartRecord>,
    retire: Vec<u32>,
    ext: Vec<u8>,
    installs: Vec<PartInstall>,
    renames: Vec<(String, String)>,
    /// FASTADD: schemas this plan describes ([`PlanBuilder::describe`]): the table's schema before a metadata-only
    /// ALTER, which the parts written under it keep.
    described: Vec<SchemaEntry>,
    /// IX0: the row-ID ranges of the parts this plan installs (`SealedPart::ids`, or [`PlanBuilder::set_ranges`]).
    ranges: Vec<(u32, pgrc_format::manifest::IdRanges)>,
    /// SORTRUNS-E0: run members this plan rewrites in place, `(retired member, its replacement)`
    /// ([`PlanBuilder::repoint_member`]).
    repoint: Vec<(u32, u32)>,
}

impl PlanBuilder<'_> {
    /// FASTADD (pgrc-plans/FASTADD.done): describe a schema some carried part may have been written with, other than
    /// the publisher's: the ALTER TABLE that adds or drops columns without a rewrite describes the table's schema
    /// before it, so the base's parts keep reading under it ([`pgrc_format::manifest::EXT_SCHEMAS`]). A base whose
    /// header fingerprint is not the publisher's is refused unless it is described here.
    pub fn describe(&mut self, cols: Vec<ColSchema>) {
        let fingerprint = schema_fingerprint(&cols);
        if !self.described.iter().any(|e| e.fingerprint == fingerprint) {
            self.described.push(SchemaEntry { fingerprint, cols, runs: Vec::new() });
        }
    }

    /// Install a sealed part under the next part number.
    pub fn install(&mut self, sealed: &SealedPart) -> WriteResult<u32> {
        let part_no = self.next_part_no;
        self.install_as(sealed, part_no)?;
        Ok(part_no)
    }

    pub fn install_as(&mut self, s: &SealedPart, part_no: u32) -> WriteResult<()> {
        let mut header = Vec::with_capacity(PART_HEADER_LEN);
        PartHeader::new(part_no, self.spec.schema_fingerprint, self.spec.spc, self.spec.db, self.spec.relfilenumber)
            .encode_into(&mut header);
        let mut footer = FooterFixed::decode(&s.footer_image)?;
        footer.part_no = part_no;
        let granule_rows = footer.granule_rows;
        let mut footer_bytes = Vec::with_capacity(FOOTER_FIXED_LEN);
        footer.encode_into(&mut footer_bytes);
        self.installs.push(PartInstall { tmp: s.tmp_name.clone(), part_no, header, footer_off: s.footer_off, footer: footer_bytes });
        self.set_ranges(part_no, s.ids.clone());
        self.upsert.push(PartRecord {
            rows: s.rows,
            file_len: s.file_len,
            footer_off: s.footer_off,
            dv_gen: 0,
            dv_len: 0,
            part_no,
            flags: 0,
            granule_count: s.granule_count,
            band_count: s.band_count,
            dv_crc: 0,
            granule_rows,
        });
        self.next_part_no = self.next_part_no.max(part_no + 1);
        Ok(())
    }

    /// IX0: the row IDs part `part_no` (installed by this plan) homes; the last call for a number wins.
    pub fn set_ranges(&mut self, part_no: u32, ids: pgrc_format::manifest::IdRanges) {
        self.ranges.retain(|r| r.0 != part_no);
        self.ranges.push((part_no, ids));
    }

    pub fn upsert(&mut self, rec: PartRecord) {
        self.next_part_no = self.next_part_no.max(rec.part_no + 1);
        self.upsert.push(rec);
    }

    pub fn retire(&mut self, part_no: u32) {
        self.retire.push(part_no);
    }

    /// Set the `flags` of the record this plan upserts for `part_no` (K1: a compaction output's map form and
    /// `PART_FLAG_COMPACTED`).
    pub fn set_flags(&mut self, part_no: u32, flags: u32) {
        self.upsert.iter_mut().filter(|u| u.part_no == part_no).for_each(|u| u.flags = flags);
    }

    pub fn rename(&mut self, from: String, to: String) {
        self.renames.push((from, to));
    }

    /// SORTRUNS-E0 L2: the DV triplet of the record this plan upserts for `part_no` (a run member's carried rows).
    pub fn set_dv(&mut self, part_no: u32, dv_gen: u64, dv_len: u64, dv_crc: u32) {
        self.upsert.iter_mut().filter(|u| u.part_no == part_no).for_each(|u| (u.dv_gen, u.dv_len, u.dv_crc) = (dv_gen, dv_len, dv_crc));
    }

    pub fn ext(&mut self, ext: Vec<u8>) {
        self.ext = ext;
    }

    /// SORTRUNS-E0: the generation's sorted runs ([`pgrc_format::sortrun`]; none when empty), replacing the base's.
    /// The members' slots then follow this plan's retirements at [`PlanBuilder::finish`].
    pub fn set_runs(&mut self, runs: &[pgrc_format::sortrun::SortRun]) -> WriteResult<()> {
        self.ext = pgrc_format::sortrun::with_runs(&self.ext, runs)?;
        Ok(())
    }

    /// SORTRUNS-E0: part `to` (which this plan upserts) takes run member `from`'s slot (which it retires): a member
    /// rewritten in place, its virtual frames kept, so the run's reverse stays valid. Without it a retired member's
    /// slot empties.
    pub fn repoint_member(&mut self, from: u32, to: u32) {
        self.repoint.retain(|r| r.0 != from);
        self.repoint.push((from, to));
    }

    /// SEAL-OFFHEAD: this plan upserts or retires `part_no`.
    pub fn touches(&self, part_no: u32) -> bool {
        self.upsert.iter().any(|u| u.part_no == part_no) || self.retire.contains(&part_no)
    }

    /// SEAL-OFFHEAD: the generation's `EXT_UNDER` tags (none when empty), after the edit (an edit that rebuilds the
    /// extension block from the base's carries the base's).
    pub fn set_under(&mut self, entries: &[pgrc_format::manifest::UnderEntry]) -> WriteResult<()> {
        self.ext = pgrc_format::manifest::with_under(&self.ext, entries)?;
        Ok(())
    }

    /// The publishing transaction.
    pub fn fxid(&self) -> u64 {
        self.fxid
    }

    /// GENID: number the generation above `used.0` and the parts this plan installs or reserves from `used.1` on,
    /// whatever the base says: the numbers the process already handed out in the directory
    /// ([`crate::publish::TxnProbe::used_numbers`]). Before any install or reservation.
    pub fn number_above(&mut self, used: (u64, u32)) {
        self.used_gen = self.used_gen.max(used.0);
        self.next_part_no = self.next_part_no.max(used.1);
    }

    /// The generation this plan publishes ([`crate::publish::next_gen`] of its base).
    pub fn gen(&self) -> u64 {
        crate::publish::next_gen(self.base.map_or(0, |b| b.header.gen), (self.used_gen, 0))
    }

    /// Reserve the next part number for a part a later publish installs (a segment's, design §4.3).
    pub fn reserve(&mut self) -> u32 {
        self.next_part_no += 1;
        self.next_part_no - 1
    }

    pub fn finish(mut self) -> WriteResult<PublishPlan> {
        let spec = self.spec;
        if let Some(b) = self.base {
            if b.header.relfilenumber != spec.relfilenumber {
                return Err(WriteError::Contract { detail: "manifest base identity mismatch" });
            }
        }
        let prev_gen = self.base.map_or(0, |b| b.header.gen);
        let gen = self.gen();
        let parts = try_merged_parts(self.base, &self.upsert, &self.retire)?;
        // IX0 L2: a part this plan installs or retires changes the row-ID directory; a plan that does neither keeps the
        // base's records as they are (no pass over the parts)
        if !self.ranges.is_empty() || !self.retire.is_empty() {
            self.ext = pgrc_format::manifest::rebuild_ranges(&std::mem::take(&mut self.ext), &parts, &self.ranges)?;
        }
        // SORTRUNS-E0: a retired member leaves its run's slot (or hands it to its in-place rewrite); a run left with
        // no member goes. The edited runs are checked against the generation's parts here, before the record is
        // logged, as a decode checks them.
        let runs = pgrc_format::sortrun::has_runs(&self.ext);
        if runs && !self.retire.is_empty() {
            self.ext = pgrc_format::sortrun::retire_members(&self.ext, &self.retire, &self.repoint)?;
        }
        self.ext = schemas_ext(self.base, spec.schema_fingerprint, &self.installs, &self.described, &parts, std::mem::take(&mut self.ext))?;
        let header = ManifestHeader {
            gen,
            prev_gen,
            publisher_fxid: self.fxid,
            relfilenumber: spec.relfilenumber,
            schema_fingerprint: spec.schema_fingerprint,
            magic: MANIFEST_MAGIC,
            format_version: FORMAT_VERSION,
            spc: spec.spc,
            db: spec.db,
            part_count: parts.len() as u32,
            next_part_no: self.next_part_no,
            flags: 0,
            reserved: 0,
        };
        let manifest = Manifest::new(header, parts).with_ext(self.ext.clone());
        if runs {
            pgrc_format::sortrun::check(&manifest)?;
        }
        let bytes = manifest.encode();
        let rec = PublishRec {
            dir: dir_loc(&spec),
            base_gen: prev_gen,
            kind: self.kind,
            header: manifest.header,
            upsert: self.upsert,
            retire: self.retire,
            ext: self.ext,
            installs: self.installs,
            renames: self.renames,
            manifest_crc: trailing_crc(&bytes),
        };
        Ok(PublishPlan { rec, manifest, bytes })
    }
}

/// FASTADD: `ext` with the `EXT_SCHEMAS` record of the manifest `parts` make under the publisher's fingerprint `fp`.
/// Every part's schema: a part this plan installs was written with `fp`; a part of the base keeps the base's (its
/// run's, else the base header's); any other part (carried from elsewhere) is the publisher's. The parts of a schema
/// other than `fp` are listed by `part_no` runs (manifest-consecutive parts of one schema), and the schema must be
/// described, by the base's record or by [`PlanBuilder::describe`]; otherwise the publish is refused as before
/// ("manifest base identity mismatch"). A base whose header fingerprint is not `fp` must have it described even with
/// no part left under it (a publisher of a stale schema never moves the header back). Descriptions no part needs
/// are dropped. With nothing to list (the base's fingerprint is `fp` and the base lists no part), `ext` is returned
/// as it is: no work over the parts.
fn schemas_ext(base: Option<&Manifest>, fp: u64, installs: &[PartInstall], described: &[SchemaEntry], parts: &[PartRecord], ext: Vec<u8>) -> WriteResult<Vec<u8>> {
    let refuse = || WriteError::Contract { detail: "manifest base identity mismatch" };
    let base_fp = base.map_or(fp, |b| b.header.schema_fingerprint);
    let base_lists = base.is_some_and(|b| b.header.flags & MANIFESTF_SCHEMAS != 0);
    if base_fp == fp && !base_lists {
        return Ok(if has_tag(&ext, EXT_SCHEMAS) { with_schemas(&ext, &[])? } else { ext });
    }
    let base_entries = match base {
        Some(b) if base_lists => schemas(b)?,
        _ => Vec::new(),
    };
    let known = |f: u64| base_entries.iter().chain(described).find(|e| e.fingerprint == f);
    if base_fp != fp && known(base_fp).is_none() {
        return Err(refuse());
    }
    let base_parts = base.map_or(&[][..], |b| &b.parts[..]);
    let mut installed: Vec<u32> = installs.iter().map(|i| i.part_no).collect();
    installed.sort_unstable();
    let mut j = 0;
    let fps = parts.iter().map(|p| {
        while j < base_parts.len() && base_parts[j].part_no < p.part_no {
            j += 1;
        }
        let f = if installed.binary_search(&p.part_no).is_ok() || base_parts.get(j).is_none_or(|b| b.part_no != p.part_no) {
            fp
        } else {
            base_entries.iter().find(|e| e.holds(p.part_no)).map_or(base_fp, |e| e.fingerprint)
        };
        (p.part_no, f)
    });
    let entries = entries_of(fps, fp, &mut |f| known(f).map(|e| e.cols.clone())).map_err(|_| refuse())?;
    Ok(with_schemas(&ext, &entries)?)
}

fn trailing_crc(manifest_bytes: &[u8]) -> u32 {
    pgrc_format::sections::manifest_crc(manifest_bytes)
}

#[cfg(test)]
pub(crate) fn merged_parts(base: Option<&Manifest>, upsert: &[PartRecord], retire: &[u32]) -> Vec<PartRecord> {
    try_merged_parts(base, upsert, retire).expect("test merge")
}

/// The base's parts (`part_no` ascending) less `retire`, then `upsert` by `part_no` (the last upsert of a number wins;
/// a retired number upserted again comes back), `part_no` ascending. One merge (P-1 L4a): O(base + k log k) for k
/// changes, where a scan of the list per upsert and per retire was O(base × k).
pub(crate) fn try_merged_parts(base: Option<&Manifest>, upsert: &[PartRecord], retire: &[u32]) -> WriteResult<Vec<PartRecord>> {
    let mut up: Vec<(u32, usize)> = upsert.iter().enumerate().map(|(i, p)| (p.part_no, i)).collect();
    up.sort_unstable();
    let up: Vec<PartRecord> = up.iter().enumerate().filter(|&(k, x)| up.get(k + 1).is_none_or(|n| n.0 != x.0)).map(|(_, x)| upsert[x.1]).collect();
    let mut gone: Vec<u32> = pgrc_format::try_to_vec(retire, "plan")?;
    gone.sort_unstable();
    let base = base.map_or(&[][..], |b| &b.parts[..]);
    let mut out = pgrc_format::try_vec(base.len() + up.len(), "plan")?;
    let mut j = 0;
    for b in base.iter().filter(|p| gone.binary_search(&p.part_no).is_err()) {
        while j < up.len() && up[j].part_no < b.part_no {
            out.push(up[j]);
            j += 1;
        }
        if j < up.len() && up[j].part_no == b.part_no {
            out.push(up[j]);
            j += 1;
        } else {
            out.push(*b);
        }
    }
    out.extend_from_slice(&up[j..]);
    Ok(out)
}

#[cfg(test)]
pub fn manifest_of(base: Option<&Manifest>, rec: &PublishRec) -> Manifest {
    try_manifest_of(base, rec).expect("test manifest")
}

/// The manifest a publish record produces over its base: the base's parts less the retired ones, upserts by
/// `part_no`, ordered by `part_no`; the record's header and extension block.
pub fn try_manifest_of(base: Option<&Manifest>, rec: &PublishRec) -> WriteResult<Manifest> {
    Ok(Manifest::new(rec.header, try_merged_parts(base, &rec.upsert, &rec.retire)?).with_ext(rec.ext.clone()))
}

/// Every name `apply_publish` leaves written or renamed into place: what the directory's sync registration covers.
pub fn publish_touched(rec: &PublishRec) -> Vec<String> {
    let mut v: Vec<String> = rec.installs.iter().map(|i| part_file_name(i.part_no)).collect();
    v.extend(rec.renames.iter().map(|(_, to)| to.clone()));
    v.push(manifest_file_name(rec.header.gen));
    v.push(CURRENT_FILE_NAME.to_string());
    v
}

fn write_whole(vfs: &mut dyn WriteVfs, p: &str, bytes: &[u8]) -> WriteResult<()> {
    let fd = vfs.create_rw(p)?;
    vfs.pwrite_at(&fd, 0, bytes)?;
    vfs.close_file(fd)
}

/// Point `CURRENT` at generation `gen` (its manifest image is `mbytes`), through `CURRENT.tmp` and a rename.
pub fn write_current(vfs: &mut dyn WriteVfs, dir: &str, gen: u64, mbytes: &[u8]) -> WriteResult<()> {
    write_pointer(vfs, dir, CommitPointer::new(gen, mbytes.len() as u64, crc32c(&mbytes[..mbytes.len() - 4])))
}

/// Point `CURRENT` at `cp`'s generation, through `CURRENT.tmp` and a rename.
fn write_pointer(vfs: &mut dyn WriteVfs, dir: &str, cp: CommitPointer) -> WriteResult<()> {
    let tmp = path(dir, CURRENT_TMP_FILE_NAME);
    write_whole(vfs, &tmp, &cp.encode())?;
    vfs.rename_path(&tmp, &path(dir, CURRENT_FILE_NAME))
}

/// Whether the file at `p` is this install's part: its footer is the sealed image, patched or not. A tmp name
/// is never reused within a transaction, but a part number is reused after its generation dies, so redo patches
/// `part-<n>` only when it still holds this part.
fn holds_part(vfs: &mut dyn WriteVfs, p: &str, i: &PartInstall) -> WriteResult<bool> {
    let at = vfs.read_at(p, i.footer_off, FOOTER_FIXED_LEN)?;
    if at == i.footer {
        return Ok(true);
    }
    let mut unpatched = FooterFixed::decode(&i.footer)?;
    unpatched.part_no = 0;
    let mut img = Vec::with_capacity(FOOTER_FIXED_LEN);
    unpatched.encode_into(&mut img);
    Ok(at == img)
}

/// Whether the file at `p` already carries this install's patched header and footer, byte for byte.
fn installed(vfs: &mut dyn WriteVfs, p: &str, i: &PartInstall) -> WriteResult<bool> {
    Ok(vfs.read_at(p, i.footer_off, i.footer.len())? == i.footer && vfs.read_at(p, 0, i.header.len())? == i.header)
}

fn apply_part_install(vfs: &mut dyn WriteVfs, dir: &str, i: &PartInstall, mode: ApplyMode) -> WriteResult<()> {
    let tmp = path(dir, &i.tmp);
    let target = path(dir, &part_file_name(i.part_no));
    let renaming = mode == ApplyMode::Primary || vfs.exists_path(&tmp)?;
    if mode == ApplyMode::Redo {
        if renaming {
            if !holds_part(vfs, &tmp, i)? {
                return Err(WriteError::Format(FormatError::Corrupt { at: "PUBLISH redo: tmp part is not the sealed part" }));
            }
        } else if !vfs.exists_path(&target)? || !holds_part(vfs, &target, i)? {
            return crate::cleanup::check_not_swept(&tmp);
        } else if installed(vfs, &target, i)? {
            // the install already reached this file whole: rewriting the same bytes would only move its mtime, and
            // the part registry reads another mtime as another file (V5SYNC: the fd pool reopens by (dev, ino, len,
            // mtime)), so a hot-standby reader re-replaying after a restart would get a typed open mismatch
            return Ok(());
        }
    }
    let fd = vfs.open_rw(if renaming { &tmp } else { &target })?;
    vfs.pwrite_at(&fd, 0, &i.header)?;
    vfs.pwrite_at(&fd, i.footer_off, &i.footer)?;
    vfs.close_file(fd)?;
    if renaming {
        vfs.rename_path(&tmp, &target)?;
    }
    Ok(())
}

/// Redo reads each install's footer and rewrites its first page and footer. On a standby those pages of a large load
/// are cold by then, so hint them all first: the reads go in parallel instead of two round trips a part.
fn prefetch_installs(vfs: &mut dyn WriteVfs, dir: &str, rec: &PublishRec) {
    for i in &rec.installs {
        for p in [path(dir, &i.tmp), path(dir, &part_file_name(i.part_no))] {
            vfs.willneed(&p, 0, i.header.len() as u64);
            vfs.willneed(&p, i.footer_off, i.footer.len() as u64);
        }
    }
}

fn apply_rename(vfs: &mut dyn WriteVfs, dir: &str, from: &str, to: &str, mode: ApplyMode) -> WriteResult<()> {
    match vfs.rename_path(&path(dir, from), &path(dir, to)) {
        Err(e) if mode == ApplyMode::Redo && is_enoent(&e) => crate::cleanup::check_not_swept(&path(dir, from)),
        r => r,
    }
}

/// Install the parts (header and footer patches, then the rename), apply the renames, write the manifest and point
/// `CURRENT` at it. Idempotent: redo re-runs it over any prefix of its own effects. The rebuilt manifest must
/// match the record's crc.
pub fn apply_publish(
    vfs: &mut dyn WriteVfs,
    dir: &str,
    base: Option<&Manifest>,
    rec: &PublishRec,
    mode: ApplyMode,
) -> WriteResult<Manifest> {
    let manifest = try_manifest_of(base, rec)?;
    let bytes = manifest.encode();
    apply_image(vfs, dir, rec, &bytes, mode)?;
    Ok(manifest)
}

/// [`apply_publish`] by the publisher, which holds the image its plan encoded ([`PublishPlan::into_parts`]): the
/// manifest is not rebuilt from the base and the record (P-1 L4a: one encode per publish). Redo rebuilds it; debug
/// builds check here that the rebuild is this image.
pub fn apply_planned(vfs: &mut dyn WriteVfs, dir: &str, base: Option<&Manifest>, rec: &PublishRec, bytes: &[u8]) -> WriteResult<()> {
    debug_assert!(try_manifest_of(base, rec)?.encode() == bytes, "redo's rebuild of the manifest is the plan's image");
    apply_image(vfs, dir, rec, bytes, ApplyMode::Primary)
}

/// Install the parts, apply the renames, write the manifest image `bytes` (its crc the record's) and point `CURRENT`.
fn apply_image(vfs: &mut dyn WriteVfs, dir: &str, rec: &PublishRec, bytes: &[u8], mode: ApplyMode) -> WriteResult<()> {
    if trailing_crc(bytes) != rec.manifest_crc {
        return Err(WriteError::Format(FormatError::Corrupt { at: "PUBLISH manifest crc" }));
    }
    if mode == ApplyMode::Redo {
        prefetch_installs(vfs, dir, rec);
    }
    for i in &rec.installs {
        apply_part_install(vfs, dir, i, mode)?;
    }
    for (from, to) in &rec.renames {
        apply_rename(vfs, dir, from, to, mode)?;
    }
    write_whole(vfs, &path(dir, &manifest_file_name(rec.header.gen)), bytes)?;
    // the image's trailing crc was checked against the record's: `CURRENT` takes that crc (P-1 L4a: not a second pass
    // over the manifest)
    write_pointer(vfs, dir, CommitPointer::new(rec.header.gen, bytes.len() as u64, rec.manifest_crc))
}

/// Redo of `PUBLISH`: the base generation is read back from the directory. `None` when it is gone, which is legal
/// only for a publish that never committed (end-of-recovery checks the rest) or one an earlier pass over this WAL
/// applied before a later `RECLAIM` retired its base (replay restarted from an older redo point). Either way its
/// temps are dead: `FILE_WRITE` redo recreated them, and nothing renames them now, so they are unlinked here.
pub fn redo_publish(vfs: &mut dyn WriteVfs, dir: &str, rec: &PublishRec) -> WriteResult<Option<Manifest>> {
    let base = match rec.base_gen {
        0 => None,
        g => match read_manifest(vfs, dir, g) {
            Some(m) => Some(m),
            None => {
                unlink_temps(vfs, dir, rec)?;
                return Ok(None);
            }
        },
    };
    apply_publish(vfs, dir, base.as_ref(), rec, ApplyMode::Redo).map(Some)
}

/// The record's temp names (its installs' parts and its renames' sources that classify as temps, stats run staging
/// names included), each unlinked when present.
fn unlink_temps(vfs: &mut dyn WriteVfs, dir: &str, rec: &PublishRec) -> WriteResult<()> {
    use pgrc_format::dirlayout::{classify, FileClass};
    let temps = rec.installs.iter().map(|i| &i.tmp).chain(rec.renames.iter().map(|(from, _)| from));
    for n in temps.filter(|n| matches!(classify(n), Some(FileClass::Tmp { .. } | FileClass::BankStatsTmp(_) | FileClass::RunTmp(..)))) {
        match vfs.unlink_path(&path(dir, n)) {
            Err(e) if is_enoent(&e) => {}
            r => r?,
        }
    }
    Ok(())
}

pub fn apply_install(vfs: &mut dyn WriteVfs, dir: &str, rec: &InstallRec, mode: ApplyMode) -> WriteResult<()> {
    for (from, to) in &rec.renames {
        apply_rename(vfs, dir, from, to, mode)?;
    }
    Ok(())
}

/// Unlink the record's names; one already gone is fine, on the primary too (a writer may abort before creating it).
pub fn apply_reclaim(vfs: &mut dyn WriteVfs, dir: &str, rec: &ReclaimRec) -> WriteResult<()> {
    for n in &rec.names {
        match vfs.unlink_path(&path(dir, n)) {
            Err(e) if is_enoent(&e) => {}
            r => r?,
        }
    }
    Ok(())
}

/// Redo of `FILE_WRITE` / `FILE_CREATE`. A create truncates. A later chunk whose file is gone is skipped: the file
/// was finished durably (`file_done`) and then renamed or unlinked, and recreating it would leave a sparse file. A
/// temp a restartpoint swept is never skipped (WR L5f).
pub fn apply_file_write(vfs: &mut dyn WriteVfs, dir: &str, name: &str, off: u64, bytes: &[u8], create: bool) -> WriteResult<()> {
    let p = path(dir, name);
    let fd = if create {
        vfs.create_rw(&p)?
    } else if vfs.exists_path(&p)? {
        vfs.open_rw(&p)?
    } else {
        return crate::cleanup::check_not_swept(&p);
    };
    vfs.pwrite_at(&fd, off, bytes)?;
    vfs.close_file(fd)
}

/// Where redo of a `FILE_REF` takes the referenced bytes from (WR).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefSource {
    /// This server's spool entry of the reference (streamed, or linked by the primary).
    Spool(String),
    /// The pending spool link a primary made before the insert (crash recovery before the rename).
    Pending(String),
    /// The table file itself (a base backup's copy, or the primary's own file).
    InPlace,
    /// A file restored from the archive.
    Archive(String),
}

/// The masked-crc read size of [`verify_ref`].
const VERIFY_STEP: usize = 4 << 20;

/// Whether `path` holds exactly the referenced bytes: `rec.len` bytes whose masked crc is `rec.crc`. A missing file
/// does not.
pub fn verify_ref(vfs: &mut dyn WriteVfs, path: &str, rec: &FileRefRec<'_>) -> WriteResult<bool> {
    if !vfs.exists_path(path)? {
        return Ok(false);
    }
    let mut crc = MaskedCrc::new(rec.masks);
    let mut off = 0u64;
    loop {
        let b = vfs.read_at(path, off, VERIFY_STEP)?;
        crc.update(off, &b);
        off += b.len() as u64;
        if b.len() < VERIFY_STEP || off > rec.len {
            break;
        }
    }
    Ok(off == rec.len && crc.finish() == rec.crc)
}

/// Redo of `FILE_REF`: make `dir/<rec.name>` hold the referenced bytes. `InPlace` only verifies the file there;
/// another source is verified, then linked (or copied and fsynced where a link cannot be made) over the name,
/// unless the name already holds the same bytes. A source or file that does not verify is `Corrupt`: redo cannot go
/// on without the file.
///
/// A source with another name already lives in the directory under the name a later record gave it (redo replays
/// records whose effects are on disk: a primary's crash, a standby's restart). A second link under the record's name
/// would alias that later name: redo's rename of the one onto the other is a no-op (`rename(2)` of two links of one
/// inode), and a later `FILE_CREATE` of the same tmp name (a transaction's seal and compile stage a part's sidecars
/// under one) truncates the shared inode. So such a source is not linked: a missing name stays missing (the later
/// records find theirs) and a name holding other bytes gets a copy.
pub fn apply_file_ref(vfs: &mut dyn WriteVfs, dir: &str, rec: &FileRefRec<'_>, src: RefSource) -> WriteResult<()> {
    let target = path(dir, rec.name);
    let from = match src {
        RefSource::InPlace => {
            return match verify_ref(vfs, &target, rec)? {
                true => Ok(()),
                false => Err(WriteError::Format(FormatError::Corrupt { at: "FILE_REF redo: the referenced file is not available" })),
            };
        }
        RefSource::Spool(p) | RefSource::Pending(p) | RefSource::Archive(p) => p,
    };
    if !verify_ref(vfs, &from, rec)? {
        return Err(WriteError::Format(FormatError::Corrupt { at: "FILE_REF redo: the source does not hold the referenced file" }));
    }
    if verify_ref(vfs, &target, rec)? {
        return Ok(());
    }
    let linked_elsewhere = vfs.link_count(&from)? > 1;
    let exists = vfs.exists_path(&target)?;
    if linked_elsewhere && !exists {
        return Ok(());
    }
    if exists {
        vfs.unlink_path(&target)?;
    }
    if linked_elsewhere {
        return copy_file(vfs, &from, &target);
    }
    match vfs.link_path(&from, &target) {
        Err(e) if e.is_link_unsupported() => copy_file(vfs, &from, &target),
        r => r,
    }
}

/// Copy `from` over `to` and fsync it: the stand-in for a hard link across file systems.
fn copy_file(vfs: &mut dyn WriteVfs, from: &str, to: &str) -> WriteResult<()> {
    let fd = vfs.create_rw(to)?;
    let mut off = 0u64;
    loop {
        let b = vfs.read_at(from, off, VERIFY_STEP)?;
        if b.is_empty() {
            break;
        }
        vfs.pwrite_at(&fd, off, &b)?;
        off += b.len() as u64;
    }
    vfs.fsync_file(&fd)?;
    vfs.close_file(fd)
}

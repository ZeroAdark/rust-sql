//! End-of-recovery cleanup of one table directory (design §7.6, `rm_cleanup`): remove what uncommitted writers left.

use std::collections::{BTreeMap, BTreeSet};
use pgsync::Mutex;

use pgrc_format::dirlayout::{
    self, manifest_file_name, parse_manifest_file_name, parse_part_file_name, CURRENT_FILE_NAME, CURRENT_TMP_FILE_NAME,
};
use pgrc_format::manifest::{CommitPointer, Manifest};
use pgrc_format::sections::{manifest_crc, read_front_bytes};
use pgrc_format::sidecar::SidecarKind;
use pgrc_format::wire::crc32c;

use crate::plan::{read_manifest, write_current};
use crate::publish::{effective_manifest, TxnProbe, TxnVerdict};
use crate::walsink::WalSink;
use crate::wvfs::WriteVfs;
use crate::{WriteError, WriteResult};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CleanupReport {
    /// 0 = no effective generation (empty table).
    pub effective_gen: u64,
    pub removed: Vec<String>,
    /// `CURRENT` was rewritten (or unlinked, for an empty table) because it named a generation this pass removes.
    pub current_repointed: bool,
}

fn full_path(dir: &str, name: &str) -> String {
    format!("{dir}/{name}")
}

/// SORTRUNS-E0: the runs with a reverse file that `m`'s `EXT_RUNS` names (their `runrev-<run_id>` files live).
fn run_reverses(m: &Manifest) -> WriteResult<Vec<u64>> {
    Ok(pgrc_format::sortrun::runs_in(&m.ext)?.into_iter().filter(|r| r.homes()).map(|r| r.run_id).collect())
}

/// WR L5f: the temps a hot standby's restartpoints removed in this server's life (path -> writer fxid). Redo never
/// skips one of them as missing ([`check_not_swept`]); after a restart it is empty, and replay from the last
/// restartpoint may skip them as designed.
static SWEPT: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());

fn swept() -> pgsync::MutexGuard<'static, BTreeMap<String, u64>> {
    pgsync::lock(&SWEPT)
}

/// A restartpoint removed the temp at `path` (redo's form of it: `<table dir>/<name>`) of the writer `fxid`.
pub fn note_swept(path: String, fxid: u64) {
    swept().insert(path, fxid);
}

/// Forget every swept temp: at the start of redo, and at its end.
pub fn forget_swept() {
    swept().clear();
}

/// Redo is about to skip the missing file at `path`: refused when a restartpoint swept it, since its writer then
/// wrote after the sweep (the sweep's rule was wrong) and the skip would lose that write silently.
pub fn check_not_swept(path: &str) -> WriteResult<()> {
    match swept().get(path) {
        Some(&fxid) => Err(WriteError::SweptAlive { path: path.to_string(), fxid }),
        None => Ok(()),
    }
}

/// The writer fxid of a temp name `tmp-<fxid>-<seq>.pgrct`.
pub fn parse_temp_fxid(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("tmp-")?.strip_suffix(".pgrct")?;
    let (fxid, _seq) = rest.split_once('-')?;
    fxid.parse().ok()
}

/// Does the pointer pin exactly the on-disk manifest it names (length and the manifest's own trailing crc)?
fn pointer_matches_manifest(vfs: &mut dyn WriteVfs, dir: &str, cp: &CommitPointer) -> WriteResult<bool> {
    let path = full_path(dir, &manifest_file_name(cp.gen));
    if !vfs.exists_path(&path)? {
        return Ok(false);
    }
    let (b, s0) = match read_front_bytes(&mut |len| vfs.read_prefix(&path, len)) {
        Ok(front) => front,
        Err(WriteError::Format(_)) => return Ok(false),
        Err(e) => return Err(e),
    };
    Ok(s0 as u64 == cp.manifest_len && crc32c(&b[..s0 - 4]) == cp.manifest_crc)
}

/// Refuse a directory whose `CURRENT` chain holds a manifest that is intact but does not decode: its trailing crc
/// matches its bytes (and `CURRENT`'s length and crc, for the generation `CURRENT` names), so it is not a torn write,
/// yet this binary cannot read it; a newer binary wrote it (a format this one predates, e.g. SEAL-OFFHEAD's stacked
/// generations). Treated as torn, [`effective_manifest`] would fall back to the newest generation this binary can
/// decode and the removal pass would delete every generation above it with their parts, losing the rows they hold.
/// The walk follows `prev_gen` from `CURRENT` to the first committed generation, as the reader walk does; a missing or
/// torn manifest ends it (the existing rules own those).
fn refuse_unreadable_chain(vfs: &mut dyn WriteVfs, dir: &str, probe: &dyn TxnProbe) -> WriteResult<()> {
    let cur_path = full_path(dir, CURRENT_FILE_NAME);
    if !vfs.exists_path(&cur_path)? {
        return Ok(());
    }
    let Some(cp) = vfs.read_full(&cur_path).ok().and_then(|b| CommitPointer::decode(&b).ok()) else { return Ok(()) };
    let mut gen = cp.gen;
    while gen != 0 {
        let path = full_path(dir, &manifest_file_name(gen));
        if !vfs.exists_path(&path)? {
            return Ok(());
        }
        let named = gen == cp.gen;
        let (b, s0) = match read_front_bytes(&mut |len| vfs.read_prefix(&path, len)) {
            Ok(front) => front,
            // IX0 L0 (q019g): a layout this binary predates can make the length it computes wrong, so its prefix read
            // fails; the whole file still tells intact from torn
            Err(WriteError::Format(_)) if whole_file_intact(vfs, &path, named.then_some(&cp))? => return Err(newer_format()),
            Err(WriteError::Format(_)) => return Ok(()),
            Err(e) => return Err(e),
        };
        let bytes = &b[..s0];
        let intact = s0 >= 4 && crc32c(&bytes[..s0 - 4]) == manifest_crc(bytes);
        match Manifest::decode(bytes) {
            Ok(m) => {
                if probe.verdict(m.header.publisher_fxid) == TxnVerdict::Committed {
                    return Ok(());
                }
                gen = m.header.prev_gen;
            }
            Err(_) if intact && (!named || (s0 as u64 == cp.manifest_len && manifest_crc(bytes) == cp.manifest_crc)) => {
                return Err(newer_format());
            }
            // the length this binary computed disagrees with a file whose header and crc check out: a newer layout
            Err(_) if whole_file_intact(vfs, &path, named.then_some(&cp))? => return Err(newer_format()),
            Err(_) => return Ok(()),
        }
    }
    Ok(())
}

fn newer_format() -> WriteError {
    WriteError::ManifestChain {
        at: "a manifest on CURRENT's chain is intact but this binary cannot decode it (written by a newer pgrcolumnar \
             format); end-of-recovery cleanup refuses the directory and changes nothing",
    }
}

/// The whole manifest file at `path` is intact by its own header and trailer, whatever length this binary would compute
/// for it: the manifest magic, a trailing crc32c matching every byte before it, and (`cur`: the generation `CURRENT`
/// names) `CURRENT`'s length and crc. A torn write fails the crc; a manifest of a layout this binary predates (longer
/// records, IX0 L0's q019g) passes, and must be refused, not treated as torn and cleaned up.
fn whole_file_intact(vfs: &mut dyn WriteVfs, path: &str, cur: Option<&CommitPointer>) -> WriteResult<bool> {
    let b = match vfs.read_full(path) {
        Ok(b) => b,
        Err(WriteError::Io { .. }) => return Ok(false),
        Err(e) => return Err(e),
    };
    let n = b.len();
    if n < pgrc_format::manifest::MANIFEST_HEADER_LEN + 4 || b[40..44] != pgrc_format::manifest::MANIFEST_MAGIC.to_le_bytes() {
        return Ok(false);
    }
    let crc = manifest_crc(&b);
    Ok(crc32c(&b[..n - 4]) == crc && cur.is_none_or(|cp| cp.manifest_len == n as u64 && cp.manifest_crc == crc))
}

/// The #462 repoint law: the reader walk is scan-free, so `CURRENT` must never name a generation the removal pass
/// reclaims. It is normalized to the effective generation first; a `CURRENT` naming an intact in-progress
/// generation (a prepared transaction's) is the legal pre-commit state and stays.
fn normalize_current(
    vfs: &mut dyn WriteVfs,
    sink: &mut dyn WalSink,
    dir: &str,
    eff: Option<&Manifest>,
    eff_gen: u64,
    probe: &dyn TxnProbe,
) -> WriteResult<bool> {
    let cur_path = full_path(dir, CURRENT_FILE_NAME);
    let exists = vfs.exists_path(&cur_path)?;
    let decoded = if exists { vfs.read_full(&cur_path).ok().and_then(|b| CommitPointer::decode(&b).ok()) } else { None };
    let safe = match &decoded {
        Some(cp) => {
            let survives = if cp.gen != 0 && cp.gen == eff_gen {
                true
            } else if cp.gen > eff_gen {
                matches!(
                    read_manifest(vfs, dir, cp.gen).map(|m| probe.verdict(m.header.publisher_fxid)),
                    Some(TxnVerdict::InProgress)
                )
            } else {
                false
            };
            survives && pointer_matches_manifest(vfs, dir, cp)?
        }
        None => !exists && eff.is_none(),
    };
    if safe {
        return Ok(false);
    }
    match eff {
        Some(_) => {
            let mpath = full_path(dir, &manifest_file_name(eff_gen));
            let (mbytes, s0) = read_front_bytes(&mut |len| vfs.read_prefix(&mpath, len))?;
            write_current(vfs, dir, eff_gen, &mbytes[..s0])?;
        }
        None if exists => vfs.unlink_path(&cur_path)?,
        None => return Ok(false),
    }
    sink.register(CURRENT_FILE_NAME)?;
    Ok(true)
}

/// Remove dead writer temps, parts (and their sidecars) that neither the effective generation nor an in-progress
/// (prepared) one names, DVs of dead compiles, generations whose publisher did not commit, their sections and stats planes, and
/// `CURRENT.tmp`. Every change is registered through `sink` for the
/// end-of-recovery checkpoint; nothing is logged. Precondition: no concurrent publisher on this directory.
pub fn cleanup_dir(vfs: &mut dyn WriteVfs, sink: &mut dyn WalSink, dir: &str, probe: &dyn TxnProbe) -> WriteResult<CleanupReport> {
    cleanup_dir_keeping(vfs, sink, dir, probe, &BTreeSet::new())
}

/// [`cleanup_dir`] that also keeps the parts in `held` (and their sidecars). RECLAIMRACE C: at the promotion of a hot
/// standby, the parts of the committed generations its catalog still holds, which its read-only sessions may resolve
/// (a deferred retirement waits for them, K1 L5); they go with that retirement's drain or the new primary's reclaim.
pub fn cleanup_dir_keeping(
    vfs: &mut dyn WriteVfs,
    sink: &mut dyn WalSink,
    dir: &str,
    probe: &dyn TxnProbe,
    held: &BTreeSet<u32>,
) -> WriteResult<CleanupReport> {
    refuse_unreadable_chain(vfs, dir, probe)?;
    let eff = effective_manifest(vfs, dir, probe, None)?;
    let eff_gen = eff.as_ref().map_or(0, |m| m.header.gen);

    let current_repointed = normalize_current(vfs, sink, dir, eff.as_ref(), eff_gen, probe)?;
    let mut report = CleanupReport { effective_gen: eff_gen, current_repointed, ..CleanupReport::default() };
    let names = vfs.list_dir(dir)?;
    let manifest_names: BTreeSet<&String> = names.iter().filter(|n| parse_manifest_file_name(n).is_some()).collect();
    let in_progress = |vfs: &mut dyn WriteVfs, g: u64| {
        matches!(read_manifest(vfs, dir, g).map(|m| probe.verdict(m.header.publisher_fxid)), Some(TxnVerdict::InProgress))
    };
    let mut live_parts: BTreeSet<u32> = eff.as_ref().map(|m| m.parts.iter().map(|p| p.part_no).collect()).unwrap_or_default();
    live_parts.extend(held.iter().copied());
    // SORTRUNS-E0: a run's reverse lives while the effective or an in-progress generation's EXT_RUNS names its run
    let mut live_runs: BTreeSet<u64> = BTreeSet::new();
    if let Some(m) = &eff {
        live_runs.extend(run_reverses(m)?);
    }
    for g in names.iter().filter_map(|n| parse_manifest_file_name(n)).filter(|g| *g > eff_gen) {
        if let Some(m) = read_manifest(vfs, dir, g) {
            match probe.verdict(m.header.publisher_fxid) {
                TxnVerdict::InProgress => {
                    live_parts.extend(m.parts.iter().map(|p| p.part_no));
                    live_runs.extend(run_reverses(&m)?);
                }
                // D2876 (trunk 9afaaac28b5, ported): a committed generation above the effective one is refused before
                // anything is unlinked (its parts are in no live set: the removal loop below would take them first)
                TxnVerdict::Committed => return Err(WriteError::ManifestChain { at: "committed generation above effective" }),
                TxnVerdict::Aborted => {}
            }
        }
    }
    for name in names.iter() {
        let remove = if dirlayout::is_temp_file_name(name) {
            match parse_temp_fxid(name) {
                Some(fxid) => probe.verdict(fxid) != TxnVerdict::InProgress,
                None => true,
            }
        } else if let Some(pn) = parse_part_file_name(name) {
            !live_parts.contains(&pn)
        } else if let Some((pn, kind, g)) = dirlayout::parse_sidecar_file_name(name) {
            // a DV is named at its compile's generation: one above the effective one from a dead compile goes
            !live_parts.contains(&pn) || (kind == SidecarKind::Dv && g > eff_gen && !in_progress(vfs, g))
        } else if let Some(g) = parse_manifest_file_name(name) {
            if g <= eff_gen {
                false
            } else {
                match read_manifest(vfs, dir, g) {
                    Some(m) => match probe.verdict(m.header.publisher_fxid) {
                        TxnVerdict::InProgress => false,
                        TxnVerdict::Aborted => true,
                        TxnVerdict::Committed => {
                            return Err(WriteError::ManifestChain { at: "committed generation above effective" })
                        }
                    },
                    None => true,
                }
            }
        } else if let Some(g) = pgrc_format::bankstats::parse_bankstats_file_name(name) {
            g != eff_gen && (g < eff_gen || !in_progress(vfs, g))
        } else if let Some((g, _)) = dirlayout::parse_run_file_name(name) {
            // a stats run is named at its writer's generation: one above the effective one from a dead publish goes
            g > eff_gen && !in_progress(vfs, g)
        } else if let Some(run) = dirlayout::parse_runrev_file_name(name) {
            !live_runs.contains(&run)
        } else if let Some(g) = dirlayout::parse_sections_file_name(name) {
            !manifest_names.contains(&manifest_file_name(g)) || (g > eff_gen && !in_progress(vfs, g))
        } else {
            name.ends_with(".pgrcf") || name.ends_with(".pgrcx.tmp") || name == CURRENT_TMP_FILE_NAME || name.ends_with(".pgrcbs.tmp") || name.ends_with(".pgrcr.tmp")
        };
        if remove {
            vfs.unlink_path(&full_path(dir, name))?;
            sink.register(name)?;
            report.removed.push(name.clone());
        }
    }
    Ok(report)
}

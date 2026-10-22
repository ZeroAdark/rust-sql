//! The publish/commit round protocol of the `qa_crash_harness` binary (a real directory through `RealVfs`,
//! killed with SIGKILL by the ladder driver). One round = ingest rows, seal, publish through the WAL sink, then the
//! flushed commit record (the round's ack). Every few rounds the harness checkpoints.
//!
//! The harness recovers on start: replay the WAL from its checkpoint, `cleanup_dir`, the end-of-recovery
//! checkpoint, then resume at the next round, so every respawn exercises crash recovery before it writes a byte.
//! The checker recovers the same way, then proves old-or-new: effective rows == committed rounds × rows/round,
//! every live part decodes to the oracle, both manifest walks agree, and the directory holds exactly the live set.

use pgrc_format::dirlayout::{manifest_file_name, part_file_name, sections_file_name, CURRENT_FILE_NAME};
use pgrc_format::manifest::Manifest;
use pgrc_format::sections::{self, SectionsFile};
use pgrc_format::class::ColSchema;
use pgrc_read::manifest_walk::{resolve_effective, TableExpect};
use pgrc_write::publish::{effective_manifest, TxnVerdict};
use pgrc_write::writer::PartCutPolicy;
use pgrc_write::wvfs::WriteVfs;
use pgrc_write::{WriteError, WriteResult};
use std::collections::BTreeSet;

use crate::adapters::{memdir_of, ForcedPlan};
use crate::walmodel::{probe_of, FileWal};
use crate::corpus::{
    append_rows_planned, finish_planned, open_writer, verify_manifest, Fixture, OracleVal,
    RowOracle,
};
use crate::{int8_col, text_col};

pub const FXID_BASE: u64 = 1_000;
pub const RELFILENUMBER: u64 = 4242;
pub const CHECKPOINT_EVERY: u64 = 3;

fn mix(i: u64) -> u64 {
    i.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(31) ^ i
}

/// The ladder's deterministic row truth: value(row) is a pure function of
/// the GLOBAL row index, so any committed prefix is checkable.
pub struct LadderOracle {
    schema: Vec<ColSchema>,
}

impl LadderOracle {
    pub fn new() -> LadderOracle {
        LadderOracle {
            schema: vec![int8_col(1), text_col(2)],
        }
    }
}

impl Default for LadderOracle {
    fn default() -> LadderOracle {
        LadderOracle::new()
    }
}

impl RowOracle for LadderOracle {
    fn schema(&self) -> &[ColSchema] {
        &self.schema
    }

    fn value(&self, col: usize, row: u64) -> Option<OracleVal> {
        match col {
            0 => {
                if row % 7 == 3 {
                    None
                } else {
                    Some(OracleVal::Word(mix(row)))
                }
            }
            _ => {
                if row % 5 == 2 {
                    None
                } else if row % 13 == 0 {
                    Some(OracleVal::Bytes(Vec::new()))
                } else {
                    let len = 3 + (mix(row) % 60) as usize;
                    Some(OracleVal::Bytes(
                        (0..len).map(|k| (mix(row ^ (k as u64 + 11)) & 0xFF) as u8).collect(),
                    ))
                }
            }
        }
    }
}

fn ladder_fixture(base: &str) -> Fixture {
    let o = LadderOracle::new();
    Fixture {
        name: "ladder",
        dir: format!("{base}/table"),
        spc: 1663,
        db: 5,
        relfilenumber: RELFILENUMBER,
        schema: o.schema.clone(),
        oracle: Vec::new(), // rows come from LadderOracle, never from here
        plans: vec![ForcedPlan::ByteFor {
            delta_width: 8,
            signed: true,
        }],
        policy: PartCutPolicy::default(),
    }
}

/// The fxids whose commit record the WAL in `base` holds.
pub fn read_commits(base: &str) -> WriteResult<BTreeSet<u64>> {
    Ok(FileWal::open(base)?.committed())
}

pub fn round_fxid(round: u64, salt: u64) -> u64 {
    FXID_BASE + round * 8 + salt
}

// ---------------------------------------------------------------------------
// rounds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSummary {
    pub resumed_at_round: u64,
    pub completed_rounds: u64,
    pub effective_gen: u64,
}

/// Recover, resume, and run publish/commit rounds until `target_rounds` rounds are committed. When `commit_last`
/// is false the final round publishes but never writes its commit record (the #254 shape), and the harness exits.
pub fn run_rounds(
    vfs: &mut dyn WriteVfs,
    base: &str,
    target_rounds: u64,
    rows_per_round: u64,
    salt: u64,
    commit_last: bool,
) -> WriteResult<RunSummary> {
    let fx = ladder_fixture(base);
    let oracle = LadderOracle::new();
    let wal = FileWal::open(base)?;
    wal.recover(vfs, &fx.dir)?;
    let mut probe = probe_of(&wal.committed(), &[]);
    let eff = effective_manifest(vfs, &fx.dir, &probe, None)?;
    let total: u64 = eff
        .as_ref()
        .map(|m| m.parts.iter().map(|p| p.rows).sum())
        .unwrap_or(0);
    if total % rows_per_round != 0 {
        return Err(WriteError::Contract {
            detail: "recovered row total is not a whole number of rounds",
        });
    }
    let start = total / rows_per_round;
    let mut gen = eff.map(|m| m.header.gen).unwrap_or(0);
    for round in start..target_rounds {
        let fxid = round_fxid(round, salt);
        probe.mark(fxid, TxnVerdict::InProgress);
        let mut w = open_writer(&fx, fxid)?.with_sink(wal.sink(&fx.dir));
        let lo = round * rows_per_round;
        append_rows_planned(vfs, &mut w, &oracle, lo, lo + rows_per_round, &fx.plans)?;
        finish_planned(vfs, &mut w, &fx.plans)?;
        let outcome = w.publish(vfs, &probe)?;
        gen = outcome.gen;
        let last = round + 1 == target_rounds;
        if last && !commit_last {
            println!("UNCOMMITTED round={round} gen={gen} fxid={fxid}");
            return Ok(RunSummary {
                resumed_at_round: start,
                completed_rounds: round,
                effective_gen: gen,
            });
        }
        wal.commit(fxid)?;
        probe.mark(fxid, TxnVerdict::Committed);
        println!(
            "ACK round={round} gen={gen} total={}",
            (round + 1) * rows_per_round
        );
        if (round + 1) % CHECKPOINT_EVERY == 0 {
            wal.checkpoint(vfs)?;
        }
    }
    Ok(RunSummary {
        resumed_at_round: start,
        completed_rounds: target_rounds,
        effective_gen: gen,
    })
}

// ---------------------------------------------------------------------------
// the checker (old-or-new + residue + oracle, both manifest walks)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    pub effective_gen: u64,
    pub total_rows: u64,
    pub committed_rounds: u64,
    pub removed_residue: usize,
}

/// Recover a harness directory the way a respawn would, then adjudicate it. `Err(String)` is a battery failure
/// naming the law that broke.
pub fn check_dir(
    vfs: &mut dyn WriteVfs,
    base: &str,
    rows_per_round: u64,
) -> Result<CheckReport, String> {
    let fx = ladder_fixture(base);
    let oracle = LadderOracle::new();
    let wal = FileWal::open(base).map_err(|e| format!("wal open: {e}"))?;
    let commits = wal.committed();
    if !vfs.exists_path(&fx.dir).unwrap_or(false) {
        if !commits.is_empty() {
            return Err("the WAL holds commit records but the table dir is missing".to_string());
        }
        return Ok(CheckReport {
            effective_gen: 0,
            total_rows: 0,
            committed_rounds: 0,
            removed_residue: 0,
        });
    }
    let report = wal.recover(vfs, &fx.dir).map_err(|e| format!("recover: {e}"))?;
    let probe = probe_of(&commits, &[]);

    let eff = effective_manifest(vfs, &fx.dir, &probe, None).map_err(|e| format!("effective: {e}"))?;
    let eff_gen = eff.as_ref().map(|m| m.header.gen).unwrap_or(0);
    let mut files = std::collections::BTreeMap::new();
    for n in vfs.list_dir(&fx.dir).map_err(|e| format!("listdir: {e}"))? {
        let b = vfs
            .read_full(&format!("{}/{n}", fx.dir))
            .map_err(|e| format!("read {n}: {e}"))?;
        files.insert(n, b);
    }
    match resolve_effective(&memdir_of(&files), &probe, &TableExpect::default()) {
        Ok(r) => {
            let rgen = r.as_ref().map(|m| m.manifest.header.gen).unwrap_or(0);
            if rgen != eff_gen {
                return Err(format!("walk disagreement: writer gen {eff_gen} vs reader gen {rgen}"));
            }
        }
        Err(e) => return Err(format!("the reader walk refused a recovered table (#462 law): {e:?}")),
    }

    let total: u64 = eff.as_ref().map(|m| m.parts.iter().map(|p| p.rows).sum()).unwrap_or(0);
    let committed_rounds = commits.len() as u64;
    if total != committed_rounds * rows_per_round {
        return Err(format!(
            "old-or-new broken: effective rows {total} != committed rounds \
             {committed_rounds} × {rows_per_round}"
        ));
    }
    if let Some(m) = &eff {
        let verified =
            verify_manifest(&files, m, &oracle).map_err(|e| format!("decode-vs-oracle: {e:?}"))?;
        if verified != total {
            return Err(format!("verified {verified} rows, manifest claims {total}"));
        }
    }

    let after: BTreeSet<String> = vfs
        .list_dir(&fx.dir)
        .map_err(|e| format!("listdir after clean: {e}"))?
        .into_iter()
        .collect();
    let mut allowed: BTreeSet<String> = BTreeSet::new();
    let mut required: BTreeSet<String> = BTreeSet::new();
    if let Some(m) = &eff {
        for p in &m.parts {
            required.insert(part_file_name(p.part_no));
        }
        for g in 1..=m.header.gen {
            required.insert(manifest_file_name(g));
        }
    }
    allowed.extend(required.iter().cloned());
    allowed.insert(CURRENT_FILE_NAME.to_string());
    if let Some(m) = &eff {
        allowed.extend((1..=m.header.gen).map(pgrc_format::dirlayout::sections_file_name));
        allowed.extend(m.parts.iter().map(|p| {
            pgrc_format::dirlayout::sidecar_file_name(p.part_no, pgrc_format::sidecar::SidecarKind::Stats, 1)
        }));
    }
    // The effective generation's stats plane is allowed, never required; when present it must prove its binding
    // to the effective part-set.
    if let Some(m) = &eff {
        let plane = pgrc_format::bankstats::bankstats_file_name(m.header.gen);
        if after.contains(&plane) {
            let bytes = vfs
                .read_full(&format!("{}/{plane}", fx.dir))
                .map_err(|e| format!("read live plane {plane}: {e}"))?;
            let (h, pidents, _cols) = pgrc_format::bankstats::decode_meta(&bytes)
                .map_err(|e| format!("live plane {plane} undecodable: {e:?}"))?;
            let bound = h.gen == m.header.gen
                && h.schema_fingerprint == m.header.schema_fingerprint
                && h.part_count as usize == m.parts.len()
                && pidents.iter().zip(m.parts.iter()).all(|(a, b)| {
                    a.part_no == b.part_no
                        && a.granule_count == b.granule_count
                        && a.band_count == b.band_count
                        && a.rows == b.rows
                        && a.footer_off == b.footer_off
                });
            if !bound {
                return Err(format!("live plane {plane} fails the identity witness"));
            }
        }
        allowed.insert(plane);
        // D356: every kept generation's sections file (`recover_and_clean` keeps it exactly as long as its manifest)
        for g in 1..=m.header.gen {
            if let Some(s) = check_sections_sidecar(vfs, &fx.dir, g)? {
                allowed.insert(s);
            }
        }
    }
    // Stats runs (P-1): the effective root and the runs it lists are required, and the recovered stats cover the
    // effective part set exactly; older runs (not yet reclaimed) are allowed at or below the effective generation.
    if let Some(m) = &eff {
        if let Some(root) = pgrc_format::manifest::stats_ref(m).map_err(|e| format!("effective stats ref: {e:?}"))? {
            let state = pgrc_write::stats::StatsState::load(vfs, &fx.dir, m, None, &mut |v, rec| pgrc_write::publish::part_stats_on_disk(v, &fx.dir, m, rec))
                .map_err(|e| format!("effective stats runs refused: {e}"))?;
            required.insert(pgrc_format::dirlayout::run_file_name(root.gen, root.seq));
            required.extend(state.runs.iter().map(|r| pgrc_format::dirlayout::run_file_name(r.rref.gen, r.rref.seq)));
        }
        allowed.extend(required.iter().cloned());
        allowed.extend(after.iter().filter(|n| pgrc_format::dirlayout::parse_run_file_name(n).is_some_and(|(g, _)| g <= m.header.gen)).cloned());
    }
    for n in &after {
        if !allowed.contains(n) {
            return Err(format!("residue survived cleanup: {n}"));
        }
    }
    for n in &required {
        if !after.contains(n) {
            return Err(format!("live file missing after cleanup: {n}"));
        }
    }
    Ok(CheckReport {
        effective_gen: eff_gen,
        total_rows: total,
        committed_rounds,
        removed_residue: report.removed.len(),
    })
}

/// Generation `gen`'s sections file (`sections-<gen>.pgrcx`, pgrc_format::sections: the planner facts and the per-column
/// part-grain answers) is a LIVE derived sidecar: the publisher writes it after the manifest and before CURRENT, and
/// `recover_and_clean` keeps it exactly as long as `manifest-<gen>` (D356: the checkers' allowed sets predated it, so every
/// recovered universe with one failed as "residue survived cleanup"). Allowed, never required: a publisher that cannot fold
/// the facts leaves it unwritten and the catalog completes the generation. When present it must positively name its manifest
/// (gen, publisher fxid, the manifest file's own crc), never ride through a checker as unadjudicated bytes. Its name when
/// present and bound; None when absent.
pub fn check_sections_sidecar(vfs: &mut dyn WriteVfs, dir: &str, gen: u64) -> Result<Option<String>, String> {
    let name = sections_file_name(gen);
    let path = format!("{dir}/{name}");
    if !vfs.exists_path(&path).map_err(|e| format!("stat {name}: {e}"))? {
        return Ok(None);
    }
    let bytes = vfs.read_full(&path).map_err(|e| format!("read live {name}: {e}"))?;
    let file = SectionsFile::parse(bytes).map_err(|e| format!("live {name} undecodable: {e:?}"))?;
    let mname = manifest_file_name(gen);
    let mbytes = vfs.read_full(&format!("{dir}/{mname}")).map_err(|e| format!("{name} without its manifest {mname}: {e}"))?;
    let m = Manifest::decode(&mbytes).map_err(|e| format!("{mname} undecodable: {e:?}"))?;
    if !file.names(&m, sections::manifest_crc(&mbytes)) {
        return Err(format!("live {name} names another manifest than {mname}"));
    }
    Ok(Some(name))
}

/// Real-filesystem directory bootstrap (RealVfs mkdir refuses EEXIST;
/// tolerate it — reruns share the base).
pub fn ensure_dirs(vfs: &mut dyn WriteVfs, base: &str) -> WriteResult<()> {
    for d in [base.to_string(), format!("{base}/table")] {
        match vfs.mkdir_path(&d) {
            Ok(()) => {}
            Err(WriteError::Io { errno, .. }) if errno == libc::EEXIST => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

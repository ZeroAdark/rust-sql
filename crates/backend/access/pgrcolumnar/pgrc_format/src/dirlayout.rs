//! Directory layout + file naming (spec §12; ruling O-7): parts are real files in a per-table directory, a SIBLING of
//! where the relation's main fork would live (tablespace semantics preserved). Lifecycle wiring (pendingDeletes at
//! CREATE/DROP/abort) is M3-H's; the names and parsers are frozen here. Pure string functions — no I/O in this crate.

/// The per-table directory name beside the main-fork path.
pub fn table_dir_name(relfilenumber: u64) -> String {
    format!("pgrc_{relfilenumber}")
}

/// A temporary table's directory, `pgrc_t<procno>_<n>`: Postgres's `t<procno>_<n>` naming, so startup removes it,
/// backups and the colcache load skip it, and it never collides with a permanent table's `pgrc_<n>`.
pub fn temp_table_dir_name(procno: u32, relfilenumber: u64) -> String {
    format!("pgrc_t{procno}_{relfilenumber}")
}

/// Parse a table directory name: `(None, n)` for `pgrc_<n>`, `(Some(procno), n)` for `pgrc_t<procno>_<n>`.
pub fn parse_table_dir_name(name: &str) -> Option<(Option<u32>, u64)> {
    let rest = name.strip_prefix("pgrc_")?;
    match rest.strip_prefix('t') {
        Some(temp) => {
            let (procno, n) = temp.split_once('_')?;
            let procno = parse_decimal_u64(procno).and_then(|v| u32::try_from(v).ok())?;
            Some((Some(procno), parse_decimal_u64(n)?))
        }
        None => Some((None, parse_decimal_u64(rest)?)),
    }
}

/// `part-<part_no>.pgrc`
pub fn part_file_name(part_no: u32) -> String {
    format!("part-{part_no}.pgrc")
}

/// `manifest-<gen>.pgrcm`
pub fn manifest_file_name(gen: u64) -> String {
    format!("manifest-{gen}.pgrcm")
}

/// `sections-<gen>.pgrcx` — the generation's sections file (`crate::sections`: the planner facts and the per-column
/// part-grain answers), written by the publisher after `manifest-<gen>` and before `CURRENT`; it lives exactly as long
/// as its manifest.
pub fn sections_file_name(gen: u64) -> String {
    format!("sections-{gen}.pgrcx")
}

/// Parse `sections-<gen>.pgrcx` → gen.
pub fn parse_sections_file_name(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("sections-")?.strip_suffix(".pgrcx")?;
    if rest.is_empty() || (rest.len() > 1 && rest.starts_with('0')) || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

/// `srun-<gen>-<seq>.pgrcr` — a stats run (`crate::statsrun`), the `seq`-th run the publish of generation `gen`
/// wrote. Immutable; it lives while a generation's root lists it or carries it.
pub fn run_file_name(gen: u64, seq: u32) -> String {
    format!("srun-{gen}-{seq}.pgrcr")
}

/// `srun-<gen>-<seq>.pgrcr.tmp` — a run's staging name: logged by `FILE_WRITE`, renamed into place by its publish.
pub fn run_tmp_file_name(gen: u64, seq: u32) -> String {
    format!("srun-{gen}-{seq}.pgrcr.tmp")
}

/// Parse `srun-<gen>-<seq>.pgrcr` → (gen, seq).
pub fn parse_run_file_name(name: &str) -> Option<(u64, u32)> {
    let (gen, seq) = name.strip_prefix("srun-")?.strip_suffix(".pgrcr")?.split_once('-')?;
    Some((parse_decimal_u64(gen)?, parse_decimal_u64(seq).and_then(|v| u32::try_from(v).ok())?))
}

/// `runrev-<run_id>.pgrcv` — SORTRUNS-E0: a sorted run's reverse (`crate::runrev`), named by the run's ID (the
/// generation that published the run, never reused). Written as a writer temp and renamed into place by the run's
/// publish; immutable for the run's life; reclaimed only once no held generation's `EXT_RUNS` names the run.
pub fn runrev_file_name(run_id: u64) -> String {
    format!("runrev-{run_id}.pgrcv")
}

/// Parse `runrev-<run_id>.pgrcv` → run_id.
pub fn parse_runrev_file_name(name: &str) -> Option<u64> {
    parse_decimal_u64(name.strip_prefix("runrev-")?.strip_suffix(".pgrcv")?)
}

/// The commit pointer (spec §13.2).
pub const CURRENT_FILE_NAME: &str = "CURRENT";
/// IX0: the table's row-ID counter (`crate::rowid::encode_rowid_file`), written in place, never renamed or reclaimed.
pub const ROWID_FILE_NAME: &str = "rowid";
/// IX0 DV-1: the live segments' extent table (`crate::rowid::encode_segext`), rewritten in place whole, never renamed
/// or reclaimed.
pub const SEGEXT_FILE_NAME: &str = "segext";
/// The commit pointer's rename source.
pub const CURRENT_TMP_FILE_NAME: &str = "CURRENT.tmp";

/// `part-<part_no>-<tag>-<gen>.pgrcs`
pub fn sidecar_file_name(part_no: u32, kind: crate::sidecar::SidecarKind, gen: u64) -> String {
    format!("part-{part_no}-{}-{gen}.pgrcs", kind.tag())
}

/// `tmp-<fxid>-<seq>.pgrct` — writer scratch; abort/crash cleanup scans this prefix (spec §13.3 dead-band discipline).
pub fn temp_file_name(fxid: u64, seq: u32) -> String {
    format!("tmp-{fxid}-{seq}.pgrct")
}

/// Parse `part-<n>.pgrc` → part_no.
pub fn parse_part_file_name(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("part-")?.strip_suffix(".pgrc")?;
    parse_decimal_u64(rest).and_then(|v| u32::try_from(v).ok())
}

/// Parse `manifest-<gen>.pgrcm` → gen.
pub fn parse_manifest_file_name(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("manifest-")?.strip_suffix(".pgrcm")?;
    parse_decimal_u64(rest)
}

/// Parse `part-<n>-<tag>-<gen>.pgrcs` → (part_no, kind, gen).
pub fn parse_sidecar_file_name(name: &str) -> Option<(u32, crate::sidecar::SidecarKind, u64)> {
    let rest = name.strip_prefix("part-")?.strip_suffix(".pgrcs")?;
    let mut it = rest.split('-');
    let part_no = parse_decimal_u64(it.next()?).and_then(|v| u32::try_from(v).ok())?;
    let kind = match it.next()? {
        "dv" => crate::sidecar::SidecarKind::Dv,
        "pcache" => crate::sidecar::SidecarKind::PredicateCache,
        "memo" => crate::sidecar::SidecarKind::Memo,
        "trgm" => crate::sidecar::SidecarKind::Trgm,
        "stats" => crate::sidecar::SidecarKind::Stats,
        "fp128" => crate::sidecar::SidecarKind::Fp128,
        "rowmap" => crate::sidecar::SidecarKind::RowIdentMap,
        _ => return None,
    };
    let gen = parse_decimal_u64(it.next()?)?;
    if it.next().is_some() {
        return None;
    }
    Some((part_no, kind, gen))
}

/// Is this a writer temp file (cleanup-scan predicate)?
pub fn is_temp_file_name(name: &str) -> bool {
    name.starts_with("tmp-") && name.ends_with(".pgrct")
}

/// Every file name a table directory may hold. WAL records carry names, and redo writes only names that classify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileClass {
    Tmp { fxid: u64, seq: u32 },
    Part(u32),
    Manifest(u64),
    Current,
    CurrentTmp,
    Sections(u64),
    Sidecar(u32, crate::sidecar::SidecarKind, u64),
    BankStats(u64),
    BankStatsTmp(u64),
    /// A stats run: (writer gen, seq).
    Run(u64, u32),
    /// A stats run's staging name.
    RunTmp(u64, u32),
    /// IX0: the row-ID counter.
    Rowid,
    /// IX0 DV-1: the segment extent table.
    SegExt,
    /// SORTRUNS-E0: a sorted run's reverse, by run ID.
    RunRev(u64),
}

/// Classify a table-directory file name; `None` for anything this crate never writes.
pub fn classify(name: &str) -> Option<FileClass> {
    if name == CURRENT_FILE_NAME {
        return Some(FileClass::Current);
    }
    if name == CURRENT_TMP_FILE_NAME {
        return Some(FileClass::CurrentTmp);
    }
    if name == ROWID_FILE_NAME {
        return Some(FileClass::Rowid);
    }
    if name == SEGEXT_FILE_NAME {
        return Some(FileClass::SegExt);
    }
    if let Some(rest) = name.strip_prefix("tmp-").and_then(|r| r.strip_suffix(".pgrct")) {
        let (fxid, seq) = rest.split_once('-')?;
        return Some(FileClass::Tmp { fxid: parse_decimal_u64(fxid)?, seq: parse_decimal_u64(seq).and_then(|v| u32::try_from(v).ok())? });
    }
    if let Some(n) = parse_part_file_name(name) {
        return Some(FileClass::Part(n));
    }
    if let Some(g) = parse_manifest_file_name(name) {
        return Some(FileClass::Manifest(g));
    }
    if let Some(g) = parse_sections_file_name(name) {
        return Some(FileClass::Sections(g));
    }
    if let Some((p, k, g)) = parse_sidecar_file_name(name) {
        return Some(FileClass::Sidecar(p, k, g));
    }
    if let Some((g, seq)) = parse_run_file_name(name) {
        return Some(FileClass::Run(g, seq));
    }
    if let Some(r) = parse_runrev_file_name(name) {
        return Some(FileClass::RunRev(r));
    }
    if let Some((g, seq)) = name.strip_suffix(".tmp").and_then(parse_run_file_name) {
        return Some(FileClass::RunTmp(g, seq));
    }
    if let Some(stem) = name.strip_suffix(".tmp") {
        let g = crate::bankstats::parse_bankstats_file_name(stem)?;
        return (crate::bankstats::bankstats_file_name(g) == stem).then_some(FileClass::BankStatsTmp(g));
    }
    let g = crate::bankstats::parse_bankstats_file_name(name)?;
    (crate::bankstats::bankstats_file_name(g) == name).then_some(FileClass::BankStats(g))
}

/// Strict decimal parse: no signs, no leading zeros (except "0" itself) —
/// names are canonical, so parse(format(x)) == x and nothing else parses.
fn parse_decimal_u64(s: &str) -> Option<u64> {
    if s.is_empty() || (s.len() > 1 && s.starts_with('0')) {
        return None;
    }
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_file_class_round_trips_and_nothing_else_classifies() {
        use crate::sidecar::SidecarKind;
        let cases = [
            (temp_file_name(744, 3), FileClass::Tmp { fxid: 744, seq: 3 }),
            (part_file_name(17), FileClass::Part(17)),
            (manifest_file_name(9), FileClass::Manifest(9)),
            (CURRENT_FILE_NAME.to_string(), FileClass::Current),
            (CURRENT_TMP_FILE_NAME.to_string(), FileClass::CurrentTmp),
            (sections_file_name(9), FileClass::Sections(9)),
            (sidecar_file_name(17, SidecarKind::Dv, 4), FileClass::Sidecar(17, SidecarKind::Dv, 4)),
            (sidecar_file_name(5, SidecarKind::RowIdentMap, 1), FileClass::Sidecar(5, SidecarKind::RowIdentMap, 1)),
            (crate::bankstats::bankstats_file_name(9), FileClass::BankStats(9)),
            (format!("{}.tmp", crate::bankstats::bankstats_file_name(9)), FileClass::BankStatsTmp(9)),
            (run_file_name(9, 0), FileClass::Run(9, 0)),
            (run_file_name(u64::MAX, u32::MAX), FileClass::Run(u64::MAX, u32::MAX)),
            (run_tmp_file_name(9, 2), FileClass::RunTmp(9, 2)),
            (ROWID_FILE_NAME.to_string(), FileClass::Rowid),
            (SEGEXT_FILE_NAME.to_string(), FileClass::SegExt),
            (runrev_file_name(12), FileClass::RunRev(12)),
            (runrev_file_name(u64::MAX), FileClass::RunRev(u64::MAX)),
        ];
        for (name, class) in &cases {
            assert_eq!(classify(name), Some(*class), "{name}");
        }
        for bad in ["", ".", "..", "../part-1.pgrc", "part-1.pgrc/x", "part-01.pgrc", "tmp-1.pgrct", "tmp-1-x.pgrct", "CURRENT2", "manifest-0x1.pgrcm", "bankstats-01.pgrcbs", "sections-1.pgrcx.tmp", "pg_internal.init", "srun-1.pgrcr", "srun-1-01.pgrcr", "srun-01-1.pgrcr", "srun-1-2-3.pgrcr", "srun-1-4294967296.pgrcr", "srun-1-2.pgrcr.tmp.tmp", "srun-01-2.pgrcr.tmp", "srun--2.pgrcr", "rowid.tmp", "rowid2", "ROWID", "segext.tmp", "SEGEXT", "runrev-.pgrcv", "runrev-012.pgrcv", "runrev-12.pgrcv.tmp", "runrev-12-1.pgrcv", "runrev-x.pgrcv", "runrev-18446744073709551616.pgrcv", "part-3-runrev-1.pgrcs"] {
            assert_eq!(classify(bad), None, "{bad}");
        }
    }

    #[test]
    fn table_dir_names_round_trip() {
        assert_eq!(parse_table_dir_name(&table_dir_name(16384)), Some((None, 16384)));
        assert_eq!(temp_table_dir_name(3, 16384), "pgrc_t3_16384");
        assert_eq!(parse_table_dir_name("pgrc_t3_16384"), Some((Some(3), 16384)));
        for bad in ["pgrc_", "pgrc_t", "pgrc_t3", "pgrc_t3_", "pgrc_016384", "pgrc_x", "16384", "pgrc_t03_1"] {
            assert_eq!(parse_table_dir_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn sections_file_names_round_trip_and_nothing_else_parses() {
        assert_eq!(sections_file_name(12), "sections-12.pgrcx");
        assert_eq!(parse_sections_file_name("sections-12.pgrcx"), Some(12));
        for bad in ["sections-012.pgrcx", "sections-.pgrcx", "sections-12.pgrcx.tmp", "manifest-12.pgrcm", "sections-x.pgrcx"] {
            assert_eq!(parse_sections_file_name(bad), None, "{bad}");
        }
    }
}

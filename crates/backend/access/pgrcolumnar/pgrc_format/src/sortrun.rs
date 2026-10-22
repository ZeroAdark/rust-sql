//! SORTRUNS-E0 (pgrc-plans/SORTRUNS-E0-STEP1.md D1): the manifest's sorted runs, extension tag
//! [`crate::manifest::EXT_RUNS`].
//!
//! A sorted run is a list of parts (its members, in key order: member `slot` holds keys at or below member
//! `slot + 1`'s) that one merge wrote. The record lists each run with its members and, when the run homes its rows'
//! IDs (law HOMES, reverse form LABELS or FRAMEIX), the row-ID ranges it homes: such a run's members have no
//! [`crate::manifest::EXT_RANGES`] entry, and an ID in the run's ranges is found through the run's reverse file
//! (`runrev-<run_id>.pgrcv`, [`crate::runrev`]). A run of form NONE (self-homed, after VACUUM FULL) homes nothing:
//! its members home their own ranges in `EXT_RANGES` as any part does, and it has no reverse file.
//!
//! The one-home law becomes "a part or a run": [`crate::manifest::check_row_ids`] walks the parts' and the HOMES
//! runs' ranges together. [`crate::manifest::MANIFESTF_RUNS`] is set exactly when the record is present, so a binary
//! that predates runs refuses the manifest instead of finding no home for a run-homed ID.
//!
//! Body, little-endian: `u8 version = 1, u8 law = 1 (HOMES), u16 0, u32 nruns`, then per run, `run_id` ascending:
//! `u64 run_id, u8 level, u8 rev, u8 rank, u8 flags = 0, u32 nmembers, u64 rows, u64 rev_len, u32 rev_crc,
//! u32 nranges` (40 B), `nmembers × u32 part_no` (0 = an emptied slot), `nranges × (u64 lo, u64 hi)`.

use crate::manifest::{ext_records, Manifest, EXT_RUNS};
use crate::rowid::ROWID_LIMIT;
use crate::wire::{put_u32, put_u64, put_u8, Cur};
use crate::{FormatError, FormatResult};

/// The record's version byte.
pub const RUNS_VERSION: u8 = 1;
/// Law HOMES: a run with a reverse homes its ranges (the base table's law; COVERAGE, 2, is reserved for M9's index
/// directory and never written by E0).
pub const LAW_HOMES: u8 = 1;
/// The most members a run holds (a slot is a `u16` in the reverse file).
pub const MAX_MEMBERS: usize = 65535;
/// A run's fixed header bytes in the record.
pub const RUN_HEADER_LEN: usize = 40;

const AT: &str = "Manifest sorted runs";

fn corrupt() -> FormatError {
    FormatError::Corrupt { at: AT }
}

/// A run's ID → (member, ordinal) form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunRevForm {
    /// Self-homed: no reverse file; the members home their own ranges.
    None,
    /// A group label per ID rank, the groups' frames in the members' maps.
    Labels,
    /// A run-global frame number per ID rank.
    FrameIx,
}

impl RunRevForm {
    pub fn code(self) -> u8 {
        match self {
            RunRevForm::None => 0,
            RunRevForm::Labels => 1,
            RunRevForm::FrameIx => 2,
        }
    }

    pub fn of(c: u8) -> Option<RunRevForm> {
        Some(match c {
            0 => RunRevForm::None,
            1 => RunRevForm::Labels,
            2 => RunRevForm::FrameIx,
            _ => return None,
        })
    }

    /// The form's name in `pgrc_compact_stats` / `pgrc_runs_verify`.
    pub fn name(self) -> &'static str {
        match self {
            RunRevForm::None => "none",
            RunRevForm::Labels => "labels",
            RunRevForm::FrameIx => "frameix",
        }
    }
}

/// How the reverse ranks an ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankForm {
    /// The ID's offset in the run's ranges (every ID of the ranges has a rank; one absent at build is a hole).
    Dense,
    /// The ID's rank in the run's ID set at build, which the reverse file carries.
    Set,
}

impl RankForm {
    pub fn code(self) -> u8 {
        match self {
            RankForm::Dense => 0,
            RankForm::Set => 1,
        }
    }

    pub fn of(c: u8) -> Option<RankForm> {
        Some(match c {
            0 => RankForm::Dense,
            1 => RankForm::Set,
            _ => return None,
        })
    }

    /// The rank's name (SORTRUNS-E0 L2: the run merge's log line, `pgrc_compact_stats`).
    pub fn name(self) -> &'static str {
        match self {
            RankForm::Dense => "dense",
            RankForm::Set => "set",
        }
    }
}

/// One sorted run as the record holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortRun {
    /// The generation that published the run: unique in the table's life, never reused; names the reverse file.
    pub run_id: u64,
    /// 1 for a run merged from loose parts, one more than its inputs' highest for a merge of runs.
    pub level: u8,
    pub rev: RunRevForm,
    pub rank: RankForm,
    /// The run's rows at build (the members' rows then).
    pub rows: u64,
    /// The reverse file's length (0 for NONE).
    pub rev_len: u64,
    /// crc32c of the reverse file's page 0 (0 for NONE): ties the record to the exact file.
    pub rev_crc: u32,
    /// Part number per slot, in key order; 0 = an emptied slot (every row of the member dropped).
    pub members: Vec<u32>,
    /// The row-ID ranges the run homes (none for NONE): sorted, disjoint, non-empty `[lo, hi)`.
    pub ranges: Vec<(u64, u64)>,
}

impl SortRun {
    /// Whether the run homes its ranges (has a reverse).
    pub fn homes(&self) -> bool {
        self.rev != RunRevForm::None
    }

    /// The live members' part numbers (emptied slots skipped), in slot order.
    pub fn live(&self) -> impl Iterator<Item = u32> + '_ {
        self.members.iter().copied().filter(|&p| p != 0)
    }

    /// The number of IDs the run's ranges span (a DENSE reverse's rank count).
    pub fn span(&self) -> u64 {
        self.ranges.iter().map(|&(lo, hi)| hi - lo).sum()
    }

    /// The slot of `part_no`, `None` when the run lacks it.
    pub fn slot_of(&self, part_no: u32) -> Option<usize> {
        if part_no == 0 {
            return None;
        }
        self.members.iter().position(|&p| p == part_no)
    }
}

/// The record's body for `runs` (`run_id` ascending).
pub fn try_encode(runs: &[SortRun]) -> FormatResult<Vec<u8>> {
    let len = 8 + runs.iter().map(|r| RUN_HEADER_LEN + 4 * r.members.len() + 16 * r.ranges.len()).sum::<usize>();
    let mut out = crate::try_vec(len, AT)?;
    put_u8(&mut out, RUNS_VERSION);
    put_u8(&mut out, LAW_HOMES);
    out.extend_from_slice(&[0, 0]);
    put_u32(&mut out, runs.len() as u32);
    for r in runs {
        put_u64(&mut out, r.run_id);
        put_u8(&mut out, r.level);
        put_u8(&mut out, r.rev.code());
        put_u8(&mut out, r.rank.code());
        put_u8(&mut out, 0);
        put_u32(&mut out, r.members.len() as u32);
        put_u64(&mut out, r.rows);
        put_u64(&mut out, r.rev_len);
        put_u32(&mut out, r.rev_crc);
        put_u32(&mut out, r.ranges.len() as u32);
        for &p in &r.members {
            put_u32(&mut out, p);
        }
        for &(lo, hi) in &r.ranges {
            put_u64(&mut out, lo);
            put_u64(&mut out, hi);
        }
    }
    debug_assert_eq!(out.len(), len);
    Ok(out)
}

#[cfg(test)]
pub fn encode(runs: &[SortRun]) -> Vec<u8> {
    try_encode(runs).expect("encode")
}

/// Decode a record body, with every check that needs no manifest: the version and law, `run_id`s ascending, each
/// run's level, forms and flags, 1 to [`MAX_MEMBERS`] slots with at least one live, a NONE run with no ranges, no
/// reverse and rank DENSE, a HOMES run with ranges (sorted, disjoint, non-empty, below `ROWID_LIMIT`), a whole number
/// of reverse pages and, for DENSE, no more rows than its ranges span; no trailing bytes.
pub fn decode(body: &[u8]) -> FormatResult<Vec<SortRun>> {
    let mut c = Cur::new(body);
    let (version, law, r0, r1) = (c.u8(AT)?, c.u8(AT)?, c.u8(AT)?, c.u8(AT)?);
    if version != RUNS_VERSION {
        return Err(FormatError::BadVersion { at: AT, got: u32::from(version) });
    }
    if law != LAW_HOMES || r0 != 0 || r1 != 0 {
        return Err(corrupt());
    }
    let n = c.u32(AT)? as usize;
    if n.saturating_mul(RUN_HEADER_LEN) > c.remaining() {
        return Err(corrupt());
    }
    let mut out: Vec<SortRun> = crate::try_vec(n, AT)?;
    for _ in 0..n {
        let run_id = c.u64(AT)?;
        let (level, rev, rank, flags) = (c.u8(AT)?, c.u8(AT)?, c.u8(AT)?, c.u8(AT)?);
        let nmembers = c.u32(AT)? as usize;
        let (rows, rev_len, rev_crc, nranges) = (c.u64(AT)?, c.u64(AT)?, c.u32(AT)?, c.u32(AT)? as usize);
        let rev = RunRevForm::of(rev).ok_or_else(corrupt)?;
        let rank = RankForm::of(rank).ok_or_else(corrupt)?;
        let ok = run_id != 0
            && out.last().is_none_or(|l| l.run_id < run_id)
            && level >= 1
            && flags == 0
            && (1..=MAX_MEMBERS).contains(&nmembers)
            && rows >= 1
            && nmembers.saturating_mul(4).saturating_add(nranges.saturating_mul(16)) <= c.remaining();
        if !ok {
            return Err(corrupt());
        }
        let mut members = crate::try_vec(nmembers, AT)?;
        for _ in 0..nmembers {
            members.push(c.u32(AT)?);
        }
        let mut ranges = crate::try_vec(nranges, AT)?;
        let mut prev_hi = 0u64;
        for _ in 0..nranges {
            let (lo, hi) = (c.u64(AT)?, c.u64(AT)?);
            if !(prev_hi <= lo && lo < hi && hi <= ROWID_LIMIT) {
                return Err(corrupt());
            }
            prev_hi = hi;
            ranges.push((lo, hi));
        }
        let r = SortRun { run_id, level, rev, rank, rows, rev_len, rev_crc, members, ranges };
        let shape = match r.rev {
            RunRevForm::None => r.ranges.is_empty() && r.rev_len == 0 && r.rev_crc == 0 && r.rank == RankForm::Dense,
            _ => !r.ranges.is_empty() && r.rev_len >= crate::runrev::PAGE as u64 && r.rev_len % crate::runrev::PAGE as u64 == 0 && (r.rank == RankForm::Set || r.rows <= r.span()),
        };
        if !shape || r.live().next().is_none() {
            return Err(corrupt());
        }
        out.push(r);
    }
    if c.remaining() != 0 {
        return Err(corrupt());
    }
    Ok(out)
}

/// The record's body in `ext`, `None` without one; a second record is `Corrupt`.
fn record(ext: &[u8]) -> FormatResult<Option<&[u8]>> {
    let mut out = None;
    for (t, bytes) in ext_records(ext)? {
        if t == EXT_RUNS && out.replace(bytes).is_some() {
            return Err(corrupt());
        }
    }
    Ok(out)
}

/// Whether an extension block holds an [`EXT_RUNS`] record (header flag [`crate::manifest::MANIFESTF_RUNS`]).
pub fn has_runs(ext: &[u8]) -> bool {
    crate::manifest::has_tag(ext, EXT_RUNS)
}

/// The runs of an extension block, decoded ([`decode`]'s checks), empty without the record.
pub fn runs_in(ext: &[u8]) -> FormatResult<Vec<SortRun>> {
    match record(ext)? {
        None => Ok(Vec::new()),
        Some(b) => decode(b),
    }
}

/// The manifest's runs, checked against it ([`check`]), empty without the record.
pub fn runs(m: &Manifest) -> FormatResult<Vec<SortRun>> {
    let r = runs_in(&m.ext)?;
    check_against(m, &r)?;
    Ok(r)
}

/// The manifest checks of the runs, run by [`Manifest::decode`]: every live slot names a part of the manifest, and
/// no part is in two slots (of one run or two); a member of a HOMES run has no `EXT_RANGES` entry (its IDs' home is
/// the run). The ranges' alignment, floor and one home with the parts' are [`crate::manifest::check_row_ids`]'s.
pub fn check(m: &Manifest) -> FormatResult<()> {
    let r = runs_in(&m.ext)?;
    check_against(m, &r)
}

fn check_against(m: &Manifest, runs: &[SortRun]) -> FormatResult<()> {
    if runs.is_empty() {
        return Ok(());
    }
    let mut seen: Vec<u32> = runs.iter().flat_map(|r| r.live()).collect();
    seen.sort_unstable();
    if seen.windows(2).any(|w| w[0] == w[1]) || seen.iter().any(|&p| m.parts.binary_search_by_key(&p, |x| x.part_no).is_err()) {
        return Err(corrupt());
    }
    let ranged: Vec<u32> = crate::manifest::range_entries(m)?.into_iter().map(|e| e.0).collect();
    for r in runs.iter().filter(|r| r.homes()) {
        if r.live().any(|p| ranged.binary_search(&p).is_ok()) {
            return Err(corrupt());
        }
    }
    Ok(())
}

/// The HOMES runs' ranges, appended to `out` (for the one-home check).
pub fn homed_ranges(ext: &[u8], out: &mut Vec<(u64, u64)>) -> FormatResult<()> {
    for r in runs_in(ext)?.iter().filter(|r| r.homes()) {
        out.extend_from_slice(&r.ranges);
    }
    Ok(())
}

/// `ext` with its [`EXT_RUNS`] record replaced by `runs` (removed when empty), every other record kept in order.
pub fn with_runs(ext: &[u8], runs: &[SortRun]) -> FormatResult<Vec<u8>> {
    let body = (!runs.is_empty()).then(|| try_encode(runs)).transpose()?;
    crate::manifest::with_record(ext, EXT_RUNS, body.as_deref())
}

/// A publish's edit of the runs (SORTRUNS-E0 D1): each slot whose part `retired` names takes the part `repoint` maps
/// it to (a member rewritten in place: its DV folded, the run reverse unchanged), else 0 (the member left with every
/// row); a run left with no live slot is dropped (its reverse file goes with the next reclaim). `ext` unchanged
/// without a record or when nothing applies. `repoint`'s targets must not be members already.
pub fn retire_members(ext: &[u8], retired: &[u32], repoint: &[(u32, u32)]) -> FormatResult<Vec<u8>> {
    let Some(_) = record(ext)? else { return crate::try_to_vec(ext, AT) };
    let mut runs = runs_in(ext)?;
    let mut changed = false;
    for r in runs.iter_mut() {
        for p in r.members.iter_mut().filter(|p| **p != 0) {
            if let Some(&(_, to)) = repoint.iter().find(|x| x.0 == *p) {
                *p = to;
                changed = true;
            } else if retired.contains(p) {
                *p = 0;
                changed = true;
            }
        }
    }
    if !changed {
        return crate::try_to_vec(ext, AT);
    }
    runs.retain(|r| r.live().next().is_some());
    with_runs(ext, &runs)
}

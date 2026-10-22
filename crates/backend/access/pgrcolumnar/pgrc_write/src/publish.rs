//! Manifest publish (spec §13; delta v2 design §7.4) and the effective-generation walk.
//!
//! ## The publish (WAL first)
//!
//! [`publish_parts`] builds a [`PublishPlan`] over the base generation, stages the stats runs its stats plan writes
//! (P-1: `crate::stats`; bounded by the merge policy, none for most publishes), the stats plane (an empty base only)
//! and the Stats companions through the table's [`LoggedVfs`] (each a logged file, finished by `file_done`), then
//! logs one `PUBLISH` whose apply closure — [`crate::plan::apply_planned`], redo's [`crate::plan::apply_publish`] over
//! the image the plan encoded — patches and renames the parts, renames the staged files, writes `manifest-<gen>` (its
//! `EXT_STATS` names the root run) and repoints `CURRENT`.
//! Nothing is fsynced here: the checkpointer syncs the registered directory, and redo rebuilds anything a crash lost.
//! The caller's commit record makes the generation effective.
//!
//! ## The clog fence (#254)
//!
//! A generation is EFFECTIVE iff its epoch-qualified `publisher_fxid` committed ([`TxnProbe`] — a passed
//! capability over the clog). [`effective_manifest`] tries the `CURRENT` hint, walks `prev_gen` past
//! non-committed generations, and falls back to a directory scan when the hint chain is unreadable. An aborted
//! or crashed-before-commit publish is structurally invisible; end-of-recovery cleanup
//! ([`crate::cleanup::cleanup_dir`]) reclaims it.
//!
//! ## Serialization precondition
//!
//! Publishes to one table are serialized by the caller (the table's publish lock). Two live publishers on one
//! directory would race `next_part_no`. The chain is linear: a publisher chains on the effective generation or on its
//! own transaction tree's in-progress one ([`TxnProbe::is_own`]). A FOREIGN in-progress head is stacked on by a
//! publisher that [`TxnProbe::stacks`] when the run under it only added parts (their creators tagged, `EXT_UNDER`);
//! otherwise it is [`WriteError::PublishBusy`], returned before anything is staged, and the caller waits for that
//! transaction to end (PUBLISH-WAIT: `pgrc_am::ingest`; a background publisher defers) and redoes the publish from a
//! fresh read.

use std::borrow::Cow;
use std::collections::BTreeMap;

use pgrc_format::dirlayout::{parse_manifest_file_name, part_file_name, run_file_name, run_tmp_file_name, sidecar_file_name, CURRENT_FILE_NAME};
use pgrc_format::facts::FactsImage;
use pgrc_format::manifest::{stats_ref, under, with_stats_ref, CommitPointer, Manifest, PartRecord, UnderEntry};
use pgrc_format::sidecar::{SidecarFileHeader, SidecarKind};
use pgrc_format::wal::PublishKind;
use pgrc_format::wire::crc32c;
use pgrc_format::bankstats::PartIdent;
use pgrc_format::FormatError;

use crate::plan::{apply_planned, read_manifest, PlanBuilder, PublishPlan};
use crate::seal::{PartSpec, SealedPart};
use crate::stats::{self, ident_of, FullTest, PartStats, Run, StatsState, LOOSE_MAX, SMALL_RUNS_MAX};
use crate::walsink::{LoggedVfs, WalSink};
use crate::wvfs::WriteVfs;
use crate::{WriteError, WriteResult};

/// Clog verdicts for a FullTransactionId (a passed capability; M3-H wires
/// the real clog probe, tests script it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnVerdict {
    Committed,
    Aborted,
    InProgress,
}

pub trait TxnProbe {
    fn verdict(&self, fxid: u64) -> TxnVerdict;

    /// `publisher` is in the transaction tree of the publisher stamping `own`: its in-progress generation is a base
    /// to chain on, never a foreign head. Publishes stamp the current SUBtransaction's fxid, so the clog probe answers
    /// `TransactionIdIsCurrentTransactionId` (a savepoint's publish chains on its parent's and on a released
    /// subtransaction's generation); the default is equality.
    fn is_own(&self, publisher: u64, own: u64) -> bool {
        publisher == own
    }

    /// GENID: the numbers this process already handed out in the table directory `dir`, `(generation, next part
    /// number)`: the highest generation and the highest `next_part_no` any generation of `dir` held in this process
    /// (0 = none known). A publish numbers its generation and its parts above them as well as above its base, so two
    /// contents never share a (directory, generation) or a part file name while any cache of this process can hold the
    /// first: a same-transaction TRUNCATE empties the directory, and a rolled-back publish leaves numbers its successor's
    /// base does not know. The default knows none (the numbering is the base's alone).
    fn used_numbers(&self, _dir: &str) -> (u64, u32) {
        (0, 0)
    }

    /// SEAL-OFFHEAD: this publisher chains on a FOREIGN in-progress head instead of [`WriteError::PublishBusy`], when
    /// every generation the in-progress run under it published only added parts ([`effective_base`]): a maintenance
    /// step (a rotation, a seal, a freed segment, a settle) or, CONCLOAD, a bulk load. The parts the run added ride
    /// the new generation tagged with their creator (`EXT_UNDER`), so every reader resolves them by its own snapshot.
    /// COMPACTP: so does a compaction (it retires only committed parts, none of the run's). The default (compile, a
    /// first activation) never does.
    fn stacks(&self) -> bool {
        false
    }
}

/// GENID: the generation a publish on `base_gen` numbers itself, given the directory's `used` numbers
/// ([`TxnProbe::used_numbers`]): above both.
pub fn next_gen(base_gen: u64, used: (u64, u32)) -> u64 {
    base_gen.max(used.0) + 1
}

/// Publish witnesses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishOutcome {
    pub gen: u64,
    /// The record's kind (the P-1 bench times each kind's own publish).
    pub kind: PublishKind,
    pub part_nos: Vec<u32>,
    /// [fmt-land] The bank-grain stats plane write witness (best-effort
    /// derived sidecar; `bankplane.rs`). Never affects publish success.
    pub bankstats_plane: crate::bankplane::PlaneOutcome,
    /// The generation as written (the process catalog's record of this publish).
    pub manifest: Manifest,
    /// `CURRENT`'s record naming it: the manifest's length and the plan's crc (P-1 L4a: the catalog holds it, so the
    /// next publish's check that `CURRENT` still names its base encodes nothing).
    pub pointer: CommitPointer,
    /// The generation's stats runs and loose parts (P-1): its planner facts and answers, and the next publish's base.
    /// The catalog assembles the image and answers from them on first use; the publisher assembles nothing.
    pub stats: StatsState,
    /// The stats runs this publish wrote (the root carrier included) and their bytes.
    pub runs_written: u32,
    pub run_bytes: u64,
    /// Microseconds in the stats: the base state, the installs' stats, the plan and the run encodes.
    pub stats_us: u64,
    /// The base was the caller's generation ([`BaseStats::Gen`], `CURRENT` still naming it): no manifest or run was
    /// read. The bench's witness that a publish took the catalog's path.
    pub base_given: bool,
    /// The parts of the legacy run this publish wrote over a root-less base of `LOOSE_MAX` parts or more (a generation
    /// written before P-1, OC-5); 0 otherwise.
    pub legacy_parts: u32,
    /// SEAL-OFFHEAD: the foreign transactions still in progress whose parts this generation carries tagged
    /// (`EXT_UNDER`): empty unless a stacking publisher chained over them.
    pub stacked_over: Vec<u64>,
    /// SEAL-OFFHEAD: the parts of rolled-back creators the base carried tagged, which this generation drops.
    pub dropped: Vec<u32>,
}

/// The base generation as the process catalog holds it (P-1 choice 3): used when `CURRENT` still names it.
#[derive(Clone, Copy)]
pub struct BaseGen<'a> {
    pub manifest: &'a Manifest,
    /// `CURRENT`'s record for this generation (the catalog computes it once per generation), so the check that
    /// `CURRENT` still names the base encodes nothing.
    pub pointer: CommitPointer,
    pub stats: &'a StatsState,
    /// K1 L7's full test of the table (the catalog's, from an open part): `hold`. `None`: built from the directory
    /// when a merge could happen.
    pub full: Option<&'a FullTest>,
}

/// What the caller supplies of the base generation's stats.
#[derive(Clone, Copy, Default)]
pub enum BaseStats<'a> {
    /// Nothing: the publisher loads them from the directory (the root run, the runs it lists, the loose footers).
    #[default]
    None,
    /// The catalog's generation.
    Gen(BaseGen<'a>),
    /// A root-less generation's image and answers as the catalog holds them (a pre-P-1 directory's sections file, or
    /// a completion): loaded like `None`, and for a root-less base of `LOOSE_MAX` parts or more (OC-5) the seed of its
    /// legacy run, so its parts are not walked.
    Folded(Option<&'a FactsImage>, Option<&'a [(u32, Vec<u8>)]>),
}

impl<'a> From<(Option<&'a FactsImage>, Option<&'a [(u32, Vec<u8>)]>)> for BaseStats<'a> {
    fn from((i, a): (Option<&'a FactsImage>, Option<&'a [(u32, Vec<u8>)]>)) -> BaseStats<'a> {
        BaseStats::Folded(i, a)
    }
}

/// `CURRENT`'s record for `m` (what [`BaseGen::pointer`] holds): the manifest's encoded length and trailing crc.
pub fn commit_pointer(m: &Manifest) -> CommitPointer {
    let bytes = m.encode();
    CommitPointer::new(m.header.gen, bytes.len() as u64, crc32c(&bytes[..bytes.len() - 4]))
}

impl<'a> From<Option<BaseGen<'a>>> for BaseStats<'a> {
    fn from(g: Option<BaseGen<'a>>) -> BaseStats<'a> {
        g.map_or(BaseStats::None, BaseStats::Gen)
    }
}

fn full_path(dir: &str, name: &str) -> String {
    format!("{dir}/{name}")
}

/// The newest EFFECTIVE (committed-publisher) manifest generation, or None
/// for an empty table. `CURRENT` is a hint + O(1) entry point; the chain
/// walk and the scan fallback are the authority (spec §13.2).
/// `own_fxid`: Some(f) for a publisher — an in-progress generation of its own
/// transaction tree ([`TxnProbe::is_own`]) is a valid base to chain on (a
/// second publish in one transaction, or across a savepoint), and a FOREIGN
/// in-progress head refuses the publish with [`WriteError::PublishBusy`]
/// (reusing its generation and part numbers would overwrite that
/// transaction's files); None for readers, which skip every non-committed
/// generation.
pub fn effective_manifest(
    vfs: &mut dyn WriteVfs,
    dir: &str,
    probe: &dyn TxnProbe,
    own_fxid: Option<u64>,
) -> WriteResult<Option<Manifest>> {
    Ok(effective_base(vfs, dir, probe, own_fxid)?.map(|(m, _)| m))
}

/// [`effective_manifest`], with the parts a stacking publisher ([`TxnProbe::stacks`]) chains over: for a foreign
/// in-progress head, the creator of every part of the run under it it can name ([`stack_run`]). Empty otherwise.
pub fn effective_base(
    vfs: &mut dyn WriteVfs,
    dir: &str,
    probe: &dyn TxnProbe,
    own_fxid: Option<u64>,
) -> WriteResult<Option<(Manifest, Vec<UnderEntry>)>> {
    // Hint arm: CURRENT → candidate gen → walk prev_gen.
    let current_path = full_path(dir, CURRENT_FILE_NAME);
    if vfs.exists_path(&current_path)? {
        if let Ok(bytes) = vfs.read_full(&current_path) {
            if let Ok(cp) = CommitPointer::decode(&bytes) {
                if let Some(m) = walk_chain(vfs, dir, cp.gen, probe, own_fxid)? {
                    return Ok(Some(m));
                }
            }
        }
    }
    // Scan arm: newest decodable committed generation wins (the chain is
    // linear — serialized publishes — so scan order == chain order).
    let mut gens: Vec<u64> = vfs
        .list_dir(dir)?
        .iter()
        .filter_map(|n| parse_manifest_file_name(n))
        .collect();
    gens.sort_unstable_by(|a, b| b.cmp(a));
    for g in gens {
        if let Some(m) = read_manifest(vfs, dir, g) {
            match probe.verdict(m.header.publisher_fxid) {
                TxnVerdict::Committed => return Ok(Some((m, Vec::new()))),
                TxnVerdict::InProgress if own_fxid.is_some_and(|o| probe.is_own(m.header.publisher_fxid, o)) => {
                    return Ok(Some((m, Vec::new())))
                }
                TxnVerdict::InProgress if own_fxid.is_some() => {
                    return Err(WriteError::PublishBusy { holder_fxid: m.header.publisher_fxid })
                }
                _ => continue,
            }
        }
    }
    Ok(None)
}

/// Walk prev_gen from `gen` until a committed generation; None when the
/// chain is unreadable (caller falls back to the scan).
fn walk_chain(
    vfs: &mut dyn WriteVfs,
    dir: &str,
    mut gen: u64,
    probe: &dyn TxnProbe,
    own_fxid: Option<u64>,
) -> WriteResult<Option<(Manifest, Vec<UnderEntry>)>> {
    while gen != 0 {
        let Some(m) = read_manifest(vfs, dir, gen) else { return Ok(None) };
        match probe.verdict(m.header.publisher_fxid) {
            TxnVerdict::Committed => return Ok(Some((m, Vec::new()))),
            TxnVerdict::InProgress if own_fxid.is_some_and(|o| probe.is_own(m.header.publisher_fxid, o)) => {
                return Ok(Some((m, Vec::new())))
            }
            TxnVerdict::InProgress if own_fxid.is_some() => {
                return match own_fxid.filter(|_| probe.stacks()).map(|o| stack_run(vfs, dir, &m, probe, o)).transpose()?.flatten() {
                    Some(run) => Ok(Some((m, run))),
                    None => Err(WriteError::PublishBusy { holder_fxid: m.header.publisher_fxid }),
                };
            }
            _ => {}
        }
        gen = m.header.prev_gen;
    }
    Ok(None)
}

/// SEAL-OFFHEAD: the creators a stacking publisher learns when `head`'s publisher is a foreign transaction in
/// progress, walking `prev_gen` from `head` while the publisher is foreign and in progress or rolled back (a rolled-back
/// generation sits under an in-progress one only because that one stacked over it while it was in progress): each
/// walked generation's added parts (against the generation it built on) with its publisher, and the `EXT_UNDER` tags
/// of every walked generation and of the committed or own one the walk ends at. CONCLOAD: a publisher does not tag its
/// own parts, so with two loaders publishing in turn a part one of them tagged below the run can be untagged in the
/// run; the tags below still name it. `None` (the publisher waits or defers) unless every walked generation only added
/// parts or dropped parts of rolled-back creators ([`adds_only`]): a generation of that run that changed anything else
/// (segments, deletion vectors, the tombstone heaps' state, a retirement) would need its edits undone for a reader that
/// does not see it, which a tag cannot say.
fn stack_run(vfs: &mut dyn WriteVfs, dir: &str, head: &Manifest, probe: &dyn TxnProbe, own: u64) -> WriteResult<Option<Vec<UnderEntry>>> {
    stack_run_by(head, &mut |g| read_manifest(vfs, dir, g), probe, own)
}

/// [`stack_run`] over the manifests `read` returns.
fn stack_run_by(head: &Manifest, read: &mut dyn FnMut(u64) -> Option<Manifest>, probe: &dyn TxnProbe, own: u64) -> WriteResult<Option<Vec<UnderEntry>>> {
    let (mut run, mut retired) = (Vec::new(), Vec::new());
    let mut g = head.clone();
    loop {
        let prev = match g.header.prev_gen {
            0 => None,
            p => match read(p) {
                Some(m) => Some(m),
                None => return Ok(None),
            },
        };
        let Some((gone, added)) = adds_only(prev.as_ref(), &g) else { return Ok(None) };
        retired.extend(gone);
        run.extend(under(&g)?);
        run.extend(added.into_iter().map(|part_no| UnderEntry { part_no, fxid: g.header.publisher_fxid }));
        let Some(prev) = prev else { break };
        let p = prev.header.publisher_fxid;
        match probe.verdict(p) {
            TxnVerdict::InProgress if !probe.is_own(p, own) => g = prev,
            TxnVerdict::Aborted => g = prev,
            _ => {
                run.extend(under(&prev)?);
                break;
            }
        }
    }
    let dead = |n: u32| run.iter().any(|e| e.part_no == n && probe.verdict(e.fxid) == TxnVerdict::Aborted);
    Ok(retired.into_iter().all(dead).then_some(run))
}

/// COMPACTP item 2 (b): whether a stacking publisher (`probe.stacks()`) stamping `own` chains on the chain from
/// generation `head` as [`walk_chain`] walks it, over the manifests `read` returns (the process catalog's: a
/// compaction's check before its job decodes anything); false where its publish would be [`WriteError::PublishBusy`].
/// A generation `read` does not return ends the walk as chainable: the publish's own walk decides.
pub fn stacks_on(head: u64, read: &mut dyn FnMut(u64) -> Option<Manifest>, probe: &dyn TxnProbe, own: u64) -> WriteResult<bool> {
    let mut gen = head;
    while gen != 0 {
        let Some(m) = read(gen) else { return Ok(true) };
        let p = m.header.publisher_fxid;
        match probe.verdict(p) {
            TxnVerdict::Committed => return Ok(true),
            TxnVerdict::InProgress if probe.is_own(p, own) => return Ok(true),
            TxnVerdict::InProgress => return Ok(probe.stacks() && stack_run_by(&m, read, probe, own)?.is_some()),
            _ => {}
        }
        gen = m.header.prev_gen;
    }
    Ok(true)
}

/// The parts `g` retired and added over `prev` when parts are all it changed: the same schema, and the same extension
/// block but for the stats root, the under tags and the row-ID directory (IX0: an added part's `EXT_RANGES` entry, a
/// retired one's gone, `EXT_ROWID` raised), whose entries for the parts both list must be the same. `None` otherwise.
pub(crate) fn adds_only(prev: Option<&Manifest>, g: &Manifest) -> Option<(Vec<u32>, Vec<u32>)> {
    use pgrc_format::manifest::{ext_records, flat_ranges, EXT_RANGES, EXT_ROWID, EXT_STATS, EXT_UNDER};
    fn rest(ext: &[u8]) -> Option<Vec<(u16, &[u8])>> {
        Some(ext_records(ext).ok()?.into_iter().filter(|(t, _)| ![EXT_STATS, EXT_UNDER, EXT_RANGES, EXT_ROWID].contains(t)).collect())
    }
    let (pparts, pext) = prev.map_or((&[][..], &[][..]), |p| (&p.parts[..], &p.ext[..]));
    if prev.is_some_and(|p| p.header.schema_fingerprint != g.header.schema_fingerprint) || rest(pext)? != rest(&g.ext)? {
        return None;
    }
    if let Some(p) = prev {
        // IXCOPYPATH: the parts both list, by one merge of the two part lists
        let (before, after) = (flat_ranges(p).ok()?, flat_ranges(g).ok()?);
        let mut i = 0;
        for (j, rec) in g.parts.iter().enumerate() {
            while p.parts.get(i).is_some_and(|q| q.part_no < rec.part_no) {
                i += 1;
            }
            if p.parts.get(i).is_some_and(|q| q.part_no == rec.part_no) && before.of(i) != after.of(j) {
                return None;
            }
        }
    }
    let (retired, added) = part_changes(pparts, &g.parts);
    Some((retired, added.into_iter().map(|k| g.parts[k].part_no).collect()))
}

/// SEAL-OFFHEAD: what a publish on `base` does with the creators its `EXT_UNDER` tags and the stacked run `run` name
/// for the parts it holds: the tags the new generation carries (a creator still in progress), and the parts to drop
/// (a creator that rolled back: no reader ever sees them). A foreign creator in progress under a publisher that does
/// not stack is [`WriteError::PublishBusy`]: it waits for it as it would for the head it was.
fn under_plan(base: Option<&Manifest>, run: Vec<UnderEntry>, probe: &dyn TxnProbe, fxid: u64) -> WriteResult<(Vec<UnderEntry>, Vec<u32>)> {
    let Some(base) = base else { return Ok((Vec::new(), Vec::new())) };
    let (mut keep, mut dead, mut seen) = (Vec::new(), Vec::new(), std::collections::BTreeSet::new());
    for e in under(base)?.into_iter().chain(run) {
        if base.parts.binary_search_by_key(&e.part_no, |p| p.part_no).is_err() || !seen.insert(e.part_no) {
            continue;
        }
        match probe.verdict(e.fxid) {
            TxnVerdict::Committed => {}
            TxnVerdict::Aborted => dead.push(e.part_no),
            // the publisher's own parts need no tag (whoever sees this generation sees its publisher)
            TxnVerdict::InProgress if e.fxid == fxid => {}
            TxnVerdict::InProgress if probe.is_own(e.fxid, fxid) || probe.stacks() => keep.push(e),
            TxnVerdict::InProgress => return Err(WriteError::PublishBusy { holder_fxid: e.fxid }),
        }
    }
    keep.sort_unstable();
    dead.sort_unstable();
    Ok((keep, dead))
}

/// Publish sealed parts as one manifest generation. The caller holds the table's publish lock and commits after
/// this returns; the commit is the effectiveness switch (clog fence).
#[allow(clippy::too_many_arguments)]
pub fn publish_parts<'b>(
    vfs: &mut dyn WriteVfs,
    sink: &mut dyn WalSink,
    dir: &str,
    spec: &PartSpec,
    sealed: &[SealedPart],
    fxid: u64,
    probe: &dyn TxnProbe,
    base: impl Into<BaseStats<'b>>,
) -> WriteResult<PublishOutcome> {
    if sealed.is_empty() {
        return Err(WriteError::Contract { detail: "publish with zero sealed parts" });
    }
    use pgrc_format::manifest::{PART_FLAG_ROWMAP, PART_ROWMAP_FORM_SHIFT, PART_ROWMAP_GRANULES};
    let mut install = |_: Option<&Manifest>, b: &mut PlanBuilder<'_>| {
        sealed.iter().try_for_each(|s| {
            let no = b.install(s)?;
            if s.ids.len() > 1 {
                b.set_flags(no, PART_FLAG_ROWMAP | PART_ROWMAP_GRANULES << PART_ROWMAP_FORM_SHIFT);
            }
            Ok(())
        })
    };
    publish_with(vfs, sink, dir, spec, sealed, &[], PublishKind::Bulk, fxid, probe, base.into(), &mut install)
}

/// IDBURN: a bulk part whose writer's grants landed apart homes several ranges, every one whole but the last; its
/// rows map to them in order through a `RowIdentMap` (ROWMAP2 M1: the granules form, MONO; `None` for one range: the
/// implicit map).
fn bulk_runs(s: &SealedPart) -> Option<Vec<u8>> {
    if s.ids.len() < 2 {
        return None;
    }
    let mut ord = 0u64;
    let runs: Vec<(u64, u32)> = s
        .ids
        .iter()
        .map_while(|&(lo, hi)| {
            let len = (hi - lo).min(s.rows - ord);
            let run = (len > 0).then_some((lo, len as u32));
            ord += len;
            run
        })
        .collect();
    // ascending whole ranges: always MONO, and a MONO map always encodes
    pgrc_format::rowmap::encode_runs(&runs, pgrc_format::rowmap::Opts::default()).ok()
}

/// A publish that installs no part and changes the extension block only (the segment records of a rotation, F1):
/// `edit` gets the base generation and the plan.
#[allow(clippy::too_many_arguments)]
pub fn publish_ext<'b>(
    vfs: &mut dyn WriteVfs,
    sink: &mut dyn WalSink,
    dir: &str,
    spec: &PartSpec,
    fxid: u64,
    probe: &dyn TxnProbe,
    base_facts: impl Into<BaseStats<'b>>,
    edit: Edit<'_>,
) -> WriteResult<PublishOutcome> {
    publish_with(vfs, sink, dir, spec, &[], &[], PublishKind::Rotate, fxid, probe, base_facts.into(), edit)
}

/// A segment's seal (F1, design §4.5): its part, when it had committed rows, installed under the segment's reserved
/// `part_no` with its `RowIdentMap`; `edit` flips the segment record.
#[allow(clippy::too_many_arguments)]
pub fn publish_seal<'b>(
    vfs: &mut dyn WriteVfs,
    sink: &mut dyn WalSink,
    dir: &str,
    spec: &PartSpec,
    part: Option<(&SealedPart, u32, &[u8])>,
    fxid: u64,
    probe: &dyn TxnProbe,
    base_facts: impl Into<BaseStats<'b>>,
    edit: Edit<'_>,
) -> WriteResult<PublishOutcome> {
    let base_facts = base_facts.into();
    let sealed = part.map_or(&[][..], |(s, ..)| std::slice::from_ref(s));
    let sidecars: Vec<(u32, SidecarKind, u64, &[u8])> = part.iter().map(|&(_, no, map)| (no, SidecarKind::RowIdentMap, 1, map)).collect();
    use pgrc_format::manifest::{PART_FLAG_ROWMAP, PART_ROWMAP_FORM_SHIFT, PART_ROWMAP_GRANULES};
    let mut install = |base: Option<&Manifest>, b: &mut PlanBuilder<'_>| {
        if let Some((s, part_no, _)) = part {
            b.install_as(s, part_no)?;
            // ROWMAP2 M1: a seal's map is the granules form, authoritative (its absence is corruption)
            b.set_flags(part_no, PART_FLAG_ROWMAP | PART_ROWMAP_GRANULES << PART_ROWMAP_FORM_SHIFT);
        }
        edit(base, b)
    };
    publish_with(vfs, sink, dir, spec, sealed, &sidecars, PublishKind::Seal, fxid, probe, base_facts, &mut install)
}

/// A compile (F2 L7, design §4.7): each `(part_no, rows)` folds its `(granule, row)` deletions into the part's DV.
/// The DV is seeded with the part's existing one (replace-not-stack), encoded with `dv_gen` = the generation this
/// publishes and staged at that name; the part's record is upserted with the new triplet. The caller heap-deletes the
/// folded tombstones in the same transaction, so one commit switches both. `Ok(None)` when no part gains a row:
/// nothing is published.
#[allow(clippy::too_many_arguments)]
pub fn publish_compile<'b>(
    vfs: &mut dyn WriteVfs,
    sink: &mut dyn WalSink,
    dir: &str,
    spec: &PartSpec,
    marks: &[(u32, &[(u32, u16)])],
    fxid: u64,
    probe: &dyn TxnProbe,
    base_facts: impl Into<BaseStats<'b>>,
) -> WriteResult<Option<PublishOutcome>> {
    has_storage(spec)?;
    let base_facts = base_facts.into();
    let mut by_part: BTreeMap<u32, Vec<(u32, u16)>> = BTreeMap::new();
    for &(part_no, rows) in marks.iter().filter(|(_, rows)| !rows.is_empty()) {
        by_part.entry(part_no).or_default().extend_from_slice(rows);
    }
    if by_part.is_empty() {
        return Ok(None);
    }
    let base = plan_base(vfs, dir, fxid, probe, &base_facts)?.ok_or(WriteError::Contract { detail: "compile of a table with no generation" })?;
    let base_gen = base.header.gen;
    let gen = next_gen(base_gen, probe.used_numbers(dir));
    let mut dvs: Vec<(PartRecord, Vec<u8>)> = Vec::new();
    for (part_no, rows) in &by_part {
        let at = base.parts.binary_search_by_key(part_no, |p| p.part_no).map_err(|_| WriteError::Contract { detail: "compile names a part not in the base generation" })?;
        let rec = &base.parts[at];
        if rows.iter().any(|&(g, r)| g >= rec.granule_count || u32::from(r) >= rec.granule_rows) {
            return Err(WriteError::Contract { detail: "compile row beyond its part" });
        }
        let existing = if rec.dv_gen == 0 { None } else { Some(read_dv(vfs, dir, rec)?) };
        if let Some((payload, t)) = crate::dml::compile_part_dv(existing.as_deref(), *part_no, rows, gen)? {
            dvs.push((PartRecord { dv_gen: t.dv_gen, dv_len: t.dv_len, dv_crc: t.dv_crc, ..*rec }, payload));
        }
    }
    if dvs.is_empty() {
        return Ok(None);
    }
    let sidecars: Vec<(u32, SidecarKind, u64, &[u8])> = dvs.iter().map(|(r, p)| (r.part_no, SidecarKind::Dv, gen, p.as_slice())).collect();
    let mut edit = |b: Option<&Manifest>, plan: &mut PlanBuilder<'_>| {
        if b.map(|m| m.header.gen) != Some(base_gen) || plan.gen() != gen {
            return Err(WriteError::Contract { detail: "compile base generation moved" });
        }
        dvs.iter().for_each(|(r, _)| plan.upsert(*r));
        Ok(())
    };
    publish_with(vfs, sink, dir, spec, &[], &sidecars, PublishKind::Compile, fxid, probe, base_facts, &mut edit).map(Some)
}

/// TOMB: [`publish_compile`] whose plan `extra` also edits (the tombstone heaps' `EXT_TOMBS` state rides the DVs of
/// the heap it folds, in one publish). The folded tombstones are not heap-deleted: their heap retires whole.
/// `Ok(None)` when no part gains a row: nothing is published, and the caller publishes the edit alone.
#[allow(clippy::too_many_arguments)]
pub fn publish_compile_ext<'b>(
    vfs: &mut dyn WriteVfs,
    sink: &mut dyn WalSink,
    dir: &str,
    spec: &PartSpec,
    marks: &[(u32, &[(u32, u16)])],
    fxid: u64,
    probe: &dyn TxnProbe,
    base_facts: impl Into<BaseStats<'b>>,
    extra: Edit<'_>,
) -> WriteResult<Option<PublishOutcome>> {
    has_storage(spec)?;
    let base_facts = base_facts.into();
    let mut by_part: BTreeMap<u32, Vec<(u32, u16)>> = BTreeMap::new();
    for &(part_no, rows) in marks.iter().filter(|(_, rows)| !rows.is_empty()) {
        by_part.entry(part_no).or_default().extend_from_slice(rows);
    }
    if by_part.is_empty() {
        return Ok(None);
    }
    let base = plan_base(vfs, dir, fxid, probe, &base_facts)?.ok_or(WriteError::Contract { detail: "compile of a table with no generation" })?;
    let base_gen = base.header.gen;
    let gen = next_gen(base_gen, probe.used_numbers(dir));
    let mut dvs: Vec<(PartRecord, Vec<u8>)> = Vec::new();
    for (part_no, rows) in &by_part {
        let at = base.parts.binary_search_by_key(part_no, |p| p.part_no).map_err(|_| WriteError::Contract { detail: "compile names a part not in the base generation" })?;
        let rec = &base.parts[at];
        if rows.iter().any(|&(g, r)| g >= rec.granule_count || u32::from(r) >= rec.granule_rows) {
            return Err(WriteError::Contract { detail: "compile row beyond its part" });
        }
        let existing = if rec.dv_gen == 0 { None } else { Some(read_dv(vfs, dir, rec)?) };
        if let Some((payload, t)) = crate::dml::compile_part_dv(existing.as_deref(), *part_no, rows, gen)? {
            dvs.push((PartRecord { dv_gen: t.dv_gen, dv_len: t.dv_len, dv_crc: t.dv_crc, ..*rec }, payload));
        }
    }
    if dvs.is_empty() {
        return Ok(None);
    }
    let sidecars: Vec<(u32, SidecarKind, u64, &[u8])> = dvs.iter().map(|(r, p)| (r.part_no, SidecarKind::Dv, gen, p.as_slice())).collect();
    let mut edit = |b: Option<&Manifest>, plan: &mut PlanBuilder<'_>| {
        if b.map(|m| m.header.gen) != Some(base_gen) || plan.gen() != gen {
            return Err(WriteError::Contract { detail: "compile base generation moved" });
        }
        dvs.iter().for_each(|(r, _)| plan.upsert(*r));
        extra(b, plan)
    };
    publish_with(vfs, sink, dir, spec, &[], &sidecars, PublishKind::Compile, fxid, probe, base_facts, &mut edit).map(Some)
}

/// A compaction (K1, design §4.9): retire every input `(part_no, dv_gen)` of the base (its DV unchanged since the job
/// read it) and, when a row survived, install `part` = `(sealed, map form, map)` under the next part number, flagged a
/// compaction output with an authoritative `RowIdentMap` of that form. IX0: the output homes `sealed.ids` (its
/// inputs' ranges) in `EXT_RANGES`, and the retired parts' entries go (the plan's [`PlanBuilder::finish`]). One
/// `PUBLISH` of kind `Compact`; the part number installed.
#[allow(clippy::too_many_arguments)]
pub fn publish_compact<'b>(
    vfs: &mut dyn WriteVfs,
    sink: &mut dyn WalSink,
    dir: &str,
    spec: &PartSpec,
    part: Option<(&SealedPart, u32, &[u8])>,
    retire: &[(u32, u64)],
    fxid: u64,
    probe: &dyn TxnProbe,
    base_facts: impl Into<BaseStats<'b>>,
) -> WriteResult<(PublishOutcome, Option<u32>)> {
    has_storage(spec)?;
    let base_facts = base_facts.into();
    use pgrc_format::manifest::{PART_FLAG_COMPACTED, PART_FLAG_ROWMAP, PART_ROWMAP_FORM_SHIFT, PART_ROWMAP_GRANULES};
    let contract = |detail| WriteError::Contract { detail };
    // ROWMAP2 M2: the granules map is the one form a reader loads; the forms before M1 are retired
    if part.is_some_and(|(_, form, _)| form != PART_ROWMAP_GRANULES) {
        return Err(contract("a compaction output's RowIdentMap is not the granules form (the forms before ROWMAP2 M1 are retired)"));
    }
    let base = effective_manifest(vfs, dir, probe, Some(fxid))?.ok_or(contract("compaction of a table with no generation"))?;
    if retire.is_empty() || retire.iter().any(|&(no, dv_gen)| !base.parts.iter().any(|p| p.part_no == no && p.dv_gen == dv_gen)) {
        return Err(contract("compaction inputs are not the base generation's"));
    }
    let used = probe.used_numbers(dir);
    let out = part.map(|_| base.header.next_part_no.max(used.1));
    let sealed = part.map_or(&[][..], |(s, ..)| std::slice::from_ref(s));
    let base_gen = base.header.gen;
    let gen = next_gen(base_gen, used);
    // SORTRUNS-E0 L1, the gates' knob: the output as a one-member run that homes its IDs (its map a member's, its
    // reverse a file of the publish); never in production
    let home = match (crate::testruns::knob(), out, part) {
        (crate::testruns::Knob::Homes(form), Some(no), Some((s, _, map))) => {
            let mut ids = vec![0u64; s.rows as usize];
            pgrc_format::rowmap::GranMap::decode(map, s.rows)?.decode_ids(0, &mut ids);
            Some(crate::testruns::build(form, None, gen, no, &ids, &s.ids)?)
        }
        _ => None,
    };
    let sidecars: Vec<(u32, SidecarKind, u64, &[u8])> =
        out.iter().zip(part).map(|(&no, (_, _, map))| (no, SidecarKind::RowIdentMap, 1, home.as_ref().map_or(map, |h| &h.member_map[..]))).collect();
    let files: Vec<(String, &[u8])> = home.iter().map(|h| (pgrc_format::dirlayout::runrev_file_name(h.run.run_id), &h.file[..])).collect();
    let base_next = base.header.next_part_no;
    let holds = |m: &Manifest| retire.iter().all(|&(no, dv_gen)| m.parts.iter().any(|p| p.part_no == no && p.dv_gen == dv_gen));
    let mut edit = |b: Option<&Manifest>, plan: &mut PlanBuilder<'_>| {
        // COMPACTP (b): a stacked base whose run's creator rolled back since the read above (a verdict, not a publish:
        // the caller holds the table's publish lock) is passed by the publish's own walk, so its base is a generation
        // below; the inputs (committed parts under that run) are still that one's, with their deletion vectors. The
        // generation and the output are numbered above the read base's (GENID: the rolled-back run's numbers stay used)
        let moved = match b {
            Some(m) if m.header.gen < base_gen && holds(m) => {
                plan.number_above((base_gen, base_next));
                false
            }
            Some(m) => m.header.gen != base_gen,
            None => true,
        };
        if moved || plan.gen() != gen {
            return Err(contract("compaction base generation moved"));
        }
        retire.iter().for_each(|&(no, _)| plan.retire(no));
        if let (Some(no), Some((s, form, ..))) = (out, part) {
            plan.install_as(s, no)?;
            plan.set_flags(no, PART_FLAG_ROWMAP | form << PART_ROWMAP_FORM_SHIFT | PART_FLAG_COMPACTED);
            match (&home, crate::testruns::knob()) {
                (Some(h), _) => {
                    plan.set_ranges(no, Vec::new());
                    add_run(b, plan, h.run.clone())?;
                }
                (None, crate::testruns::Knob::SelfHomed) => add_self_run(b, plan, no, s.rows)?,
                _ => {}
            }
        }
        Ok(())
    };
    publish_with_files(vfs, sink, dir, spec, sealed, &sidecars, &files, PublishKind::Compact, fxid, probe, base_facts, &mut edit).map(|o| (o, out))
}

/// SORTRUNS-E0 L0, the gates' knob `PGRUST_PGRC_TEST_SELF_RUNS=1` (never set in production; [`crate::testruns`]):
/// every compaction output is published as a one-member self-homed sorted run (`EXT_RUNS`, form NONE: the member keeps
/// its `EXT_RANGES` entry, nothing reads the run), so the on-disk format of a table with runs (`MANIFESTF_RUNS`,
/// `PUBLISH_KIND_RUNS`) exists before the run merge writes one (the old-binary refusal leg, walcrash, standby,
/// dirhash). A later compaction of the output empties and drops the run ([`PlanBuilder::finish`]).
/// L1: `frameix` / `labels` publish a run that homes the output's IDs instead ([`add_run`]).

/// The knob's edit: the base's runs (`b`'s) and a NONE run of part `no` (`rows` rows) under the plan's generation.
pub(crate) fn add_self_run(b: Option<&Manifest>, plan: &mut PlanBuilder<'_>, no: u32, rows: u64) -> WriteResult<()> {
    use pgrc_format::sortrun::{RankForm, RunRevForm, SortRun};
    let run_id = plan.gen();
    add_run(b, plan, SortRun { run_id, level: 1, rev: RunRevForm::None, rank: RankForm::Dense, rows: rows.max(1), rev_len: 0, rev_crc: 0, members: vec![no], ranges: Vec::new() })
}

/// SORTRUNS-E0: the base's runs (`b`'s) and `run`, published by the plan, its job having read the base itself (the
/// gates' knob, VACUUM FULL's renumber): [`add_run_from`] with the base's generation as the job's start.
pub(crate) fn add_run(b: Option<&Manifest>, plan: &mut PlanBuilder<'_>, run: pgrc_format::sortrun::SortRun) -> WriteResult<()> {
    add_run_from(b, plan, run, b.map_or(0, |m| m.header.gen))
}

/// SORTRUNS-E0: the base's runs (`b`'s) and `run`, published by the plan, its job having started from generation
/// `start_gen`. Its `run_id` is the plan's generation (the gates' knob, a renumbered run) or, L2, a generation number
/// its job reserved before it wrote the run's reverse (so the file and its page 0 are whole before the publish):
/// [`run_id_ok`]. The runs stay in `run_id` order.
pub(crate) fn add_run_from(b: Option<&Manifest>, plan: &mut PlanBuilder<'_>, run: pgrc_format::sortrun::SortRun, start_gen: u64) -> WriteResult<()> {
    let mut runs = b.map_or(Ok(Vec::new()), |m| pgrc_format::sortrun::runs_in(&m.ext))?;
    if !run_id_ok(run.run_id, start_gen, plan.gen(), &runs) {
        return Err(WriteError::Contract { detail: "a sorted run's ID is its publish's generation or one its job reserved after the generation it read, and no run's of the base" });
    }
    runs.push(run);
    runs.sort_by_key(|r| r.run_id);
    plan.set_runs(&runs)
}

/// SORTRUNS-COPYRACE (main 10-08; pgrc-plans/SORTRUNS-E0-STEP1.md §9.6): may run `id` join a publish of generation
/// `plan_gen` whose base holds `base_runs`, its job having started from generation `start_gen`? The ID is a generation
/// number the job reserved after `start_gen` (`pgrc_am::catalog::reserve_gen`: never a generation's, never another
/// job's) or the publish's own, so it lies above `start_gen` and at or below `plan_gen`, and no run of the base has it.
/// Other publishes (a COPY's: no maintenance lock) may land between the reservation and the run's publish, so the
/// base's generation may pass the ID: uniqueness rests on the reservation, not on "above the base's generation".
pub fn run_id_ok(id: u64, start_gen: u64, plan_gen: u64, base_runs: &[pgrc_format::sortrun::SortRun]) -> bool {
    id > start_gen && id <= plan_gen && !base_runs.iter().any(|r| r.run_id == id)
}

/// The temp sequence space of [`publish_with_files`]' files (sidecars use `0x4000_0000`, stats sidecars `0x8000_0000`).
const FILE_TMP_SEQ: u32 = 0x2000_0000;
/// SORTRUNS-E0 L2: the temp sequence space of a run job's staged files (`TableWriter::write_file`: the run's reverse),
/// inside [`FILE_TMP_SEQ`]'s.
pub const RUN_FILE_TMP_SEQ: u32 = FILE_TMP_SEQ | 0x0010_0000;

/// SORTRUNS-E0 L2, DV carry ([`publish_runs`]): given the publish's base, the ordinals of each member slot whose rows
/// its inputs' DVs marked since the job read them.
pub type Carry<'c> = &'c mut dyn FnMut(&Manifest) -> WriteResult<Vec<(usize, Vec<u32>)>>;

/// SORTRUNS-E0 L2 (COLIDX2 §17.1.3 item 5, §17.1.4): a run merge's one `PUBLISH` (kind `Compact`), its job having
/// read generation `start_gen` (the run's ID was reserved after it; other publishes may have landed since:
/// [`run_id_ok`]). The inputs
/// `retire` (part numbers) leave, each with the DV the base holds now; every sealed part is installed, in seal order, as
/// a member of `run` under the next part numbers, with its map `maps[k]` (RUN form) and no `EXT_RANGES` entry (the
/// run homes its members' IDs, `run.ranges`); the run's reverse, staged by the job, is renamed into place (`staged`:
/// (temp, name)). DV carry: `carry(base)`, under the publish lock, names the rows the inputs' DVs marked since the
/// job read them, as member ordinals; each member with any gets a DV of this publish. The inputs' runs empty and drop
/// ([`PlanBuilder::finish`]). The part numbers installed.
#[allow(clippy::too_many_arguments)]
pub fn publish_runs<'b>(
    vfs: &mut dyn WriteVfs,
    sink: &mut dyn WalSink,
    dir: &str,
    spec: &PartSpec,
    sealed: &[SealedPart],
    maps: &[Vec<u8>],
    retire: &[u32],
    mut run: pgrc_format::sortrun::SortRun,
    start_gen: u64,
    staged: &[(String, String)],
    carry: Carry<'_>,
    fxid: u64,
    probe: &dyn TxnProbe,
    base_facts: impl Into<BaseStats<'b>>,
) -> WriteResult<(PublishOutcome, Vec<u32>)> {
    use pgrc_format::manifest::{PART_FLAG_COMPACTED, PART_FLAG_ROWMAP, PART_ROWMAP_FORM_SHIFT, PART_ROWMAP_GRANULES};
    let base_facts = base_facts.into();
    let contract = |detail| WriteError::Contract { detail };
    if sealed.is_empty() || sealed.len() != maps.len() || !run.homes() || retire.is_empty() {
        return Err(contract("a run merge's members, maps and inputs"));
    }
    let base = effective_manifest(vfs, dir, probe, Some(fxid))?.ok_or(contract("run merge of a table with no generation"))?;
    let now: Vec<(u32, u64)> = retire
        .iter()
        .map(|&no| base.parts.iter().find(|p| p.part_no == no).map(|p| (no, p.dv_gen)))
        .collect::<Option<_>>()
        .ok_or(contract("run merge inputs are not the base generation's"))?;
    let used = probe.used_numbers(dir);
    let first = base.header.next_part_no.max(used.1);
    let nos: Vec<u32> = (0..sealed.len() as u32).map(|k| first + k).collect();
    let base_gen = base.header.gen;
    let gen = next_gen(base_gen, used);
    if run.run_id == 0 || run.run_id >= gen {
        return Err(contract("a run merge's run ID is not reserved below its publish"));
    }
    let mut dvs: Vec<(u32, crate::dml::DvTriplet, Vec<u8>)> = Vec::new();
    for (slot, ords) in carry(&base)? {
        let s = sealed.get(slot).ok_or(contract("DV carry names no member"))?;
        let grain = pgrc_format::part::FooterFixed::decode(&s.footer_image)?.granule_rows;
        if grain == 0 || ords.iter().any(|&o| u64::from(o) >= s.rows) {
            return Err(contract("DV carry names a row past its member"));
        }
        let rows: Vec<(u32, u16)> = ords.iter().map(|&o| (o / grain, (o % grain) as u16)).collect();
        if let Some((payload, t)) = crate::dml::compile_part_dv(None, nos[slot], &rows, gen)? {
            dvs.push((nos[slot], t, payload));
        }
    }
    run.members = nos.clone();
    let sidecars: Vec<(u32, SidecarKind, u64, &[u8])> = nos.iter().zip(maps).map(|(&no, m)| (no, SidecarKind::RowIdentMap, 1, &m[..])).collect();
    // a member's DV is staged as one of the publish's files: a publish stages one sidecar a part (the temp name
    // `stage_sidecar_gen` gives it), here its map; the image and the name are the sidecar's
    let dv_images: Vec<(String, Vec<u8>)> = dvs
        .iter()
        .map(|(no, _, p)| (sidecar_file_name(*no, SidecarKind::Dv, gen), crate::sidecar::sidecar_file_image(SidecarKind::Dv, [0u8; 16], spec.schema_fingerprint, p)))
        .collect();
    let files: Vec<(String, &[u8])> = dv_images.iter().map(|(n, b)| (n.clone(), &b[..])).collect();
    let base_next = base.header.next_part_no;
    let holds = |m: &Manifest| now.iter().all(|&(no, dv_gen)| m.parts.iter().any(|p| p.part_no == no && p.dv_gen == dv_gen));
    let mut edit = |b: Option<&Manifest>, plan: &mut PlanBuilder<'_>| {
        // COMPACTP (b), as publish_compact: a stacked base whose run's creator rolled back since the read
        let moved = match b {
            Some(m) if m.header.gen < base_gen && holds(m) => {
                plan.number_above((base_gen, base_next));
                false
            }
            Some(m) => m.header.gen != base_gen,
            None => true,
        };
        if moved || plan.gen() != gen {
            return Err(contract("run merge base generation moved"));
        }
        now.iter().for_each(|&(no, _)| plan.retire(no));
        for (s, &no) in sealed.iter().zip(&nos) {
            plan.install_as(s, no)?;
            plan.set_flags(no, PART_FLAG_ROWMAP | PART_ROWMAP_GRANULES << PART_ROWMAP_FORM_SHIFT | PART_FLAG_COMPACTED);
            plan.set_ranges(no, Vec::new());
        }
        for (no, t, _) in &dvs {
            plan.set_dv(*no, t.dv_gen, t.dv_len, t.dv_crc);
        }
        for (tmp, name) in staged {
            plan.rename(tmp.clone(), name.clone());
        }
        add_run_from(b, plan, run.clone(), start_gen)
    };
    publish_with_files(vfs, sink, dir, spec, sealed, &sidecars, &files, PublishKind::Compact, fxid, probe, base_facts, &mut edit).map(|o| (o, nos.clone()))
}

/// SORTRUNS-E0 L3b1: VACUUM FULL's renumber of a sorted run, one `PUBLISH` (kind `Compact`) under the caller's
/// AccessExclusiveLock (no DML runs: an input whose DV moved since the read is a contract error, nothing is carried).
/// The inputs `retire` (the run's live members) leave; every sealed part is installed, in seal order, under the next
/// part numbers with its map `maps[k]` (`(form, body)`) and its own dense `EXT_RANGES` entry `ranges[k]`, as a member
/// of a self-homed run (form NONE: nothing reads a reverse) of `level`, numbered by this publish's generation. The old
/// run empties and drops ([`PlanBuilder::finish`]); its reverse goes at the next reclaim. The part numbers installed.
#[allow(clippy::too_many_arguments)]
pub fn publish_self_run<'b>(
    vfs: &mut dyn WriteVfs,
    sink: &mut dyn WalSink,
    dir: &str,
    spec: &PartSpec,
    sealed: &[SealedPart],
    maps: &[(u32, Vec<u8>)],
    ranges: &[pgrc_format::manifest::IdRanges],
    retire: &[(u32, u64)],
    level: u8,
    fxid: u64,
    probe: &dyn TxnProbe,
    base_facts: impl Into<BaseStats<'b>>,
) -> WriteResult<(PublishOutcome, Vec<u32>)> {
    use pgrc_format::manifest::{PART_FLAG_COMPACTED, PART_FLAG_ROWMAP, PART_ROWMAP_FORM_SHIFT};
    use pgrc_format::sortrun::{RankForm, RunRevForm, SortRun};
    let base_facts = base_facts.into();
    let contract = |detail| WriteError::Contract { detail };
    if sealed.is_empty() || sealed.len() != maps.len() || sealed.len() != ranges.len() || retire.is_empty() {
        return Err(contract("a run renumber's members, maps, ranges and inputs"));
    }
    let base = effective_manifest(vfs, dir, probe, Some(fxid))?.ok_or(contract("run renumber of a table with no generation"))?;
    if !retire.iter().all(|&(no, dv_gen)| base.parts.iter().any(|p| p.part_no == no && p.dv_gen == dv_gen)) {
        return Err(contract("run renumber inputs moved since the read"));
    }
    let used = probe.used_numbers(dir);
    let first = base.header.next_part_no.max(used.1);
    let nos: Vec<u32> = (0..sealed.len() as u32).map(|k| first + k).collect();
    let base_gen = base.header.gen;
    let gen = next_gen(base_gen, used);
    let sidecars: Vec<(u32, SidecarKind, u64, &[u8])> = nos.iter().zip(maps).map(|(&no, m)| (no, SidecarKind::RowIdentMap, 1, &m.1[..])).collect();
    let rows: u64 = sealed.iter().map(|s| s.rows).sum();
    let mut edit = |b: Option<&Manifest>, plan: &mut PlanBuilder<'_>| {
        if !b.is_some_and(|m| m.header.gen == base_gen) || plan.gen() != gen {
            return Err(contract("run renumber base generation moved"));
        }
        retire.iter().for_each(|&(no, _)| plan.retire(no));
        for ((s, &no), (m, r)) in sealed.iter().zip(&nos).zip(maps.iter().zip(ranges)) {
            plan.install_as(s, no)?;
            plan.set_flags(no, PART_FLAG_ROWMAP | m.0 << PART_ROWMAP_FORM_SHIFT | PART_FLAG_COMPACTED);
            plan.set_ranges(no, r.clone());
        }
        add_run(b, plan, SortRun { run_id: gen, level, rev: RunRevForm::None, rank: RankForm::Dense, rows: rows.max(1), rev_len: 0, rev_crc: 0, members: nos.clone(), ranges: Vec::new() })
    };
    publish_with_files(vfs, sink, dir, spec, sealed, &sidecars, &[], PublishKind::Compact, fxid, probe, base_facts, &mut edit).map(|o| (o, nos.clone()))
}

/// SORTRUNS-E0 L3b1, DV carry of a member's slot swap ([`publish_member_swap`]): given the publish's base, the new
/// member's ordinals whose rows the old member's DV marked since the job read it.
pub type SwapCarry<'c> = &'c mut dyn FnMut(&Manifest) -> WriteResult<Vec<u32>>;

/// SORTRUNS-E0 L3b1 (`policy.rs` §6 item 9): a run member's slot swap, one `PUBLISH` (kind `Compact`). Member `from`
/// (of a run that homes its IDs) retires with the DV the base holds now; the one sealed part, its survivors in ordinal
/// order under the same virtual frames, is installed under the next part number with its map `map` (RUN form) and no
/// `EXT_RANGES` entry, and takes `from`'s slot ([`PlanBuilder::repoint_member`]): the run's record and its reverse are
/// unchanged, since the reverse names a slot and a virtual frame. DV carry: `carry(base)`, under the publish lock, names
/// the new member's ordinals whose rows `from`'s DV marked since the job read it. The part number installed.
#[allow(clippy::too_many_arguments)]
pub fn publish_member_swap<'b>(
    vfs: &mut dyn WriteVfs,
    sink: &mut dyn WalSink,
    dir: &str,
    spec: &PartSpec,
    sealed: &[SealedPart],
    map: &[u8],
    from: u32,
    carry: SwapCarry<'_>,
    fxid: u64,
    probe: &dyn TxnProbe,
    base_facts: impl Into<BaseStats<'b>>,
) -> WriteResult<(PublishOutcome, u32)> {
    use pgrc_format::manifest::{PART_FLAG_COMPACTED, PART_FLAG_ROWMAP, PART_ROWMAP_FORM_SHIFT, PART_ROWMAP_GRANULES};
    let base_facts = base_facts.into();
    let contract = |detail| WriteError::Contract { detail };
    let [s] = sealed else { return Err(contract("a member's slot swap seals one part")) };
    let base = effective_manifest(vfs, dir, probe, Some(fxid))?.ok_or(contract("member swap of a table with no generation"))?;
    let dv_gen = base.parts.iter().find(|p| p.part_no == from).map(|p| p.dv_gen).ok_or(contract("member swap input is not the base generation's"))?;
    let homes = pgrc_format::sortrun::runs_in(&base.ext)?.iter().any(|r| r.homes() && r.members.contains(&from));
    if !homes {
        return Err(contract("member swap input is no member of a run that homes its IDs"));
    }
    let used = probe.used_numbers(dir);
    let no = base.header.next_part_no.max(used.1);
    let base_gen = base.header.gen;
    let gen = next_gen(base_gen, used);
    let ords = carry(&base)?;
    let grain = pgrc_format::part::FooterFixed::decode(&s.footer_image)?.granule_rows;
    if grain == 0 || ords.iter().any(|&o| u64::from(o) >= s.rows) {
        return Err(contract("DV carry names a row past the member"));
    }
    let rows: Vec<(u32, u16)> = ords.iter().map(|&o| (o / grain, (o % grain) as u16)).collect();
    let dv = crate::dml::compile_part_dv(None, no, &rows, gen)?;
    let sidecars: Vec<(u32, SidecarKind, u64, &[u8])> = vec![(no, SidecarKind::RowIdentMap, 1, map)];
    let dv_images: Vec<(String, Vec<u8>)> = dv.iter().map(|(p, _)| (sidecar_file_name(no, SidecarKind::Dv, gen), crate::sidecar::sidecar_file_image(SidecarKind::Dv, [0u8; 16], spec.schema_fingerprint, p))).collect();
    let files: Vec<(String, &[u8])> = dv_images.iter().map(|(n, b)| (n.clone(), &b[..])).collect();
    let base_next = base.header.next_part_no;
    let holds = |m: &Manifest| m.parts.iter().any(|p| p.part_no == from && p.dv_gen == dv_gen);
    let mut edit = |b: Option<&Manifest>, plan: &mut PlanBuilder<'_>| {
        // as publish_runs: a stacked base whose run's creator rolled back since the read
        let moved = match b {
            Some(m) if m.header.gen < base_gen && holds(m) => {
                plan.number_above((base_gen, base_next));
                false
            }
            Some(m) => m.header.gen != base_gen,
            None => true,
        };
        if moved || plan.gen() != gen {
            return Err(contract("member swap base generation moved"));
        }
        plan.retire(from);
        plan.install_as(s, no)?;
        plan.set_flags(no, PART_FLAG_ROWMAP | PART_ROWMAP_GRANULES << PART_ROWMAP_FORM_SHIFT | PART_FLAG_COMPACTED);
        plan.set_ranges(no, Vec::new());
        if let Some((_, t)) = &dv {
            plan.set_dv(no, t.dv_gen, t.dv_len, t.dv_crc);
        }
        plan.repoint_member(from, no);
        Ok(())
    };
    publish_with_files(vfs, sink, dir, spec, sealed, &sidecars, &files, PublishKind::Compact, fxid, probe, base_facts, &mut edit).map(|o| (o, no))
}

/// A part's DV payload, checked against its manifest triplet (the reader's `load_part_deletes` law).
/// AUTOVAC FREEZE (spec §13.1 "Freeze"; the delta-v2 port of main's d8502dd97e4): the publisher a frozen generation
/// records, `FrozenTransactionId` at epoch 0. Every clog and snapshot probe calls it committed and visible to every
/// snapshot, so a walk that reaches a frozen generation stops there without consulting pg_xact.
pub const FROZEN_PUBLISHER_FXID: u64 = 2;

/// A publisher or `EXT_UNDER` creator whose status a reader looks up in pg_xact: a normal xid (low 32 bits at
/// `FirstNormalTransactionId` or above). The frozen and bootstrap publishers are committed without a lookup.
pub fn clog_bound(fxid: u64) -> bool {
    fxid & 0xffff_ffff >= 3
}

/// What VACUUM's freeze step found in a table's chain ([`freeze_scan`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreezeScan {
    /// The oldest fxid a reader may look up in pg_xact while the chain stays as it is (`None`: none): the publishers of
    /// the generations a live or later snapshot walks through from `CURRENT` down to the newest generation every
    /// snapshot sees, that one included, and the `EXT_UNDER` creators of the committed ones. VACUUM keeps
    /// `relfrozenxid` at or below it, as heap keeps it at or below its unfrozen xmins.
    pub floor: Option<u64>,
    /// The newest generation every snapshot sees (committed, its publisher frozen or before the cutoff), when no
    /// generation above it is committed or in progress: the base a frozen generation would copy.
    pub base: Option<Manifest>,
    /// A freeze is due: `base` has a clog-bound publisher, or `CURRENT` names a rolled-back generation above it whose
    /// publisher readers look up.
    pub due: bool,
    /// Why no freeze is published now, when one is due.
    pub skip: Option<&'static str>,
}

/// The chain of `dir` against `cutoff_fxid` (the vacuum's horizon, epoch-qualified): see [`FreezeScan`]. `Err` only
/// for an I/O error; a chain that cannot be read is `floor = Some(0)` (the caller keeps `relfrozenxid`).
pub fn freeze_scan(vfs: &mut dyn WriteVfs, dir: &str, probe: &dyn TxnProbe, cutoff_fxid: u64) -> WriteResult<FreezeScan> {
    let unreadable = |why| FreezeScan { floor: Some(0), base: None, due: false, skip: Some(why) };
    let current = full_path(dir, CURRENT_FILE_NAME);
    if !vfs.exists_path(&current)? {
        return Ok(FreezeScan { floor: None, base: None, due: false, skip: None });
    }
    let Some(cp) = vfs.read_full(&current).ok().and_then(|b| CommitPointer::decode(&b).ok()) else { return Ok(unreadable("CURRENT unreadable")) };
    let mut floor: Option<u64> = None;
    fn note(floor: &mut Option<u64>, x: u64) {
        if clog_bound(x) {
            *floor = Some(floor.map_or(x, |f| f.min(x)));
        }
    }
    // why a generation every snapshot sees is not the head a freeze may copy; and a rolled-back one readers probe
    let mut above: Option<&'static str> = None;
    let mut residue = false;
    let mut g = cp.gen;
    while g != 0 {
        let Some(m) = read_manifest(vfs, dir, g) else { return Ok(unreadable("chain unreadable")) };
        let p = m.header.publisher_fxid;
        note(&mut floor, p);
        match probe.verdict(p) {
            TxnVerdict::Committed => {
                let tags = under(&m)?;
                tags.iter().for_each(|e| note(&mut floor, e.fxid));
                if !clog_bound(p) || p < cutoff_fxid {
                    let due = clog_bound(p) || residue;
                    let skip = above.or((!tags.is_empty()).then_some("tagged parts in the head"));
                    return Ok(FreezeScan { floor, base: Some(m), due, skip: skip.filter(|_| due) });
                }
                above.get_or_insert("publisher not below the cutoff");
            }
            TxnVerdict::InProgress => {
                above.get_or_insert("publish in progress");
            }
            TxnVerdict::Aborted => residue |= clog_bound(p),
        }
        g = m.header.prev_gen;
    }
    Ok(FreezeScan { floor, base: None, due: false, skip: None })
}

/// What [`publish_freeze`] did.
#[derive(Debug)]
pub enum FreezeOutcome {
    /// A frozen generation heads the chain now: its publish (its base is the manifest's `prev_gen`).
    Frozen(Box<PublishOutcome>),
    /// Nothing published: why (`None`: nothing needed a freeze).
    Skipped(Option<&'static str>),
}

/// VACUUM's freeze step (spec §13.1 "Freeze"). Heap freezes the xmins before `OldestXmin` before `relfrozenxid` passes
/// them and pg_xact is truncated below it; a generation keeps its publisher's xid, so when the newest generation every
/// snapshot sees has a clog-bound publisher before `cutoff_fxid`, or `CURRENT` names a rolled-back generation above it,
/// this publishes a generation over it with its parts and extension block and `FROZEN_PUBLISHER_FXID` as publisher
/// (kind `Freeze`, through the table's WAL sink; no xid is needed, so the emergency anti-wraparound VACUUM of a
/// cluster in the xid stop zone can run it). Old manifests stay as chain history no walk reaches again.
///
/// Skips (the floor then bounds `relfrozenxid`) when a committed or in-progress generation heads that one (a snapshot
/// may still walk past them), or it carries `EXT_UNDER` tags (the front worker's settle drops them first). The caller
/// holds the table's publish lock. Returns the outcome and the floor after it (`None` once frozen).
#[allow(clippy::too_many_arguments)]
pub fn publish_freeze<'b>(
    vfs: &mut dyn WriteVfs,
    sink: &mut dyn WalSink,
    dir: &str,
    spec: &PartSpec,
    probe: &dyn TxnProbe,
    base_facts: impl Into<BaseStats<'b>>,
    cutoff_fxid: u64,
) -> WriteResult<(FreezeOutcome, Option<u64>)> {
    has_storage(spec)?;
    let scan = freeze_scan(vfs, dir, probe, cutoff_fxid)?;
    let (base_gen, base_fp) = match (&scan.base, scan.due, scan.skip) {
        (Some(b), true, None) => (b.header.gen, b.header.schema_fingerprint),
        (_, _, skip) => return Ok((FreezeOutcome::Skipped(skip), scan.floor)),
    };
    // D2189: a freeze changes only the publisher, so the frozen generation keeps the schema fingerprint of the one
    // it copies, whatever the caller's spec says. A column PostgreSQL drops with no ALTER publish (a dependency's
    // cascade, a temp type's cleanup) leaves that generation under a stamp the live schema no longer has; a publish of
    // the live fingerprint over it was refused ("manifest base identity mismatch": a base of another schema must be
    // described), so VACUUM failed and relfrozenxid could not advance. A writer first publishes the stamp's description
    // (pgrc_am's catch-up); the freeze has no xid to do that.
    let spec = &PartSpec { schema_fingerprint: base_fp, ..*spec };
    let mut edit = |b: Option<&Manifest>, _: &mut PlanBuilder<'_>| match b {
        Some(m) if m.header.gen == base_gen => Ok(()),
        _ => Err(WriteError::Contract { detail: "freeze: the effective generation is not the scanned one" }),
    };
    match publish_with(vfs, sink, dir, spec, &[], &[], PublishKind::Freeze, FROZEN_PUBLISHER_FXID, probe, base_facts.into(), &mut edit) {
        Ok(o) => Ok((FreezeOutcome::Frozen(Box::new(o)), None)),
        Err(WriteError::PublishBusy { .. }) => Ok((FreezeOutcome::Skipped(Some("publish in progress")), scan.floor)),
        Err(e) => Err(e),
    }
}

fn read_dv(vfs: &mut dyn WriteVfs, dir: &str, rec: &PartRecord) -> WriteResult<Vec<u8>> {
    let bytes = vfs.read_full(&full_path(dir, &sidecar_file_name(rec.part_no, SidecarKind::Dv, rec.dv_gen)))?;
    let (h, payload) = SidecarFileHeader::validate_file(&bytes)?;
    if h.kind != SidecarKind::Dv.as_u16() || payload.len() as u64 != rec.dv_len || crc32c(payload) != rec.dv_crc {
        return Err(WriteError::Format(FormatError::Corrupt { at: "part DV vs its manifest triplet at compile" }));
    }
    Ok(pgrc_format::try_to_vec(payload, "publish")?)
}

pub type Edit<'e> = &'e mut dyn FnMut(Option<&Manifest>, &mut PlanBuilder<'_>) -> WriteResult<()>;

/// The base generation and its stats. With the catalog's generation, `CURRENT` must still name it (a 32-byte read and
/// the manifest's crc); otherwise the effective manifest is read and its stats loaded from the directory.
fn resolve_base<'a>(vfs: &mut dyn WriteVfs, dir: &str, fxid: u64, probe: &dyn TxnProbe, given: &BaseStats<'a>) -> WriteResult<(Option<Cow<'a, Manifest>>, BaseState, bool, Vec<UnderEntry>)> {
    if let BaseStats::Gen(g) = given {
        if given_is_base(vfs, dir, fxid, probe, g) {
            let state = match (g.stats.root, g.stats.runs.is_empty(), g.stats.loose.len() >= LOOSE_MAX) {
                (None, true, true) => {
                    let parts: Vec<PartStats> = g.stats.loose.iter().map(|p| (**p).clone()).collect();
                    BaseState::Legacy(stats::legacy_run_from_parts(&parts)?)
                }
                _ => BaseState::Runs(g.stats.clone()),
            };
            // the catalog's manifest by reference (P-1 L4a: no copy of the part list per publish)
            return Ok((Some(Cow::Borrowed(g.manifest)), state, true, Vec::new()));
        }
    }
    let Some((base, run)) = effective_base(vfs, dir, probe, Some(fxid))? else { return Ok((None, BaseState::Runs(StatsState::default()), false, Vec::new())) };
    let state = load_base_stats(vfs, dir, &base, given)?;
    Ok((Some(Cow::Owned(base)), state, false, run))
}

/// The catalog's generation is the publish's base: `CURRENT` still names it and its publisher is usable.
fn given_is_base(vfs: &mut dyn WriteVfs, dir: &str, fxid: u64, probe: &dyn TxnProbe, g: &BaseGen<'_>) -> bool {
    let names_it = vfs
        .read_full(&full_path(dir, CURRENT_FILE_NAME))
        .ok()
        .and_then(|b| CommitPointer::decode(&b).ok())
        // the manifest it names (a decoded pointer carries its own crc, a computed one 0)
        .is_some_and(|cp| (cp.gen, cp.manifest_len, cp.manifest_crc) == (g.pointer.gen, g.pointer.manifest_len, g.pointer.manifest_crc));
    let usable = match probe.verdict(g.manifest.header.publisher_fxid) {
        TxnVerdict::Committed => true,
        // PUBLISH-WAIT: this transaction tree's own generation (a savepoint's too); a foreign in-progress head
        // falls through to effective_manifest, which returns PublishBusy
        TxnVerdict::InProgress => probe.is_own(g.manifest.header.publisher_fxid, fxid),
        TxnVerdict::Aborted => false,
    };
    names_it && usable
}

/// The base manifest a publisher reads before its plan (a compile's DV folds): the catalog's by reference when it is
/// the base ([`given_is_base`]; P-1 L4a, no manifest read per compile), else the effective manifest from the directory.
fn plan_base<'a>(vfs: &mut dyn WriteVfs, dir: &str, fxid: u64, probe: &dyn TxnProbe, given: &BaseStats<'a>) -> WriteResult<Option<Cow<'a, Manifest>>> {
    if let BaseStats::Gen(g) = given {
        if given_is_base(vfs, dir, fxid, probe, g) {
            return Ok(Some(Cow::Borrowed(g.manifest)));
        }
    }
    Ok(effective_manifest(vfs, dir, probe, Some(fxid))?.map(Cow::Owned))
}

/// A base generation's stats before the plan.
enum BaseState {
    Runs(StatsState),
    /// Root-less with `LOOSE_MAX` parts or more (OC-5): one legacy run of all its parts, written by this publish.
    Legacy(stats::Run),
}

/// A part's stats from its file, opened through `vfs` against its manifest record.
pub fn part_stats_on_disk(vfs: &mut dyn WriteVfs, dir: &str, m: &Manifest, rec: &PartRecord) -> WriteResult<PartStats> {
    let io = vfs.part_io(&full_path(dir, &part_file_name(rec.part_no)))?;
    let fp = pgrc_format::manifest::part_fingerprint(m, rec.part_no)?;
    let expect = pgrc_read::PartExpect::from_manifest(rec, fp, m.header.relfilenumber);
    let part = pgrc_read::OpenPart::open(io, &expect).map_err(stats::read_error)?;
    stats::part_stats_from_open(&part, rec).map_err(stats::read_error)
}

fn load_base_stats(vfs: &mut dyn WriteVfs, dir: &str, base: &Manifest, given: &BaseStats<'_>) -> WriteResult<BaseState> {
    if stats_ref(base)?.is_some() {
        let loaded = StatsState::load(vfs, dir, base, None, &mut |v, rec| part_stats_on_disk(v, dir, base, rec));
        match loaded {
            Ok(s) => return Ok(BaseState::Runs(s)),
            // a root that does not describe its generation is never trusted: the parts are walked (below)
            Err(WriteError::Io { .. }) | Err(WriteError::Format(_)) | Err(WriteError::Contract { .. }) => {}
            Err(e) => return Err(e),
        }
    } else if base.parts.len() < LOOSE_MAX {
        let loose = base.parts.iter().map(|rec| part_stats_on_disk(vfs, dir, base, rec).map(std::sync::Arc::new)).collect::<WriteResult<Vec<_>>>()?;
        return Ok(BaseState::Runs(StatsState { root: None, runs: Vec::new(), loose }));
    } else if let BaseStats::Folded(Some(img), Some(ans)) = given {
        if let Ok(run) = stats::legacy_run(base, img, ans) {
            return Ok(BaseState::Legacy(run));
        }
    }
    let parts = base.parts.iter().map(|rec| part_stats_on_disk(vfs, dir, base, rec)).collect::<WriteResult<Vec<_>>>()?;
    if parts.len() < LOOSE_MAX {
        return Ok(BaseState::Runs(StatsState { root: None, runs: Vec::new(), loose: parts.into_iter().map(std::sync::Arc::new).collect() }));
    }
    Ok(BaseState::Legacy(stats::legacy_run_from_parts(&parts)?))
}

#[allow(clippy::too_many_arguments)]
/// K1 L6: process-wide counters by publish kind (D2's part bytes by kind; P-1's evidence: what a publish costs at a
/// part count). Per kind: publishes, part bytes installed, sidecar bytes staged, the bytes its own writes logged
/// (stats runs, bankstats plane, stats and other sidecars; `FILE_WRITE` payloads when the sink logs), the `PUBLISH`
/// record's bytes, its `EXT_RANGES` bytes (the `births` column), the nanoseconds in [`publish_with`] (fold, stage, log, apply), the part
/// rows installed, of the logged bytes the whole-table metadata (the bankstats plane, written over an empty base only;
/// before P-1 L2 also the sections file: what grew with the part count), and (P-1) of the logged bytes the stats runs
/// this publish wrote (bounded by the merge policy, 0 for a publish that changes no run). A publish whose transaction
/// later aborts is counted.
pub mod counters {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    use pgrc_format::wal::PublishKind;

    pub const FIELDS: [&str; 10] = ["n", "part", "sidecar", "logged", "rec", "births", "us", "rows", "meta", "runs"];
    pub const KINDS: [(PublishKind, &str); 6] = [(PublishKind::Bulk, "bulk"), (PublishKind::Rotate, "rotate"), (PublishKind::Seal, "seal"), (PublishKind::Compile, "compile"), (PublishKind::Compact, "compact"), (PublishKind::Freeze, "freeze")];

    pgsync::process_global! {
        static BY_KIND: [[AtomicU64; 10]; 7] = [const { [const { AtomicU64::new(0) }; 10] }; 7];
    }

    pub(crate) fn add(kind: PublishKind, v: [u64; 10]) {
        for (c, x) in BY_KIND[kind as usize].iter().zip(v) {
            c.fetch_add(x, Relaxed);
        }
    }

    /// `kind`'s counters, in [`FIELDS`] order (the time in microseconds).
    pub fn get(kind: PublishKind) -> [u64; 10] {
        let mut v: [u64; 10] = std::array::from_fn(|i| BY_KIND[kind as usize][i].load(Relaxed));
        v[6] /= 1000;
        v
    }

    /// `pub.<kind>=<n>,<part>,<sidecar>,<logged>,<rec>,<births>,<us>,<rows>,<meta>,<runs>` for every kind, space-separated.
    pub fn line() -> String {
        KINDS.map(|(k, name)| format!("pub.{name}={}", get(k).map(|x| x.to_string()).join(","))).join(" ")
    }
}

/// What a publish changes against its base, by one merge join of the two part lists (both `part_no` ascending): the
/// base parts it retires (gone, or their ident changed) and the positions in `new` of the parts it adds (new, or their
/// ident changed). O(base + new) (P-1 L4a; two maps of every part were built per publish).
pub(crate) fn part_changes(base: &[PartRecord], new: &[PartRecord]) -> (Vec<u32>, Vec<usize>) {
    let (mut retired, mut added) = (Vec::new(), Vec::new());
    let (mut i, mut j) = (0, 0);
    loop {
        match (base.get(i), new.get(j)) {
            (Some(b), Some(n)) if b.part_no == n.part_no => {
                if ident_of(b) != ident_of(n) {
                    retired.push(b.part_no);
                    added.push(j);
                }
                i += 1;
                j += 1;
            }
            (Some(b), Some(n)) if b.part_no < n.part_no => {
                retired.push(b.part_no);
                i += 1;
            }
            (Some(b), None) => {
                retired.push(b.part_no);
                i += 1;
            }
            (_, Some(_)) => {
                added.push(j);
                j += 1;
            }
            (None, None) => return (retired, added),
        }
    }
}

/// The refusal of a publish on relfilenumber 0 ([`has_storage`]).
pub const NO_STORAGE: &str = "a publish on relfilenumber 0 (a relation without storage, as a partitioned table is) is refused before any directory scan";

/// A relation without storage has relfilenumber 0 (`InvalidRelFileNumber`) and no table directory: a partitioned
/// table's relam only names its partitions' default. Its publish fails closed here, before the base walk lists
/// `pgrc_0` under the zero locator's path (which failed as an I/O error, 58030, or would read whatever is there).
/// [`publish_with_files`] (every publish ends there) checks it; the publishes that read the directory before it (a
/// compile, a compaction, the freeze) check it first.
fn has_storage(spec: &PartSpec) -> WriteResult<()> {
    match spec.relfilenumber {
        0 => Err(WriteError::Contract { detail: NO_STORAGE }),
        _ => Ok(()),
    }
}

#[allow(clippy::too_many_arguments)]
fn publish_with(
    vfs: &mut dyn WriteVfs,
    sink: &mut dyn WalSink,
    dir: &str,
    spec: &PartSpec,
    sealed: &[SealedPart],
    sidecars: &[(u32, SidecarKind, u64, &[u8])],
    kind: PublishKind,
    fxid: u64,
    probe: &dyn TxnProbe,
    given: BaseStats<'_>,
    edit: Edit<'_>,
) -> WriteResult<PublishOutcome> {
    publish_with_files(vfs, sink, dir, spec, sealed, sidecars, &[], kind, fxid, probe, given, edit)
}

/// [`publish_with`] that also writes `files` (`(name, bytes)`; SORTRUNS-E0: a run's reverse) as temps renamed to their
/// names by the publish, as its sidecars are (`FILE_WRITE` logged, renamed by redo, a dead publish's unlinked).
#[allow(clippy::too_many_arguments)]
fn publish_with_files(
    vfs: &mut dyn WriteVfs,
    sink: &mut dyn WalSink,
    dir: &str,
    spec: &PartSpec,
    sealed: &[SealedPart],
    sidecars: &[(u32, SidecarKind, u64, &[u8])],
    files: &[(String, &[u8])],
    kind: PublishKind,
    fxid: u64,
    probe: &dyn TxnProbe,
    given: BaseStats<'_>,
    edit: Edit<'_>,
) -> WriteResult<PublishOutcome> {
    has_storage(spec)?;
    let t0 = std::time::Instant::now();
    let tb = pg_clock::MonoStamp::now();
    let (base, base_state, base_given, run) = resolve_base(vfs, dir, fxid, probe, &given)?;
    let base_ns = tb.elapsed_ns();
    // SEAL-OFFHEAD: the tags this generation carries and the parts of rolled-back creators it drops
    let (keep, dead) = under_plan(base.as_deref(), run, probe, fxid)?;
    let mut b = PublishPlan::build(base.as_deref(), spec, fxid, kind);
    b.number_above(probe.used_numbers(dir));
    edit(base.as_deref(), &mut b)?;
    if keep.iter().map(|e| e.part_no).chain(dead.iter().copied()).any(|n| b.touches(n)) {
        return Err(WriteError::Contract { detail: "a publish edits a part another transaction's uncommitted generation added" });
    }
    dead.iter().for_each(|&n| b.retire(n));
    b.set_under(&keep)?;
    let mut plan = b.finish()?;
    let gen = plan.manifest.header.gen;
    let part_nos = plan.part_nos();

    // the stats plan: the parts this publish adds (from the seal, or from disk) and the ones it drops
    let ts = pg_clock::MonoStamp::now();
    let (retired, added) = part_changes(base.as_deref().map_or(&[][..], |m| &m.parts[..]), &plan.manifest.parts);
    let mut installs: Vec<PartStats> = Vec::new();
    for rec in added.iter().map(|&k| &plan.manifest.parts[k]) {
        let from_seal = part_nos.iter().position(|&n| n == rec.part_no).map(|i| &sealed[i]);
        installs.push(match from_seal.map(|s| stats::part_stats_from_sealed(s, rec.part_no)) {
            Some(Ok(ps)) => ps,
            Some(Err(_)) => {
                let s = from_seal.expect("sealed");
                let io = vfs.part_io(&full_path(dir, &s.tmp_name))?;
                let part = pgrc_read::OpenPart::open(io, &pgrc_read::PartExpect::none()).map_err(stats::read_error)?;
                stats::part_stats_from_open(&part, rec).map_err(stats::read_error)?
            }
            None => part_stats_on_disk(vfs, dir, &plan.manifest, rec)?,
        });
    }
    let fp = spec.schema_fingerprint;
    // hold (P-1 L2, K1's install): a run holding a part below the bulk cut stays out of big-run merges, up to
    // HELD_SMALL_MAX such runs (L2b). The full test is the catalog's; without it, one base part is opened, and only when
    // this publish could merge.
    let given_full = match given {
        BaseStats::Gen(g) => g.full,
        _ => None,
    };
    let (small_runs, loose) = match &base_state {
        BaseState::Runs(s) => (s.runs.iter().filter(|r| r.parts.len() <= LOOSE_MAX).count(), s.loose.len()),
        BaseState::Legacy(_) => (0, 0),
    };
    let may_merge = small_runs + (loose + installs.len()) / LOOSE_MAX + retired.len() >= SMALL_RUNS_MAX;
    let owned_full = match (given_full, may_merge, base.as_deref().and_then(|m| m.parts.first().map(|rec| (m, rec)))) {
        (None, true, Some((m, rec))) => {
            let io = vfs.part_io(&full_path(dir, &part_file_name(rec.part_no)))?;
            let pfp = pgrc_format::manifest::part_fingerprint(m, rec.part_no)?;
            let part = pgrc_read::OpenPart::open(io, &pgrc_read::PartExpect::from_manifest(rec, pfp, m.header.relfilenumber)).map_err(stats::read_error)?;
            Some(FullTest::from_part(&part).map_err(stats::read_error)?)
        }
        _ => None,
    };
    let full = given_full.or(owned_full.as_ref());
    let hold = |r: &Run| full.is_some_and(|t| t.run_holds(r));
    let base_manifest = base.as_deref();
    let mut refold = |idents: &[PartIdent]| -> WriteResult<Vec<PartStats>> {
        let m = base_manifest.ok_or(WriteError::Contract { detail: "stats: a refold without a base" })?;
        idents
            .iter()
            .map(|id| {
                let i = m.parts.binary_search_by_key(&id.part_no, |p| p.part_no).map_err(|_| WriteError::Contract { detail: "stats: a refold of a part not in the base" })?;
                part_stats_on_disk(vfs, dir, m, &m.parts[i])
            })
            .collect()
    };
    let legacy_parts = match &base_state {
        BaseState::Legacy(run) => run.parts.len() as u32,
        BaseState::Runs(_) => 0,
    };
    let stats_plan = match base_state {
        BaseState::Runs(s) => stats::plan_stats(&s, gen, fxid, fp, installs, &retired, &mut refold, &hold)?,
        BaseState::Legacy(run) => stats::plan_stats_legacy(run, gen, fxid, fp, installs, &retired, &mut refold, &hold)?,
    };
    let stats_us = (base_ns + ts.elapsed_ns()) / 1000;
    if stats_plan.root_changed {
        let ext = with_stats_ref(&plan.manifest.ext, stats_plan.state.root.as_ref())?;
        plan.set_ext(ext);
    }
    stats_plan.state.check(&plan.manifest).map_err(|e| match e { stats::OrderError::Mismatch(detail) => WriteError::Contract { detail }, stats::OrderError::Alloc(e) => e.into() })?;

    let mut lvfs = LoggedVfs::new(vfs, sink, dir);
    for (rref, bytes) in &stats_plan.stage {
        let tmp = run_tmp_file_name(rref.gen, rref.seq);
        let fd = lvfs.create_rw(&full_path(dir, &tmp))?;
        lvfs.pwrite_at(&fd, 0, bytes)?;
        lvfs.close_file(fd)?;
        plan.rename(tmp, run_file_name(rref.gen, rref.seq));
    }
    let run_bytes = lvfs.written();
    let (bankstats_plane, plane) = crate::bankplane::stage_plane(&mut lvfs, dir, base.as_deref(), &plan.manifest, sealed);
    let meta = lvfs.written();
    if let Some((tmp, name)) = plane {
        plan.rename(tmp, name);
    }
    let pairs: Vec<(u32, &[u8])> = part_nos.iter().copied().zip(sealed.iter().map(|s| s.stats_payload.as_slice())).collect();
    for (tmp, name) in crate::sidecar::stage_stats_sidecars(&mut lvfs, dir, spec.schema_fingerprint, &pairs, fxid)? {
        plan.rename(tmp, name);
    }
    for &(part_no, kind, sgen, payload) in sidecars {
        let (tmp, name) = crate::sidecar::stage_sidecar_gen(&mut lvfs, dir, kind, spec.schema_fingerprint, part_no, sgen, payload, fxid)?;
        plan.rename(tmp, name);
    }
    for (k, (name, bytes)) in files.iter().enumerate() {
        let tmp = pgrc_format::dirlayout::temp_file_name(fxid, FILE_TMP_SEQ | k as u32);
        let fd = lvfs.create_rw(&full_path(dir, &tmp))?;
        lvfs.pwrite_at(&fd, 0, bytes)?;
        lvfs.close_file(fd)?;
        plan.rename(tmp, name.clone());
    }
    let mut sidecar_bytes: u64 = sidecars.iter().map(|s| s.3.len() as u64).chain(files.iter().map(|f| f.1.len() as u64)).sum();
    if kind == PublishKind::Bulk {
        for (&part_no, s) in part_nos.iter().zip(sealed) {
            if let Some(map) = bulk_runs(s) {
                let (tmp, name) = crate::sidecar::stage_sidecar_gen(&mut lvfs, dir, SidecarKind::RowIdentMap, spec.schema_fingerprint, part_no, 1, &map, fxid)?;
                plan.rename(tmp, name);
                sidecar_bytes += map.len() as u64;
            }
        }
    }

    let logged = lvfs.written();
    let (vfs, sink) = lvfs.into_parts();
    let (rec, manifest, bytes) = plan.into_parts();
    let pointer = CommitPointer::new(gen, bytes.len() as u64, rec.manifest_crc);
    let base = base.as_deref();
    sink.publish(&rec, &mut || apply_planned(&mut *vfs, dir, base, &rec, &bytes))?;
    let mut enc = Vec::new();
    let rec_bytes = rec.encode(&mut enc).map_or(0, |()| enc.len() as u64);
    // the `births` column (its name kept for the scripts that read the counters by position): the row-ID directory's
    // bytes, `EXT_RANGES` (IX0; K1's `EXT_BIRTHS` before it)
    let births_bytes = pgrc_format::manifest::ext_records(&rec.ext).map_or(0, |r| r.iter().filter(|x| x.0 == pgrc_format::manifest::EXT_RANGES).map(|x| x.1.len() as u64).sum());
    let part_bytes = sealed.iter().map(|s| s.file_len).sum();
    counters::add(kind, [1, part_bytes, sidecar_bytes, logged, rec_bytes, births_bytes, t0.elapsed().as_nanos() as u64, sealed.iter().map(|s| s.rows).sum(), meta - run_bytes, run_bytes]);
    let runs_written = stats_plan.stage.len() as u32;
    let mut stacked_over: Vec<u64> = keep.iter().map(|e| e.fxid).filter(|&x| !probe.is_own(x, fxid)).collect();
    stacked_over.sort_unstable();
    stacked_over.dedup();
    Ok(PublishOutcome { gen, kind, part_nos, bankstats_plane, manifest, pointer, stats: stats_plan.state, runs_written, run_bytes, stats_us, base_given, legacy_parts, stacked_over, dropped: dead })
}

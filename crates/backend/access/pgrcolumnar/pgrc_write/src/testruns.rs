//! SORTRUNS-E0 L1 (pgrc-plans/SORTRUNS-E0-STEP1.md; charter L1 "tests on runs built by a test-only writer"): sorted
//! runs that home their rows' IDs, written before the run merge exists (L2), so every by-ID path, the reverse's file
//! lifecycle (WAL, standby, base backup, dirhash, reclaim) and the format legs meet run-homed rows.
//!
//! The gates' knob `PGRUST_PGRC_TEST_SELF_RUNS` (never set in production) makes every compaction output a one-member
//! sorted run ([`crate::publish::publish_compact`]):
//! - `1` or `none`: a self-homed run (form NONE): the member keeps its `EXT_RANGES` entry; nothing reads the run (L0).
//! - `frameix` / `labels`: a run that homes the output's ranges: the member has no `EXT_RANGES` entry, its map is a run
//!   member's (`pgrc_format::rowmap::encode_run_member`: small frames for FRAMEIX, large frames with fence minimums
//!   for LABELS, four frames a group), and the run's reverse (`runrev-<run_id>.pgrcv`, [`build`]) is written and
//!   renamed by the publish like a sidecar. A later compaction of the member empties the run, which drops it; the next
//!   reclaim unlinks the reverse.

use pgrc_format::manifest::IdRanges;
use pgrc_format::rowmap::{encode_run_member, GranMap, RUN_FRAME_LARGE, RUN_FRAME_SMALL};
use pgrc_format::runrev::{encode, Spec};
use pgrc_format::sortrun::{RankForm, RunRevForm, SortRun};

use crate::{WriteError, WriteResult};

/// What the knob asks of a compaction's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Knob {
    /// No run (production).
    Off,
    /// A self-homed one-member run.
    SelfHomed,
    /// A one-member run that homes the output's IDs, with this reverse form.
    Homes(RunRevForm),
}

/// The knob, read once per process.
pub fn knob() -> Knob {
    static K: pgsync::OnceLock<Knob> = pgsync::OnceLock::new();
    *K.get_or_init(|| match std::env::var("PGRUST_PGRC_TEST_SELF_RUNS").as_deref() {
        Ok("1") | Ok("none") => Knob::SelfHomed,
        Ok("frameix") => Knob::Homes(RunRevForm::FrameIx),
        Ok("labels") => Knob::Homes(RunRevForm::Labels),
        _ => Knob::Off,
    })
}

/// LABELS: the member's large frames a group spans.
pub const GROUP_FRAMES: u32 = 4;

/// A one-member run that homes its IDs: the run's `EXT_RUNS` entry, the member's map (its sidecar's payload) and the
/// reverse file.
#[derive(Debug, Clone)]
pub struct HomeRun {
    pub run: SortRun,
    pub member_map: Vec<u8>,
    pub file: Vec<u8>,
}

fn contract(detail: &'static str) -> WriteError {
    WriteError::Contract { detail }
}

/// The run `run_id` of one member, part `part_no`, whose rows hold `ids` in ordinal order and whose ranges `ranges`
/// (sorted, disjoint) hold every one of them: the member's map with `form`'s virtual frames, and the reverse ranking
/// the IDs `rank`-wise (`None`: DENSE when the ranges span at most twice the rows, else SET). An ID of the ranges no
/// row holds is absent (DENSE: the label that names no group or frame).
pub fn build(form: RunRevForm, rank: Option<RankForm>, run_id: u64, part_no: u32, ids: &[u64], ranges: &IdRanges) -> WriteResult<HomeRun> {
    let rows = ids.len();
    let span: u64 = ranges.iter().map(|&(lo, hi)| hi - lo).sum();
    if rows == 0 || form == RunRevForm::None {
        return Err(contract("test run: no rows, or a form without a reverse"));
    }
    let rank = rank.unwrap_or(if span <= 2 * rows as u64 { RankForm::Dense } else { RankForm::Set });
    let frame = if form == RunRevForm::FrameIx { RUN_FRAME_SMALL } else { RUN_FRAME_LARGE } as usize;
    let vstarts: Vec<u32> = (0..rows).step_by(frame).map(|o| o as u32).collect();
    let nv = vstarts.len() as u32;
    let vmin: Vec<u64> = vstarts.iter().map(|&s| ids[s as usize..(s as usize + frame).min(rows)].iter().copied().min().expect("a frame holds a row")).collect();
    let member_map = encode_run_member(ids, &vstarts, (form == RunRevForm::Labels).then_some(&vmin[..]))?;
    let member = GranMap::decode(&member_map, rows as u64)?;
    // the IDs in rank order with their ordinals; the ranks
    let mut by_id: Vec<(u64, u32)> = ids.iter().enumerate().map(|(o, &id)| (id, o as u32)).collect();
    by_id.sort_unstable();
    let dense_rank = |id: u64| -> Option<u64> {
        let mut r0 = 0;
        for &(lo, hi) in ranges {
            if (lo..hi).contains(&id) {
                return Some(r0 + id - lo);
            }
            r0 += hi - lo;
        }
        None
    };
    let (ngroups, nsym) = match form {
        RunRevForm::FrameIx => (0, nv),
        _ => {
            let g = nv.div_ceil(GROUP_FRAMES);
            (g, g)
        }
    };
    // each row's label: FRAMEIX its virtual frame; LABELS its frame's group, or nsym (an exception) where the
    // member's fast path misses it
    let mut exceptions: Vec<(u64, u16, u32)> = Vec::new();
    let mut label_of = |id: u64, o: u32| -> WriteResult<u32> {
        let v = o / frame as u32;
        if form == RunRevForm::FrameIx {
            return Ok(v);
        }
        let g = v / GROUP_FRAMES;
        let (va, vb) = (g * GROUP_FRAMES, ((g + 1) * GROUP_FRAMES).min(nv));
        if member.run_find(va, vb, id)? == Some(o) {
            Ok(g)
        } else {
            exceptions.push((id, 0, v));
            Ok(nsym)
        }
    };
    let (ranks, s_runs, labels) = match rank {
        RankForm::Dense => {
            let mut labels = pgrc_format::try_filled(span as usize, nsym, "testruns")?;
            for &(id, o) in &by_id {
                let r = dense_rank(id).ok_or(contract("test run: an ID outside the run's ranges"))?;
                labels[r as usize] = label_of(id, o)?;
            }
            (span, Vec::new(), labels)
        }
        RankForm::Set => {
            let mut s_runs: Vec<(u64, u64)> = Vec::new();
            let mut labels = pgrc_format::try_vec(rows, "testruns")?;
            for &(id, o) in &by_id {
                if dense_rank(id).is_none() {
                    return Err(contract("test run: an ID outside the run's ranges"));
                }
                match s_runs.last_mut() {
                    Some((lo, len)) if *lo + *len == id => *len += 1,
                    _ => s_runs.push((id, 1)),
                }
                labels.push(label_of(id, o)?);
            }
            (rows as u64, s_runs, labels)
        }
    };
    let groups: Vec<(u16, u32, u32)> = (0..ngroups).map(|g| (0, g * GROUP_FRAMES, ((g + 1) * GROUP_FRAMES).min(nv))).collect();
    let prefix: Vec<u32> = if form == RunRevForm::FrameIx { vec![0, nv] } else { Vec::new() };
    let spec = Spec { run_id, form, rank, ranks, rows: rows as u64, nsym, members: 1 };
    let (file, fin) = encode(spec, &s_runs, &labels, &groups, &exceptions, &prefix)?;
    let run = SortRun { run_id, level: 1, rev: form, rank, rows: rows as u64, rev_len: fin.rev_len, rev_crc: fin.rev_crc, members: vec![part_no], ranges: ranges.clone() };
    Ok(HomeRun { run, member_map, file })
}

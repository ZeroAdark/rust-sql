//! Each row's (rank, frame) goes to a bucket of ranks through the caller's [`Spill`], so the build's memory does not grow with the run.
//! A LABELS group is a maximal span of one member's frames whose ID fences ascend apart, so the search finds every row of it and the reverse lists no exception.

use pgrc_format::manifest::IdRanges;
use pgrc_format::rowid::ROWID_LIMIT;
use pgrc_format::runrev::{form_pages, Finished, Spec, Writer, PAGE};
use pgrc_format::sortrun::{RankForm, RunRevForm, MAX_MEMBERS};
use pgrc_format::FormatError;

/// At most `max_rows`, at most `max_bytes` at the inputs' average bytes per row (`input_bytes` over `input_rows`), and at least one.
pub fn cut_rows(max_rows: u64, max_bytes: u64, input_rows: u64, input_bytes: u64) -> u64 {
    let by_bytes = match (input_rows, input_bytes) {
        (0, _) | (_, 0) => u64::MAX,
        (r, b) => (u128::from(max_bytes) * u128::from(r) / u128::from(b)).min(u128::from(u64::MAX)) as u64,
    };
    max_rows.min(by_bytes).max(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunPolicy {
    pub fanin: usize,
    pub cap_rows: u64,
    pub cap_bytes: u64,
}

impl Default for RunPolicy {
    fn default() -> RunPolicy {
        RunPolicy { fanin: 16, cap_rows: 1 << 30, cap_bytes: 16 << 30 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunUnit {
    pub level: u8,
    /// Age order within a level (a loose part's number; a run's ID): the oldest merge first.
    pub order: u64,
    pub rows: u64,
    pub bytes: u64,
    pub parts: Vec<u32>,
}

/// A level where no window fits is at the cap: its units stay, and the next level is tried.
/// `None` when no level has a job (or `fanin` is below 2: one unit is already a run).
pub fn next_run_job(units: &[RunUnit], p: &RunPolicy) -> Option<Vec<u32>> {
    if p.fanin < 2 {
        return None;
    }
    let mut levels: Vec<u8> = units.iter().map(|u| u.level).collect();
    levels.sort_unstable();
    levels.dedup();
    for l in levels {
        let mut at: Vec<&RunUnit> = units.iter().filter(|u| u.level == l).collect();
        if at.len() < p.fanin {
            continue;
        }
        at.sort_by_key(|u| u.order);
        for w in at.windows(p.fanin) {
            let (rows, bytes) = w.iter().fold((0u64, 0u64), |(r, b), u| (r.saturating_add(u.rows), b.saturating_add(u.bytes)));
            if rows <= p.cap_rows && bytes <= p.cap_bytes {
                let mut parts: Vec<u32> = w.iter().flat_map(|u| u.parts.iter().copied()).collect();
                parts.sort_unstable();
                return Some(parts);
            }
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberJob {
    Retire(Vec<u32>),
    /// Rewritten in place: its slot is repointed and its virtual frames kept, so the run's reverse stays valid.
    Swap(u32),
}

/// Dead members retire together, at most `p.max_inputs`; else the member with the largest deleted share at or above `p.dv_rewrite` (the lowest part number of equals) is rewritten alone.
pub fn next_member_job(members: &[crate::policy::PartView], p: &crate::policy::Policy) -> Option<MemberJob> {
    let dead: Vec<u32> = members.iter().filter(|m| m.live == 0).take(p.max_inputs).map(|m| m.part_no).collect();
    if !dead.is_empty() {
        return Some(MemberJob::Retire(dead));
    }
    let share = |m: &crate::policy::PartView| if m.rows == 0 { 0.0 } else { (m.rows - m.live.min(m.rows)) as f64 / m.rows as f64 };
    members.iter().filter(|m| share(m) >= p.dv_rewrite).max_by(|a, b| share(a).total_cmp(&share(b)).then(b.part_no.cmp(&a.part_no))).map(|m| MemberJob::Swap(m.part_no))
}

/// Each virtual frame's first row among the survivors `kept` (ascending): an emptied frame repeats the next one's;
/// trailing emptied frames start at the survivors' count.
pub fn kept_vstarts(vstarts: &[u32], kept: &[u32]) -> Vec<u32> {
    vstarts.iter().map(|&s| kept.partition_point(|&o| o < s) as u32).collect()
}

pub const FRAME: u32 = pgrc_format::rowmap::RUN_FRAME_SMALL;
/// Ranks per spill bucket (a DENSE bucket's labels are one array at write: 16 MiB).
pub const BUCKET_RANKS: u64 = 1 << 22;
pub const MAX_BUCKETS: u64 = 256;
const STAGE: usize = 64 << 10;

/// Where the builder's (rank, frame) entries go by bucket, and come back from: a whole bucket, in append order.
pub trait Spill {
    type Error;
    fn append(&mut self, bucket: usize, bytes: &[u8]) -> Result<(), Self::Error>;
    fn read(&mut self, bucket: usize, out: &mut Vec<u8>) -> Result<(), Self::Error>;
    fn format(e: FormatError) -> Self::Error;
}

#[derive(Debug, Default)]
pub struct MemSpill {
    pub buckets: Vec<Vec<u8>>,
}

impl Spill for MemSpill {
    type Error = FormatError;

    fn append(&mut self, bucket: usize, bytes: &[u8]) -> Result<(), FormatError> {
        if self.buckets.len() <= bucket {
            pgrc_format::try_resize(&mut self.buckets, bucket + 1, Vec::new(), "pgrc run spill")?;
        }
        self.buckets[bucket].extend_from_slice(bytes);
        Ok(())
    }

    fn read(&mut self, bucket: usize, out: &mut Vec<u8>) -> Result<(), FormatError> {
        out.clear();
        if let Some(b) = self.buckets.get(bucket) {
            out.extend_from_slice(b);
        }
        Ok(())
    }

    fn format(e: FormatError) -> FormatError {
        e
    }
}

fn contract(detail: &'static str) -> FormatError {
    FormatError::EncodeContract { detail }
}

fn varint(out: &mut Vec<u8>, mut x: u64) {
    while x >= 0x80 {
        out.push(x as u8 | 0x80);
        x >>= 7;
    }
    out.push(x as u8);
}

fn read_varint(b: &[u8], at: &mut usize) -> Option<u64> {
    let mut x = 0u64;
    for sh in (0..64).step_by(7) {
        let c = *b.get(*at)?;
        *at += 1;
        x |= u64::from(c & 0x7f) << sh;
        if c < 0x80 {
            return Some(x);
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberFrames {
    pub vstarts: Vec<u32>,
    pub vmin: Vec<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Force {
    pub form: Option<RunRevForm>,
    pub rank: Option<RankForm>,
}

/// A run's reverse while its members are written ([`RevBuilder::member`] per member, in slot order).
#[derive(Debug)]
pub struct RevBuilder<S: Spill> {
    run_id: u64,
    ranges: IdRanges,
    base: Vec<u64>,
    span: u64,
    bucket: u64,
    spill: S,
    stage: Vec<Vec<u8>>,
    last: Vec<(u64, u64)>,
    count: Vec<u64>,
    hint: usize,
    rows: u64,
    frames: u64,
    prefix: Vec<u32>,
    /// Per LABELS group, its first frame (run-global); a member's first frame always starts one.
    group_starts: Vec<u32>,
    force: Force,
    spilled: u64,
}

impl<S: Spill> RevBuilder<S> {
    /// The reverse of run `run_id`, which homes `ranges` (sorted, disjoint, non-empty, below `ROWID_LIMIT`).
    pub fn new(run_id: u64, ranges: IdRanges, bucket_ranks: u64, spill: S, force: Force) -> Result<RevBuilder<S>, S::Error> {
        let ok = run_id != 0 && !ranges.is_empty() && ranges.iter().all(|&(lo, hi)| lo < hi && hi <= ROWID_LIMIT) && ranges.windows(2).all(|w| w[0].1 <= w[1].0);
        if !ok {
            return Err(S::format(contract("run reverse build: the run's ranges")));
        }
        let mut base = pgrc_format::try_vec(ranges.len(), "pgrc run reverse").map_err(S::format)?;
        let mut span = 0u64;
        for &(lo, hi) in &ranges {
            base.push(span);
            span += hi - lo;
        }
        let want = if bucket_ranks == 0 { BUCKET_RANKS } else { bucket_ranks };
        let bucket = want.max(span.div_ceil(MAX_BUCKETS)).max(1);
        let nb = span.div_ceil(bucket) as usize;
        let stage = pgrc_format::try_filled(nb, Vec::new(), "pgrc run reverse").map_err(S::format)?;
        let last = pgrc_format::try_filled(nb, (0, 0), "pgrc run reverse").map_err(S::format)?;
        let count = pgrc_format::try_filled(nb, 0, "pgrc run reverse").map_err(S::format)?;
        Ok(RevBuilder {
            run_id,
            ranges,
            base,
            span,
            bucket,
            spill,
            stage,
            last,
            count,
            hint: 0,
            rows: 0,
            frames: 0,
            prefix: Vec::new(),
            group_starts: Vec::new(),
            force,
            spilled: 0,
        })
    }

    fn rank(&mut self, id: u64) -> Option<u64> {
        let i = match self.ranges.get(self.hint) {
            Some(&(lo, hi)) if lo <= id && id < hi => self.hint,
            _ => self.ranges.partition_point(|r| r.0 <= id).checked_sub(1)?,
        };
        let (lo, hi) = self.ranges[i];
        self.hint = i;
        (id < hi).then(|| self.base[i] + id - lo)
    }

    fn put(&mut self, rank: u64, frame: u64) -> Result<(), S::Error> {
        let b = (rank / self.bucket) as usize;
        let r = rank % self.bucket;
        let (lr, lf) = self.last[b];
        let d = r as i64 - lr as i64;
        varint(&mut self.stage[b], ((d << 1) ^ (d >> 63)) as u64);
        varint(&mut self.stage[b], frame - lf);
        self.last[b] = (r, frame);
        self.count[b] += 1;
        if self.stage[b].len() >= STAGE {
            self.flush(b)?;
        }
        Ok(())
    }

    fn flush(&mut self, b: usize) -> Result<(), S::Error> {
        if self.stage[b].is_empty() {
            return Ok(());
        }
        let bytes = std::mem::take(&mut self.stage[b]);
        self.spilled += bytes.len() as u64;
        self.spill.append(b, &bytes)
    }

    /// The next member (slot `members()`): its rows' IDs in ordinal order.
    pub fn member(&mut self, ids: &[u64]) -> Result<MemberFrames, S::Error> {
        let nv = ids.len().div_ceil(FRAME as usize) as u64;
        if ids.is_empty() || self.prefix.len() >= MAX_MEMBERS || self.frames + nv >= u64::from(u32::MAX) {
            return Err(S::format(contract("run reverse build: a member")));
        }
        let mut out = MemberFrames { vstarts: pgrc_format::try_vec(nv as usize, "pgrc run reverse").map_err(S::format)?, vmin: pgrc_format::try_vec(nv as usize, "pgrc run reverse").map_err(S::format)? };
        self.prefix.push(self.frames as u32);
        let mut prev_max: Option<u64> = None;
        for (v, chunk) in ids.chunks(FRAME as usize).enumerate() {
            let (mn, mx) = chunk.iter().fold((u64::MAX, 0), |(a, b), &x| (a.min(x), b.max(x)));
            if prev_max.is_none_or(|p| mn <= p) {
                self.group_starts.push(self.frames as u32);
            }
            prev_max = Some(mx);
            out.vstarts.push((v * FRAME as usize) as u32);
            out.vmin.push(mn);
            let frame = self.frames;
            for &id in chunk {
                let r = self.rank(id).ok_or_else(|| S::format(contract("run reverse build: an ID outside the run's ranges")))?;
                self.put(r, frame)?;
            }
            self.frames += 1;
        }
        self.rows += ids.len() as u64;
        Ok(out)
    }

    pub fn members(&self) -> usize {
        self.prefix.len()
    }

    /// Rank: DENSE when the ranges' span is at most 1.05 times the rows, else SET.
    /// Form: LABELS when within 0.5 bit a row of FRAMEIX (LABELS is the faster lookup).
    pub fn finish(mut self) -> Result<RevPlan<S>, S::Error> {
        if self.rows == 0 {
            return Err(S::format(contract("run reverse build: no member")));
        }
        for b in 0..self.stage.len() {
            self.flush(b)?;
        }
        let rows = self.rows;
        let rank = self.force.rank.unwrap_or(if self.span <= rows + rows / 20 { RankForm::Dense } else { RankForm::Set });
        let ranks = if rank == RankForm::Dense { self.span } else { rows };
        let members = self.prefix.len() as u32;
        let (f, g) = (self.frames as u32, self.group_starts.len() as u32);
        let labels = form_pages(RunRevForm::Labels, ranks, g, members) * PAGE as u64;
        let frameix = form_pages(RunRevForm::FrameIx, ranks, f, members) * PAGE as u64;
        let form = self.force.form.unwrap_or(if labels <= frameix + rows / 16 { RunRevForm::Labels } else { RunRevForm::FrameIx });
        let nsym = if form == RunRevForm::Labels { g } else { f };
        let spec = Spec { run_id: self.run_id, form, rank, ranks, rows, nsym, members };
        let mut prefix = self.prefix;
        prefix.push(f);
        Ok(RevPlan {
            spec,
            ranges: self.ranges,
            base: self.base,
            span: self.span,
            bucket: self.bucket,
            count: self.count,
            spill: self.spill,
            prefix,
            group_starts: self.group_starts,
            stats: RevStats { rows, frames: f, groups: g, members, spilled: self.spilled, labels_bytes: labels, frameix_bytes: frameix },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevStats {
    pub rows: u64,
    pub frames: u32,
    pub groups: u32,
    pub members: u32,
    pub spilled: u64,
    pub labels_bytes: u64,
    pub frameix_bytes: u64,
}

#[derive(Debug)]
pub struct RevPlan<S: Spill> {
    spec: Spec,
    ranges: IdRanges,
    base: Vec<u64>,
    span: u64,
    bucket: u64,
    count: Vec<u64>,
    spill: S,
    prefix: Vec<u32>,
    group_starts: Vec<u32>,
    stats: RevStats,
}

impl<S: Spill> RevPlan<S> {
    pub fn spec(&self) -> Spec {
        self.spec
    }

    pub fn stats(&self) -> RevStats {
        self.stats
    }

    fn id_of(&self, r: u64) -> u64 {
        let i = self.base.partition_point(|&x| x <= r) - 1;
        self.ranges[i].0 + r - self.base[i]
    }

    fn label(&self, frame: u64) -> u32 {
        match self.spec.form {
            RunRevForm::Labels => (self.group_starts.partition_point(|&s| u64::from(s) <= frame) - 1) as u32,
            _ => frame as u32,
        }
    }

    /// Bucket `b`'s entries, (rank in the bucket, frame), by rank; an ID twice is a contract error.
    fn load(&mut self, b: usize, buf: &mut Vec<u8>, ents: &mut Vec<(u64, u64)>) -> Result<(), S::Error> {
        self.spill.read(b, buf)?;
        ents.clear();
        let (mut at, mut r, mut f) = (0usize, 0i64, 0u64);
        for _ in 0..self.count[b] {
            let (Some(z), Some(df)) = (read_varint(buf, &mut at), read_varint(buf, &mut at)) else {
                return Err(S::format(FormatError::Corrupt { at: "run reverse build: a spill bucket" }));
            };
            r += ((z >> 1) as i64) ^ -((z & 1) as i64);
            f += df;
            ents.push((r as u64, f));
        }
        if at != buf.len() {
            return Err(S::format(FormatError::Corrupt { at: "run reverse build: a spill bucket" }));
        }
        ents.sort_unstable();
        if ents.windows(2).any(|w| w[0].0 == w[1].0) {
            return Err(S::format(contract("run reverse build: an ID twice in the run")));
        }
        Ok(())
    }

    /// Write the file through `out(offset, bytes)` (in file order but page 0, which is last).
    /// The same file every time: a dry pass gives page 0 first, so a caller can write the file in order.
    pub fn write(&mut self, out: &mut dyn FnMut(u64, &[u8]) -> Result<(), S::Error>) -> Result<(Finished, RevStats), S::Error> {
        let spec = self.spec;
        let mut w = Writer::new(spec).map_err(S::format)?;
        let (mut buf, mut ents) = (Vec::new(), Vec::new());
        let drain = |w: &mut Writer, out: &mut dyn FnMut(u64, &[u8]) -> Result<(), S::Error>| -> Result<(), S::Error> {
            let at = w.next_out_page() * PAGE as u64;
            let bytes = w.take();
            if bytes.is_empty() {
                return Ok(());
            }
            out(at, &bytes)
        };
        let nb = self.count.len();
        if spec.rank == RankForm::Set {
            for b in 0..nb {
                self.load(b, &mut buf, &mut ents)?;
                for &(r, _) in &ents {
                    let id = self.id_of(b as u64 * self.bucket + r);
                    w.push_s_run(id, 1).map_err(S::format)?;
                }
                drain(&mut w, &mut *out)?;
            }
        }
        for b in 0..nb {
            self.load(b, &mut buf, &mut ents)?;
            match spec.rank {
                RankForm::Dense => {
                    let len = self.bucket.min(self.span - b as u64 * self.bucket);
                    let mut j = 0usize;
                    for r in 0..len {
                        let label = match ents.get(j) {
                            Some(&(er, f)) if er == r => {
                                j += 1;
                                self.label(f)
                            }
                            _ => spec.nsym,
                        };
                        w.push_label(label).map_err(S::format)?;
                    }
                    if j != ents.len() {
                        return Err(S::format(contract("run reverse build: a rank past its bucket")));
                    }
                }
                RankForm::Set => {
                    for &(_, f) in &ents {
                        w.push_label(self.label(f)).map_err(S::format)?;
                    }
                }
            }
            drain(&mut w, &mut *out)?;
        }
        let members = self.prefix.len() - 1;
        if spec.form == RunRevForm::Labels {
            let total = self.prefix[members];
            for (k, &gs) in self.group_starts.iter().enumerate() {
                let ge = self.group_starts.get(k + 1).copied().unwrap_or(total);
                let slot = self.prefix[..members].partition_point(|&p| p <= gs) - 1;
                let p = self.prefix[slot];
                w.push_group(slot as u16, gs - p, ge - p).map_err(S::format)?;
            }
            drain(&mut w, &mut *out)?;
        }
        let at = w.next_out_page() * PAGE as u64;
        let prefix: &[u32] = if spec.form == RunRevForm::FrameIx { &self.prefix } else { &[] };
        let (rest, fin) = w.finish(prefix).map_err(S::format)?;
        if !rest.is_empty() {
            out(at, &rest)?;
        }
        out(0, &fin.page0)?;
        Ok((fin, self.stats))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgrc_format::rowmap::{encode_run_member, GranMap};
    use pgrc_format::runrev::{BytePages, Hit, RunRev};
    use pgrc_format::sortrun::SortRun;

    #[test]
    fn cut_rows_takes_the_tighter_bound() {
        assert_eq!(cut_rows(1 << 20, 256 << 20, 1000, 1000 * 100), 1 << 20);
        assert_eq!(cut_rows(1 << 20, 256 << 20, 1000, 1000 * 1340), (256u64 << 20) / 1340);
        assert_eq!(cut_rows(1 << 20, 256 << 20, 0, 0), 1 << 20);
        assert_eq!(cut_rows(1 << 20, 1, 10, 1 << 30), 1);
    }

    fn build(members: &[Vec<u64>], ranges: IdRanges, bucket: u64, force: Force) -> (Vec<u8>, SortRun, Vec<GranMap>, RevStats) {
        let mut b = RevBuilder::new(77, ranges.clone(), bucket, MemSpill::default(), force).unwrap();
        let mut maps = Vec::new();
        for ids in members {
            let fr = b.member(ids).unwrap();
            let body = encode_run_member(ids, &fr.vstarts, Some(&fr.vmin[..])).unwrap();
            maps.push(GranMap::decode(&body, ids.len() as u64).unwrap());
        }
        let mut plan = b.finish().unwrap();
        let spec = plan.spec();
        let mut file: Vec<u8> = Vec::new();
        let (fin, stats) = plan
            .write(&mut |at, bytes| {
                let end = at as usize + bytes.len();
                if file.len() < end {
                    file.resize(end, 0);
                }
                file[at as usize..end].copy_from_slice(bytes);
                Ok(())
            })
            .unwrap();
        assert_eq!(file.len() as u64, fin.rev_len);
        let run = SortRun { run_id: 77, level: 1, rev: spec.form, rank: spec.rank, rows: spec.rows, rev_len: fin.rev_len, rev_crc: fin.rev_crc, members: (1..=members.len() as u32).collect(), ranges };
        (file, run, maps, stats)
    }

    fn resolve(rev: &RunRev, file: &[u8], maps: &[GranMap], id: u64) -> Option<(usize, u32)> {
        match rev.lookup(id, &BytePages(file)).unwrap() {
            Hit::Absent => None,
            Hit::Group { slot, va, vb } => maps[slot as usize].run_find(va, vb, id).unwrap().map(|o| (slot as usize, o)),
            Hit::Frame { slot, vframe } => maps[slot as usize].run_find_in(vframe, id).unwrap().map(|o| (slot as usize, o)),
        }
    }

    fn check_every(members: &[Vec<u64>], ranges: &IdRanges, file: &[u8], run: &SortRun, maps: &[GranMap]) {
        let rev = RunRev::open(run, &BytePages(file)).unwrap();
        let mut seen = std::collections::HashSet::new();
        for (s, ids) in members.iter().enumerate() {
            for (o, &id) in ids.iter().enumerate() {
                assert_eq!(resolve(&rev, file, maps, id), Some((s, o as u32)), "id {id}");
                seen.insert(id);
            }
        }
        for &(lo, hi) in ranges {
            for id in (lo..hi).filter(|id| !seen.contains(id)).take(5000) {
                assert_eq!(resolve(&rev, file, maps, id), None, "absent id {id}");
            }
        }
    }

    fn keyed(e: u64, per: u64, stride: u64) -> Vec<u64> {
        let n = e * per;
        let mut by: Vec<(u64, u64)> = (0..n).map(|i| (i % e, 1000 + i * stride)).collect();
        by.sort_unstable();
        by.into_iter().map(|(_, id)| id).collect()
    }

    fn split(all: &[u64], cut: usize) -> Vec<Vec<u64>> {
        all.chunks(cut).map(|c| c.to_vec()).collect()
    }

    #[test]
    fn ascending_groups_elect_labels_and_answer_every_id() {
        let all = keyed(8, 4000, 1);
        let members = split(&all, 9000);
        let ranges = vec![(1000, 1000 + all.len() as u64)];
        let (file, run, maps, st) = build(&members, ranges.clone(), 0, Force::default());
        assert_eq!((run.rev, run.rank), (RunRevForm::Labels, RankForm::Dense));
        assert!(st.groups <= 8 * 2 + members.len() as u32 + 1, "{st:?}");
        check_every(&members, &ranges, &file, &run, &maps);
    }

    #[test]
    fn scattered_ids_elect_frameix_and_answer_every_id() {
        // a permutation of the range (7919 is prime to 200 000): every frame's fences span it, so a group is a frame
        let all: Vec<u64> = (0..200_000u64).map(|i| 5000 + (i * 7919) % 200_000).collect();
        let members = split(&all, 60_000);
        let ranges = vec![(5000, 205_000)];
        let (file, run, maps, st) = build(&members, ranges.clone(), 0, Force::default());
        assert_eq!(run.rev, RunRevForm::FrameIx, "{st:?}");
        check_every(&members, &ranges, &file, &run, &maps);
    }

    #[test]
    fn sparse_ids_take_set_ranks_across_many_buckets() {
        let ids: Vec<u64> = (0..300u64).flat_map(|blk| (0..100).map(move |k| 291 * blk + k)).collect();
        let all = keyed(1, ids.len() as u64, 1).into_iter().map(|i| ids[(i - 1000) as usize]).collect::<Vec<_>>();
        let ranges = vec![(0, 291 * 100), (291 * 100, 291 * 200), (291 * 200 + 7, 291 * 300)];
        let all: Vec<u64> = all.into_iter().filter(|&id| ranges.iter().any(|&(lo, hi)| (lo..hi).contains(&id))).collect();
        let members = split(&all, 7000);
        for force in [Force::default(), Force { form: Some(RunRevForm::FrameIx), rank: None }, Force { form: None, rank: Some(RankForm::Dense) }] {
            let (file, run, maps, st) = build(&members, ranges.clone(), 1000, force);
            assert_eq!(run.rank, force.rank.unwrap_or(RankForm::Set), "{st:?}");
            assert!(st.spilled > 0);
            check_every(&members, &ranges, &file, &run, &maps);
        }
    }

    #[test]
    fn dense_holes_answer_absent_and_buckets_split_the_span() {
        let mut all = keyed(3, 3000, 1);
        all.retain(|id| id % 23 != 0);
        let members = split(&all, 2500);
        let ranges = vec![(1000, 1000 + 9000)];
        for bucket in [0, 777, 4096] {
            let (file, run, maps, _) = build(&members, ranges.clone(), bucket, Force::default());
            assert_eq!(run.rank, RankForm::Dense);
            check_every(&members, &ranges, &file, &run, &maps);
        }
    }

    #[test]
    fn a_bad_id_or_an_id_twice_is_refused() {
        let mut b = RevBuilder::new(9, vec![(10, 20)], 0, MemSpill::default(), Force::default()).unwrap();
        assert!(b.member(&[10, 11, 25]).is_err());
        let mut b = RevBuilder::new(9, vec![(10, 20)], 0, MemSpill::default(), Force::default()).unwrap();
        b.member(&[10, 11, 12]).unwrap();
        b.member(&[12, 13]).unwrap();
        let mut plan = b.finish().unwrap();
        assert!(plan.write(&mut |_, _| Ok(())).is_err());
        assert!(RevBuilder::new(9, vec![(10, 20), (15, 30)], 0, MemSpill::default(), Force::default()).is_err());
        assert!(RevBuilder::new(0, vec![(10, 20)], 0, MemSpill::default(), Force::default()).is_err());
    }

    fn unit(level: u8, order: u64, rows: u64, parts: &[u32]) -> RunUnit {
        RunUnit { level, order, rows, bytes: rows * 16, parts: parts.to_vec() }
    }

    #[test]
    fn the_run_pass_merges_the_oldest_fanin_units_of_the_lowest_level() {
        let p = RunPolicy { fanin: 4, ..RunPolicy::default() };
        let loose: Vec<RunUnit> = (0..6u32).map(|n| unit(0, u64::from(20 - n), 1000, &[20 - n])).collect();
        assert_eq!(next_run_job(&loose, &p), Some(vec![15, 16, 17, 18]), "the oldest four (lowest part numbers)");
        assert_eq!(next_run_job(&loose[..3], &p), None, "three of four");
        let mut mixed = loose[..3].to_vec();
        mixed.extend((0..4u64).map(|r| unit(1, 100 + r, 4000, &[40 + 2 * r as u32, 41 + 2 * r as u32])));
        assert_eq!(next_run_job(&mixed, &p), Some(vec![40, 41, 42, 43, 44, 45, 46, 47]), "level 0 has three; level 1 has four runs");
        mixed.push(unit(0, 1, 1000, &[1]));
        assert_eq!(next_run_job(&mixed, &p), Some(vec![1, 18, 19, 20]), "level 0 first once it has four");
        assert_eq!(next_run_job(&[], &p), None);
        assert_eq!(next_run_job(&loose, &RunPolicy { fanin: 1, ..p }), None, "one unit is already a run");
    }

    #[test]
    fn the_run_pass_keeps_jobs_within_the_row_and_byte_caps() {
        let p = RunPolicy { fanin: 2, cap_rows: 3000, cap_bytes: u64::MAX };
        let v = [unit(0, 1, 2500, &[1]), unit(0, 2, 1000, &[2]), unit(0, 3, 1000, &[3])];
        assert_eq!(next_run_job(&v, &p), Some(vec![2, 3]), "1+2 is 3500 rows; 2+3 fits");
        let capped = [unit(1, 1, 2000, &[5]), unit(1, 2, 2000, &[6]), unit(2, 3, 1000, &[7]), unit(2, 4, 1000, &[8])];
        assert_eq!(next_run_job(&capped, &p), Some(vec![7, 8]), "level 1 at the cap; level 2 fits");
        let bytes = RunPolicy { cap_rows: u64::MAX, cap_bytes: 30_000, ..p };
        assert_eq!(next_run_job(&[unit(0, 1, 1000, &[1]), unit(0, 2, 1000, &[2])], &bytes), None, "32,000 bytes");
        assert_eq!(next_run_job(&[unit(0, 1, 900, &[1]), unit(0, 2, 900, &[2])], &bytes), Some(vec![1, 2]));
        assert_eq!(RunPolicy::default(), RunPolicy { fanin: 16, cap_rows: 1 << 30, cap_bytes: 16 << 30 });
    }

    fn member(part_no: u32, rows: u64, live: u64) -> crate::policy::PartView {
        crate::policy::PartView { part_no, rows, live, bytes: rows * 16, compacted: true, sorted: true, age_s: 0 }
    }

    #[test]
    fn member_maintenance_retires_the_dead_then_rewrites_the_most_deleted() {
        let p = crate::policy::Policy::default();
        assert_eq!(next_member_job(&[], &p), None);
        assert_eq!(next_member_job(&[member(5, 1000, 1000), member(6, 1000, 600)], &p), None, "40% deleted, the floor is 50%");
        assert_eq!(next_member_job(&[member(5, 1000, 1000), member(6, 1000, 500), member(7, 1000, 300)], &p), Some(MemberJob::Swap(7)));
        assert_eq!(next_member_job(&[member(8, 1000, 300), member(6, 1000, 300)], &p), Some(MemberJob::Swap(6)), "equals: the lower part number");
        assert_eq!(next_member_job(&[member(5, 1000, 0), member(6, 1000, 100), member(7, 1000, 0)], &p), Some(MemberJob::Retire(vec![5, 7])), "the dead first");
        let many: Vec<_> = (0..40u32).map(|n| member(100 + n, 1000, 0)).collect();
        assert_eq!(next_member_job(&many, &p), Some(MemberJob::Retire((100..100 + p.max_inputs as u32).collect())));
    }

    #[test]
    fn a_kept_members_frames_start_among_its_survivors() {
        let vstarts = [0, 512, 1024, 1536, 2000];
        // rows 0..2100; survivors: the odd rows, none of frame 2 (1024..1536), none from 2000 on
        let kept: Vec<u32> = (0..2100u32).filter(|o| o % 2 == 1 && !(1024..1536).contains(o) && *o < 2000).collect();
        let k = kept_vstarts(&vstarts, &kept);
        assert_eq!(k, vec![0, 256, 512, 512, kept.len() as u32]);
        assert_eq!(kept_vstarts(&[0], &[3, 4]), vec![0]);
    }
}

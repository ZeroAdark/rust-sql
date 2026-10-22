//! A part is full when it reached the target (rows, or its buffer bytes): it is never concatenated, only rewritten once its deleted fraction reaches `dv_rewrite`.
//! `max_share` bounds write amplification: a row a merge rewrites lands in a part at least 1 / `max_share` its old part's live size,
//! so it is rewritten at most log(target / smallest part) / log(1 / `max_share`) times.

use pgrc_write::writer::PartCutPolicy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartView {
    pub part_no: u32,
    pub rows: u64,
    pub live: u64,
    /// The part's buffer bytes as the bulk writer counts them for its cut (`ColBuffer::approx_bytes`), all rows.
    pub bytes: u64,
    pub compacted: bool,
    /// The part attests the table's `sorted_by` key (informational; a sorted merge sorts regardless).
    pub sorted: bool,
    pub age_s: u64,
}

impl PartView {
    fn deleted_fraction(&self) -> f64 {
        if self.rows == 0 { 0.0 } else { (self.rows - self.live) as f64 / self.rows as f64 }
    }

    pub fn live_bytes(&self) -> u64 {
        if self.rows == 0 { 0 } else { (u128::from(self.bytes) * u128::from(self.live) / u128::from(self.rows)) as u64 }
    }

    pub fn full(&self, p: &Policy) -> bool {
        self.rows >= p.max_rows || self.bytes >= p.max_bytes
    }

    pub fn tiny(&self, p: &Policy) -> bool {
        self.bytes <= p.max_bytes / TINY_DIV
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Policy {
    pub max_rows: u64,
    pub max_bytes: u64,
    pub max_inputs: usize,
    pub max_inputs_tiny: usize,
    pub min_fanin: usize,
    /// A merge of tiny inputs only needs this many when it is above the fan-in (0: the fan-in alone).
    pub tiny_fanin: usize,
    pub aged_fanin: usize,
    pub age_s: u64,
    pub max_share: f64,
    pub dv_rewrite: f64,
}

impl Default for Policy {
    fn default() -> Policy {
        let cut = PartCutPolicy::default();
        Policy { max_rows: cut.max_rows, max_bytes: cut.max_bytes, max_inputs: 16, max_inputs_tiny: 256, min_fanin: 4, tiny_fanin: 0, aged_fanin: 2, age_s: 600, max_share: 2.0 / 3.0, dv_rewrite: 0.5 }
    }
}

/// A part is tiny at most the target's bytes / this.
pub const TINY_DIV: u64 = 1024;

impl Policy {
    /// The worker's jobs after the first in one call: a run of tiny parts waits until [`CONTINUING_TINY_FANIN`].
    pub fn continuing(self) -> Policy {
        Policy { tiny_fanin: CONTINUING_TINY_FANIN, ..self }
    }
}

pub const CONTINUING_TINY_FANIN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Concat,
    SortedMerge,
    DvRewrite,
    /// For the ledger only: no policy here chooses one, and no `Job` carries it (`pgrc_am::compact::run_job` and `shadow` read the kind with `==`).
    Run,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Retire,
    Fanin,
    Aged,
    DvFloor,
    Forced,
    Tiered,
}

impl Reason {
    pub const ALL: [Reason; 6] = [Reason::Retire, Reason::Fanin, Reason::Aged, Reason::DvFloor, Reason::Forced, Reason::Tiered];

    pub fn name(self) -> &'static str {
        match self {
            Reason::Retire => "retire",
            Reason::Fanin => "fanin",
            Reason::Aged => "aged",
            Reason::DvFloor => "dv_floor",
            Reason::Forced => "forced",
            Reason::Tiered => "tiered",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub kind: JobKind,
    pub reason: Reason,
    pub inputs: Vec<u32>,
}

pub fn next_job(parts: &[PartView], sorted_by: bool, p: &Policy) -> Option<Job> {
    let job = |kind, reason, v: &[PartView]| Job { kind, reason, inputs: v.iter().map(|x| x.part_no).collect() };
    let dead: Vec<PartView> = parts.iter().filter(|x| x.live == 0).take(p.max_inputs).copied().collect();
    if !dead.is_empty() {
        return Some(job(JobKind::DvRewrite, Reason::Retire, &dead));
    }
    let merge = if sorted_by { JobKind::SortedMerge } else { JobKind::Concat };
    let tiny_cap = if sorted_by { p.max_inputs } else { p.max_inputs_tiny };
    // per window prefix: (live sum, largest live, oldest age), so each candidate length costs O(1)
    let mut pre: Vec<(u64, u64, u64)> = Vec::new();
    let mut i = 0;
    while i < parts.len() {
        let end = i + parts[i..].iter().take_while(|x| !x.full(p)).count();
        for s in i..end {
            let w = &parts[s..s + extend(&parts[s..end], p, tiny_cap, |_| true)];
            let tiny = w.iter().take_while(|x| x.tiny(p)).count();
            pre.clear();
            pre.extend(w.iter().scan((0u64, 0u64, 0u64), |a, x| {
                *a = (a.0 + x.live, a.1.max(x.live), a.2.max(x.age_s));
                Some(*a)
            }));
            for k in (1..=w.len()).rev() {
                let (total, top, oldest) = pre[k - 1];
                let fanin = if k <= tiny { p.min_fanin.max(p.tiny_fanin) } else { p.min_fanin };
                if (k >= fanin || (k >= p.aged_fanin && oldest >= p.age_s)) && top as f64 <= p.max_share * total as f64 {
                    return Some(job(merge, if k >= fanin { Reason::Fanin } else { Reason::Aged }, &w[..k]));
                }
            }
        }
        i = end + 1;
    }
    let heavy = |x: &PartView| x.deleted_fraction() >= p.dv_rewrite;
    let (h, _) = parts.iter().enumerate().filter(|(_, x)| heavy(x)).max_by(|a, b| a.1.deleted_fraction().total_cmp(&b.1.deleted_fraction()).then(b.0.cmp(&a.0)))?;
    let mut lo = h;
    while lo > 0 && heavy(&parts[lo - 1]) && fits(&parts[lo - 1..=h], p) {
        lo -= 1;
    }
    let hi = lo + extend(&parts[lo..], p, p.max_inputs, |k| k <= h - lo || heavy(&parts[lo + k]));
    let kind = if hi - lo > 1 && sorted_by { JobKind::SortedMerge } else { JobKind::DvRewrite };
    Some(job(kind, Reason::DvFloor, &parts[lo..hi]))
}

fn fits(v: &[PartView], p: &Policy) -> bool {
    v.len() <= p.max_inputs && v.iter().map(|x| x.live).sum::<u64>() <= p.max_rows && v.iter().map(|x| x.live_bytes()).sum::<u64>() <= p.max_bytes
}

fn extend(v: &[PartView], p: &Policy, tiny_cap: usize, ok: impl Fn(usize) -> bool) -> usize {
    let Some(first) = v.first() else { return 0 };
    let (mut n, mut live, mut bytes, mut tiny) = (1, first.live, first.live_bytes(), first.tiny(p));
    while n < v.len() && ok(n) {
        let x = &v[n];
        let (l, b, t) = (live + x.live, bytes + x.live_bytes(), tiny && x.tiny(p));
        if n + 1 > if t { tiny_cap } else { p.max_inputs } || l > p.max_rows || b > p.max_bytes {
            break;
        }
        (n, live, bytes, tiny) = (n + 1, l, b, t);
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(part_no: u32, rows: u64, live: u64, compacted: bool) -> PartView {
        PartView { part_no, rows, live, bytes: rows * 40, compacted, sorted: false, age_s: 0 }
    }

    fn small(part_no: u32, rows: u64) -> PartView {
        part(part_no, rows, rows, false)
    }

    /// A part of a wide table: 1,400 buffer bytes a row, so the byte budget cuts at ~192 K rows.
    fn wide(part_no: u32, rows: u64) -> PartView {
        PartView { bytes: rows * 1400, ..small(part_no, rows) }
    }

    fn inputs(j: Option<Job>) -> Option<(JobKind, Vec<u32>)> {
        j.map(|j| (j.kind, j.inputs))
    }

    #[test]
    fn each_job_names_the_rule_that_chose_it() {
        let p = Policy::default();
        let reason = |parts: &[PartView]| next_job(parts, false, &p).map(|j| j.reason);
        assert_eq!(reason(&[small(0, 1000), part(1, 1000, 0, false)]), Some(Reason::Retire));
        assert_eq!(reason(&(0..4).map(|i| small(i, 1000)).collect::<Vec<_>>()), Some(Reason::Fanin));
        assert_eq!(reason(&[small(0, 1000), small(1, 1000)]), None, "two young small parts wait for the fan-in");
        assert_eq!(reason(&[PartView { age_s: p.age_s, ..small(0, 1000) }, small(1, 1000)]), Some(Reason::Aged), "under the fan-in, aged");
        assert_eq!(reason(&[part(0, 1 << 20, 1 << 19, false)]), Some(Reason::DvFloor));
        let names: std::collections::BTreeSet<&str> = Reason::ALL.iter().map(|r| r.name()).collect();
        assert_eq!(names.len(), Reason::ALL.len());
    }

    #[test]
    fn the_target_is_the_bulk_writers_cut() {
        let (p, cut) = (Policy::default(), PartCutPolicy::default());
        assert_eq!((p.max_rows, p.max_bytes), (cut.max_rows, cut.max_bytes));
        assert!(small(0, cut.max_rows).full(&p) && !small(0, cut.max_rows - 1).full(&p));
        assert!(wide(0, 196_000).full(&p) && !wide(0, 190_000).full(&p), "a wide table: the byte budget governs");
    }

    #[test]
    fn policy_tables() {
        let p = Policy::default();
        let seals: Vec<PartView> = (0..6).map(|i| small(i, 60_000)).collect();
        let bulk: Vec<PartView> = (0..5).map(|i| small(i, 1 << 20)).collect();
        let wide_parts: Vec<PartView> = (0..5).map(|i| wide(i, 196_000)).collect();
        let big: Vec<PartView> = (0..6).map(|i| small(i, 400_000)).collect();
        let mixed = vec![small(0, 1 << 20), small(1, 30_000), small(2, 30_000), small(3, 30_000), small(4, 1 << 20), small(5, 20_000), small(6, 20_000), small(7, 20_000), small(8, 20_000), small(9, 20_000)];
        let cases: Vec<(&str, Vec<PartView>, bool, Option<(JobKind, Vec<u32>)>)> = vec![
            ("small seals concatenate up to the target", seals.clone(), false, Some((JobKind::Concat, vec![0, 1, 2, 3, 4, 5]))),
            ("sorted_by merges", seals.clone(), true, Some((JobKind::SortedMerge, vec![0, 1, 2, 3, 4, 5]))),
            ("a bulk load has nothing to do", bulk, false, None),
            ("nor does a wide table's (bytes)", wide_parts, false, None),
            ("two fit the target: under the fan-in", big, false, None),
            ("a run of three waits; the run of five goes", mixed.clone(), false, Some((JobKind::Concat, vec![5, 6, 7, 8, 9]))),
            ("empty", Vec::new(), false, None),
        ];
        for (why, parts, sorted_by, want) in cases {
            assert_eq!(inputs(next_job(&parts, sorted_by, &p)), want, "{why}");
        }
        let mut aged = mixed[..5].to_vec();
        aged[1].age_s = 601;
        assert_eq!(inputs(next_job(&aged, false, &p)), Some((JobKind::Concat, vec![1, 2, 3])), "an aged run of three");
        let rows: Vec<PartView> = (0..8).map(|i| small(i, 200_000)).collect();
        assert_eq!(inputs(next_job(&rows, false, &p)), Some((JobKind::Concat, vec![0, 1, 2, 3, 4])), "capped at the target's rows");
        let many: Vec<PartView> = (0..40).map(|i| PartView { bytes: p.max_bytes / 1024 + 1, ..small(i, 1000) }).collect();
        assert_eq!(next_job(&many, false, &p).unwrap().inputs, (0..16).collect::<Vec<_>>(), "capped at 16 inputs");
        let wides: Vec<PartView> = (0..8).map(|i| wide(i, 41_000)).collect();
        assert_eq!(inputs(next_job(&wides, false, &p)), Some((JobKind::Concat, vec![0, 1, 2, 3])), "capped at the target's bytes");
        let slide: Vec<PartView> = [250_000, 250_000, 250_000, 100_000, 100_000, 100_000, 100_000].iter().enumerate().map(|(i, &r)| small(i as u32, r)).collect();
        assert_eq!(inputs(next_job(&slide, false, &Policy { max_rows: 500_000, ..p })), Some((JobKind::Concat, vec![3, 4, 5, 6])));
    }

    fn sized(part_no: u32, rows: u64, bytes: u64) -> PartView {
        PartView { bytes, ..small(part_no, rows) }
    }

    #[test]
    fn tiny_inputs_raise_the_cap() {
        let p = Policy::default();
        let tiny_max = p.max_bytes / 1024;
        let copies: Vec<PartView> = (0..300).map(|i| small(i, 10)).collect();
        assert_eq!(inputs(next_job(&copies, false, &p)), Some((JobKind::Concat, (0..256).collect())), "256 tiny parts make one job");
        assert_eq!(next_job(&copies, true, &p).map(|j| (j.kind, j.inputs.len())), Some((JobKind::SortedMerge, 16)), "sorted_by stays at 16");
        let at: Vec<PartView> = (0..40).map(|i| sized(i, 100, tiny_max)).collect();
        assert_eq!(next_job(&at, false, &p).map(|j| j.inputs.len()), Some(40), "at the bound: tiny");
        let over: Vec<PartView> = (0..40).map(|i| sized(i, 100, tiny_max + 1)).collect();
        assert_eq!(next_job(&over, false, &p).map(|j| j.inputs.len()), Some(16), "over the bound: 16");
        let mut mixed: Vec<PartView> = (0..60).map(|i| small(i, 10)).collect();
        mixed[40] = sized(40, 10, tiny_max + 1);
        assert_eq!(inputs(next_job(&mixed, false, &p)), Some((JobKind::Concat, (0..40).collect())), "the tiny prefix");
        mixed[40] = small(40, 10);
        mixed[5] = sized(5, 10, tiny_max + 1);
        assert_eq!(inputs(next_job(&mixed, false, &p)), Some((JobKind::Concat, (0..16).collect())), "a non-tiny input: 16");
        let dense: Vec<PartView> = (0..300).map(|i| sized(i, 10_000, 200_000)).collect();
        assert_eq!(next_job(&dense, false, &p).map(|j| j.inputs.len()), Some(104), "the target's rows");
        let mut lead: Vec<PartView> = (0..100).map(|i| small(i, 10)).collect();
        lead[0] = sized(0, 5000, tiny_max);
        let j = next_job(&lead, false, &p).expect("a job");
        assert!(!j.inputs.contains(&0) && j.inputs.len() == 99, "{:?}", j.inputs.len());
        assert_eq!(next_job(&copies[..3], false, &p), None);
        assert_eq!(next_job(&copies[..4], false, &p).map(|j| j.inputs.len()), Some(4));
    }

    #[test]
    fn outputs_merge_again_up_to_the_target() {
        let p = Policy::default();
        let out = |no, rows| part(no, rows, rows, true);
        let level = vec![out(0, 240_000), out(1, 240_000), out(2, 240_000), out(3, 240_000)];
        assert_eq!(inputs(next_job(&level, false, &p)), Some((JobKind::Concat, vec![0, 1, 2, 3])));
        assert_eq!(inputs(next_job(&[out(0, 600_000), out(1, 600_000)], false, &Policy { min_fanin: 2, ..p })), None, "past the target together");
        let mut near = vec![out(0, 900_000), small(1, 30_000), small(2, 30_000), small(3, 30_000), small(4, 30_000)];
        assert_eq!(inputs(next_job(&near, false, &p)), Some((JobKind::Concat, vec![1, 2, 3, 4])));
        near[0].age_s = 601;
        near.truncate(3);
        assert_eq!(inputs(next_job(&near, false, &p)), None, "aged, but 900 K would take 94 % of the job");
        let pair = vec![PartView { age_s: 601, ..out(0, 400_000) }, out(1, 300_000)];
        assert_eq!(inputs(next_job(&pair, false, &p)), Some((JobKind::Concat, vec![0, 1])), "an aged pair of like sizes");
    }

    #[test]
    fn no_job_builds_past_the_target() {
        let p = Policy::default();
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = |m: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % m.max(1)
        };
        let (mut jobs, mut near_full) = (0, 0);
        for round in 0..20_000 {
            let n = 1 + next(24) as usize;
            let parts: Vec<PartView> = (0..n)
                .map(|i| {
                    let per_row = 40 + next(4000);
                    let cut = p.max_bytes.div_ceil(per_row).min(p.max_rows);
                    let rows = match next(4) {
                        0 => cut + next(8192),
                        1 => cut * (90 + next(10)) / 100,
                        2 => 1 + next(cut / 4),
                        _ => 1 + next(cut),
                    };
                    let live = if next(4) == 0 { next(rows + 1) } else { rows };
                    PartView { part_no: i as u32, rows, live, bytes: rows * per_row, compacted: next(2) == 0, sorted: false, age_s: next(1200) }
                })
                .collect();
            let sorted_by = round % 2 == 0;
            for pol in [p, Policy { min_fanin: 2, aged_fanin: 2, max_share: 1.0, ..p }] {
                let Some(job) = next_job(&parts, sorted_by, &pol) else { continue };
                jobs += 1;
                let v: Vec<PartView> = job.inputs.iter().map(|&no| parts[no as usize]).collect();
                let (live, bytes) = (v.iter().map(|x| x.live).sum::<u64>(), v.iter().map(|x| x.live_bytes()).sum::<u64>());
                assert!(live <= p.max_rows && bytes <= p.max_bytes, "{job:?} over {v:?}: {live} rows, {bytes} B");
                let heavy = v.iter().all(|x| x.live == 0 || x.deleted_fraction() >= p.dv_rewrite);
                match job.kind {
                    JobKind::Concat => assert!(v.len() > 1 && v.iter().all(|x| !x.full(&p)), "{job:?} over {v:?}"),
                    JobKind::SortedMerge => assert!(v.len() > 1 && (v.iter().all(|x| !x.full(&p)) || heavy), "{job:?} over {v:?}"),
                    JobKind::DvRewrite => assert!(heavy, "{job:?} over {v:?}"),
                    JobKind::Run => panic!("the policy chose a run job: {job:?}"),
                }
                if v.len() > 1 && v.iter().any(|x| !x.full(&p) && x.live_bytes() * 10 >= p.max_bytes * 9) {
                    near_full += 1;
                }
            }
        }
        assert!(jobs > 10_000 && near_full > 10, "{jobs} jobs, {near_full} merges of a part at 0.9 of the budget or more");
    }

    #[test]
    fn dv_rewrites_and_retires() {
        let p = Policy::default();
        let heavy = vec![small(0, 1 << 20), part(1, 1 << 20, 400_000, false), part(2, 1 << 20, 300_000, true), part(3, 1 << 20, 900_000, true)];
        assert_eq!(inputs(next_job(&heavy, false, &p)), Some((JobKind::DvRewrite, vec![1, 2])), "the heaviest joins its heavy neighbour");
        assert_eq!(inputs(next_job(&heavy, true, &p)), Some((JobKind::SortedMerge, vec![1, 2])), "sorted_by merges two parts");
        let alone = vec![part(0, 1 << 20, 500_000, false), part(1, 1 << 20, 600_000, true)];
        assert_eq!(inputs(next_job(&alone, true, &p)), Some((JobKind::DvRewrite, vec![0])), "a light neighbour stays; one part keeps its order");
        let light = vec![part(0, 1 << 20, 600_000, true)];
        assert_eq!(inputs(next_job(&light, false, &p)), None, "a full part under 50 % deleted stays");
        let wide_part = vec![PartView { live: 150_000, ..wide(0, 196_000) }];
        assert_eq!(inputs(next_job(&wide_part, false, &p)), None, "and so does a wide table's part");
        let too_big = vec![part(0, 4 << 20, 1 << 20, true), part(1, 4 << 20, 1 << 20, true)];
        assert_eq!(inputs(next_job(&too_big, false, &p)), Some((JobKind::DvRewrite, vec![0])), "two would pass the target's rows");
        let dead = vec![small(0, 1 << 20), part(1, 5000, 0, false), part(2, 1 << 20, 0, true), small(3, 10)];
        assert_eq!(inputs(next_job(&dead, false, &p)), Some((JobKind::DvRewrite, vec![1, 2])), "all-deleted parts retire first, together");
    }
}

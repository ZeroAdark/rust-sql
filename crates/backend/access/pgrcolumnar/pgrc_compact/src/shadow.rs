//! The costed policy's shadow (logs only; no job is added, skipped or moved). Guardrail jobs are never scored.

use crate::policy::{next_job, JobKind, PartView, Policy, Reason};

pub const GRANULE_ROWS: f64 = 8192.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reads {
    pub scans_per_s: f64,
    pub key_share: f64,
    /// Granules decoded / (decoded + pruned + answered): the share of a dead row's granules a scan reads.
    pub read_share: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Costs {
    pub k: f64,
    pub r: f64,
    pub h_s: f64,
    /// A part with a delete vector costs a scan this many times as much per live row (1: no difference).
    pub dv_factor: f64,
    pub kappa: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate<'a> {
    pub kind: JobKind,
    pub inputs: &'a [PartView],
}

pub fn saved_per_scan(kind: JobKind, inputs: &[PartView], reads: &Reads, c: &Costs) -> f64 {
    let live: u64 = inputs.iter().map(|x| x.live).sum();
    let removed = inputs.len().saturating_sub(usize::from(live > 0));
    let granules = if kind == JobKind::SortedMerge { inputs.len().saturating_sub(1) as f64 * reads.key_share } else { 0.0 };
    removed as f64 * c.k + deleted_term(inputs, reads, c) + granules * GRANULE_ROWS
}

pub fn deleted_term(inputs: &[PartView], reads: &Reads, c: &Costs) -> f64 {
    inputs
        .iter()
        .filter(|x| x.rows > x.live)
        .map(|x| reads.read_share * (x.live as f64 * (c.dv_factor - 1.0).max(0.0) + (x.rows - x.live) as f64 * c.kappa))
        .sum()
}

/// Seconds of the table's reads the job takes to pay back (0 for a job that rewrites nothing, infinite with no benefit).
pub fn payback_s(kind: JobKind, inputs: &[PartView], reads: &Reads, c: &Costs) -> f64 {
    let cost = inputs.iter().map(|x| x.live).sum::<u64>() as f64 * c.r;
    let gain = reads.scans_per_s * saved_per_scan(kind, inputs, reads, c);
    if cost <= 0.0 {
        0.0
    } else if gain > 0.0 {
        cost / gain
    } else {
        f64::INFINITY
    }
}

pub fn would_run(kind: JobKind, inputs: &[PartView], reads: &Reads, c: &Costs) -> bool {
    payback_s(kind, inputs, reads, c) <= c.h_s
}

pub fn skipped<'a>(parts: &'a [PartView], sorted_by: bool, p: &Policy) -> Vec<Candidate<'a>> {
    let mut out = Vec::new();
    let aged = Policy { age_s: 0, dv_rewrite: f64::INFINITY, ..*p };
    let mut rest = parts;
    while let Some(j) = next_job(rest, sorted_by, &aged).filter(|j| j.reason == Reason::Aged) {
        let Some(i) = rest.iter().position(|x| x.part_no == j.inputs[0]) else { break };
        let end = (i + j.inputs.len()).min(rest.len());
        out.push(Candidate { kind: j.kind, inputs: &rest[i..end] });
        rest = &rest[end..];
    }
    for x in parts {
        if x.live > 0 && x.live < x.rows && ((x.rows - x.live) as f64) < p.dv_rewrite * x.rows as f64 {
            out.push(Candidate { kind: JobKind::DvRewrite, inputs: std::slice::from_ref(x) });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(part_no: u32, rows: u64, live: u64) -> PartView {
        PartView { part_no, rows, live, bytes: rows * 40, compacted: false, sorted: false, age_s: 0 }
    }

    const G: Costs = Costs { k: 1386.0, r: 340.0, h_s: 3600.0, dv_factor: 4.9, kappa: 0.0 };
    const R: Costs = Costs { k: 116.0, r: 15.0, h_s: 3600.0, dv_factor: 1.0, kappa: 0.0 };

    #[test]
    fn a_dv_rewrite_saves_gravity_its_slow_live_rows_and_the_row_executor_its_dead_rows() {
        let x = [part(0, 1_000_000, 750_000)];
        let hot = Reads { scans_per_s: 1.0, key_share: 0.0, read_share: 1.0 };
        assert!((saved_per_scan(JobKind::DvRewrite, &x, &hot, &G) - 750_000.0 * 3.9).abs() < 1e-6, "no part removed: the live rows' slowdown");
        assert!((saved_per_scan(JobKind::DvRewrite, &x, &Reads { read_share: 0.5, ..hot }, &G) - 750_000.0 * 3.9 * 0.5).abs() < 1e-6, "half the granules pruned");
        assert_eq!(saved_per_scan(JobKind::DvRewrite, &x, &hot, &R), 0.0, "dead rows cost the row executor nothing (narrow)");
        assert_eq!(saved_per_scan(JobKind::DvRewrite, &x, &hot, &Costs { kappa: 0.5, ..R }), 125_000.0, "wide: half a row-read a dead row");
        assert_eq!(deleted_term(&[part(0, 1000, 1000)], &hot, &G), 0.0, "no delete vector, no term");
        // 750k x 340 / (2.925M x scans_per_s): pays back within the hour at one scan a second, not at one an hour
        assert!(would_run(JobKind::DvRewrite, &x, &hot, &G));
        assert!(!would_run(JobKind::DvRewrite, &x, &Reads { scans_per_s: 1.0 / 3600.0, ..hot }, &G));
        assert!(!would_run(JobKind::DvRewrite, &x, &hot, &R), "no gain under the row executor: never pays back");
        assert_eq!(payback_s(JobKind::DvRewrite, &x, &Reads { scans_per_s: 0.0, ..hot }, &G), f64::INFINITY);
    }

    #[test]
    fn only_a_sorted_merge_saves_granules() {
        let x = [part(0, 1000, 1000), part(1, 1000, 1000), part(2, 1000, 1000)];
        let r = Reads { scans_per_s: 1.0, key_share: 0.5, read_share: 1.0 };
        assert_eq!(saved_per_scan(JobKind::Concat, &x, &r, &G), 2.0 * G.k);
        assert_eq!(saved_per_scan(JobKind::SortedMerge, &x, &r, &G), 2.0 * G.k + 2.0 * 0.5 * GRANULE_ROWS);
        assert_eq!(payback_s(JobKind::Concat, &[part(0, 10, 0), part(1, 10, 0)], &r, &G), 0.0, "nothing rewritten");
    }

    #[test]
    fn the_skipped_jobs_are_the_waiting_runs_and_the_parts_under_the_dv_floor() {
        let p = Policy::default();
        let parts = [part(0, 1000, 1000), part(1, 1000, 1000), part(2, 1 << 20, 900_000), part(3, 1000, 1000), part(4, 1000, 1000), part(5, 5000, 4000)];
        assert_eq!(next_job(&parts, false, &p), None, "the policy waits");
        let got: Vec<(JobKind, Vec<u32>)> = skipped(&parts, false, &p).into_iter().map(|c| (c.kind, c.inputs.iter().map(|x| x.part_no).collect())).collect();
        assert_eq!(got, [(JobKind::Concat, vec![0, 1]), (JobKind::Concat, vec![3, 4, 5]), (JobKind::DvRewrite, vec![2]), (JobKind::DvRewrite, vec![5])]);
        assert!(skipped(&[part(0, 1000, 1000)], true, &p).is_empty(), "one small part: nothing to merge, nothing deleted");
    }
}

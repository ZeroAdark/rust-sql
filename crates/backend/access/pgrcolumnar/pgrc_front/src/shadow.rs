//! Reads only the table's resident facts and its read rates: no relation, lock or file.

use pgrc_compact::policy::{Job, PartView, Policy, Reason};
use pgrc_compact::shadow::{self as sc, Costs, Reads};
use types_core::Oid;

pub const K_GRAVITY: u64 = 1386;
pub const K_ROWEXEC: u64 = 116;
pub const R_GRAVITY: u64 = 340;
pub const R_ROWEXEC: u64 = 15;
pub const H_S: u64 = 3600;
pub const DV_FACTOR_GRAVITY: f64 = 4.9;
pub const KAPPA_ROWEXEC: f64 = 0.0;
pub const SWEEP_TABLES: u32 = 256;

pub fn costs() -> [Costs; 2] {
    let s = |name: &str, d: u64| crate::maint::setting(name).unwrap_or(d) as f64;
    let f = |name: &str, d: f64| crate::maint::setting_f64(name).unwrap_or(d);
    let h_s = s("pgrust.pgrc_shadow_h_s", H_S);
    [
        Costs { k: s("pgrust.pgrc_shadow_k", K_GRAVITY), r: s("pgrust.pgrc_shadow_r", R_GRAVITY), h_s, dv_factor: f("pgrust.pgrc_shadow_dv_factor", DV_FACTOR_GRAVITY), kappa: 0.0 },
        Costs { k: s("pgrust.pgrc_shadow_k_rowexec", K_ROWEXEC), r: s("pgrust.pgrc_shadow_r_rowexec", R_ROWEXEC), h_s, dv_factor: 1.0, kappa: f("pgrust.pgrc_shadow_kappa_rowexec", KAPPA_ROWEXEC) },
    ]
}

enum Tel {
    Cold,
    Busy,
    Reads(Reads),
}

fn reads(db: Oid, relid: Oid) -> Tel {
    match pgrc_am::scanstats::read_rates(db, relid) {
        None => Tel::Cold,
        Some(None) => Tel::Busy,
        Some(Some(r)) => {
            let g = r.granules_read + r.granules_pruned + r.granules_answered;
            Tel::Reads(Reads {
                scans_per_s: r.scans,
                key_share: if r.scans > 0.0 { (r.key_scans / r.scans).min(1.0) } else { 0.0 },
                read_share: if g > 0.0 { r.granules_read / g } else { 1.0 },
            })
        }
    }
}

pub fn job(db: Oid, relid: Oid, job: &Job, parts: &[PartView], first: bool) {
    if job.reason == Reason::Forced {
        return;
    }
    let r = match reads(db, relid) {
        Tel::Cold => return crate::ledger::shadow_cold(db, relid),
        Tel::Busy => return,
        Tel::Reads(r) => r,
    };
    if job.reason != Reason::Aged {
        return crate::ledger::shadow_job(db, relid, false, [false, false], first.then_some([0.0, 0.0]));
    }
    let mut ins = job.inputs.clone();
    ins.sort_unstable();
    let inputs: Vec<PartView> = parts.iter().filter(|v| ins.binary_search(&v.part_no).is_ok()).copied().collect();
    let cs = costs();
    let payback = cs.map(|c| sc::payback_s(job.kind, &inputs, &r, &c));
    let skip = [0, 1].map(|i| payback[i] > cs[i].h_s);
    crate::ledger::shadow_job(db, relid, true, skip, first.then_some(payback));
}

pub fn idle(db: Oid, relid: Oid, parts: &[PartView], sorted_by: Option<bool>, p: &Policy, cs: &[Costs; 2]) -> bool {
    let mut cands = sc::skipped(parts, sorted_by.unwrap_or(false), p);
    if cands.is_empty() {
        crate::ledger::shadow_idle(db, relid, 0, [0, 0]);
        return false;
    }
    let r = match reads(db, relid) {
        Tel::Cold => {
            crate::ledger::shadow_cold(db, relid);
            return true;
        }
        Tel::Busy => return true,
        Tel::Reads(r) => r,
    };
    if sorted_by.is_none() && r.key_share > 0.0 {
        cands = sc::skipped(parts, true, p);
    }
    let add = [0, 1].map(|i| cands.iter().filter(|c| sc::would_run(c.kind, &c.inputs, &r, &cs[i])).count() as u64);
    crate::ledger::shadow_idle(db, relid, cands.len() as u64, add);
    true
}

pub fn sweep_due(slot: pgrc_am::catalog::RelSlot, relids: &[(pgrc_am::catalog::RelSlot, Oid)], cs: &[Costs; 2], budget: &mut u32, spent_ns: &mut u64) -> bool {
    let Some(v) = crate::compact::head_views(slot) else { return false };
    let p = Policy::default();
    if pgrc_compact::policy::next_job(&v, false, &p).is_some() {
        return true;
    }
    if *budget > 0 {
        if let Ok(i) = relids.binary_search_by_key(&slot, |x| x.0) {
            let t0 = pg_clock::MonoStamp::now();
            if idle(slot.1, relids[i].1, &v, None, &p, cs) {
                *budget -= 1;
            }
            *spent_ns += t0.elapsed_ns();
        }
    }
    false
}

pub fn pass_end(db: Oid, relids: &[Oid]) {
    crate::ledger::shadow_pass_end(db, relids);
}

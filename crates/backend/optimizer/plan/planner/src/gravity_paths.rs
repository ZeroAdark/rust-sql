
use types_error::PgResult;
use types_nodes::plannodes::GravityRewrite;
use types_nodes::{Node, NodeTag};
use types_pathnodes::{PathId, PathNode, RelId, AGGSPLIT_SIMPLE, AGG_HASHED, AGG_SORTED, AMFLAG_PGRCOLUMNAR, COMPARE_GT, COMPARE_LT};

use crate::run::PlannerRun;

const DEFAULT_LIMIT_BOUND: u64 = 100;


/// The census of a zone walk over `ranges` (one `(min, max)` per granule,
/// `min > max` = an all-null granule, never a survivor) for the first `n`
/// keys in the walk's direction. `None` when there are no keyed granules or
/// `n == 0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ZoneCensus {
    pub survivors: u64,
    pub keyed: u64,
    pub granules: u64,
}

/// `per_granule` is the rows a keyed granule is estimated to hand the walk (the scan's estimated rows over its
/// granules, at least 1). Two bounds on the n-th key, the nearer one taken: the n-th edge (each edge is a key, so n
/// keys lie at or before it: one key per granule, true of any layout), and the farthest far side (maxima ascending,
/// minima descending) of the first `ceil(n / per_granule)` granules in walk order, which hold n rows by the
/// estimate. A clustered layout's bound is then its first granule's far side, not its n-th edge: a LIMIT of more
/// than one key per granule no longer reads n granules. The bound is an estimate for the path's cost only;
/// the walk itself stops at the bound its rows prove.
pub(crate) fn zone_walk_census(ranges: &[(i64, i64)], n: u64, desc: bool, per_granule: u64) -> Option<ZoneCensus> {
    if n == 0 || ranges.is_empty() {
        return None;
    }
    let mut keyed: Vec<(i64, i64)> = ranges.iter().filter(|(lo, hi)| lo <= hi).map(|&(lo, hi)| if desc { (hi, lo) } else { (lo, hi) }).collect();
    if keyed.is_empty() {
        return None;
    }
    if desc {
        keyed.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    } else {
        keyed.sort_unstable_by_key(|k| k.0);
    }
    let nearer = |a: i64, b: i64| if desc { a.max(b) } else { a.min(b) };
    let farther = |a: i64, b: i64| if desc { a.min(b) } else { a.max(b) };
    let nth_edge = keyed[(n as usize).min(keyed.len()) - 1].0;
    let take = n.div_ceil(per_granule.max(1)).min(keyed.len() as u64) as usize;
    let far = keyed[..take].iter().map(|k| k.1).reduce(farther).expect("take >= 1");
    let bound = if take as u64 * per_granule.max(1) >= n { nearer(nth_edge, far) } else { nth_edge };
    let survivors = keyed.iter().filter(|k| if desc { k.0 >= bound } else { k.0 <= bound }).count() as u64;
    Some(ZoneCensus { survivors, keyed: keyed.len() as u64, granules: ranges.len() as u64 })
}

/// The census of a NULLS FIRST walk (PL-06's planner half). The walk keys every granule holding a NULL lowest, so it
/// reads all of them first and no bound skips one (hosts `granule_source` `nulls_first_walk_order`), then the keyed
/// granules [`zone_walk_census`] counts for the rows the NULLs leave (`n` less the scan's NULL rows, `scan_nulls`). The
/// all-null granules (`min > max`) are known from the zones; the NULLs they do not account for (`table_nulls`, the
/// statistics' null fraction of the relation's rows: 0 when no statistics say otherwise) are counted in the fewest keyed
/// granules of `rows` rows they fill, and the keyed census's survivors among the granules that hold none. The fewest,
/// not a uniform spread: the null fraction is a sample, and a sample a hair above the all-null granules' rows spread
/// over the keyed granules would hold a NULL in nearly every one and decline a walk that reads 9 of 128 granules
/// against a plain top 17x slower; a walk whose every granule does hold a NULL takes the plain claims at run time.
/// `None` when no granule is keyed.
pub(crate) fn nulls_first_census(ranges: &[(i64, i64)], n: u64, desc: bool, per_granule: u64, rows: f64, table_nulls: f64, scan_nulls: f64) -> Option<ZoneCensus> {
    if n == 0 || ranges.is_empty() {
        return None;
    }
    let granules = ranges.len() as u64;
    let all_null = ranges.iter().filter(|(lo, hi)| lo > hi).count() as u64;
    let keyed = granules - all_null;
    if keyed == 0 {
        return None;
    }
    let rows = if rows.is_finite() && rows >= 1.0 { rows } else { 1.0 };
    let known = |x: f64| if x.is_finite() && x > 0.0 { x } else { 0.0 };
    let (table_nulls, scan_nulls) = (known(table_nulls), known(scan_nulls));
    let spread = (table_nulls - all_null as f64 * rows).max(0.0);
    let holding = (spread / rows).ceil().min(keyed as f64);
    let left = n.saturating_sub(scan_nulls.floor() as u64);
    let walked = if left == 0 { 0 } else { zone_walk_census(ranges, left, desc, per_granule).map_or(0, |c| c.survivors) };
    let keyed_read = holding + walked as f64 * (1.0 - holding / keyed as f64);
    let survivors = (all_null + keyed_read.ceil() as u64).min(granules);
    Some(ZoneCensus { survivors, keyed, granules })
}

/// The rows a granule of `scan_id`'s relation hands a zone walk, estimated: the scan's rows (after its quals) over
/// its granules, at least 1.
fn rows_per_granule(run: &PlannerRun<'_>, scan_id: PathId, granules: u64) -> u64 {
    let rows = run.root.path(scan_id).base().rows;
    if rows.is_finite() && rows >= 1.0 { ((rows / granules.max(1) as f64).floor() as u64).max(1) } else { 1 }
}

pub(crate) fn two_pass_passes_cost(input_total: f64, input_rows: f64, cpu_operator_cost: f64) -> f64 {
    2.0 * (input_total + cpu_operator_cost * input_rows.max(0.0))
}

/// Item 5's candidate rows: the rows whose leading key repeats (`1 − ndv /
/// rows`, bounded) plus the `n` first keys' rows, at most every row.
pub(crate) fn two_pass_candidates(input_rows: f64, ndv: f64, n: f64) -> f64 {
    if input_rows <= 0.0 {
        return 0.0;
    }
    let repeat = (1.0 - ndv / input_rows).clamp(0.0, 1.0);
    (input_rows * repeat + n).min(input_rows).max(0.0)
}

const ANALYZE_ROWS_PER_TARGET: f64 = 300.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct KeySample {
    pub ndv: f64,
    pub rows: f64,
    pub mcv_keys: f64,
    pub mcv_freq: f64,
    pub sample: f64,
}

/// The share of a key's values seen once among keys whose counts follow a zero-truncated Poisson of mean `mean` (every
/// key seen at least once): `λ e^-λ / (1 - e^-λ)` where `λ / (1 - e^-λ) = mean`.
fn once_share(mean: f64) -> f64 {
    if !(mean > 1.0 + 1e-9) {
        return 1.0;
    }
    let (mut lo, mut hi) = (0.0f64, mean + 1.0);
    for _ in 0..100 {
        let mid = (lo + hi) / 2.0;
        if mid / -(-mid).exp_m1() < mean { lo = mid } else { hi = mid }
    }
    let l = (lo + hi) / 2.0;
    (l * (-l).exp() / -(-l).exp_m1()).clamp(0.0, 1.0)
}

/// OF-TG2-3: a count top's candidate rows as the run time admits them (lowerv4 topn raw.rs `admitted`: every key seen
/// twice, then the `n` first keys seen once): the rows less the keys seen once, plus `n`, at most every row.
/// The keys seen once come from what ANALYZE saw. Its ndistinct is Haas-Stokes over the sample (`n d / (n - f1 +
/// f1 n / N)`); inverted for the sample's values seen once `f1` (its other values outside the MCV list taken as seen
/// twice), it gives the keys outside the MCV list re-estimated on the sample's rows outside it, whose keys seen once
/// follow a zero-truncated Poisson of their mean count. So keys that all repeat (two rows each: ndistinct 0.98 M of 2 M
/// rows) admit four fifths of the rows where [`two_pass_candidates`] reads half, and a heavy MCV head over a unique tail
/// (D2078's k: 35 MCVs, the rest unique, ndistinct read as 1.21 M for ~1.98 M) a fifth where it reads two fifths. A
/// sample whose values repeat more than twice (ndistinct under the all-pairs reading) keeps PostgreSQL's ndistinct. The
/// share is the table's, applied to the scan's `input_rows`. The caller takes the larger of this and the repeats'
/// reading: the sample's size is an assumption (a tiny sampling rate sees a key of a few rows once, so a column of a few
/// rows per key reads as unique here), so the tail only adds candidates, never removes them.
pub(crate) fn count_candidates(input_rows: f64, n: f64, s: KeySample) -> f64 {
    if input_rows <= 0.0 {
        return 0.0;
    }
    let rows = s.rows.max(1.0);
    let ndv = s.ndv.clamp(1.0, rows);
    let f = s.mcv_freq.clamp(0.0, 0.999);
    let k = s.mcv_keys.clamp(0.0, ndv);
    let ns = s.sample.clamp(1.0, rows);
    let (nt, tail_rows) = (ns * (1.0 - f), rows * (1.0 - f));
    // Haas-Stokes as ANALYZE computes it, the sample's tail holding f1 values seen once and the rest seen twice: it
    // grows with f1 (the numerator grows, the denominator falls since ns <= rows)
    let hs = |f1: f64| ns * (k + f1 + (nt - f1) / 2.0) / (ns - f1 + f1 * ns / rows);
    let tail_ndv = if hs(0.0) >= ndv {
        ndv - k
    } else {
        let f1 = if hs(nt) <= ndv {
            nt
        } else {
            let (mut lo, mut hi) = (0.0f64, nt);
            for _ in 0..100 {
                let mid = (lo + hi) / 2.0;
                if hs(mid) < ndv { lo = mid } else { hi = mid }
            }
            lo
        };
        let seen = f1 + (nt - f1) / 2.0;
        nt * seen / (nt - f1 + f1 * nt / tail_rows).max(f64::MIN_POSITIVE)
    }
    .clamp(1.0, tail_rows.max(1.0));
    let once = tail_ndv * once_share(tail_rows / tail_ndv);
    (input_rows * (1.0 - once / rows) + n).clamp(0.0, input_rows)
}

/// What a grouped top costs Gravity, one pass against two, in PostgreSQL's units: the inputs [`top_costs`] reads.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TopCostIn {
    pub scan: f64,
    pub rows: f64,
    pub groups: f64,
    pub keys: f64,
    pub cand: f64,
    pub cand_groups: f64,
    /// The top's bound rank: its LIMIT plus its OFFSET (the candidates' `n`).
    pub n: f64,
    pub fold_row: f64,
    pub fold_group: f64,
    pub merge_row: f64,
    pub cpu_op: f64,
    pub pass1_record: f64,
    pub hash_mem: f64,
    pub key_corr: Option<f64>,
    /// `pgrust.gravity_grant_mb` in bytes when set, else 0: exec's grant as a whole, which
    /// replaces the participants' hash memory as pass 1's room.
    pub grant: f64,
    pub width: f64,
    pub group_bytes: f64,
    pub t1_record: f64,
    pub fold_key: f64,
    /// The groups a table can meet: the grouping columns' ndistinct multiplied, at most the rows and at least `groups`
    /// (`groups` is PostgreSQL's estimate, clamped to a tenth of the rows over two columns).
    pub groups_met: f64,
    /// at width 0, PostgreSQL's hashed-Agg spill price per input row (costsize `hashed_agg_spill_surcharge`'s
    /// total over the plain Agg's groups and rows: the partitions' pages written and read, the tuples spilled at its
    /// depth), charged to a form whose table Gravity's room does not hold; 0 under `pgrust.gravity_twopass_w0_spill = off`.
    pub w0_spill_row: f64,
    /// the bytes a group keeps past its record for its by-reference transition states (a text / numeric min or
    /// max keeps its value's copy: the argument column's average width, else its type's), as PostgreSQL's
    /// `transitionSpace` counts them; `internal` states are Gravity's fixed slots and count nothing.
    pub byref_state_bytes: f64,
}

impl TopCostIn {
    /// Pass 1's table fits Gravity's grant (else it spills and pass 2 admits every key, D459): its keys times its
    /// record (a hashed table filled to 7/8) against `pgrust.gravity_grant_mb` when set, else the participants' hash
    /// memory together. An extreme top's (`key_corr`) is also read per participant, as the run time spills it: ONE
    /// participant's phase-1 table spills when its pool fills ([`cost_profile::hash::t1_pool_records`] at its share: a
    /// flush comes only then) and, folded by key, keeps more than 3/4 of it
    /// ([`cost_profile::hash::t1_fold_records`]). A scattered key's rows each reach the pool once its values outnumber
    /// the slots, and a pool-full of them meets its whole set; a clustered key's rows are together, so each of the
    /// participant's share of its values reaches the pool once and the fold has nothing to fold. The correlation's
    /// square weighs the two (as `cost_index` weighs an index's order).
    /// TRACED (OF-TG2-1, phase-1 counters per participant and table, 2 M rows, an 8 MiB share; W8 and W4, a 32-byte
    /// record: 243 k pool records, 182 k kept): 150 k scattered keys folded on every participant; at 200 k and 300 k the
    /// participants whose window met the whole set spilled, and one is enough (pass 2 then admits every key). D2078's g
    /// tops (333 k clustered, a 40-byte record: 200 k pool records, 150 k kept) at W2: 166 k a participant never filled
    /// the pool (no pass-1 flush in 6 of 8 steady runs; the program dump puts pass 1 on the report's slot). The pass-1 fit
    /// agrees on every cell: 60-150 k scattered held and 200-500 k spilled at W4 and W8; clustered 500 k held at W4, 1 M
    /// held at W8 and spilled at W4, 2 M spilled at W8; D2078's h top (200 k scattered) spills.
    pub(crate) fn pass1_holds(&self) -> bool {
        let parts = self.width.max(1.0);
        let together = if self.grant > 0.0 { self.grant } else { self.hash_mem * parts };
        let held = self.keys * self.pass1_record * 8.0 / 7.0 <= together;
        let Some(corr) = self.key_corr else { return held };
        let (room, rec) = ((if self.grant > 0.0 { self.grant / parts } else { self.hash_mem }) as u64, self.pass1_record.ceil() as u64);
        let (keys, c2) = (self.keys.min(self.rows), (corr * corr).clamp(0.0, 1.0));
        let inflow = (1.0 - c2) * self.rows / parts + c2 * keys / parts;
        let window = (1.0 - c2) * keys + c2 * keys / parts;
        held && (inflow <= cost_profile::hash::t1_pool_records(room, rec, 0, cost_profile::machine::host()) as f64 || window <= cost_profile::hash::t1_fold_records(room, rec, 0, cost_profile::machine::host()) as f64)
    }

    fn window(&self, rows: f64, groups: f64) -> f64 {
        let (parts, c2) = (self.width.max(1.0), self.key_corr.map_or(0.0, |c| (c * c).clamp(0.0, 1.0)));
        let groups = groups.max(1.0);
        c2 * groups / parts + (1.0 - c2) * groups * (1.0 - (-rows / parts / groups).exp())
    }

    /// The one pass's partial groups fit each participant's room (`pgrust.gravity_grant_mb` over the participants when
    /// set, else its hash memory): a participant holds at most its share of the rows' groups, `min(groups, rows /
    /// participants)`. Past it the one pass spills its partials and reads them back. A count top's reading above width 0.
    pub(crate) fn one_pass_holds(&self) -> bool {
        let parts = self.width.max(1.0);
        let room = if self.grant > 0.0 { self.grant / parts } else { self.hash_mem };
        self.groups.min(self.rows / parts) * self.group_bytes <= room
    }

    pub(crate) fn group_table_holds(&self) -> bool {
        let parts = self.width.max(1.0);
        let room = (if self.grant > 0.0 { self.grant / parts } else { self.hash_mem }) as u64;
        self.window(self.rows, self.groups_met) <= cost_profile::hash::t1_fold_records(room, self.t1_record.ceil() as u64, self.groups as u64, cost_profile::machine::host()) as f64
    }

    pub(crate) fn one_table_holds(&self) -> bool {
        let room = if self.grant > 0.0 { self.grant } else { self.hash_mem };
        (self.width > 0.0 || self.groups.min(self.rows) * (self.group_bytes + self.byref_state_bytes) <= room)
            && if self.key_corr.is_some() { self.group_table_holds() } else { self.one_pass_holds() }
    }

    pub(crate) fn pass1_spill_mib(&self) -> f64 {
        if self.pass1_holds() {
            return 0.0;
        }
        self.keys * self.pass1_record * 8.0 / 7.0 / (1u64 << 20) as f64
    }

    pub(crate) fn one_spill_mib(&self) -> f64 {
        if self.one_table_holds() {
            return 0.0;
        }
        let bytes = if self.width > 0.0 {
            self.width * self.groups.min(self.rows / self.width) * self.group_bytes
        } else {
            self.groups.min(self.rows) * (self.group_bytes + self.byref_state_bytes)
        };
        bytes / (1u64 << 20) as f64
    }

    /// An extreme top above width 0 whose key is scattered (its correlation's square under 1/2) and whose one pass holds
    /// its groups: every participant meets nearly every group and keeps it, so pass 1 meets every key in every
    /// participant as the one pass meets every group, and the two passes add a second read and a probe a row for one
    /// transition's saving. Measured slower at every width where the weights read the two forms near a tie (the fit
    /// set's 60 k scattered keys: x1.07 / x1.36 / x1.35 at W2 / W4 / W8; tq's 300 k at W8: x1.28 / x1.03), and the
    /// planner's Sort over the one pass's groups would tip such a tie to the two passes; so the two passes are not built
    /// for it.
    pub(crate) fn scattered_one_pass_holds(&self) -> bool {
        self.width > 0.0 && self.key_corr.is_some_and(|r| r * r < 0.5) && self.group_table_holds()
    }

    fn pass1_merge(&self) -> f64 {
        let combines = if self.key_corr.is_some() { 3.0 } else { 2.0 };
        combines * self.cpu_op * self.window(self.rows, self.keys.min(self.rows))
    }
}

/// OF-TG2-1: top_costs' weights for an extreme top above width 0 on the planner's own terms, the read's weight 1. FITTED
/// by least squares on log(two / one) (a grid's choice; a finer grid finds 4 % less loss at the same picks) over the 63
/// extreme pairs of tests::gravity_paths `OF_TG2_1_FIT` (D2078's tph
/// tops, the scattered tq tops, the g tops' extreme and direction varied, the pass-1 fit set's scattered and clustered
/// min(w) tops; W2 / W4 / W8), the scattered held cells declined ([`TopCostIn::scattered_one_pass_holds`]): 2 picks
/// wrong outside a 5 % tie, both the one pass where the two passes ran x0.77 / x0.94 (the fit set's 90 k scattered keys
/// at W2 / W8, whose one pass spills a little before the fold room reads it). The
/// table's 5 count-top pairs are read by PL-07 and take the faster form. A Gravity fold costs far more against its scan
/// than PostgreSQL's units say: the group key's probe, each transition, the owners' merge of the partials and a spilled
/// record's write and refill; the owners rank the groups they finish (the top's offer in the owner phase), at
/// PostgreSQL's price split across them.
pub(crate) const TOP_W_HASH: f64 = 8.0;
pub(crate) const TOP_W_KEY: f64 = 2.0;
pub(crate) const TOP_W_TRANS: f64 = 12.0;
pub(crate) const TOP_W_MERGE: f64 = 32.0;
pub(crate) const TOP_W_SPILL: f64 = 12.0;
pub(crate) const TOP_W_PASS1: f64 = 28.0;
pub(crate) const TOP_W_FINISH: f64 = 1.0;

pub(crate) fn top_costs(c: &TopCostIn) -> (f64, f64) {
    let held = c.pass1_holds();
    let (cand, cand_groups) = if held { (c.cand.min(c.rows), c.cand_groups.min(c.groups)) } else { (c.rows, c.groups) };
    if c.width <= 0.0 {
        // the leader's one table spills past its room as PostgreSQL's hashed Agg does (an extreme top's by the
        // traced fold room, a count top's by its partial groups); pass 2 spills when its candidate groups do not fit
        let room = if c.grant > 0.0 { c.grant } else { c.hash_mem };
        let one_holds = c.groups.min(c.rows) * (c.group_bytes + c.byref_state_bytes) <= room && if c.key_corr.is_some() { c.group_table_holds() } else { c.one_pass_holds() };
        let one_spill = if one_holds { 0.0 } else { c.w0_spill_row * c.rows };
        let two_spill = if cand_groups.min(cand) * (c.group_bytes + c.byref_state_bytes) <= room { 0.0 } else { c.w0_spill_row * cand };
        let one = c.scan + c.fold_row * c.rows + c.fold_group * c.groups + one_spill;
        let pass1_rank = 2.0 * c.cpu_op * c.keys.min(c.rows) * (2.0 * c.n.max(1.0)).log2();
        let two = two_pass_passes_cost(c.scan, c.rows, c.cpu_op) + c.fold_row * cand + pass1_rank + c.fold_group * cand_groups + two_spill;
        return (one, two);
    }
    if c.key_corr.is_none() {
        let parts = c.width;
        let merged = if c.one_pass_holds() { parts } else { 1.0 };
        let merge = |partials: f64| c.merge_row * partials / merged;
        let one = (c.scan + c.fold_row * c.rows) / parts + merge((c.groups * parts).min(c.rows)) + c.fold_group * c.groups;
        let pass1_merge = c.cpu_op * (c.keys * parts).min(c.rows) / parts;
        let two = (two_pass_passes_cost(c.scan, c.rows, c.cpu_op) + c.fold_row * cand) / parts + pass1_merge + merge((cand_groups * parts).min(cand)) + c.fold_group * cand_groups;
        return (one, two);
    }
    let parts = c.width;
    let per_row = TOP_W_KEY * c.fold_key + TOP_W_TRANS * (c.fold_row - c.fold_key).max(0.0);
    let one_spill = if c.group_table_holds() { 0.0 } else { TOP_W_SPILL * c.merge_row * c.rows / parts };
    let one_fold = per_row * c.rows / parts + TOP_W_MERGE * c.fold_key * c.window(c.rows, c.groups_met) + one_spill;
    let one = c.scan / parts + one_fold + TOP_W_FINISH * c.fold_group * c.groups / parts;
    let pass2 = if held { per_row * cand / parts + TOP_W_MERGE * c.fold_key * c.window(cand, c.groups_met * cand_groups / c.groups.max(1.0)) } else { one_fold };
    let two = 2.0 * c.scan / parts + TOP_W_HASH * 2.0 * c.cpu_op * c.rows / parts + TOP_W_PASS1 * c.pass1_merge() + pass2 + TOP_W_FINISH * c.fold_group * cand_groups / parts;
    (one, two)
}

pub(crate) struct TopShape {
    pub lead_keys: f64,
    pub lead_spread: Option<f64>,
    pub key_spread: Option<f64>,
}

/// SPILL-TP1's off switch `pgrust.gravity_twopass_spill_priced` (on unless set `off` / `false` / `0`): a top whose pass 1
/// or one pass outgrows its room is priced by the measured spill rows (`cost_profile::top::forms`) and the two passes'
/// rows are their candidates' (a spilled pass 1 still names its exact set, D3158); off, today's reading (a spilled pass 1
/// admits every key, D459, and has no measured price). Only where the measured profile prices the forms (its corpus'
/// input sizes): the fitted reading has no measured spill and keeps D459.
pub(crate) fn twopass_spill_priced() -> bool {
    !pgrust_guc("pgrust.gravity_twopass_spill_priced").is_some_and(|v| ["off", "false", "0", "no"].iter().any(|o| v.trim().eq_ignore_ascii_case(o)))
}

pub(crate) fn top_costs_priced(c: &TopCostIn, shape: &TopShape) -> (f64, f64, bool) {
    use cost_profile::walk::Profile;
    #[cfg(test)]
    let setting = TEST_TOP_PROFILE.lock().unwrap_or_else(|e| e.into_inner()).map(str::to_owned).or_else(|| pgrust_guc("pgrust.gravity_cost_profile"));
    #[cfg(not(test))]
    let setting = pgrust_guc("pgrust.gravity_cost_profile");
    if Profile::parse(setting.as_deref()) == Profile::Measured {
        let forms = cost_profile::top::TopForms {
            rows: c.rows,
            groups: c.groups,
            lead_keys: shape.lead_keys,
            lead_spread: shape.lead_spread,
            keys: c.keys,
            key_spread: shape.key_spread,
            cand: c.cand,
            n: c.n,
            extreme: c.key_corr.is_some(),
            one_holds: c.one_table_holds(),
            pass1_holds: c.pass1_holds(),
            spill_priced: twopass_spill_priced(),
            one_spill_mib: c.one_spill_mib(),
            pass1_spill_mib: c.pass1_spill_mib(),
            width: c.width.max(0.0) as u32,
        };
        if let Some((one, two)) = cost_profile::top::forms(&forms) {
            return (one.0, two.0, true);
        }
    }
    let (one, two) = top_costs(c);
    (one, two, false)
}

pub(crate) fn group_record_bytes(keys: usize, aggs: usize) -> f64 {
    ((keys + aggs + 1) * 8) as f64 * 8.0 / 7.0
}

pub(crate) fn phase1_record_bytes(keys: usize, aggs: usize, nullable_args: usize) -> f64 {
    ((keys + aggs + 1 + nullable_args) * 8) as f64
}

pub(crate) fn pass1_record_bytes(key_nullable: bool) -> f64 {
    let words = 1 + usize::from(key_nullable) + 1 + 1;
    (words * 8) as f64
}

/// An extreme top's pass-1 record, in bytes, as decompose lays it (agg `pass1`, abi `RecordLayout`): the key image (a
/// NULL flag word too when nullable), the count, the extreme, its column's non-NULL count when that column is
/// nullable, and the validity word a min / max slot takes; [`TopCostIn::pass1_holds`] sizes its table's slots.
pub(crate) fn pass1_extreme_record_bytes(key_nullable: bool, arg_nullable: bool, spread: bool) -> f64 {
    let words = 1 + usize::from(key_nullable) + 1 + 1 + usize::from(arg_nullable) + usize::from(spread) + 1;
    (words * 8) as f64
}

fn walk_price() -> cost_profile::walk::Walk {
    cost_profile::walk::Walk::of(cost_profile::walk::Profile::parse(pgrust_guc("pgrust.gravity_cost_profile").as_deref()))
}

/// A `pgrust.*` placeholder GUC's text, `None` when it was never set (or no GUC boot is installed: a unit-test binary):
/// the planner reaches the custom options only through this seam, as guc_tables' gather_fair.rs does.
fn pgrust_guc(name: &str) -> Option<String> {
    if !guc_seams::get_config_option_missing_ok::is_installed() {
        return None;
    }
    guc_seams::get_config_option_missing_ok::call(name).ok().flatten()
}

pub(crate) fn spread_candidates(input_rows: f64, ndv: f64, n: f64) -> f64 {
    if input_rows <= 0.0 {
        return 0.0;
    }
    let ndv = ndv.max(1.0);
    let m = (input_rows / ndv).max(1.0);
    let t = 1.0 - (n / ndv).min(1.0).powf(1.0 / m);
    (input_rows * (1.0 - t.powf(m))).max(n).min(input_rows)
}

/// An extreme top's candidate rows: the rows of the `n` keys whose extremes rank first (`n / ndv` of the rows), at
/// least `n` rows, at most every row.
pub(crate) fn extreme_candidates(input_rows: f64, ndv: f64, n: f64) -> f64 {
    if input_rows <= 0.0 {
        return 0.0;
    }
    (input_rows * (n / ndv.max(1.0))).max(n).min(input_rows)
}

pub(crate) fn zone_word_type(typid: types_core::Oid) -> PgResult<bool> {
    let (typlen, byval) = lsyscache::get_typlenbyval(typid)?;
    Ok(gravity_fn::typesem::keysem::word_orders_as_i64(typid, byval, typlen))
}

pub(crate) fn default_btree_order(pk: &types_pathnodes::PathKey, typid: types_core::Oid) -> PgResult<bool> {
    let tc = typcache::lookup_type_cache(typid, typcache::TYPECACHE_BTREE_OPFAMILY)?;
    Ok(tc.btree_opf() != 0 && tc.btree_opf() == pk.pk_opfamily)
}

pub(crate) const ORDER_FREE_AGGS: [&str; 29] = [
    "count", "sum", "avg", "min", "max", "bool_and", "bool_or", "every", "bit_and", "bit_or", "bit_xor", "stddev",
    "stddev_pop", "stddev_samp", "variance", "var_pop", "var_samp", "corr", "covar_pop", "covar_samp", "regr_avgx",
    "regr_avgy", "regr_count", "regr_intercept", "regr_r2", "regr_slope", "regr_sxx", "regr_sxy", "regr_syy",
];

/// C's FirstNormalObjectId: OIDs below it are the bootstrap and initdb objects; a user's aggregate is never order-free
/// here, whatever its name.
const FIRST_NORMAL_OBJECT_ID: types_core::Oid = 16384;

pub(crate) fn order_insensitive_agg(fnoid: types_core::Oid) -> PgResult<bool> {
    if fnoid >= FIRST_NORMAL_OBJECT_ID {
        return Ok(false);
    }
    let name = syscache_seams::pg_proc_proname::call(fnoid)?;
    Ok(name.is_some_and(|n| ORDER_FREE_AGGS.iter().any(|a| a.as_bytes() == n.name_str())))
}

/// Item 5's amendment 2: an aggregate argument that cannot raise on a row
/// pass 2 never evaluates — a `Var`, a `Const`, or a `RelabelType` of one.
pub(crate) fn simple_agg_arg(node: Node<'_>) -> bool {
    if node.as_var().is_some() || node.as_const().is_some() {
        return true;
    }
    match node.as_relabel_type() {
        Some(r) => r.arg.as_var().is_some() || r.arg.as_const().is_some(),
        None => false,
    }
}

/// pass 2 projects only the admitted groups, but PostgreSQL's Agg projects every group before its Sort, so each
/// entry of `target` must raise nothing: a `Var`, a `Const`, an `Aggref` (its arguments are `simple_agg_arg`s and a
/// never-admitted group has one row, so no transition runs twice), a `RelabelType` of one, or a grouping expression
/// (`groups`: its value comes from the input row, which both passes compute).
fn quiet_target(run: &PlannerRun<'_>, target: types_pathnodes::PtId, groups: &[u32]) -> bool {
    let t = run.root.pathtarget(target);
    t.exprs.iter().enumerate().all(|(i, &id)| {
        let mut e = *run.root.expr_node(id);
        while let Some(r) = e.as_relabel_type() {
            e = r.arg;
        }
        e.as_var().is_some() || e.as_const().is_some() || e.as_aggref().is_some() || t.sortgrouprefs.get(i).is_some_and(|r| *r != 0 && groups.contains(r))
    })
}


#[cfg(test)]
pub(crate) struct TestFacts {
    pub granules: u64,
    pub zones: Vec<(i16, Vec<(i64, i64)>)>,
}

#[cfg(test)]
pub(crate) static TEST_FACTS: std::sync::Mutex<Option<TestFacts>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) static TEST_NULL_FRAC: std::sync::Mutex<Option<(i16, f64)>> = std::sync::Mutex::new(None);

fn rel_granules(run: &PlannerRun<'_>, rel: RelId) -> Option<u64> {
    #[cfg(test)]
    if let Some(f) = TEST_FACTS.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return Some(f.granules);
    }
    let r = run.root.rel(rel);
    (r.amflags & AMFLAG_PGRCOLUMNAR != 0 && r.pgrcolumnar_granules > 0).then_some(r.pgrcolumnar_granules)
}

fn zone_ranges<'mcx>(run: &PlannerRun<'mcx>, rel: RelId, attno: i16) -> Option<std::borrow::Cow<'mcx, [(i64, i64)]>> {
    #[cfg(test)]
    if let Some(f) = TEST_FACTS.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return f.zones.iter().find(|(a, _)| *a == attno).map(|(_, z)| std::borrow::Cow::Owned(z.clone()));
    }
    run.root.rel(rel).pgrcolumnar_zones.iter().find(|z| z.attno == attno && !z.ranges.is_empty()).map(|z| std::borrow::Cow::Borrowed(z.ranges))
}

#[cfg(test)]
pub(crate) static TEST_TOP_PROFILE: std::sync::Mutex<Option<&'static str>> = std::sync::Mutex::new(None);
#[cfg(test)]
pub(crate) static LAST_DECLINE: std::sync::Mutex<Option<&'static str>> = std::sync::Mutex::new(None);

fn decline(reason: &'static str) -> PgResult<()> {
    #[cfg(test)]
    {
        *LAST_DECLINE.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
    }
    let _ = reason;
    Ok(())
}


/// The LIMIT bound `n = count + offset` from `preprocess_limit`'s estimates:
/// `None` for `LIMIT 0` or a limit option other than COUNT; a non-constant
/// count is priced at `DEFAULT_LIMIT_BOUND`.
fn limit_bound(run: &PlannerRun<'_>, offset_est: i64, count_est: i64) -> Option<u64> {
    let parse = run.parse();
    if parse.limitOption != types_nodes::nodes_enums::LimitOption::LIMIT_OPTION_COUNT || parse.limitCount.is_none() {
        return None;
    }
    let count = if count_est > 0 { count_est as u64 } else if count_est < 0 { DEFAULT_LIMIT_BOUND } else { return None };
    let offset = if offset_est > 0 { offset_est as u64 } else { 0 };
    Some(count.saturating_add(offset))
}

/// The null fraction of `rel`'s column `attno` (the scan Var `var`): 0 for a NOT NULL column; else the pgrcolumnar
/// facts' exact one (NEW-stats-2: the parts' Stats non-null sums under the folded row total, read at plan time, so it
/// holds for a table appended to since its ANALYZE or never analyzed; a cost input only, the run-time walk re-checks);
/// else the statistics' `stanullfrac` (pgrcolumnar's ANALYZE fold takes it from the same records), else 0 (no
/// statistics: the NULLS FIRST census counts the all-null granules only).
fn key_null_fraction<'mcx>(run: &mut PlannerRun<'mcx>, rel: RelId, attno: i16, var: Node<'mcx>) -> PgResult<f64> {
    if crate::relnode::relids_is_member(attno as i32, &run.root.rel(rel).notnullattnums) {
        return Ok(0.0);
    }
    #[cfg(test)]
    let facts = TEST_NULL_FRAC.lock().unwrap_or_else(|e| e.into_inner()).filter(|t| t.0 == attno).map(|t| t.1);
    #[cfg(not(test))]
    let facts = None;
    let facts = facts.or_else(|| attno.checked_sub(1).and_then(|i| run.root.rel(rel).pgrcolumnar_col_null_frac.get(i as usize).copied().flatten()));
    if let Some(f) = facts {
        return Ok(if f.is_finite() { f.clamp(0.0, 1.0) } else { 0.0 });
    }
    let vid = run.intern_expr(var);
    let vardata = crate::selfuncs::examine_variable(run, vid, var, 0)?;
    let f = vardata.nullfrac();
    Ok(if f.is_finite() { f.clamp(0.0, 1.0) } else { 0.0 })
}

fn scan_correlation(run: &PlannerRun<'_>, relid: i32, attno: i16) -> PgResult<Option<f64>> {
    let rte = run.rte(relid as usize);
    let Some(bundle) = crate::selfuncs::get_att_stats(run, rte.relid, attno, rte.inh)? else { return Ok(None) };
    let Some(slot) = bundle.slots.iter().find(|sl| sl.kind == crate::selfuncs::STATISTIC_KIND_CORRELATION) else { return Ok(None) };
    Ok(slot.numbers()?.first().map(|&c| f64::from(c).clamp(-1.0, 1.0)))
}

/// What ANALYZE read of range-table index `relid`'s column `attno` ([`KeySample`]): pg_statistic's ndistinct (a
/// negative one is a share of the relation's rows) and MCV list, the relation's rows, and the sample's rows (300 x the
/// column's statistics target, else `default_statistics_target`); `None` without statistics or rows.
fn key_sample(run: &PlannerRun<'_>, relid: i32, rel: RelId, attno: i16) -> PgResult<Option<KeySample>> {
    let rte = run.rte(relid as usize);
    let Some(bundle) = crate::selfuncs::get_att_stats(run, rte.relid, attno, rte.inh)? else { return Ok(None) };
    let rows = run.root.rel(rel).tuples;
    let stadistinct = f64::from(bundle.stadistinct);
    let ndv = if stadistinct < 0.0 { -stadistinct * rows } else { stadistinct };
    if !(rows >= 1.0) || !(ndv >= 1.0) {
        return Ok(None);
    }
    let (mut mcv_keys, mut mcv_freq) = (0.0, 0.0);
    if let Some(slot) = bundle.slots.iter().find(|sl| sl.kind == crate::selfuncs::STATISTIC_KIND_MCV) {
        let freqs = slot.numbers()?;
        mcv_keys = freqs.len() as f64;
        mcv_freq = freqs.iter().map(|&x| f64::from(x)).sum::<f64>();
    }
    let column = if syscache_seams::lookup_pg_attribute_stattarget::is_installed() { syscache_seams::lookup_pg_attribute_stattarget::call(rte.relid, attno)? } else { None };
    let target = match column {
        Some(t) if t >= 0 => f64::from(t),
        _ if guc_tables::vars::default_statistics_target.installed() => f64::from(guc_tables::vars::default_statistics_target.read()),
        _ => 100.0,
    };
    Ok(Some(KeySample { ndv, rows, mcv_keys, mcv_freq, sample: (ANALYZE_ROWS_PER_TARGET * target).max(1.0) }))
}

fn peel_projection(run: &PlannerRun<'_>, path_id: PathId) -> (Option<(RelId, types_pathnodes::PtId)>, PathId) {
    if let PathNode::ProjectionPath(pp) = run.root.path(path_id) {
        return (Some((pp.path.parent, pp.path.pathtarget_id.expect("projection path has a target"))), pp.subpath.expect("ProjectionPath has a subpath"));
    }
    (None, path_id)
}

struct FactsScan {
    rel: RelId,
    relid: i32,
    granules: u64,
    scan: PathId,
    proj: Option<(RelId, types_pathnodes::PtId)>,
}

fn facts_scan(run: &PlannerRun<'_>, input: PathId) -> Option<FactsScan> {
    let (proj, scan) = peel_projection(run, input);
    let rel = plain_scan_rel(run, scan)?;
    let granules = rel_granules(run, rel)?;
    Some(FactsScan { rel, relid: run.root.rel(rel).relid as i32, granules, scan, proj })
}

fn plain_scan_rel(run: &PlannerRun<'_>, scan: PathId) -> Option<RelId> {
    let b = run.root.path(scan).base();
    (b.pathtype == types_pathnodes::tag16(NodeTag::T_SeqScan) && matches!(run.root.path(scan), PathNode::Path(_)) && b.param_info.is_none()).then_some(b.parent)
}

fn scan_volatile(run: &PlannerRun<'_>, input: PathId) -> PgResult<bool> {
    let (_, scan) = peel_projection(run, input);
    let Some(rel) = plain_scan_rel(run, scan) else { return Ok(true) };
    for &rid in run.root.rel(rel).baserestrictinfo.iter() {
        if clauses::contain_volatile_functions(*run.root.expr_node(run.root.rinfo(rid).clause))? {
            return Ok(true);
        }
    }
    let target = run.root.path(scan).base().pathtarget_id.expect("scan path has a target");
    for &id in run.root.pathtarget(target).exprs.iter() {
        if clauses::contain_volatile_functions(*run.root.expr_node(id))? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn scan_gated(run: &PlannerRun<'_>, input: PathId) -> bool {
    let (_, scan) = peel_projection(run, input);
    plain_scan_rel(run, scan).is_none_or(|rel| run.root.rel(rel).baserestrictinfo.iter().any(|&rid| run.root.rinfo(rid).pseudoconstant))
}

fn plain_scan_relid(run: &PlannerRun<'_>, input: PathId) -> Option<i32> {
    let (_, scan) = peel_projection(run, input);
    plain_scan_rel(run, scan).map(|rel| run.root.rel(rel).relid as i32)
}

fn scan_var(node: Node<'_>, relid: i32) -> Option<(i16, types_core::Oid)> {
    let v = node.as_var()?;
    (v.varno == relid && v.varlevelsup == 0 && v.varattno >= 1).then_some((v.varattno, v.vartype))
}

fn pathkey_has<'mcx>(run: &PlannerRun<'mcx>, pk: &types_pathnodes::PathKey, mut pred: impl FnMut(Node<'mcx>) -> bool) -> bool {
    let Some(ec) = pk.pk_eclass else { return false };
    run.root.ec(ec).ec_members.iter().any(|&m| pred(*run.root.expr_node(run.root.em(m).em_expr)))
}

fn pathkey_order(pk: &types_pathnodes::PathKey) -> Option<(bool, bool)> {
    match pk.pk_cmptype {
        COMPARE_LT => Some((false, pk.pk_nulls_first)),
        COMPARE_GT => Some((true, pk.pk_nulls_first)),
        _ => None,
    }
}

fn byref_state_bytes<'mcx>(run: &PlannerRun<'mcx>, target: types_pathnodes::PtId, relid: i32, rel: RelId) -> PgResult<f64> {
    struct Collect<'m>(Vec<&'m types_nodes::primnodes::Aggref<'m>>);
    impl<'m> nodes_core::NodeWalker<'m> for Collect<'m> {
        fn visit(&mut self, node: Node<'m>) -> PgResult<bool> {
            if let Some(a) = node.as_aggref() {
                self.0.push(a);
                return Ok(false);
            }
            nodes_core::expression_tree_walker(node, self)
        }
    }
    let mut c = Collect(Vec::new());
    let exprs: Vec<Node<'mcx>> = run.root.pathtarget(target).exprs.iter().map(|&id| *run.root.expr_node(id)).collect();
    for e in exprs {
        nodes_core::NodeWalker::visit(&mut c, e)?;
    }
    let r = run.root.rel(rel);
    let mut bytes = 0usize;
    for a in &c.0 {
        if a.aggtranstype == types_core::catalog::INTERNALOID || lsyscache::get_typlenbyval(a.aggtranstype)?.1 {
            continue;
        }
        let col = a.args.iter().next().and_then(|n| n.as_target_entry()).and_then(|te| scan_var(te.expr, relid)).map(|(c, _)| c);
        let stat = col.and_then(|c| usize::try_from(i32::from(c) - r.min_attr as i32).ok()).and_then(|i| r.attr_widths.get(i).copied()).filter(|&w| w > 0);
        let width = match stat {
            Some(w) => w as usize,
            None => lsyscache::get_typavgwidth(a.aggtranstype, -1)?.max(0) as usize,
        };
        bytes += ((width + 7) & !7) + 2 * 8;
    }
    Ok(bytes as f64)
}

fn nullable_arg_aggregates<'mcx>(run: &PlannerRun<'mcx>, target: types_pathnodes::PtId, relid: i32, rel: RelId) -> PgResult<usize> {
    struct Collect<'m>(Vec<&'m types_nodes::primnodes::Aggref<'m>>);
    impl<'m> nodes_core::NodeWalker<'m> for Collect<'m> {
        fn visit(&mut self, node: Node<'m>) -> PgResult<bool> {
            if let Some(a) = node.as_aggref() {
                self.0.push(a);
                return Ok(false);
            }
            nodes_core::expression_tree_walker(node, self)
        }
    }
    let mut c = Collect(Vec::new());
    let exprs: Vec<Node<'mcx>> = run.root.pathtarget(target).exprs.iter().map(|&id| *run.root.expr_node(id)).collect();
    for e in exprs {
        nodes_core::NodeWalker::visit(&mut c, e)?;
    }
    let notnull = &run.root.rel(rel).notnullattnums;
    let mut n = 0;
    for a in &c.0 {
        for arg in &a.args {
            if arg.as_target_entry().and_then(|te| scan_var(te.expr, relid)).is_some_and(|(col, _)| !crate::relnode::relids_is_member(i32::from(col), notnull)) {
                n += 1;
                break;
            }
        }
    }
    Ok(n)
}

/// The Aggrefs of a path target's expressions (the Agg's output) and, for
/// each, whether it is admissible under the rewrites: order-insensitive, no
/// FILTER, no DISTINCT, no ORDER BY, not an ordered-set aggregate, and (with
/// `simple_args`) arguments that are `Var` / `Const` / `RelabelType` of those.
/// Returns `None` when an inadmissible aggregate is present, else the count
/// of aggregates and whether one of them is `count(*)`.
fn admissible_aggregates<'mcx>(run: &PlannerRun<'mcx>, target: types_pathnodes::PtId, simple_args: bool) -> PgResult<Option<(usize, bool)>> {
    struct Collect<'m>(Vec<&'m types_nodes::primnodes::Aggref<'m>>);
    impl<'m> nodes_core::NodeWalker<'m> for Collect<'m> {
        fn visit(&mut self, node: Node<'m>) -> PgResult<bool> {
            if let Some(a) = node.as_aggref() {
                self.0.push(a);
                return Ok(false);
            }
            nodes_core::expression_tree_walker(node, self)
        }
    }
    let mut c = Collect(Vec::new());
    let exprs: Vec<Node<'mcx>> = run.root.pathtarget(target).exprs.iter().map(|&id| *run.root.expr_node(id)).collect();
    for e in exprs {
        nodes_core::NodeWalker::visit(&mut c, e)?;
    }
    let mut has_count_star = false;
    for a in &c.0 {
        if !order_insensitive_agg(a.aggfnoid)? || a.aggfilter.is_some() || !a.aggdistinct.is_nil() || !a.aggorder.is_nil() || !a.aggdirectargs.is_nil() || a.agglevelsup != 0 || a.aggsplit != types_nodes::primnodes::AGGSPLIT_SIMPLE {
            return Ok(None);
        }
        if a.aggstar {
            has_count_star = true;
        }
        if simple_args {
            for arg in &a.args {
                let te = arg.as_target_entry().expect("Aggref.args cell");
                if !simple_agg_arg(te.expr) {
                    return Ok(None);
                }
            }
        }
    }
    Ok(Some((c.0.len(), has_count_star)))
}

/// The order a two-pass top's first sort key gives its groups, where pass 1 (one record per leading key) holds values
/// that bound every group of its key: `count(*)` DESC (a key's row count bounds its groups' counts, and a key seen once
/// is one group of count 1), or `max(v)` DESC / `min(v)` ASC of a scan column `v` (a key's extreme bounds its groups'
/// extremes, and the group holding the extreme's row attains it), or (gtopn GT-1, `spread`) `min(v)` DESC, `max(v)` ASC
/// or an integer `avg(v)` either way: every group's value lies between the key's min and max of `v`, so the key's
/// extreme on the order's worse side is reached by one of its groups (the witness) and the other is never bettered (the
/// best; decompose `Class::two_pass_bounds`). A sum's terms take either sign and a count of values is not bounded by
/// `v`'s extremes, so those keep one pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TopOrder {
    Count,
    Extreme(i16, bool),
}

fn top_order<'mcx>(run: &PlannerRun<'mcx>, pk: &types_pathnodes::PathKey, relid: i32) -> PgResult<Option<TopOrder>> {
    let Some((desc, _)) = pathkey_order(pk) else { return Ok(None) };
    let mut aggs: Vec<&'mcx types_nodes::primnodes::Aggref<'mcx>> = Vec::new();
    pathkey_has(run, pk, |e| {
        aggs.extend(e.as_aggref());
        false
    });
    for a in aggs {
        if a.aggfnoid >= FIRST_NORMAL_OBJECT_ID || a.aggfilter.is_some() || !a.aggdistinct.is_nil() || !a.aggorder.is_nil() || a.agglevelsup != 0 {
            continue;
        }
        let Some(name) = syscache_seams::pg_proc_proname::call(a.aggfnoid)? else { continue };
        let name = name.name_str();
        if a.aggstar && name == b"count" && desc {
            return Ok(Some(TopOrder::Count));
        }
        let (max, avg) = (name == b"max", name == b"avg");
        let spread = avg || ((max || name == b"min") && max != desc);
        if (max || avg || name == b"min") && a.args.len() == 1 && !(spread && !twopass_spread_on()) {
            let arg = a.args.nth(0).as_target_entry().map_or(a.args.nth(0), |t| t.expr);
            use types_core::catalog::{BOOLOID, INT2OID, INT4OID, INT8OID};
            if let Some((col, t)) = scan_var(arg, relid).filter(|&(_, t)| t != BOOLOID && (!avg || matches!(t, INT2OID | INT4OID | INT8OID))) {
                if zone_word_type(t)? && default_btree_order(pk, a.aggtype)? {
                    return Ok(Some(TopOrder::Extreme(col, spread)));
                }
            }
        }
    }
    Ok(None)
}

fn twopass_keys_any() -> bool {
    !pgrust_guc("pgrust.gravity_twopass_keys").is_some_and(|v| v.trim().eq_ignore_ascii_case("leading"))
}

/// `pgrust.gravity_twopass_spread` (GT-1's off switch; on unless `off`): whether min DESC / max ASC / avg tops may take
/// the two passes.
fn twopass_spread_on() -> bool {
    !pgrust_guc("pgrust.gravity_twopass_spread").is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "false" | "0"))
}

/// `pgrust.gravity_twopass_w0_spill` (D3026's off switch; on unless `off`): whether the width-0 price charges a spilling
/// form PostgreSQL's hashed-Agg spill.
fn twopass_w0_spill_on() -> bool {
    !pgrust_guc("pgrust.gravity_twopass_w0_spill").is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "false" | "0"))
}

/// `pgrust.gravity_hashagg_pg_spill` (D3142's off switch; off unless `on`): whether a hashed Agg on a query level that
/// reads a pgrcolumnar relation is charged PostgreSQL's hashed-Agg spill surcharge.
pub(crate) fn hashagg_pg_spill_on() -> bool {
    pgrust_guc("pgrust.gravity_hashagg_pg_spill").is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "on" | "true" | "1"))
}

fn leading_group_var(run: &PlannerRun<'_>, agg: &types_pathnodes::AggPath<'_>, relid: i32) -> Option<(i16, types_core::Oid)> {
    scan_var(leading_group_expr(run, agg)?, relid)
}

fn leading_group_expr<'mcx>(run: &PlannerRun<'mcx>, agg: &types_pathnodes::AggPath<'_>) -> Option<Node<'mcx>> {
    let sgref = run.root.expr_node(*agg.groupClause.first()?).as_sort_group_clause()?.tleSortGroupRef;
    run.parse().targetList.iter().find_map(|n| {
        let t = n.as_target_entry()?;
        (t.ressortgroupref == sgref).then_some(t.expr)
    })
}

pub(crate) fn zone_spread(ranges: &[(i64, i64)]) -> f64 {
    let (mut sum, mut keyed, mut lo, mut hi) = (0.0, 0u64, i64::MAX, i64::MIN);
    for &(a, b) in ranges.iter().filter(|(a, b)| a <= b) {
        (sum, keyed, lo, hi) = (sum + (b as f64 - a as f64), keyed + 1, lo.min(a), hi.max(b));
    }
    cost_profile::walk::key_spread(sum, keyed, lo, hi)
}

fn scan_over_granules(run: &mut PlannerRun<'_>, scan_id: PathId, rel: RelId, survivors: u64, granules: u64) -> PgResult<PathId> {
    let plain = run.root.path(scan_id).base().clone();
    let fraction = survivors as f64 / granules.max(1) as f64;
    let mut p = plain.clone();
    p.pathkeys = crate::relnode::pgvec_clone_shallow(run.mcx, &plain.pathkeys);
    let tuples = run.root.rel(rel).tuples;
    let rows_in_subset = (tuples * fraction).max(1.0);
    p.rows = plain.rows.min(rows_in_subset).max(1.0);
    match ::costsize::pgrc_scan_cost_over_granules(run, rel, scan_id, plain.rows, survivors)? {
        Some(priced) => {
            let target_per_tuple = run.root.path_pathtarget(scan_id).cost.per_tuple;
            p.total_cost = plain.startup_cost + priced.run + target_per_tuple * p.rows;
        }
        None => {
            p.total_cost = plain.startup_cost + (plain.total_cost - plain.startup_cost) * fraction;
        }
    }
    Ok(run.root.alloc_path(PathNode::Path(p)))
}

fn finish(run: &mut PlannerRun<'_>, final_rel: RelId, top: PathId, proj: Option<(RelId, types_pathnodes::PtId)>, offset_est: i64, count_est: i64) -> PgResult<()> {
    let mut top = top;
    if let Some((prel, target)) = proj {
        top = crate::pathnode::apply_projection_to_path(run, prel, top, target)?;
    }
    let parse = run.parse();
    let limited = crate::pathnode::create_limit_path(run, final_rel, top, parse.limitOffset, parse.limitCount, parse.limitOption, offset_est, count_est);
    crate::pathnode::add_path(run, final_rel, limited);
    Ok(())
}


pub(crate) fn add_gravity_paths<'mcx>(run: &mut PlannerRun<'mcx>, final_rel: RelId, pre_limit: PathId, offset_est: i64, count_est: i64, limit_tuples: f64) -> PgResult<()> {
    let parse = run.parse();
    if parse.commandType != types_nodes::CmdType::CMD_SELECT || !parse.rowMarks.is_nil() || !parse.groupingSets.is_nil() || parse.hasWindowFuncs || parse.hasTargetSRFs || !parse.distinctClause.is_nil() || parse.setOperations.is_some() {
        return Ok(());
    }
    let Some(n) = limit_bound(run, offset_est, count_est) else { return Ok(()) };
    add_zone_topk_path(run, final_rel, pre_limit, n, offset_est, count_est, limit_tuples)?;
    add_zone_walk_path(run, final_rel, pre_limit, n, offset_est, count_est, limit_tuples)?;
    add_two_pass_path(run, final_rel, pre_limit, n, offset_est, count_est, limit_tuples)?;
    Ok(())
}

fn walk_group_desc_on() -> bool {
    !pgrust_guc("pgrust.gravity_zonetopk").is_some_and(|v| v.trim().eq_ignore_ascii_case("asc"))
}

pub(crate) fn add_zone_topk_path<'mcx>(run: &mut PlannerRun<'mcx>, final_rel: RelId, pre_limit: PathId, n: u64, offset_est: i64, count_est: i64, limit_tuples: f64) -> PgResult<()> {
    let parse = run.parse();
    if parse.groupClause.is_nil() || parse.havingQual.is_some() || !crate::gucs::enable_hashagg() {
        return Ok(());
    }
    let ordered = !parse.sortClause.is_nil();
    let (proj, cur) = peel_projection(run, pre_limit);
    let (sort, inner_proj, agg_id) = if !ordered {
        (None, None, cur)
    } else {
        match run.root.path(cur) {
            PathNode::SortPath(sp) => {
                let (sort_rel, pathkeys) = (sp.path.parent, crate::relnode::pgvec_clone_shallow(run.mcx, &sp.path.pathkeys));
                let (inner, agg_id) = peel_projection(run, sp.subpath.expect("SortPath has a subpath"));
                (Some((sort_rel, pathkeys)), inner, agg_id)
            }
            PathNode::AggPath(ap) if ap.aggstrategy == AGG_SORTED => (Some((ap.path.parent, crate::relnode::pgvec_clone_shallow(run.mcx, &ap.path.pathkeys))), None, cur),
            _ => return decline("zone top-k: the ordered group is neither a Sort over its Agg nor a sorted Agg"),
        }
    };
    let mut agg = match run.root.path(agg_id) {
        PathNode::AggPath(ap) => ap.clone(),
        _ => return Ok(()),
    };
    if agg.gravity_rewrite.is_some() || !agg.qual.is_empty() || agg.groupClause.is_empty() || agg.aggsplit != AGGSPLIT_SIMPLE {
        return Ok(());
    }
    // The walk replaces the plain plan the planner would otherwise take under this Limit, so its price scales the cheapest
    // of the group's (or the ordered group's) paths as the Limit reads them (its startup and the share of its run the
    // first n groups take: create_limit_path's arithmetic): at a width that is the parallel plan, which Gravity runs at
    // that width as well
    let plain = run.root.rel(run.root.path(cur).base().parent).pathlist.iter().copied().filter(|&p| live(run, p)).chain([cur]).map(|p| limited_cost(run, p, n)).fold(f64::INFINITY, f64::min);
    let input_id = match agg.aggstrategy {
        AGG_HASHED => agg.subpath.expect("AggPath has a subpath"),
        AGG_SORTED if sort.is_some() && crate::grouping::grouping_is_hashable(run, &agg.groupClause) => {
            let mut below = agg.subpath.expect("AggPath has a subpath");
            loop {
                below = match run.root.path(below) {
                    PathNode::SortPath(sp) => sp.subpath.expect("SortPath has a subpath"),
                    PathNode::IncrementalSortPath(sp) => sp.spath.subpath.expect("IncrementalSortPath has a subpath"),
                    _ => break,
                };
            }
            below
        }
        _ => return Ok(()),
    };
    let Some(fs) = facts_scan(run, input_id) else { return decline("zone top-k: no facts scan under the Agg") };
    let (rel, relid, granules, scan_id) = (fs.rel, fs.relid, fs.granules, fs.scan);
    let Some((attno, typid)) = leading_group_var(run, &agg, relid) else { return decline("zone top-k: the leading key is not a scan Var") };
    if !zone_word_type(typid)? {
        return decline("zone top-k: the leading key is not a word type");
    }
    let mut desc = false;
    if let Some((_, pathkeys)) = &sort {
        let leads = pathkeys.first().is_some_and(|pk| pathkey_has(run, pk, |e| scan_var(e, relid).is_some_and(|(a, _)| a == attno)));
        let Some((down, _)) = pathkey_order(&pathkeys[0]).filter(|_| leads) else {
            return decline("zone top-k: the ORDER BY does not lead with the grouping key");
        };
        if down && !walk_group_desc_on() {
            return decline("zone top-k: a descending group walk (pgrust.gravity_zonetopk = asc)");
        }
        desc = down;
        if !default_btree_order(&pathkeys[0], typid)? {
            return decline("zone top-k: the ORDER BY is not the key type's default btree order");
        }
        if !crate::relnode::relids_is_member(i32::from(attno), &run.root.rel(rel).notnullattnums) {
            return decline("zone top-k: an ordered walk over a nullable key");
        }
    }
    let target = agg.path.pathtarget_id.expect("AggPath has a target");
    if admissible_aggregates(run, target, false)?.is_none() {
        return decline("zone top-k: an aggregate is not order-insensitive");
    }
    if sort.is_some() {
        // D451's rule: PostgreSQL's Agg projects every group before the Sort, the walk only the groups it keeps
        let groups: Vec<u32> = agg.groupClause.iter().filter_map(|&id| run.root.expr_node(id).as_sort_group_clause().map(|c| c.tleSortGroupRef)).collect();
        if !quiet_target(run, target, &groups) || inner_proj.is_some_and(|(_, t)| !quiet_target(run, t, &groups)) {
            return decline("zone top-k: a group's projection can raise or act (D451)");
        }
    }
    let Some(ranges) = zone_ranges(run, rel, attno) else { return decline("zone top-k: no zones for the key") };
    let Some(c) = zone_walk_census(&ranges, n, desc, 1) else { return decline("zone top-k: no keyed granules") };
    if c.survivors >= c.granules {
        return decline("zone top-k: every granule survives");
    }
    let ratio = c.survivors as f64 / granules.max(1) as f64;
    let key_expr = leading_group_expr(run, &agg).expect("the leading key was read above");
    let key_id = run.intern_expr(key_expr);
    let nd1 = crate::selfuncs::estimate_num_groups(run, &[(key_id, key_expr)], run.root.path(scan_id).base().rows.max(1.0))?.max(1.0);
    let kept = (n as f64 * (agg.numGroups / nd1).max(1.0)).min(u64::MAX as f64) as u64;
    let plain_groups = agg.numGroups;
    let presence = cost_profile::walk::key_presence(rows_per_granule(run, scan_id, granules) as f64, nd1);
    let spread = zone_spread(&ranges);
    let mut scan2 = scan_over_granules(run, scan_id, rel, c.survivors, granules)?;
    if let Some((prel, ptarget)) = fs.proj {
        scan2 = crate::pathnode::apply_projection_to_path(run, prel, scan2, ptarget)?;
    }
    agg.aggstrategy = AGG_HASHED;
    agg.subpath = Some(scan2);
    agg.gravity_rewrite = Some(GravityRewrite::ZoneTopK { attno, desc, nulls_first: false });
    agg.groupClause = crate::relnode::pgvec_clone_shallow(run.mcx, &agg.groupClause);
    agg.qual = crate::relnode::pgvec_clone_shallow(run.mcx, &agg.qual);
    agg.path.pathkeys = mcx::PgVec::new_in(run.mcx);
    agg.path.rows = (agg.path.rows * ratio).max(n as f64).min(agg.path.rows).max(1.0);
    agg.numGroups = agg.numGroups.min(agg.path.rows.max(1.0));
    let mut top = run.root.alloc_path(PathNode::AggPath(agg));
    if let Some((prel, ptarget)) = inner_proj {
        top = crate::pathnode::apply_projection_to_path(run, prel, top, ptarget)?;
    }
    if let Some((sort_rel, pathkeys)) = sort {
        top = crate::pathnode::create_sort_path(run, sort_rel, top, pathkeys, limit_tuples);
    }
    let force = pgrust_guc("pgrust.gravity_zonetopk").is_some_and(|v| v.trim().eq_ignore_ascii_case("force"));
    let share = if force { f64::MIN_POSITIVE } else { walk_price().group_share_of(c.survivors, granules, n, kept, spread, presence, plain_groups, gravity_width(run)) };
    let b = run.root.path_mut(top).base_mut();
    (b.startup_cost, b.total_cost) = (plain * share, plain * share);
    fires::fire(if ordered { "gravity_paths.zone_topk.ordered" } else { "gravity_paths.zone_topk" });
    if desc {
        fires::fire("gravity_paths.zone_topk.desc"); // shapecheck fire signal (GP-1b: the group walk goes down the key maxima)
    }
    finish(run, final_rel, top, proj, offset_est, count_est)
}

fn live(run: &PlannerRun<'_>, p: PathId) -> bool {
    run.root.path_arena.is_live(p)
}

fn limited_cost(run: &PlannerRun<'_>, p: PathId, n: u64) -> f64 {
    let b = run.root.path(p).base();
    b.startup_cost + (b.total_cost - b.startup_cost).max(0.0) * (n as f64 / b.rows.max(1.0)).min(1.0)
}

pub(crate) fn nest_sublink(run: &mut PlannerRun<'_>) {
    static SECTIONS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*SECTIONS.get_or_init(|| std::env::var("PGRUST_GRAVITY_RERUN_SECTIONS").as_deref() != Ok("0")) {
        run.root.gravity_nested = true;
    } else if !run.root.gravity_nested && gravity_width(run) > 0 {
        fires::fire("planner.sublink.width"); // shapecheck fire signal: a SubLink level priced at the statement's width
    }
}

pub(crate) fn gravity_width(run: &PlannerRun<'_>) -> u32 {
    let pool = if guc_tables::vars::max_parallel_workers.installed() { guc_tables::vars::max_parallel_workers.read() } else { i32::MAX };
    let width = u32::try_from(crate::gucs::max_parallel_workers_per_gather().min(pool).max(0)).unwrap_or(0);
    if run.root.gravity_nested {
        if width > 0 {
            fires::fire("planner.nested.width0"); // shapecheck fire signal (D2914 (2a)): a nested level priced at width 0 under a wide statement
        }
        return 0;
    }
    width
}

pub(crate) fn add_zone_walk_path<'mcx>(run: &mut PlannerRun<'mcx>, final_rel: RelId, pre_limit: PathId, n: u64, offset_est: i64, count_est: i64, limit_tuples: f64) -> PgResult<()> {
    let parse = run.parse();
    if parse.sortClause.is_nil() || !parse.groupClause.is_nil() || parse.hasAggs || parse.havingQual.is_some() {
        return Ok(());
    }
    let (proj, cur) = peel_projection(run, pre_limit);
    let (sort_rel, scan_id, pathkeys, rewrite_set) = match run.root.path(cur) {
        PathNode::SortPath(sp) => (sp.path.parent, sp.subpath.expect("SortPath has a subpath"), crate::relnode::pgvec_clone_shallow(run.mcx, &sp.path.pathkeys), sp.gravity_rewrite.is_some()),
        _ => return Ok(()),
    };
    if rewrite_set || pathkeys.is_empty() {
        return decline("zone walk: the Sort is annotated already or has no key");
    }
    let Some(fs) = facts_scan(run, scan_id) else { return decline("zone walk: no facts scan under the Sort") };
    let (rel, relid, granules, scan_id) = (fs.rel, fs.relid, fs.granules, fs.scan);
    let mut key: Option<(i16, types_core::Oid)> = None;
    let mut key_node: Option<Node<'mcx>> = None;
    if !pathkey_has(run, &pathkeys[0], |e| {
        if key.is_none() {
            key = scan_var(e, relid);
            key_node = key.map(|_| e);
        }
        key.is_some()
    }) {
        return Ok(());
    }
    let (Some((attno, typid)), Some(key_node)) = (key, key_node) else { return decline("zone walk: the first sort key is not a scan Var") };
    if !zone_word_type(typid)? {
        return decline("zone walk: the first sort key is not a word type");
    }
    if !default_btree_order(&pathkeys[0], typid)? {
        return decline("zone walk: the first sort key is not the key type's default btree order");
    }
    let Some((desc, nulls_first)) = pathkey_order(&pathkeys[0]) else { return decline("zone walk: the first sort key has no btree order") };
    let Some(ranges) = zone_ranges(run, rel, attno) else { return decline("zone walk: no zones for the key") };
    let per_granule = rows_per_granule(run, scan_id, granules);
    let (census, nulls_counted) = if nulls_first {
        let (tuples, scan_rows) = (run.root.rel(rel).tuples, run.root.path(scan_id).base().rows);
        let nullfrac = key_null_fraction(run, rel, attno, key_node)?;
        let rows = tuples / granules.max(1) as f64;
        (nulls_first_census(&ranges, n, desc, per_granule, rows, nullfrac * tuples, nullfrac * scan_rows), nullfrac > 0.0 || ranges.iter().any(|(lo, hi)| lo > hi))
    } else {
        (zone_walk_census(&ranges, n, desc, per_granule), false)
    };
    let Some(c) = census else { return decline("zone walk: no keyed granules") };
    if c.survivors >= c.granules {
        return decline("zone walk: every granule survives");
    }
    if nulls_counted {
        fires::fire("gravity_paths.zone_walk.nulls_first");
    }
    // the walk replaces the plain plan the planner would otherwise take under this Limit: its price scales the cheapest
    // of the ordered rel's paths as the Limit reads them (at a width the parallel plan, which Gravity runs at that width)
    let plain = run.root.rel(run.root.path(cur).base().parent).pathlist.iter().copied().filter(|&p| live(run, p)).chain([cur]).map(|p| limited_cost(run, p, n)).fold(f64::INFINITY, f64::min);
    let mut scan2 = scan_over_granules(run, scan_id, rel, c.survivors, granules)?;
    if let Some((prel, ptarget)) = fs.proj {
        scan2 = crate::pathnode::apply_projection_to_path(run, prel, scan2, ptarget)?;
    }
    let sort2 = crate::pathnode::create_sort_path(run, sort_rel, scan2, pathkeys, limit_tuples);
    let share = walk_price().share(c.survivors, granules, gravity_width(run), nulls_counted);
    if let PathNode::SortPath(sp) = run.root.path_mut(sort2) {
        sp.gravity_rewrite = Some(GravityRewrite::ZoneWalk { attno, desc, nulls_first });
        (sp.path.startup_cost, sp.path.total_cost) = (plain * share, plain * share);
    }
    fires::fire(if desc { "gravity_paths.zone_walk.desc" } else { "gravity_paths.zone_walk" });
    finish(run, final_rel, sort2, proj, offset_est, count_est)
}

pub(crate) fn add_two_pass_path<'mcx>(run: &mut PlannerRun<'mcx>, final_rel: RelId, pre_limit: PathId, n: u64, offset_est: i64, count_est: i64, limit_tuples: f64) -> PgResult<()> {
    let parse = run.parse();
    if parse.sortClause.is_nil() || parse.groupClause.is_nil() || parse.havingQual.is_some() || !crate::gucs::enable_hashagg() {
        return decline("two-pass: no ORDER BY, no GROUP BY, a HAVING, or hashagg off");
    }
    // pass 1 exports its admitted keys for a bound fixed when the plan is lowered, so a LIMIT or OFFSET that is
    // not a constant (a generic plan's `$1`) takes the one-pass top, which reads its bound at run time.
    if count_est < 0 || offset_est < 0 {
        return decline("two-pass: a LIMIT or OFFSET that is not a constant");
    }
    let (proj, cur) = peel_projection(run, pre_limit);
    let (sort_rel, below_sort, pathkeys) = match run.root.path(cur) {
        PathNode::SortPath(sp) => (sp.path.parent, sp.subpath.expect("SortPath has a subpath"), crate::relnode::pgvec_clone_shallow(run.mcx, &sp.path.pathkeys)),
        _ => return decline("two-pass: no Sort under the Limit"),
    };
    if pathkeys.is_empty() {
        return decline("two-pass: the Sort has no key");
    }
    let (inner_proj, agg_id) = peel_projection(run, below_sort);
    let mut agg = match run.root.path(agg_id) {
        PathNode::AggPath(ap) => ap.clone(),
        _ => return decline("two-pass: no Agg under the Sort"),
    };
    if agg.gravity_rewrite.is_some() || !agg.qual.is_empty() || agg.groupClause.len() < 2 || agg.aggsplit != AGGSPLIT_SIMPLE {
        return decline("two-pass: the Agg is not a plain Agg with two or more keys and no HAVING");
    }
    let agg_input = agg.subpath.expect("AggPath has a subpath");
    let (scan_id, agg_own_run) = match agg.aggstrategy {
        AGG_HASHED => (agg_input, (agg.path.total_cost - agg.path.startup_cost).max(0.0)),
        AGG_SORTED if crate::grouping::grouping_is_hashable(run, &agg.groupClause) => {
            let mut below = agg_input;
            loop {
                below = match run.root.path(below) {
                    PathNode::SortPath(sp) => sp.subpath.expect("SortPath has a subpath"),
                    PathNode::IncrementalSortPath(sp) => sp.spath.subpath.expect("IncrementalSortPath has a subpath"),
                    _ => break,
                };
            }
            if plain_scan_relid(run, below).is_some() {
                (below, 0.0)
            } else {
                let (proj, ordered) = peel_projection(run, below);
                let rel = run.root.path(ordered).base().parent;
                let Some(scan) = run.root.rel(rel).pathlist.iter().copied().find(|&p| live(run, p) && plain_scan_relid(run, p).is_some()) else {
                    return decline("two-pass: a sorted Agg over a relation without a plain scan");
                };
                // a fresh ProjectionPath: the shared scan path is never re-targeted in place
                let scan = match proj {
                    Some((prel, ptarget)) => {
                        let pp = crate::pathnode::create_projection_path(run, prel, scan, ptarget, false);
                        run.root.alloc_path(pp)
                    }
                    None => scan,
                };
                (scan, 0.0)
            }
        }
        _ => return decline("two-pass: the Agg is neither hashed nor sorted over hashable keys"),
    };
    let agg_own_startup = {
        let input_total = run.root.path(agg_input).base().total_cost;
        let own = if agg.aggstrategy == AGG_HASHED { agg.path.startup_cost - input_total } else { agg.path.total_cost - input_total };
        own.max(0.0)
    };
    agg.aggstrategy = AGG_HASHED;
    agg.subpath = Some(scan_id);
    let Some(relid) = plain_scan_relid(run, scan_id) else { return decline("two-pass: no plain scan under the Agg") };
    // pass 1 reads the scan again with its own copy of the WHERE, so a VOLATILE clause (nextval, random()) would
    // run twice per row where PostgreSQL runs it once, and pass 2 would filter on what pass 1 left (a sequence advanced)
    if scan_volatile(run, scan_id)? {
        return decline("two-pass: a VOLATILE scan clause or column (pass 1 would run it again, D3573)");
    }
    if scan_gated(run, scan_id) {
        return decline("two-pass: a pseudo-constant scan clause (a gating Result under the Agg, D3589)");
    }
    let Some(order) = top_order(run, &pathkeys[0], relid)? else { return decline("two-pass: the first sort key is not count(*) DESC or a min / max / integer avg of a word column") };
    let target = agg.path.pathtarget_id.expect("AggPath has a target");
    let Some((_, has_count_star)) = admissible_aggregates(run, target, true)? else { return decline("two-pass: an aggregate is not admissible (kind, FILTER, DISTINCT, ORDER BY or a non-Var argument)") };
    if order == TopOrder::Count && !has_count_star {
        return decline("two-pass: no count(*)");
    }
    let groups: Vec<u32> = agg.groupClause.iter().filter_map(|&id| run.root.expr_node(id).as_sort_group_clause().map(|c| c.tleSortGroupRef)).collect();
    if !quiet_target(run, target, &groups) || inner_proj.is_some_and(|(_, t)| !quiet_target(run, t, &groups)) {
        return decline("two-pass: a group's projection can raise (D451)");
    }
    let input_rows = run.root.path(scan_id).base().rows.max(1.0);
    let scan_target = run.root.path(scan_id).base().pathtarget_id.expect("scan path has a target");
    let scan_parent = run.root.path(scan_id).base().parent;
    let scan_rel = scan_parent;
    let scan_expr = |run: &PlannerRun<'mcx>, col: i16| run.root.pathtarget(scan_target).exprs.iter().map(|&id| *run.root.expr_node(id)).find(|&e| scan_var(e, relid).is_some_and(|(a, _)| a == col));
    // the groups a table can meet: the grouping columns' ndistinct multiplied (PostgreSQL's estimate clamps two columns
    // to a tenth of the rows: D2078's (g, j) reads as g's 331 k groups, 666 k at run time); a column that is not a scan
    // Var keeps the estimate
    let mut groups_met = Some(1.0f64);
    let mut group_cols: Vec<(i16, types_core::Oid)> = Vec::new();
    for &sgref in &groups {
        let col = run.parse().targetList.iter().find_map(|n| n.as_target_entry().filter(|t| t.ressortgroupref == sgref)).and_then(|t| scan_var(t.expr, relid));
        if let Some(c) = col.filter(|c| !group_cols.iter().any(|g| g.0 == c.0)) {
            group_cols.push(c);
        }
        let Some(e) = col.and_then(|(a, _)| scan_expr(run, a)) else {
            groups_met = None;
            continue;
        };
        if groups_met.is_some() {
            let id = run.intern_expr(e);
            let col_ndv = crate::selfuncs::estimate_num_groups(run, &[(id, e)], input_rows)?;
            groups_met = groups_met.map(|g| g * col_ndv.max(1.0));
        }
    }
    let nullable_args = nullable_arg_aggregates(run, target, relid, scan_rel)?;
    let aggs = run.root.pathtarget(target).exprs.len().saturating_sub(agg.groupClause.len()).max(1);
    // Gravity's price of each form (`top_costs`): the plain Agg's cost scaled by two passes over one, so `add_path`
    // takes the two passes exactly where Gravity's two passes cost less than its one pass at this width and hash memory
    let groups = agg.numGroups.max(1.0);
    let groups_met = groups_met.map_or(groups, |g| g.min(input_rows).max(groups));
    let cpu_op = crate::gucs::cpu_operator_cost();
    let ncols = agg.groupClause.len() as f64;
    let (mut simple, mut combine) = (types_pathnodes::AggClauseCosts::default(), types_pathnodes::AggClauseCosts::default());
    crate::prepagg::get_agg_clause_costs(run, AGGSPLIT_SIMPLE, &mut simple)?;
    crate::prepagg::get_agg_clause_costs(run, types_pathnodes::AGGSPLIT_FINAL_DESERIAL, &mut combine)?;
    let (credit_startup, credit_total) = crate::grouping::gravity_hash_spill_credit(run, agg_id, &simple);
    let nullable = |run: &PlannerRun<'mcx>, col: i16| !crate::relnode::relids_is_member(i32::from(col), &run.root.rel(scan_rel).notnullattnums);
    let width = gravity_width(run);
    // PostgreSQL's spill price per input row of the plain hashed Agg (width 0 only: above it the weights carry one)
    let w0_spill_row = if width == 0 && twopass_w0_spill_on() {
        let w = run.root.pathtarget(run.root.path(scan_id).base().pathtarget_id.expect("scan path has a target")).width;
        ::costsize::hashed_agg_spill_surcharge(run, &simple, agg.numGroups.max(1.0), input_rows, w).1 / input_rows
    } else {
        0.0
    };
    let hash_mem = init_small::globals::work_mem() as f64 * guc_tables::vars::hash_mem_multiplier.read() * 1024.0;
    let grant = pgrust_guc("pgrust.gravity_grant_mb").and_then(|v| v.trim().parse::<u64>().ok()).map_or(0.0, |mb| (mb << 20) as f64);
    let byref = byref_state_bytes(run, target, relid, scan_rel)?;
    // the pass-1 key priced: any group key bounds every group of it (a group's rows are among its key's), so any one
    // is sound. `pgrust.gravity_twopass_keys = leading` keeps the pre-MK-1 key (the first group key); else every word-typed
    // scan-Var group key (at most the GROUP BY's keys priced). Key pairs are parked (B01 / B3-15): a two-key pass 1 takes
    // the general hashed form, 30-200x slower than the one-key bare form on near-unique keys, with no cost term for it.
    // a key type whose word orders as the type (typesem's verdict, per candidate column)
    let mut word_cols: Vec<i16> = Vec::new();
    for c in &group_cols {
        if zone_word_type(c.1)? {
            word_cols.push(c.0);
        }
    }
    let mut sets: Vec<i16> = Vec::new();
    if twopass_keys_any() {
        sets.extend(group_cols.iter().filter(|c| word_cols.contains(&c.0)).map(|c| c.0));
    } else {
        match leading_group_var(run, &agg, relid) {
            Some((attno, typid)) if zone_word_type(typid)? => sets.push(attno),
            Some(_) => return decline("two-pass: the leading key is not a word type"),
            None => return decline("two-pass: the leading key is not a scan Var"),
        }
    }
    // count(*): then a tie key in either order, the order pass 1's keys seen once are offered in (two of them hold
    // different first keys, so the keys after it never order one before the other): the set's first key. An extreme
    // admits every key whose extreme ranks with the n-th best, ties included, so no key after it decides an admission;
    // its hint's flags are the extreme's own order.
    let tie = match (order, pathkeys.len()) {
        (TopOrder::Count, n) if n >= 2 => {
            let Some(order) = pathkey_order(&pathkeys[1]) else { return decline("two-pass: the floor key has no btree order") };
            let Some((col, typid)) = group_cols.iter().copied().find(|&(a, _)| pathkey_has(run, &pathkeys[1], |e| scan_var(e, relid).is_some_and(|(x, _)| x == a))) else {
                return decline("two-pass: the second sort key is not a grouping key");
            };
            if !default_btree_order(&pathkeys[1], typid)? {
                return decline("two-pass: the floor key is not the key type's default btree order");
            }
            Some((col, order))
        }
        _ => None,
    };
    let (floor_desc, floor_nulls_first) = match (order, tie) {
        (TopOrder::Extreme(..), _) => pathkey_order(&pathkeys[0]).unwrap_or((false, false)),
        (TopOrder::Count, Some((_, o))) => o,
        _ => (false, false),
    };
    if let Some((col, _)) = tie {
        sets.retain(|&s| s == col);
    }
    if sets.is_empty() {
        return decline("two-pass: no word-typed group key to key pass 1 (or none leads with the tie key)");
    }
    let key_of = |run: &PlannerRun<'mcx>, col: i16| scan_expr(run, col).unwrap_or(*run.root.expr_node(run.root.pathtarget(target).exprs[0]));
    let mut best: Option<(f64, i16, TopCostIn, f64, bool)> = None;
    let spread_of = |run: &PlannerRun<'mcx>, col: i16| zone_ranges(run, scan_rel, col).map(|r| zone_spread(&r));
    let lead = match group_cols.first() {
        Some(&(col, _)) => {
            let e = key_of(run, col);
            let id = run.intern_expr(e);
            (crate::selfuncs::estimate_num_groups(run, &[(id, e)], input_rows)?, spread_of(run, col))
        }
        None => (agg.numGroups.max(1.0), None),
    };
    for &attno in &sets {
        let key_expr = key_of(run, attno);
        let key_id = run.intern_expr(key_expr);
        let ndv = crate::selfuncs::estimate_num_groups(run, &[(key_id, key_expr)], input_rows)?;
        // The candidates: a count top's rows of keys seen twice plus the n first keys' rows (from what ANALYZE read of
        // a lone key, else the rows whose key repeats); an extreme top's n first keys' rows.
        let mut cand = match order {
            TopOrder::Count => {
                let repeats = two_pass_candidates(input_rows, ndv, n as f64);
                match key_sample(run, relid, scan_parent, attno)? {
                    Some(sample) => count_candidates(input_rows, n as f64, sample).max(repeats),
                    _ => repeats,
                }
            }
            TopOrder::Extreme(_, false) => extreme_candidates(input_rows, ndv, n as f64),
            TopOrder::Extreme(_, true) => spread_candidates(input_rows, ndv, n as f64),
        };
        let mut admitted = 0.0;
        if let (TopOrder::Extreme(col, _), true) = (order, floor_nulls_first) {
            if let Some(e) = scan_expr(run, col) {
                let f = key_null_fraction(run, scan_rel, col, e)?;
                admitted = 1.0 - (1.0 - f).powf(input_rows / ndv.max(1.0));
                cand = (cand + input_rows * admitted).min(input_rows);
            }
        }
        let key_nullable = nullable(run, attno);
        let (pass1_record, key_corr) = match order {
            TopOrder::Count => (pass1_record_bytes(key_nullable), None),
            TopOrder::Extreme(col, spread) => (pass1_extreme_record_bytes(key_nullable, nullable(run, col), spread), scan_correlation(run, relid, attno)?),
        };
        let costs = TopCostIn {
            scan: run.root.path(scan_id).base().total_cost,
            rows: input_rows,
            groups,
            keys: ndv,
            cand,
            cand_groups: groups.min(cand).max((n as f64).min(groups)).max(groups * admitted),
            n: n as f64,
            fold_row: simple.transCost.per_tuple + cpu_op * ncols,
            fold_group: simple.finalCost.per_tuple + crate::gucs::cpu_tuple_cost(),
            merge_row: combine.transCost.per_tuple + cpu_op * ncols,
            cpu_op,
            pass1_record,
            hash_mem,
            key_corr,
            grant,
            width: f64::from(width),
            group_bytes: group_record_bytes(agg.groupClause.len(), aggs),
            t1_record: phase1_record_bytes(agg.groupClause.len(), aggs, nullable_args),
            fold_key: cpu_op * ncols,
            groups_met,
            w0_spill_row,
            byref_state_bytes: byref,
        };
        let (one, two, measured) = top_costs_priced(&costs, &TopShape { lead_keys: lead.0, lead_spread: lead.1, key_spread: spread_of(run, attno) });
        // MK-1's non-leading pass-1 keys only where the measured price reads them (the fitted one mis-priced them: gtopn's
        // KEYED-04 losses; CC-2b's config A census): else the leading key alone, as before MK-1
        if !measured && group_cols.first().is_some_and(|&(c, _)| c != attno) {
            continue;
        }
        let scale = if one > 0.0 { two / one } else { 1.0 };
        if best.as_ref().is_none_or(|b| scale < b.0) {
            best = Some((scale, attno, costs, cand, measured));
        }
    }
    let Some((scale, attno, costs, cand, measured)) = best else { return decline("two-pass: no pass-1 key set priced") };
    // pgrust.gravity_twopass (a testing and rollback knob, not a tuning one): `cost` (unset) the reading above; `force`
    // the pre-PL-07 gate, PostgreSQL's reading: both passes over the input plus the Agg's own work over the candidate
    // fraction, so `add_path` takes the two passes wherever their shape applies and the one-pass hash aggregate's spill
    // prices above them (e2e files that exist to exercise the form under a grant or width where the cost takes one pass)
    let pg_reading = pgrust_guc("pgrust.gravity_twopass").is_some_and(|v| v.trim().eq_ignore_ascii_case("force")).then(|| {
        let frac = (cand / input_rows).clamp(0.0, 1.0);
        let startup = two_pass_passes_cost(costs.scan, input_rows, cpu_op) + agg_own_startup * frac;
        (startup, startup + agg_own_run * frac)
    });
    if pg_reading.is_none() && costs.scattered_one_pass_holds() {
        return decline("two-pass: a scattered key's one pass holds its groups");
    }
    let mut agg2 = agg;
    agg2.gravity_rewrite = Some(GravityRewrite::TwoPass { attno, floor_desc, floor_nulls_first });
    agg2.groupClause = crate::relnode::pgvec_clone_shallow(run.mcx, &agg2.groupClause);
    agg2.qual = crate::relnode::pgvec_clone_shallow(run.mcx, &agg2.qual);
    agg2.path.pathkeys = mcx::PgVec::new_in(run.mcx);
    if let Some((startup, total)) = pg_reading {
        (agg2.path.startup_cost, agg2.path.total_cost) = (startup, total);
    } else {
        // the credit is one absolute price (PostgreSQL's spill of the plain hashed Agg, which neither Gravity form
        // pays), so the scale applies to the plain Agg as PostgreSQL prices it and the credit comes off once: the one pass
        // and the two passes compare as they do without the credit. Scaling the credited price let the Sort above decide
        // (its rows: every group under the one pass, the candidates under the two): av2/twopassw0's unforced W = 0
        // statement took the two passes, 16.2 ms, over the one pass's 7.6 ms.
        agg2.path.startup_cost = ((agg2.path.total_cost - agg_own_run + credit_startup) * scale - credit_startup).max(0.0);
        agg2.path.total_cost = ((agg2.path.total_cost + credit_total) * scale - credit_total).max(0.0);
    }
    if costs.pass1_holds() || pg_reading.is_some() || measured {
        agg2.path.rows = agg2.path.rows.min(cand).max(n as f64).max(1.0);
    }
    agg2.numGroups = agg2.numGroups.min(agg2.path.rows);
    let mut top = run.root.alloc_path(PathNode::AggPath(agg2));
    if let Some((prel, ptarget)) = inner_proj {
        top = crate::pathnode::apply_projection_to_path(run, prel, top, ptarget)?;
    }
    let sort2 = crate::pathnode::create_sort_path(run, sort_rel, top, pathkeys, limit_tuples);
    finish(run, final_rel, sort2, proj, offset_est, count_est)
}

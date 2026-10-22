//! The width and claim knobs (partitioned-record.md §4.4 ruling 7; breakers.md): the morsel rule, the whole-part claim law, the forced-width / forced-claim / dense-front levers (the whole-part kill switch env knob went in v5: no script set it).

use super::{Kind, Knob};
use crate::inputs::knobs::room::clamp_width;

pub static KNOBS: &[Knob] = &[
    Knob { name: "PGRUST_GRAVITY_WIDTH", kind: Kind::Env, default: "unset", doc: "the width forced on every granule pipeline for a measurement (capped by the grant; read once)" },
    Knob { name: "PGRUST_GRAVITY_C0", kind: Kind::Env, default: "unset", doc: "the startup claim forced to n granules and whole-part claims off, for a claim-cost measurement (read once)" },
    Knob { name: "PGRUST_GRAVITY_CLAIM_SPLIT", kind: Kind::Env, default: "1", doc: "in-flight claim stealing (D3320), ON by default: a worker that finds a pgrc section's cursor exhausted takes the back half of the running claim with the most time left (paced by the units the claim read, not the ones it skipped: D3247), on cursors whose claims are not part-grain work; 0: off (whole claims run to their end, as before the ruling); 2: on with every unit taken counted in the pace (measurement only, the A/B arm); unset or any other value: on (driver claimcut.rs; read once)" },
    Knob { name: "PGRUST_GRAVITY_SPLIT_TRACE", kind: Kind::Env, default: "0", doc: "1: a stderr line per range a running claim hands over and per claim's end while claim stealing is on (PGRUST_GRAVITY_CLAIM_SPLIT, on by default) (driver claimcut.rs; read once)" },
    Knob { name: "PGRUST_GRAVITY_DENSE", kind: Kind::Env, default: "1", doc: "0: the dense key front off for the measurement in breakers.md (read once)" },
    Knob { name: "PGRUST_GRAVITY_WALK_RECHECK", kind: Kind::Env, default: "1", doc: "0: a group walk's run-time re-check off (RUNS-02), so a fit's walk arm walks past its price (read once)" },
    Knob { name: "PGRUST_GRAVITY_RERUN_SECTIONS", kind: Kind::Env, default: "1", doc: "0: every nested member (a rerun's, an Init's) runs on the leader (D3014 S2' inner widening and D3137 off; read once per process, as the planner's mirror)" },
];

/// Startup claim in granules per worker. MEASURED-HELDOUT (W8 sweep 4 / 8 / 16 / 32 / 64 over the held-out width
/// set, wrong 0 every arm, plus two repeat takes of 16 against 32): 4..32 are flat within take-to-take noise (16 ran
/// 72-85 ms, 32 ran 75-92 ms), 64 is slower, so the value stays.
pub(crate) const GRANULE_C0: u64 = 16;
/// Heap blocks per worker in the morsel rule. MEASURED-HELDOUT (W8 sweep 16 / 32 / 64 / 128 / 256 over the
/// held-out width set, wrong 0 every arm): 16..64 are flat (within 3%), 128 and 256 are slower, so the value stays.
///
/// COST-DERIVED (gap-optoverfit EX-10 / EX-19, keep): the blocks a worker must have before its wake pays. Two workers pay
/// once a heap carries more work than 2 x (SECTION + 2 x WORKER), the section's start and end and each worker's wake and
/// claims. Quiet dev sweep (lanes/inputs/work/fit_width2.py, box 54 whole, 16 vCPU, trunk 273de83ad8b, widths 0 / 2 / 4 /
/// 8 / 16 forced, 3 warm-ups and the median of 5, two takes: A/A 0.956-1.040 p10-p90, THP fallbacks and compaction stalls
/// +0): SECTION 65-75 us and WORKER 8.5-9 us, so ~0.18 ms; a block of the densest light rows the sweep holds (153 rows a
/// block, count and one term over a filtered scan) costs ~1.9 us on the leader, so ~95 blocks, ~48 a worker; 64 sits
/// between that and the next density's (~90), inside the flat range above. Measured there: at 63-64 blocks two workers lose to the leader alone
/// (1.18x on that scan, 1.33x hash-probed), at 125 blocks they win on the scan (0.86x; 1.05x hash-probed). Lighter rows
/// break even later (~90 / 170 / 290 blocks a worker at 74 / 32 / 12 rows a block), where the rule's width runs up to
/// 1.31x the best measured one. What the rule does not see: values out of line or compressed inline cost 25-600x a
/// light row while the catalog and the plan price them as light (PostgreSQL charges no detoast and no decompression:
/// 0.020 cost units a row there, 0.026 on the light scan); at 16-64 blocks of 2.7-25 KB out-of-line values the leader
/// runs alone where two workers take 0.59-0.70 of its time. A width from the plan's per-row cost and the toast
/// relation's bytes a row (EX-10 (a)) took that gain but lost 2.3-2.5x on ~250 blocks of compressed inline values (no
/// catalog fact shows them; the rule's three workers ran them) and 1.06-1.28x on 128-256 light blocks, so the rule stays
/// (the reading: lanes/inputs/work/fit2/fit2_eval.py over the two takes; its does-not-apply test is
/// `a_heap_runs_one_worker_per_startup_claim_of_blocks`).
/// The value is gravity_program's (`program::BLOCK_C0`, beside `heap_width`, the rule the planner's width costing reads too).
pub(crate) const BLOCK_C0: u64 = gravity_program::program::BLOCK_C0;
pub(crate) const MORSEL: u64 = GRANULE_C0;

/// gap-optoverfit EX-11, keep: the law's one premise is a worker per live part, no tuned constant (the part-claims kill
/// switch went in c0572469384); its does-not-apply test is tests/admit_runner.rs `whole_part_claims_split_inside_a_part_below_the_width`.
/// The whole-part claim law: a claim whose end groups by code per part runs its whole part (`code_groups`: the runner's text-groups fold, the two-level code aggregation, appends per part); an answerable site's too (one answer per part, no read: a site some part of which the catalog's records skip or answer, granule_source::answered_parts); every other non-sparse pipeline runs whole parts for the cold I/O (one run read per lane per part, W parts' reads in flight) only while every worker gets a part — fewer live parts than the width would leave `width - live_parts` workers idle, so the sizer cuts claims inside the parts instead. V4R-10: a site that only reads or filters code lanes is cut like int lanes (a v4 program keeps its code state per window: a window reads one part's dictionary).
pub fn whole_part_claims(code_groups: bool, answerable: bool, sparse: bool, live_parts: u64, width: u16) -> bool {
    code_groups || (!sparse && (answerable || live_parts >= width as u64))
}

/// The tail rule, where the law runs whole parts for the cold I/O alone (no per-part code groups, not answerable, not
/// sparse, no part taken whole as entry runs: runner/plan.rs `claims_of`): the first ⌊P/W⌋·W live parts are claimed
/// whole and the last P mod W are cut inside each part by the runtime's duration-adaptive sizer (never across a part; a
/// fast part's claim still grows to the part's end), so the last wave runs on every worker instead of P mod W of them.
/// The count of whole live parts, or None: every wave full (P mod W = 0), fewer live parts than the width (the law cuts
/// already), or no section. P and W are the bind's exact facts: no constant, no part-count threshold (what a cut saves
/// and what its extra claims cost are both per tail part).
pub fn tail_from_part(live_parts: u64, width: u16) -> Option<u64> {
    let w = width as u64;
    (w >= 2 && live_parts > w && live_parts % w != 0).then(|| live_parts / w * w)
}

pub(crate) fn forced_width() -> Option<u16> {
    static FORCED: pgsync::OnceLock<Option<u16>> = pgsync::OnceLock::new();
    *FORCED.get_or_init(|| gravity_driver::env::num("PGRUST_GRAVITY_WIDTH").map(clamp_width))
}

/// RUNS-02's run-time re-check of a group walk off (`PGRUST_GRAVITY_WALK_RECHECK=0`; testing: a fit's walk arm past its price).
pub(crate) fn walk_recheck_off() -> bool {
    static OFF: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *OFF.get_or_init(|| gravity_driver::env::num("PGRUST_GRAVITY_WALK_RECHECK") == Some(0))
}

/// skewsplit (D3320): whether a running pgrc claim may give its back half to an idle worker: on by default, off only with
/// `PGRUST_GRAVITY_CLAIM_SPLIT=0` (the knob style of `PGRUST_GRAVITY_DENSE`).
pub(crate) fn claim_split_on() -> bool {
    static ON: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *ON.get_or_init(|| gravity_driver::env::num::<u64>("PGRUST_GRAVITY_CLAIM_SPLIT") != Some(0))
}

pub(crate) fn forced_c0() -> Option<u64> {
    static FORCED: pgsync::OnceLock<Option<u64>> = pgsync::OnceLock::new();
    *FORCED.get_or_init(|| gravity_driver::env::num("PGRUST_GRAVITY_C0").filter(|&n| n > 0))
}

/// The width a granule pipeline runs at: the rule over the zone census, or the forced width; 0 without a pool.
pub(crate) fn width_for(surviving: u64, granted: u16) -> u16 {
    width_of(surviving, granted, forced_width())
}

/// EX-09 (keep): `width_for`'s rule with the forced width as an argument. The width comes from the bind's exact facts, the
/// zone census's surviving granules (`surviving`) over the grant the pool gave (`granted`), one worker per MORSEL
/// granules; never from the planner's selectivity or a reltuples estimate (PostgreSQL's compute_parallel_worker answers
/// from pages at plan time; these facts are exact at bind). MORSEL is GRANULE_C0, a claim size measured on held-out
/// widths (MEASURED-HELDOUT above), not a worker count.
fn width_of(surviving: u64, granted: u16, forced: Option<u16>) -> u16 {
    if granted == 0 {
        return 0;
    }
    match forced {
        Some(f) => f.min(granted),
        None => (surviving / MORSEL).clamp(1, granted as u64) as u16,
    }
}

/// H15 b (plan §5.2 DRIVER H15a/b, R12-L17): a program that writes or locks rows runs on the leader alone, every
/// pipeline: no pool wake, no worker. PostgreSQL plans no parallel query under a data-modifying or row-locking statement;
/// a producer pipeline of such a program (a hash build, a sub-select) would otherwise run wide beside the write.
pub(crate) fn writes_or_locks(p: &gravity_program::Program) -> bool {
    p.write.cmd != gravity_program::Cmd::Select || p.write.lock != gravity_program::LockKind::None
}

/// The statement's width before any pipeline's facts (`run_parallel`, the pull adapter's producers): 0 (the leader alone)
/// under `admit::leader_only` (combo CIDs, EXPLAIN ANALYZE's counters) and for a write or a lock (H15 b); else the grant,
/// itself 0 without a pool or under SERIALIZABLE, narrowed to the widest width whose floor fits the pool
/// (`grant::narrow_past_pool`, D3034 (f3)).
pub(crate) fn statement_width(p: &gravity_program::Program) -> u16 {
    if crate::admit::leader_only() || writes_or_locks(p) {
        return 0;
    }
    crate::inputs::knobs::grant::narrow_past_pool(p, crate::inputs::knobs::room::granted_width())
}

/// What a v4 `ProtoKind::Width` declares in its `arg` (program/src/v4.rs): bit 63 = the pipeline writes or locks (H15 b),
/// the low bits = the lowerer's estimate of the rows the pipeline reads (planner-side; the exact facts decide).
pub const WIDTH_LEADER_ONLY: u64 = 1 << 63;

/// H15 a (R2-L05, R4-L09): a pipeline's width from the bind's exact facts, never from the planner's selectivity. `claimed` =
/// the width the runner's claims already chose (the zone census over the unanswered granules for a pgrc source, the grant
/// for a heap); `blocks` = the heap relation's blocks when a heap drives the pipeline. The leader alone (0: no section, no
/// pool wake) under bit 63, when the claims chose fewer than two workers, for a heap of fewer blocks than two startup
/// claims, and for a pgrc site the leader `answered` whole (L1: every part from the catalog's part records,
/// no read; ~0.3 µs a part against the pool's ~0.35 ms wake-and-claim floor at W8, ~0.45 at W16); a heap otherwise runs
/// one worker per startup claim of blocks, up to the claimed width (`width_for`'s rule over blocks). A narrower width
/// never changes the rows (the claims tile the relation either way).
pub fn by_work(claimed: u16, arg: u64, blocks: Option<u64>, answered: bool) -> u16 {
    if arg & WIDTH_LEADER_ONLY != 0 || claimed < 2 || answered {
        return 0;
    }
    let Some(blocks) = blocks else { return claimed };
    if forced_width().is_some() {
        return claimed;
    }
    match gravity_program::heap_width(blocks, claimed) {
        0 => 0,
        w => {
            fires::fire("width.heap_wide"); // shapecheck fire signal (REGISTRY EX-10 / EX-11 / NEW-kernels-exec-72): a heap pipeline runs wide
            w
        }
    }
}

/// `by_work` over one declared `ProtoKind::Width` (exec/src/v4/m1.rs's Protocol::prepare): the pipeline's claimed width in
/// `cx.widths`, the proto's `arg`, and the heap's blocks when a heap range recipe drives the pipeline (one smgr size read,
/// the one `claims_of` made), or whether the leader answers a pgrc site whole (`answered_whole`).
pub(crate) fn pipeline_width(cx: &crate::ops::Prepare<'_>, p: &gravity_program::protos::Proto) -> types_error::PgResult<u16> {
    let i = p.pipeline as usize;
    // pglfast C6(ii): `by_work` answers the leader alone before it reads the blocks or the answered test (leader-only, or
    // claims under two workers: every width-0/1 statement), so neither is worked out: no smgr size read per pipeline
    if p.arg & WIDTH_LEADER_ONLY != 0 || cx.widths[i] < 2 {
        return Ok(by_work(cx.widths[i], p.arg, None, false));
    }
    let site = cx.program.pipelines[i].driving_site;
    let blocks = match (site >= 0).then(|| cx.admitted.registry.range_recipe(site as usize)).flatten() {
        // D3586: an arm the initial pruning removed runs empty, sized as no blocks (runner plan.rs `claims_of`), unopened
        Some(r) if r.set.is_none() && cx.initial.is_some_and(|x| x.pruned(i)) => Some(0),
        // D3585: a relation set's blocks are its every relation's, (D3663) its initial pruning's survivors' when it prunes
        Some(r) => match r.set.as_deref() {
            Some(set) => {
                let kept = |i: usize| cx.initial.and_then(|x| x.set_kept(site as usize)).is_none_or(|v| crate::ops::append::prune::member(v, i));
                Some(set.iter().enumerate().filter(|&(i, _)| kept(i)).try_fold(0u64, |n, (_, &rel)| Ok::<u64, Box<types_error::PgError>>(n + gravity_hosts::ops::scan::heap_source::nblocks(cx.mcx, rel)?))?)
            }
            None => Some(gravity_hosts::ops::scan::heap_source::nblocks(cx.mcx, r.heap)? as u64),
        },
        None => None,
    };
    let answered = cx.widths[i] >= 2 && p.arg & WIDTH_LEADER_ONLY == 0 && site >= 0 && answered_whole(cx, i, site as usize)?;
    Ok(by_work(cx.widths[i], p.arg, blocks, answered))
}

/// L1: the leader answers pipeline `i`'s pgrc site `site` whole, as a serial walk of whole parts does: no zone walk or
/// top-k, no word-valued zone predicate (bound after this protocol, by src.verdicts), the zone verdicts on, every
/// attribute lane a filter or an answer lane, and every part answered from the catalog's records
/// (`granule_source::answered_whole` over the site's static zone predicates).
fn answered_whole(cx: &crate::ops::Prepare<'_>, i: usize, site: usize) -> types_error::PgResult<bool> {
    use gravity_program::protos::{ProtoKind, VERDICT_ZONE};
    let Some(r) = cx.admitted.registry.granule_recipe(site) else { return Ok(false) };
    let (desc, pipe) = (&cx.program.sites[site], &cx.program.pipelines[i]);
    let protos = cx.program.v4.as_ref().map_or(&[][..], |d| &d.protos[..]);
    let verdicts = |slot: fn(u8) -> bool| protos.iter().filter(move |q| q.kind == ProtoKind::Verdicts && q.site as i64 == site as i64 && slot(q.slot));
    let zone_on = verdicts(|s| s == 0).all(|q| q.arg & VERDICT_ZONE != 0);
    // NEW-72: a bound literal's or an IN list's slot binds before this read (`site_zone`); a generic plan's or an open-time
    // one does not
    let late = |arg: u64| gravity_program::protos::word_zone_at_of(arg).is_none() && gravity_program::protos::word_zone_list_of(arg).is_none();
    let words = verdicts(|s| s >= gravity_program::protos::VERDICT_WORD_ZONE).any(|q| late(q.arg));
    let lanes = (0..desc.natts()).all(|k| desc.filter_lanes.contains(k) || desc.answer_lanes.contains(k));
    let zone = crate::ops::scan::pgrc::verdicts::site_zone(cx.program, site, &cx.words[..], &cx.nulls[..]).unwrap_or_default();
    if cx.admitted.walk.is_some() || pipe.topk.is_some() || !zone_on || words || !lanes || zone.is_empty() {
        return Ok(false);
    }
    let rel = table::table_open(cx.mcx, r.rel, types_storage::lock::NoLock)?;
    let list = gravity_hosts::ops::scan::pgrc::granule_source::resolve(&rel, Some(cx.snap));
    rel.close(types_storage::lock::NoLock)?;
    let answer: Vec<usize> = (0..desc.natts()).filter(|&k| desc.answer_lanes.contains(k)).collect(); // alloc: Exec
    Ok(list.is_ok_and(|l| gravity_hosts::ops::scan::pgrc::granule_source::answered_whole(&l, r, &zone, &answer)))
}

#[cfg(test)]
mod width_of_tests {
    use super::{width_of, MORSEL};

    /// EX-09 (keep, does not apply): the catalog's premise (a worker count from PostgreSQL's compute_parallel_worker and a
    /// reltuples x width estimate) does not apply: the width is the census's exact survivor count over the grant.
    #[test]
    fn the_width_is_the_census_over_the_grant() {
        assert_eq!(width_of(1 << 30, 0, None), 0, "no pool: the leader alone");
        assert_eq!(width_of(1 << 30, 0, Some(8)), 0, "a forced width needs a pool too");
        assert_eq!(width_of(0, 8, None), 1, "an empty census still runs one claim");
        assert_eq!(width_of(MORSEL - 1, 8, None), 1);
        assert_eq!(width_of(3 * MORSEL, 8, None), 3, "one worker per MORSEL surviving granules");
        assert_eq!(width_of(3 * MORSEL + MORSEL - 1, 8, None), 3);
        assert_eq!(width_of(100 * MORSEL, 8, None), 8, "never past the grant");
        assert_eq!(width_of(100 * MORSEL, 16, None), 16);
        assert_eq!(width_of(0, 8, Some(4)), 4, "a forced width (a measurement) ignores the census");
        assert_eq!(width_of(1 << 30, 8, Some(32)), 8, "a forced width is capped by the grant");
    }
}

#[cfg(test)]
mod by_work_tests {
    use super::{by_work, BLOCK_C0, WIDTH_LEADER_ONLY};

    #[test]
    fn a_write_or_lock_and_a_census_under_two_workers_run_on_the_leader() {
        assert_eq!(by_work(16, WIDTH_LEADER_ONLY | 1_000_000, None, false), 0);
        assert_eq!(by_work(16, WIDTH_LEADER_ONLY, Some(1 << 20), false), 0);
        assert_eq!(by_work(1, 0, None, false), 0);
        assert_eq!(by_work(0, 0, None, false), 0);
        assert_eq!(by_work(9, 0, None, false), 9, "a pgrc census width stands");
        assert_eq!(by_work(16, 0, None, true), 0, "a site the leader answers whole: no pool wake (L1)");
    }

    /// gap-optoverfit EX-10 / EX-19 (keep, does not apply): the catalog's premise (a worker count from PostgreSQL's
    /// compute_parallel_worker rules and a per-row cost from reltuples x width) does not apply: neither fact prices what a
    /// heap row costs here (detoast, decompression), and a width from them lost to this rule on the quiet sweep
    /// (`BLOCK_C0`'s doc). The width reads the heap's blocks and the grant alone, for the runner's claims and a declared
    /// Width proto (`width.work`) both.
    #[test]
    fn a_heap_runs_one_worker_per_startup_claim_of_blocks() {
        assert_eq!(by_work(16, 0, Some(0), false), 0);
        assert_eq!(by_work(16, 0, Some(2 * BLOCK_C0 - 1), false), 0, "fewer blocks than two startup claims: the leader");
        assert_eq!(by_work(16, 0, Some(2 * BLOCK_C0), false), 2);
        assert_eq!(by_work(16, 0, Some(5 * BLOCK_C0 + 7), false), 5);
        assert_eq!(by_work(16, 0, Some(1 << 30), false), 16, "never past the claimed width");
    }
}

/// The dense key front (§2 decision 7) reads the group key's extrema from the resident zone summary; `PGRUST_GRAVITY_DENSE=0` turns it off.
pub(crate) fn dense_on() -> bool {
    static OFF: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    !*OFF.get_or_init(|| gravity_driver::env::os("PGRUST_GRAVITY_DENSE").is_some_and(|v| v == "0"))
}

/// D3014 S2', D3137: `PGRUST_GRAVITY_RERUN_SECTIONS=0` runs every nested member (a rerun's, an Init's) on the leader and
/// binds it at width 0 (the off switch); read once per process (D3865: it was an environment read at every runner plan's
/// prepare), as the planner's mirror (`nest_sublink`) reads it.
pub(crate) fn rerun_sections_on() -> bool {
    static ON: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *ON.get_or_init(|| gravity_driver::env::num::<u32>("PGRUST_GRAVITY_RERUN_SECTIONS") != Some(0))
}

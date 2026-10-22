//! The one-owner fold (`own_single`) inside the grant (D193, lane MEMORY): it cuts its region in the idle leader region (the ask,
//! or a two-round shape's floor with round 2 spilling), else pins inside the grant's room, else refuses by name; never a draw
//! past the reservation. The pool is armed at exactly the program's footprint, so any such draw would fail the statement.
#[path = "../../parallel/tests/common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use gravity_abi::breaker::{ORDER_KEY, ORDER_NONE};
use gravity_driver::{footprint, Entry, Error, Grant, Reservation, OWN_BOUNDED, OWN_CUT, OWN_PAST_GRANT, OWN_PINNED, POOL};
use gravity_kernels::table::params as par_hashagg;
use gravity_kernels::table::params::{DEFAULT_PARAMS, PARAM_ORDER, PARAM_SPILL_BITS, PARAM_TOPK_N};
use gravity_parallel::{FixturePool, HashAggPartial};
use gravity_program::{hand as programs, Code, End, OutCol, OwnerDesc, Pipeline, Program, Reason, SiteDesc, WritePlan};

static LOCK: Mutex<()> = Mutex::new(());

struct Keyed {
    inner: FixturePlan,
    merge: HashAggPartial,
}

impl gravity_parallel::Plan for Keyed {
    fn width_of(&self, p: usize) -> u16 {
        self.inner.width_of(p)
    }
    fn claims_for(&self, p: usize) -> &dyn gravity_parallel::MorselSource {
        self.inner.claims_for(p)
    }
    fn merge_for(&self, _p: usize) -> &dyn gravity_driver::Merge {
        &self.merge
    }
}

fn declared(rows: u64, d: OwnerDesc) -> Program {
    let st = gravity_program::Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), T_KV, &[1, 2])];
    let out = [OutCol::new("k", programs::INT4, -1, 4, true).origin(T_KV, 1), OutCol::new("sum", programs::INT8, -1, 8, true), OutCol::new("count", programs::INT8, -1, 8, true)];
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(par_hashagg::breaker(0)), rows).owner(d), Code::hand(par_hashagg::par_hashagg)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(par_hashagg::par_hashagg_emit)),
    ];
    Program::with_pipelines(gravity_program::Layout { nsites: 1, out_natts: 3, out_rows: 16, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes)
}

fn gen_100(r: u64) -> Option<[i64; 2]> {
    Some([(r % 100) as i64, r as i64])
}

fn gen_wide(r: u64) -> Option<[i64; 2]> {
    Some([r as i64, 1])
}

/// One statement admitted at the program's footprint over a pool of exactly that many bytes.
fn admitted(program: &Arc<Program>, grant: Grant, w: u16, run: impl FnOnce(Rc<Reservation>) -> gravity_driver::Result<u64>) -> gravity_driver::Result<u64> {
    admitted_with(program, grant, w, 0, run)
}

/// `admitted` with `headroom` bytes of the pool past the statement's reservation (a draw past its grant, D254 / D285).
fn admitted_with(program: &Arc<Program>, grant: Grant, w: u16, headroom: u64, run: impl FnOnce(Rc<Reservation>) -> gravity_driver::Result<u64>) -> gravity_driver::Result<u64> {
    let need = footprint(grant, program, w);
    POOL.arm(need + headroom);
    // D3353: the pool is armed at exactly the need, so an earlier statement's bytes still on their way back (a release
    // that lags under load) must be waited for: admission waits while another holder can give bytes back, and returns at
    // once when the room is there, so the long cap costs nothing on a quiet box (10 ms ran out on loaded ones)
    let a = Rc::new(POOL.admit_need(grant, &|g| footprint(g, program, w), Duration::from_secs(5)).unwrap());
    // D4043: a want at or under the floor that waited takes the floor road's same-size trim (`Grant::trimmed`), which
    // drops no byte and carries the trim mark: the grant is the asked one but for that mark
    assert_eq!((Grant { trimmed: false, ..a.grant }, a.reserved), (grant, need), "admitted whole");
    let r = run(a);
    POOL.arm(0);
    r
}

#[test]
fn the_d193_group_by_at_w4_folds_its_one_owner_in_the_leader_region_in_every_form() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let table = FixtureTable::new(T_KV, 40, 50, gen_100);
    let mut derive = FixtureDerive::new(&[&table]);
    let pool = FixturePool::new(4);
    let mut entry = Entry::new(2);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    let all: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u64, *s as u64, *c as u64]).collect();
    for (form, order_slot, topk) in [("unordered", ORDER_NONE, 0u32), ("key-ordered", ORDER_KEY, 0), ("top-k", ORDER_NONE, 5)] {
        let d = OwnerDesc::generic(par_hashagg::LAYOUT, order_slot, topk);
        let program = Arc::new(declared(2000, d));
        let mut params = DEFAULT_PARAMS;
        params[PARAM_ORDER] = (order_slot == ORDER_KEY) as u64;
        params[PARAM_TOPK_N] = topk as u64;
        let expect: Vec<Vec<u64>> = if topk > 0 { all.iter().take(topk as usize).cloned().collect() } else { all.clone() };
        table.fresh();
        let (cut, pinned) = (OWN_CUT.load(Ordering::Relaxed), OWN_PINNED.load(Ordering::Relaxed));
        let mut sink = LegSink::new(true);
        let rows = admitted(&program, Grant::of(8 << 20), 4, |lease| {
            let plan = Keyed { inner: FixturePlan::new(&program, &derive, 4, 7), merge: HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut params.clone(), 3, None))) };
            run_planned_in(&mut entry, &program, &mut derive, &pool, &plan, lease, &params, &NO_ABORT, &mut sink)
        })
        .unwrap_or_else(|e| panic!("{form}: {e:?}"));
        assert_eq!(rows, expect.len() as u64, "{form}");
        assert_eq!(sink.sorted(), { let mut v = expect.clone(); v.sort(); v }, "{form}");
        if order_slot == ORDER_KEY || topk > 0 {
            assert_eq!(sink.rows(), expect, "{form}: the emitted order");
        }
        assert!(OWN_CUT.load(Ordering::Relaxed) > cut, "{form}: the leader folds its few records as the one owner, in the leader region");
        assert_eq!(OWN_PINNED.load(Ordering::Relaxed), pinned, "{form}: nothing pinned");
    }
}

fn gen_1000(r: u64) -> Option<[i64; 2]> {
    Some([(r % 1000) as i64, (r % 7) as i64])
}

#[test]
fn a_planner_estimate_a_thousand_times_the_groups_sizes_nothing_the_real_records_decide() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let table = FixtureTable::new(T_KV, 40, 250, gen_1000);
    let mut derive = FixtureDerive::new(&[&table]);
    let pool = FixturePool::new(4);
    let mut entry = Entry::new(2);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    let mut expect: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u64, *s as u64, *c as u64]).collect();
    expect.sort();
    // 1,000 groups declared as 1,000,000: the shape's floor (tens of MiB) passes the leader region and the grant's room
    let d = OwnerDesc { groups: 1000 * groups.len() as u32, ..OwnerDesc::generic(par_hashagg::LAYOUT, ORDER_NONE, 0) };
    let program = Arc::new(declared(10_000, d));
    let params = DEFAULT_PARAMS;
    for w in [2u16, 4] {
        table.fresh();
        let (cut, pinned) = (OWN_CUT.load(Ordering::Relaxed), OWN_PINNED.load(Ordering::Relaxed));
        let mut sink = LegSink::new(true);
        let rows = admitted(&program, Grant::of(8 << 20), w, |lease| {
            let plan = Keyed { inner: FixturePlan::new(&program, &derive, w, 7), merge: HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut params.clone(), 3, None))) };
            run_planned_in(&mut entry, &program, &mut derive, &pool, &plan, lease, &params, &NO_ABORT, &mut sink)
        })
        .unwrap_or_else(|e| panic!("width {w}: {e:?}"));
        assert_eq!(rows, expect.len() as u64, "width {w}");
        assert_eq!(sink.sorted(), expect, "width {w}");
        assert!(OWN_CUT.load(Ordering::Relaxed) > cut, "width {w}: one owner over the leader region");
        assert_eq!(OWN_PINNED.load(Ordering::Relaxed), pinned, "width {w}: nothing pinned");
    }
}

fn gen_100k(r: u64) -> Option<[i64; 2]> {
    Some([(r * 7919 % 100_000) as i64, (r % 5) as i64])
}

/// The archived v2ledger D193 statement's shape (lowerv2 is out of the build): 100,000 groups at W4 over a pool of exactly the
/// footprint; the one owner's region is the real records' layout, inside the leader region.
#[test]
fn a_hundred_thousand_groups_at_w4_fold_their_one_owner_inside_the_grant() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    install_mem_spill();
    let table = FixtureTable::new(T_KV, 400, 500, gen_100k);
    let mut derive = FixtureDerive::new(&[&table]);
    let pool = FixturePool::new(4);
    let mut entry = Entry::new(2);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    let mut expect: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u64, *s as u64, *c as u64]).collect();
    expect.sort();
    let d = OwnerDesc { groups: groups.len() as u32, ..OwnerDesc::generic(par_hashagg::LAYOUT, ORDER_NONE, 0) };
    let program = Arc::new(declared(200_000, d));
    let params = DEFAULT_PARAMS;
    for grant in [8u64 << 20, 64 << 20] {
        table.fresh();
        let mut sink = LegSink::new(true);
        let rows = admitted(&program, Grant::of(grant), 4, |lease| {
            let plan = Keyed { inner: FixturePlan::new(&program, &derive, 4, 33), merge: HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut params.clone(), 3, None))) };
            run_planned_in(&mut entry, &program, &mut derive, &pool, &plan, lease, &params, &NO_ABORT, &mut sink)
        })
        .unwrap_or_else(|e| panic!("grant {grant}: {e:?}"));
        assert_eq!(rows, expect.len() as u64, "grant {grant}");
        assert_eq!(sink.sorted(), expect, "grant {grant}");
    }
}

/// A pool that runs claim `c` (in the cursor's order) on worker `assign(c) % width`: the same rows under a forced assignment.
struct Assigned {
    assign: fn(usize) -> u16,
}

impl gravity_parallel::Pool for Assigned {
    fn threads(&self) -> u16 {
        4
    }

    fn run(&self, width: u16, claims: &dyn gravity_parallel::MorselSource, job: &gravity_parallel::Job<'_>) -> Result<(), gravity_parallel::FirstError> {
        let mut all = Vec::new();
        while let Some(r) = claims.claim() {
            all.push(r);
        }
        let funnel = gravity_parallel::ErrorFunnel::new();
        std::thread::scope(|s| {
            for o in 0..width {
                let (all, funnel, assign) = (&all, &funnel, self.assign);
                s.spawn(move || {
                    for (c, r) in all.iter().enumerate() {
                        if assign(c) % width == o {
                            if let Err(e) = job(o, *r) {
                                funnel.offer(r.lo, o, e);
                                return;
                            }
                        }
                    }
                });
            }
        });
        funnel.take().map_or(Ok(()), Err)
    }
}

/// D220: the verdict is the real groups', never the scheduling's. The 100,000-group statement at W4 with every claim (rows and
/// owner partitions) forced onto worker 0 (skewed: owner 0 overflows its slice of the leader region, `own_single` redoes the
/// fold) and round-robin (spread: the four owners serve): one reservation, one serve / refuse verdict per grant.
#[test]
fn d220_the_one_owner_verdict_is_the_same_whether_a_groups_rows_land_on_one_worker_or_two() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    install_mem_spill();
    let table = FixtureTable::new(T_KV, 400, 500, gen_100k);
    let mut derive = FixtureDerive::new(&[&table]);
    let mut entry = Entry::new(2);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    let mut expect: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u64, *s as u64, *c as u64]).collect();
    expect.sort();
    let d = OwnerDesc { groups: groups.len() as u32, ..OwnerDesc::generic(par_hashagg::LAYOUT, ORDER_NONE, 0) };
    let program = Arc::new(declared(200_000, d));
    let params = DEFAULT_PARAMS;
    let skewed = Assigned { assign: |_| 0 };
    let spread = Assigned { assign: |c| c as u16 };
    for grant in [1u64 << 20, 8 << 20] {
        let mut verdicts = Vec::new();
        for (name, pool) in [("skewed", &skewed), ("spread", &spread)] {
            table.fresh();
            let (bounded, pinned, cut, past) = (OWN_BOUNDED.load(Ordering::Relaxed), OWN_PINNED.load(Ordering::Relaxed), OWN_CUT.load(Ordering::Relaxed), OWN_PAST_GRANT.load(Ordering::Relaxed));
            let mut sink = LegSink::new(true);
            let mut reserved = 0;
            let r = admitted_with(&program, Grant::of(grant), 4, 256 << 20, |lease| {
                reserved = lease.reserved;
                let plan = Keyed { inner: FixturePlan::new(&program, &derive, 4, 50), merge: HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut params.clone(), 3, None))) };
                run_planned_in(&mut entry, &program, &mut derive, pool, &plan, lease, &params, &NO_ABORT, &mut sink)
            });
            let served = match r {
                Ok(rows) => {
                    assert_eq!(rows, expect.len() as u64, "{name} at {grant}");
                    assert_eq!(sink.sorted(), expect, "{name} at {grant}");
                    true
                }
                Err(Error::Refused(m)) if m.contains("leader region") && m.contains("grant's") => false,
                Err(e) => panic!("{name} at {grant}: {e:?}"),
            };
            if name == "skewed" && grant == 8 << 20 {
                // DR-12: the footprint reserved the records' bound the estimate's rows give, so the one owner is placed inside the
                // room: pinned there, or (KN-02 (B)) laid in the chunk the overflowed owners were pinned in
                assert_eq!(OWN_BOUNDED.load(Ordering::Relaxed), bounded, "skewed: the one owner's records' bound is within the room");
                let laid = OWN_PAST_GRANT.load(Ordering::Relaxed) > past && OWN_CUT.load(Ordering::Relaxed) > cut;
                assert!(OWN_PINNED.load(Ordering::Relaxed) > pinned || laid, "skewed: the one owner past the leader region is placed inside the room");
            }
            verdicts.push((name, reserved, served));
        }
        assert_eq!((verdicts[0].1, verdicts[0].2), (verdicts[1].1, verdicts[1].2), "grant {grant}: {verdicts:?}");
        // D285: the 1 MiB statement's one owner outgrows its bounded region and is placed again past the grant, never refused
        assert!(verdicts[0].2, "grant {grant}: 100,000 groups served, past the 1 MiB statement's leader region and room (D285)");
    }
}

/// D285 (was: refused by name, D220): a one-round one owner whose records' bound passes the leader region and the grant's
/// room, after an estimate a hundredth of the groups, is placed again past the grant (the pool's room past the reservation,
/// counted as grown) and the statement returns every group. With no pool room past the reservation the one error is a
/// real allocation failure, out of memory (53200), never a refusal.
#[test]
fn a_one_round_owner_past_the_leader_region_and_the_grants_room_grows_past_the_grant() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    install_mem_spill();
    let table = FixtureTable::new(T_KV, 2000, 100, gen_wide);
    let mut derive = FixtureDerive::new(&[&table]);
    let pool = FixturePool::new(4);
    let mut entry = Entry::new(2);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    let mut expect: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u64, *s as u64, *c as u64]).collect();
    expect.sort();
    // an estimate a hundredth of the rows and the groups: the pool's owner regions overflow and `own_single` asks for every
    // record twice, past what the footprint reserved (DR-12 reserves the bound the estimate's rows give)
    let d = OwnerDesc { groups: 2000, ..OwnerDesc::generic(par_hashagg::LAYOUT, ORDER_NONE, 0) };
    let program = Arc::new(declared(2_000, d));
    let params = DEFAULT_PARAMS;
    for (headroom, grows) in [(256u64 << 20, true), (0, false)] {
        table.fresh();
        let mut sink = LegSink::new(true);
        let mut grown = 0;
        let r = admitted_with(&program, Grant::of(1 << 20), 4, headroom, |lease| {
            let plan = Keyed { inner: FixturePlan::new(&program, &derive, 4, 33), merge: HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut params.clone(), 3, None))) };
            let r = run_planned_in(&mut entry, &program, &mut derive, &pool, &plan, lease.clone(), &params, &NO_ABORT, &mut sink);
            grown = lease.grown();
            r
        });
        match (r, grows) {
            (Ok(rows), true) => {
                assert_eq!(rows, expect.len() as u64);
                assert_eq!(sink.sorted(), expect, "every group, the one owner past the grant");
                assert!(grown > 0, "the owner region past the grant is counted as grown");
            }
            // D2990: with no pool room the one owner's region is the session temp file mapped shared (D491's road), not
            // OutOfMemory: every group, nothing grown
            (Ok(rows), false) => {
                assert_eq!(rows, expect.len() as u64);
                assert_eq!(sink.sorted(), expect, "every group, the one owner in the mapped temp file");
                assert_eq!(grown, 0, "the mapped owner region is not the pool's");
            }
            (other, _) => panic!("headroom {headroom}: {other:?}"),
        }
    }
}

/// DR-12: the same groups after a hundredth-groups estimate over the right rows: the footprint reserves the records' bound
/// those rows give past the leader region, so with no pool room past the reservation the one owner pins inside it and
/// the statement returns every group, nothing grown.
#[test]
fn a_parallel_owner_whose_rows_estimate_bounds_its_records_pins_inside_its_reservation() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    install_mem_spill();
    let table = FixtureTable::new(T_KV, 2000, 100, gen_wide);
    let mut derive = FixtureDerive::new(&[&table]);
    let pool = FixturePool::new(4);
    let mut entry = Entry::new(2);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    let mut expect: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u64, *s as u64, *c as u64]).collect();
    expect.sort();
    let d = OwnerDesc { groups: 2000, ..OwnerDesc::generic(par_hashagg::LAYOUT, ORDER_NONE, 0) };
    let program = Arc::new(declared(200_000, d));
    let params = DEFAULT_PARAMS;
    let (bounded, pinned) = (OWN_BOUNDED.load(Ordering::Relaxed), OWN_PINNED.load(Ordering::Relaxed));
    let mut sink = LegSink::new(true);
    let mut grown = u64::MAX;
    let r = admitted(&program, Grant::of(1 << 20), 4, |lease| {
        let plan = Keyed { inner: FixturePlan::new(&program, &derive, 4, 33), merge: HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut params.clone(), 3, None))) };
        let r = run_planned_in(&mut entry, &program, &mut derive, &pool, &plan, lease.clone(), &params, &NO_ABORT, &mut sink);
        grown = lease.grown();
        r
    });
    assert_eq!(r.unwrap_or_else(|e| panic!("{e:?}")), expect.len() as u64);
    assert_eq!(sink.sorted(), expect, "every group");
    assert_eq!(grown, 0, "nothing drawn past the grant");
    assert_eq!(OWN_BOUNDED.load(Ordering::Relaxed), bounded, "the records' bound is within the room");
    assert!(OWN_PINNED.load(Ordering::Relaxed) > pinned, "the one owner past the leader region pins inside the room");
}

/// D440: an owner phase that fails ends its statement as one that returns does. `Entry::end` releases the node
/// (`settle_regions`): every slot life drops and every region the fold cut or pinned parks, settled, in the depot. So at the
/// statement's end G has no child life and no byte charged, and nothing but the caller holds the lease. Once the lease
/// drops, the pool holds only the depot's parked regions, and the next statement draws what the one before the error drew.
/// The failing statement's arena census line prints before that release (`Entry::release`), so it still shows the regions.
#[test]
fn d440_an_owner_error_releases_its_lives_and_regions_at_the_statements_end() {
    use gravity_driver::REGIONS;
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    install_mem_spill();
    let table = FixtureTable::new(T_KV, 2000, 100, gen_wide);
    let mut derive = FixtureDerive::new(&[&table]);
    let pool = FixturePool::new(4);
    let mut entry = Entry::new(2);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    let mut expect: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u64, *s as u64, *c as u64]).collect();
    expect.sort();
    // the D285 shape: an estimate a hundredth of the rows and the groups, so the one owner outgrows its bounded region and is
    // pinned again past the grant, into the pool's headroom; with none that pin fails out of memory, mid owner phase
    let d = OwnerDesc { groups: 2000, ..OwnerDesc::generic(par_hashagg::LAYOUT, ORDER_NONE, 0) };
    let program = Arc::new(declared(2_000, d));
    let params = DEFAULT_PARAMS;
    // D3125: at W4 a statement's draw past its grant follows its fold's and owner phase's schedule, not only the statement
    // before it: on loaded boxes the statement BEFORE the error drew 98 MB (3x: the owners' chunk sized from three times the
    // records, or regrown) against the usual 34 MB. Each statement's release is asserted on its own; the draws compare across
    // a sequence that runs again (at most three more times) while they differ, each try's draws and owner paths
    // (OWN_PAST_GRANT, OWN_OVERFLOW) in the message. A release that leaks fails every try.
    for w in [4u16, 1] {
        let mut tries = Vec::new();
        loop {
            REGIONS.clear();
            let mut draws = Vec::new();
            let mut paths = Vec::new();
            for (headroom, ok) in [(256u64 << 20, true), (0, false), (256 << 20, true)] {
                table.fresh();
                let mut sink = LegSink::new(true);
                let mut end = None;
                let past = (OWN_PAST_GRANT.load(Ordering::Relaxed), gravity_parallel::OWN_OVERFLOW.load(Ordering::Relaxed));
                let r = admitted_with(&program, Grant::of(1 << 20), w, headroom, |lease| {
                    let plan = Keyed { inner: FixturePlan::new(&program, &derive, w, 33), merge: HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut params.clone(), 3, None))) };
                    let r = run_planned_in(&mut entry, &program, &mut derive, &pool, &plan, lease.clone(), &params, &NO_ABORT, &mut sink);
                    let g = lease.grant_ctx().expect("an admitted statement's G");
                    end = Some((Rc::strong_count(&lease), g.stats_tree().children.len(), g.subtree_used(), lease.drawn(), lease.grown()));
                    r
                });
                let at = format!("W{w} headroom {headroom}");
                match (r, ok) {
                    (Ok(rows), true) => {
                        assert_eq!(rows, expect.len() as u64, "{at}");
                        assert_eq!(sink.sorted(), expect, "{at}: every group");
                    }
                    // D2990: no pool room takes D491's mapped temp file for the one owner: every group, no error
                    (Ok(rows), false) => {
                        assert_eq!(rows, expect.len() as u64, "{at}");
                        assert_eq!(sink.sorted(), expect, "{at}: every group, the one owner in the mapped temp file");
                    }
                    (Err(Error::OutOfMemory { .. }), false) => {}
                    (other, _) => panic!("{at}: {other:?}"),
                }
                let (refs, lives, used, drawn, grown) = end.expect("the statement ran");
                let depot = REGIONS.stats();
                assert_eq!((refs, lives, used), (1, 0, 0), "{at}: at the statement's end no slot life and no charge is left under G, and the lease is the caller's alone");
                assert_eq!(POOL.held(), depot.bytes as u64, "{at}: past the statement the pool holds only the depot's parked regions");
                draws.push((drawn, grown));
                paths.push((OWN_PAST_GRANT.load(Ordering::Relaxed) - past.0, gravity_parallel::OWN_OVERFLOW.load(Ordering::Relaxed) - past.1));
            }
            assert!(draws[1].0 > 0, "W{w}: the failing statement drew its owner regions before the pin failed");
            let same = draws[2] == draws[0];
            tries.push((draws, paths));
            if same {
                break;
            }
            assert!(tries.len() < 4, "W{w}: the statement after the error draws what the one before it drew: every try differs (draws, owner paths past / overflow): {tries:?}");
        }
    }
}

#[test]
fn the_one_owner_region_is_its_exact_layout_and_one_byte_short_places_elsewhere() {
    use gravity_driver::{one_owner_bytes, owner_place, OwnerPlace};
    let rec = par_hashagg::LAYOUT.size() as usize;
    let held = 1000 * rec;
    let need = one_owner_bytes(held, rec);
    // exactly enough leader region cuts; one byte short pins inside the room when it holds the need, else refuses by name
    assert_eq!(owner_place(need, need, false, Some(0)), OwnerPlace::Cut(need));
    assert_eq!(owner_place(need, need - 1, false, Some(need as u64)), OwnerPlace::Pin);
    assert_eq!(owner_place(need, need - 1, false, Some(need as u64 - 1)), OwnerPlace::Refuse(need as u64 - 1));
    assert_eq!(owner_place(need, need - 1, false, None), OwnerPlace::Pin, "no admitted statement: unbounded");
    // the generic owner lays out its runs and table in exactly that region (exec/src/owner.rs `owner_open`)
    let d = OwnerDesc::generic(par_hashagg::LAYOUT, ORDER_NONE, 0);
    let fns = gravity_exec::table::owner::bind(&d, &mut DEFAULT_PARAMS.clone(), 3, None);
    let mut region = vec![0u64; need / 8];
    let merge = HashAggPartial::new(0, Some(fns));
    gravity_driver::Merge::open_owner(&merge, gravity_driver::sink::Region { ptr: region.as_mut_ptr() as *mut u8, len: need }, 0).expect("the exact region lays out");
    let top = unsafe { gravity_abi::breaker::OwnerTop::view(region.as_ptr() as *const u8) };
    assert!(top.table_off as usize + held <= need && top.table_off as usize >= held, "runs hold every record; the table half holds them too");
}

/// D438's keys: two groups whose routes (`KeyHash::word`, `k * PHI64` below 2^32) share their top 16 bits, the partition
/// bits and the three splits' (both fit the int4 output column).
fn d438_keys() -> (i64, i64) {
    (2, 46370)
}

fn gen_d438(r: u64) -> Option<[i64; 2]> {
    let (a, b) = d438_keys();
    Some([if r % 2 == 0 { a } else { b }, r as i64])
}

#[test]
fn d438_two_groups_whose_routes_share_every_split_bit_fold_in_the_one_owner() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (ka, kb) = d438_keys();
    let (a, b) = (gravity_abi::hash::KeyHash::of(&[ka as u64]), gravity_abi::hash::KeyHash::of(&[kb as u64]));
    assert_eq!((a.part(2), a.sub(2, 3)), (b.part(2), b.sub(2, 3)), "the keys collide past the third split");
    let table = FixtureTable::new(T_KV, 40, 50, gen_d438);
    let mut derive = FixtureDerive::new(&[&table]);
    let pool = FixturePool::new(4);
    let mut entry = Entry::new(2);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    let expect: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u64, *s as u64, *c as u64]).collect();
    let d = OwnerDesc::generic(par_hashagg::LAYOUT, ORDER_NONE, 0);
    let program = Arc::new(declared(2000, d));
    // two partition bits: the estimate behind them keeps the phase-1 table (no bypass); one claim of every block: two records held
    let mut params = DEFAULT_PARAMS;
    params[PARAM_SPILL_BITS] = 2;
    for w in [1u16, 4] {
        table.fresh();
        let mut sink = LegSink::new(true);
        // W1: the one owner's region at the two records' bytes: its table holds one record, the two groups fill it at every depth
        let rows = admitted(&program, Grant::of(8 << 20), w, |lease| {
            let plan = Keyed { inner: FixturePlan::new(&program, &derive, w, 64), merge: HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut params.clone(), 2, None))) };
            run_planned_in(&mut entry, &program, &mut derive, &pool, &plan, lease, &params, &NO_ABORT, &mut sink)
        })
        .unwrap_or_else(|e| panic!("W{w}: {e:?}"));
        assert_eq!(rows, 2, "W{w}");
        assert_eq!(sink.sorted(), expect, "W{w}");
    }
}

/// Key `i`'s route (`KeyHash::word`) is `i << 32 | (i ^ 1)`: keys under 2^12 share their top 20 route bits (up to 11
/// partition bits and the three splits' nine), keys under 2^16 their top 16. Their low 32 bits differ (an int4 output tells
/// them apart; the low route word `i ^ 1` was found by search so that they do under the current mix, which the tests assert).
fn adv_key(i: u64) -> i64 {
    unword((i << 32) | (i ^ 1)) as i64
}

/// The one-word key whose `KeyHash::word` is `h` (abi's `word_preimage`).
fn unword(h: u64) -> u64 {
    gravity_abi::hash::word_preimage(h)
}

fn gen_adv_4096(r: u64) -> Option<[i64; 2]> {
    Some([adv_key(r % 4096), (r / 4096) as i64])
}

fn gen_adv_65536(r: u64) -> Option<[i64; 2]> {
    Some([adv_key(r % 65536), (r / 65536) as i64])
}

/// The controls: as many keys, routes spread (`KeyHash::word(i)`).
fn gen_plain_65536(r: u64) -> Option<[i64; 2]> {
    Some([(r % 65536) as i64, (r / 65536) as i64])
}

#[test]
fn d445_crafted_keys_route_where_they_say() {
    for i in [0u64, 1, 7, 4095, 65535] {
        let h = gravity_abi::hash::KeyHash::of(&[adv_key(i) as u64]);
        assert_eq!((h.part(16), h.sub(16, 5)), (0, (i >> 1) & 0x7fff), "{i}: the route is i << 32 | i");
    }
}

/// D445's keys through the one-round owner at three estimates and partition bits, W1 and W4: D438's refold folds them.
#[test]
fn d445_a_one_round_owner_over_the_same_keys_folds_them() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    install_mem_spill(); // D1629: the spill factory is process-wide; never rely on a sibling test having installed it
    let pool = FixturePool::new(4);
    let mut entry = Entry::new(2);
    let mut outcomes = Vec::new();
    for (name, blocks, per, gen) in [("adv 4096x8", 512u64, 64u64, gen_adv_4096 as fn(u64) -> Option<[i64; 2]>), ("adv 65536x2", 2048, 64, gen_adv_65536), ("plain 65536x2", 2048, 64, gen_plain_65536)] {
        let table = FixtureTable::new(T_KV, blocks, per, gen);
        let mut derive = FixtureDerive::new(&[&table]);
        let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
        for [k, v] in table.rows() {
            let g = groups.entry(k).or_insert((0, 0));
            g.0 += v;
            g.1 += 1;
        }
        // an int4 output column: both sides compared on the key's low 32 bits
        assert_eq!(groups.keys().map(|k| *k as u32).collect::<std::collections::BTreeSet<_>>().len(), groups.len(), "{name}: the int4 output needs keys with distinct low 32 bits");
        let mut expect: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u32 as u64, *s as u64, *c as u64]).collect();
        expect.sort();
        for (est, bits) in [(2000u64, 2u64), (groups.len() as u64, 0), (100, 4)] {
            let d = OwnerDesc { groups: est as u32, ..OwnerDesc::generic(par_hashagg::LAYOUT, ORDER_NONE, 0) };
            let program = Arc::new(declared(blocks * per, d));
            let mut params = DEFAULT_PARAMS;
            if bits != 0 {
                params[PARAM_SPILL_BITS] = bits;
            }
            for w in [1u16, 4] {
                table.fresh();
                let mut sink = LegSink::new(true);
                let r = admitted_with(&program, Grant::of(8 << 20), w, 256 << 20, |lease| {
                    let plan = Keyed { inner: FixturePlan::new(&program, &derive, w, 64), merge: HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut params.clone(), 3, None))) };
                    run_planned_in(&mut entry, &program, &mut derive, &pool, &plan, lease, &params, &NO_ABORT, &mut sink)
                });
                let mut got: Vec<Vec<u64>> = sink.sorted().into_iter().map(|row| vec![row[0] as u32 as u64, row[1], row[2]]).collect();
                got.sort();
                let ok = matches!(r, Ok(n) if n == expect.len() as u64) && got == expect;
                outcomes.push((name, est, bits, w, format!("{r:?}"), ok));
            }
        }
    }
    assert!(outcomes.iter().all(|o| o.5), "{outcomes:?}");
}


/// D446's keys: `KeyHash::word(k)` = 3 << 16 | i, so 65536 keys agree in their top 48 route bits: a split divides them only
/// past the old 32-bit wall (depth 15 and more after 3 partition bits). The prefix 3 was found by search so that the keys'
/// low 32 bits differ under the current mix (the int4 output tells them apart; the test asserts it).
fn gen_d446(r: u64) -> Option<[i64; 2]> {
    Some([unword(3 << 16 | (r % 65536)) as i64, (r / 65536) as i64])
}

/// D446's keys through the one-round owner at W1 and W4 (D438's refold, the deeper split behind it).
#[test]
fn d446_a_one_round_owner_over_the_same_keys_folds_them() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    install_mem_spill(); // D1629: the spill factory is process-wide; never rely on a sibling test having installed it
    let pool = FixturePool::new(4);
    let mut entry = Entry::new(2);
    let table = FixtureTable::new(T_KV, 2048, 64, gen_d446);
    let mut derive = FixtureDerive::new(&[&table]);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    assert_eq!(groups.keys().map(|k| *k as u32).collect::<std::collections::BTreeSet<_>>().len(), groups.len(), "the int4 output needs keys with distinct low 32 bits");
    let mut expect: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u32 as u64, *s as u64, *c as u64]).collect();
    expect.sort();
    let mut outcomes = Vec::new();
    for (est, bits) in [(2000u64, 2u64), (100, 4)] {
        let d = OwnerDesc { groups: est as u32, ..OwnerDesc::generic(par_hashagg::LAYOUT, ORDER_NONE, 0) };
        let program = Arc::new(declared(2048 * 64, d));
        let mut params = DEFAULT_PARAMS;
        params[PARAM_SPILL_BITS] = bits;
        for w in [1u16, 4] {
            table.fresh();
            let mut sink = LegSink::new(true);
            let r = admitted_with(&program, Grant::of(8 << 20), w, 256 << 20, |lease| {
                let plan = Keyed { inner: FixturePlan::new(&program, &derive, w, 64), merge: HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut params.clone(), 3, None))) };
                run_planned_in(&mut entry, &program, &mut derive, &pool, &plan, lease, &params, &NO_ABORT, &mut sink)
            });
            let mut got: Vec<Vec<u64>> = sink.sorted().into_iter().map(|row| vec![row[0] as u32 as u64, row[1], row[2]]).collect();
            got.sort();
            outcomes.push((est, bits, w, format!("{r:?}"), matches!(r, Ok(n) if n == expect.len() as u64) && got == expect));
        }
    }
    assert!(outcomes.iter().all(|o| o.4), "{outcomes:?}");
}

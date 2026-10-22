//! The program-declared owner phase (milestone-2.md §2.6 items 12–13): `par_hashagg`'s hand
//! program under its own owner functions against the same program declaring an `OwnerDesc` and
//! bound to `gravity_exec::owner::owner_fns` — the same groups at every width and claim, in the
//! unordered, key-ordered and top-k forms; the descriptor enters the front key.
#[path = "../../parallel/tests/common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, SlotKind, Spill, ORDER_KEY, ORDER_NONE};
use gravity_driver::Entry;
use gravity_kernels::table::params as par_hashagg;
use gravity_kernels::table::params::{DEFAULT_PARAMS, NPARAMS, PARAM_ORDER, PARAM_TOPK_N};
use gravity_parallel::{FixturePool, HashAggPartial};
use gravity_program::{hand as programs, Code, End, OutCol, OwnerDesc, Pipeline, Program, Reason, SiteDesc, WritePlan};

struct KeyedPlan {
    inner: FixturePlan,
    merge: HashAggPartial,
}

impl gravity_parallel::Plan for KeyedPlan {
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

/// `par_hashagg_with` with pipeline 0 declaring the owner phase over the kernel's layout.
fn declared(rel: u32, rows: u64, d: OwnerDesc) -> Program {
    let st = gravity_program::Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), rel, &[1, 2])];
    let out = [OutCol::new("k", programs::INT4, -1, 4, true).origin(rel, 1), OutCol::new("sum", programs::INT8, -1, 8, true), OutCol::new("count", programs::INT8, -1, 8, true)];
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(par_hashagg::breaker(0)), rows).owner(d), Code::hand(par_hashagg::par_hashagg)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(par_hashagg::par_hashagg_emit)),
    ];
    Program::with_pipelines(gravity_program::Layout { nsites: 1, out_natts: 3, out_rows: 16, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes)
}

fn spill_bits(p: &Program) -> u8 {
    match p.pipelines[0].breaker() {
        Some(Breaker { spill: Spill::Partitioned { bits }, .. }) => bits,
        _ => 0,
    }
}

fn run(entry: &mut Entry, program: &Arc<Program>, derive: &mut FixtureDerive, pool: &FixturePool, w: u16, claim: u64, merge: HashAggPartial, params: &[u64]) -> (u64, Vec<Vec<u64>>, Vec<Vec<u64>>) {
    let plan = KeyedPlan { inner: FixturePlan::new(program, derive, w, claim), merge };
    let mut sink = LegSink::new(true);
    let rows = run_planned(entry, program, derive, pool, &plan, gravity_driver::Grant::of(64 << 20), params, &NO_ABORT, &mut sink).unwrap();
    (rows, sink.rows(), sink.sorted())
}

#[test]
fn the_declared_owner_phase_gives_the_hand_kernels_groups_in_every_form() {
    let table = FixtureTable::new(T_KV, 160, 60, gen_kv);
    let mut derive = FixtureDerive::new(&[&table]);
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    // `all` in the signed key order (the ordered forms' emitted order); the sorted views compare as u64 rows
    let all: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u64, *s as u64, *c as u64]).collect();
    let sorted = |rows: &[Vec<u64>]| {
        let mut v = rows.to_vec();
        v.sort();
        v
    };
    let hand = Arc::new(programs::par_hashagg_with(T_KV, 16, 9600));
    let bits = spill_bits(&hand);
    for (form, order_slot, topk) in [("unordered", ORDER_NONE, 0u32), ("key-ordered", ORDER_KEY, 0), ("top-k", ORDER_NONE, 5)] {
        let d = OwnerDesc::generic(par_hashagg::LAYOUT, order_slot, topk);
        let decl = Arc::new(declared(T_KV, 9600, d));
        assert_eq!(decl.pipelines[0].owner, Some(d));
        assert_ne!(decl.front_key(), hand.front_key(), "{form}: the descriptor is part of the shape");
        assert_eq!(decl.pipelines[0].breaker(), hand.pipelines[0].breaker());
        let mut params = DEFAULT_PARAMS;
        params[PARAM_ORDER] = (order_slot == ORDER_KEY) as u64;
        params[PARAM_TOPK_N] = topk as u64;
        let expect: Vec<Vec<u64>> = if topk > 0 { all.iter().take(topk as usize).cloned().collect() } else { all.clone() };
        for w in WIDTHS {
            for claim in [2u64, 33] {
                table.fresh();
                let h = run(&mut entry, &hand, &mut derive, &pool, w, claim, HashAggPartial::new(0, Some(par_hashagg::owner_fns(&params))), &params);
                table.fresh();
                let g = run(&mut entry, &decl, &mut derive, &pool, w, claim, HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut params.clone(), bits, None))), &params);
                assert_eq!(h.0, expect.len() as u64, "{form} width {w} claim {claim}: hand rows");
                assert_eq!(h.2, sorted(&expect), "{form} width {w} claim {claim}: hand groups");
                assert_eq!(g.0, h.0, "{form} width {w} claim {claim}: rows");
                assert_eq!(g.2, h.2, "{form} width {w} claim {claim}: groups");
                if order_slot == ORDER_KEY || topk > 0 {
                    assert_eq!(g.1, h.1, "{form} width {w} claim {claim}: the emitted order");
                    assert_eq!(g.1, expect, "{form} width {w} claim {claim}: key-ascending");
                }
                assert!(table.ledger.consumed_once(), "{form} width {w} claim {claim}");
            }
        }
    }
}

/// The generic phase serves the count-ordered form (`count_topk`: a Count slot and a bound); a count order
/// without a bound is not a form and is refused at construction.
#[test]
#[should_panic(expected = "count-ordered owner form")]
fn the_count_ordered_form_without_a_bound_is_refused_at_construction() {
    let _ = Pipeline::eligible(0, End::Breaker(par_hashagg::breaker(0)), 1).owner(OwnerDesc::generic(par_hashagg::LAYOUT, 0, 0));
}

#[test]
fn the_count_ordered_form_with_a_bound_is_declared() {
    let slot = par_hashagg::LAYOUT.slot_kinds().iter().position(|s| matches!(s, gravity_abi::breaker::SlotKind::Count)).expect("a Count slot") as u8;
    let p = Pipeline::eligible(0, End::Breaker(par_hashagg::breaker(0)), 1).owner(OwnerDesc::count_topk(par_hashagg::LAYOUT, slot, 3));
    assert_eq!(p.owner.map(|o| (o.order_slot, o.topk)), Some((slot, 3)));
}

/// D-06: the generic runs form under a top (`OwnerDesc::runs_top`: n, the direction, the count slot) lays, of each
/// partition's groups, those at or past that partition's n-th best count: every group of the top's n best counts and of
/// their ties, at every width and claim, each with its final values; at n = 1 not every group, past the groups all.
#[test]
fn the_runs_form_under_a_top_lays_every_group_of_the_top_and_its_ties() {
    let table = FixtureTable::new(T_KV, 160, 60, gen_kv);
    let mut derive = FixtureDerive::new(&[&table]);
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    let kinds = par_hashagg::LAYOUT.slot_kinds();
    let count = kinds.iter().position(|s| matches!(s, SlotKind::Count)).expect("a Count slot") as u8;
    let hand = programs::par_hashagg_with(T_KV, 16, 9600);
    let bits = spill_bits(&hand);
    let params = DEFAULT_PARAMS;
    let mut all: Vec<Vec<u64>> = groups.iter().map(|(g, (s, c))| vec![*g as u64, *s as u64, *c as u64]).collect();
    all.sort();
    for (n, below) in [(1u32, false), (1, true), (3, false), (7, true), (200, false)] {
        let d = OwnerDesc { floor: OwnerDesc::runs_top(n, below, count), ..OwnerDesc::generic(par_hashagg::LAYOUT, ORDER_NONE, 0) };
        assert_eq!(d.runs_top_of(), Some((n, below, count)));
        assert_eq!(d.runs_floor_of(), None);
        let mut counts: Vec<i64> = groups.values().map(|g| g.1).collect();
        counts.sort_by(|a, b| if below { a.cmp(b) } else { b.cmp(a) });
        let cut = counts[(n as usize).min(counts.len()) - 1];
        let top: Vec<&Vec<u64>> = all.iter().filter(|r| if below { r[2] as i64 <= cut } else { r[2] as i64 >= cut }).collect();
        let decl = Arc::new(declared(T_KV, 9600, d));
        for w in WIDTHS {
            for claim in [2u64, 33] {
                table.fresh();
                let g = run(&mut entry, &decl, &mut derive, &pool, w, claim, HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut params.to_vec(), bits, None))), &params);
                let at = format!("n {n} below {below} width {w} claim {claim}");
                assert_eq!(g.0, g.2.len() as u64, "{at}: rows");
                assert!(g.2.iter().all(|r| all.contains(r)), "{at}: a laid group's values are its final ones");
                assert!(top.iter().all(|r| g.2.contains(r)), "{at}: every group of the top and its ties is laid ({} of {})", g.2.len(), top.len());
                if n == 1 {
                    assert!(g.2.len() < all.len(), "{at}: the top drops some groups ({} of {})", g.2.len(), all.len());
                }
                if n as usize >= all.len() {
                    assert_eq!(g.2, all, "{at}: a bound past the groups lays them all");
                }
                assert!(table.ledger.consumed_once(), "{at}");
            }
        }
    }
}

/// D3153: the generic runs form under a bound (`OwnerDesc::runs_floor`, K read from its bound word at bind) lays only the
/// groups within it, at every width and claim: `count > K`, `count >= K`, an int4 K whose word's high half is not its sign
/// (the bind sign-extends the low half), `count < K`, and the sum slot's `sum >= K` / `sum <= K`. The other groups are
/// never gathered.
#[test]
fn the_runs_form_under_a_count_floor_lays_only_the_groups_over_it() {
    let table = FixtureTable::new(T_KV, 160, 60, gen_kv);
    let mut derive = FixtureDerive::new(&[&table]);
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    let least = groups.values().map(|g| g.1).min().expect("groups");
    let most = groups.values().map(|g| g.1).max().expect("groups");
    let mut sums: Vec<i64> = groups.values().map(|g| g.0).collect();
    sums.sort();
    let mid = sums[sums.len() / 2];
    let kinds = par_hashagg::LAYOUT.slot_kinds();
    let count = kinds.iter().position(|s| matches!(s, SlotKind::Count)).expect("a Count slot") as u8;
    let sum = kinds.iter().position(|s| matches!(s, SlotKind::SumI64)).expect("a SumI64 slot") as u8;
    let hand = programs::par_hashagg_with(T_KV, 16, 9600);
    let bits = spill_bits(&hand);
    let params = DEFAULT_PARAMS;
    let cases = [(count, false, true, false, least, least as u64), (count, false, false, false, least + 1, (least + 1) as u64), (count, false, true, true, least, least as u32 as u64 | 0xdead_beef << 32), (count, true, true, false, most, most as u64), (sum, false, false, false, mid, mid as u64), (sum, true, false, false, mid, mid as u64)];
    for (slot, below, gt, int4, k, word) in cases {
        let d = OwnerDesc { floor: OwnerDesc::runs_floor(NPARAMS as u16, gt, int4, below, slot), ..OwnerDesc::generic(par_hashagg::LAYOUT, ORDER_NONE, 0) };
        assert_eq!(d.runs_floor_of(), Some((NPARAMS as u16, gt, int4, below, slot)));
        let mut words = params.to_vec();
        words.push(word);
        let keeps = |v: i64| match (below, gt) {
            (false, true) => v > k,
            (false, false) => v >= k,
            (true, true) => v < k,
            (true, false) => v <= k,
        };
        let mut expect: Vec<Vec<u64>> = groups.iter().filter(|(_, g)| keeps(if slot == sum { g.0 } else { g.1 })).map(|(g, (s, c))| vec![*g as u64, *s as u64, *c as u64]).collect();
        expect.sort();
        assert!(!expect.is_empty() && expect.len() < groups.len(), "the bound keeps some groups and drops some ({} of {})", expect.len(), groups.len());
        let decl = Arc::new(declared(T_KV, 9600, d));
        for w in WIDTHS {
            for claim in [2u64, 33] {
                table.fresh();
                let g = run(&mut entry, &decl, &mut derive, &pool, w, claim, HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut words.clone(), bits, None))), &params);
                assert_eq!(g.0, expect.len() as u64, "slot {slot} below {below} gt {gt} int4 {int4} width {w} claim {claim}: rows");
                assert_eq!(g.2, expect, "slot {slot} below {below} gt {gt} int4 {int4} width {w} claim {claim}: groups");
                assert!(table.ledger.consumed_once(), "slot {slot} below {below} gt {gt} int4 {int4} width {w} claim {claim}");
            }
        }
    }
}

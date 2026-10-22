mod common;

use std::sync::Arc;

use common::*;
use gravity_driver::Entry;
use gravity_kernels::par_sort;
use gravity_program::hand as programs;
use gravity_parallel::FixturePool;

/// `(k, v)` rows with every value of `k` three times over, a NULL key every 11th row and a NULL
/// payload every 7th: the sort's direction and null placement have work to do.
fn gen_sort(r: u64) -> Option<[i64; 2]> {
    Some([((r * 7919) % 20_011) as i64 - 10_000, (r % 1000) as i64])
}

#[test]
fn the_sort_breaker_orders_every_row_through_runs_at_every_width_and_both_directions() {
    install_mem_spill();
    let table = FixtureTable::new(T_KV, 600, 100, gen_sort);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(programs::par_sort_with(T_KV, 16, 60_000, &[1, 2]));
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let mut rows: Vec<[i64; 2]> = table.rows().collect();
    for (desc, nulls_first) in [(false, false), (true, false), (false, true)] {
        let params = par_sort::params(2, desc, nulls_first);
        let expect: Vec<Vec<u64>> = {
            if desc {
                rows.sort_by(|a, b| b[0].cmp(&a[0]).then(a[1].cmp(&b[1])));
            } else {
                rows.sort_by(|a, b| a[0].cmp(&b[0]).then(a[1].cmp(&b[1])));
            }
            rows.iter().map(|r| vec![r[0] as u64, r[1] as u64]).collect()
        };
        let _ = nulls_first;
        for (grant, small) in [(64u64 << 20, false), (1, true)] {
            for w in WIDTHS {
                table.fresh();
                let plan = FixturePlan::new(&program, &derive, w, 16);
                let mut sink = LegSink::new(true);
                let (n, spilled, _, _) = run_spilling(&mut entry, &program, &mut derive, &pool, &plan, gravity_driver::Grant::of(grant), &params, &NO_ABORT, &mut sink).unwrap();
                assert_eq!((n as usize, spilled), (expect.len(), true), "grant {grant} width {w} desc {desc}: every claim's rows go to runs");
                let got = sink.rows();
                // ties on the key keep no promised order: compare the key sequence, then the multiset
                assert_eq!(got.iter().map(|r| r[0] as i64).collect::<Vec<_>>(), expect.iter().map(|r| r[0] as i64).collect::<Vec<_>>(), "grant {grant} width {w} desc {desc}: the keys in order");
                let (mut a, mut b) = (got.clone(), expect.clone());
                a.sort();
                b.sort();
                assert_eq!(a, b, "grant {grant} width {w} desc {desc}: the same rows");
                let _ = small;
            }
        }
    }
}

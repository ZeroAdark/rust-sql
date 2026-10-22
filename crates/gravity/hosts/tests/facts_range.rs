//! M3-L6J: `RelcacheFacts::att_estimate` on a heap relation is the `pg_statistic` bounds (the MCV list's and the histogram's extrema over an int-word type), none without statistics or over another type; `att_range` stays the exact answer (none on heap).
use gravity_hosts_rig as common;

use std::sync::Mutex;

use common::*;
use datum::Datum;
use gravity_decompose::Facts;
use gravity_hosts::facts::RelcacheFacts;
use mcx::{MemoryContext, PgVec};
use syscache_seams::{PgStatisticBundle, PgStatisticSlotData};

/// The fixture's statistics for the heap relation: `(stakind, valuetype, values)` per slot; none = no `pg_statistic` row.
static STATS: Mutex<Option<Vec<(i16, u32, Vec<i64>)>>> = Mutex::new(None);

fn install_stats() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        syscache_seams::lookup_pg_statistic_bundle::set(|mcx, relid, attnum, inh| {
            assert_eq!((relid, attnum, inh), (HEAP_OID, 1, false));
            let Some(slots) = STATS.lock().unwrap().clone() else { return Ok(None) };
            let mut out = PgVec::new_in(mcx);
            for (kind, typ, vals) in slots {
                let mut values = PgVec::new_in(mcx);
                for v in vals {
                    values.push(match typ {
                        16 => Datum::from_bool(v != 0),
                        21 => Datum::from_i16(v as i16),
                        23 => Datum::from_i32(v as i32),
                        _ => Datum::from_i64(v),
                    });
                }
                out.push(PgStatisticSlotData::from_decoded(kind, 0, 0, typ, values, PgVec::new_in(mcx), PgVec::new_in(mcx)));
            }
            Ok(Some(PgStatisticBundle { stanullfrac: 0.0, stawidth: 4, stadistinct: 0.0, slots: out }))
        });
    });
}

fn estimate(stats: Option<Vec<(i16, u32, Vec<i64>)>>) -> (Option<(i64, i64)>, Option<(i64, i64)>) {
    *STATS.lock().unwrap() = stats;
    let ctx = MemoryContext::new("facts-range");
    let facts = RelcacheFacts::new(ctx.mcx());
    (facts.att_range(HEAP_OID, 1), facts.att_estimate(HEAP_OID, 1))
}

#[test]
fn heap_att_estimate_is_the_statistics_bounds_and_att_range_stays_none() {
    let _g = serial();
    install();
    reset_fixture();
    install_stats();
    assert_eq!(estimate(None), (None, None), "no statistics: no estimate, the table road");
    assert_eq!(estimate(Some(vec![(2, 23, vec![-3, 0, 7, 22])])), (None, Some((-3, 22))), "the histogram's ends");
    assert_eq!(estimate(Some(vec![(1, 23, vec![5, 40, 2])])), (None, Some((2, 40))), "the MCV list's extrema");
    assert_eq!(estimate(Some(vec![(1, 23, vec![5, 40]), (2, 23, vec![-3, 0, 7, 22]), (3, 23, vec![99])])), (None, Some((-3, 40))), "MCV and histogram folded, the correlation slot ignored");
    assert_eq!(estimate(Some(vec![(2, 16, vec![0, 1])])), (None, Some((0, 1))), "a bool histogram");
    assert_eq!(estimate(Some(vec![(2, 21, vec![-7, 9])])), (None, Some((-7, 9))), "int2 values");
    assert_eq!(estimate(Some(vec![(2, 20, vec![i64::MIN + 1, i64::MAX])])), (None, Some((i64::MIN + 1, i64::MAX))), "int8 values");
    assert_eq!(estimate(Some(vec![(2, 25, vec![1, 2])])), (None, None), "a text histogram: no int-word domain");
    assert_eq!(estimate(Some(vec![(3, 23, vec![99])])), (None, None), "only a correlation slot: no bounds");
}

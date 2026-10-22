//! The `record` host (M2-work §B.5): on a zeroed partial it opens the one record (the sentinel 1, the table's len 1) and answers the same pointer on every call.

use gravity_kernels_rig as common;

use common::*;
use gravity_abi::breaker::{HashAgg, Rec, RecordLayout};
use gravity_abi::hash::KeyHash;
use gravity_abi::program::partial;
use gravity_kernels::contract::entry2;
use gravity_kernels::host_abi::Record;
use gravity_kernels::par_scan_count;
use gravity_program::hand::{self as programs, Oids};

const LAYOUT: RecordLayout = RecordLayout::with_sentinel(&[], &[], true, None);
const BYTES: u32 = HashAgg::bytes(&LAYOUT, 1);

#[test]
fn record_opens_the_one_record_once_and_answers_the_same_pointer() {
    assert!(BYTES <= par_scan_count::BYTES, "the test layout fits the rig's partial");
    let mut rig = Rig::new(programs::par_scan_count(Oids::T_A), vec![]);
    let ctx = rig.ctx();
    unsafe {
        let own = partial(ctx);
        assert!(core::slice::from_raw_parts(own, BYTES as usize).iter().all(|&b| b == 0), "a zeroed partial before the open");
        let p = entry2::<Record>(ctx as u64, &LAYOUT as *const RecordLayout as u64, BYTES as u64) as *mut u8;
        assert!(!p.is_null());
        let r = Rec::at(p, &LAYOUT);
        assert!(r.used(), "the record is open");
        assert_eq!(r.count(LAYOUT.sentinel_slot()), 1, "the sentinel counts the open");
        let t = &*(own as *const HashAgg);
        assert_eq!((t.cap, t.len), (1, 1), "a one-record table with its record");
        let q = entry2::<Record>(ctx as u64, &LAYOUT as *const RecordLayout as u64, BYTES as u64) as *mut u8;
        assert_eq!(p, q, "the second open answers the same record");
        assert_eq!(Rec::at(q, &LAYOUT).count(LAYOUT.sentinel_slot()), 1, "the second open counts nothing");
        assert_eq!((*(own as *const HashAgg)).len, 1);
    }
}

use gravity_abi::breaker::{KeyKind, SlotKind, SortRun, TopK, T1, TABLE_T1};
use gravity_abi::ctx::resume;
use gravity_kernels::host_abi::{agg_finish, agg_miss, agg_open, sort_begin, sort_full, sort_last, sort_next, sort_open, sort_stop, topk_insert, topk_open, topk_order, topk_run, AGG_STATE_BYTES, SORT_MODE_MEM, SORT_MODE_MERGE};

const AGG_LAYOUT: RecordLayout = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count, SlotKind::SumI64]);

#[test]
fn agg_open_arms_a_t1_over_a_zeroed_share_region() {
    let mut rig = Rig::new(programs::par_hashagg(Oids::T_A), vec![]);
    let ctx = rig.ctx();
    let mut st = [0u64; AGG_STATE_BYTES as usize / 8];
    unsafe {
        let own = partial(ctx);
        assert_eq!(T1::view(own).cap, 0, "a zeroed region before the open");
        assert_eq!(agg_open(ctx, st.as_mut_ptr() as *mut u8, &AGG_LAYOUT, 0, 0), 0);
        let t = T1::view(own);
        assert_eq!((t.kind, t.pbits, t.len), (TABLE_T1, 4, 0), "the T1 armed at the default partition bits");
        assert!(t.cap >= 2 && t.cap.is_power_of_two());
        assert_eq!(st[0], own as u64, "the state names the table");
        assert_eq!(st[3], t.cap as u64 - 1, "the slot mask");
        assert_eq!(st[6], 15, "the partition mask");
    }
}

/// `bits` 1: the table path (the miss opens the record in the table, the finish drains it); the default bits take the bypass (appended straight to the chain).
fn miss_then_finish(bits: u64, in_table: u32) {
    let mut rig = Rig::new(programs::par_hashagg(Oids::T_A), vec![]);
    let ctx = rig.ctx();
    let mut st = [0u64; AGG_STATE_BYTES as usize / 8];
    let mut img = [0u8; 256];
    unsafe {
        assert_eq!(agg_open(ctx, st.as_mut_ptr() as *mut u8, &AGG_LAYOUT, bits, 0), 0);
        let src = Rec::at(img.as_mut_ptr(), &AGG_LAYOUT);
        src.open(&[42]);
        src.add_count(0, 1);
        src.add_i64(1, 7);
        assert_eq!(agg_miss(ctx, st.as_mut_ptr() as *mut u8, img.as_ptr()), 0);
        let t = T1::view(partial(ctx));
        assert_eq!(t.len, in_table, "the miss's record in the table (bits {bits})");
        assert_eq!(agg_finish(ctx, st.as_mut_ptr() as *mut u8), 0);
        assert_eq!(t.len, 0, "the finish drained the table");
        let part = KeyHash::of(&[42]).part(t.pbits as u32);
        let recs: Vec<_> = t.chain(part).collect();
        assert_eq!(recs.iter().map(|b| b.recs).sum::<usize>(), 1, "one record in its partition's chain");
        let r = Rec::at(recs[0].ptr as *mut u8, &AGG_LAYOUT);
        assert_eq!((r.keys(), r.count(0), r.i64(1)), (&[42u64][..], 1, 7));
        for p in 0..t.parts() as u32 {
            if p != part {
                assert_eq!(t.chain(p).count(), 0, "the other partitions stay empty");
            }
        }
    }
}

#[test]
fn agg_miss_then_finish_leave_one_record_in_its_partition_chain() {
    miss_then_finish(1, 1);
    miss_then_finish(0, 0);
}

/// D118: a miss image of an implicit-count layout carries the record form (`1 + rows`); the table's record, opened at 1,
/// ends at `1 + rows` — a one-row group whose image carried its rows alone stayed at 1 and rendered its sum NULL.
#[test]
fn agg_miss_of_an_implicit_count_keeps_its_rows() {
    const NN: RecordLayout = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::SumI64]);
    let s = NN.sentinel_slot();
    for bits in [1, 0] {
        let mut rig = Rig::new(programs::par_hashagg(Oids::T_A), vec![]);
        let ctx = rig.ctx();
        let mut st = [0u64; AGG_STATE_BYTES as usize / 8];
        let mut img = [0u8; 256];
        unsafe {
            assert_eq!(agg_open(ctx, st.as_mut_ptr() as *mut u8, &NN, bits, 0), 0);
            let src = Rec::at(img.as_mut_ptr(), &NN);
            src.open(&[42]);
            src.add_count(s, 1);
            src.add_i64(0, 7);
            assert_eq!(agg_miss(ctx, st.as_mut_ptr() as *mut u8, img.as_ptr()), 0);
            assert_eq!(agg_finish(ctx, st.as_mut_ptr() as *mut u8), 0);
            let t = T1::view(partial(ctx));
            let recs: Vec<_> = t.chain(KeyHash::of(&[42]).part(t.pbits as u32)).collect();
            let r = Rec::at(recs[0].ptr as *mut u8, &NN);
            assert_eq!((r.count(s), r.i64(0)), (2, 7), "bits {bits}: the one row's record reads opened with its row");
        }
    }
}

#[test]
fn topk_open_insert_order_keep_the_n_smallest() {
    let mut rig = Rig::new(programs::par_topk(Oids::T_A, 3), vec![]);
    let ctx = rig.ctx();
    unsafe {
        let run = topk_open(ctx, 3, 3, 2, 0);
        assert!(!run.is_null());
        assert_eq!(run, topk_run(ctx), "the emit's run is the region");
        let mut entered = 0;
        for k in [9i64, 4, 7, 1, 8, 2, 6, 3, 5, 0] {
            let v = [k as u64, (k * 10) as u64];
            entered += topk_insert(run, v.as_ptr(), [0u8, 0].as_ptr(), 2);
        }
        assert!(entered >= 3);
        assert_eq!(topk_order(run, 3), 3);
        let t = TopK::view(run);
        let keys: Vec<(i64, u64)> = (0..3).map(|i| (t.key_of(i), t.row(i).0[1])).collect();
        assert_eq!(keys, vec![(0, 0), (1, 10), (2, 20)]);
    }
}

const ROW_BYTES: u32 = 40;
const SORT_TBL: [u32; 6] = [ROW_BYTES, 2, 2, 0, 0, 0];

unsafe fn push_row(run: *mut u8, key: i64, tag: u64) {
    let p = (*(run as *mut SortRun)).push() as *mut u64;
    *p = 1;
    *p.add(1) = (key ^ i64::MIN) as u64;
    *p.add(2) = key as u64;
    *p.add(3) = tag;
    *(p.add(4) as *mut u8) = 0;
    *(p.add(4) as *mut u8).add(1) = 0;
}

fn shuffled() -> Vec<i64> {
    (0..100).map(|i| (i * 37 + 11) % 100).collect()
}

#[test]
fn sort_open_full_begin_next_round_trip_100_rows_through_two_runs() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    let ctx = rig.ctx();
    unsafe {
        let run = sort_open(ctx, ROW_BYTES as u64, 2, 0);
        assert!(!run.is_null());
        for (i, k) in shuffled().into_iter().enumerate() {
            if i == 50 {
                assert_eq!(sort_full(ctx, run, 2, 0), 0, "the first run put");
                assert_eq!(rig.host.runs.len(), 1);
            }
            push_row(run, k, i as u64);
        }
        assert_eq!(sort_last(ctx, run, 2, 0), 0, "the last run put behind the first");
        assert_eq!(rig.host.runs.len(), 2);
        assert_eq!(sort_begin(ctx, SORT_TBL.as_ptr()), SORT_MODE_MERGE);
        let mut got = Vec::new();
        loop {
            let p = sort_next(ctx, SORT_TBL.as_ptr(), SORT_MODE_MERGE as u64, got.len() as u64);
            assert!(p as i64 >= 0, "a status below zero");
            if p.is_null() {
                break;
            }
            got.push(*(p as *const u64).add(2) as i64);
        }
        assert_eq!(got, (0..100).collect::<Vec<i64>>());
    }
}

/// D1301: a stop inside row `j` (a write's recheck, an expanding step's offset) resumes by reading row `j` again with no
/// hold word; the merge hands the same row, as SORT_MODE_MEM re-reads it by index, and row `j + 1` follows.
#[test]
fn a_merge_emit_read_again_at_its_last_row_hands_the_same_row() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    let ctx = rig.ctx();
    unsafe {
        let run = sort_open(ctx, ROW_BYTES as u64, 2, 0);
        for (i, k) in shuffled().into_iter().enumerate() {
            if i == 50 {
                assert_eq!(sort_full(ctx, run, 2, 0), 0, "the first run put");
            }
            push_row(run, k, i as u64);
        }
        assert_eq!(sort_last(ctx, run, 2, 0), 0, "the last run put behind the first");
        assert_eq!(sort_begin(ctx, SORT_TBL.as_ptr()), SORT_MODE_MERGE);
        let key = |j: u64| -> i64 {
            let p = sort_next(ctx, SORT_TBL.as_ptr(), SORT_MODE_MERGE as u64, j);
            assert!(!p.is_null() && p as i64 > 0, "row {j}");
            *(p as *const u64).add(2) as i64
        };
        let mut got = Vec::new();
        for j in 0..100u64 {
            let k = key(j);
            if j % 7 == 3 {
                assert_eq!(key(j), k, "row {j} read again after its stop");
            }
            got.push(k);
        }
        assert!(sort_next(ctx, SORT_TBL.as_ptr(), SORT_MODE_MERGE as u64, 100).is_null());
        assert_eq!(got, (0..100).collect::<Vec<i64>>());
    }
}

#[test]
fn sort_last_seals_the_leaders_run_in_memory_and_a_stop_resumes_at_its_row() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    let ctx = rig.ctx();
    unsafe {
        let run = sort_open(ctx, ROW_BYTES as u64, 2, 0);
        for (i, k) in shuffled().into_iter().enumerate() {
            push_row(run, k, i as u64);
        }
        assert_eq!(sort_last(ctx, run, 2, 0), 0);
        assert_eq!(rig.host.runs.len(), 0, "no run put: the run stays in the region");
        assert_eq!(sort_begin(ctx, SORT_TBL.as_ptr()), SORT_MODE_MEM);
        let row = |i: u64| -> *const u8 { sort_next(ctx, SORT_TBL.as_ptr(), SORT_MODE_MEM as u64, i) };
        let keys: Vec<i64> = (0..100).map(|i| *(row(i) as *const u64).add(2) as i64).collect();
        assert_eq!(keys, (0..100).collect::<Vec<i64>>());
        assert!(row(100).is_null(), "the end");
        sort_stop(ctx, SORT_TBL.as_ptr(), SORT_MODE_MEM as u64, 8, 1);
        assert_eq!((*ctx).resume, resume::word(resume::EMIT, 7), "the held row is re-delivered");
        assert_eq!(sort_begin(ctx, SORT_TBL.as_ptr()), SORT_MODE_MEM, "the resume reads the sealed run");
        assert_eq!(*(row(resume::position((*ctx).resume)) as *const u64).add(2), 7);
    }
}

use gravity_abi::breaker::MINCODE_NONE;
use gravity_abi::lane::EMPTY_CODE_NONE;
use gravity_abi::status::{unsupported, UNSUPPORTED_SHAPE};
use gravity_kernels::host_abi::{agg_bind, agg_render, BIND_BITS, BIND_FORM_WORDS, BIND_FP, BIND_LEN, BIND_OUT_WORDS};
use gravity_kernels::util::{advance, seek};

/// The count program over one code lane and its table lane (lane 0 the codes, 1 the offset, 2 the table): the site shape of a
/// `count(*) WHERE x LIKE ..` over a dictionary column; its scan kernel is never called (the tests drive the hosts over an
/// open window).
fn code_table_program(rel: u32, attno: i16) -> gravity_program::Program {
    use gravity_abi::breaker::{Breaker, BreakerKind};
    use gravity_program::{Code, End, LaneDesc, Layout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};
    unsafe extern "C-unwind" fn never(_: *mut gravity_abi::Ctx) -> i64 {
        unreachable!("the host tests never run the scan pipeline")
    }
    let st = gravity_program::Arena::new(16 << 10);
    let sites = [SiteDesc::range_source(st.handle(), rel, &[attno]).codes_lane(0).with_lane(LaneDesc::table(0), false)];
    let out = [OutCol::new("count", programs::INT8, -1, 8, true)];
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, par_scan_count::BYTES)), 0), Code::hand(never)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(par_scan_count::par_scan_count_emit)),
    ];
    Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes)
}

/// A rig over `code_table_program`'s site with a window open: `dict` the entries, `tables` the site's table lanes.
fn dict_rig(tables: Vec<RigTable>) -> (Rig, Vec<Vec<u8>>) {
    let dict: Vec<Vec<u8>> = ["", "http://garden.com/", "search", "mail.garden.com", "plain"].iter().map(|s| s.as_bytes().to_vec()).collect();
    let rows_for: RowsFor = |_, _| (0..20u64).map(|r| (vec![r % 5], vec![0u8])).collect();
    let mut rig = Rig::new(code_table_program(Oids::T_A, 1), vec![0]);
    rig.site(0).rows_for = rows_for;
    rig.site(0).codes_lanes = 0b1;
    rig.site(0).dict = dict.clone();
    rig.site(0).tables = tables;
    let ctx = rig.ctx();
    unsafe {
        assert_eq!(seek(ctx, 0), 0);
        assert!(advance(ctx, 0) > 0, "a window");
    }
    (rig, dict)
}

#[test]
fn agg_bind_fills_a_like_bit_table_once_per_window_and_reads_it_back() {
    let (mut rig, dict) = dict_rig(vec![RigTable::Like { pattern: b"%garden%".to_vec(), negated: false }]);
    let ctx = rig.ctx();
    let spec = [2u64 | (BIND_BITS as u64) << 16];
    let mut out = [0u64; BIND_OUT_WORDS + 1];
    unsafe {
        assert_eq!(agg_bind(ctx as u64, 0, spec.as_ptr() as u64, 1, out.as_mut_ptr() as u64), 0);
        let words = out[0] as *const u64;
        for (c, e) in dict.iter().enumerate() {
            let bit = *words.add(c / 64) >> (c % 64) & 1;
            assert_eq!(bit == 1, e.windows(6).any(|w| w == b"garden"), "code {c}");
        }
        assert_ne!(out[1], 0, "the fault words (a zero page)");
        assert_eq!(*(out[1] as *const u64), 0);
        assert_eq!((out[2], out[3]), (BIND_FORM_WORDS, 1), "no faults");
        assert_eq!(rig.host.calls.iter().filter(|c| c.starts_with("lane(")).count(), 1, "one fill");
        // the bits read as a length or a fingerprint leave by shape
        for class in [BIND_LEN, BIND_FP] {
            let spec = [2u64 | (class as u64) << 16];
            assert_eq!(agg_bind(ctx as u64, 0, spec.as_ptr() as u64, 1, out.as_mut_ptr() as u64), unsupported(UNSUPPORTED_SHAPE), "class {class}");
        }
    }
}

#[test]
fn agg_bind_reads_a_length_and_a_fingerprint_table() {
    let (mut rig, dict) = dict_rig(vec![RigTable::OctetLength]);
    let ctx = rig.ctx();
    let spec = [2u64 | (BIND_LEN as u64) << 16];
    let mut out = [0u64; BIND_OUT_WORDS + 1];
    unsafe {
        assert_eq!(agg_bind(ctx as u64, 0, spec.as_ptr() as u64, 1, out.as_mut_ptr() as u64), 0);
        let lens = core::slice::from_raw_parts(out[0] as *const i64, dict.len());
        assert_eq!(lens, dict.iter().map(|e| e.len() as i64).collect::<Vec<_>>());
        assert_eq!(out[2], BIND_FORM_WORDS);
    }
    let (mut rig, dict) = dict_rig(vec![RigTable::Fingerprint]);
    let ctx = rig.ctx();
    let spec = [2u64 | (BIND_FP as u64) << 16];
    unsafe {
        assert_eq!(agg_bind(ctx as u64, 0, spec.as_ptr() as u64, 1, out.as_mut_ptr() as u64), 0);
        let fps = core::slice::from_raw_parts(out[0] as *const [u64; 2], dict.len());
        let distinct: std::collections::HashSet<[u64; 2]> = fps.iter().copied().collect();
        assert_eq!(distinct.len(), dict.len(), "distinct fingerprints per entry");
    }
}

#[test]
fn agg_render_answers_the_entry_image_the_empty_literal_and_null() {
    let (mut rig, dict) = dict_rig(vec![]);
    let ctx = rig.ctx();
    unsafe {
        for (c, e) in dict.iter().enumerate() {
            let p = agg_render(ctx as u64, 0, 0, c as u64) as *const u8;
            assert!(!p.is_null(), "code {c}");
            assert_eq!(gravity_kernels::dict::reps::varlena_bytes(p), e.as_slice(), "code {c}");
        }
        assert!((agg_render(ctx as u64, 0, 0, MINCODE_NONE) as *const u8).is_null(), "no representative is NULL");
        let p = agg_render(ctx as u64, 0, 0, EMPTY_CODE_NONE as u64) as *const u8;
        assert_eq!(gravity_kernels::dict::reps::varlena_bytes(p), b"", "the guarded empty code");
        assert_eq!(rig.host.calls.iter().filter(|c| c.starts_with("dict(")).count(), dict.len(), "one dictionary call per rendered code");
    }
}

use gravity_abi::breaker::T1_BYPASS;

use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill, SORTRUN_HDR};
use gravity_kernels::host_abi::{spool_begin, spool_open};
use gravity_program::program::End;

const SPOOL_TBL: [u32; 6] = [ROW_BYTES, 0, 2, 0, 0, 0];

/// The sort program with its breaker end re-kinded Materialize (`Spill::Runs`, `Budget::Share`): the M8 spool's slot through the runtime's arms (M8-plan §4.7, C.0 verify).
fn spool_program() -> gravity_program::Program {
    let mut p = programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]);
    p.pipelines[0].ends_in = End::Breaker(Breaker { kind: BreakerKind::Materialize, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::Runs });
    p
}

#[test]
fn spool_open_sizes_the_run_for_its_rows_and_spool_begin_hands_the_sealed_run_back() {
    let mut rig = Rig::new(spool_program(), vec![]);
    let ctx = rig.ctx();
    unsafe {
        let run = spool_open(ctx, ROW_BYTES as u64, 23);
        assert!(!run.is_null());
        let r = &mut *(run as *mut SortRun);
        // D17: 2 × 23 rows round up to the run's floor, 4 KiB of rows (102 of 40 bytes), in a Materialize slot
        let floor = gravity_abi::breaker::SORTRUN_FLOOR_BYTES / ROW_BYTES;
        assert_eq!((r.cap, r.len, r.kind), (floor, 0, gravity_abi::breaker::TABLE_SORT), "2 × 23 rounds up to the 4 KiB floor");
        for (i, k) in shuffled().into_iter().enumerate() {
            if r.len == r.cap {
                assert!(r.grow(), "the run doubles within the share (row {i})");
            }
            push_row(run, k, i as u64);
        }
        assert_eq!(r.cap, floor, "100 rows fit the floor's run");
        assert_eq!(sort_last(ctx, run, 0, 0), 0, "a zero-key last: sealed in place, unsorted");
        assert_eq!(rig.host.runs.len(), 0, "no run put");
        let p = spool_begin(ctx, SPOOL_TBL.as_ptr());
        assert_eq!(p, run as *const u8, "MEM mode: the sealed run's pointer");
        assert_eq!((*(p as *const SortRun)).len, 100);
        let tags: Vec<u64> = (0..100).map(|i| *((*(p as *const SortRun)).rows_ptr().add(i * ROW_BYTES as usize) as *const u64).add(3)).collect();
        assert_eq!(tags, (0..100).collect::<Vec<u64>>(), "insertion order kept: the spool never sorts");
        assert_eq!(p.add(SORTRUN_HDR), (*(p as *const SortRun)).rows_ptr());
    }
}

#[test]
fn spool_begin_is_null_after_a_put_and_the_merge_drains_every_row() {
    let mut rig = Rig::new(spool_program(), vec![]);
    let ctx = rig.ctx();
    unsafe {
        let run = spool_open(ctx, ROW_BYTES as u64, 23);
        // D17: past the 4 KiB floor's 102 rows: 200 rows (the shuffled keys twice)
        for (i, k) in shuffled().into_iter().chain(shuffled()).enumerate() {
            if (*(run as *mut SortRun)).len == (*(run as *mut SortRun)).cap {
                assert_eq!(sort_full(ctx, run, 0, 0), 0, "the run put at its cap (the spill arm takes a Materialize slot)");
            }
            push_row(run, k, i as u64);
        }
        assert_eq!(sort_last(ctx, run, 0, 0), 0);
        assert_eq!(rig.host.runs.len(), 2, "102 + 98 rows in two runs");
        assert!(spool_begin(ctx, SPOOL_TBL.as_ptr()).is_null(), "MERGE mode: null, the slow arm reads through sort_next");
        let mut tags = Vec::new();
        loop {
            let p = sort_next(ctx, SPOOL_TBL.as_ptr(), SORT_MODE_MERGE as u64, tags.len() as u64);
            assert!(p as i64 >= 0, "a status below zero");
            if p.is_null() {
                break;
            }
            tags.push(*(p as *const u64).add(3));
        }
        tags.sort_unstable();
        assert_eq!(tags, (0..200).collect::<Vec<u64>>(), "every row once");
    }
}

/// M6-Q1-SPILL (D069): TPC-H Q1's shape — two code keys, four groups, numeric sums and averages and a count — opened with the
/// planner's stats-less estimate (5,998,605 groups on pgrc) and fed 200 k rows: the estimate only sizes the table, the table
/// never bypasses (no record reaches a partition chain before the finish), and the finish drains exactly one record per group —
/// the no-spill form. Before the rule the open set `T1_BYPASS` and every row became a record appended to its partition (the
/// Rig's 1 MiB share has no spill host: the pool refuses after ~10 k records).
#[test]
fn a_tiny_group_numeric_fold_under_a_huge_estimate_never_bypasses_and_drains_one_record_per_group() {
    const LAYOUT: RecordLayout = RecordLayout::new(&[KeyKind::I64, KeyKind::I64], &[SlotKind::SumNum, SlotKind::SumNum, SlotKind::AvgNum, SlotKind::Count]);
    const ROWS: i64 = 200_000;
    let mut rig = Rig::new(programs::par_hashagg(Oids::T_A), vec![]);
    let ctx = rig.ctx();
    let mut st = [0u64; AGG_STATE_BYTES as usize / 8];
    let mut img = [0u8; 256];
    unsafe {
        assert_eq!(agg_open(ctx, st.as_mut_ptr() as *mut u8, &LAYOUT, 0, 5_998_605), 0);
        let t = T1::view(partial(ctx));
        assert_eq!(t.flags & T1_BYPASS, 0, "the estimate never sets the bypass at the open");
        for i in 0..ROWS {
            img.fill(0);
            let src = Rec::at(img.as_mut_ptr(), &LAYOUT);
            src.open(&[(i % 2) as u64, ((i / 2) % 2) as u64]);
            src.add_i128(0, i as i128 * 100);
            src.add_i128(1, 7);
            src.add_avg(2, i as i128, 1);
            src.add_count(3, 1);
            assert_eq!(agg_miss(ctx, st.as_mut_ptr() as *mut u8, img.as_ptr()), 0, "row {i}");
        }
        assert_eq!(t.flags & T1_BYPASS, 0, "four groups never thrash the table: observe() keeps table mode");
        assert_eq!((0..t.parts() as u32).map(|p| t.chain(p).count()).sum::<usize>(), 0, "nothing appended before the finish");
        assert_eq!(t.len, 4, "the four groups in the table");
        assert_eq!(agg_finish(ctx, st.as_mut_ptr() as *mut u8), 0);
        let mut recs = 0;
        let mut total = 0i64;
        for p in 0..t.parts() as u32 {
            for b in t.chain(p) {
                for i in 0..b.recs {
                    let r = Rec::at((b.ptr as *mut u8).add(i * LAYOUT.size() as usize), &LAYOUT);
                    recs += 1;
                    total += r.count(3);
                    assert_eq!(r.count(3), ROWS / 4, "a group's row count");
                    assert_eq!(r.i128(1), 7 * (ROWS / 4) as i128, "a group's numeric sum");
                }
            }
        }
        assert_eq!((recs, total), (4, ROWS), "one record per group reaches the owners: parts carry 4 records, no spill");
    }
}


use gravity_abi::breaker::adapt_win;

/// D1448: the observe() bypass in `locate` opens the miss's record in the table (len + 1), then drains the table and moves the
/// record to its partition as a copy. The open is undone first: without it a layout whose sentinel is its explicit count left a
/// keys-only slot the drain skips (len 1 after the drain, breaker.rs `debug_assert_eq!(len, 0)`), and an implicit-count layout
/// appended the slot as an empty duplicate record. Both layouts: the table ends empty at the switch and each key reaches the
/// owners exactly once, with its rows.
fn observe_bypass_moves_the_miss_once(layout: &'static RecordLayout, count_slot: usize, count_base: i64) {
    let mut rig = Rig::new(programs::par_hashagg(Oids::T_A), vec![]);
    let ctx = rig.ctx();
    let mut st = [0u64; AGG_STATE_BYTES as usize / 8];
    let mut img = [0u8; 256];
    let sum_slot = 1 - count_slot.min(1);
    unsafe {
        assert_eq!(agg_open(ctx, st.as_mut_ptr() as *mut u8, layout, 1, 0), 0);
        let t = partial(ctx) as *mut T1;
        assert_eq!((*t).flags & T1_BYPASS, 0, "the table path at the open");
        // a thrashing window already observed: the next eviction's observe() answers bypass
        ((*t).inserts, (*t).win_at, (*t).evictions, (*t).win_ev) = (adapt_win((*t).cap as u64), 0, adapt_win((*t).cap as u64), 0);
        let (mut n, mut switched_at) = (0u64, None);
        while n < 20_000 && switched_at.map_or(true, |s| n < s + 50) {
            img.fill(0);
            let src = Rec::at(img.as_mut_ptr(), layout);
            src.open(&[n]);
            src.add_count(count_slot, 1);
            src.add_i64(sum_slot, n as i64);
            assert_eq!(agg_miss(ctx, st.as_mut_ptr() as *mut u8, img.as_ptr()), 0, "key {n}");
            if switched_at.is_none() && (*t).flags & T1_BYPASS != 0 {
                assert_eq!((*t).len, 0, "key {n}: the table drained at the switch, the bypassed miss not left open in it");
                switched_at = Some(n);
            }
            n += 1;
        }
        assert!(switched_at.is_some(), "an eviction came and observe() switched to the bypass");
        assert_eq!(agg_finish(ctx, st.as_mut_ptr() as *mut u8), 0);
        let t = &*t;
        let mut seen = vec![0u32; n as usize];
        for p in 0..t.parts() as u32 {
            for b in t.chain(p) {
                for i in 0..b.recs {
                    let r = Rec::at((b.ptr as *mut u8).add(i * layout.size() as usize), layout);
                    let k = r.keys()[0] as usize;
                    assert!(k < n as usize, "a record of a key fed: {k}");
                    seen[k] += 1;
                    assert_eq!((r.count(count_slot), r.i64(sum_slot)), (count_base + 1, k as i64), "key {k}: its one row");
                }
            }
        }
        assert!(seen.iter().all(|&s| s == 1), "every key exactly once: {:?}", seen.iter().enumerate().filter(|p| *p.1 != 1).take(5).collect::<Vec<_>>());
    }
}

#[test]
fn d1448_the_observe_bypass_undoes_its_open_before_the_drain() {
    // the explicit count is the sentinel (0 until the fold): the case the drain skipped
    observe_bypass_moves_the_miss_once(&AGG_LAYOUT, 0, 0);
    // the implicit count (opened at 1): the case the drain appended twice
    const NN: RecordLayout = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::SumI64]);
    observe_bypass_moves_the_miss_once(&NN, NN.sentinel_slot(), 1);
}

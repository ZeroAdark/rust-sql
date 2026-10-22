//! D597: a key outside the dense domain an owner was bound with (a stale domain: D580's retiring writer) never ends the
//! statement with XX000 where PostgreSQL returns a value. The dense reduce stops round 1 (`UNSUPPORTED_DOMAIN`), the merge
//! (`HashAggPartial::with_plain`) takes the form without the domain from then on and hands the driver its scratch road,
//! and round 1 folds again from the partials it only read, through the hash reduce: the same groups as the plain path and
//! as the rows say. Without a plain form the miss is an error by name, never a count short. The driver's rerun is the one
//! owner's (section.rs `own_all`: a fresh region opened, round 1 again), driven here over T1 partials.

use std::collections::{BTreeMap, BTreeSet};

use gravity_abi::breaker::{KeyKind, OwnerFns, OwnerTop, Rec, RecordLayout, SlotKind, ORDER_NONE, OWNERTOP_HDR, T1};
use gravity_abi::status::UNSUPPORTED_SCRATCH;
use gravity_driver::{Error, Merge, Partial, Region};
use gravity_exec::table::owner::owner_fns_dense;
use gravity_kernels::dict::reps::{MAX_REPS, REP_NONE};
use gravity_parallel::HashAggPartial;
use gravity_program::{OwnerDesc, ACC_PAIRS, ACC_SLOT_BASE};

const PBITS: u8 = 3;
const WORKERS: usize = 2;
const WORKER_BYTES: usize = 4 << 20;
const REGION_BYTES: usize = 16 << 20;

fn lcg(s: &mut u64) -> u64 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *s >> 17
}

/// `(k, x, v)` rows: `k` over `0..span`, `x` over `0..dom`.
fn rows(n: usize, span: u64, dom: u64, seed: u64) -> Vec<(i64, i64, i64)> {
    let mut s = seed;
    (0..n).map(|_| ((lcg(&mut s) % span) as i64, (lcg(&mut s) % dom) as i64, (lcg(&mut s) % 1000) as i64 - 500)).collect()
}

fn pair_layout(slots: bool) -> RecordLayout {
    if slots { RecordLayout::new_nn(&[KeyKind::I32, KeyKind::I32], &[SlotKind::Count, SlotKind::SumI64]) } else { RecordLayout::new_nn(&[KeyKind::I32, KeyKind::I32], &[]) }
}

fn desc(slots: bool) -> OwnerDesc {
    let pair = pair_layout(slots);
    OwnerDesc::distinct(pair, OwnerDesc::accumulator_of(&pair, 1), 1, 2_000, ORDER_NONE, 0, [(REP_NONE, 0); MAX_REPS])
}

/// A group: the key, its distinct pairs, then its riding slots.
type Group = (i64, i64, Vec<i64>);

fn oracle(rows: &[(i64, i64, i64)], slots: bool) -> Vec<Group> {
    let mut seen = BTreeSet::new();
    let mut m: BTreeMap<i64, (i64, i64, i64)> = BTreeMap::new();
    for &(k, x, v) in rows {
        let g = m.entry(k).or_default();
        if seen.insert((k, x)) {
            g.0 += 1;
        }
        g.1 += 1;
        g.2 += v;
    }
    m.into_iter().map(|(k, (p, c, s))| (k, p, if slots { vec![c, s] } else { vec![] })).collect()
}

/// The workers' phase-1 T1 partials: every row's pair record appended to its route's partition chain.
fn partials(rows: &[(i64, i64, i64)], pair: &RecordLayout, slots: bool) -> Vec<Vec<u64>> {
    let mut regions: Vec<Vec<u64>> = (0..WORKERS).map(|_| vec![0u64; WORKER_BYTES / 8]).collect();
    let ts: Vec<*mut T1> = regions.iter_mut().map(|r| unsafe { T1::at(r.as_mut_ptr() as *mut u8, pair, PBITS, 8192, WORKER_BYTES as u32) as *mut T1 }).collect();
    let mut buf = [0u64; 32];
    for (i, &(k, x, v)) in rows.iter().enumerate() {
        buf.fill(0);
        let r = unsafe { Rec::at(buf.as_mut_ptr() as *mut u8, pair) };
        r.open(&[k as u64, x as u64]);
        if slots {
            r.add_count(0, 1);
            r.add_i64(1, v);
        }
        let part = r.route().part(PBITS as u32);
        assert!(unsafe { T1::append_bytes(ts[i % WORKERS], part, buf.as_ptr() as *const u8) }, "the worker region holds its rows");
    }
    regions
}

/// The one owner as the driver runs it (section.rs `own_all`): a region opened, round 1 over every partition, a scratch
/// refusal opening a fresh region and folding round 1 again from the same partials, the round closed, then round 2 over
/// the round-2 buffers; the owner's groups and the round-1 reruns.
fn own(merge: &HashAggPartial, parts: &[Vec<u64>], acc: &RecordLayout) -> Result<(Vec<Group>, usize), Error> {
    let partials: Vec<Partial<'_>> = parts.iter().enumerate().map(|(o, r)| Partial { ordinal: o as u16, bytes: unsafe { core::slice::from_raw_parts(r.as_ptr() as *const u8, WORKER_BYTES) }, rows_processed: 0 }).collect();
    let mut reruns = 0;
    let mut region = vec![0u64; REGION_BYTES / 8];
    let owner = 'round1: loop {
        region.iter_mut().for_each(|w| *w = 0);
        let owner = Region { ptr: region.as_mut_ptr() as *mut u8, len: REGION_BYTES };
        merge.open_owner(owner, 0)?;
        for part in 0..1u32 << PBITS {
            match merge.own_partition(owner, part, PBITS, &partials, &[], core::ptr::null_mut(), core::ptr::null(), 1, &mut || Ok(())) {
                Err(Error::Unsupported(UNSUPPORTED_SCRATCH)) if reruns == 0 => {
                    reruns += 1;
                    continue 'round1;
                }
                r => r?,
            }
        }
        merge.end_round(owner, &mut || Ok(()))?;
        break owner;
    };
    let r2 = merge.round2_partial(owner).expect("a two-round owner's round-2 buffers");
    let pbits2 = merge.round2_pbits(owner);
    let p2 = [Partial { ordinal: 0, bytes: unsafe { core::slice::from_raw_parts(r2.ptr, r2.len) }, rows_processed: 0 }];
    for q in 0..1u32 << pbits2 {
        merge.own_partition(owner, q, pbits2, &p2, &[], core::ptr::null_mut(), core::ptr::null(), 2, &mut || Ok(()))?;
    }
    let base = owner.ptr as *const u8;
    let h = unsafe { OwnerTop::view(base) };
    let dir = unsafe { base.add(OWNERTOP_HDR) as *const u32 };
    let mut groups = Vec::new();
    for run in 0..h.worst as usize {
        let (off, n) = unsafe { (*dir.add(2 * run) as usize, *dir.add(2 * run + 1) as usize) };
        for i in 0..n {
            let r = unsafe { Rec::at(base.add(off + i * acc.size() as usize) as *mut u8, acc) };
            let slots = (ACC_SLOT_BASE as usize..acc.nslots()).map(|j| if acc.slot_kind(j) == SlotKind::Count { r.count(j) } else { r.i64(j) }).collect();
            groups.push((r.key(0) as i64, r.count(ACC_PAIRS as usize), slots));
        }
    }
    assert_eq!(groups.len(), h.len as usize, "every run read");
    groups.sort();
    Ok((groups, reruns))
}

fn bound(d: &OwnerDesc, dense: Option<(i64, i64)>) -> OwnerFns {
    owner_fns_dense(d, 3, dense)
}

/// The domain misses the upper half of the keys: round 1 runs again through the hash reduce and returns every group as the
/// plain path and the rows do, with and without riding slots (the fold table's identity fold and the dense walk).
#[test]
fn a_key_outside_a_stale_domain_reruns_round_one_through_the_hash_reduce() {
    for slots in [false, true] {
        let data = rows(30_000, 100, 60, 0xd597 ^ slots as u64);
        let d = desc(slots);
        let parts = partials(&data, &d.layout, slots);
        let expect = oracle(&data, slots);
        let (plain, reruns) = own(&HashAggPartial::new(0, Some(bound(&d, None))), &parts, &d.acc).expect("the plain path");
        assert_eq!(reruns, 0, "slots {slots}: the plain path runs once");
        assert_eq!(plain, expect, "slots {slots}: the plain path is the rows'");
        // the domain holds: the dense form runs once, the same groups
        let merge = HashAggPartial::with_plain(0, Some(bound(&d, Some((0, 99)))), Some(bound(&d, None)));
        let (held, reruns) = own(&merge, &parts, &d.acc).expect("a domain that holds");
        assert_eq!((held, reruns), (expect.clone(), 0), "slots {slots}: a domain that holds folds once");
        // a stale domain: keys 50..99 outside it
        let merge = HashAggPartial::with_plain(0, Some(bound(&d, Some((0, 49)))), Some(bound(&d, None)));
        let (got, reruns) = own(&merge, &parts, &d.acc).expect("a stale domain returns the value, not an error");
        assert_eq!(reruns, 1, "slots {slots}: round 1 again, once");
        assert_eq!(got, expect, "slots {slots}: the hash reduce's groups");
        // the miss is sticky: the next open is the plain form's (no second rerun)
        let (again, reruns) = own(&merge, &parts, &d.acc).expect("the merge after its miss");
        assert_eq!((again, reruns), (expect.clone(), 0), "slots {slots}: the plain form from the miss on");
    }
}

/// Without a plain form (a merge bound before D597's `with_plain`), the miss is an error by name, never a count short.
#[test]
fn a_miss_without_a_plain_form_is_an_error_by_name() {
    let data = rows(5_000, 100, 60, 7);
    let d = desc(false);
    let parts = partials(&data, &d.layout, false);
    match own(&HashAggPartial::new(0, Some(bound(&d, Some((0, 49))))), &parts, &d.acc) {
        Err(Error::Host(m)) => assert!(m.contains("outside the owner's dense domain"), "{m}"),
        r => panic!("a miss without a plain form: {:?}", r.map(|(g, n)| (g.len(), n))),
    }
}

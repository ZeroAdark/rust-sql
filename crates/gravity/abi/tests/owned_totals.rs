//! `Owned::totals`: the plain distinct form's observation (exec statement.rs Observe) sums the owners' `total` (their
//! deduped pairs) beside `records`, where `counts` sums `groups`; a region that is not a finished `Owned` one has neither.

use gravity_abi::breaker::{Owned, OwnerTop, GROUPS_UNCOUNTED, OWNED_HDR, OWNERTOP_HDR, TABLE_OWNED};

fn region(tops: &[(u64, u64, u64)]) -> Vec<u64> {
    let owner_bytes = (OWNERTOP_HDR + 63) & !63;
    let mut buf = vec![0u64; (OWNED_HDR + tops.len() * owner_bytes) / 8];
    let p = buf.as_mut_ptr() as *mut u8;
    unsafe {
        (*(p as *mut Owned)).nowners = tops.len() as u32;
        (*(p as *mut Owned)).owner_bytes = owner_bytes as u32;
        (*(p as *mut Owned)).kind = TABLE_OWNED;
        for (i, &(records, total, groups)) in tops.iter().enumerate() {
            let t = p.add(OWNED_HDR + i * owner_bytes) as *mut OwnerTop;
            (*t).records = records;
            (*t).total = total;
            (*t).groups = groups;
        }
    }
    buf
}

fn bytes(buf: &[u64]) -> &[u8] {
    unsafe { core::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) }
}

#[test]
fn the_plain_distinct_totals_sum_the_owners_deduped_pairs_where_counts_sums_their_groups() {
    let r = region(&[(40_000, 39_999, 0), (25_000, 25_000, 0), (0, 0, 0)]);
    assert_eq!(Owned::totals(bytes(&r)), Some((65_000, Some(64_999))));
    assert_eq!(Owned::counts(bytes(&r)), Some((65_000, Some(0))));
}

#[test]
fn an_uncounted_group_leaves_the_totals_and_a_region_that_is_not_owned_has_none() {
    let r = region(&[(10, 7, GROUPS_UNCOUNTED), (5, 5, 3)]);
    assert_eq!(Owned::totals(bytes(&r)), Some((15, Some(12))));
    assert_eq!(Owned::counts(bytes(&r)), Some((15, None)));
    let mut other = region(&[(10, 7, 1)]);
    unsafe { (*(other.as_mut_ptr() as *mut Owned)).kind = TABLE_OWNED + 1 };
    assert_eq!(Owned::totals(bytes(&other)), None);
    assert_eq!(Owned::totals(&bytes(&r)[..OWNED_HDR - 8]), None);
}

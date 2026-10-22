//! The part cache's census and its statement account with zone memos: a part's decoded zone records
//! (`OpenPart::zone_records`) are charged to the part cache when the part keeps them, and the part's drop debits
//! exactly what was charged. Its own test binary: the census (`pgrc_read::partcache::bytes`) and the account hook are
//! process-wide, and this is the binary's only test, so every count they show is this test's.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{build_fixture, int8_fixture, open_part_bytes, BuiltFixture};
use pgrc_read::{partcache, OpenPart};
use pgrc_write::writer::PartCutPolicy;

static CHARGED: AtomicU64 = AtomicU64::new(0);
static RELEASED: AtomicU64 = AtomicU64::new(0);
static OVER_RELEASED: AtomicBool = AtomicBool::new(false);

/// The statement account (`partcache::set_account_hook`): what the part cache charged and released.
fn account(n: u64, charged: bool, _rec: &mut partcache::Charged) {
    if charged {
        CHARGED.fetch_add(n, Ordering::SeqCst);
    } else if RELEASED.fetch_add(n, Ordering::SeqCst) + n > CHARGED.load(Ordering::SeqCst) {
        OVER_RELEASED.store(true, Ordering::SeqCst);
    }
}

/// The account's net charge (negative: it released more than it was charged).
fn net() -> i128 {
    i128::from(CHARGED.load(Ordering::SeqCst)) - i128::from(RELEASED.load(Ordering::SeqCst))
}

fn open_parts(bf: &BuiltFixture) -> Vec<Arc<OpenPart>> {
    bf.manifest
        .parts
        .iter()
        .enumerate()
        .map(|(i, rec)| {
            let name = part_file_name(rec.part_no);
            let bytes = bf.files.get(&name).expect("part file present");
            open_part_bytes(bytes, 500 + i as u64).expect("open part")
        })
        .collect()
}

/// Every part's zone memo is charged to the part cache and the statement account as the part keeps it, and the
/// parts' drops bring the census back to 0 and the account back to its net: no debit of bytes never charged, so
/// neither counter underflows (the census's saturating debit never clamps, the account never over-releases).
#[test]
fn zone_memo_bytes_are_charged_to_the_part_cache_and_debited_with_the_part() {
    partcache::set_account_hook(account);
    let (c0, n0, z0) = (partcache::bytes(), net(), pgrc_read::mem::zone_bytes());
    assert_eq!(c0, 0, "a fresh process: nothing in the part cache yet");
    let fx = int8_fixture("int8_zonecensus", 7105, 40_000, vec![], PartCutPolicy::default(), |i| Some((i * 3) as i64));
    let bf = build_fixture(fx);
    let attno = bf.fx.schema[0].attno;
    let parts = open_parts(&bf);
    let (c1, n1) = (partcache::bytes(), net());
    let mut kept = 0;
    for part in &parts {
        assert!(pgrc_scan::meta::zone_records(part, attno).expect("read").is_some(), "a stats section");
        kept += part.zone_resident();
    }
    assert!(kept > 0, "the parts keep zone memos");
    assert_eq!(partcache::bytes() - c1, kept, "the zone memos are charged to the part cache");
    assert_eq!(net() - n1, i128::from(kept), "and to the statement account");
    assert_eq!(pgrc_read::mem::zone_bytes() - z0, kept, "and counted sticky in the residency plane");
    drop(parts);
    drop(bf);
    assert_eq!(partcache::bytes(), c0, "the parts' drops debit exactly what was charged: the part cache is back to 0");
    assert_eq!(net(), n0, "the account releases exactly what it was charged");
    assert!(!OVER_RELEASED.load(Ordering::SeqCst), "the account never released more than it was charged");
    assert_eq!(pgrc_read::mem::zone_bytes(), z0, "the zone memos went with their parts");
}

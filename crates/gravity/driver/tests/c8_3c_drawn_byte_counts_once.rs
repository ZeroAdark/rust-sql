//! C8 move 3c (a): a statement's draw inside its grant counts once on its memory ledger account. The region's pages go on
//! the doors (its kind lane); the same bytes leave the account's unbacked room (`pool_reserved_bytes`, the grant pool's
//! promise not yet backed), so `pool_reserved_bytes` + the drawn bytes is the claim and no more. The claim itself
//! (`memledger::pool_reserved`, what the decisions on the pool's promise read) is unchanged. Born red on trunk 9cc5454758c:
//! the account read the whole reservation beside the region's pages, the claim plus the draw.

use std::sync::Mutex;
use std::time::Duration;

use gravity_driver::{try_region_in, Grant, POOL, REGIONS};

static LOCK: Mutex<()> = Mutex::new(());
const MIB: u64 = 1 << 20;
const M: i64 = MIB as i64;

fn armed() -> std::sync::MutexGuard<'static, ()> {
    let g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    POOL.gate(0);
    REGIONS.trim();
    POOL.arm(256 * MIB);
    g
}

fn done() {
    REGIONS.trim();
    POOL.arm(0);
}

#[test]
fn a_draw_inside_the_grant_counts_once_on_the_statements_account() {
    let _g = armed();
    memledger::bind_backend(11, 3611);
    let acct = memledger::current();
    let unbacked = || memledger::account_row(acct).pool_reserved_bytes;
    let drawn = || memledger::kind_bytes(acct, memledger::Kind::Region);
    let a = POOL.admit(Grant::of(64 * MIB), 1, 1, Duration::from_millis(100)).expect("admitted");
    let claim = a.reserved as i64;
    assert!(claim >= 32 * M, "a reservation with room for the draws below ({claim} B)");
    assert_eq!((unbacked(), memledger::pool_reserved(acct)), (claim, claim), "admission: the whole claim is promise");
    let base = drawn();
    let r = try_region_in(&a, 16 * MIB as usize).expect("a region inside G");
    assert_eq!(drawn() - base, 16 * M, "the region's pages are on the account");
    assert_eq!(unbacked() + (drawn() - base), claim, "the room it drew counts once: promise + pages = the claim, no more");
    assert_eq!(memledger::pool_reserved(acct), claim, "the claim is unchanged");
    let s = try_region_in(&a, 8 * MIB as usize).expect("a second region inside G");
    assert_eq!(unbacked() + (drawn() - base), claim, "two draws: still the claim");
    drop(r);
    assert_eq!((drawn() - base, unbacked()), (8 * M, claim - 8 * M), "a region given back: its room is promise again");
    drop(s);
    assert_eq!(unbacked(), claim);
    drop(a);
    assert_eq!((unbacked(), memledger::pool_reserved(acct)), (0, 0), "the reservation's drop takes the whole claim off");
    memledger::unbind_backend();
    done();
}

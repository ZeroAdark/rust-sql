use std::collections::BTreeMap;
use pgsync::RwLock;

use ::types_core::primitive::{BlockNumber, ForkNumber};
use ::types_core::Oid;
use ::types_storage::RelFileLocator;

// Sentinel: file is mutated outside md; never cache, always walk.
const POISON: u64 = u64::MAX;

struct Inner {
    // Bumped by every mutation that leaves (or could leave) a key absent.
    epoch: u64,
    map: BTreeMap<u128, u64>,
}

// Deliberately not Copy/Clone: one walk, one publish attempt.
pub struct WalkToken {
    epoch: u64,
}

// The map's nodes are this holder's, made and freed in its home whichever backend changes the map: an insert by one
// session is often removed by another's (a DROP DATABASE's purge frees every session's), and door 1 takes a free off the
// freeing thread's account. Entered only where a node can be made or freed, never on a lookup or an in-place update.
#[inline]
fn home() -> Option<memledger::Entered> {
    memledger::Home::of(memledger::Acct::of(memledger::Consumer::NblocksCache)).enter()
}

fn cache() -> &'static RwLock<Inner> {
    static CACHE: pgsync::OnceLock<RwLock<Inner>> = pgsync::OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(Inner { epoch: 0, map: BTreeMap::new() }))
}

// dbOid in the top bits so a whole database is one contiguous key range.
#[inline]
fn key(locator: RelFileLocator, forknum: ForkNumber) -> u128 {
    ((locator.dbOid as u128) << 96)
        | ((locator.spcOid as u128) << 64)
        | ((locator.relNumber as u128) << 32)
        | forknum as u128
}

#[inline]
fn db_range(db: Oid) -> (u128, u128) {
    let lo = (db as u128) << 96;
    let hi = lo | ((1u128 << 96) - 1);
    (lo, hi)
}

pub fn lookup(locator: RelFileLocator, forknum: ForkNumber) -> Option<BlockNumber> {
    let g = cache().read().unwrap();
    match g.map.get(&key(locator, forknum)) {
        Some(&v) if v != POISON => Some(v as BlockNumber),
        _ => None,
    }
}

// Taking the token in the same critical section as miss detection ensures the walk begins no earlier than the snapshot.
pub fn lookup_or_begin_walk(
    locator: RelFileLocator,
    forknum: ForkNumber,
) -> Result<BlockNumber, WalkToken> {
    let g = cache().read().unwrap();
    match g.map.get(&key(locator, forknum)) {
        Some(&v) if v != POISON => Ok(v as BlockNumber),
        _ => Err(WalkToken { epoch: g.epoch }),
    }
}

pub fn note_walked(
    locator: RelFileLocator,
    forknum: ForkNumber,
    nblocks: BlockNumber,
    token: WalkToken,
) {
    let mut g = cache().write().unwrap();
    if g.epoch == token.epoch {
        let k = key(locator, forknum);
        if let std::collections::btree_map::Entry::Vacant(e) = g.map.entry(k) {
            let _home = home();
            e.insert(nblocks as u64);
        }
    }
}

// On an absent entry, bumps epoch instead of inserting: recovery calls this with a lower bound that may undershoot real size.
pub fn extend_to(locator: RelFileLocator, forknum: ForkNumber, new_min: BlockNumber) {
    let mut g = cache().write().unwrap();
    match g.map.get_mut(&key(locator, forknum)) {
        Some(e) => {
            if *e != POISON && *e < new_min as u64 {
                *e = new_min as u64;
            }
        }
        None => g.epoch += 1,
    }
}

pub fn set_exact(locator: RelFileLocator, forknum: ForkNumber, nblocks: BlockNumber) {
    let mut g = cache().write().unwrap();
    let k = key(locator, forknum);
    if let Some(e) = g.map.get_mut(&k) {
        if *e != POISON {
            *e = nblocks as u64;
        }
        return;
    }
    let _home = home();
    g.map.insert(k, nblocks as u64);
}

// Always bumps the epoch: the file may have changed size even when no entry existed to remove.
pub fn invalidate(locator: RelFileLocator, forknum: ForkNumber) {
    let mut g = cache().write().unwrap();
    g.epoch += 1;
    if let Some(&v) = g.map.get(&key(locator, forknum)) {
        if v != POISON {
            let _home = home();
            g.map.remove(&key(locator, forknum));
        }
    }
}

pub fn remove(locator: RelFileLocator, forknum: ForkNumber) {
    let mut g = cache().write().unwrap();
    g.epoch += 1;
    let _home = home();
    g.map.remove(&key(locator, forknum));
}

pub fn poison(locator: RelFileLocator, forknum: ForkNumber) {
    let mut g = cache().write().unwrap();
    let _home = home();
    g.map.insert(key(locator, forknum), POISON);
}

pub fn purge_db(db: Oid) {
    let (lo, hi) = db_range(db);
    let mut g = cache().write().unwrap();
    g.epoch += 1;
    let _home = home();
    let doomed: Vec<u128> = g.map.range(lo..=hi).map(|(&k, _)| k).collect();
    for k in doomed {
        g.map.remove(&k);
    }
}

pub fn clear() {
    let mut g = cache().write().unwrap();
    g.epoch += 1;
    let _home = home();
    g.map.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rl(rel: u32) -> RelFileLocator {
        RelFileLocator::new(1663, 5, rel)
    }
    const MAIN: ForkNumber = ForkNumber::MAIN_FORKNUM;

    // The map is process-global; serialize tests so concurrent epoch bumps do not void WalkTokens.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn walk_token(l: RelFileLocator) -> WalkToken {
        lookup_or_begin_walk(l, MAIN).expect_err("expected a cache miss")
    }

    #[test]
    fn walk_then_lookup_roundtrip() {
        let _g = serial();
        let l = rl(9_000_001);
        let t = walk_token(l);
        assert_eq!(lookup(l, MAIN), None);
        note_walked(l, MAIN, 42, t);
        assert_eq!(lookup(l, MAIN), Some(42));
        assert_eq!(lookup_or_begin_walk(l, MAIN).ok(), Some(42));
    }

    #[test]
    fn extend_is_monotonic_and_needs_an_entry() {
        let _g = serial();
        let l = rl(9_000_002);
        extend_to(l, MAIN, 7);
        assert_eq!(lookup(l, MAIN), None, "absent stays absent");
        let t = walk_token(l);
        note_walked(l, MAIN, 10, t);
        extend_to(l, MAIN, 12);
        assert_eq!(lookup(l, MAIN), Some(12));
        extend_to(l, MAIN, 11);
        assert_eq!(lookup(l, MAIN), Some(12), "max() semantics");
    }

    #[test]
    fn truncate_sets_exact_and_invalidate_forgets() {
        let _g = serial();
        let l = rl(9_000_003);
        let t = walk_token(l);
        note_walked(l, MAIN, 100, t);
        set_exact(l, MAIN, 3);
        assert_eq!(lookup(l, MAIN), Some(3));
        invalidate(l, MAIN);
        assert_eq!(lookup(l, MAIN), None);
    }

    #[test]
    fn poison_is_sticky_until_remove() {
        let _g = serial();
        let l = rl(9_000_004);
        let t = walk_token(l);
        poison(l, MAIN);
        assert_eq!(lookup(l, MAIN), None);
        note_walked(l, MAIN, 5, t);
        set_exact(l, MAIN, 6);
        extend_to(l, MAIN, 7);
        invalidate(l, MAIN);
        assert_eq!(lookup(l, MAIN), None, "poison survives all");
        remove(l, MAIN);
        let t = walk_token(l);
        note_walked(l, MAIN, 5, t);
        assert_eq!(lookup(l, MAIN), Some(5), "remove clears poison");
    }

    #[test]
    fn purge_db_takes_only_that_database() {
        let _g = serial();
        let mine = RelFileLocator::new(1663, 77001, 9_000_005);
        let other = RelFileLocator::new(1663, 77002, 9_000_006);
        let tm = walk_token(mine);
        let to = walk_token(other);
        note_walked(mine, MAIN, 1, tm);
        note_walked(other, MAIN, 2, to);
        purge_db(77001);
        assert_eq!(lookup(mine, MAIN), None);
        assert_eq!(lookup(other, MAIN), Some(2));
    }

    #[test]
    fn forks_are_independent_keys() {
        let _g = serial();
        let l = rl(9_000_007);
        let tm = lookup_or_begin_walk(l, ForkNumber::MAIN_FORKNUM).unwrap_err();
        let tf = lookup_or_begin_walk(l, ForkNumber::FSM_FORKNUM).unwrap_err();
        note_walked(l, ForkNumber::MAIN_FORKNUM, 8, tm);
        note_walked(l, ForkNumber::FSM_FORKNUM, 3, tf);
        assert_eq!(lookup(l, ForkNumber::MAIN_FORKNUM), Some(8));
        assert_eq!(lookup(l, ForkNumber::FSM_FORKNUM), Some(3));
        remove(l, ForkNumber::FSM_FORKNUM);
        assert_eq!(lookup(l, ForkNumber::MAIN_FORKNUM), Some(8));
        assert_eq!(lookup(l, ForkNumber::FSM_FORKNUM), None);
    }

    #[test]
    fn stale_walk_cannot_overwrite_a_concurrent_extension() {
        let _g = serial();
        let l = rl(9_000_010);
        let reader = walk_token(l);
        let extender = walk_token(l);
        note_walked(l, MAIN, 0, extender);
        extend_to(l, MAIN, 1);
        note_walked(l, MAIN, 0, reader);
        assert_eq!(lookup(l, MAIN), Some(1), "stale walk must be discarded");
    }

    #[test]
    fn extend_on_absent_entry_defeats_a_racing_walker() {
        let _g = serial();
        let l = rl(9_000_011);
        let reader = walk_token(l);
        extend_to(l, MAIN, 1);
        note_walked(l, MAIN, 0, reader);
        assert_eq!(lookup(l, MAIN), None, "stale walk must be discarded");
    }

    #[test]
    fn remove_defeats_a_racing_walker() {
        let _g = serial();
        let l = rl(9_000_012);
        let t0 = walk_token(l);
        note_walked(l, MAIN, 100, t0);
        let reader = lookup_or_begin_walk(l, MAIN);
        assert_eq!(reader.ok(), Some(100));
        remove(l, MAIN);
        let fresh = walk_token(l);
        note_walked(l, MAIN, 100, fresh);
        assert_eq!(lookup(l, MAIN), Some(100));
        remove(l, MAIN);
        let l2 = rl(9_000_013);
        let pre = walk_token(l2);
        remove(l2, MAIN);
        note_walked(l2, MAIN, 55, pre);
        assert_eq!(lookup(l2, MAIN), None, "pre-remove token must be dead");
    }

    #[test]
    fn stale_walk_cannot_overwrite_a_truncation() {
        let _g = serial();
        let l = rl(9_000_014);
        let t0 = walk_token(l);
        note_walked(l, MAIN, 100, t0);
        invalidate(l, MAIN);
        let reader = walk_token(l);
        set_exact(l, MAIN, 3);
        note_walked(l, MAIN, 100, reader);
        assert_eq!(lookup(l, MAIN), Some(3), "post-truncate size must win");
    }
}

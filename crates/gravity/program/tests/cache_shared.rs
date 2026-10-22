//! The process-wide program tier (PROGCACHE, V4R-35): `gravity_program::cache::SharedCache` under concurrent insert and
//! lookup, its entry and byte bounds (LRU), first-writer-wins, and D303's identity (interpreted programs of one entry
//! stay distinct under distinct graph keys).

use std::sync::Arc;
use std::thread;

use gravity_abi::Ctx;
use gravity_program::cache::{SharedCache, SHARDS};
use gravity_program::{Arena, Code, Layout, OutCol, Program, SiteDesc, WritePlan};

unsafe extern "C-unwind" fn fake_entry(_ctx: *mut Ctx) -> i64 {
    gravity_abi::status::DONE
}

unsafe extern "C-unwind" fn interp_run(_state: *const (), _ctx: *mut Ctx) -> i64 {
    gravity_abi::status::DONE
}

fn program_with(rel: u32, code: Code) -> Arc<Program> {
    let st = Arena::new(1 << 12);
    let sites = [SiteDesc::index_source(st.handle(), rel, rel + 1, &[1, 2], &[0, 1], &[3, 4])];
    let out = [OutCol::new("a", 23, -1, 4, true)];
    Arc::new(Program::new(Layout { nsites: 1, out_natts: 1, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, code))
}

fn program(rel: u32) -> Arc<Program> {
    program_with(rel, Code::hand(fake_entry))
}

/// Keys that land in one shard (the shard is the top 4 bits of the folded key times the golden ratio).
fn same_shard_keys(n: usize) -> Vec<u128> {
    let shard = |k: u128| (((k as u64) ^ ((k >> 64) as u64)).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 60) as usize % SHARDS;
    (1u128..).filter(|&k| shard(k) == shard(1)).take(n).collect()
}

#[test]
fn hit_miss_and_first_insert_wins() {
    let c: SharedCache<Arc<Program>> = SharedCache::new(64, 1 << 20);
    assert!(c.get(7).is_none());
    let a = program(1);
    let filed = c.insert(7, a.clone(), 100);
    assert!(Arc::ptr_eq(&filed, &a));
    // a second backend that lowered the same shape at once gets the first program back
    let b = program(1);
    let got = c.insert(7, b.clone(), 100);
    assert!(Arc::ptr_eq(&got, &a) && !Arc::ptr_eq(&got, &b));
    assert!(Arc::ptr_eq(&c.get(7).expect("filed"), &a));
    let st = c.stats();
    assert_eq!((st.hits, st.misses, st.inserts, st.entries, st.bytes), (1, 1, 1, 1, 100));
}

#[test]
fn eviction_holds_the_entry_bound_lru() {
    // 16 entries over 16 shards = 1 per shard: a second key of a shard evicts the first
    let c: SharedCache<Arc<Program>> = SharedCache::new(SHARDS * 2, 1 << 30);
    let ks = same_shard_keys(3);
    c.insert(ks[0], program(1), 1);
    c.insert(ks[1], program(2), 1);
    assert!(c.get(ks[0]).is_some(), "a hit refreshes ks[0]");
    c.insert(ks[2], program(3), 1);
    assert!(c.contains(ks[0]) && !c.contains(ks[1]) && c.contains(ks[2]), "the least recently used goes");
    assert_eq!(c.stats().evictions, 1);
}

#[test]
fn eviction_holds_the_byte_bound() {
    let c: SharedCache<Arc<Program>> = SharedCache::new(1 << 20, SHARDS * 1000);
    let ks = same_shard_keys(4);
    for (i, k) in ks.iter().enumerate() {
        c.insert(*k, program(i as u32 + 1), 400);
    }
    let st = c.stats();
    assert!(st.bytes <= 1000, "one shard's budget: {} bytes", st.bytes);
    assert_eq!(st.entries, 2);
    assert!(c.contains(ks[2]) && c.contains(ks[3]));
    // over the shard's whole budget: returned, never filed
    let big = program(9);
    let k = same_shard_keys(5)[4];
    assert!(Arc::ptr_eq(&c.insert(k, big.clone(), 5000), &big));
    assert!(!c.contains(k));
}

#[test]
fn evicted_values_stay_alive_with_their_holders() {
    let c: SharedCache<Arc<Program>> = SharedCache::new(SHARDS, 1 << 30);
    let ks = same_shard_keys(2);
    let held = c.insert(ks[0], program(1), 1);
    c.insert(ks[1], program(2), 1);
    assert!(!c.contains(ks[0]));
    assert_eq!(held.sites.len(), 1, "a backend's Arc outlives the tier's");
}

#[test]
fn concurrent_insert_and_lookup() {
    const THREADS: usize = 16;
    const KEYS: u128 = 512;
    let c: Arc<SharedCache<Arc<Program>>> = Arc::new(SharedCache::new(256, 1 << 30));
    let progs: Arc<Vec<Arc<Program>>> = Arc::new((0..KEYS as u32).map(|i| program(i + 1)).collect());
    let hs: Vec<_> = (0..THREADS)
        .map(|t| {
            let (c, progs) = (c.clone(), progs.clone());
            thread::spawn(move || {
                let mut seen = 0u64;
                for round in 0..20_000u128 {
                    let k = (round * 7 + t as u128 * 13) % KEYS;
                    match c.get(k) {
                        // a key only ever holds its own program: no torn or crossed entry
                        Some(p) => {
                            assert_eq!(p.key, progs[k as usize].key);
                            seen += 1;
                        }
                        None => {
                            let got = c.insert(k, progs[k as usize].clone(), 64);
                            assert_eq!(got.key, progs[k as usize].key);
                        }
                    }
                }
                seen
            })
        })
        .collect();
    let hits: u64 = hs.into_iter().map(|h| h.join().expect("no panic")).sum();
    let st = c.stats();
    assert!(st.entries <= 256, "the entry bound under contention: {}", st.entries);
    assert!(st.bytes <= 256 * 64);
    assert!(hits > 0 && st.hits == hits);
    assert!(st.evictions > 0);
}

#[test]
fn d303_interpreted_programs_stay_distinct() {
    // two interpreted programs run through one entry (`interp_identity`); D303: their content keys carry the IR's key,
    // so two graph keys keep two programs and neither answers for the other
    let a = program_with(1, Code::interp(interp_run, Box::new(1u8)).with_identity(0xaaaa));
    let b = program_with(1, Code::interp(interp_run, Box::new(2u8)).with_identity(0xbbbb));
    assert_ne!(a.key, b.key, "D303: the identity is in the key");
    let c: SharedCache<Arc<Program>> = SharedCache::new(64, 1 << 20);
    c.insert(1, a.clone(), 1);
    c.insert(2, b.clone(), 1);
    assert_eq!(c.get(1).expect("a").key, a.key);
    assert_eq!(c.get(2).expect("b").key, b.key);
    assert_eq!(c.stats().entries, 2);
}

/// D4579: `remove_where` takes out exactly the entries the predicate names, over every shard, hands each value to `gone`
/// (the tier's `Arc`: a backend that holds the program keeps it) and leaves the rest served.
#[test]
fn remove_where_takes_the_doomed_entries_only() {
    let c: SharedCache<(u32, Arc<Program>)> = SharedCache::new(256, 1 << 30);
    for k in 1u128..=40 {
        c.insert(k, ((k % 4) as u32, program(k as u32)), 1);
    }
    let held = c.get(8).expect("filed").1;
    let mut gone = Vec::new();
    let n = c.remove_where::<256>(|v| v.0 == 0, |v| gone.push(v.1.sites.len()));
    assert_eq!((n, gone.len()), (10, 10), "the ten keys whose tag is 0");
    for k in 1u128..=40 {
        assert_eq!(c.contains(k), k % 4 != 0, "key {k}");
    }
    assert_eq!(held.sites.len(), 1, "a holder's Arc outlives the removal");
    assert_eq!(c.stats().entries, 30);
    assert_eq!(c.remove_where::<256>(|v| v.0 == 0, |_| panic!("nothing doomed is left")), 0);
    // a pass of one key: a shard with more doomed entries than a pass takes them over several passes, all of them
    let n = c.remove_where::<1>(|v| v.0 == 1, |_| ());
    assert_eq!(n, 10, "the ten keys whose tag is 1, one per pass");
    assert_eq!(c.stats().entries, 20);
}

#[test]
fn reset_empties() {
    let c: SharedCache<Arc<Program>> = SharedCache::new(64, 1 << 20);
    c.insert(1, program(1), 1);
    c.reset();
    assert!(c.get(1).is_none());
    assert_eq!(c.stats().entries, 0);
}

/// C2: a program the tier files moves its arena's bytes onto the program caches' memory account once (a second filing
/// moves nothing), and they leave it only at the program's last drop: evicted from the tier while a backend still holds
/// it, it stays counted. Nothing else in this binary moves bytes on that account.
#[test]
fn a_filed_program_is_on_the_program_caches_account_until_its_last_drop() {
    let caches = memledger::Acct::of(memledger::Consumer::ProgramCaches);
    let on = || memledger::account_row(caches).account_bytes;
    let _in = memledger::charge::enter_as(memledger::Owner::Session(memledger::Acct::backend(4086)));
    let p0 = on();
    let c: SharedCache<Arc<Program>> = SharedCache::new(SHARDS, 1 << 20);
    let a = program(9);
    let (got, filed) = c.insert_filed(11, a.clone(), 100);
    assert!(filed && Arc::ptr_eq(&got, &a));
    assert_eq!((on(), a.ledgered_bytes()), (p0, 0), "filing in the cache alone moves nothing");
    a.file_on_ledger();
    let moved = a.ledgered_bytes();
    assert!(moved > 0, "the arena's bytes");
    assert_eq!(on() - p0, moved as i64);
    a.file_on_ledger();
    assert_eq!((on() - p0, a.ledgered_bytes()), (moved as i64, moved), "once");
    let (_, again) = c.insert_filed(11, program(9), 100);
    assert!(!again, "a key filed already keeps its first program");
    c.reset();
    drop(got);
    assert_eq!(on() - p0, moved as i64, "evicted, still held: still counted");
    drop(a);
    assert_eq!(on(), p0, "the last drop takes it back");
}

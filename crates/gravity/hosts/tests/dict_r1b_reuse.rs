//! R1b (main 09-25): `build_owned_bytes` builds into a buffer reused from the process pool (text_hosts `reuse`), its words
//! as the last table left them. A table built over a reused, dirtied buffer must read exactly as one built over a fresh
//! zeroed buffer: every builder writes or zeroes what it reads (`Region::alloc_zeroed` / `copy`).
use gravity_hosts::dict::text_hosts::{build_owned, reuse};

/// The pool is process-wide: the tests take turns.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Words(Vec<Vec<u8>>);

impl gravity_dict::Dict for Words {
    fn ncodes(&self) -> u32 {
        self.0.len() as u32
    }
    fn entries(&self, f: &mut dyn FnMut(u32, gravity_dict::View)) {
        for (c, w) in self.0.iter().enumerate() {
            f(c as u32, gravity_dict::View::of(w));
        }
    }
}

const N: u32 = 20_000;

fn dict() -> Words {
    Words((0..N).map(|i| format!("https://host-{}.example/{i}", i / 7).into_bytes()).collect()) // alloc: Cold
}

/// The Fp128 table's cells and the I64 table's values of `dict`, each built through `build_owned` (the pool's take).
fn tables(d: &Words) -> (Vec<[u64; 2]>, Vec<i64>) {
    let fp = build_owned(N as usize, 0, &mut |region| {
        // every fourth entry faults: its cell must read [0, 0] and its fault bit set, dirty buffer or not
        let mut i = 0u32;
        Ok(gravity_dict::Fp128::build(d, region, &mut |v| {
            i += 1;
            if i % 4 == 0 { Err(gravity_dict::Fault(0)) } else { Ok(v) }
        })?
        .header())
    })
    .unwrap();
    let t = unsafe { gravity_dict::Fp128::at(fp.header()) };
    assert!(t.has_faults());
    let cells = (0..N).map(|c| unsafe { t.get(c) }).collect(); // alloc: Cold
    let lens = build_owned(N as usize, 0, &mut |region| Ok(gravity_dict::I64::build(d, region, &mut |v| Ok(unsafe { v.bytes() }.len() as i64))?.header())).unwrap();
    let l = unsafe { gravity_dict::I64::at(lens.header()) };
    let vals = (0..N).map(|c| unsafe { l.get(c) }).collect(); // alloc: Cold
    (cells, vals)
}

#[test]
fn a_table_built_over_a_reused_dirty_buffer_reads_as_one_built_over_a_fresh_one() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    gravity_dict::fp::install(0x1234, 0x5678);
    let d = dict();
    let fresh = tables(&d);
    // the pool now holds the two tables' buffers (dropped above); dirty every kept word
    let (kept, _) = reuse::held();
    assert!(kept >= 2, "a dropped table's buffer is kept for the next build");
    let words = 64 + 48 * N as usize;
    let mut dirty = Vec::new(); // alloc: Cold
    for _ in 0..kept {
        let mut b = reuse::take(words / 8);
        b.fill(u64::MAX);
        dirty.push(b); // alloc: Cold
    }
    assert_eq!(reuse::held().0, 0, "every kept buffer taken");
    dirty.into_iter().for_each(reuse::give);
    assert!(reuse::held().0 >= 2);
    let (held_before, _) = reuse::held();
    let again = tables(&d);
    assert!(reuse::held().0 >= held_before, "the builds took kept buffers and gave them back");
    assert_eq!(again, fresh, "a dirty reused buffer builds the same tables");
}

/// H25: the cap is min(RAM / 8, the grant pool's capacity) (the pool's share of the one process-cache budget), whichever
/// is known, with no fixed floor: an unarmed grant pool leaves RAM / 8, a grant pool smaller than that caps the pool
/// (the old 768 MiB floor kept more than it), unknown RAM leaves the grant pool, and nothing is kept when neither is known.
#[test]
fn the_pool_cap_is_the_ram_and_grant_pool_fraction_with_no_floor() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!((reuse::cap_of(0, 0), reuse::cap_of(0, 5 << 20), reuse::cap_of(8 << 30, 0)), (0, 5 << 20, 1 << 30));
    assert_eq!((reuse::cap_of(8 << 30, 64 << 20), reuse::cap_of(8 << 30, 3 << 30)), (64 << 20, 1 << 30));
    let ram: u64 = std::fs::read_to_string("/proc/meminfo").ok().and_then(|t| t.lines().find_map(|l| l.strip_prefix("MemTotal:")).and_then(|r| r.split_whitespace().next()).and_then(|n| n.parse::<u64>().ok())).map_or(0, |kb| kb << 10);
    for pool in [0u64, 64 << 20, 3 << 30] {
        gravity_driver::POOL.arm(pool);
        assert_eq!(reuse::pool_bytes(), reuse::cap_of(ram, pool), "grant pool {pool}");
    }
    gravity_driver::POOL.arm(0);
}

#[test]
fn the_resident_tables_cap_is_an_eighth_of_the_machine_under_any_grant_pool() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    use gravity_hosts::dict::text_hosts::fp_cache_cap;
    use gravity_program::cost::resident_tables_cap;
    assert_eq!((resident_tables_cap(0, 0), resident_tables_cap(0, 5 << 20), resident_tables_cap(8 << 30, 0)), (0, 5 << 20, 1 << 30));
    assert_eq!((resident_tables_cap(8 << 30, 64 << 20), resident_tables_cap(8 << 30, 3 << 30)), (1 << 30, 1 << 30));
    // no pgrust.gravity_fp_cache_mb here (no GUC seam): the cap is the rule over this machine and the pool as armed
    let ram: u64 = std::fs::read_to_string("/proc/meminfo").ok().and_then(|t| t.lines().find_map(|l| l.strip_prefix("MemTotal:")).and_then(|r| r.split_whitespace().next()).and_then(|n| n.parse::<u64>().ok())).map_or(0, |kb| kb << 10);
    for pool in [0u64, 64 << 20, 3 << 30] {
        gravity_driver::POOL.arm(pool);
        assert_eq!(fp_cache_cap(), resident_tables_cap(ram, pool), "grant pool {pool}");
        if ram > 0 {
            assert_eq!(fp_cache_cap(), ram / 8, "grant pool {pool}: an eighth of the machine");
            let stores = reuse::store_bytes(gravity_program::cost::condition_cache_cap) + reuse::store_bytes(gravity_program::cost::zone_store_cap);
            assert!(fp_cache_cap() + (reuse::pool_bytes() + stores) as u64 <= ram / 64 * 18 + ram % 64 * 18 / 64, "grant pool {pool}: the four process caches within 9 / 32 of RAM");
        }
    }
    gravity_driver::POOL.arm(0);
}

/// One process-cache budget, in 64ths of RAM: the resident tables (8: RAM / 8), the build-buffer pool (8: min(RAM / 8,
/// pool)), pgrc's condition cache and zone store (1 each: min(RAM / 64, pool)) sum to at most 18 / 64 of RAM for any RAM
/// and grant pool, and to 18 / 64 where the pool does not bind; unknown RAM leaves the pool's bound alone.
#[test]
fn the_four_process_caches_stay_within_their_shares_of_ram_under_any_grant_pool() {
    use gravity_program::cost::{build_pool_cap, condition_cache_cap, process_caches_cap, resident_tables_cap, zone_store_cap};
    for ram in [0u64, 1 << 20, 3 << 30, 8 << 30, 16 << 30, 64 << 30, (256 << 30) + 12345] {
        for pool in [0u64, 1, 5 << 20, 64 << 20, 512 << 20, 3 << 30, 17 << 30, 1 << 40, u64::MAX / 8] {
            let sum = process_caches_cap(ram, pool);
            assert_eq!(sum, resident_tables_cap(ram, pool) + build_pool_cap(ram, pool) + condition_cache_cap(ram, pool) + zone_store_cap(ram, pool), "ram {ram} pool {pool}");
            if ram > 0 {
                assert!(sum as u128 <= ram as u128 * 18 / 64, "ram {ram} pool {pool}: {sum} past 18 / 64 of RAM");
                assert_eq!(resident_tables_cap(ram, pool), ram / 8, "ram {ram} pool {pool}: the tables not pool-bound");
                for (cap, n) in [(build_pool_cap(ram, pool), 8u128), (condition_cache_cap(ram, pool), 1), (zone_store_cap(ram, pool), 1)] {
                    let share = (ram as u128 * n / 64) as u64;
                    assert_eq!(cap, if pool == 0 { share } else { share.min(pool) }, "ram {ram} pool {pool} 64ths {n}");
                }
            } else {
                assert_eq!((build_pool_cap(0, pool), condition_cache_cap(0, pool), zone_store_cap(0, pool)), (pool, pool, pool));
            }
        }
    }
    assert_eq!(process_caches_cap(16 << 30, 0), (4 << 30) + (512 << 20), "16 GiB: 2 GiB + 2 GiB + 256 MiB + 256 MiB");
    assert_eq!(process_caches_cap(0, u64::MAX), u64::MAX, "an unbounded pool over unknown RAM saturates");
    assert_eq!((condition_cache_cap(16 << 30, 0), zone_store_cap(16 << 30, 128 << 20)), (256 << 20, 128 << 20));
}

#[test]
fn kept_buffers_are_released_after_the_pool_is_idle() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    reuse::set_idle_ms(100);
    let mut b = reuse::take(1 << 16);
    b.iter_mut().for_each(|w| *w = u64::MAX);
    let before = reuse::IDLE_RELEASES.load(std::sync::atomic::Ordering::Relaxed);
    reuse::give(b);
    let t = std::time::Instant::now();
    while reuse::IDLE_RELEASES.load(std::sync::atomic::Ordering::Relaxed) == before {
        assert!(t.elapsed() < std::time::Duration::from_secs(20), "no idle release");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // the kept buffer comes back (the pool's best fit: the one given, or a larger kept one), released
    let back = reuse::take(1 << 16);
    assert!(back.len() >= 1 << 16, "a kept buffer, not a fresh one");
    if cfg!(target_os = "linux") {
        // the whole pages inside it read zero once released
        assert!(back[512..back.len() - 512].iter().all(|&w| w == 0), "the kept pages were released");
    }
    reuse::set_idle_ms(reuse::IDLE_RELEASE_MS);
}

/// Broker (b), `broker_value=1`: a kept buffer the reclaimer frees moves the pool's byte clock and leaves a ghost, which the
/// next take it would have served finds (a ghost hit, once, at distance 0: nothing freed since); a take only the oldest
/// tenth of the kept words could serve is a tail hit, one a newer buffer serves too is not.
#[test]
fn broker_value_counts_the_pool_s_frees_ghosts_and_tail_takes() {
    use memledger::{value, Consumer::BuildPool};
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let words = gravity_program::cost::DICT_POOL_MIN_WORDS * 3;
    // the other tests' kept buffers freed before the switch: no ghosts of theirs
    reuse::release_to(0);
    value::set_ext(true);
    reuse::give(vec![7u64; words]);
    assert_eq!(reuse::held(), (1, words));
    let e0 = value::ext_stats(BuildPool);
    assert_eq!(reuse::release_to(0), words as u64 * 8);
    let e1 = value::ext_stats(BuildPool);
    assert_eq!(e1.evicted_bytes - e0.evicted_bytes, words as u64 * 8, "the clock moves by the bytes freed");
    drop(reuse::take(words));
    let e2 = value::ext_stats(BuildPool);
    assert_eq!(e2.ghost_hits - e1.ghost_hits, 1, "the take the freed buffer would have served");
    assert_eq!(e2.refault_log2[0] - e1.refault_log2[0], 1, "nothing freed since: distance 0");
    drop(reuse::take(words));
    assert_eq!(value::ext_stats(BuildPool).ghost_hits, e2.ghost_hits, "a ghost counts once");

    // the oldest buffer twice the size of nine newer ones: the tenth is the oldest alone
    reuse::give(vec![0u64; 2 * words]);
    for _ in 0..9 {
        reuse::give(vec![0u64; words]);
    }
    reuse::publish_tail();
    assert_eq!(value::ext_stats(BuildPool).tail_bytes, 11 * words as u64 * 8 / 10);
    let t0 = value::ext_stats(BuildPool).tail_hits;
    reuse::give(reuse::take(words));
    assert_eq!(value::ext_stats(BuildPool).tail_hits, t0, "a newer buffer serves it too");
    let big = reuse::take(2 * words);
    assert_eq!(big.len(), 2 * words);
    assert_eq!(value::ext_stats(BuildPool).tail_hits, t0 + 1, "only the oldest serves it");
    reuse::release_to(0);
}

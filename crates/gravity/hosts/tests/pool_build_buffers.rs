//! pagepool P3 (`PGRUST_POOL_GRAVITY=1`): a table build's buffer comes from the page pool with its own charge
//! (`Door::Pool`), on the calling thread's owner from its take, on the build pool's account while kept, back on a taker's;
//! the idle release gives a kept buffer's whole runs to the pool dead (the purger may release them) and off the build
//! pool's account, and the next take brings them back on it.
#![cfg(target_os = "linux")]

use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use gravity_hosts::dict::text_hosts::reuse;
use memledger::{account_row, Acct, Consumer};

fn pool_bytes(a: Acct) -> i64 {
    memledger::fold();
    account_row(a).pagepool_bytes
}

#[test]
fn a_build_buffer_is_the_pools_and_its_kept_runs_go_dead_at_the_idle_release() {
    std::env::set_var("PGRUST_POOL_GRAVITY", "1");
    assert!(pagepool::switches().gravity, "the switch, read at its first call");
    assert!(pagepool::init(&pagepool::Config::from_policy()), "the pool reserves its zones");
    let bp = Acct::of(Consumer::BuildPool);
    std::thread::spawn(move || {
        memledger::bind_backend(4801, 9000 + 4801);
        let a = Acct::backend(4801);
        let words = gravity_program::cost::DICT_POOL_MIN_WORDS * 16;
        let (q0, b0) = (pool_bytes(a), pool_bytes(bp));
        let mut b = reuse::take(words);
        assert!(pagepool::owns(b.as_ptr() as *const u8), "the pool's buffer");
        assert!(b.len() >= words);
        let bytes = (b.len() * 8) as i64;
        assert_eq!(pool_bytes(a) - q0, bytes, "on the calling thread's owner from its take");
        b.iter_mut().for_each(|w| *w = 7);
        let at = b.as_ptr();
        reuse::give(b);
        assert_eq!((pool_bytes(a) - q0, pool_bytes(bp) - b0), (0, bytes), "kept: the build pool's");
        let dead0 = pagepool::stats().dead;
        reuse::set_idle_ms(100);
        let before = reuse::IDLE_RELEASES.load(Relaxed);
        let t = Instant::now();
        while reuse::IDLE_RELEASES.load(Relaxed) == before {
            assert!(t.elapsed() < Duration::from_secs(20), "no idle release");
            std::thread::sleep(Duration::from_millis(10));
        }
        reuse::set_idle_ms(reuse::IDLE_RELEASE_MS);
        assert!(pagepool::stats().dead >= dead0 + bytes as usize - 2 * (64 << 10), "its whole runs dead in the pool");
        assert!(pool_bytes(bp) - b0 <= 2 * (64 << 10), "and off the build pool's account");
        let back = reuse::take(words);
        assert_eq!(back.as_ptr(), at, "the kept buffer");
        assert_eq!(pagepool::stats().dead, dead0, "its runs live again");
        assert_eq!((pool_bytes(a) - q0, pool_bytes(bp) - b0), (bytes, 0), "back on the taker");
        drop(back);
        assert_eq!(pool_bytes(a), q0, "off at the drop");
        reuse::release_to(0);
        pagepool::flush_shelf();
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}

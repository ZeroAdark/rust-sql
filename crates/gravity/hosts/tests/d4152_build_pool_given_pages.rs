//! D4152 (C8 area move 7): the build pool's given-back pages are each kept buffer's own. The pool kept one count of the
//! pages its idle release gave back, and the next buffer to leave took them back whichever buffer it was: a fresh buffer
//! given after an idle release and taken again put another buffer's given-back pages back on the pool's account (and
//! off memledger's given-back total), though those pages stayed given back.
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use gravity_hosts::dict::text_hosts::reuse;
use memledger::{account_row, given_back_bytes, Acct, Consumer};

#[test]
fn a_fresh_buffer_leaves_the_pool_with_none_of_an_older_buffers_given_pages() {
    if !cfg!(target_os = "linux") {
        // the idle release gives pages back on Linux only
        return;
    }
    let m = gravity_program::cost::DICT_POOL_MIN_WORDS;
    let net = || account_row(Acct::of(Consumer::BuildPool)).account_bytes;
    // an older buffer of 16 m words, its pages given back by the idle release
    reuse::set_idle_ms(100);
    let before = reuse::IDLE_RELEASES.load(Relaxed);
    reuse::give(vec![1u64; 16 * m]);
    let t = Instant::now();
    while reuse::IDLE_RELEASES.load(Relaxed) == before {
        assert!(t.elapsed() < Duration::from_secs(20), "no idle release");
        std::thread::sleep(Duration::from_millis(10));
    }
    reuse::set_idle_ms(reuse::IDLE_RELEASE_MS);
    let (n0, g0) = (net(), given_back_bytes());
    assert!(g0 >= (m as i64) * 8, "the older buffer's pages were given back: {g0}");
    // a fresh buffer of m words in and out (the older one is past FIT for a take of m words)
    reuse::give(vec![2u64; m]);
    assert_eq!(net(), n0 + (m as i64) * 8, "the fresh buffer's bytes on the pool's account");
    let b = reuse::take(m);
    assert_eq!(b.len(), m, "the fresh buffer back");
    assert_eq!((net(), given_back_bytes()), (n0, g0), "the older buffer's pages stay given back, off the pool's account");
    drop(b);
    reuse::release_to(0);
}

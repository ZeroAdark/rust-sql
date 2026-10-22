//! Takes on one thread, gives on another: no block handed out twice (each stamped, its first and last grains held in a
//! table) and the counters balance exactly at quiescence. Default 8 threads; PAGEPOOL_BIG_TESTS=1: 32 threads, 10 M ops.

mod common;
use common::*;
use core::sync::atomic::{AtomicU8, Ordering::*};
use pagepool::*;
use std::sync::mpsc::{sync_channel, TrySendError};
use std::sync::Arc;

struct Held {
    lo: usize,
    flags: Vec<AtomicU8>,
}

impl Held {
    fn grain(&self, a: usize) -> &AtomicU8 {
        &self.flags[(a - self.lo) >> 9]
    }
    fn hold(&self, a: usize, len: usize) {
        for g in [a, a + len - 512] {
            assert_eq!(self.grain(g).swap(1, AcqRel), 0, "{a:#x} (+{len}) handed out while held");
        }
    }
    fn free(&self, a: usize, len: usize) {
        for g in [a, a + len - 512] {
            assert_eq!(self.grain(g).swap(0, AcqRel), 1, "{a:#x} given while not held");
        }
    }
}

fn size(r: &mut u64) -> usize {
    *r ^= *r << 13;
    *r ^= *r >> 7;
    *r ^= *r << 17;
    let x = *r % 1000;
    let y = (*r >> 20) as usize;
    match x {
        0..700 => 1024 + y % (128 << 10),
        700..960 => (2 + y % 30) * (64 << 10),
        960..998 => (2 + y % 6) * SUPER,
        _ => 32 * MIB + y % (8 * MIB),
    }
}

fn stamp(a: usize, len: usize, tok: u64) {
    // SAFETY: a block we hold, at least 1 KiB.
    unsafe {
        ((a + 16) as *mut u64).write_unaligned(tok);
        ((a + len - 8) as *mut u64).write_unaligned(!tok);
    }
}

fn check(a: usize, len: usize, tok: u64) {
    // SAFETY: as above.
    let (x, y) = unsafe { (((a + 16) as *const u64).read_unaligned(), ((a + len - 8) as *const u64).read_unaligned()) };
    assert_eq!((x, y), (tok, !tok), "{a:#x} (+{len}) was written by another holder");
}

#[test]
fn cross_thread() {
    init_with(|c| c.va = if big() { 4 << 30 } else { 1 << 30 });
    let (threads, ops) = if big() { (32usize, 10_000_000usize) } else { (8, 400_000) };
    let (lo, len) = range();
    let held = Arc::new(Held { lo, flags: (0..len >> 9).map(|_| AtomicU8::new(0)).collect() });
    let mut txs = Vec::new();
    let mut rxs = Vec::new();
    for _ in 0..threads {
        let (tx, rx) = sync_channel::<(usize, usize, u64)>(16);
        txs.push(tx);
        rxs.push(Some(rx));
    }
    let t0 = std::time::Instant::now();
    let mut hs = Vec::new();
    for i in 0..threads {
        let tx = txs[(i + 1) % threads].clone();
        let rx = rxs[i].take().unwrap();
        let held = held.clone();
        let per = ops / threads;
        hs.push(std::thread::spawn(move || {
            let mut r = 0x9e37_79b9_7f4a_7c15u64 ^ (i as u64 + 1);
            let class = [Class::Task, Class::Session, Class::General][i % 3];
            let h = Hint::new(class, Tag(i as u8 % 16));
            let give_one = |(a, len, tok): (usize, usize, u64)| {
                check(a, len, tok);
                held.free(a, len);
                // SAFETY: taken by some thread, handed over, not used again.
                unsafe { give(core::ptr::NonNull::new(a as *mut u8).unwrap(), len) };
            };
            for k in 0..per {
                let want = size(&mut r);
                let p = take(want, 16, h).expect("take");
                let (a, len) = (p.cast::<u8>().as_ptr() as usize, p.len());
                assert!(len >= want);
                held.hold(a, len);
                let tok = ((i as u64) << 40) | k as u64;
                stamp(a, len, tok);
                match tx.try_send((a, len, tok)) {
                    Ok(()) => {}
                    Err(TrySendError::Full(x)) | Err(TrySendError::Disconnected(x)) => give_one(x),
                }
                while let Ok(x) = rx.try_recv() {
                    give_one(x);
                }
            }
            drop(tx);
            for x in rx {
                give_one(x);
            }
        }));
    }
    drop(txs);
    for h in hs {
        h.join().unwrap();
    }
    let secs = t0.elapsed().as_secs_f64();
    eprintln!("MEASURED cross-thread: {threads} threads, {ops} take + give pairs in {secs:.2} s = {:.0} ops/s", 2.0 * ops as f64 / secs);
    balanced();
    let s = stats();
    assert_eq!(s.in_use_total(), 0, "{s:?}");
}

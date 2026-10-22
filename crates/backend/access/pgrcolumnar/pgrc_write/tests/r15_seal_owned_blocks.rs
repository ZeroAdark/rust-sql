#![cfg(not(loom))]
//! R15-SEAL-IMAGE-CHARGE + D4594: no big block a seal allocates sits unowned while the seal works on. At every allocation
//! of at least `BIG` on the sealing thread, the thread's heap that no charge owns (its live bytes less its statement's
//! committed bytes and the parked holder's) is within `TOLERANCE` of what it was when the seal began: a block counted
//! only after the work that follows its allocation shows up at the next big allocation. Its own binary: the global
//! allocator here counts each thread's live bytes and checks them at the big allocations of a watched thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use memledger::charge::{context, owner_row};
use memledger::{bind_backend, door_heap, statement_begin, statement_end, unbind_backend, Holder, Owner};
use pgrc_write::ingest::RawDatum;
use pgrc_write::testkit::{img_4b_u, int8_col, mem_with_dir, open_writer_policy, stamp, text_col, Kit};
use pgrc_write::writer::{PartCutPolicy, SealEnv};

const BIG: usize = 1 << 20;
const TOLERANCE: i64 = 1 << 20;

thread_local! {
    static LIVE: Cell<i64> = const { Cell::new(0) };
    /// The watched statement owner, and the unowned bytes at the watch's start.
    static WATCH: Cell<Option<Owner>> = const { Cell::new(None) };
    static BASE: Cell<i64> = const { Cell::new(0) };
}

static WORST: AtomicI64 = AtomicI64::new(i64::MIN);
static WORST_SIZE: AtomicUsize = AtomicUsize::new(0);
static CHECKED: AtomicUsize = AtomicUsize::new(0);

fn unowned(o: Owner) -> i64 {
    LIVE.with(Cell::get) - owner_row(o).committed - owner_row(Owner::Cache(Holder::Parked)).committed
}

struct Door1;

// SAFETY: forwards to the system allocator; the counters are const thread-locals and atomics (no allocation, no
// destructor), and `owner_row` reads static cells.
unsafe impl GlobalAlloc for Door1 {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if l.size() >= BIG {
            if let Some(o) = WATCH.with(Cell::get) {
                let past = unowned(o) - BASE.with(Cell::get);
                CHECKED.fetch_add(1, Ordering::Relaxed);
                if past > WORST.fetch_max(past, Ordering::Relaxed) {
                    WORST_SIZE.store(l.size(), Ordering::Relaxed);
                }
            }
        }
        // SAFETY: the caller's layout, forwarded.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            door_heap(l.size() as isize);
            LIVE.with(|c| c.set(c.get() + l.size() as i64));
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: `p` came from `alloc` with `l`.
        unsafe { System.dealloc(p, l) };
        door_heap(-(l.size() as isize));
        LIVE.with(|c| c.set(c.get() - l.size() as i64));
    }
}

#[global_allocator]
static A: Door1 = Door1;

/// splitmix64: values no encoding packs.
fn scatter(i: u64) -> u64 {
    let mut z = i.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// A part of an incompressible int8 column and a text column of 40,000 distinct 64-byte values (a dictionary build of
/// megabytes), sealed by the serial writer on its statement's owner.
#[test]
fn a_seal_leaves_no_big_block_unowned_while_it_works() {
    std::thread::spawn(|| {
        bind_backend(47, 8200 + 47);
        statement_begin();
        let (o, _) = context();
        let mut vfs = mem_with_dir();
        let mut kit = Kit::new();
        let policy = PartCutPolicy { max_rows: 1 << 20, max_bytes: u64::MAX, cut_granule_rows: 1024 };
        let mut w = open_writer_policy(vec![int8_col(1), text_col(2)], stamp(79, 1), policy);
        w.set_owner(Some(o));
        let n = 256 * 1024u64;
        for i in 0..n {
            let text = img_4b_u(format!("{:064x}", scatter(i % 40_000) as u128 * 3).as_bytes());
            let sources: [&dyn pgrc_write::elect::CandidateSource; 1] = [&kit.cands];
            let mut env =
                SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
            w.append_row(&[RawDatum::Word(scatter(i)), RawDatum::Bytes(&text)], &mut kit.ext, &mut env).expect("append");
        }
        BASE.with(|b| b.set(unowned(o)));
        WATCH.with(|c| c.set(Some(o)));
        {
            let sources: [&dyn pgrc_write::elect::CandidateSource; 1] = [&kit.cands];
            let mut env =
                SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
            w.finish(&mut env).unwrap();
        }
        WATCH.with(|c| c.set(None));
        assert_eq!(w.sealed_parts().len(), 1);
        let (worst, size, checked) =
            (WORST.load(Ordering::Relaxed), WORST_SIZE.load(Ordering::Relaxed), CHECKED.load(Ordering::Relaxed));
        assert!(checked >= 3, "the seal allocated {checked} blocks of {BIG} B or more: the shape is too small to watch");
        assert!(
            worst < TOLERANCE,
            "{worst} B of the seal's heap were unowned at the allocation of a {size} B block ({checked} big blocks)"
        );
        drop(w);
        drop(vfs);
        statement_end();
        unbind_backend();
    })
    .join()
    .unwrap();
}

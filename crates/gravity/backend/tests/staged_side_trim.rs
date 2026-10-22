//! D3168: a staged function's IR is the code cache's (backend `staged`, cache `file_side`). Filed, it is in the cache's
//! bytes (`stats().bytes`, the program caches' census) by exactly what the program caches' owner holds for it; the ladder's
//! trim of the cache (`cache::trim_to`) gives every one of those bytes back while the sides live, and a trimmed side makes
//! no code (its plain function runs with the stage off); dropping the sides leaves the program caches where they started.
//! A side whose code is made gives its IR back at the make. Door 1 counts every block on the calling thread here, as the
//! server's allocator does.
use std::alloc::{GlobalAlloc, Layout, System};

use gravity_backend::cache;
use gravity_ir::text::parse_program;
use gravity_program::code::Staged;
use memledger::{Consumer, Holder, Owner};

struct Door1;

unsafe impl GlobalAlloc for Door1 {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            memledger::door_heap(l.size() as isize);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l);
        memledger::door_heap(-(l.size() as isize));
    }
}

#[global_allocator]
static A: Door1 = Door1;

const PC: Owner = Owner::Cache(Holder::Of(Consumer::ProgramCaches));
/// Sides per round.
const N: i64 = 64;
/// This test's backend slot.
const PROCNO: u32 = 4078;

fn konst(k: i64) -> gravity_ir::Program {
    let src = format!("(program (fn e ((c ptr)) (i64) (block entry ((c ptr)) (k (const i64 {k})) (return k))))");
    parse_program(&src, &|_| None).unwrap_or_else(|e| panic!("{e}"))
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Snap {
    account: i64,
    owner: i64,
    keepers: i64,
    census: i64,
}

fn snap() -> Snap {
    memledger::fold();
    Snap {
        account: memledger::account_row(PC.acct()).account_bytes,
        owner: memledger::charge::owner_row(PC).committed + memledger::charge::owner_ctx(PC).0,
        keepers: memledger::keepers(PC.acct()),
        census: cache::stats().bytes as i64,
    }
}

fn sides(base: i64) -> Vec<Box<dyn Staged>> {
    (0..N).map(|i| gravity_backend::staged(&konst(base + i), false, 0)).collect()
}

#[test]
fn a_staged_functions_ir_is_the_code_caches_and_its_trim_gives_every_byte_back() {
    memledger::set_home(true);
    cache::set_budget_bytes(1 << 30);
    std::thread::spawn(move || {
        memledger::bind_backend(PROCNO, 9000 + PROCNO);
        memledger::statement_begin();
        // the first round grows what stays (the cache's tables, this thread's pools)
        drop(sides(3_168_000_000));
        let s0 = snap();
        let held = sides(3_168_100_000);
        let s1 = snap();
        let ir = s1.census - s0.census;
        let boxes = (s1.account - s0.account) - ir;
        eprintln!("D3168 side IR, {N} sides: filed {s0:?} -> {s1:?}: census +{ir}, the sides' boxes {boxes}");
        assert!(ir > 0, "filed: the IR is in the cache's bytes");
        assert_eq!((s1.account - s0.account) - (s1.owner - s0.owner) - (s1.keepers - s0.keepers), 0, "every byte the sides added to the program caches is their owner's (or a parked keeper)");
        assert!(boxes > 0 && boxes % N == 0 && boxes / N <= 1024, "what the census leaves out is the sides' own boxes: {boxes}");
        let freed = cache::trim_to(0) as i64;
        let s2 = snap();
        eprintln!("D3168 side IR: trim_to(0) freed {freed}: {s2:?}");
        assert_eq!(freed, ir, "the trim takes every IR no compile reads");
        assert_eq!((s2.census, s2.account - s0.account), (s0.census, boxes), "trimmed: the program caches gave back every byte the census counted; the boxes stay with their sides");
        assert_eq!((s2.account - s0.account) - (s2.owner - s0.owner) - (s2.keepers - s0.keepers), 0, "and what stays is their owner's");
        assert!(held.iter().all(|s| s.code().is_none()), "a trimmed side makes no code: its plain function runs with the stage off");
        drop(held);
        let s3 = snap();
        eprintln!("D3168 side IR: the sides dropped: {s3:?}");
        assert_eq!((s3.account, s3.owner, s3.census), (s0.account, s0.owner, s0.census), "the sides dropped: the program caches back to exactly their start");
        let made = sides(3_168_200_000);
        assert!(made.iter().all(|s| s.code().is_some()), "an untrimmed side makes its code");
        assert_eq!(cache::in_use_bytes(), cache::stats().bytes, "made: every IR given back; the cache holds the modules their codes run");
        drop(made);
        memledger::statement_end();
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}

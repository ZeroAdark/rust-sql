//! C8: the charge API allocates nothing (a part use is taken on every scan's part, a region commits on every grow; the
//! allocator's own door may reach a charge's owner). One test in its own binary, under a counting allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use memledger::charge::{context, enter_portal, owner_row, portal_begin, portal_end, worker_set_begin, worker_set_end, Door};
use memledger::{bind_backend, statement_begin, statement_end, unbind_backend, Acct, Charge, Consumer, Holder, Kind, Owner, Release};

thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

// SAFETY: forwards to the system allocator; the count is a const thread-local cell (no destructor, no allocation).
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.with(|n| n.set(n.get() + 1));
        // SAFETY: the caller's layout, forwarded.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: `p` came from `alloc` with `l`.
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static A: Counting = Counting;

#[test]
fn the_charge_api_allocates_nothing() {
    std::thread::spawn(|| {
        bind_backend(3950, 1);
        let a = Acct::backend(3950);
        statement_begin();
        let before = ALLOCS.with(|n| n.get());
        portal_begin(5);
        let (o, rel) = context();
        let mut c = Charge::reserve(o, rel, Kind::HashBuild, Door::Mapped, 4 << 20);
        c.commit(2 << 20);
        c.uncommit(1 << 20);
        c.grow(1 << 20);
        c.shrink(1 << 20);
        let u = {
            let _p = enter_portal(5);
            let (p, prel) = context();
            Charge::use_of(p, prel)
        };
        c.transfer(Owner::Cache(Holder::Of(Consumer::FpTables)), Release::Evict);
        worker_set_begin(17, a);
        let w = Charge::open(Owner::WorkerSet(17), Release::Session, Kind::Arena, Door::Mapped, 1 << 20);
        let mut f = Charge::open(Owner::Cache(Holder::Of(Consumer::PartCache)), Release::Evict, Kind::Region, Door::Heap, 1 << 20);
        f.free(1 << 19);
        let _ = owner_row(o);
        drop((c, u, w, f));
        let _ = portal_end(a, 5);
        let _ = worker_set_end(17);
        assert_eq!(ALLOCS.with(|n| n.get()), before, "a charge's life allocated");
        statement_end();
        unbind_backend();
    })
    .join()
    .unwrap();
}

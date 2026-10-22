//! C8 area move 7 (C1): a memory context's mapped block (heap.rs: a request of `MMAP_THRESHOLD` bytes or more, its own
//! mapping) is door 2's through the ledger's home (`memledger::charge::door_mapped`): its bytes go on the calling thread's
//! context owner, its cell as well as its account, at the map, and the home remembers that owner for the block, so the
//! unmap and a resize move THAT owner's numbers, whichever owner frees or grows it (memlead 2026-10-12T17:00:54Z, main
//! 17:02:28Z). Before C8 they moved the calling thread's account only: no owner's cell showed the block, so the per-owner
//! rank never counted it and an owner's end never read a block that outlived the owner; charged to the freeing thread's
//! owner instead, a cursor's block freed at its CLOSE statement left a +b residue on the portal and -b on the CLOSE.
#![cfg(all(feature = "std", unix))]

use std::alloc::Layout;

use mcx::heap::{Heap, MMAP_THRESHOLD};
use mcx::Allocator;
use memledger::charge::{enter_portal, new_owner_id, owner_row, portal_begin, portal_end, OwnerClass};
use memledger::owner_end::{for_each_recent, Source};
use memledger::{Acct, Owner};

/// A block past the threshold, a whole number of pages.
const BYTES: usize = 2 * MMAP_THRESHOLD;

fn block() -> Layout {
    Layout::from_size_align(BYTES, 8).unwrap()
}

/// `block` grown by half again (a remap).
const GROWN: usize = 3 * MMAP_THRESHOLD;

fn grown() -> Layout {
    Layout::from_size_align(GROWN, 8).unwrap()
}

/// This backend's residues in the owner-end ring: (class, committed).
fn residues(pid: u32) -> Vec<(OwnerClass, i64)> {
    let mut out = Vec::new();
    for_each_recent(|r| {
        if r.pid == pid && r.source == Source::Owner {
            out.push((r.class, r.committed));
        }
    });
    out
}

/// On a new thread bound to backend `procno` (pid `pid`), with a statement running: `f(account, the statement owner)`.
fn in_statement(procno: u32, pid: u32, f: impl FnOnce(Acct, Owner) + Send + 'static) {
    std::thread::spawn(move || {
        let a = Acct::backend(procno);
        assert!(a.is_backend());
        memledger::bind_backend(procno, pid);
        memledger::statement_begin();
        let x = Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
        f(a, x);
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}

#[test]
fn a_mapped_block_is_its_statements_until_the_free() {
    in_statement(4040, 9040, |a, x| {
        let c0 = owner_row(x).committed;
        let p = Heap.allocate(block()).expect("a mapping");
        assert_eq!(owner_row(x).committed - c0, BYTES as i64, "the block is on its statement's cell");
        // SAFETY: `p` is the block `allocate` just mapped for this layout.
        unsafe { Heap.deallocate(p.cast(), block()) };
        assert_eq!(owner_row(x).committed, c0, "the free takes it off");
        let _ = memledger::statement_end();
        let mut residue = None;
        for_each_recent(|r| {
            if r.pid == 9040 && r.acct == a {
                residue = Some(r.committed);
            }
        });
        assert_eq!(residue, None, "the statement ended owning nothing");
    });
}

#[test]
fn a_mapped_block_that_outlives_its_statement_is_its_residue() {
    in_statement(4041, 9041, |a, x| {
        let p = Heap.allocate(block()).expect("a mapping");
        assert_eq!(owner_row(x).committed, BYTES as i64, "on its statement's cell");
        let _ = memledger::statement_end();
        let mut seen = None;
        for_each_recent(|r| {
            if seen.is_none() && r.pid == 9041 && r.source == Source::Owner && r.class == OwnerClass::Statement {
                seen = Some(r.committed);
            }
        });
        assert_eq!(seen, Some(BYTES as i64), "the statement's end read the block that outlived it");
        let s = Owner::Session(a);
        assert_eq!(owner_row(s).committed, BYTES as i64, "handed on to its session");
        // SAFETY: as above; freed between statements, under the session
        unsafe { Heap.deallocate(p.cast(), block()) };
        assert_eq!(owner_row(s).committed, 0, "the free under the session takes it off");
    });
}

#[test]
fn a_cursors_block_freed_at_the_close_statement_leaves_no_residue() {
    std::thread::spawn(|| {
        let a = Acct::backend(4042);
        memledger::bind_backend(4042, 9042);
        // the statement that runs the cursor: its portal maps a block (a sort's or a hash table's) while it executes
        memledger::statement_begin();
        let token = new_owner_id();
        portal_begin(token);
        let p = {
            let _run = enter_portal(token);
            Heap.allocate(block()).expect("a mapping")
        };
        let portal = Owner::Portal { acct: a, portal: token };
        assert_eq!(owner_row(portal).committed, BYTES as i64, "the block is the cursor's portal's");
        let _ = memledger::statement_end();
        // the CLOSE statement: PortalDrop's ExecutorEnd frees the block outside the portal's run, then the portal ends
        memledger::statement_begin();
        let close = Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
        // SAFETY: `p` is the block `allocate` mapped for this layout
        unsafe { Heap.deallocate(p.cast(), block()) };
        assert_eq!(owner_row(close).committed, 0, "the CLOSE statement does not pay for the cursor's block");
        assert_eq!(owner_row(portal).committed, 0, "the free takes it off the portal that owns it");
        assert_eq!(portal_end(a, token), None, "the portal ends owning nothing");
        let _ = memledger::statement_end();
        assert_eq!(residues(9042), vec![], "no owner of this backend ended with a residue");
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}

#[test]
fn a_mapped_block_grown_under_another_statement_stays_its_owners() {
    in_statement(4043, 9043, |a, _| {
        let p = Heap.allocate(block()).expect("a mapping");
        // the statement that mapped it ends: the block (its residue) is its session's
        let _ = memledger::statement_end();
        memledger::statement_begin();
        let (y, s) = (Owner::Statement { acct: a, stmt: memledger::running_statement(a) }, Owner::Session(a));
        // SAFETY: `p` is the block `allocate` mapped for `block()`; `grown()` keeps its alignment
        let q = unsafe { Heap.grow(p.cast(), block(), grown()) }.expect("a remap");
        assert_eq!(owner_row(y).committed, 0, "the statement that grows it does not take it");
        assert_eq!(owner_row(s).committed, GROWN as i64, "grown, it is still its owner's (the session, after its statement)");
        // SAFETY: `q` is the grown block
        unsafe { Heap.deallocate(q.cast(), grown()) };
        assert_eq!((owner_row(y).committed, owner_row(s).committed), (0, 0), "the free takes it off its owner");
    });
}

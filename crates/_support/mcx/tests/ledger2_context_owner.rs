//! LEDGER2 step 2 (the accounting prototype): a memory context's blocks charge the context's owner,
//! not the thread's current statement. A child takes its parent's owner; an unnamed root takes the creating thread's owner
//! once, at its creation; a named root is re-owned in one step (`set_owner_*`). Its bytes follow the owner
//! through its end (to the session), and a free under a later statement lands where the bytes are, so no owner row goes
//! negative (D4382 g/h's shape). A mapped block maps for the context's owner. A block allocated or freed on a thread of
//! another account moves to (from) the owner's account with it, so an account holds what its owners hold.
#![cfg(all(feature = "std", unix))]

use std::alloc::Layout;

use mcx::heap::MMAP_THRESHOLD;
use mcx::{Allocator, MemoryContext};
use memledger::charge::{owner_ctx, owner_row, usage};
use memledger::{Acct, Owner};

const KIB: usize = 1 << 10;

fn chunk(n: usize) -> Layout {
    Layout::from_size_align(n, 8).unwrap()
}

/// On a new thread bound to backend `procno`: `f(account)`.
fn on(procno: u32, f: impl FnOnce(Acct) + Send + 'static) {
    std::thread::spawn(move || {
        memledger::bind_backend(procno, 9000 + procno);
        f(Acct::backend(procno));
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}

fn stmt(a: Acct) -> Owner {
    Owner::Statement { acct: a, stmt: memledger::running_statement(a) }
}

#[test]
fn ledger2_an_unnamed_roots_bytes_follow_its_creating_statement_and_a_later_free_finds_them() {
    on(4060, |a| {
        memledger::statement_begin();
        let s1 = stmt(a);
        let ctx = MemoryContext::new("ledger2-unnamed-root");
        let p = ctx.mcx().allocate(chunk(256 * KIB)).expect("a dedicated chunk");
        let (_, inferred) = owner_ctx(s1);
        assert!(inferred >= 256 * KIB as i64, "the creating statement's, inferred: {inferred}");
        let mut seen = 0;
        memledger::charge::unnamed_roots(|name, b| {
            if name == "ledger2-unnamed-root" {
                seen = b;
            }
        });
        assert_eq!(seen, inferred, "counted under the root's name");
        let _ = memledger::statement_end();
        let s = Owner::Session(a);
        assert_eq!(owner_ctx(s).1, inferred, "the statement's end hands its context bytes to the session");
        memledger::statement_begin();
        let s2 = stmt(a);
        // SAFETY: `p` is the chunk this context handed out for this layout
        unsafe { ctx.mcx().deallocate(p.cast(), chunk(256 * KIB)) };
        drop(ctx);
        assert_eq!(owner_ctx(s2), (0, 0), "the later statement pays nothing");
        assert_eq!(owner_ctx(s), (0, 0), "the free and the drop take them off where they are: never below 0");
        let _ = memledger::statement_end();
    });
}

#[test]
fn ledger2_a_named_root_moves_its_bytes_and_its_children_take_its_owner() {
    on(4061, |a| {
        memledger::statement_begin();
        let st = stmt(a);
        let root = MemoryContext::new("ledger2-named-root");
        let p = root.mcx().allocate(chunk(64 * KIB)).expect("a dedicated chunk");
        let (_, inferred) = owner_ctx(st);
        root.set_owner_session();
        let s = Owner::Session(a);
        assert_eq!((owner_ctx(st).1, owner_ctx(s).0), (0, inferred), "moved in one step, now named");
        let child = root.new_child("ledger2-child");
        let q = child.mcx().allocate(chunk(32 * KIB)).expect("a dedicated chunk");
        assert!(owner_ctx(s).0 >= inferred + 32 * KIB as i64, "the child takes its parent's owner");
        let token = memledger::charge::new_owner_id();
        memledger::charge::portal_begin(token);
        let po = Owner::Portal { acct: a, portal: token };
        let pc = root.new_child("ledger2-portal-context");
        pc.set_owner_portal(token);
        let r = pc.mcx().allocate(chunk(128 * KIB)).expect("a dedicated chunk");
        assert!(owner_ctx(po).0 >= 128 * KIB as i64, "the portal's");
        assert_eq!(usage(st).held, 0, "nothing of it on the statement");
        // SAFETY: each pointer is the chunk its context handed out for its layout
        unsafe {
            pc.mcx().deallocate(r.cast(), chunk(128 * KIB));
            child.mcx().deallocate(q.cast(), chunk(32 * KIB));
            root.mcx().deallocate(p.cast(), chunk(64 * KIB));
        }
        drop(pc);
        let _ = memledger::charge::portal_end(a, token);
        drop(child);
        drop(root);
        assert_eq!((owner_ctx(po), owner_ctx(s)), ((0, 0), (0, 0)));
        let _ = memledger::statement_end();
    });
}

#[test]
fn ledger2_a_mapped_block_maps_for_its_contexts_owner_not_the_running_statement() {
    on(4062, |a| {
        memledger::statement_begin();
        let st = stmt(a);
        let ctx = MemoryContext::new("ledger2-mapped");
        ctx.set_owner_session();
        let s = Owner::Session(a);
        let big = chunk(2 * MMAP_THRESHOLD);
        let (c0, n0) = (owner_row(s).committed, owner_ctx(s).0);
        let p = ctx.mcx().allocate(big).expect("a mapping");
        assert_eq!(owner_row(s).committed - c0, 2 * MMAP_THRESHOLD as i64, "on the context's owner, door 2");
        assert_eq!(owner_ctx(s).0, n0, "not also in its door-1 context bytes");
        assert_eq!(owner_row(st).committed, 0, "not on the running statement");
        // SAFETY: `p` is the mapping this context handed out for this layout
        unsafe { ctx.mcx().deallocate(p.cast(), big) };
        assert_eq!(owner_row(s).committed, c0);
        drop(ctx);
        assert_eq!(owner_ctx(s).0, 0);
        let _ = memledger::statement_end();
    });
}

#[test]
fn ledger2_a_reset_gives_back_what_it_frees() {
    on(4063, |a| {
        let ctx = MemoryContext::new_wholesale("ledger2-reset");
        ctx.set_owner_session();
        let s = Owner::Session(a);
        for _ in 0..64 {
            let _ = ctx.mcx().allocate(chunk(4 * KIB)).expect("a pooled chunk");
        }
        let grown = owner_ctx(s).0;
        let mut ctx = ctx;
        ctx.reset();
        let kept = owner_ctx(s).0;
        assert!(kept < grown && kept >= 0, "the reset's freed blocks leave the owner: {grown} -> {kept}");
        drop(ctx);
        assert_eq!(owner_ctx(s).0, 0);
    });
}

#[test]
fn ledger2_a_block_on_another_accounts_thread_moves_to_the_owners_account() {
    on(4064, |a| {
        let b = Acct::backend(4065);
        let net = |x: Acct| memledger::account_row(x).account_bytes;
        let ctx = MemoryContext::new("ledger2-cross-account");
        ctx.set_owner_session();
        let s = Owner::Session(a);
        memledger::fold();
        let (a0, c0) = (net(a), owner_ctx(s).0);
        let g = memledger::charge::enter_as(Owner::Session(b));
        let p = ctx.mcx().allocate(chunk(256 * KIB)).expect("a dedicated chunk");
        memledger::fold();
        let grown = owner_ctx(s).0 - c0;
        assert!(grown >= 256 * KIB as i64, "the context's owner: {grown}");
        assert_eq!(net(a) - a0, grown, "and its account, though another account's thread allocated it");
        // SAFETY: `p` is the chunk this context handed out for this layout
        unsafe { ctx.mcx().deallocate(p.cast(), chunk(256 * KIB)) };
        memledger::fold();
        drop(g);
        memledger::fold();
        assert_eq!((net(a) - a0, owner_ctx(s).0 - c0), (0, 0), "the free on that thread takes them back off both");
        drop(ctx);
    });
}

#[test]
fn ledger2_set_owner_tree_moves_the_whole_live_subtree_and_later_children_take_the_new_owner() {
    on(4066, |a| {
        memledger::statement_begin();
        let st = stmt(a);
        let s = Owner::Session(a);
        // a product built under the statement: its root, a child, a grandchild, and a child dropped before the adoption
        let product = MemoryContext::new("ledger2-product");
        product.set_owner(st);
        let p = product.mcx().allocate(chunk(64 * KIB)).expect("a dedicated chunk");
        let child = product.new_child("ledger2-product-child");
        let q = child.mcx().allocate(chunk(32 * KIB)).expect("a dedicated chunk");
        let grand = child.new_child("ledger2-product-grandchild");
        let r = grand.mcx().allocate(chunk(16 * KIB)).expect("a dedicated chunk");
        let gone = product.new_child("ledger2-product-gone");
        let _ = gone.mcx().allocate(chunk(8 * KIB)).expect("a dedicated chunk");
        drop(gone);
        let held = owner_ctx(st).0;
        assert!(held >= (64 + 32 + 16) * KIB as i64, "the building statement's: {held}");
        let s0 = owner_ctx(s).0;
        // a plain set_owner moves the root alone; the tree form moves what is left on the statement too
        product.set_owner(s);
        let root_only = owner_ctx(s).0 - s0;
        assert!(root_only > 0 && owner_ctx(st).0 == held - root_only, "set_owner leaves the children: {root_only}");
        product.set_owner(st);
        assert_eq!(owner_ctx(st).0, held, "back on the statement");
        product.set_owner_tree(s);
        assert_eq!(owner_ctx(st).0, 0, "nothing of the product left on the statement");
        assert_eq!(owner_ctx(s).0 - s0, held, "all of it on the holder, in one step");
        let later = grand.new_child("ledger2-product-later");
        let t = later.mcx().allocate(chunk(4 * KIB)).expect("a dedicated chunk");
        assert_eq!(owner_ctx(st).0, 0, "a child made after the adoption takes the holder");
        let adopted = owner_ctx(s).0 - s0;
        let _ = memledger::statement_end();
        assert_eq!(owner_ctx(s).0 - s0, adopted, "the statement's end moves nothing: the holder already has it");
        // SAFETY: each pointer is the chunk its context handed out for its layout
        unsafe {
            later.mcx().deallocate(t.cast(), chunk(4 * KIB));
            grand.mcx().deallocate(r.cast(), chunk(16 * KIB));
            child.mcx().deallocate(q.cast(), chunk(32 * KIB));
            product.mcx().deallocate(p.cast(), chunk(64 * KIB));
        }
        drop(later);
        drop(grand);
        drop(child);
        drop(product);
        assert_eq!(owner_ctx(s).0, s0, "the drops take them off the holder: never below where it was");
    });
}

/// The check on `set_owner_tree` (debug builds): a descendant another owner named (hung under a product by mistake)
/// stops the adoption instead of moving silently.
#[cfg(debug_assertions)]
#[test]
fn ledger2_set_owner_tree_stops_at_a_descendant_another_owner_named() {
    on(4067, |a| {
        memledger::statement_begin();
        let st = stmt(a);
        let product = MemoryContext::new("ledger2-product-foreign");
        product.set_owner(st);
        let foreign = product.new_child("ledger2-foreign-child");
        foreign.set_owner(Owner::Session(a));
        let holder = Owner::Cache(memledger::Holder::Of(memledger::Consumer::ProgramCaches));
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| product.set_owner_tree(holder)));
        assert!(r.is_err(), "a descendant the session named is not the statement's product");
        drop(foreign);
        drop(product);
        let _ = memledger::statement_end();
    });
}

/// D4464: a host's scratch root and its child took the building statement; the statement ended, and the root posted since
/// (following the end to the session) while the child did not. They are one owner's still: the tree moves whole, with no
/// debug stop, and the child's bytes leave where the end put them.
#[test]
fn ledger2_set_owner_tree_moves_a_child_that_has_not_followed_its_ended_owner() {
    on(4068, |a| {
        memledger::statement_begin();
        let st = stmt(a);
        let s = Owner::Session(a);
        let root = MemoryContext::new("ledger2-scratch");
        root.set_owner(st);
        let child = root.new_child("ledger2-scratch-child");
        let q = child.mcx().allocate(chunk(32 * KIB)).expect("a dedicated chunk");
        let s0 = owner_ctx(s).0;
        let _ = memledger::statement_end();
        let ended = owner_ctx(s).0 - s0;
        assert!(ended >= 32 * KIB as i64, "the statement's end moved its contexts' bytes to the session: {ended}");
        // the root posts after the end (it follows to the session); the child does not
        let p = root.mcx().allocate(chunk(64 * KIB)).expect("a dedicated chunk");
        let both = owner_ctx(s).0 - s0;
        assert!(both >= ended + 64 * KIB as i64, "the root's new block lands on the session: {both}");
        let depot = Owner::Cache(memledger::Holder::SlotDepot);
        let d0 = owner_ctx(depot).0;
        root.set_owner_tree(depot);
        assert_eq!(owner_ctx(depot).0 - d0, both, "the whole tree on the depot, the child's bytes too");
        assert_eq!(owner_ctx(s).0, s0, "none left on the session");
        // SAFETY: each pointer is the chunk its context handed out for its layout
        unsafe {
            child.mcx().deallocate(q.cast(), chunk(32 * KIB));
            root.mcx().deallocate(p.cast(), chunk(64 * KIB));
        }
        drop(child);
        drop(root);
        assert_eq!(owner_ctx(depot).0, d0, "the drops take them off the depot");
    });
}

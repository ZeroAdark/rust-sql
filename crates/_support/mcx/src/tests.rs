extern crate std;

use super::*;
use core::fmt::Write as _;

#[test]
fn alloc_size_gate_matches_palloc() {
    assert!(check_alloc_size(MAX_ALLOC_SIZE).is_ok());
    let err = check_alloc_size(MAX_ALLOC_SIZE + 1).unwrap_err();
    assert_eq!(
        err.message(),
        alloc::format!("invalid memory alloc request size {}", MAX_ALLOC_SIZE + 1)
    );

    let ctx = MemoryContext::new("t");
    let r: PgResult<PgVec<u64>> = vec_with_capacity_in(ctx.mcx(), (-1i32) as isize as usize);
    assert!(r
        .unwrap_err()
        .message()
        .starts_with("invalid memory alloc request size"));
}

#[test]
fn alloc_ceiling_unwinds_as_catchable_pg_error() {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    let ctx = MemoryContext::new("t");
    let payload = catch_unwind(AssertUnwindSafe(|| {
        let mut v: PgVec<u8> = PgVec::new_in(ctx.mcx());
        let _ = v.try_reserve_exact(MAX_ALLOC_SIZE + 1);
    }))
    .unwrap_err();
    let err = payload
        .downcast::<alloc::boxed::Box<PgError>>()
        .expect("ceiling payload must be a PgError, as C's elog(ERROR)");
    assert_eq!(
        err.message(),
        alloc::format!("invalid memory alloc request size {}", MAX_ALLOC_SIZE + 1)
    );
    assert_eq!(err.sqlstate(), ::types_error::ERRCODE_INTERNAL_ERROR);
}

#[test]
fn append_growth_clamps_to_max_alloc_size() {
    assert_eq!(grow_target(1024, 1025), 2048);
    assert_eq!(grow_target(1024, 5000), 8192);
    assert_eq!(grow_target(0, 5), 8);
    assert_eq!(grow_target(536_870_912, 536_936_448), MAX_ALLOC_SIZE);
    assert_eq!(grow_target(MAX_ALLOC_SIZE, MAX_ALLOC_SIZE + 1), MAX_ALLOC_SIZE + 1);

    let ctx = MemoryContext::new("t");
    let mut v: PgVec<u8> = PgVec::new_in(ctx.mcx());
    vec_append_bytes(&mut v, &[7u8; 1025]).unwrap();
    assert_eq!(v.len(), 1025);
    assert_eq!(v.capacity(), 2048);
    vec_append_bytes(&mut v, &[8u8; 3000]).unwrap();
    assert_eq!(v.len(), 4025);
    assert_eq!(v.capacity(), 4096);
}

#[test]
fn accounting_tracks_capacity_exactly() {
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    let mut v: PgVec<u64> = PgVec::new_in(mcx);
    assert_eq!(ctx.used(), 0);
    for i in 0..100u64 {
        v.push(i);
        assert_eq!(ctx.used(), v.capacity() * 8, "after push {}", i);
    }
    v.shrink_to_fit();
    assert_eq!(ctx.used(), v.capacity() * 8);
    assert_eq!(v.capacity(), 100);
    drop(v);
    assert_eq!(ctx.used(), 0, "drop returns every byte");
    assert!(ctx.peak() >= 800);
}

#[test]
fn accounting_multiple_collections_compose() {
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    let a = vec_with_capacity_in::<u8>(mcx, 64).unwrap();
    let b = vec_with_capacity_in::<u8>(mcx, 128).unwrap();
    let mut m: PgHashMap<u32, u32> = PgHashMap::new_in(mcx);
    m.insert(1, 2);
    assert!(ctx.used() >= 192 + core::mem::size_of::<(u32, u32)>());
    drop(a);
    drop(b);
    drop(m);
    assert_eq!(ctx.used(), 0);
}

#[test]
fn limit_enforced_via_try_reserve() {
    let ctx = MemoryContext::new("limited").with_limit(1024);
    let mcx = ctx.mcx();
    let mut v: PgVec<u8> = PgVec::new_in(mcx);
    v.try_reserve_exact(1024).expect("exactly at limit is fine");
    assert_eq!(ctx.used(), 1024);
    let mut w: PgVec<u8> = PgVec::new_in(mcx);
    let err = w.try_reserve_exact(1);
    assert!(err.is_err(), "limit must reject the 1025th byte");
    assert_eq!(ctx.used(), 1024, "failed reservation charged nothing");
}

#[test]
fn oom_error_shape_matches_mcxt_c() {
    let ctx = MemoryContext::new("ExprContext").with_limit(8);
    let mcx = ctx.mcx();
    let mut v: PgVec<u8> = PgVec::new_in(mcx);
    let e = match v.try_reserve_exact(64) {
        Err(_) => mcx.oom(64),
        Ok(()) => panic!("limit not enforced"),
    };
    assert_eq!(e.sqlstate, ERRCODE_OUT_OF_MEMORY);
    assert_eq!(e.message, "out of memory");
    assert_eq!(
        e.detail.as_deref(),
        Some("Failed on request of size 64 in memory context \"ExprContext\".")
    );
}

#[test]
fn bump_context_reset_reclaims_and_reuses() {
    let mut ctx = MemoryContext::new_bump("per-tuple");
    {
        let mcx = ctx.mcx();
        let mut v: PgVec<u32> = PgVec::new_in(mcx);
        for i in 0..1000 {
            v.push(i);
        }
        assert!(ctx.used() > 0);
    } // v drops; bump.c model — dealloc is a full no-op, the charge stays
    assert!(ctx.used() > 0, "bump free is unaccounted; reset releases the charge");
    let footprint_before = ctx.stats().arena_footprint;
    assert!(footprint_before > 0, "arena retains memory after drops");
    ctx.reset();
    assert_eq!(
        ctx.used(),
        ctx.stats().arena_footprint,
        "reset releases everything but the retained keeper (which stays charged)"
    );
    {
        let mcx = ctx.mcx();
        let mut v: PgVec<u32> = PgVec::new_in(mcx);
        for i in 0..1000 {
            v.push(i);
        }
        drop(v);
    }
    assert_eq!(ctx.peak(), ctx.stats().peak);
}

#[test]
fn alloc_bytes_and_the_keeper_rewind() {
    let mut ctx = MemoryContext::new_bump("per-tuple");
    let first = ctx.mcx().alloc_bytes(12).unwrap().as_ptr() as usize;
    assert_eq!(first % 8, 0);
    let keeper = ctx.stats().arena_footprint;
    for _ in 0..1000 {
        ctx.reset();
        assert_eq!(ctx.mcx().alloc_bytes(12).unwrap().as_ptr() as usize, first, "the row reuses the keeper's first bytes");
        assert_eq!(ctx.stats().arena_footprint, keeper);
        assert_eq!(ctx.used(), keeper);
    }
    for _ in 0..4096 {
        ctx.mcx().alloc_bytes(100).unwrap();
    }
    assert!(ctx.stats().arena_footprint > keeper);
    ctx.reset();
    assert_eq!(ctx.stats().arena_footprint, keeper, "the full reset frees the extra blocks");
    assert_eq!(ctx.used(), keeper);
    let big = ctx.mcx().alloc_bytes(64 << 20).unwrap().as_ptr() as usize;
    assert_eq!(big % 8, 0);
    ctx.reset();
    assert_eq!(ctx.stats().arena_footprint, keeper, "an oversize chunk goes at the next reset");
}

// aset.c:537-597 AllocSetReset releases every chunk of the set whether or not
// it was pfree'd first and leaves context->mem_allocated at the keeper
// baseline: after a reset nothing is charged. A charge still outstanding at
// reset is the release-build shape of a leaked owner (a PgVec whose Drop
// never ran; the leak-detector debug_assert in reset_noncore is skipped only
// under the session-root wholesale teardown, which is also the one way a
// dev-profile test reaches the release arm). The arena is released either
// way; the accounting must follow it, or used()/subtree_used() keep the
// stale bytes for the life of the context and grow by them on every cycle.
#[test]
fn aset_reset_returns_charged_bytes_to_zero() {
    let mut ctx = MemoryContext::new("t");
    let v: PgVec<u8> = vec_with_capacity_in(ctx.mcx(), 4096).unwrap();
    assert_eq!(ctx.used(), 4096);
    core::mem::forget(v);
    SESSION_ROOT_RETIRING.with(|r| r.set(true));
    ctx.reset();
    SESSION_ROOT_RETIRING.with(|r| r.set(false));
    assert_eq!(ctx.used(), 0, "AllocSetReset leaves nothing charged");
    assert_eq!(ctx.subtree_used(), 0);
    assert_eq!(ctx.peak(), 0);
    // The context accounts from zero again afterwards.
    let w: PgVec<u8> = vec_with_capacity_in(ctx.mcx(), 64).unwrap();
    assert_eq!(ctx.used(), 64);
    drop(w);
    assert_eq!(ctx.used(), 0);
}

#[test]
fn reset_callbacks_fire_lifo_on_reset_and_drop() {
    use core::cell::RefCell;
    use alloc::rc::Rc;
    let order: Rc<RefCell<alloc::vec::Vec<u8>>> = Rc::default();

    let mut ctx = MemoryContext::new("cb");
    let (o1, o2) = (order.clone(), order.clone());
    ctx.register_reset_callback(move || o1.borrow_mut().push(1));
    ctx.register_reset_callback(move || o2.borrow_mut().push(2));
    ctx.reset();
    assert_eq!(&*order.borrow(), &[2, 1], "LIFO like PG");

    let o3 = order.clone();
    ctx.register_reset_callback(move || o3.borrow_mut().push(3));
    drop(ctx);
    assert_eq!(&*order.borrow(), &[2, 1, 3], "delete fires callbacks too");
}

#[test]
fn pg_string_basics() {
    let ctx = MemoryContext::new("s");
    let mcx = ctx.mcx();
    let mut s = PgString::from_str_in("héllo", mcx).unwrap();
    s.try_push(' ').unwrap();
    s.try_push_str("wörld").unwrap();
    assert_eq!(s, "héllo wörld");
    assert_eq!(ctx.used(), s.capacity_bytes());
    write!(s, " {}", 42).unwrap();
    assert_eq!(s.as_str(), "héllo wörld 42");
    drop(s);
    assert_eq!(ctx.used(), 0);
}

#[test]
fn nested_scopes_thread_explicitly() {
    fn build_row<'mcx>(mcx: Mcx<'mcx>, n: u32) -> PgResult<PgVec<'mcx, u32>> {
        let mut row = vec_with_capacity_in(mcx, n as usize)?;
        row.extend(0..n);
        Ok(row)
    }
    let per_query = MemoryContext::new("per-query");
    let rows = build_row(per_query.mcx(), 16).unwrap();
    assert_eq!(rows.len(), 16);
    assert_eq!(per_query.used(), 64);
}

#[test]
fn child_charges_propagate_to_ancestors() {
    let root = MemoryContext::new("root");
    let query = root.new_child("per-query");
    let tuple = query.new_child("per-tuple");

    let v = vec_with_capacity_in::<u8>(tuple.mcx(), 100).unwrap();
    assert_eq!(tuple.used(), 100);
    assert_eq!(query.used(), 0, "parent's own bytes unaffected");
    assert_eq!(query.subtree_used(), 100);
    assert_eq!(root.subtree_used(), 100);

    let w = vec_with_capacity_in::<u8>(query.mcx(), 50).unwrap();
    assert_eq!(query.subtree_used(), 150);
    assert_eq!(root.subtree_used(), 150);

    drop(v);
    drop(w);
    assert_eq!(root.subtree_used(), 0);
    assert_eq!(root.subtree_peak(), 150);
}

// Retention-cap law (execmain skeleton cap, portalmem context pools):
// retention caps measure the SUBTREE — children of a parked skeleton are
// retained memory. A parent whose own accounting reads empty still retains
// every byte charged to a live child, so self_used alone under-reads exactly
// the allocations that accumulate across parked executions.
#[test]
fn retention_measures_subtree_not_self() {
    let query = MemoryContext::new("es_query_cxt");
    let expr = query.new_child("ExprContext");

    let v = vec_with_capacity_in::<u8>(expr.mcx(), 4096).unwrap();
    assert_eq!(query.used(), 0, "self accounting blind to the child");
    assert_eq!(query.subtree_used(), 4096, "the cap's measure sees it");

    // Dead children fall out of the subtree: the cap never counts memory
    // that was actually released.
    drop(v);
    drop(expr);
    assert_eq!(query.subtree_used(), 0);
}

#[test]
fn ancestor_limit_caps_descendants() {
    let root = MemoryContext::new("hash-agg").with_limit(1000);
    let child = root.new_child("batch");

    let _a = vec_with_capacity_in::<u8>(root.mcx(), 600).unwrap();
    let mut v: PgVec<u8> = PgVec::new_in(child.mcx());
    assert!(v.try_reserve_exact(500).is_err(), "600+500 exceeds ancestor limit");
    assert_eq!(root.subtree_used(), 600, "failed charge applied nothing");
    assert_eq!(child.subtree_used(), 0);
    v.try_reserve_exact(400).expect("exactly at the ancestor limit");
    assert_eq!(root.subtree_used(), 1000);
}

#[test]
fn stats_tree_reflects_hierarchy_and_prunes_dropped() {
    let root = MemoryContext::new("root");
    let a = root.new_child("a");
    let _hold = vec_with_capacity_in::<u8>(a.mcx(), 64).unwrap();
    {
        let b = root.new_child("b");
        let t = root.stats_tree();
        assert_eq!(t.children.len(), 2);
        drop(b);
    }
    let t = root.stats_tree();
    assert_eq!(t.name, "root");
    assert_eq!(t.children.len(), 1, "dropped child pruned");
    assert_eq!(t.children[0].name, "a");
    assert_eq!(t.children[0].used, 64);
    assert_eq!(t.subtree_used, 64);
}

// mcxt.c:1134-1137 MemoryContextCreate links the new node at the head of the
// parent's firstchild list, so sibling walks (MemoryContextStats lines,
// pg_get_backend_memory_contexts ids and paths) go newest-first.
#[test]
fn stats_tree_lists_siblings_newest_first() {
    let root = MemoryContext::new("root");
    let _a = root.new_child("a");
    let _b = root.new_child("b");
    let _c = root.new_child("c");
    let names: alloc::vec::Vec<&str> = root.stats_tree().children.iter().map(|c| c.name).collect();
    assert_eq!(names, ["c", "b", "a"]);
}

// mcxt.c:908 MemoryContextStatsInternal stops recursing when the stack runs
// low; a chain one context per nested SAVEPOINT must snapshot and drop on a
// stack far smaller than its depth.
#[test]
fn stats_tree_of_a_deep_chain_needs_no_stack() {
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            const DEPTH: usize = 50_000;
            let root = MemoryContext::new("root");
            let mut chain = alloc::vec::Vec::with_capacity(DEPTH);
            let mut cur = root.new_child("CurTransactionContext");
            for _ in 1..DEPTH {
                let next = cur.new_child("CurTransactionContext");
                chain.push(cur);
                cur = next;
            }
            chain.push(cur);
            let _hold = vec_with_capacity_in::<u8>(chain.last().unwrap().mcx(), 16).unwrap();
            let t = root.stats_tree();
            assert_eq!(t.subtree_used, 16);
            let mut depth = 0;
            let mut n = &t;
            while let Some(c) = n.children.first() {
                depth += 1;
                n = c;
            }
            assert_eq!(depth, DEPTH);
            drop(t);
            drop(_hold);
            while chain.pop().is_some() {}
        })
        .unwrap()
        .join()
        .unwrap();
}

// mcxt.c:345 MemoryContextInit's tree dies with the backend; a retired
// session root keeps its (poisoned) shell but no block, keeper included.
#[test]
fn session_root_retirement_releases_the_keeper() {
    for mut ctx in [
        MemoryContext::new("aset"),
        MemoryContext::new_bump("bump"),
        MemoryContext::new_bumpforget("bumpforget"),
        MemoryContext::new_generation("generation"),
    ] {
        let v: PgVec<u8> = vec_with_capacity_in(ctx.mcx(), 64).unwrap();
        drop(v);
        assert!(ctx.stats().arena_footprint > 0, "{}: keeper retained over a reset", ctx.name());
        ctx.retire_session_root();
        assert_eq!(ctx.stats().arena_footprint, 0, "{}", ctx.name());
        assert_eq!(ctx.stats().nblocks, 0, "{}", ctx.name());
        assert_eq!(ctx.stats().used, 0, "{}", ctx.name());
    }
}

#[test]
fn child_may_outlive_parent_accounting_safely() {
    let child;
    {
        let root = MemoryContext::new("root");
        child = root.new_child("survivor");
    } // root dropped; its Acct node stays alive via the child's parent Rc
    let v = vec_with_capacity_in::<u8>(child.mcx(), 32).unwrap();
    assert_eq!(child.used(), 32);
    drop(v);
    assert_eq!(child.used(), 0);
}

#[test]
fn child_churn_does_not_grow_parent_child_list() {
    let root = MemoryContext::new("root");
    for _ in 0..10_000 {
        let child = root.new_child("per-tuple");
        let _v = vec_with_capacity_in::<u8>(child.mcx(), 16).unwrap();
    }
    let t = root.stats_tree();
    assert_eq!(t.children.len(), 0);
    assert_eq!(root.subtree_used(), 0);
}

#[test]
fn pg_string_round_trips_and_keys() {
    let ctx = MemoryContext::new("s");
    let other = MemoryContext::new("o");
    let mcx = ctx.mcx();

    let s = PgString::from_str_in("key", mcx).unwrap();
    let s2 = s.clone_in(other.mcx()).unwrap();
    assert_eq!(s, s2);
    assert_eq!(other.used(), s2.capacity_bytes());

    let mut m: PgHashMap<PgString, u32> = PgHashMap::new_in(mcx);
    m.insert(s, 7);
    assert_eq!(m.get("key"), Some(&7));

    let raw = slice_in(mcx, b"caf\xc3\xa9".as_slice()).unwrap();
    assert_eq!(PgString::from_utf8(raw).unwrap(), "café");
    let bad = slice_in(mcx, b"\xff\xfe".as_slice()).unwrap();
    assert!(PgString::from_utf8(bad).is_err());
}

#[test]
fn chomp_strips_only_trailing_newlines() {
    let ctx = MemoryContext::new("s");
    let mcx = ctx.mcx();
    assert_eq!(PgString::chomp_in("warn: x\n\n", mcx).unwrap(), "warn: x");
    assert_eq!(PgString::chomp_in("a\nb\n", mcx).unwrap(), "a\nb");
    assert_eq!(PgString::chomp_in("no newline", mcx).unwrap(), "no newline");
    assert_eq!(PgString::chomp_in("\n", mcx).unwrap(), "");
}

#[test]
fn ident_set_forget_and_stats() {
    let root = MemoryContext::new("CachedPlanSource");
    assert_eq!(root.ident(), None);
    root.set_ident(Some("SELECT 1"));
    assert_eq!(root.ident().as_deref(), Some("SELECT 1"));
    assert_eq!(root.stats().ident.as_deref(), Some("SELECT 1"));

    let child = root.new_child("CachedPlanQuery");
    child.set_ident(Some("q"));
    let t = root.stats_tree();
    assert_eq!(t.ident.as_deref(), Some("SELECT 1"));
    assert_eq!(t.children[0].ident.as_deref(), Some("q"));

    root.set_ident(None);
    assert_eq!(root.ident(), None, "NULL forgets the old identifier");
}

// Counters must be byte-identical to a naive reference model for any sequence.
#[test]
fn hotpath_invariance_counters_byte_identical() {
    struct Ref {
        used: alloc::vec::Vec<usize>,
        self_peak: alloc::vec::Vec<usize>,
        subtree_peak: alloc::vec::Vec<usize>,
        parent: alloc::vec::Vec<usize>,
    }
    impl Ref {
        fn subtree(&self, i: usize) -> usize {
            let mut total = self.used[i];
            for j in 0..self.used.len() {
                if j != i {
                    let mut p = self.parent[j];
                    while p != usize::MAX {
                        if p == i {
                            total += self.used[j];
                            break;
                        }
                        p = self.parent[p];
                    }
                }
            }
            total
        }
        fn charge(&mut self, i: usize, n: usize) {
            self.used[i] += n;
            if self.used[i] > self.self_peak[i] {
                self.self_peak[i] = self.used[i];
            }
            let mut k = i;
            loop {
                let st = self.subtree(k);
                if st > self.subtree_peak[k] {
                    self.subtree_peak[k] = st;
                }
                if self.parent[k] == usize::MAX {
                    break;
                }
                k = self.parent[k];
            }
        }
        fn uncharge(&mut self, i: usize, n: usize) {
            self.used[i] -= n;
        }
    }

    let root = MemoryContext::new("root");
    let a = root.new_child("a");
    let a1 = a.new_child("a1");
    let b = root.new_child("b");
    let ctxs = [&root, &a, &a1, &b];
    let mut refm = Ref {
        used: alloc::vec![0; 4],
        self_peak: alloc::vec![0; 4],
        subtree_peak: alloc::vec![0; 4],
        parent: alloc::vec![usize::MAX, 0, 1, 0],
    };

    let mut vecs: [PgVec<u8>; 4] = [
        PgVec::new_in(root.mcx()),
        PgVec::new_in(a.mcx()),
        PgVec::new_in(a1.mcx()),
        PgVec::new_in(b.mcx()),
    ];

    let check = |ctxs: &[&MemoryContext; 4], refm: &Ref| {
        for i in 0..4 {
            assert_eq!(ctxs[i].used(), refm.used[i], "used[{i}]");
            assert_eq!(ctxs[i].subtree_used(), refm.subtree(i), "subtree_used[{i}]");
            assert_eq!(ctxs[i].peak(), refm.self_peak[i], "peak[{i}]");
            assert_eq!(
                ctxs[i].subtree_peak(),
                refm.subtree_peak[i],
                "subtree_peak[{i}]"
            );
        }
    };

    let do_reserve = |vecs: &mut [PgVec<u8>; 4], refm: &mut Ref, i: usize, total: usize| {
        let before = vecs[i].capacity();
        vecs[i].reserve_exact(total - vecs[i].len());
        let after = vecs[i].capacity();
        refm.charge(i, after - before);
    };

    do_reserve(&mut vecs, &mut refm, 2, 100);
    check(&ctxs, &refm);
    do_reserve(&mut vecs, &mut refm, 1, 40);
    check(&ctxs, &refm);
    do_reserve(&mut vecs, &mut refm, 0, 7);
    check(&ctxs, &refm);
    do_reserve(&mut vecs, &mut refm, 3, 256);
    check(&ctxs, &refm);
    do_reserve(&mut vecs, &mut refm, 2, 500);
    check(&ctxs, &refm);

    let cap = vecs[2].capacity();
    vecs[2] = PgVec::new_in(a1.mcx());
    refm.uncharge(2, cap);
    check(&ctxs, &refm);

    assert!(root.subtree_peak() >= 7 + 40 + 500 + 256, "peaks persist after frees");

    for (i, v) in vecs.iter_mut().enumerate() {
        let cap = v.capacity();
        *v = PgVec::new_in(ctxs[i].mcx());
        refm.uncharge(i, cap);
    }
    check(&ctxs, &refm);
    assert_eq!(root.subtree_used(), 0);
}

#[test]
fn hotpath_limit_flag_lifecycle_and_enforcement() {
    let root = MemoryContext::new("root");
    let child = root.new_child("child");
    let mut v: PgVec<u8> = PgVec::new_in(child.mcx());
    v.try_reserve_exact(1 << 20).expect("unlimited, skip path");
    assert_eq!(root.subtree_used(), 1 << 20);
    drop(v);

    {
        let limited = MemoryContext::new("limited").with_limit(256);
        let mut w: PgVec<u8> = PgVec::new_in(limited.mcx());
        assert!(w.try_reserve_exact(257).is_err(), "over limit rejected");
        assert_eq!(limited.used(), 0, "failed charge applied nothing");
        w.try_reserve_exact(256).expect("at limit ok");
        assert_eq!(limited.used(), 256);

        let cap_root = MemoryContext::new("cap").with_limit(100);
        let kid = cap_root.new_child("kid");
        let mut k: PgVec<u8> = PgVec::new_in(kid.mcx());
        assert!(k.try_reserve_exact(101).is_err(), "ancestor limit caps kid");
        k.try_reserve_exact(100).expect("at ancestor limit ok");
        assert_eq!(cap_root.subtree_used(), 100);
    }

    let root2 = MemoryContext::new("root2");
    let mut x: PgVec<u8> = PgVec::new_in(root2.mcx());
    x.try_reserve_exact(1 << 20).expect("skip path restored after limits drop");
    assert_eq!(root2.used(), 1 << 20);
}

// BumpDrop: destructors run exactly once at reset/drop.
mod bumpdrop {
    use super::*;
    use alloc::rc::Rc;
    use core::cell::Cell;

    struct DropCounter {
        drops: Rc<Cell<u32>>,
    }
    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }

    #[test]
    fn destructor_runs_exactly_once_at_reset_not_at_alloc() {
        let drops = Rc::new(Cell::new(0u32));
        let mut ctx = MemoryContext::new_bumpdrop("arena");
        {
            let mcx = ctx.mcx();
            for _ in 0..5 {
                let _r: &mut DropCounter =
                    arena_box_in(mcx, DropCounter { drops: drops.clone() }).unwrap();
            }
            assert_eq!(drops.get(), 0, "no per-object Drop ran at allocation");
            assert!(ctx.used() > 0, "values are charged while live");
        }
        assert_eq!(drops.get(), 0, "leaked values do NOT drop when borrows end");

        ctx.reset();
        assert_eq!(drops.get(), 5, "all destructors run exactly once at reset");
        assert_eq!(ctx.used(), ctx.stats().arena_footprint, "only the keeper stays charged");

        ctx.reset();
        assert_eq!(drops.get(), 5, "no destructor runs twice");
    }

    #[test]
    fn destructor_runs_on_drop_when_context_dropped() {
        let drops = Rc::new(Cell::new(0u32));
        {
            let ctx = MemoryContext::new_bumpdrop("arena");
            let mcx = ctx.mcx();
            for _ in 0..3 {
                let _r = arena_box_in(mcx, DropCounter { drops: drops.clone() }).unwrap();
            }
            assert_eq!(drops.get(), 0);
        }
        assert_eq!(drops.get(), 3, "context drop runs the drop list");
    }

    #[test]
    fn destructors_run_lifo() {
        let order: Rc<RefCell<alloc::vec::Vec<u8>>> = Rc::default();
        struct OrderRec {
            id: u8,
            order: Rc<RefCell<alloc::vec::Vec<u8>>>,
        }
        impl Drop for OrderRec {
            fn drop(&mut self) {
                self.order.borrow_mut().push(self.id);
            }
        }
        let mut ctx = MemoryContext::new_bumpdrop("arena");
        {
            let mcx = ctx.mcx();
            for id in 1..=4u8 {
                let _r = arena_box_in(mcx, OrderRec { id, order: order.clone() }).unwrap();
            }
        }
        ctx.reset();
        assert_eq!(&*order.borrow(), &[4, 3, 2, 1], "drop list runs LIFO like C");
    }

    #[test]
    fn arena_vec_of_non_pod_reclaimed_at_reset() {
        let drops = Rc::new(Cell::new(0u32));
        let mut ctx = MemoryContext::new_bumpdrop("arena");
        {
            let mcx = ctx.mcx();
            let mut v: PgVec<DropCounter> = PgVec::new_in(mcx);
            for _ in 0..10 {
                v.push(DropCounter { drops: drops.clone() });
            }
            let _leaked: &mut PgVec<DropCounter> = arena_vec_in(mcx, v).unwrap();
            assert_eq!(drops.get(), 0, "elements not dropped while vec is live in arena");
        }
        assert_eq!(drops.get(), 0, "vec leaked, elements still live");
        ctx.reset();
        assert_eq!(drops.get(), 10, "all 10 elements dropped once at reset");
        assert_eq!(ctx.used(), ctx.stats().arena_footprint);
    }

    #[test]
    fn arena_string_reclaimed_at_reset() {
        let mut ctx = MemoryContext::new_bumpdrop("arena");
        {
            let mcx = ctx.mcx();
            let s = PgString::from_str_in("hello arena", mcx).unwrap();
            let leaked: &mut PgString = arena_string_in(mcx, s).unwrap();
            assert_eq!(leaked.as_str(), "hello arena");
            assert!(ctx.used() > 0);
        }
        ctx.reset();
        assert_eq!(ctx.used(), ctx.stats().arena_footprint, "string buffer reclaimed at reset");
    }

    #[test]
    fn used_and_subtree_used_invariant_across_alloc_reset() {
        let root = MemoryContext::new("root");
        let mut arena = root.new_child_bumpdrop("arena");
        let drops = Rc::new(Cell::new(0u32));
        for round in 0..3 {
            {
                let mcx = arena.mcx();
                for _ in 0..20 {
                    let _r = arena_box_in(mcx, DropCounter { drops: drops.clone() }).unwrap();
                }
                assert!(arena.used() > 0, "round {round}: charged while live");
                assert_eq!(
                    root.subtree_used(),
                    arena.used(),
                    "round {round}: ancestor subtree mirrors child"
                );
            }
            arena.reset();
            let keeper = arena.stats().arena_footprint;
            assert_eq!(arena.used(), keeper, "round {round}: only the keeper stays charged");
            assert_eq!(
                root.subtree_used(),
                keeper,
                "round {round}: reset propagates to ancestor subtree_used"
            );
            assert_eq!(drops.get(), 20 * (round + 1), "round {round}: 20 more drops");
        }
    }

    #[test]
    fn nested_arenas_reclaim_independently() {
        let drops_outer = Rc::new(Cell::new(0u32));
        let drops_inner = Rc::new(Cell::new(0u32));
        {
            let outer = MemoryContext::new_bumpdrop("outer");
            let _o = arena_box_in(outer.mcx(), DropCounter { drops: drops_outer.clone() }).unwrap();
            let mut inner = outer.new_child_bumpdrop("inner");
            {
                let _i =
                    arena_box_in(inner.mcx(), DropCounter { drops: drops_inner.clone() }).unwrap();
                assert_eq!(outer.subtree_used(), outer.used() + inner.used());
            }
            inner.reset();
            assert_eq!(drops_inner.get(), 1, "inner reset drops inner only");
            assert_eq!(drops_outer.get(), 0, "outer untouched by inner reset");
        }
        assert_eq!(drops_outer.get(), 1, "outer drop runs outer's list once");
        assert_eq!(drops_inner.get(), 1, "inner already drained; no double drop");
    }

    #[test]
    fn panic_in_drop_glue_does_not_double_run_remaining() {
        use std::panic::{catch_unwind, AssertUnwindSafe};

        let drops = Rc::new(Cell::new(0u32));
        struct PanicOnDrop {
            boom: bool,
            drops: Rc<Cell<u32>>,
        }
        impl Drop for PanicOnDrop {
            fn drop(&mut self) {
                self.drops.set(self.drops.get() + 1);
                if self.boom {
                    panic!("destructor panic");
                }
            }
        }

        let mut ctx = MemoryContext::new_bumpdrop("arena");
        {
            let mcx = ctx.mcx();
            // Registered A(no), B(BOOM), C(no); LIFO drop order C, B, A.
            let _a = arena_box_in(mcx, PanicOnDrop { boom: false, drops: drops.clone() }).unwrap();
            let _b = arena_box_in(mcx, PanicOnDrop { boom: true, drops: drops.clone() }).unwrap();
            let _c = arena_box_in(mcx, PanicOnDrop { boom: false, drops: drops.clone() }).unwrap();
        }
        let n_before = drops.get();
        assert_eq!(n_before, 0);
        let res = catch_unwind(AssertUnwindSafe(|| ctx.reset()));
        assert!(res.is_err(), "the panic propagates out of reset");
        assert_eq!(drops.get(), 2, "popped-before-run: C+B ran, no double drop");

        let res2 = catch_unwind(AssertUnwindSafe(|| ctx.reset()));
        assert!(res2.is_ok(), "second reset is clean");
        assert_eq!(drops.get(), 3, "A drained on the 2nd reset; B/C never re-run");
    }

    #[test]
    fn pod_value_in_arena_needs_no_drop_entry() {
        let mut ctx = MemoryContext::new_bumpdrop("arena");
        {
            let mcx = ctx.mcx();
            let r: &mut u64 = arena_box_in(mcx, 42u64).unwrap();
            assert_eq!(*r, 42);
        }
        ctx.reset();
        assert_eq!(ctx.used(), ctx.stats().arena_footprint);
    }

    // The hash-join batchCxt pattern: per-batch drop + wholesale reset + reuse.
    #[test]
    fn child_bump_owned_vec_wholesale_reset_and_reuse() {
        let drops = Rc::new(Cell::new(0u32));
        let root = MemoryContext::new("query");
        let mut batch = root.new_child_bump("HashBatchContext");

        let total_batches = 6usize;
        let per_batch = 50usize;
        for b in 0..total_batches {
            let mut v: PgVec<DropCounter> = {
                let mcx = batch.mcx();
                let mut v = PgVec::new_in(mcx);
                for _ in 0..per_batch {
                    v.push(DropCounter { drops: drops.clone() });
                }
                v
            };
            assert_eq!(drops.get() as usize, b * per_batch, "no drops mid-batch");
            assert!(batch.used() > 0, "arena charged while the batch is live");

            v.clear();
            drop(v);
            assert_eq!(
                drops.get() as usize,
                (b + 1) * per_batch,
                "every element dropped exactly once at the batch boundary"
            );
            batch.reset();
            assert_eq!(
                batch.used(),
                batch.stats().arena_footprint,
                "wholesale reset releases everything but the keeper"
            );
        }
        assert_eq!(drops.get() as usize, total_batches * per_batch);

        drop(batch);
        assert_eq!(drops.get() as usize, total_batches * per_batch);
        assert_eq!(root.subtree_used(), 0, "child charge fully released to parent");
    }

    #[test]
    fn child_bump_rebatch_move_preserves_elements() {
        let drops = Rc::new(Cell::new(0u32));
        let mut batch = MemoryContext::new_bump("HashBatchContext");
        {
            let mcx = batch.mcx();
            let mut old: PgVec<DropCounter> = PgVec::new_in(mcx);
            for _ in 0..8 {
                old.push(DropCounter { drops: drops.clone() });
            }
            let mut newv: PgVec<DropCounter> = PgVec::new_in(mcx);
            for e in old.into_iter() {
                newv.push(e);
            }
            assert_eq!(drops.get(), 0, "moved tuples are not dropped during rebatch");
            newv.clear();
            drop(newv);
            assert_eq!(drops.get(), 8, "elements drop once when the new arena drops");
        }
        batch.reset();
        assert_eq!(batch.used(), batch.stats().arena_footprint);
    }

    // OLD (Aset, per-tuple frees) vs NEW (bump, wholesale reset) churn counts.
    #[test]
    fn churn_measurement_per_tuple_free_vs_wholesale_reset() {
        use crate::{alloc_in, PgVec, PgBox};

        let tuples_per_batch = 500usize;
        let nbatch = 16usize;

        fn build_batch<'m>(
            mcx: Mcx<'m>,
            n: usize,
        ) -> alloc::vec::Vec<(PgBox<'m, [u8; 24]>, PgVec<'m, u8>)> {
            let mut v = alloc::vec::Vec::with_capacity(n);
            for _ in 0..n {
                let b = alloc_in(mcx, [0u8; 24]).unwrap();
                let mut data = PgVec::new_in(mcx);
                data.extend_from_slice(&[7u8; 40]);
                v.push((b, data));
            }
            v
        }

        let _ = crate::churn_probe::take();
        {
            let qcx = MemoryContext::new("query-old");
            for _ in 0..nbatch {
                let batch = build_batch(qcx.mcx(), tuples_per_batch);
                drop(batch);
                assert_eq!(qcx.used(), 0);
            }
        }
        let old_real_frees = crate::churn_probe::take();

        let mut wholesale_resets = 0u64;
        {
            let qcx = MemoryContext::new("query-new");
            let mut batch_cxt = qcx.new_child_bump("HashBatchContext");
            for _ in 0..nbatch {
                {
                    let batch = build_batch(batch_cxt.mcx(), tuples_per_batch);
                    assert!(batch_cxt.used() > 0, "batch tuples charged while live");
                    drop(batch);
                }
                batch_cxt.reset();
                wholesale_resets += 1;
                assert_eq!(
                    batch_cxt.used(),
                    batch_cxt.stats().arena_footprint,
                    "wholesale reset releases everything but the keeper"
                );
            }
        }
        let new_real_frees = crate::churn_probe::take();

        std::eprintln!(
            "\n==== HASH-JOIN batchCxt CHURN ({} tuples/batch x {} batches) ====\n\
             OLD (per-query Aset + Vec::clear): real per-chunk frees = {}\n\
             NEW (bump batchCxt + wholesale reset): real per-chunk frees = {} \
             (reclaimed by {} wholesale resets)\n\
             ELIMINATED per-tuple free operations: {}\n\
             ===============================================================",
            tuples_per_batch, nbatch,
            old_real_frees, new_real_frees, wholesale_resets,
            old_real_frees - new_real_frees,
        );

        // Pollution-tolerant: parallel tests also free on Asets.
        let expected_old = (tuples_per_batch * 2 * nbatch) as u64;
        assert!(
            old_real_frees >= expected_old,
            "old model frees every box+vec individually every batch (>= {}, got {})",
            expected_old, old_real_frees,
        );
        // Window-relative, not absolute: the probe counter is global, and the
        // parallel suite's own frees land in both measurement windows — a
        // fixed expected_old/4 bound sat inside that pollution band (CI cluster
        // 16-thread runs measured 3.9-4.2k ambient frees vs the 4.0k line).
        assert!(
            new_real_frees < old_real_frees / 3,
            "new model eliminates the bulk of per-tuple frees (got {}, old {})",
            new_real_frees, old_real_frees,
        );
        assert_eq!(wholesale_resets, nbatch as u64);
    }
}

mod owned {
    use crate::*;

    struct Plan<'mcx> {
        nodes: PgVec<'mcx, u64>,
    }
    crate::bind!(PlanTy => Plan<'mcx>);

    fn build_plan(root: &MemoryContext, n: u64) -> PgResult<McxOwned<PlanTy>> {
        McxOwned::try_new(root.new_child("cached-plan"), |mcx| {
            let mut nodes = vec_with_capacity_in(mcx, n as usize)?;
            nodes.extend(0..n);
            Ok(Plan { nodes })
        })
    }

    #[test]
    fn bundle_moves_and_outlives_its_builder_scope() {
        let cache_root = MemoryContext::new("CacheMemoryContext");
        let mut cache: alloc::vec::Vec<McxOwned<PlanTy>> = alloc::vec::Vec::new();
        {
            let plan = build_plan(&cache_root, 100).unwrap();
            assert_eq!(plan.with(|p| p.nodes.len()), 100);
            cache.push(plan);
        }
        let plan = &mut cache[0];
        assert_eq!(plan.with(|p| p.nodes.iter().sum::<u64>()), 4950);
        assert!(cache_root.subtree_used() >= 800, "bundle bytes visible from the cache root");

        let before = plan.context().used();
        plan.with_mut(|p| {
            for i in 0..1000 {
                p.nodes.push(i);
            }
        });
        assert!(plan.context().used() > before);

        drop(cache);
        assert_eq!(cache_root.subtree_used(), 0, "dropping the bundle returns every byte");
    }

    struct Tree<'mcx> {
        plan_tree: Option<PgBox<'mcx, u64>>,
    }
    crate::bind!(TreeTy => Tree<'mcx>);

    #[test]
    fn leak_projection_yields_honest_borrow_reclaimed_by_context_drop() {
        let root = MemoryContext::new("root");
        let mut bundle = McxOwned::<TreeTy>::try_new(root.new_child("ExecutorState"), |mcx| {
            Ok(Tree { plan_tree: Some(alloc_in(mcx, 42u64)?) })
        })
        .unwrap();

        let seen = bundle.with_mut(|t| {
            let leaked: Option<&u64> = t.plan_tree.take().map(|b| &*crate::leak_in(b));
            leaked.copied()
        });
        assert_eq!(seen, Some(42));

        drop(bundle);
        assert_eq!(root.subtree_used(), 0, "context drop reclaims the leaked plan node");
    }

    #[test]
    fn build_failure_passes_through_and_drops_context() {
        let root = MemoryContext::new("root");
        let r = McxOwned::<PlanTy>::try_new(root.new_child("doomed").with_limit(8), |mcx| {
            let mut nodes: PgVec<u64> = PgVec::new_in(mcx);
            nodes.try_reserve_exact(64).map_err(|_| mcx.oom(512))?;
            nodes.extend(0..64);
            Ok(Plan { nodes })
        });
        assert!(r.is_err());
        assert_eq!(root.subtree_used(), 0);
    }

    #[test]
    fn slice_in_bytes_roundtrip() {
        let ctx = MemoryContext::new("copy");
        let src: [u8; 64] = core::array::from_fn(|i| (i as u8).wrapping_mul(7).wrapping_add(3));
        let v = slice_in(ctx.mcx(), &src).unwrap();
        assert_eq!(&*v, &src);
        assert_eq!(v.len(), 64);
        assert_eq!(v.capacity(), 64);
    }

    #[test]
    fn slice_in_empty_is_empty() {
        let ctx = MemoryContext::new("copy_empty");
        let empty: [u8; 0] = [];
        let v = slice_in(ctx.mcx(), &empty).unwrap();
        assert!(v.is_empty());
        assert_eq!(v.len(), 0);
        assert_eq!(ctx.subtree_used(), 0, "empty source allocates nothing");
    }

    #[test]
    fn slice_in_charges_and_reclaims() {
        let root = MemoryContext::new("copy_acct");
        {
            let src = [9u8; 128];
            let v = slice_in(root.mcx(), &src).unwrap();
            assert!(root.subtree_used() >= 128, "charge reflects the copied bytes");
            drop(v);
        }
        assert_eq!(root.subtree_used(), 0, "dropping the vec uncharges");
    }

    #[test]
    fn slice_in_panic_safe_frees_buffer() {
        // A panicking clone() must free the buffer; the prefix needs no drops.
        use core::sync::atomic::{AtomicUsize, Ordering};
        use super::std::panic::{catch_unwind, AssertUnwindSafe};

        static CLONES: AtomicUsize = AtomicUsize::new(0);

        #[derive(Default)]
        struct Bomb(u8);
        impl Clone for Bomb {
            fn clone(&self) -> Self {
                if CLONES.fetch_add(1, Ordering::SeqCst) == 2 {
                    panic!("boom on the 3rd clone");
                }
                Bomb(self.0)
            }
        }

        let root = MemoryContext::new("panic_safe");
        let src = alloc::vec![Bomb(0), Bomb(1), Bomb(2), Bomb(3)];

        CLONES.store(0, Ordering::SeqCst);

        let mcx = root.mcx();
        let res = catch_unwind(AssertUnwindSafe(|| {
            let _ = slice_in(mcx, &src);
        }));
        assert!(res.is_err(), "the clone panic must propagate");
        assert_eq!(root.subtree_used(), 0, "the panic-aborted buffer was freed");

        drop(src);
    }
}

mod bump_pool {
    use super::*;

    // Keeper retention across reset (the pool itself is covered by keeper_pool_caps_and_recycles).
    #[test]
    fn bump_reset_retains_and_reuses_keeper_block() {
        let mut ctx = MemoryContext::new_bump("per-tuple");
        let first = {
            let mcx = ctx.mcx();
            let v = vec_with_capacity_in::<u8>(mcx, 64).unwrap();
            v.as_ptr() as usize
        };
        let footprint = ctx.stats().arena_footprint;
        assert!(footprint > 0);
        ctx.reset();
        assert_eq!(ctx.stats().arena_footprint, footprint, "keeper retained over reset");
        let again = {
            let mcx = ctx.mcx();
            let v = vec_with_capacity_in::<u8>(mcx, 64).unwrap();
            v.as_ptr() as usize
        };
        assert_eq!(first, again, "first post-reset chunk starts the retained keeper");
        assert_eq!(ctx.stats().arena_footprint, footprint, "no new block malloc'd");
    }
}

mod acct_pool {
    use super::*;
    use core::cell::{Cell, RefCell};
    use core::ptr::NonNull;

    fn dummy_acct(name: &'static str) -> Acct {
        Acct {
            name: Cell::new(name),
            ident: RefCell::new(None),
            self_used: Cell::new(0),
            self_peak: Cell::new(0),
            limit: Cell::new(usize::MAX),
            limited_path: Cell::new(false),
            arena_footprint: Cell::new(0),
            arena_nblocks: Cell::new(0),
            window_tail: Cell::new(0),
            live_chunk_bytes: Cell::new(0),
            free_chunks: Cell::new(0),
            empty_blocks: Cell::new(0),
            is_bump: false,
            wholesale_reset: Cell::new(false),
            send_root: Cell::new(false),
            unpooled: false,
            kind: "AllocSet",
            parent: None,
            children: RefCell::new(alloc::vec::Vec::new()),
            led: crate::owner::Led::NEW,
        }
    }

    #[test]
    fn refcounts_match_rc_semantics() {
        let a = AcctRc::new(dummy_acct("a"));
        assert_eq!(a.name.get(), "a");
        let b = a.clone();
        let w = a.downgrade();
        assert_eq!(w.strong_count(), 2);
        drop(b);
        assert_eq!(w.strong_count(), 1);
        let c = w.upgrade().expect("value still alive");
        assert_eq!(c.name.get(), "a");
        assert_eq!(w.strong_count(), 2);
        drop(c);
        drop(a);
        assert_eq!(w.strong_count(), 0);
        assert!(w.upgrade().is_none(), "must not resurrect a dropped value");
        drop(w);
    }

    // A recycled allocation has no live handle, so reuse cannot alias.
    #[test]
    fn recycle_is_use_after_reset_free() {
        let pool = AcctPool::new(alloc::vec::Vec::new());

        let p1: NonNull<AcctInner> = acct_take_from(&pool);
        unsafe {
            core::ptr::addr_of_mut!((*p1.as_ptr()).strong).write(Cell::new(1));
            core::ptr::addr_of_mut!((*p1.as_ptr()).weak).write(Cell::new(1));
            core::ptr::addr_of_mut!((*p1.as_ptr()).val)
                .write(core::mem::MaybeUninit::new(dummy_acct("first")));
            assert_eq!((*p1.as_ptr()).val.assume_init_ref().name.get(), "first");
            core::ptr::drop_in_place(core::ptr::addr_of_mut!((*p1.as_ptr()).val).cast::<Acct>());
            (*p1.as_ptr()).strong.set(0);
            (*p1.as_ptr()).weak.set(0);
        }
        acct_give_to(&pool, p1);
        assert_eq!(pool.try_with(|s| s.len()).unwrap(), 1, "allocation parked for reuse");

        let p2 = acct_take_from(&pool);
        assert_eq!(p1.as_ptr(), p2.as_ptr(), "parked allocation reused");
        unsafe {
            core::ptr::addr_of_mut!((*p2.as_ptr()).strong).write(Cell::new(1));
            core::ptr::addr_of_mut!((*p2.as_ptr()).weak).write(Cell::new(1));
            core::ptr::addr_of_mut!((*p2.as_ptr()).val)
                .write(core::mem::MaybeUninit::new(dummy_acct("second")));
            assert_eq!((*p2.as_ptr()).val.assume_init_ref().name.get(), "second");
            core::ptr::drop_in_place(core::ptr::addr_of_mut!((*p2.as_ptr()).val).cast::<Acct>());
            crate::heap::Heap.deallocate(p2.cast(), core::alloc::Layout::new::<AcctInner>());
        }
    }

    #[test]
    fn child_context_churn_is_sound() {
        let root = MemoryContext::new("root");
        for i in 0..50 {
            let child = root.new_child("child");
            {
                let mut v: PgVec<u64> = vec_with_capacity_in(child.mcx(), i).unwrap();
                v.push(i as u64);
            }
            drop(child);
        }
        let _ = root.stats_tree();
        assert_eq!(root.name(), "root");
    }


    #[derive(Clone, Copy)]
    #[allow(dead_code)]
    struct Node {
        oid: u32,
        cost: f64,
    }

    #[test]
    fn forget_on_reset_runs_no_destructor_for_arena_safe() {
        let mut ctx = MemoryContext::new_bumpforget("forget");
        assert_eq!(ctx.used(), 0);
        for _ in 0..8 {
            {
                let mcx = ctx.mcx();
                let n: &mut Node = arena_box_in_forget(mcx, Node { oid: 1, cost: 2.0 }).unwrap();
                assert_eq!(n.oid, 1);
                let mut v: PgVec<Node> = PgVec::new_in(mcx);
                for i in 0..100u32 {
                    v.push(Node { oid: i, cost: i as f64 });
                }
                let leaked: &mut PgVec<Node> = arena_vec_in_forget(mcx, v).unwrap();
                assert_eq!(leaked.len(), 100);
                assert!(ctx.used() > 0, "charged while live");
            }
            ctx.reset();
            assert_eq!(
                ctx.used(),
                ctx.stats().arena_footprint,
                "forget-reset returns the charge to the keeper baseline"
            );
        }
    }

    #[test]
    fn forget_on_reset_leak_harness_charge_returns_to_baseline() {
        let mut ctx = MemoryContext::new_bumpforget("planner-like");
        let baseline = ctx.used();
        assert_eq!(baseline, 0);
        for run in 0..32 {
            {
                let mcx = ctx.mcx();
                let mut graph: PgVec<Node> = PgVec::new_in(mcx);
                graph.extend((0..(run as u32 + 1) * 10).map(|i| Node { oid: i, cost: 0.0 }));
                let _leaked = arena_vec_in_forget(mcx, graph).unwrap();
                let _b = arena_box_in_forget(mcx, Node { oid: run, cost: 1.0 }).unwrap();
            }
            ctx.reset();
            assert_eq!(
                ctx.used(),
                ctx.stats().arena_footprint,
                "run {run}: no accounting leak beyond the keeper"
            );
        }
        drop(ctx);
    }

    #[test]
    fn forget_context_drop_without_reset_is_sound() {
        let drops = std::rc::Rc::new(std::cell::Cell::new(0u32));
        {
            let ctx = MemoryContext::new_bumpforget("forget");
            let mcx = ctx.mcx();
            let _a = arena_box_in_forget(mcx, Node { oid: 7, cost: 0.0 }).unwrap();
            let mut v: PgVec<u64> = PgVec::new_in(mcx);
            v.extend(0..256);
            let _leaked = arena_vec_in_forget(mcx, v).unwrap();
            let _ = &drops;
        }
        assert_eq!(drops.get(), 0, "no destructors run for forgotten values");
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "BumpForget")]
    fn forget_helper_rejects_non_forget_context() {
        let ctx = MemoryContext::new_bump("not-forget");
        let _ = arena_box_in_forget(ctx.mcx(), Node { oid: 0, cost: 0.0 }).unwrap();
    }
}

#[test]
fn generation_context_fifo_and_reset_roundtrip() {
    let mut ctx = MemoryContext::new_generation("gen");
    {
        let mcx = ctx.mcx();
        let mut ring: std::collections::VecDeque<PgBox<'_, [u64; 8]>> =
            std::collections::VecDeque::new();
        for i in 0..32u64 {
            ring.push_back(box_new_in(mcx, [i; 8]));
        }
        let peak0 = ctx.stats().arena_footprint;
        for i in 0..10_000u64 {
            ring.push_back(box_new_in(mcx, [i; 8]));
            let old = ring.pop_front().unwrap();
            assert_eq!(old[0], if i < 32 { i } else { i - 32 });
            drop(old);
        }
        assert!(ctx.stats().arena_footprint <= peak0.max(4 * 8192));
        assert!(ctx.used() > 0);
    }
    ctx.reset();
    assert_eq!(ctx.used(), 0);
    let p = box_new_in(ctx.mcx(), 7u64);
    assert_eq!(*p, 7);
    assert!(ctx.used() > 0, "keeper re-charged on first post-reset alloc");
}

#[test]
fn generation_stats_report_block_tail_and_freed_chunks() {
    let mut ctx = MemoryContext::new_generation("gen");
    {
        let mcx = ctx.mcx();
        let boxes: std::vec::Vec<PgBox<'_, [u64; 4]>> =
            (0..10u64).map(|i| box_new_in(mcx, [i; 4])).collect();
        let t = ctx.stats_tree();
        assert!(t.free_bytes > 0, "GenerationStats counts the block tail as free");
        assert_eq!(t.free_chunks, 0);
        let mut boxes = boxes;
        drop(boxes.remove(3));
        drop(boxes.remove(3));
        assert_eq!(ctx.stats_tree().free_chunks, 2);
        drop(boxes);
        assert_eq!(ctx.stats_tree().free_chunks, 0, "an emptied block drops its freed chunks");
    }
    ctx.reset();
    let t = ctx.stats_tree();
    assert_eq!(t.free_chunks, 0);
    assert_eq!(t.free_bytes + 24, t.arena_footprint, "the retained keeper is free after reset");
}

#[test]
fn generation_vec_grows_and_frees_through_context() {
    let ctx = MemoryContext::new_generation("gen");
    let mcx = ctx.mcx();
    let mut v: PgVec<u64> = PgVec::new_in(mcx);
    for i in 0..10_000u64 {
        v.push(i);
    }
    assert_eq!(v.iter().sum::<u64>(), 10_000 * 9_999 / 2);
    drop(v);
    let s = ctx.stats();
    assert!(s.arena_footprint > 0);
}

#[test]
fn slab_context_boxes_roundtrip_and_uncharge() {
    let mut ctx = MemoryContext::new_slab("slab", 8 * 1024, core::mem::size_of::<[u64; 9]>());
    {
        let mcx = ctx.mcx();
        let mut held: std::vec::Vec<PgBox<'_, [u64; 9]>> = std::vec::Vec::new();
        for i in 0..5_000u64 {
            held.push(box_new_in(mcx, [i; 9]));
        }
        let peak = ctx.used();
        assert!(peak >= 5_000 * 72);
        for (i, b) in held.drain(..).enumerate() {
            assert_eq!(b[0], i as u64);
            drop(b);
        }
        // Up to 10 empty blocks stay parked; the rest uncharge.
        assert!(ctx.used() <= 10 * 8 * 1024, "used {} after drain", ctx.used());
        assert!(ctx.used() < peak);
    }
    ctx.reset();
    assert_eq!(ctx.used(), 0);
    assert_eq!(ctx.stats().arena_footprint, 0);
}

#[test]
fn slab_stats_tree_reports_free_chunks_and_empty_blocks() {
    let mut ctx = MemoryContext::new_slab("slab-stats", 8 * 1024, 72);
    {
        let mcx = ctx.mcx();
        let t = ctx.stats_tree();
        assert_eq!((t.nblocks, t.empty_blocks, t.free_chunks, t.free_bytes), (0, 0, 0, 0));
        let mut held: std::vec::Vec<PgBox<'_, [u64; 9]>> = std::vec::Vec::new();
        for i in 0..3u64 {
            held.push(box_new_in(mcx, [i; 9]));
        }
        let t = ctx.stats_tree();
        assert_eq!(t.nblocks, 1);
        assert!(t.free_chunks > 0);
        assert_eq!(t.free_bytes, t.free_chunks * 80);
        held.clear();
        let t = ctx.stats_tree();
        assert_eq!((t.nblocks, t.empty_blocks, t.free_chunks, t.free_bytes), (0, 1, 0, 0));
    }
    ctx.reset();
    assert_eq!(ctx.stats_tree().empty_blocks, 0);
}

#[test]
fn slab_child_context_reports_in_parent_subtree() {
    let parent = MemoryContext::new("parent");
    let child = parent.new_child_slab("slab-child", 8 * 1024, 64);
    let b = box_new_in(child.mcx(), [0u8; 64]);
    assert!(parent.subtree_used() >= 8 * 1024);
    drop(b);
    drop(child);
}
// GL-MEMWATCH-1: the process-wide block-bytes counter balances across a
// context lifecycle. Other tests allocate concurrently, so the assertions
// are delta-based with a large dedicated allocation and a noise allowance.
#[test]
fn global_footprint_tracks_context_block_bytes() {
    const BIG: usize = 32 * 1024 * 1024;
    const NOISE: usize = 8 * 1024 * 1024;
    // The NOISE allowance covers small concurrent churn but not GB-scale
    // transients from sibling tests (a 2GB spike between the two samples
    // failed this on CI, 2026-08-04). A retry loop keeps full detection
    // power: a genuine accounting leak elevates the footprint PERMANENTLY,
    // failing every attempt, while concurrent spikes are transient and
    // independent across attempts.
    let mut probe_ok = false;
    for attempt in 0..5 {
        let base = global_footprint::bytes();
        let ctx = MemoryContext::new("footprint-probe");
        // Dedicated (over chunk limit) allocation: counted at exact size.
        let v: PgVec<'_, u8> = vec_with_capacity_in(ctx.mcx(), BIG).unwrap();
        let held = global_footprint::bytes();
        drop(v);
        // NOISE rides BOTH directions: concurrent tests FREE between the
        // two samples of the process-global counter too (the lower bound
        // without the allowance was a latent flake — adjudicated by the
        // logdec lane, GL-CONCMEM-1 pays it in passing).
        let grew = held + NOISE >= base + BIG;
        drop(ctx);
        let after = global_footprint::bytes();
        let returned = after + NOISE >= base && after <= held.saturating_sub(BIG) + NOISE;
        if grew && returned {
            probe_ok = true;
            break;
        }
        std::eprintln!(
            "footprint probe attempt {attempt}: base {base} held {held} after {after} \
             (grew={grew} returned={returned}) — concurrent transient, retrying"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        probe_ok,
        "global footprint never balanced across 5 context lifecycles — accounting leak"
    );
    // Bump family balances too: create, spill past the keeper, drop.
    // Same retry discipline as above (a leak fails all attempts).
    let mut bump_ok = false;
    for attempt in 0..5 {
        let base2 = global_footprint::bytes();
        let grew2;
        {
            let mut ctx = MemoryContext::new_bump("footprint-bump-probe");
            for _ in 0..4000 {
                let _ = box_new_in(ctx.mcx(), [0u8; 4096]);
            }
            grew2 = global_footprint::bytes() + NOISE >= base2 + 4000 * 4096;
            ctx.reset();
        }
        let after2 = global_footprint::bytes();
        if grew2 && after2 <= base2 + NOISE {
            bump_ok = true;
            break;
        }
        std::eprintln!(
            "bump probe attempt {attempt}: base {base2} after {after2} (grew={grew2}) \
             — concurrent transient, retrying"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        bump_ok,
        "bump-family bytes never balanced across 5 lifecycles — accounting leak"
    );
}


// LocalStack (the per-thread pool container; the TLS statics themselves are
// cfg(not(test)) — the integration tests in tests/ exercise those).
#[cfg(feature = "std")]
mod local_stack {
    use crate::LocalStack;
    use core::sync::atomic::{AtomicUsize, Ordering};

    static DISPOSED: AtomicUsize = AtomicUsize::new(0);
    fn dispose(_v: u32) {
        DISPOSED.fetch_add(1, Ordering::Relaxed);
    }

    // One test fn: the dispose counter is shared, so the scenarios run
    // sequentially here rather than as parallel #[test]s.
    #[test]
    fn bounded_lifo_with_dispose() {
        // LIFO take/give.
        let s: LocalStack<u32> = LocalStack::new(3, dispose);
        assert_eq!(s.take(), None);
        s.give(1);
        s.give(2);
        assert_eq!(s.take(), Some(2));
        assert_eq!(s.take(), Some(1));
        assert_eq!(s.take(), None);

        // give: full list disposes the INCOMING item, keeps the cached ones.
        let before = DISPOSED.load(Ordering::Relaxed);
        s.give(1);
        s.give(2);
        s.give(3);
        s.give(4);
        assert_eq!(DISPOSED.load(Ordering::Relaxed), before + 1);
        assert_eq!(s.len(), 3);
        assert_eq!(s.take(), Some(3));

        // give_wholesale: full list drains EVERYTHING, keeps the incoming.
        s.give(3); // back to full: [1, 2, 3]
        let before = DISPOSED.load(Ordering::Relaxed);
        s.give_wholesale(9);
        assert_eq!(DISPOSED.load(Ordering::Relaxed), before + 3);
        assert_eq!(s.len(), 1);
        assert_eq!(s.take(), Some(9));

        // Drop drains the residue.
        s.give(7);
        s.give(8);
        let before = DISPOSED.load(Ordering::Relaxed);
        drop(s);
        assert_eq!(DISPOSED.load(Ordering::Relaxed), before + 2);
    }


}

// ---------------------------------------------------------------------------
// Allocation ceiling in the ALLOCATOR (C palloc's MaxAllocSize admission).
// Pre-fix, every one of these over-ceiling requests actually allocated >1GB
// and returned Ok — the ceiling lived only in opt-in wrapper helpers, so
// PgVec::push / try_reserve growth slipped past it (boundary-guard audit
// finding 7, the mechanism behind the nested-ROW() memory blowup).
// ---------------------------------------------------------------------------

#[test]
fn allocator_rejects_over_ceiling_allocate() {
    use allocator_api2::alloc::Allocator;
    let layout = core::alloc::Layout::from_size_align(MAX_ALLOC_SIZE + 1, 1).unwrap();
    for ctx in [MemoryContext::new("t"), MemoryContext::new_bump("t")] {
        let mcx = ctx.mcx();
        let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Allocator::allocate(&mcx, layout).map(|_| ()).map_err(|_| ())
        }));
        assert_ceiling_error(refused, Some(MAX_ALLOC_SIZE + 1));
        assert_eq!(ctx.used(), 0, "the refused request must not be charged");
        // At the ceiling itself the request is admitted (parity: palloc
        // accepts MaxAllocSize exactly).
        let ok = core::alloc::Layout::from_size_align(MAX_ALLOC_SIZE, 1).unwrap();
        let p = Allocator::allocate(&mcx, ok).expect("MaxAllocSize itself is admitted");
        // SAFETY: freeing the allocation just made, same layout.
        unsafe { Allocator::deallocate(&mcx, p.cast::<u8>(), ok) };
    }
}

#[test]
fn allocator_rejects_over_ceiling_grow() {
    // PgVec growth reallocates through Allocator::grow, the path the
    // nested-ROW() finding rode: a check only in `allocate` would miss it.
    let ctx = MemoryContext::new("t");
    let mut v: PgVec<u8> = vec_with_capacity_in(ctx.mcx(), 16).unwrap();
    v.extend_from_slice(b"payload");
    let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        v.try_reserve(MAX_ALLOC_SIZE + 1).map_err(|_| ())
    }));
    assert_ceiling_error(refused, None);
    assert_eq!(&v[..], b"payload", "failed growth leaves the vec intact");
    // The infallible lane is the one that used to abort the server.
    let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        v.resize(MAX_ALLOC_SIZE + 1, 0);
    }));
    assert!(refused.is_err(), "resize above MaxAllocSize must unwind, not abort");
}

// Over-ceiling requests unwind with C's MemoryContextSizeFailure ERROR so
// the statement boundary recovers them as ERROR. D3267: the payload is the
// ERROR itself (a boxed PgError, XX000), which the boundary's classifier
// recovers; a plain message is a crash there.
fn assert_ceiling_error<T: core::fmt::Debug>(
    r: Result<Result<T, ()>, std::boxed::Box<dyn core::any::Any + Send>>,
    size: Option<usize>,
) {
    let payload = r.expect_err("over-ceiling request must unwind, not return");
    let err = ::types_error::pg_error_from_panic(payload)
        .unwrap_or_else(|_| panic!("the ceiling's payload is a PgError the statement boundary recovers"));
    assert_eq!(err.sqlstate(), ::types_error::ERRCODE_INTERNAL_ERROR, "elog(ERROR)'s XX000");
    let msg = err.message();
    match size {
        Some(size) => assert_eq!(msg, alloc::format!("invalid memory alloc request size {size}")),
        None => assert!(msg.starts_with("invalid memory alloc request size "), "{msg}"),
    }
}

#[test]
fn huge_entry_points_bypass_ceiling() {
    // C palloc_extended(MCXT_ALLOC_HUGE): the explicit opt-out still admits
    // requests above MaxAllocSize (hash bucket arrays, memtuples).
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    let mut v: PgVec<u8> = vec_with_capacity_huge_in(mcx, MAX_ALLOC_SIZE + 2).unwrap();
    assert!(v.capacity() > MAX_ALLOC_SIZE);
    v.extend_from_slice(b"xyz");

    // Huge GROWTH (C repalloc_huge / simplehash SH_GROW): starts under the
    // ceiling, crosses it, keeps its contents.
    let mut g: PgVec<u64> = vec_with_capacity_in(mcx, 4).unwrap();
    g.extend_from_slice(&[1, 2, 3]);
    vec_reserve_huge(&mut g, MAX_ALLOC_SIZE / 8 + 16).unwrap();
    assert!(g.capacity() * 8 > MAX_ALLOC_SIZE);
    assert_eq!(&g[..], &[1, 2, 3]);
    g.push(4);
    assert_eq!(&g[..], &[1, 2, 3, 4]);

    // ... but even huge requests are admitted against MaxAllocHugeSize.
    let mut w: PgVec<u64> = vec_with_capacity_in(mcx, 4).unwrap();
    let err = vec_reserve_huge(&mut w, MAX_ALLOC_HUGE_SIZE).unwrap_err();
    assert!(err.message().starts_with("invalid memory alloc request size"));
    let err = vec_with_capacity_huge_in::<u64>(mcx, MAX_ALLOC_HUGE_SIZE).unwrap_err();
    assert!(err.message().starts_with("invalid memory alloc request size"));
}

// upstream 3f3eefc28892 (18.4): Detect pfree or repalloc of a previously-freed memory chunk.
// Debug-build tripwire (C MEMORY_CONTEXT_CHECKING); release builds carry no check.
#[cfg(debug_assertions)]
mod freed_chunk_detection {
    use super::*;
    use allocator_api2::alloc::Allocator;

    fn l(size: usize) -> Layout {
        Layout::from_size_align(size, 8).unwrap()
    }

    #[test]
    #[should_panic(expected = "detected double pfree in t 0x")]
    fn aset_immediate_double_pfree_is_detected() {
        let ctx = MemoryContext::new("t");
        let m = ctx.mcx();
        let a = m.allocate(l(8)).unwrap().cast::<u8>();
        unsafe {
            m.deallocate(a, l(8));
            m.deallocate(a, l(8));
        }
    }

    #[test]
    #[should_panic(expected = "detected double pfree in t 0x")]
    fn aset_non_adjacent_double_pfree_is_detected() {
        // Pre-fix shape: freelist a -> b -> a, then every alloc of the class returns a.
        let ctx = MemoryContext::new("t");
        let m = ctx.mcx();
        let a = m.allocate(l(8)).unwrap().cast::<u8>();
        let b = m.allocate(l(8)).unwrap().cast::<u8>();
        unsafe {
            m.deallocate(a, l(8));
            m.deallocate(b, l(8));
            m.deallocate(a, l(8));
        }
    }

    #[test]
    #[should_panic(expected = "detected double pfree in t 0x")]
    fn aset_pfree_of_moved_realloc_source_is_detected() {
        let ctx = MemoryContext::new("t");
        let m = ctx.mcx();
        let a = m.allocate(l(8)).unwrap().cast::<u8>();
        let _pin = m.allocate(l(8)).unwrap();
        unsafe {
            let moved = m.grow(a, l(8), l(4096)).unwrap().cast::<u8>();
            assert_ne!(moved, a, "always-move realloc");
            m.deallocate(a, l(8));
        }
    }

    #[test]
    #[should_panic(expected = "detected double pfree in t 0x")]
    fn aset_dedicated_double_pfree_is_detected() {
        let ctx = MemoryContext::new("t");
        let m = ctx.mcx();
        let a = m.allocate(l(100_000)).unwrap().cast::<u8>();
        unsafe {
            m.deallocate(a, l(100_000));
            m.deallocate(a, l(100_000));
        }
    }

    #[test]
    #[should_panic(expected = "detected realloc of freed chunk in t 0x")]
    fn aset_grow_of_freed_chunk_is_detected() {
        let ctx = MemoryContext::new("t");
        let m = ctx.mcx();
        let a = m.allocate(l(8)).unwrap().cast::<u8>();
        unsafe {
            m.deallocate(a, l(8));
            let _ = m.grow(a, l(8), l(64));
        }
    }

    #[test]
    #[should_panic(expected = "detected realloc of freed chunk in t 0x")]
    fn aset_shrink_of_freed_chunk_is_detected() {
        let ctx = MemoryContext::new("t");
        let m = ctx.mcx();
        let a = m.allocate(l(64)).unwrap().cast::<u8>();
        unsafe {
            m.deallocate(a, l(64));
            let _ = m.shrink(a, l(64), l(8));
        }
    }

    #[test]
    fn aset_reused_and_reset_chunks_are_not_flagged() {
        let mut ctx = MemoryContext::new("t");
        let a = ctx.mcx().allocate(l(8)).unwrap().cast::<u8>();
        unsafe { ctx.mcx().deallocate(a, l(8)) };
        let b = ctx.mcx().allocate(l(8)).unwrap().cast::<u8>();
        assert_eq!(a, b, "LIFO freelist reuse");
        unsafe { ctx.mcx().deallocate(b, l(8)) };
        ctx.reset();
        let c = ctx.mcx().allocate(l(8)).unwrap().cast::<u8>();
        unsafe { ctx.mcx().deallocate(c, l(8)) };
    }

    #[test]
    #[should_panic(expected = "detected double pfree in t 0x")]
    fn generation_double_pfree_is_detected() {
        let ctx = MemoryContext::new_generation("t");
        let m = ctx.mcx();
        let a = m.allocate(l(32)).unwrap().cast::<u8>();
        let b = m.allocate(l(32)).unwrap().cast::<u8>();
        unsafe {
            m.deallocate(a, l(32));
            m.deallocate(a, l(32));
            m.deallocate(b, l(32));
        }
    }

    #[test]
    #[should_panic(expected = "detected realloc of freed chunk in t 0x")]
    fn generation_grow_of_freed_chunk_is_detected() {
        let ctx = MemoryContext::new_generation("t");
        let m = ctx.mcx();
        let a = m.allocate(l(32)).unwrap().cast::<u8>();
        let _keep = m.allocate(l(32)).unwrap();
        unsafe {
            m.deallocate(a, l(32));
            let _ = m.grow(a, l(32), l(64));
        }
    }

    #[test]
    #[should_panic(expected = "detected realloc of freed chunk in t 0x")]
    fn generation_shrink_of_freed_chunk_is_detected() {
        let ctx = MemoryContext::new_generation("t");
        let m = ctx.mcx();
        let a = m.allocate(l(64)).unwrap().cast::<u8>();
        let _keep = m.allocate(l(32)).unwrap();
        unsafe {
            m.deallocate(a, l(64));
            let _ = m.shrink(a, l(64), l(32));
        }
    }

    #[test]
    fn generation_recarved_chunk_is_not_flagged() {
        let ctx = MemoryContext::new_generation("t");
        let m = ctx.mcx();
        let a = m.allocate(l(32)).unwrap().cast::<u8>();
        unsafe { m.deallocate(a, l(32)) };
        // Emptied current block rewinds; the re-carve rewrites the header.
        let b = m.allocate(l(32)).unwrap().cast::<u8>();
        assert_eq!(a, b);
        unsafe { m.deallocate(b, l(32)) };
    }

    #[test]
    #[should_panic(expected = "detected double pfree in t 0x")]
    fn slab_double_pfree_is_detected() {
        let ctx = MemoryContext::new_slab("t", 8 * 1024, 72);
        let m = ctx.mcx();
        let a = m.allocate(l(72)).unwrap().cast::<u8>();
        let b = m.allocate(l(72)).unwrap().cast::<u8>();
        unsafe {
            m.deallocate(a, l(72));
            m.deallocate(b, l(72));
            m.deallocate(a, l(72));
        }
    }

    #[test]
    fn slab_reused_chunk_is_not_flagged() {
        let ctx = MemoryContext::new_slab("t", 8 * 1024, 72);
        let m = ctx.mcx();
        let a = m.allocate(l(72)).unwrap().cast::<u8>();
        let b = m.allocate(l(72)).unwrap().cast::<u8>();
        unsafe { m.deallocate(a, l(72)) };
        let c = m.allocate(l(72)).unwrap().cast::<u8>();
        assert_eq!(a, c, "LIFO freelist reuse");
        unsafe {
            m.deallocate(c, l(72));
            m.deallocate(b, l(72));
        }
        // Block now parked empty; its chunks come back off the freelist.
        let d = m.allocate(l(72)).unwrap().cast::<u8>();
        unsafe { m.deallocate(d, l(72)) };
    }
}

// aset.c:1545-1609 AllocSetStats: nblocks counts every block on set->blocks
// (the single-chunk blocks of AllocSetAllocLarge included), totalspace is the
// bytes those blocks hold, freespace is Σ (endptr - freeptr) over the blocks
// plus the size of every chunk parked on a freelist, freechunks is the
// freelist population, and "used" is totalspace - freespace. The pgrust
// AllocSet tracks all of it (blocks, dedicated list, mem_allocated,
// freelists); the stats surface (stats_tree / stats, and through them
// pg_backend_memory_contexts and the context dump) must report it rather
// than 0 blocks / 0 free for every AllocSet context.
#[test]
fn aset_stats_walk_blocks_and_freelists() {
    use allocator_api2::alloc::Allocator;
    let mut ctx = MemoryContext::new("aset-stats");
    let l = Layout::from_size_align(64, 8).unwrap();
    // mcx's own tests take no recycled keeper (take_recycled_blocks), so the
    // first chunk allocates the 8K keeper: 389 64-byte chunks fill the keeper
    // (128), a second 8K block (128) and 133 slots of a 16K block (256).
    let n = 3 * aset::INIT_BLOCK_SIZE / 64 + 5;
    let mut ptrs = alloc::vec::Vec::new();
    {
        let mcx = ctx.mcx();
        for _ in 0..n {
            ptrs.push(Allocator::allocate(&mcx, l).unwrap().cast::<u8>());
        }
    }
    let t = ctx.stats_tree();
    assert_eq!(t.nblocks, 3, "AllocSetStats counts the keeper and both overflow blocks");
    assert_eq!(t.arena_footprint, 4 * aset::INIT_BLOCK_SIZE, "totalspace is the block bytes");
    // freespace = Σ (endptr - freeptr): only the 16K block's 123 unused slots.
    let tails = 4 * aset::INIT_BLOCK_SIZE - n * 64;
    assert_eq!(t.free_bytes, tails, "block tails are free space");
    assert_eq!(t.free_chunks, 0, "nothing parked on a freelist yet");

    // AllocSetFree parks the chunk on its freelist: freechunks counts it and
    // freespace grows by its class size (aset.c:1571-1590).
    for p in ptrs.drain(..10) {
        unsafe { Allocator::deallocate(&ctx.mcx(), p, l) };
    }
    let t = ctx.stats_tree();
    assert_eq!(t.free_chunks, 10);
    assert_eq!(t.free_bytes, tails + 10 * 64);
    let s = ctx.stats();
    assert_eq!(
        (s.nblocks, s.free_bytes, s.free_chunks),
        (3, tails + 10 * 64, 10),
        "ContextStats carries the same figures"
    );
    // A freelist hit un-parks the chunk.
    ptrs.push(Allocator::allocate(&ctx.mcx(), l).unwrap().cast::<u8>());
    let t = ctx.stats_tree();
    assert_eq!((t.free_chunks, t.free_bytes), (9, tails + 9 * 64));

    // AllocSetAllocLarge: a chunk above the freelist classes is a block of its
    // own — one more block, its bytes in totalspace, nothing free in it.
    let big = Layout::from_size_align(100_000, 8).unwrap();
    let d = Allocator::allocate(&ctx.mcx(), big).unwrap().cast::<u8>();
    let t = ctx.stats_tree();
    assert_eq!(t.nblocks, 4, "a single-chunk block is on set->blocks");
    assert_eq!(t.arena_footprint, 4 * aset::INIT_BLOCK_SIZE + 100_000);
    assert_eq!(t.free_bytes, tails + 9 * 64, "a single-chunk block has no free space");
    unsafe { Allocator::deallocate(&ctx.mcx(), d, big) };
    let t = ctx.stats_tree();
    assert_eq!(t.nblocks, 3, "AllocSetFree unlinks a single-chunk block");
    assert_eq!((t.free_chunks, t.free_bytes), (9, tails + 9 * 64));

    // Every chunk freed (nothing charged, so the reset leak check holds), then
    // AllocSetReset (aset.c:537-597): the keeper stays, everything else goes.
    for p in ptrs {
        unsafe { Allocator::deallocate(&ctx.mcx(), p, l) };
    }
    assert_eq!(ctx.stats_tree().free_chunks, n);
    ctx.reset();
    let t = ctx.stats_tree();
    assert_eq!(t.nblocks, 1, "the keeper block survives a reset");
    assert_eq!(t.arena_footprint, aset::INIT_BLOCK_SIZE);
    assert_eq!(t.free_bytes, aset::INIT_BLOCK_SIZE, "the keeper is entirely free");
    assert_eq!(t.free_chunks, 0, "AllocSetReset empties the freelists");
}

#[test]
fn start_small_aset_grows_from_one_kilobyte() {
    use allocator_api2::alloc::Allocator;
    let ctx = MemoryContext::new_start_small("start-small");
    let l = Layout::from_size_align(64, 8).unwrap();
    let first = Allocator::allocate(&ctx.mcx(), l).unwrap().cast::<u8>();
    let t = ctx.stats_tree();
    assert_eq!((t.nblocks, t.arena_footprint), (1, ALLOCSET_SMALL_INITSIZE));
    let mut ptrs = alloc::vec![first];
    for _ in 0..ALLOCSET_SMALL_INITSIZE / 64 {
        ptrs.push(Allocator::allocate(&ctx.mcx(), l).unwrap().cast::<u8>());
    }
    let t = ctx.stats_tree();
    assert_eq!(t.nblocks, 2);
    assert_eq!(t.arena_footprint, 2 * ALLOCSET_SMALL_INITSIZE, "the next block is 1 kB, then doubles");
    for p in ptrs {
        unsafe { Allocator::deallocate(&ctx.mcx(), p, l) };
    }
    let default = MemoryContext::new("default");
    let _ = Allocator::allocate(&default.mcx(), l).unwrap();
    assert_eq!(default.stats_tree().arena_footprint, aset::INIT_BLOCK_SIZE);
}

#[test]
fn pinned_context_handle_survives_moves() {
    struct Holder {
        rows: PgVec<'static, u64>,
        ctx: PinnedContext,
    }
    for ctx in [MemoryContext::new("pinned aset"), MemoryContext::new_bump("pinned bump")] {
        let ctx = PinnedContext::new(ctx);
        // SAFETY: rows drops before ctx, including while Holder is unwound.
        let mut rows = vec_with_capacity_in(unsafe { ctx.handle() }, 2).unwrap();
        rows.extend(0..2);
        let holder = Holder { rows, ctx };
        let mut parked = alloc::vec::Vec::new();
        parked.push(holder);
        let mut holder = parked.pop().unwrap();
        holder.rows.extend(2..32);
        assert_eq!(holder.rows.iter().sum::<u64>(), 496);
        assert!(holder.ctx.used() > 0);
        let Holder { rows, ctx } = holder;
        drop(rows);
        drop(ctx);
    }
}

#[test]
fn aset_stats_footprint_includes_freed_blocks_and_reset_keeper() {
    let mut ctx = MemoryContext::new("stats-retained");
    let buffers: std::vec::Vec<_> = (0..200)
        .map(|_| vec_with_capacity_in::<u8>(ctx.mcx(), 100).unwrap())
        .collect();
    let allocated = ctx.stats_tree().arena_footprint;
    assert!(allocated > ctx.used());
    assert_eq!(ctx.stats().arena_footprint, allocated);
    drop(buffers);
    assert_eq!(ctx.used(), 0);
    assert_eq!(ctx.stats().arena_footprint, allocated);
    ctx.reset();
    let keeper = ctx.stats_tree().arena_footprint;
    assert!(keeper > 0 && keeper < allocated);
    assert_eq!(ctx.stats().arena_footprint, keeper);
    assert_eq!(ctx.used(), 0);
}

#[test]
fn allocated_subtree_retains_freed_child_blocks() {
    let root = MemoryContext::new_bump("root");
    let root_blocks = root.subtree_allocated();
    let child = root.new_child("child");
    let v: PgVec<u8> = vec_with_capacity_in(child.mcx(), 64).unwrap();
    let child_blocks = child.subtree_allocated();
    assert!(child_blocks >= 8192);
    assert_eq!(root.subtree_allocated(), root_blocks + child_blocks);
    drop(v);
    assert_eq!(child.subtree_used(), 0);
    assert_eq!(child.subtree_allocated(), child_blocks);
    assert_eq!(root.subtree_allocated(), root_blocks + child_blocks);
    drop(child);
    assert_eq!(root.subtree_allocated(), root_blocks);
}

#[test]
fn allocated_subtree_counts_reset_keeper() {
    let mut root = MemoryContext::new("root");
    let v: PgVec<u8> = vec_with_capacity_in(root.mcx(), 64).unwrap();
    drop(v);
    root.reset();
    assert_eq!(root.used(), 0);
    assert_eq!(root.subtree_allocated(), 8192);
}

#[test]
fn allocated_subtree_counts_malloc_children() {
    let root = MemoryContext::new_bump("root");
    let baseline = root.subtree_allocated();
    let child = MemoryContext::with_backend("malloc", Backend::Malloc, Some(root.acct.clone()));
    let v: PgVec<u8> = vec_with_capacity_in(child.mcx(), 64).unwrap();
    assert_eq!(root.subtree_allocated(), baseline + 64);
    drop(v);
    assert_eq!(root.subtree_allocated(), baseline);
}

#[test]
fn generation_shrink_to_zero_releases_the_chunk() {
    let ctx = MemoryContext::new_generation("g");
    let mcx = ctx.mcx();
    let small = Layout::from_size_align(64, 8).unwrap();
    let big = Layout::from_size_align(2 * 1024 * 1024, 8).unwrap();
    let zero = Layout::from_size_align(0, 8).unwrap();
    let _keeper = Allocator::allocate(&mcx, small).unwrap();
    let p1 = Allocator::allocate(&mcx, big).unwrap().cast::<u8>();
    let p2 = Allocator::allocate(&mcx, big).unwrap().cast::<u8>();
    assert_eq!(ctx.stats().nblocks, 3, "keeper + two dedicated blocks");
    // SAFETY: p1/p2 are live from allocate with `big`; the zero-size results
    // are dangling and their deallocate is a no-op by contract.
    unsafe {
        let z1 = Allocator::shrink(&mcx, p1, big, zero).unwrap();
        assert_eq!(z1.len(), 0);
        let z2 = Allocator::shrink(&mcx, p2, big, zero).unwrap();
        Allocator::deallocate(&mcx, z1.cast(), zero);
        Allocator::deallocate(&mcx, z2.cast(), zero);
    }
    assert_eq!(ctx.stats().nblocks, 2, "one emptied block parks as the freeblock, the other is released");
}

/// Fallible std-Vec helpers (the sanctioned route for ported code that has
/// no memory context in hand): an unsatisfiable request is ERROR 53200 with
/// C's DETAIL line, never a handle_alloc_error abort. A capacity past
/// isize::MAX is refused deterministically without touching the allocator.
#[test]
fn std_vec_helpers_report_oom_as_error_53200() {
    let err = std_vec_with_capacity::<u64>("TopMemoryContext", usize::MAX / 16).unwrap_err();
    assert_eq!(err.sqlstate(), ERRCODE_OUT_OF_MEMORY);
    assert_eq!(err.message(), "out of memory");
    assert!(err.detail().unwrap_or("").contains("in memory context \"TopMemoryContext\""), "{err:?}");

    let mut v: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    assert!(std_vec_reserve("x", &mut v, usize::MAX).is_err());
    assert!(std_vec_resize("x", &mut v, usize::MAX, 0u8).is_err());
    assert!(std_vec_from_elem("x", 0u8, usize::MAX).is_err());
    assert!(v.is_empty(), "a failed reserve leaves the vector untouched");

    // The success lanes behave like their infallible twins.
    std_vec_resize("x", &mut v, 3, 7u8).unwrap();
    assert_eq!(v, [7, 7, 7]);
    assert_eq!(std_vec_from_slice("x", &v).unwrap(), v);
    assert_eq!(std_vec_from_elem("x", 1u16, 2).unwrap(), [1, 1]);
    assert!(std_vec_with_capacity::<u8>("x", 16).unwrap().capacity() >= 16);
    assert_eq!(oom_malloc().sqlstate(), ERRCODE_OUT_OF_MEMORY);
    assert!(oom_malloc().detail().is_none(), "malloc-shaped OOM carries no DETAIL");
}

#[test]
fn thread_freed_since_peak_tracks_block_bytes_per_thread() {
    std::thread::spawn(|| {
        let _ = crate::global_footprint::thread_freed_since_peak_rebase();
        {
            let cx = crate::MemoryContext::new("peak-test");
            let layout = core::alloc::Layout::from_size_align(4 << 20, 8).unwrap();
            cx.mcx().alloc_uninit_bytes(layout).unwrap();
        }
        let freed = crate::global_footprint::thread_freed_since_peak_rebase();
        assert!(freed >= 4 << 20, "freed {freed} misses a 4 MB chunk");
        assert_eq!(crate::global_footprint::thread_freed_since_peak_rebase(), 0, "rebase resets the peak");

        // A statement that only frees memory held from an earlier one (CLOSE of
        // a WITH HOLD cursor) still reports it; one that grows and keeps does not.
        let held = crate::MemoryContext::new("held");
        let layout = core::alloc::Layout::from_size_align(4 << 20, 8).unwrap();
        held.mcx().alloc_uninit_bytes(layout).unwrap();
        assert!(crate::global_footprint::thread_freed_since_peak_rebase() < 1 << 20);
        drop(held);
        assert!(crate::global_footprint::thread_freed_since_peak_rebase() >= 4 << 20);
    })
    .join()
    .unwrap();
}

// wio P334/P347: C's ALLOCSET_SMALL_SIZES (PortalContext) and a default-size
// context whose block waits for the first alloc (subxact
// CurTransactionContext): an unused context holds no block, and a small one
// starts at 1 kB and never grows a block past 8 kB.
#[test]
fn small_and_lazy_children_size_blocks_like_c() {
    let parent = MemoryContext::new("parent");
    let small = parent.new_child_small("PortalContext");
    let lazy = parent.new_child_lazy("CurTransactionContext");
    assert_eq!(small.stats().arena_footprint, 0);
    assert_eq!(lazy.stats().arena_footprint, 0);
    let _a = alloc_leak_in(small.mcx(), [0u8; 16]).unwrap();
    assert_eq!(small.stats().arena_footprint, ALLOCSET_SMALL_INITSIZE);
    for _ in 0..64 {
        let _b = alloc_leak_in(small.mcx(), [0u8; 1000]).unwrap();
    }
    let s = small.stats();
    assert!(s.arena_footprint <= s.nblocks * ALLOCSET_SMALL_MAXSIZE, "{s:?}");
    let _c = alloc_leak_in(lazy.mcx(), [0u8; 16]).unwrap();
    assert_eq!(lazy.stats().arena_footprint, 8 * 1024);
    let root = MemoryContext::new_small_wholesale("PortalPlanContext");
    assert_eq!(root.stats().arena_footprint, 0);
    let _d = alloc_leak_in(root.mcx(), [0u8; 100]).unwrap();
    assert_eq!(root.stats().arena_footprint, ALLOCSET_SMALL_INITSIZE);
}

#[cfg(unix)]
fn range_is_mapped(p: *const u8, len: usize) -> bool {
    // SAFETY: sysconf is a pure query.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let lo = (p as usize) & !(page - 1);
    let len = (p as usize + len - lo + page - 1) & !(page - 1);
    // SAFETY: MS_ASYNC on anonymous memory writes nothing back; an unmapped
    // range fails with ENOMEM (mincore does not on macOS).
    unsafe { libc::msync(lo as *mut _, len, libc::MS_ASYNC) == 0 }
}

// C: a chunk above glibc's mmap threshold is its own mapping, gone at pfree.
#[cfg(unix)]
#[test]
fn huge_chunk_is_returned_to_the_os_at_pfree() {
    let ctx = MemoryContext::new("t");
    let n = heap::MMAP_THRESHOLD * 2;
    let mut v: PgVec<u8> = vec_with_capacity_in(ctx.mcx(), n).unwrap();
    v.resize(n, 7);
    let (p, len) = (v.as_ptr(), v.capacity());
    assert!(range_is_mapped(p, len));
    drop(v);
    assert!(!range_is_mapped(p, len));
    let small: PgVec<u8> = vec_with_capacity_in(ctx.mcx(), heap::MMAP_THRESHOLD - 4096).unwrap();
    assert!(range_is_mapped(small.as_ptr(), small.capacity()));
}

// repalloc_huge: the array is resized in place of an alloc-copy-free, so the
// old buffer never outlives the call and the contents carry over.
#[cfg(unix)]
#[test]
fn huge_reserve_reallocs_and_releases_the_old_array() {
    let ctx = MemoryContext::new("t");
    let n = heap::MMAP_THRESHOLD / 8;
    let mut v: PgVec<u64> = vec_with_capacity_huge_in(ctx.mcx(), n).unwrap();
    v.extend((0..n as u64).map(|i| i * 3));
    let before = ctx.used();
    vec_reserve_huge(&mut v, n * 3).unwrap();
    assert!(v.capacity() >= n * 4);
    assert!(v.iter().enumerate().all(|(i, &x)| x == i as u64 * 3));
    assert_eq!(ctx.used() - before, n * 3 * 8);
    assert_eq!(ctx.peak(), ctx.used());
    let (p, len) = (v.as_ptr() as *const u8, v.capacity() * 8);
    drop(v);
    assert!(!range_is_mapped(p, len));
}

/// The tests that run statements through the process-wide governor registry (watchdog victims, fits, fair shares), one at
/// a time (`revoke::tests` takes it too).
pub(crate) static GOVERNOR_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// D4812: an episode's sample hint (`try_hint_stale`) never waits: with the registry or the pins table held, as another
/// thread's pick holds them, it returns `Busy` at once, counted, and hands nothing to the residency thread.
#[test]
fn d4812_a_sample_hint_never_waits_on_a_held_registry_or_pins_table() {
    let _one = GOVERNOR_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    for (registry, pins) in [(true, false), (false, true), (true, true)] {
        governor::with_held(registry, pins, || {
            let before = governor::sample_hint_counts();
            let t = std::time::Instant::now();
            let r = governor::try_hint_stale(1, false);
            let took = t.elapsed();
            let after = governor::sample_hint_counts();
            assert_eq!(r, Err(governor::Busy), "registry held {registry}, pins held {pins}");
            assert!(took < std::time::Duration::from_millis(50), "at once, not after the lock: {took:?}");
            assert_eq!(after.busy, before.busy + 1, "the skipped sample is counted");
            assert_eq!((after.samples, after.hints), (before.samples, before.hints), "nothing is handed over");
        });
    }
}

#[test]
fn governor_refuses_armed_block_allocation_past_the_bar() {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    let _one = GOVERNOR_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let ctx = MemoryContext::new("governed");
    let big = Layout::from_size_align(8 << 20, 8).unwrap();
    // Resident far past the bar: independent of other tests' allocations.
    governor::publish(64 << 20, 1 << 40, global_footprint::bytes());

    governor::arm(false);
    let p = ctx.mcx().alloc_uninit_bytes(big);
    assert!(p.is_ok(), "an unarmed thread is only charged");

    governor::arm(true);
    let payload = catch_unwind(AssertUnwindSafe(|| {
        let _ = ctx.mcx().alloc_uninit_bytes(big);
    }))
    .unwrap_err();
    let err = payload
        .downcast::<alloc::boxed::Box<PgError>>()
        .expect("governor payload must be a PgError");
    assert_eq!(err.message(), "out of memory");
    assert_eq!(err.sqlstate(), ::types_error::ERRCODE_OUT_OF_MEMORY);
    assert!(!governor::armed(), "firing disarms so the error report can allocate");

    governor::arm(true);
    governor::publish(0, 0, 0);
    assert!(ctx.mcx().alloc_uninit_bytes(big).is_ok(), "bar 0 is off");
    governor::arm(false);
}

#[test]
fn governor_trips_record_the_refusal_and_the_allocators_split() {
    let _one = GOVERNOR_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    governor::set_allocator_probes(|| Some(100 << 20), || 200 << 20);
    let base = global_footprint::bytes();
    governor::publish(base + (64 << 20), base, base);
    let before = governor::trip_counts();
    assert!(governor::fits_raw(32 << 20), "under the bar: no trip");
    assert!(!governor::fits_raw(128 << 20), "past the bar");
    let after = governor::trip_counts();
    assert!(after[3] > before[3], "a raw draw trip: {before:?} -> {after:?}");
    let t = governor::last_trip();
    assert_eq!((t.kind, t.name, t.n, t.bar, t.in_use), (governor::TRIP_KINDS[3], "Gravity grant", 128 << 20, base + (64 << 20), Some(100 << 20)));
    let c = governor::allocator_census(150 << 20).expect("counted");
    assert!(c.starts_with("alloc_in_use=100 alloc_committed=200 live_outside_contexts=") && c.ends_with(" resident_not_live=50"), "{c}");
    governor::set_allocator_probes(|| None, || 0);
    assert_eq!(governor::allocator_census(150 << 20), None, "not counted: no fields");
    governor::publish(0, 0, 0);
}

#[test]
fn statement_ledger_limits_blocks_and_charges() {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    let _one = GOVERNOR_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    std::thread::spawn(|| {
        let ctx = MemoryContext::new("ledgered");
        let big = Layout::from_size_align(4 << 20, 8).unwrap();
        governor::statement_start(0, false);
        assert!(governor::current().is_none(), "off: no ledger to carry");
        governor::statement_start(64 << 20, false);
        let l = governor::current().expect("a limit turns the ledger on");
        ctx.mcx().alloc_uninit_bytes(big).unwrap();
        assert!(l.bytes() >= 4 << 20, "{}", l.bytes());
        let l2 = l.clone();
        std::thread::spawn(move || {
            let _in = governor::enter(Some(l2));
            let c = MemoryContext::new("claim");
            c.mcx().alloc_uninit_bytes(Layout::from_size_align(8 << 20, 8).unwrap()).unwrap();
            drop(c);
        })
        .join()
        .unwrap();
        let grant = governor::Charge::new("Gravity grant", 16 << 20);
        assert!(l.bytes() >= 20 << 20);
        assert!(!grant.fits(64 << 20), "a draw past the limit is refused");
        assert!(grant.fits(1 << 20));
        governor::arm(true);
        let payload = catch_unwind(AssertUnwindSafe(|| {
            for _ in 0..64 {
                let _ = ctx.mcx().alloc_uninit_bytes(big);
            }
        }))
        .unwrap_err();
        let err = payload.downcast::<alloc::boxed::Box<PgError>>().expect("a PgError payload");
        assert_eq!(err.sqlstate(), ::types_error::ERRCODE_OUT_OF_MEMORY);
        let d = err.detail().unwrap();
        assert!(d.contains("Statement memory limit of 65536 kB") && d.contains("\"ledgered\""), "{d}");
        drop(grant);
        governor::statement_end();
        assert!(governor::current().is_none());
        governor::arm(false);
        drop(ctx);

        let ctx = MemoryContext::new("hog");
        governor::statement_start(0, true);
        ctx.mcx().alloc_uninit_bytes(Layout::from_size_align(48 << 20, 8).unwrap()).unwrap();
        assert!(governor::cancel_largest(95, 32 << 20) >= 48 << 20);
        assert_eq!(governor::cancel_largest(95, 32 << 20), 0, "one cancel at a time");
        governor::arm(true);
        let payload = catch_unwind(AssertUnwindSafe(|| {
            let _ = ctx.mcx().alloc_uninit_bytes(Layout::from_size_align(1 << 20, 8).unwrap());
        }))
        .unwrap_err();
        let err = payload.downcast::<alloc::boxed::Box<PgError>>().unwrap();
        assert!(err.detail().unwrap().contains("95% of its limit"), "{:?}", err.detail());
        governor::statement_end();
        governor::arm(false);
    })
    .join()
    .unwrap();
}

// The ledger batches a thread's block charges: the thread reads its exact total, another thread sees them once they net
// LEDGER_SLACK or at the claim's end, and a block allocated and freed per row never reaches the shared total.
#[test]
fn statement_ledger_batches_block_charges_per_thread() {
    let _one = GOVERNOR_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    std::thread::spawn(|| {
        let ctx = MemoryContext::new("batched");
        let row = MemoryContext::new("per row");
        governor::statement_start(0, true);
        let l = governor::current().expect("watch turns the ledger on");
        let elsewhere = |l: &alloc::sync::Arc<governor::StmtMem>| {
            let l = l.clone();
            std::thread::spawn(move || l.bytes()).join().unwrap()
        };
        let half = Layout::from_size_align(512 << 10, 8).unwrap();
        ctx.mcx().alloc_uninit_bytes(half).unwrap();
        let own = l.bytes();
        assert!(own >= 512 << 10 && own < governor::LEDGER_SLACK, "{own}");
        assert_eq!(elsewhere(&l), 0, "under the slack the charge stays on its thread");
        let block = Layout::from_size_align(64 << 10, 8).unwrap();
        for _ in 0..10_000 {
            let p = row.mcx().alloc_uninit_bytes(block).unwrap();
            // SAFETY: the chunk just allocated from `row`, same layout.
            unsafe { allocator_api2::alloc::Allocator::deallocate(&row.mcx(), p, block) };
        }
        assert_eq!(elsewhere(&l), 0, "a block per row nets nothing");
        assert!(l.bytes() - own < 128 << 10, "{} vs {own}", l.bytes());
        ctx.mcx().alloc_uninit_bytes(half).unwrap();
        let shared = elsewhere(&l);
        assert!(shared >= 1 << 20 && shared == l.bytes(), "{shared} vs {}", l.bytes());
        let l2 = l.clone();
        std::thread::spawn(move || {
            let entered = governor::enter(Some(l2));
            let c = MemoryContext::new("claim");
            c.mcx().alloc_uninit_bytes(Layout::from_size_align(256 << 10, 8).unwrap()).unwrap();
            drop(entered);
            drop(c);
        })
        .join()
        .unwrap();
        assert!(elsewhere(&l) >= shared + (256 << 10), "the claim's end flushes its charges");
        assert!(l.peak() >= l.bytes());
        governor::statement_end();
        drop(row);
        drop(ctx);
    })
    .join()
    .unwrap();
}

// D3929: the views list each charged statement under its backend's slot, with the numbers the watchdog's cancel ranks.
#[test]
fn charged_statements_are_listed_under_their_backends_slot() {
    let _one = GOVERNOR_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    std::thread::spawn(|| {
        let listed = |slot: usize| {
            let mut out = None;
            governor::for_each_statement(|s, b, p| {
                if s == slot {
                    out = Some((b, p));
                }
            });
            out
        };
        governor::register(4093, 4242);
        governor::statement_start(0, false);
        assert_eq!(listed(4093), None, "a statement the governor does not charge is not listed");
        governor::statement_start(0, true);
        let grant = governor::Charge::new("Gravity grant", 48 << 20);
        let (b, p) = listed(4093).expect("a charged statement is listed under its slot");
        assert!(b >= 48 << 20 && p >= b, "{b} {p}");
        drop(grant);
        let (b, p) = listed(4093).unwrap();
        assert!(b < 1 << 20 && p >= 48 << 20, "the peak stays: {b} {p}");
        governor::statement_end();
        assert_eq!(listed(4093), None, "an ended statement is not listed");
    })
    .join()
    .unwrap();
}

#[test]
fn governor_condemnation_is_consumed_once_by_its_backend() {
    governor::register(7, 4242);
    assert_eq!(governor::take_condemned(), 0);
    governor::condemn(7, 0x1_5a);
    assert_eq!(governor::take_condemned(), 0x1_5a, "the cancel word it names");
    assert_eq!(governor::take_condemned(), 0, "one condemnation, one 53200");
    governor::condemn(7, 0x1_5a);
    governor::register(7, 4343);
    assert_eq!(governor::take_condemned(), 0, "a new backend in the slot starts clean");
}

// D3821: the watchdog's cancel lands on the statement it chose. Its condemnation names that statement's cancel word, so
// the backend's interrupt poll raises its 53200 once while it runs, and its grant draws are refused meanwhile; a
// condemnation for a statement that ended before the poll is dropped, and the next statement is neither canceled nor
// refused.
#[test]
fn a_watchdog_cancel_lands_on_the_statement_it_chose_never_the_next() {
    let _one = GOVERNOR_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    std::thread::spawn(|| {
        let hog = Layout::from_size_align(48 << 20, 8).unwrap();
        governor::register(11, 4711);
        let ctx = MemoryContext::new("hog");
        governor::statement_start(0, true);
        ctx.mcx().alloc_uninit_bytes(hog).unwrap();
        let v = governor::cancel_largest_victim(90, 32 << 20).expect("the hog is chosen");
        assert!(v.bytes >= 48 << 20, "{v:?}");
        assert_eq!(v.backend, Some((11, 4711)), "its backend, whose interrupt the watchdog raises");
        assert_eq!(governor::cancel_largest_victim(90, 32 << 20), None, "one cancel at a time");
        assert!(governor::watchdog_cancel_pending() && !governor::fits_raw(1), "the grow door refuses its draws");
        let e = governor::take_watchdog_cancel().expect("raised while the statement it chose runs");
        assert_eq!(e.sqlstate(), ::types_error::ERRCODE_OUT_OF_MEMORY);
        assert!(e.detail().unwrap().contains("90% of its limit"), "{:?}", e.detail());
        assert!(governor::take_watchdog_cancel().is_none(), "one condemnation, one 53200");
        governor::statement_end();
        drop(ctx);

        // chosen, then the statement ends before the backend polls: the next statement never sees that cancel
        let ctx = MemoryContext::new("hog");
        governor::statement_start(0, true);
        ctx.mcx().alloc_uninit_bytes(hog).unwrap();
        assert!(governor::cancel_largest_victim(90, 32 << 20).is_some());
        governor::statement_end();
        governor::statement_start(0, true);
        assert!(!governor::watchdog_cancel_pending(), "the next statement is not canceled");
        assert!(governor::take_watchdog_cancel().is_none(), "the stale condemnation is dropped");
        assert!(governor::take_watchdog_cancel().is_none());
        governor::arm(true);
        ctx.mcx().alloc_uninit_bytes(Layout::from_size_align(1 << 20, 8).unwrap()).unwrap();
        governor::arm(false);
        governor::statement_end();
        drop(ctx);
    })
    .join()
    .unwrap();
}

// D3909: a cancel nobody answers blocks the next one only until mempolicy::watchdog::CANCEL_ACK_MS; then the next
// largest statement is canceled, and the unanswered one stays condemned and is not chosen again.
#[test]
fn an_unanswered_watchdog_cancel_stops_blocking_at_its_deadline() {
    let _one = GOVERNOR_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let (ready_tx, ready) = std::sync::mpsc::channel();
    let done = alloc::sync::Arc::new(std::sync::Barrier::new(3));
    let run = |procno: i32, pid: i32, mib: usize| {
        let (ready_tx, done) = (ready_tx.clone(), done.clone());
        std::thread::spawn(move || {
            governor::register(procno, pid);
            let ctx = MemoryContext::new("held");
            governor::statement_start(0, true);
            ctx.mcx().alloc_uninit_bytes(Layout::from_size_align(mib << 20, 8).unwrap()).unwrap();
            ready_tx.send(()).unwrap();
            done.wait();
            governor::statement_end();
            drop(ctx);
        })
    };
    let (a, b) = (run(12, 5012, 64), run(13, 5013, 40));
    ready.recv().unwrap();
    ready.recv().unwrap();
    let first = governor::cancel_largest_victim(90, 32 << 20).expect("the larger is chosen");
    assert_eq!(first.backend, Some((12, 5012)));
    // memfuzz (P4): the pick's ranked candidates, largest first, the victim's rank first
    let ranked = |v: &governor::Victim| v.top.iter().filter(|c| c.rank != 0).map(|c| (c.backend, c.canceled)).collect::<alloc::vec::Vec<_>>();
    assert_eq!(ranked(&first), [(Some((12, 5012)), false), (Some((13, 5013)), false)], "both ranked, the larger first");
    assert_eq!(first.top[0].rank, first.bytes as u64, "the victim's rank is the one it was chosen by");
    assert!(first.top[0].rank > first.top[1].rank);
    assert_eq!(governor::cancel_largest_victim(90, 32 << 20), None, "a fresh cancel blocks the next");
    std::thread::sleep(std::time::Duration::from_millis(mempolicy::watchdog::CANCEL_ACK_MS + 50));
    let next = governor::cancel_largest_victim(90, 32 << 20).expect("past its deadline the next is chosen");
    assert_eq!(next.backend, Some((13, 5013)), "the next largest, not the unanswered one again");
    assert_eq!(ranked(&next), [(Some((12, 5012)), true), (Some((13, 5013)), false)], "the unanswered one ranked, marked canceled");
    assert_eq!(governor::cancel_largest_victim(90, 32 << 20), None, "and that one blocks in turn");
    done.wait();
    a.join().unwrap();
    b.join().unwrap();
}

// Kill safety (R5b, 9d's catch): R6's own cancel is told from another road's by its victim's slot and cancel word, and
// it stops being pending when its statement ends (answered).
#[test]
fn pending_acks_tell_r6s_own_cancel_from_another_roads() {
    let _one = GOVERNOR_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let (ready_tx, ready) = std::sync::mpsc::channel();
    let (end_tx, end_rx) = std::sync::mpsc::channel::<()>();
    let done = alloc::sync::Arc::new(std::sync::Barrier::new(2));
    let (ready_tx2, done2) = (ready_tx.clone(), done.clone());
    let a = std::thread::spawn(move || {
        governor::register(16, 5016);
        let ctx = MemoryContext::new("held");
        governor::statement_start(0, true);
        ctx.mcx().alloc_uninit_bytes(Layout::from_size_align(64 << 20, 8).unwrap()).unwrap();
        ready_tx2.send(()).unwrap();
        end_rx.recv().unwrap();
        governor::statement_end();
        ready_tx2.send(()).unwrap();
        done2.wait();
        drop(ctx);
    });
    ready.recv().unwrap();
    assert_eq!(governor::try_pending_acks(None), Ok(governor::PendingAcks::default()), "no cancel yet");
    let v = governor::cancel_largest_victim(90, 32 << 20).expect("the holder is chosen");
    let own = Some((v.backend.map(|(slot, _)| slot), v.word));
    let (mine, young, old) = (
        governor::PendingAcks { own: true, other: false, other_young: false },
        governor::PendingAcks { own: false, other: true, other_young: true },
        governor::PendingAcks { own: false, other: true, other_young: false },
    );
    assert_eq!(governor::try_pending_acks(own), Ok(mine), "R6's own, unanswered");
    assert_eq!(governor::try_pending_acks(None), Ok(young), "named by no one: another road's, just raised");
    let wrong = Some((v.backend.map(|(slot, _)| slot), v.word ^ (1 << 20)));
    assert_eq!(governor::try_pending_acks(wrong), Ok(young), "another statement's word on that slot");
    std::thread::sleep(std::time::Duration::from_millis(mempolicy::ladder::R6_ACK_BOUND_MS + 10));
    assert_eq!(governor::try_pending_acks(None), Ok(old), "past R6's ack bound, inside its ack window: pending, not young");
    assert_eq!(governor::try_pending_acks(own), Ok(mine), "R6's own reads the same either way");
    end_tx.send(()).unwrap();
    ready.recv().unwrap();
    assert_eq!(governor::try_pending_acks(own), Ok(governor::PendingAcks::default()), "its statement ended: answered");
    done.wait();
    a.join().unwrap();
}

#[test]
fn governor_fits_raw_draws_under_the_bar_only() {
    let _one = GOVERNOR_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let base = global_footprint::bytes();
    governor::publish(4 << 30, 1 << 30, base);
    assert!(governor::fits_raw(1 << 30));
    assert!(!governor::fits_raw(7 << 29));
    governor::publish(0, 0, base);
    assert!(governor::fits_raw(usize::MAX / 2));
}

/// C3: under projection growth past the credit runs decide(Grow) at the next armed block, and the statement furthest
/// past its fair share is canceled (here the grower: its block fails at once with 53200); threshold decides nothing.
#[test]
fn c3_the_inline_decide_serves_a_crossed_credit_and_cancels_by_fair_share() {
    use mempolicy::decide::{self, Rise99, Rule};
    use std::panic::{catch_unwind, AssertUnwindSafe};
    let _one = GOVERNOR_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    std::thread::spawn(|| {
        const MIB: u64 = 1 << 20;
        let block = Layout::from_size_align(1 << 20, 8).unwrap();
        let rise = |b| Rise99 { bytes: b, prior: false };
        let inputs = |l: u64, b: u64| governor::DecidePublish { l, b, u: MIB, fast: 0, rise_fast: rise(MIB), rise_slow: rise(16 * MIB), revoke_on: false };
        governor::register(21, 2121);
        let ctx = MemoryContext::new("grower");
        let grow = |mib: usize| {
            for _ in 0..mib {
                ctx.mcx().alloc_uninit_bytes(block).unwrap();
            }
        };
        // threshold: no credit, so no inline decide (D4023: the other tests' threshold cancels land first)
        governor::clear_pending();
        governor::publish_decide(&inputs(64 << 30, 56 << 30));
        let c0 = governor::decide_counts();
        governor::statement_start(0, true);
        governor::arm(true);
        grow(40);
        assert_eq!(governor::decide_counts().inline_decides, c0.inline_decides, "threshold: no inline decide");
        governor::arm(false);
        governor::statement_end();

        // projection, F short of F* within the statement's share: decide(Grow) runs inline, nothing else happens. D3934:
        // the governor is off (pgrust.memory_governor_threshold = 0 publishes R 0), decide reads the backstop's own R
        decide::set_rule(Rule::Projection);
        let fp = global_footprint::bytes();
        governor::publish(0, 0, fp);
        governor::publish_decide_resident((40 * MIB) as usize, fp);
        assert!(governor::decide_inputs().r >= 40 * MIB, "D3934: decide's R is not the governor's");
        governor::statement_start(0, true);
        governor::publish_decide(&inputs(4 << 30, 64 * MIB)); // R 40 MiB: F 24 MiB, credit 24 / 4 = 6 MiB
        governor::statement_end();
        governor::statement_start(0, true);
        governor::arm(true);
        grow(16);
        let c1 = governor::decide_counts();
        assert!(c1.inline_decides > c0.inline_decides, "the credit was crossed and served");
        assert!(!governor::watchdog_cancel_pending(), "the limit is far: no cancel");
        assert!(governor::fits_raw(1), "within its share: no spill-self");
        governor::arm(false);
        governor::statement_end();

        // D3941: F = 0 with the limit far (no crossing): the grower is sent down its own road. Its draws past its grant are
        // refused; its admission (its grant) is not, or it sits in the admission queue for room no wait brings back
        governor::publish_decide_resident((70 * MIB) as usize, global_footprint::bytes());
        governor::statement_start(0, true);
        governor::publish_decide(&inputs(4 << 30, 64 * MIB));
        governor::statement_end();
        governor::statement_start(0, true);
        governor::arm(true);
        grow(4);
        assert!(governor::spills_itself() && !governor::fits_raw(1), "F = 0: no draw past its grant");
        assert!(governor::fits_raw_admit(1), "D3941: its admission is not refused");
        assert!(!governor::watchdog_cancel_pending(), "the limit is far: no cancel");
        governor::arm(false);
        governor::statement_end();
        // D3941: a start waits only on relief that is landing, re-decided every tick: not on flat pressure, while R falls
        // fast enough to close the gap (10 MiB a tick against 13 MiB short), and no longer once it is flat again
        governor::publish_decide_resident((70 * MIB) as usize, global_footprint::bytes());
        let t = governor::decide_tick().expect("projection decides at Tick");
        assert!(t.inputs.free() < t.inputs.target() && !governor::throttled(), "F < F*, nothing landing: no wait");
        governor::publish_decide_resident((60 * MIB) as usize, global_footprint::bytes());
        let t = governor::decide_tick().expect("projection decides at Tick");
        assert!(t.inputs.falling && governor::throttled(), "R falls fast enough: a start waits for it");
        let t = governor::decide_tick().expect("projection decides at Tick");
        assert!(t.inputs.free() < t.inputs.target() && !t.inputs.falling && !governor::throttled(), "flat again: no wait");

        // projection, F = 0 and R + rise99 past L - margin: the lone grower is the victim, its next block fails at once
        governor::publish(0, 0, global_footprint::bytes());
        governor::publish_decide_resident((60 * MIB) as usize, global_footprint::bytes());
        governor::statement_start(0, true);
        governor::publish_decide(&inputs(64 * MIB, 32 * MIB));
        governor::statement_end();
        governor::statement_start(0, true);
        governor::arm(true);
        let payload = catch_unwind(AssertUnwindSafe(|| grow(48))).expect_err("the grower is canceled at a block");
        governor::arm(false);
        let err = payload.downcast::<alloc::boxed::Box<PgError>>().expect("a PgError payload");
        assert_eq!(err.sqlstate(), ::types_error::ERRCODE_OUT_OF_MEMORY);
        assert!(err.detail().unwrap().contains("projected to cross it"), "{:?}", err.detail());
        assert!(err.hint().unwrap().contains("pgrust.memory_cancel_rule = projection"), "{:?}", err.hint());
        let c2 = governor::decide_counts();
        assert_eq!((c2.inline_cancels - c1.inline_cancels, c2.self_cancels - c1.self_cancels), (1, 1));
        assert!(governor::watchdog_cancel_pending() && !governor::fits_raw(1), "its draws are refused meanwhile");
        assert!(governor::take_watchdog_cancel().is_some(), "its backend raises it once");
        governor::statement_end();

        // the backstop's Tick: starts throttle while F < F* and the cancel's bytes are landing, not with room; an idle
        // holder counts in the shares
        let t = governor::decide_tick().expect("projection decides at Tick");
        assert!(t.inputs.free() < t.inputs.target() && t.inputs.pending != 0 && governor::throttled());
        let h = governor::decide_inputs().holders;
        governor::idle_holder(true);
        assert_eq!(governor::decide_inputs().holders, h + 1);
        governor::idle_holder(false);
        governor::publish_decide_resident(0, global_footprint::bytes());
        governor::publish_decide(&inputs(64 << 30, 56 << 30));
        assert!(governor::decide_tick().is_some() && !governor::throttled(), "room: no throttle");

        // back to the default: nothing decides
        decide::set_rule(Rule::Threshold);
        assert!(governor::decide_tick().is_none() && !governor::throttled());
        governor::publish_decide(&governor::DecidePublish::default());
        governor::publish_decide_resident(0, global_footprint::bytes());
        drop(ctx);
    })
    .join()
    .unwrap();
}

#[test]
fn process_cache_faults_charge_the_statement_and_the_footprint() {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    let _one = GOVERNOR_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    std::thread::spawn(|| {
        let ctx = MemoryContext::new("small");
        governor::statement_start(32 << 20, false);
        let l = governor::current().expect("a limit turns the ledger on");
        let (fp0, c0) = (global_footprint::bytes(), governor::cache_bytes());
        let l2 = l.clone();
        std::thread::spawn(move || {
            let _in = governor::enter(Some(l2));
            governor::charge_cache(40 << 20, "pgrc part cache", &mut governor::CacheCharge::default());
        })
        .join()
        .unwrap();
        assert!(global_footprint::bytes() + (16 << 20) >= fp0 + (40 << 20), "the footprint sees the cache at once");
        assert_eq!(governor::cache_bytes(), c0 + (40 << 20));
        assert!(l.bytes() >= 40 << 20, "{}", l.bytes());
        assert_eq!(l.largest(), ("pgrc part cache", 40 << 20));
        assert!(governor::refusal("pgrc part cache").is_none(), "an unarmed thread's check point never refuses");
        governor::arm(true);
        let e = governor::refusal("pgrc part cache").expect("an armed check point past the limit returns the error");
        assert_eq!(e.sqlstate(), ::types_error::ERRCODE_OUT_OF_MEMORY);
        assert!(e.detail().unwrap().contains("\"pgrc part cache\""), "{:?}", e.detail());
        let payload = catch_unwind(AssertUnwindSafe(|| {
            let _ = ctx.mcx().alloc_uninit_bytes(Layout::from_size_align(1 << 20, 8).unwrap());
        }))
        .unwrap_err();
        governor::arm(false);
        let err = payload.downcast::<alloc::boxed::Box<PgError>>().expect("a PgError payload");
        assert_eq!(err.sqlstate(), ::types_error::ERRCODE_OUT_OF_MEMORY);
        let d = err.detail().unwrap();
        assert!(d.contains("Statement memory limit of 32768 kB") && d.contains("\"pgrc part cache\""), "{d}");
        governor::statement_end();
        let held = l.bytes();
        std::thread::spawn(|| governor::release_cache(40 << 20, &mut governor::CacheCharge::default())).join().unwrap();
        assert_eq!(l.bytes(), held, "a release never credits a ledger");
        assert_eq!(governor::cache_bytes(), c0);
        assert!(global_footprint::bytes() < fp0 + (24 << 20));
        governor::release_cache(1 << 30, &mut governor::CacheCharge::default());
        assert_eq!(governor::cache_bytes(), 0, "never more than was charged");
        drop(ctx);
    })
    .join()
    .unwrap();
}

#[test]
fn free_shell_gives_back_the_box_chunk_and_leaves_what_it_points_to() {
    struct Node<'m> {
        list: PgVec<'m, u64>,
        n: u64,
    }
    crate::forget_safe_struct!(Node<'_> { list, n });
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    let mut list: PgVec<u64> = PgVec::new_in(mcx);
    list.extend_from_slice(&[1u64; 64]);
    let with_list = ctx.used();
    assert_eq!(with_list, list.capacity() * 8);
    let b = PgBox::new_in(Node { list, n: 7 }, mcx);
    assert_eq!(b.n, 7);
    assert_eq!(ctx.used(), with_list + core::mem::size_of::<Node>());
    free_shell(b);
    // the node's own chunk is back; its list is not (no drop glue ran): it stays until the context's reset
    assert_eq!(ctx.used(), with_list);
}

// D4483: Mcx::alloc_fast inlines the AllocSet pool hit at every allocation site and charges it with charge_unlimited;
// a context with a limit on its path always takes the out-of-line slow path (allocate_slow).
mod d4483_alloc_fast {
    use super::*;
    use allocator_api2::alloc::Allocator;
    use std::vec::Vec;

    fn l(size: usize) -> Layout {
        Layout::from_size_align(size, 8).unwrap()
    }

    // used, peak, mem_allocated, live chunk bytes, parked chunks, parked bytes, blocks
    fn acct_of(c: &MemoryContext) -> (usize, usize, usize, usize, usize, usize, usize) {
        (
            c.used(),
            c.peak(),
            c.mem_allocated(),
            c.acct.live_chunk_bytes.get(),
            c.acct.free_chunks.get(),
            c.acct.free_bytes(),
            c.acct.arena_nblocks.get(),
        )
    }

    // The same alloc / free sequence on a fast-path context and on one forced slow (a limit far above the sequence's
    // bytes sets limited_path, so every allocation goes through charge / check_limit_slow and AllocSet::alloc) leaves
    // the same accounting after every step: pool hits (freelist pops, bumps), refills and dedicated chunks alike.
    #[test]
    fn fast_and_forced_slow_paths_account_the_same() {
        let fast = MemoryContext::new("d4483 fast");
        let slow = MemoryContext::new("d4483 slow").with_limit(usize::MAX / 2);
        assert!(!fast.acct.limited_path.get() && slow.acct.limited_path.get());
        let sizes = [8usize, 24, 64, 100, 8, 512, 24, 1000, 4000, 9000, 64, 16, 2000, 8, 300, 40, 12000];
        let mut live: Vec<(NonNull<u8>, NonNull<u8>, Layout)> = Vec::new();
        for round in 0..4 {
            for (i, &n) in sizes.iter().enumerate() {
                let a = fast.mcx().allocate(l(n)).unwrap().cast::<u8>();
                let b = slow.mcx().allocate(l(n)).unwrap().cast::<u8>();
                live.push((a, b, l(n)));
                assert_eq!(acct_of(&fast), acct_of(&slow), "after alloc of {n} (round {round}, step {i})");
                if i % 3 == 2 {
                    let (a, b, lay) = live.remove(live.len() / 2);
                    // SAFETY: each pointer came from its own context's allocate with this layout, freed once.
                    unsafe {
                        fast.mcx().deallocate(a, lay);
                        slow.mcx().deallocate(b, lay);
                    }
                    assert_eq!(acct_of(&fast), acct_of(&slow), "after a free (round {round}, step {i})");
                }
            }
        }
        for (a, b, lay) in live.drain(..) {
            // SAFETY: as above.
            unsafe {
                fast.mcx().deallocate(a, lay);
                slow.mcx().deallocate(b, lay);
            }
        }
        assert_eq!(acct_of(&fast), acct_of(&slow), "after freeing everything");
        assert_eq!(fast.used(), 0);
    }

    // A context with a limit on its path refuses at the same byte as before even when the request would be a pool
    // hit: a parked chunk of its size class and room in the active block do not bypass check_limit_slow. A child of a
    // limited context inherits limited_path and refuses at its parent's limit.
    #[test]
    fn a_limited_context_refuses_at_the_limit_byte_even_on_a_pool_hit() {
        let ctx = MemoryContext::new("d4483 limited").with_limit(256);
        let m = ctx.mcx();
        let _a = m.allocate(l(128)).unwrap();
        let _b = m.allocate(l(64)).unwrap();
        let c = m.allocate(l(64)).unwrap();
        assert_eq!(ctx.used(), 256, "exactly at the limit is fine");
        assert!(m.allocate(l(8)).is_err(), "byte 257 is refused although the active block has room");
        // SAFETY: from this context's allocate with this layout, freed once.
        unsafe { m.deallocate(c.cast(), l(64)) };
        let _x = m.allocate(l(8)).unwrap();
        assert_eq!(ctx.used(), 200);
        let parked = ctx.acct.free_chunks.get();
        assert!(parked >= 1, "the freed 64-byte chunk is parked");
        assert!(m.allocate(l(64)).is_err(), "a parked chunk of the size does not bypass the limit (200 + 64 > 256)");
        assert_eq!((ctx.used(), ctx.acct.free_chunks.get()), (200, parked), "a refusal charges and pops nothing");
        let _y = m.allocate(l(56)).unwrap();
        assert_eq!(ctx.used(), 256);

        let parent = MemoryContext::new("d4483 limited parent").with_limit(256);
        let kid = parent.new_child("d4483 kid");
        assert!(kid.acct.limited_path.get(), "limited_path is inherited");
        let k = kid.mcx();
        let _p = parent.mcx().allocate(l(200)).unwrap();
        let _k1 = k.allocate(l(56)).unwrap();
        assert!(k.allocate(l(8)).is_err(), "the kid refuses at its parent's limit");
        assert_eq!(kid.used(), 56);
    }
}

#[test]
fn freeze_keeps_an_exact_array_until_the_context_goes() {
    let mut ctx = MemoryContext::new_wholesale("t");
    {
        let mcx = ctx.mcx();
        let mut v: PgVec<u32> = pgvec_exact_in(mcx, 5).unwrap();
        assert_eq!(v.capacity(), 5, "exactly n slots");
        assert_eq!(ctx.used(), 5 * 4);
        for i in 0..5u32 {
            v.push(i * 3);
        }
        let s: &[u32] = freeze(v);
        assert_eq!(s, &[0, 3, 6, 9, 12], "same values in the same order");
        assert_eq!(ctx.used(), 5 * 4, "a frozen array has no Drop: it stays charged, with no slack");
        let empty: &[u32] = freeze(pgvec_exact_in::<u32>(mcx, 0).unwrap());
        assert!(empty.is_empty());
        assert_eq!(ctx.used(), 5 * 4);
    }
    ctx.reset();
    assert_eq!(ctx.used(), 0, "the context's reset reclaims frozen arrays");
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "mcx::freeze: build the array with pgvec_exact_in at its final length")]
fn freeze_refuses_slack_in_debug_builds() {
    let ctx = MemoryContext::new("t");
    let mut v: PgVec<u32> = pgvec_exact_in(ctx.mcx(), 4).unwrap();
    v.push(1);
    let _ = freeze(v);
}

#[test]
fn freeze_fit_gives_back_the_spare_capacity() {
    let mut ctx = MemoryContext::new_wholesale("t");
    {
        let mcx = ctx.mcx();
        let mut v: PgVec<u64> = PgVec::new_in(mcx);
        for i in 0..5u64 {
            v.push(i + 1);
        }
        assert!(v.capacity() > 5, "pushed: spare capacity");
        let s: &[u64] = freeze_fit(v);
        assert_eq!(s, &[1, 2, 3, 4, 5]);
        assert_eq!(ctx.used(), 5 * 8, "no cap - len slack stays charged");
        let mut e: PgVec<u64> = PgVec::new_in(mcx);
        e.push(9);
        e.pop();
        let (before, buf) = (ctx.used(), e.capacity() * 8);
        assert!(freeze_fit(e).is_empty());
        assert_eq!(ctx.used(), before - buf, "an empty array gives its whole buffer back");
        let x: &[u64] = freeze_fit(pgvec_exact_in(mcx, 0).unwrap());
        assert!(x.is_empty());
    }
    ctx.reset();
    assert_eq!(ctx.used(), 0);
}

/// The idle release's trims: always with the switch off; with it on, only under ladder pressure (none in this process
/// unless a ladder test publishes one, so the answer is checked against the pressure read beside it). Every answer counted.
/// The idle-trim and purge-in-background switches are process-wide: their tests take turns.
pub(crate) static TRIM_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn idle_trim_runs_unless_the_switch_holds_it_for_ladder_pressure() {
    let _turn = TRIM_TURN.lock().unwrap_or_else(|p| p.into_inner());
    let (run0, held0) = idle_trim_counts();
    assert!(idle_trim_now(), "switch off: the idle release trims");
    set_idle_trim_pressure_only(true);
    let pressed_before = ladder::pressed_now();
    let trimmed = idle_trim_now();
    let pressed_after = ladder::pressed_now();
    set_idle_trim_pressure_only(false);
    let (run1, held1) = idle_trim_counts();
    assert_eq!(run1 + held1, run0 + held0 + 2, "both answers counted");
    if !pressed_before && !pressed_after {
        assert!(!trimmed, "switch on, no ladder pressure: the trims are held");
        assert_eq!(held1, held0 + 1, "counted as held");
    }
    if pressed_before && pressed_after {
        assert!(trimmed, "switch on under ladder pressure: the trims run");
    }
}

/// P3 (`parked_trim_rung`): under the knob the idle release never trims the parked regions and slot sets, whatever the
/// pressure (counted as held): the ladder's R2p trims them under pressure. Knob off: today's answer. Born red while the
/// idle release ignored the knob.
#[test]
fn p3_under_the_parked_trim_knob_the_idle_release_never_trims() {
    let _turn = TRIM_TURN.lock().unwrap_or_else(|p| p.into_inner());
    let (run0, held0) = idle_trim_counts();
    mempolicy::consts::set(mempolicy::consts::K::ParkedTrimRung, 1);
    let trimmed = idle_trim_now();
    mempolicy::consts::set(mempolicy::consts::K::ParkedTrimRung, 0);
    let counted = idle_trim_counts();
    assert!(!trimmed, "under the knob the idle release never trims");
    assert_eq!(counted, (run0, held0 + 1), "counted as held");
    assert!(idle_trim_now(), "knob off: the idle release trims as today");
}

/// PGRUST_PURGE_PRESSURE_BG: the idle release never trims (counted as held); the purger's trim runs the installed hook,
/// counted.
#[test]
fn under_purge_bg_the_idle_release_never_trims_and_the_purgers_trim_runs_the_hook() {
    use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static TRIMS: AtomicU64 = AtomicU64::new(0);
    fn trim() {
        TRIMS.fetch_add(1, Relaxed);
    }
    let _turn = TRIM_TURN.lock().unwrap_or_else(|p| p.into_inner());
    set_pressure_trim(trim);
    let (_, held0) = idle_trim_counts();
    let trims0 = purge_bg_counts().2;
    set_purge_bg(true);
    let trimmed = idle_trim_now();
    pressure_trim();
    set_purge_bg(false);
    assert!(!trimmed, "under the switch the idle release never trims");
    assert_eq!(idle_trim_counts().1, held0 + 1, "counted as held");
    assert_eq!(TRIMS.load(Relaxed), 1, "the purger's trim ran the hook");
    assert_eq!(purge_bg_counts().2, trims0 + 1, "and was counted");
}

/// D4812: an mcx line (ledger2's shadow line, revoke's settled-ask line) goes to the server-log writer the server
/// installed, which defers a memory safety thread's write, never to a raw stderr write.
#[test]
fn d4812_an_mcx_line_goes_to_the_installed_server_log_writer() {
    static GOT: std::sync::Mutex<std::string::String> = std::sync::Mutex::new(std::string::String::new());
    crate::set_line_sink(|s| GOT.lock().unwrap_or_else(|e| e.into_inner()).push_str(s));
    crate::server_log_line(b"ledger2-shadow: d4812 probe\n");
    assert!(GOT.lock().unwrap_or_else(|e| e.into_inner()).contains("ledger2-shadow: d4812 probe\n"));
}

// PGRUST_MCX_BUMP_FAST: a bump context's window hit is served inline (Mcx::alloc_fast), everything else by allocate_slow;
// the two routes leave identical state. Each test forces the switch per arena (set_fast), never through the process-wide
// flag, so tests running in parallel cannot flip each other's arenas.
mod bump_fast {
    use super::*;
    use core::alloc::Layout;
    use core::ptr::NonNull;

    fn force_fast(ctx: &MemoryContext, on: bool) {
        match &ctx.backend {
            // SAFETY: single-statement borrow, never re-entered (bump_mut).
            Backend::Bump(a) | Backend::BumpDrop(a, _) | Backend::BumpForget(a) => unsafe { bump_mut(a) }.set_fast(on),
            _ => panic!("not a bump context"),
        }
    }

    fn bump_ctx(kind: u8, on: bool) -> MemoryContext {
        let ctx = match kind {
            0 => MemoryContext::new_bump("bump"),
            1 => MemoryContext::new_bumpdrop("bumpdrop"),
            _ => MemoryContext::new_bumpforget("bumpforget"),
        };
        force_fast(&ctx, on);
        ctx
    }

    // allocate_slow calls since the last look.
    fn slow(ctx: &MemoryContext) -> u64 {
        ctx.slow_calls.take()
    }

    // Everything an allocation can move: the context's stats, the arena's mem_allocated and window, the ledger's tail.
    fn state(ctx: &MemoryContext) -> (ContextStats, usize, usize, usize) {
        let tail = match &ctx.backend {
            // SAFETY: single-statement borrow.
            Backend::Bump(a) | Backend::BumpDrop(a, _) | Backend::BumpForget(a) => unsafe { &*a.get() }.window_tail(),
            _ => unreachable!(),
        };
        (ctx.stats(), ctx.mem_allocated(), tail, ctx.acct.window_tail.get())
    }

    fn alloc(ctx: &MemoryContext, size: usize, align: usize) -> NonNull<[u8]> {
        Allocator::allocate(&ctx.mcx(), Layout::from_size_align(size, align).unwrap()).unwrap()
    }

    const CHUNK_LIMIT: usize = bump::BUMP_CHUNK_LIMIT;

    // One script of layouts: window hits, block refills, the chunk-limit edge, dedicated sizes, ZSTs, over-aligned.
    fn script() -> alloc::vec::Vec<(usize, usize)> {
        let mut v = alloc::vec::Vec::new();
        for i in 0..3000usize {
            v.push(((i * 37) % 200 + 1, 1 << (i % 4)));
        }
        v.extend_from_slice(&[
            (0, 1),
            (0, 64),
            (CHUNK_LIMIT - 7, 8),
            (CHUNK_LIMIT, 8),
            (CHUNK_LIMIT + 1, 8),
            (24, 16),
            (100, 4096),
            (3 * CHUNK_LIMIT, 8),
            (8, 8),
        ]);
        for i in 0..2000usize {
            v.push((((i * 7919) % 4000) + 1, 8));
        }
        v
    }

    #[test]
    fn inline_hits_leave_the_old_paths_state() {
        for kind in 0..3u8 {
            let mut fast = bump_ctx(kind, true);
            let mut old = bump_ctx(kind, false);
            for round in 0..3 {
                let (mut fprev, mut oprev) = (0usize, 0usize);
                for (i, &(size, align)) in script().iter().enumerate() {
                    let f = alloc(&fast, size, align);
                    let o = alloc(&old, size, align);
                    assert_eq!(f.len(), o.len(), "kind {kind} round {round} step {i}: length (size {size} align {align})");
                    let (fp, op) = (f.cast::<u8>().as_ptr() as usize, o.cast::<u8>().as_ptr() as usize);
                    assert_eq!(fp % align, 0, "kind {kind} step {i}: alignment {align}");
                    if size == 0 {
                        assert_eq!((fp, op), (align, align), "a ZST is dangling at its alignment");
                    }
                    // Both carve contiguously from the previous chunk, or neither does (same refill points).
                    assert_eq!(fp == fprev, op == oprev, "kind {kind} round {round} step {i}: carving (size {size} align {align})");
                    (fprev, oprev) = (fp + f.len(), op + o.len());
                    assert_eq!(state(&fast), state(&old), "kind {kind} round {round} step {i}: state after size {size} align {align}");
                }
                let (fs, os) = (slow(&fast), slow(&old));
                assert_eq!(os, script().len() as u64, "switch off: every allocation takes allocate_slow");
                assert!(fs * 4 < os, "switch on: window hits served inline ({fs} of {os} out of line)");
                fast.reset();
                old.reset();
                assert_eq!(state(&fast), state(&old), "kind {kind} round {round}: after reset");
            }
        }
    }

    #[test]
    fn window_hits_skip_allocate_slow() {
        for kind in 0..3u8 {
            let mut ctx = bump_ctx(kind, true);
            let first = alloc(&ctx, 16, 8).cast::<u8>().as_ptr() as usize;
            assert_eq!(slow(&ctx), 1, "the first allocation opens the window out of line");
            let used = ctx.used();
            for i in 1..64usize {
                let p = alloc(&ctx, 16, 8).cast::<u8>().as_ptr() as usize;
                assert_eq!(p, first + 16 * i, "contiguous carving");
            }
            let _ = alloc(&ctx, 3, 1);
            let _ = alloc(&ctx, 5, 2);
            let _ = alloc(&ctx, 8, 8);
            assert_eq!(slow(&ctx), 0, "window hits are served inline");
            assert_eq!(ctx.used(), used, "a window hit charges nothing (block transitions only)");
            // Out of line: over-aligned, ZST, dedicated.
            let big = alloc(&ctx, 16, 16).cast::<u8>().as_ptr() as usize;
            assert_eq!(big % 16, 0);
            assert_eq!(alloc(&ctx, 0, 8).len(), 0);
            assert!(alloc(&ctx, CHUNK_LIMIT + 1, 8).len() > CHUNK_LIMIT);
            assert_eq!(slow(&ctx), 3);
            ctx.reset();
            let off = bump_ctx(kind, false);
            let _ = alloc(&off, 16, 8);
            let _ = alloc(&off, 16, 8);
            assert_eq!(slow(&off), 2, "switch off: every allocation takes allocate_slow");
        }
    }

    // reset() early-exits on is_reset: an inline hit must clear it, or the reset after it is skipped and the window
    // keeps advancing.
    #[test]
    fn reset_after_inline_hits_rewinds() {
        for kind in 0..3u8 {
            let mut ctx = bump_ctx(kind, true);
            let first = alloc(&ctx, 32, 8).cast::<u8>().as_ptr() as usize;
            ctx.reset();
            slow(&ctx);
            let a = alloc(&ctx, 32, 8).cast::<u8>().as_ptr() as usize;
            assert_eq!(slow(&ctx), 0, "the post-reset window is open: an inline hit");
            assert_eq!(a, first, "the reset rewound the keeper");
            ctx.reset();
            let b = alloc(&ctx, 32, 8).cast::<u8>().as_ptr() as usize;
            assert_eq!(b, first, "the reset after an inline hit was not skipped");
            ctx.reset();
        }
    }

    #[test]
    fn bumpdrop_destructors_run_after_inline_hits() {
        use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};
        static DROPS: AtomicUsize = AtomicUsize::new(0);
        struct D(#[allow(dead_code)] u64);
        impl Drop for D {
            fn drop(&mut self) {
                DROPS.fetch_add(1, Relaxed);
            }
        }
        let mut ctx = bump_ctx(1, true);
        let _ = alloc(&ctx, 8, 8);
        ctx.reset();
        slow(&ctx);
        let before = DROPS.load(Relaxed);
        {
            let mcx = ctx.mcx();
            let _ = alloc(&ctx, 8, 8);
            let _d: &mut D = arena_leak(PgBox::new_in(D(7), mcx));
        }
        assert_eq!(slow(&ctx), 0, "both allocations were window hits");
        ctx.reset();
        assert_eq!(DROPS.load(Relaxed), before + 1);
    }
}

// BORN-RED DEFAULT WITNESS of PGRUST_MCX_BUMP_FAST (default on). Needs only the bump constructors, the Allocator trait and
// the test-only allocate_slow counter (MemoryContext::slow_calls), so it builds on trunk with that counter alone, and fails
// there at (a): trunk sends every bump allocation through allocate_slow.
mod bump_fast_witness {
    use super::*;
    use core::alloc::Layout;

    const N: u64 = 64;

    // allocate_slow calls of N window-sized allocations in each bump backend, after the allocation that opens the window
    // (always out of line: the keeper is claimed there).
    fn slow_calls_of_window_hits() -> alloc::vec::Vec<u64> {
        let mut seen = alloc::vec::Vec::new();
        for ctx in [
            MemoryContext::new_bump("witness bump"),
            MemoryContext::new_bumpdrop("witness bumpdrop"),
            MemoryContext::new_bumpforget("witness bumpforget"),
        ] {
            let mcx = ctx.mcx();
            let _ = Allocator::allocate(&mcx, Layout::from_size_align(16, 8).unwrap()).unwrap();
            ctx.slow_calls.take();
            for _ in 0..N {
                let _ = Allocator::allocate(&mcx, Layout::from_size_align(24, 8).unwrap()).unwrap();
            }
            seen.push(ctx.slow_calls.take());
        }
        seen
    }

    // (a) By default a bump context's window hit takes the inline Bump arm: no allocate_slow call.
    // (b) PGRUST_MCX_BUMP_FAST=0 restores the old path: a child process of this test binary started with it (the switch is
    //     read once per process) sends every one of those allocations through allocate_slow.
    #[test]
    fn default_bump_alloc_is_inline_and_env_zero_restores_allocate_slow() {
        let seen = slow_calls_of_window_hits();
        if std::env::var_os("MCX_BUMP_FAST_CHILD").is_some() {
            std::println!("MCX-BUMP-FAST-CHILD slow_calls={seen:?}");
            return;
        }
        assert!(std::env::var_os("PGRUST_MCX_BUMP_FAST").is_none(), "run the witness without PGRUST_MCX_BUMP_FAST set");
        assert_eq!(seen, [0, 0, 0], "(a) default: window hits are served inline, none through allocate_slow");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "tests::bump_fast_witness::default_bump_alloc_is_inline_and_env_zero_restores_allocate_slow",
                "--exact",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env("MCX_BUMP_FAST_CHILD", "1")
            .env("PGRUST_MCX_BUMP_FAST", "0")
            .output()
            .unwrap();
        let so = alloc::string::String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "child failed: {so} {}", alloc::string::String::from_utf8_lossy(&out.stderr));
        assert!(
            so.contains(&alloc::format!("MCX-BUMP-FAST-CHILD slow_calls={:?}", [N, N, N])),
            "(b) PGRUST_MCX_BUMP_FAST=0: every window-sized allocation through allocate_slow: {so}"
        );
    }
}

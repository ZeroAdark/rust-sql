//! C3 S4b: what a lowering reads under this thread's own uncommitted DDL, and what it read before that DDL. One test in
//! its own binary: the l2cache probes it installs are process-wide.
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};

use gravity_deps::{recording, Dep};

static PENDING: AtomicBool = AtomicBool::new(false);
static TOUCHED: AtomicU32 = AtomicU32::new(0);

fn pending() -> bool {
    PENDING.load(SeqCst)
}

fn touch(rels: &[u32], storage: &[u32], _cats: &[i32], _under: bool) -> bool {
    let t = TOUCHED.load(SeqCst);
    t != 0 && (rels.contains(&t) || storage.contains(&t))
}

#[test]
fn reads_under_own_ddl_hold_while_it_stands_and_reads_before_it_where_untouched() {
    l2cache::set_pending_invals_probe(pending);
    l2cache::set_pending_touch_probe(touch);
    let (a, b): (u32, u32) = (0xFFFF_E101, 0xFFFF_E202);
    let ((), before) = recording(|| gravity_deps::note_rel(a));
    assert!(before.current(), "read under the committed state, no DDL: holds");

    // the transaction's DDL registered a message of `b` (not yet a command end)
    PENDING.store(true, SeqCst);
    TOUCHED.store(b, SeqCst);
    assert!(before.current(), "the DDL touches nothing it read");
    let ((), under) = recording(|| gravity_deps::note_rel(b));
    assert!(under.current(), "read under the DDL as it stands: holds");
    // D3938: but nothing made from it is shared (a process-wide cache keys by the committed stamp)
    assert_eq!(under.committed_stamp(), None);
    assert!(before.committed_stamp().is_some());
    // C3 S5d: a read the DDL does not touch, under it, reads the committed state: it is shared and outlives the DDL
    let ((), untouched) = recording(|| gravity_deps::note_rel(a));
    assert!(untouched.current() && untouched.committed_stamp().is_some());
    let ((), touched) = recording(|| gravity_deps::note_rel(b));
    PENDING.store(false, SeqCst);
    let ((), committed_b) = recording(|| gravity_deps::note_rel(b));
    PENDING.store(true, SeqCst);
    assert!(!committed_b.current(), "read before the DDL, which touches it: not served under it");
    assert!(touched.current());

    // a command end: the DDL moved on (its next statement, or a rollback)
    l2cache::note_own_invals();
    assert!(!under.current() && !touched.current(), "read under the DDL as it was: no longer");
    assert!(before.current(), "read before the DDL, untouched: still holds");

    // two readings under different stands of the DDL never hold together
    let ((), later) = recording(|| gravity_deps::note_rel(b));
    assert!(later.current());
    let mut both = later.clone();
    both.absorb(&under);
    assert!(!both.current() && both.lagged());

    // the transaction ends: no messages pending, the count moves
    PENDING.store(false, SeqCst);
    TOUCHED.store(0, SeqCst);
    l2cache::note_own_invals();
    assert!(!later.current(), "read under the transaction's own DDL: not past its end");
    assert!(before.current() && committed_b.current(), "read under the committed state: holds again");
    assert!(untouched.current(), "C3 S5d: read under the DDL but untouched by it: holds past its end");
    assert!(before.iter().eq([Dep::Rel(a)]));
}

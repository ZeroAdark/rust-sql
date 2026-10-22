// The keeper blocks a dropped context parks for the next one (aset.c's context_freelists) are free context memory on
// their account (door 1 counted them) and no owner's: `parked_keeper_bytes` counts them (with `std`, by the account each
// parked on: `memledger::keepers`), so the owners view and the unattributed check read them as keepers, not as
// unattributed. A keeper is counted in when it parks, and out when a context takes it, when a park onto a full list
// discards the list, at a trim (idle passivation) and when its thread exits.
//
// An integration build compiles the lib WITHOUT cfg(test), so the keeper lists are live here (the in-crate unit tests
// bypass them). One #[test] in this binary, run on its own thread: the count is process-wide, and nothing else parks.
//
// Run: cargo test -p mcx --test parked_keepers [--features std]

use mcx::{box_new_in, parked_keeper_bytes, MemoryContext, ALLOCSET_SMALL_INITSIZE};

const MAX_FREE_CONTEXTS: usize = 100; // aset.c
const KEEPER: usize = 8 * 1024; // aset.c ALLOCSET_DEFAULT_INITSIZE
/// A parked entry's list buffer beside its keeper block (a few `Block` records).
const LIST_SLACK: usize = 1024;

fn parked_now() -> isize {
    parked_keeper_bytes() as isize
}

/// With `std`, the calling thread's account's keeper bytes (every keeper here parks on it), else the process's.
fn on_my_account() -> isize {
    #[cfg(feature = "std")]
    let n = memledger::keepers(memledger::current()) as isize;
    #[cfg(not(feature = "std"))]
    let n = parked_now();
    n
}

/// A context that allocated, so it holds its first block (the keeper it parks at its drop).
fn used(root: Option<&MemoryContext>, small: bool) -> MemoryContext {
    let c = match (root, small) {
        (Some(r), true) => r.new_child_small("used small"),
        (Some(r), false) => r.new_child("used"),
        (None, _) => MemoryContext::new("used root"),
    };
    let b = box_new_in(c.mcx(), [7u8; 16]);
    assert_eq!(b[15], 7);
    drop(b);
    c
}

fn one_keeper(delta: isize, block: usize) -> bool {
    delta >= block as isize && delta < (block + LIST_SLACK) as isize
}

#[test]
fn a_parked_keeper_is_counted_until_a_context_takes_it_or_its_list_lets_it_go() {
    let base = parked_now();
    std::thread::spawn(move || {
        let mine = on_my_account();
        drop(used(None, false));
        let k = parked_now() - base;
        assert!(one_keeper(k, KEEPER), "a dropped context's keeper is counted parked: {k}");
        assert_eq!(on_my_account() - mine, k, "on the account it parked on");
        let taker = MemoryContext::new("taker");
        assert_eq!(parked_now() - base, 0, "the next context takes the keeper: counted out");
        drop(taker);
        assert_eq!(parked_now() - base, k, "a context that never allocated parks the keeper it took, the same bytes");

        let root = MemoryContext::new("keeper root");
        assert_eq!(parked_now() - base, 0);
        drop(used(Some(&root), true));
        let s = parked_now() - base;
        assert!(one_keeper(s, ALLOCSET_SMALL_INITSIZE), "a small context's keeper is counted on its own list: {s}");
        drop(root);
        assert!(one_keeper(parked_now() - base - s, KEEPER), "the root's keeper parks beside it");

        let _ = mcx::passivate_trim();
        assert_eq!(parked_now() - base, 0, "a trim lets both lists go, counted out");

        // a full list: a park onto it discards the list first (aset.c AllocSetDelete), each entry counted out
        let mut live: Vec<MemoryContext> = (0..=MAX_FREE_CONTEXTS).map(|_| used(None, false)).collect();
        assert_eq!(parked_now() - base, 0);
        let last = live.pop().expect("one more than the list holds");
        drop(live);
        let full = parked_now() - base;
        let (lo, hi) = (MAX_FREE_CONTEXTS * KEEPER, MAX_FREE_CONTEXTS * (KEEPER + LIST_SLACK));
        assert!(full >= lo as isize && full < hi as isize, "a full list counts every entry: {full}");
        drop(last);
        let after = parked_now() - base;
        assert!(one_keeper(after, KEEPER), "the park onto the full list counted the discarded entries out: {after}");
    })
    .join()
    .expect("the keeper thread");
    if cfg!(feature = "std") {
        assert_eq!(parked_now() - base, 0, "a thread's exit frees its lists, counted out");
    }
}

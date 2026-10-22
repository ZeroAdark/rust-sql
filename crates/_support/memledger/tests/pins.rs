//! PINS: what a scan holds of a cache's entries is counted per owner beside `held`, by construction: a guard stored beside the
//! reference books the entry's bytes on the portal or statement it was taken under and takes them off at its drop, wherever
//! and whenever that runs. Once the cache evicts the entry, its holders' pins are evicted-pinned, and each holder's share
//! is the entry's bytes over its holders. One test (the ledger's tables are process-global), in its own binary.

use memledger::charge::{context, enter_portal, pins_of, portal_begin, portal_end};
use memledger::pins::PinSet;
use memledger::{account_row, bind_backend, statement_begin, statement_end, unbind_backend, Acct, Owner};

const ENTRY: u64 = 1000;

#[test]
fn pins_follow_their_owner_and_count_an_evicted_entry_once() {
    std::thread::spawn(|| {
        bind_backend(3952, 1);
        let a = Acct::backend(3952);
        statement_begin();
        let stmt = context().0;
        portal_begin(9);
        let cursor = {
            let _p = enter_portal(9);
            context().0
        };
        assert!(matches!(cursor, Owner::Portal { .. }) && matches!(stmt, Owner::Statement { .. }));
        let sum = || {
            let r = account_row(a);
            (r.pins_bytes, r.pins_evicted_bytes, r.pins_share_bytes)
        };
        assert_eq!(sum(), (0, 0, 0));
        let entry = PinSet::new(ENTRY);

        // a cursor's scan pins the entry under its portal; an unrelated statement then runs on the same backend and pins
        // the entry too: each is on its own owner's row, the backend's sum is both
        let on_cursor = entry.pin_as(cursor);
        assert_eq!((pins_of(cursor), pins_of(stmt)), ((1000, 0, 0), (0, 0, 0)));
        let on_stmt = entry.pin_as(stmt);
        assert_eq!((pins_of(cursor), pins_of(stmt)), ((1000, 0, 0), (1000, 0, 0)));
        assert_eq!(sum(), (2000, 0, 0));

        // the cache evicts it: both holders move into evicted; the shares sum to the entry's bytes (no double count)
        entry.evict();
        assert_eq!((pins_of(cursor), pins_of(stmt)), ((1000, 1000, 500), (1000, 1000, 500)));
        assert_eq!(sum(), (2000, 2000, 1000));

        // the statement's scan drops it: the cursor alone holds the entry, its share is the whole
        drop(on_stmt);
        assert_eq!((pins_of(cursor), pins_of(stmt)), ((1000, 1000, 1000), (0, 0, 0)));
        assert_eq!(sum(), (1000, 1000, 1000));

        // between two FETCHes another statement runs and ends on the backend: the cursor's pins stay on its row and the
        // backend's sum is unchanged
        statement_end();
        statement_begin();
        let other = context().0;
        let ephemeral = entry.pin_as(other);
        assert_eq!(pins_of(cursor), (1000, 1000, 500));
        drop(ephemeral);
        statement_end();
        assert_eq!(pins_of(cursor), (1000, 1000, 1000));
        assert_eq!(sum(), (1000, 1000, 1000));

        // the portal ends under its guard: the numbers follow to the session, the guard's drop takes them off there
        let _ = portal_end(a, 9);
        assert_eq!(pins_of(cursor), (0, 0, 0));
        assert_eq!(sum(), (1000, 1000, 1000));
        drop(on_cursor);
        assert_eq!(sum(), (0, 0, 0));
        assert_eq!(entry.holders(), 0);

        // an entry that was never evicted counts pins only
        let fresh = PinSet::new(ENTRY);
        statement_begin();
        let s2 = context().0;
        let g = fresh.pin();
        assert_eq!(pins_of(s2), (1000, 0, 0));
        drop(g);
        assert_eq!(pins_of(s2), (0, 0, 0));
        statement_end();
        unbind_backend();
    })
    .join()
    .unwrap();
}

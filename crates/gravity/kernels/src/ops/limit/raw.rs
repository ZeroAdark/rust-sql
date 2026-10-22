//! limit's raw half (rules R6 / R8): `step.limit`'s host entry, written through the contract's `host!` (no marker:
//! `extern "C"`, the body as written), moved unchanged from agg/entries.rs; its callers name it here.
use gravity_abi::status::raise;
use gravity_fn::PgError;

use crate::contract::sqlstate;

crate::contract::host! {
    /// `step.limit`'s negative bound (nodeLimit.c `recompute_limits`, OFFSET checked first): PG's error stashed, the raise
    /// block's `HOST` bit returned. `which` 0 = OFFSET, 1 = LIMIT.
    pub unsafe fn limit_negative(which: u64) -> u64 {
        let (msg, code) = match which {
            0 => ("OFFSET must not be negative", sqlstate(*b"2201X")),
            _ => ("LIMIT must not be negative", sqlstate(*b"2201W")),
        };
        let e = PgError::error(msg);
        let mut s = e.sqlstate();
        s.0 = code;
        gravity_fn::stash_error(Box::new(e.with_sqlstate(s))); // alloc: Cold
        raise::bit(raise::HOST)
    }
}

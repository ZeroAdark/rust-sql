use gravity_abi::ctx::site_ptr;
use gravity_abi::status::{WRITE_CONFLICT, WRITE_DONE, WRITE_NOT_MATCHED, WRITE_RECHECK, WRITE_SKIPPED, WRITE_SUBPLAN};
use gravity_abi::Ctx;

use super::{fail, guarded, inner_of};
use crate::source::{Outcome, WriteOp};
use crate::Error;

fn value(o: Outcome) -> i64 {
    match o {
        Outcome::Done => WRITE_DONE,
        Outcome::Skipped => WRITE_SKIPPED,
        Outcome::Recheck => WRITE_RECHECK,
        Outcome::Conflict => WRITE_CONFLICT,
        Outcome::NotMatched => WRITE_NOT_MATCHED,
        Outcome::SubPlan => WRITE_SUBPLAN,
    }
}

/// D2644: a beside writer's key (`Inner.writers`) is its write site, with `OWNS_NEXT` when its plan is ON CONFLICT DO
/// UPDATE: the conflict arm's update names the conflict site, the write site's successor (lowerv4 onconflict.rs).
pub(crate) const OWNS_NEXT: u32 = 1 << 31;

/// D2644: the beside writer keyed `key` answers a row op at site `i`.
fn owns(key: u32, i: usize) -> bool {
    let site = (key & !OWNS_NEXT) as usize;
    site == i || key & OWNS_NEXT != 0 && site + 1 == i
}

/// D1687: the writer whose write site is `$i` (a write plan beside the program's `write`), else the program's writer.
macro_rules! writer_at {
    ($n:expr, $i:expr) => {
        match $n.writers.iter_mut().find(|(s, _)| owns(*s, $i)) {
            Some((_, w)) => Some(w),
            None => $n.writer.as_mut(),
        }
    };
}

mod raw;
pub use raw::{lock, trap_lock, trap_write, write};

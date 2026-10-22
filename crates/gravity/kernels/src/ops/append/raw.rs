//! append's raw half (rule R6: the family's unsafe code lives here): D2013's `nest.prune` entry, which the lowered IR of
//! a nest inner union reaches through the contract's landing pad (`contract::entry1::<NestPrune>`, IR name `m8_prune`).
//! The object is exec's (ops/append/raw.rs), laid per execution into the protocol's handle word: a `PruneHead` whose
//! `magic` names it while it lives and whose `op` decides the pruned arms over the caller's frame words.
use gravity_abi::Ctx;

use crate::contract::Host;

/// The first field of a nest.prune object (exec ops/append/raw.rs lays it).
#[repr(C)]
pub struct PruneHead {
    /// `PRUNE_MAGIC` while the object lives; anything else is a stale or foreign handle.
    pub magic: u64,
    /// The object's runtime: the pruned arms' mask (bit `i`: arm `i`), else a status below zero with the error stashed.
    pub op: unsafe fn(h: *mut PruneHead, ctx: *mut Ctx) -> i64,
}

/// `nest.prune`'s object kind.
pub const PRUNE_MAGIC: u64 = 0x8a11_7e11_0000_0108;

/// D2013 (`m8_prune`; PG's choose_next_subplan_locally -> ExecFindMatchingSubPlans after ExecReScanAppend cleared the
/// valid set): pruning object `h` decides the arms PostgreSQL's run-time pruning keeps for the parameters the nest wrote.
/// The pruned arms as a bit mask (at most 63 arms), else a status below zero with the error stashed.
pub struct NestPrune;

impl Host for NestPrune {
    type Args = (u64,);
    /// # Safety
    /// `h` is a nest.prune handle word (exec lays the object per execution) or a stale one, which its magic refuses by name;
    /// `ctx` is the caller's context, bound for the call (the object reads its frame words).
    unsafe fn run(ctx: *mut Ctx, (h,): (u64,)) -> u64 {
        let head = h as *mut PruneHead;
        if head.is_null() || (*head).magic != PRUNE_MAGIC {
            return crate::host_abi::bare(&format!("m8_prune: a stale or foreign nest.prune handle {h:#x}")) as u64; // alloc: Cold
        }
        ((*head).op)(head, ctx) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn seven(_h: *mut PruneHead, _ctx: *mut Ctx) -> i64 {
        7
    }

    /// The entry runs a live object's op and refuses a null or foreign handle by name.
    #[test]
    fn the_entry_runs_its_objects_op_and_refuses_a_foreign_handle() {
        let mut live = PruneHead { magic: PRUNE_MAGIC, op: seven };
        let mut dead = PruneHead { magic: 0, op: seven };
        let ctx: *mut Ctx = core::ptr::null_mut();
        // SAFETY: both handles are live `PruneHead`s whose op reads no pointer; a null handle is refused before any read.
        unsafe {
            assert_eq!(NestPrune::run(ctx, (&mut live as *mut PruneHead as u64,)), 7);
            assert_eq!(NestPrune::run(ctx, (&mut dead as *mut PruneHead as u64,)) as i64, gravity_abi::status::ERROR);
            assert_eq!(NestPrune::run(ctx, (0,)) as i64, gravity_abi::status::ERROR);
        }
    }
}

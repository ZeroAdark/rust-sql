//! M9's host entries (W8-RECURSE: `setop.recursive`, PostgreSQL's RecursiveUnion, nodeRecursiveunion.c): the layout of
//! the generation object the lowered IR reads and the two C-ABI functions it calls on it (exec ops/setop/generation.rs
//! makes it).
//!
//! - The generation object (`GenCell`, `ProtoKind::Generation`): the head, then whether a term error is held. The working
//!   and intermediate tables are two nest.spool records (lowerv4 nest/spool_rec.rs `Rec`) whose handle and count parameter
//!   words the IR swaps in place at each generation: no host call and no slot swap.
//! - A generation's term rerun (`m8_rerun`) that fails with an ordinary ERROR, one PostgreSQL raises at that term row, is
//!   kept (`m9_gen_hold`): the rows the term appended before it go out first, then `m9_gen_raise` raises it where
//!   PostgreSQL raises it (ExecRecursiveUnion hands each term row on as the term produces it). A cancel, a resource or
//!   system error, an internal error, a run-time refusal or a parked unwind is never kept: the IR raises it at once.
//! - Each generation checks for interrupts first (`OP_GEN_CHECK`: raw.rs `GenCheck`, through the contract's landing pad;
//!   D2231).
//! - A recursive UNION (D1378) offers each seed and term row's key image to the object's set before the row enters its
//!   table (`OP_GEN_SEEN`: raw.rs `GenSeen`): PG's `LookupTupleHashEntry(.., &isnew)`, a row seen before is dropped.
//!
//! No entry runs under `guard!` (the crash policy, the user in session 09-29: no new catch_unwind containment); each body
//! is panic-free. The head is `m8`'s (`m8::Head`). Created by W8-RECURSE (owner M9-HOSTS).
use crate::ops::shared::object::entries::Head;

/// `setop.recursive`: a recursive union's generation object.
pub const GEN_MAGIC: u64 = 0x8a11_7e11_0000_0901;

/// D2732 (cursor form): the Generation proto's `arg` bit lowerv4 ops/setop/recursive.rs sets for a union whose seed or once-run
/// term input may raise or call a user or VOLATILE function, read on the statement's root path with no breaker reading it
/// whole (decompose `IterDesc::pull_loud`); exec runner/admission.rs refuses such a program on a pull that may count its rows.
/// The low 32 bits hold the union's table words.
pub const GEN_ARG_LOUD: u64 = 1 << 63;

/// D2230: the Generation proto's `arg` bit lowerv4 ops/setop/recursive.rs sets for a union a qual, a join, a nested loop's
/// inner, a SubPlan or an InitPlan reads with no breaker over it (decompose `IterDesc::thin`): a pull that asks for a whole
/// block may never fill it over a non-terminating union, so exec's pull of such a program asks for the rows each run owes
/// only (D439's owed cap), as PG computes no row past the run's count.
pub const GEN_ARG_THIN: u64 = 1 << 62;

/// D2805: the keyed SetOp proto's `arg` bit lowerv4 ops/setop/lower.rs sets for a sorted set operation on the statement's
/// root path whose left input streams a raise (decompose `SetDesc::Set::pull_loud`): PG hands on each left group's rows as
/// it reads them (nodeSetOp.c setop_retrieve_sorted), the grouped form reads the left whole at the first pull. exec
/// runner/admission.rs refuses such a program on a pull that may count its rows. The keyed form's fields stop at bit 47.
pub const SETOP_ARG_LOUD: u64 = 1 << 63;

/// The generation object's operation codes (its own namespace: only `GenCell`'s `op` receives them).
pub const OP_GEN_HOLD: u64 = 1;
pub const OP_GEN_RAISE: u64 = 2;
pub const OP_GEN_CHECK: u64 = 3;
/// setop P7 (D1378): a UNION's row image offered to the object's set (raw.rs `GenSeen`): 1 when new (kept), 0 when seen.
pub const OP_GEN_SEEN: u64 = 4;
/// D3374 (e) (record keys E1): whether the object's set holds an image (the same words as `OP_GEN_SEEN`'s), never keeping
/// it: 1 held, 0 not (kernels typesem userfp.rs `UserCollideProbe`).
pub const OP_GEN_HAS: u64 = 5;

/// The generation object: the head, then 1 while a term error is kept (the IR reads it at the generation's end).
#[repr(C)]
pub struct GenCell {
    pub head: Head,
    pub held: u64,
}

pub const GEN_HELD: usize = core::mem::offset_of!(GenCell, held);

/// `setop.recursive`'s two C-ABI entries (`m9_gen_hold`, `m9_gen_raise`) live in raw.rs, written through the contract's
/// `host!`; re-exported here under their old path, so the lowerers' addresses and the IR's host names are
/// unchanged.
pub use super::raw::{m9_gen_hold, m9_gen_raise};

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_abi::status::ERROR;
    use gravity_abi::Ctx;

    unsafe fn record(_h: *mut Head, _ctx: *mut Ctx, code: u64, _a: u64, _b: u64) -> i64 {
        code as i64 * 10
    }

    #[test]
    fn each_entry_runs_its_objects_op_and_a_foreign_or_null_handle_is_an_error_by_name() {
        let ctx: *mut Ctx = core::ptr::null_mut();
        let gen = Box::new(Head { magic: GEN_MAGIC, op: record }); // alloc: Cold
        let h = &*gen as *const Head as u64;
        // SAFETY: a live head whose op reads nothing.
        assert_eq!(unsafe { (m9_gen_hold(ctx, h), m9_gen_raise(ctx, h)) }, (10, 20));
        let other = Box::new(Head { magic: crate::ops::shared::object::entries::CTE_MAGIC, op: record }); // alloc: Cold
        // SAFETY: a live head of another kind: only its magic is read.
        assert_eq!(unsafe { m9_gen_hold(ctx, &*other as *const Head as u64) }, ERROR);
        let msg = gravity_fn::take_error().map(|e| e.message().to_owned()).unwrap_or_default();
        assert!(msg.starts_with("gravity: m9_gen_hold: a stale or foreign M9 handle 0x"), "{msg}");
        // SAFETY: a null handle: nothing is read.
        assert_eq!(unsafe { m9_gen_raise(ctx, 0) }, ERROR);
        assert!(gravity_fn::take_error().is_some_and(|e| e.message().contains("m9_gen_raise: a stale or foreign M9 handle 0x0")));
    }

    #[test]
    fn the_held_word_follows_the_head() {
        assert_eq!(GEN_HELD, 16);
        assert_eq!(core::mem::size_of::<GenCell>(), 24);
    }
}
